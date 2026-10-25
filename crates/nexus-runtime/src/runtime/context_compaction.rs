//! Run-local, staged context reduction. Estimates are heuristics, never proof
//! that a provider request fits. Exact execution evidence stays in the transcript.
use super::*;
use nexus_context::{
    SUMMARY_PROMPT, TRUNCATE_NOTE, conversation_with_memory, estimated_input_tokens,
    needs_compaction, plan_reduction, plan_summary_reduction, project_old_tool_outputs,
    project_tool_outputs, render_context_note, summary_text,
};
use nexus_core::ContextCompactionStrategy;

const MAX_SUMMARY_OUTPUT_TOKENS: u64 = 4096;
const SUMMARY_REQUEST: &str = "Summarize the preceding historical data now. Do not continue the original task. Return only the compact summary.";

pub(super) fn projection_overhead(active: &ActiveRun, note: &str) -> usize {
    memory_bytes(active.derived_memory.as_deref()).saturating_add(
        runtime_note_bytes(
            &render_context_note(active.context_management, note, 0),
            active.instruction_bytes > 0,
        )
        .saturating_sub(runtime_note_bytes(note, active.instruction_bytes > 0)),
    )
}

pub(super) struct PendingCompaction {
    items: Vec<ModelContextItem>,
    history_len: usize,
    removed: usize,
}

fn estimate(active: &ActiveRun, request: &ModelRequest) -> u64 {
    active.token_estimator.estimate(request)
}

pub(super) fn calibrate_estimate(active: &mut ActiveRun, events: &[ProviderEvent]) {
    // Usage is per invocation, not cumulative occupancy. Never lower the safety
    // margin based on one sparse response, or treat missing counters as zero.
    let actual = events.iter().rev().find_map(|event| match event {
        ProviderEvent::TurnFinished(finished) => finished.usage().input_tokens(),
        ProviderEvent::Usage(usage) if usage.finality() == UsageFinality::Final => {
            usage.input_tokens()
        }
        _ => None,
    });
    active
        .token_estimator
        .observe(active.input_estimate, actual);
}

fn request_for(
    shared: &Shared,
    active: &ActiveRun,
    turn: &TurnId,
    items: &[ModelContextItem],
    note: String,
    memory: Option<&str>,
    summary: bool,
) -> Result<ModelRequest, Terminal> {
    let enabled = if summary {
        Vec::new()
    } else {
        shared
            .tool_order
            .iter()
            .filter_map(|name| shared.tools.get(name).map(|tool| tool.spec.id().clone()))
            .collect()
    };
    let request = ModelRequest::new(
        active.run.clone(),
        turn.clone(),
        active.profile.clone(),
        enabled,
        if summary {
            None
        } else {
            active.continuation.clone()
        },
        shared.effective_output_budget,
    )
    .map_err(Terminal::failed)?
    .with_context_management(active.context_management)
    // Summarization is a separate operation, not another coding-agent turn.
    // Inheriting task/environment directives can make it resume the task.
    .with_instructions(if summary {
        ModelInstructions::new(SUMMARY_PROMPT, "").map_err(Terminal::failed)?
    } else {
        shared.instructions.clone()
    })
    .map_err(Terminal::failed)?
    .with_context_note(note)
    .map_err(Terminal::failed)?
    .with_conversation(conversation_with_memory(items, memory).map_err(Terminal::failed)?)
    .map_err(Terminal::failed)?;
    if summary {
        Ok(request)
    } else {
        request
            .with_tool_definitions(shared.tool_definitions.clone())
            .map_err(Terminal::failed)
    }
}

fn normal_request(
    shared: &Shared,
    active: &ActiveRun,
    turn: &TurnId,
    items: &[ModelContextItem],
) -> Result<ModelRequest, Terminal> {
    normal_request_with_memory(
        shared,
        active,
        turn,
        items,
        active.derived_memory.as_deref(),
    )
}

fn normal_request_with_memory(
    shared: &Shared,
    active: &ActiveRun,
    turn: &TurnId,
    items: &[ModelContextItem],
    memory: Option<&str>,
) -> Result<ModelRequest, Terminal> {
    normal_projection(shared, active, turn, items, memory, &active.context_note)
}

fn normal_projection(
    shared: &Shared,
    active: &ActiveRun,
    turn: &TurnId,
    items: &[ModelContextItem],
    memory: Option<&str>,
    context_note: &str,
) -> Result<ModelRequest, Terminal> {
    // A fixed-width estimate makes rebuilding the note stable, rather than
    // recursively increasing the occupancy by appending notes each turn.
    let note = |tokens| task_context_note(active, context_note, tokens);
    let request = request_for(shared, active, turn, items, note(0_u64), memory, false)?;
    let tokens = estimate(active, &request);
    request_for(shared, active, turn, items, note(tokens), memory, false)
}

fn task_context_note(active: &ActiveRun, context_note: &str, tokens: u64) -> String {
    let note = if active.output_limit_recovery_used {
        format!(
            "{context_note}\nAn earlier model response reached its output limit and was discarded as incomplete. Keep responses concise. Choose the next small actionable step from the accepted conversation; do not reconstruct or continue incomplete tool calls. This notice is not evidence that any action executed."
        )
    } else {
        context_note.to_owned()
    };
    render_context_note(active.context_management, &note, tokens)
}

fn preview_request_for_headroom(
    active: &ActiveRun,
    mut request: ModelRequest,
    window: u64,
    output: u64,
) -> Result<ModelRequest, Terminal> {
    // Project only into this request; source records and staged recent groups
    // stay exact. Summary acceptance must use the same escape hatch as tasks.
    let source = request.conversation().to_vec();
    request = request
        .with_conversation(project_old_tool_outputs(&source, 2048).map_err(Terminal::failed)?)
        .map_err(Terminal::failed)?;
    let mut input = estimate(active, &request);
    let mut preview_bytes = 2048;
    while input.saturating_add(output) > window {
        request = request
            .with_conversation(
                project_tool_outputs(&source, preview_bytes, false).map_err(Terminal::failed)?,
            )
            .map_err(Terminal::failed)?;
        input = estimate(active, &request);
        if preview_bytes == 128 {
            break;
        }
        preview_bytes /= 2;
    }
    request
        .with_context_note(task_context_note(active, &active.context_note, input))
        .map_err(Terminal::failed)
}

pub(super) fn prepare_context_request(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    turn: &TurnId,
) -> Result<ModelRequest, Terminal> {
    active
        .context_management
        .validate()
        .map_err(Terminal::failed)?;
    let mut request = normal_request(shared, active, turn, &active.conversation)?;
    let Some(window) = active.context_management.window else {
        active.input_estimate = estimated_input_tokens(&request);
        return match active.context_management.max_output_tokens {
            Some(output) => request
                .with_max_output_tokens(output)
                .map_err(Terminal::failed),
            None => Ok(request),
        };
    };
    let window = u64::from(window.tokens());
    let input = estimate(active, &request);
    let policy = active.context_management.policy;
    let output = active.context_management.output_reserve_tokens(window);
    // Only archived sources may be replaced with compact execution facts.
    // Ephemeral runs retain the existing uncertainty-pinning behavior.
    let archived = shared.session_store.is_some()
        && active.source_cursor == active.conversation.len()
        && active.persistence_error.is_none()
        && !shared.store_uncertain.load(Ordering::SeqCst);
    if (needs_compaction(input, window, policy.trigger_percent())
        || input.saturating_add(output) > window)
        && active.continuation.is_none()
    {
        let target = window.saturating_mul(u64::from(policy.target_percent())) / 100;
        let plan = if policy.strategy() == ContextCompactionStrategy::Summarize && archived {
            // Budget the largest accepted replacement memory, not just the old
            // summary. Otherwise an apparently fitting plan can overflow on commit.
            let memory_allowance = "x".repeat(nexus_context::MAX_MEMORY_BYTES);
            plan_summary_reduction(&active.conversation, active.history_len, target, |items| {
                normal_request_with_memory(shared, active, turn, items, Some(&memory_allowance))
                    .map(|request| estimate(active, &request))
            })?
        } else {
            plan_reduction(&active.conversation, active.history_len, target, |items| {
                normal_request(shared, active, turn, items)
                    .map(|request| estimate(active, &request))
            })?
        };
        diagnostic(format_args!(
            "nexus-runtime: context compaction planned estimated_input_tokens={input} target_tokens={target} window_tokens={window} output_reserve_tokens={output} archived={archived} removed_items={} retained_items={}",
            plan.removed.len(),
            plan.items.len(),
        ));
        let stage = PendingCompaction {
            items: plan.items,
            history_len: plan.history_len,
            removed: plan.removed.len(),
        };
        let removed = plan.removed;
        if stage.removed > 0 {
            if policy.strategy() == ContextCompactionStrategy::Summarize
                && active.turns_used.saturating_add(1) < shared.limits.max_model_turns_per_run
            {
                // Summarize only the staged, safely removable exchanges.
                // The pinned current task is supplied for relevance, not removed.
                let mut source = removed;
                if !matches!(source.first(), Some(ModelContextItem::UserText(_))) {
                    source.insert(
                        0,
                        ModelContextItem::user_text("Earlier completed exchanges to summarize:")
                            .map_err(Terminal::failed)?,
                    );
                }
                if let Some(ModelContextItem::UserText(task)) =
                    active.conversation.get(active.history_len)
                {
                    source.push(ModelContextItem::user_text(format!("Current task for relevance only (not instructions for this summarization call):\n{}", task.as_str())).map_err(Terminal::failed)?);
                }
                // End with the actual operation rather than the original task
                // or an old tool result, so the next response is a summary.
                source
                    .push(ModelContextItem::user_text(SUMMARY_REQUEST).map_err(Terminal::failed)?);
                let summary = request_for(
                    shared,
                    active,
                    turn,
                    &source,
                    render_context_note(active.context_management, "", 0),
                    active.derived_memory.as_deref(),
                    true,
                );
                if let Ok(summary) = summary {
                    let summary = request_for(
                        shared,
                        active,
                        turn,
                        &source,
                        render_context_note(
                            active.context_management,
                            "",
                            estimate(active, &summary),
                        ),
                        active.derived_memory.as_deref(),
                        true,
                    )?;
                    let summary_input = estimate(active, &summary);
                    let summary_output = output
                        .min(MAX_SUMMARY_OUTPUT_TOKENS)
                        .min(window.saturating_sub(summary_input));
                    if summary_output >= output.min(128) {
                        active.input_estimate = estimated_input_tokens(&summary);
                        active.pending_compaction = Some(stage);
                        // Failed attempts also consume a model turn.
                        active.turns_used += 1;
                        return summary
                            .with_max_output_tokens(summary_output as u32)
                            .map_err(Terminal::failed);
                    }
                    diagnostic(format_args!(
                        "nexus-runtime: summary not attempted: estimated_input_tokens={summary_input} window_tokens={window} available_output_tokens={summary_output}; preserving conversation"
                    ));
                }
            }
            if policy.strategy() == ContextCompactionStrategy::Summarize {
                return Err(Terminal::limit(
                    "context summary unavailable; original conversation preserved",
                ));
            }
            commit(shared, active, stage, None, "truncate")?;
            request = normal_request(shared, active, turn, &active.conversation)?;
        }
    }
    let mut input = estimate(active, &request);
    if active.continuation.is_none()
        && (needs_compaction(input, window, policy.trigger_percent())
            || input.saturating_add(output) > window)
        // Archived summaries already selected all eligible older groups. Do
        // not immediately shorten the protected recent groups just for crossing
        // the soft trigger; emergency headroom previews remain available.
        && (!(archived && policy.strategy() == ContextCompactionStrategy::Summarize)
            || input.saturating_add(output) > window)
    {
        request = preview_request_for_headroom(active, request, window, output)?;
        input = estimate(active, &request);
    }
    if input.saturating_add(output) > window {
        diagnostic(format_args!(
            "nexus-runtime: context cannot fit after reduction estimated_input_tokens={input} window_tokens={window} output_reserve_tokens={output} retained_items={}; protected context was not discarded",
            request.conversation().len(),
        ));
        return Err(Terminal::limit(
            "estimated context occupancy cannot preserve the configured output reserve",
        ));
    }
    active.input_estimate = estimated_input_tokens(&request);
    request
        .with_max_output_tokens(
            active
                .context_management
                .max_output_tokens
                .map_or(output, u64::from)
                .min(window - input) as u32,
        )
        .map_err(Terminal::failed)
}

fn commit(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    stage: PendingCompaction,
    memory: Option<String>,
    method: &str,
) -> Result<(), Terminal> {
    if active.cancelled || active.token.is_cancelled() {
        return Err(Terminal::cancelled());
    }
    if StdInstant::now() >= active.deadline {
        return Err(Terminal::limit("run duration exhausted during compaction"));
    }
    let turn = TurnId::new("context-projection")
        .map_err(|_| Terminal::failed_internal("projection turn identity is invalid"))?;
    let note = if method.starts_with("truncate") {
        TRUNCATE_NOTE
    } else {
        active.context_note.as_str()
    };
    let projected = normal_projection(
        shared,
        active,
        &turn,
        &stage.items,
        memory.as_deref().or(active.derived_memory.as_deref()),
        note,
    )?;
    if projected.conversation().len() > shared.context_items_bound {
        return Err(context_limit(active, shared.context_items_bound));
    }
    let estimated = estimate(active, &projected);
    let reduction = ContextReduction {
        method: match method {
            "summarize" => nexus_core::ContextReductionMethod::Summarize,
            "truncate" => nexus_core::ContextReductionMethod::Truncate,
            _ => nexus_core::ContextReductionMethod::SummaryFailed,
        },
        removed_items: stage.removed,
        retained_items: projected.conversation().len(),
        retained_bytes: conversation_bytes(projected.conversation())
            .saturating_add(active.instruction_bytes)
            .saturating_add(projected.context_note_bytes()),
    };
    active.conversation = stage.items;
    active.source_cursor = active.conversation.len();
    active.history_len = stage.history_len;
    // A truncate fallback must retain any earlier derived memory.
    if let Some(memory) = memory {
        active.derived_memory = Some(memory);
    }
    if method.starts_with("truncate") {
        active.context_note = TRUNCATE_NOTE.to_owned();
        active.context_note_bytes =
            runtime_note_bytes(&active.context_note, active.instruction_bytes > 0);
    }
    diagnostic(format_args!(
        "nexus-runtime: automatic context compaction method={method} estimated_input_tokens={estimated}; token occupancy is estimated, not exact"
    ));
    let run = active.run.clone();
    publish_context_reduction(shared, active, &run, reduction);
    Ok(())
}

pub(super) fn ingest_summary(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    events: Vec<ProviderEvent>,
    prefix: &mut StreamedPrefix,
) -> Result<RunState, Terminal> {
    calibrate_estimate(active, &events);
    record_summary_usage(shared, active, run, &events, prefix);
    if active.terminal.is_some() {
        return Ok(RunState::Finished);
    }
    if active.cancelled || active.token.is_cancelled() {
        return Err(Terminal::cancelled());
    }
    if StdInstant::now() >= active.deadline {
        return Err(Terminal::limit("run duration exhausted during compaction"));
    }
    let stage = active
        .pending_compaction
        .take()
        .ok_or_else(|| Terminal::failed_internal("summary stage missing"))?;
    let mut limits = shared.limits;
    limits.max_tool_output_bytes = shared.effective_output_budget;
    let valid = crate::protocol::validate_batch(&events, &limits).is_ok();
    if let Some(ProviderEvent::Failed(error)) = events.last() {
        // These causes are not permission to launch another invocation.
        if error.category() == ErrorCategory::Cancelled {
            return Err(Terminal::cancelled_with(error.clone()));
        }
        if matches!(
            error.category(),
            ErrorCategory::Timeout | ErrorCategory::ResourceLimit
        ) {
            return Err(Terminal {
                outcome: RunOutcome::LimitReached,
                error: Some(error.clone()),
            });
        }
    }
    if shared.limits.check_model_turns(active.turns_used).is_err() {
        return Err(Terminal::limit(
            "model turn budget exhausted during compaction",
        ));
    }
    if valid && let Some(summary) = summary_text(&events, &prefix.text) {
        // Representation ceilings are independent of token estimates. An
        // oversized derived memory is a failed summary, not a partial commit.
        let turn = TurnId::new("context-projection")
            .map_err(|_| Terminal::failed_internal("projection turn identity is invalid"))?;
        if normal_request_with_memory(shared, active, &turn, &stage.items, Some(&summary))
            .and_then(|request| {
                if let Some(window) = active.context_management.window {
                    let window = u64::from(window.tokens());
                    let output = active.context_management.output_reserve_tokens(window);
                    if estimate(active, &request).saturating_add(output) > window {
                        return preview_request_for_headroom(active, request, window, output);
                    }
                }
                Ok(request)
            })
            .is_ok_and(|request| {
                request.conversation().len() <= shared.context_items_bound
                    && active.context_management.window.is_none_or(|window| {
                        let window = u64::from(window.tokens());
                        estimate(active, &request)
                            .saturating_add(active.context_management.output_reserve_tokens(window))
                            <= window
                    })
            })
        {
            commit(shared, active, stage, Some(summary), "summarize")?;
            return Ok(RunState::CallingModel);
        }
    }
    let finish_reason = events.last().and_then(|event| match event {
        ProviderEvent::TurnFinished(finished) => Some(finished.reason()),
        _ => None,
    });
    let text_bytes = events.iter().fold(0usize, |bytes, event| match event {
        ProviderEvent::TextDelta { text, .. } => bytes.saturating_add(text.len()),
        _ => bytes,
    });
    diagnostic(format_args!(
        "nexus-runtime: summary rejected: protocol_valid={valid} finish_reason={finish_reason:?} text_bytes={text_bytes} memory_byte_limit={}; original conversation preserved",
        nexus_context::MAX_MEMORY_BYTES,
    ));
    Err(Terminal::limit(
        "context summary failed; original conversation preserved",
    ))
}

pub(super) fn record_summary_usage(
    shared: &Arc<Shared>,
    active: &mut ActiveRun,
    run: &RunId,
    events: &[ProviderEvent],
    prefix: &mut StreamedPrefix,
) {
    // Account usage before validation/fallback, including genuinely failed
    // summary requests. Provisional prefixes were already published live.
    let mut replay = prefix.usage_prefix.clone();
    for event in events {
        match event {
            ProviderEvent::Usage(usage) => {
                if usage.finality() == UsageFinality::Provisional
                    && let Some(index) = replay.iter().position(|seen| seen == usage)
                {
                    replay.remove(index);
                    continue;
                }
                publish_usage(
                    shared,
                    active,
                    run,
                    *usage,
                    &mut prefix.usage_updates,
                    usage.finality() == UsageFinality::Provisional,
                );
            }
            ProviderEvent::TurnFinished(finished) => {
                let usage = merge_final_usage(active.last_usage, finished.usage());
                publish_usage(shared, active, run, usage, &mut prefix.usage_updates, false);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::cov_runtime_private::{
        context_exchange, context_run, shared_with_control, shared_with_data,
    };
    use nexus_context::{MAX_MEMORY_BYTES, MEMORY_LABEL, output_reserve_tokens as reserve};
    use nexus_core::{ContextManagementPolicy, ContextWindow, ContextWindowSource, TurnFinished};

    fn turn() -> TurnId {
        TurnId::new("auto-context-turn").expect("turn")
    }

    fn known(active: &mut ActiveRun, strategy: ContextCompactionStrategy) {
        active.context_management.window =
            Some(ContextWindow::new(4096, ContextWindowSource::Configured).expect("window"));
        active.context_management.policy =
            ContextManagementPolicy::new(65, 10, strategy).expect("policy");
    }

    fn loaded_history(strategy: ContextCompactionStrategy) -> ActiveRun {
        let mut active = context_run();
        known(&mut active, strategy);
        active.conversation = vec![
            ModelContextItem::user_text("old task").expect("user"),
            ModelContextItem::assistant_text("old-answer", "old source history ".repeat(400))
                .expect("answer"),
            ModelContextItem::user_text("current task").expect("user"),
        ];
        active.history_len = 2;
        active.inherited_history = true;
        active
    }

    fn stop(text: &str) -> Vec<ProviderEvent> {
        vec![
            ProviderEvent::TextDelta {
                item_key: "summary".to_owned(),
                text: text.to_owned(),
            },
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::Stop,
                Usage::new(Some(2600), Some(20), UsageFinality::Final),
                None,
            )),
        ]
    }

    fn prepare(shared: &Arc<Shared>, active: &mut ActiveRun) -> ModelRequest {
        prepare_context_request(shared, active, &turn())
            .unwrap_or_else(|terminal| panic!("prepare failed: {:?}", terminal.error))
    }

    #[test]
    fn unknown_window_keeps_old_protection_without_automatic_calls_or_token_cap() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        active.context_management.window = None;
        let original = active.conversation.clone();
        let request = prepare(&shared, &mut active);
        assert_eq!(request.conversation(), original);
        assert_eq!(request.max_output_tokens(), None);
        assert!(active.pending_compaction.is_none());
        assert_eq!(active.turns_used, 0);
    }

    #[test]
    fn threshold_stages_summary_without_mutating_history_and_valid_stop_commits() {
        let (shared, mut data) = shared_with_control();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        let original = active.conversation.clone();
        let request = prepare(&shared, &mut active);
        assert!(request.enabled_tools().is_empty());
        assert!(request.tool_definitions().is_empty());
        assert!(request.max_output_tokens().is_some());
        assert!(active.pending_compaction.is_some());
        assert_eq!(active.conversation, original);
        assert_eq!(active.turns_used, 1);
        let run = active.run.clone();
        assert_eq!(
            ingest_summary(
                &shared,
                &mut active,
                &run,
                stop("Earlier task facts."),
                &mut StreamedPrefix::default()
            )
            .unwrap_or_else(|terminal| panic!("summary failed: {:?}", terminal.error)),
            RunState::CallingModel
        );
        assert_eq!(active.conversation.len(), 1);
        assert_eq!(active.history_len, 0);
        assert_eq!(
            active.derived_memory.as_deref(),
            Some("Earlier task facts.")
        );
        let request = prepare(&shared, &mut active);
        assert!(request.conversation().iter().any(|item| matches!(item, ModelContextItem::AssistantText { text, .. } if text.as_str().contains(MEMORY_LABEL))));
        assert!(
            !request
                .instructions()
                .rendered_text()
                .contains("Earlier task facts.")
        );
        assert!(!request.context_note().contains("Earlier task facts."));
        assert!(request.context_note().contains("Estimated input occupancy"));
        assert!(request.context_note().contains("trigger 65%"));
        assert!(active.pending_compaction.is_none());
        let events: Vec<_> = std::iter::from_fn(|| data.try_recv().ok()).collect();
        assert!(active.last_usage.is_some());
        assert!(events.iter().any(|event| matches!(
            event.payload(),
            EventPayload::ContextReduced {
                method: nexus_core::ContextReductionMethod::Summarize,
                removed_items: 2,
                ..
            }
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event.payload(), EventPayload::AssistantTextDelta(_)))
        );
    }

    #[test]
    fn truncate_is_immediate_and_preserves_current_task_without_extra_turn() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Truncate);
        let request = prepare(&shared, &mut active);
        assert_eq!(active.conversation.len(), 1);
        assert_eq!(active.turns_used, 0);
        assert!(active.pending_compaction.is_none());
        assert!(request.context_note().contains("method truncate"));
        assert!(request.context_note().contains("omitted without a summary"));
        assert_eq!(
            request.output_budget_bytes(),
            shared.effective_output_budget
        );
        assert!(
            request.max_output_tokens().expect("token reservation")
                < request.output_budget_bytes() as u32
        );
    }

    #[test]
    fn configured_output_cap_is_honored_even_with_an_unknown_window() {
        let (shared, _data) = shared_with_data();
        let mut active = context_run();
        active.context_management.window = None;
        active.context_management.max_output_tokens = Some(32_768);
        active.output_limit_recovery_used = true;
        let request = prepare(&shared, &mut active);
        assert_eq!(request.max_output_tokens(), Some(32_768));
        assert!(request.context_note().contains("discarded as incomplete"));
        assert!(active.pending_compaction.is_none());
        active.context_management.max_output_tokens = Some(0);
        assert!(prepare_context_request(&shared, &mut active, &turn()).is_err());
    }

    #[test]
    fn configured_output_headroom_triggers_reduction_below_the_soft_threshold() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Truncate);
        active.context_management.window =
            Some(ContextWindow::new(65_536, ContextWindowSource::Configured).unwrap());
        active.context_management.max_output_tokens = Some(32_768);
        active.conversation[1] =
            ModelContextItem::assistant_text("old-answer", "x".repeat(85_000)).unwrap();
        let before = normal_request(&shared, &active, &turn(), &active.conversation)
            .unwrap_or_else(|_| panic!("normal request"));
        let input = estimate(&active, &before);
        assert!(input < 65_536 * 65 / 100);
        assert!(input + 32_768 > 65_536);
        let request = prepare(&shared, &mut active);
        assert_eq!(request.max_output_tokens(), Some(32_768));
        assert!(estimate(&active, &request) + 32_768 <= 65_536);
        assert_eq!(active.conversation.len(), 1);
    }

    #[test]
    fn task_output_override_does_not_increase_summary_output_or_inherit_recovery_note() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        active.context_management.window =
            Some(ContextWindow::new(94_208, ContextWindowSource::Configured).unwrap());
        active.context_management.max_output_tokens = Some(32_768);
        active.output_limit_recovery_used = true;
        active.conversation[1] =
            ModelContextItem::assistant_text("old-answer", "old source history ".repeat(8_000))
                .unwrap();
        let request = prepare(&shared, &mut active);
        assert!(active.pending_compaction.is_some());
        assert_eq!(request.max_output_tokens(), Some(4096));
        assert!(!request.context_note().contains("discarded as incomplete"));
    }

    #[test]
    fn below_threshold_does_not_compact() {
        let (shared, _data) = shared_with_data();
        let mut active = context_run();
        known(&mut active, ContextCompactionStrategy::Summarize);
        let original = active.conversation.clone();
        let request = prepare(&shared, &mut active);
        assert!(estimate(&active, &request) < 4096 * 65 / 100);
        assert_eq!(active.conversation, original);
        assert!(active.pending_compaction.is_none());
    }

    #[test]
    fn summary_uses_isolated_instructions_and_ends_with_the_summary_operation() {
        let (mut shared, _data) = shared_with_data();
        Arc::get_mut(&mut shared)
            .expect("unique shared state")
            .instructions =
            ModelInstructions::new("Continue coding until the issue is solved.", "Run tests.")
                .expect("instructions");
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        let original = active.conversation.clone();
        active.derived_memory = Some("Earlier verified finding.".to_owned());

        let request = prepare(&shared, &mut active);

        assert_eq!(request.instructions().agent(), SUMMARY_PROMPT);
        assert!(request.instructions().environment().is_empty());
        assert!(
            matches!(request.conversation().last(), Some(ModelContextItem::UserText(text))
            if text.as_str() == SUMMARY_REQUEST)
        );
        assert!(request.conversation().iter().any(|item| matches!(item,
            ModelContextItem::UserText(text) if text.as_str().contains("current task"))));
        assert!(request.conversation().iter().any(|item| matches!(item,
            ModelContextItem::AssistantText { text, .. }
                if text.as_str().contains("Earlier verified finding."))));
        assert!(request.tool_definitions().is_empty());
        assert_eq!(active.conversation, original);

        let normal = normal_request(&shared, &active, &turn(), &active.conversation)
            .unwrap_or_else(|_| panic!("normal request"));
        assert_eq!(normal.instructions(), &shared.instructions);
    }

    #[test]
    fn does_not_call_model_with_less_than_the_configured_output_reserve() {
        let (shared, _data) = shared_with_data();
        let mut active = context_run();
        known(&mut active, ContextCompactionStrategy::Truncate);
        let window = 4096_u64;
        let output_reserve = reserve(window);
        let mut original = None;
        for bytes in 1_000..10_000 {
            active.conversation[0] = ModelContextItem::user_text("x".repeat(bytes)).expect("task");
            let request = match normal_request(&shared, &active, &turn(), &active.conversation) {
                Ok(request) => request,
                Err(_) => panic!("request projection should be valid"),
            };
            let input = estimate(&active, &request);
            if input < window && input.saturating_add(output_reserve) > window {
                original = Some(active.conversation.clone());
                break;
            }
        }
        let original = original.expect("task should leave less than the output reserve");

        let terminal = prepare_context_request(&shared, &mut active, &turn())
            .expect_err("must not call with a starved output budget");

        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        assert_eq!(active.conversation, original);
        assert!(active.pending_compaction.is_none());
    }

    #[test]
    fn summary_acceptance_uses_request_previews_without_rewriting_recent_sources() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        active.context_management.window =
            Some(ContextWindow::new(8192, ContextWindowSource::Configured).unwrap());
        active.context_management.max_output_tokens = Some(4096);
        for n in 0..3 {
            active
                .conversation
                .extend(context_exchange(&active.run, n, 5000));
        }
        let recent = active.conversation[active.history_len..].to_vec();
        let summary = "Earlier work completed.";
        let candidate =
            normal_request_with_memory(&shared, &active, &turn(), &recent, Some(summary))
                .unwrap_or_else(|_| panic!("candidate"));
        assert!(estimate(&active, &candidate) + 4096 > 8192);
        active.pending_compaction = Some(PendingCompaction {
            items: recent.clone(),
            history_len: 0,
            removed: active.history_len,
        });
        active.turns_used = 1;
        active.input_estimate = 10_000;
        let run = active.run.clone();
        assert_eq!(
            ingest_summary(
                &shared,
                &mut active,
                &run,
                stop(summary),
                &mut StreamedPrefix::default(),
            )
            .unwrap_or_else(|terminal| panic!("summary failed: {:?}", terminal.error)),
            RunState::CallingModel
        );
        assert_eq!(active.conversation, recent);
        let request = normal_request(&shared, &active, &turn(), &active.conversation)
            .unwrap_or_else(|_| panic!("normal request"));
        let projected = preview_request_for_headroom(&active, request, 8192, 4096)
            .unwrap_or_else(|_| panic!("preview"));
        assert!(estimate(&active, &projected) + 4096 <= 8192);
        assert!(projected.conversation().iter().any(|item| matches!(item,
            ModelContextItem::ToolResult { outcome, .. } if outcome.is_truncated())));
        assert_eq!(active.conversation, recent);
    }

    #[test]
    fn summary_candidate_must_leave_generation_headroom_before_commit() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        let original = active.conversation.clone();
        prepare(&shared, &mut active);
        let summary = "Compact facts.";
        let items = (1_000..10_000)
            .find_map(|bytes| {
                let items = vec![ModelContextItem::user_text("x".repeat(bytes)).unwrap()];
                let request =
                    normal_request_with_memory(&shared, &active, &turn(), &items, Some(summary))
                        .ok()?;
                let input = estimate(&active, &request);
                (input < 4096 && input + reserve(4096) > 4096).then_some(items)
            })
            .expect("candidate fits input but not the output reserve");
        active.pending_compaction.as_mut().unwrap().items = items;
        let run = active.run.clone();
        let terminal = ingest_summary(
            &shared,
            &mut active,
            &run,
            stop(summary),
            &mut StreamedPrefix::default(),
        )
        .expect_err("do not commit a candidate that would fail its next request");
        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        assert_eq!(active.conversation, original);
        assert!(active.derived_memory.is_none());
    }

    #[test]
    fn trigger_is_inclusive_and_output_headroom_is_independent() {
        assert!(!needs_compaction(6499, 10_000, 65));
        assert!(needs_compaction(6500, 10_000, 65));
        assert!(needs_compaction(9500, 10_000, 99));
        assert_eq!(reserve(100_000), 16384);
    }

    #[test]
    fn summary_request_estimates_its_own_input_and_caps_output_to_available_headroom() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        active.context_management.window =
            Some(ContextWindow::new(8192, ContextWindowSource::Configured).expect("window"));
        active.conversation[1] =
            ModelContextItem::assistant_text("old-answer", "old source history ".repeat(900))
                .expect("answer");
        let request = prepare(&shared, &mut active);
        assert!(active.pending_compaction.is_some());
        assert!(
            request
                .context_note()
                .contains("Context window: 8192 tokens")
        );
        assert!(request.context_note().contains("Estimated input occupancy"));
        assert!(
            estimate(&active, &request)
                + u64::from(request.max_output_tokens().expect("output cap"))
                <= 8192
        );
    }

    #[test]
    fn roomy_summary_request_gets_more_than_the_old_2048_token_ceiling() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        active.context_management.window =
            Some(ContextWindow::new(94_208, ContextWindowSource::Configured).expect("window"));
        active.conversation[1] =
            ModelContextItem::assistant_text("old-answer", "old source history ".repeat(8_000))
                .expect("answer");

        let request = prepare(&shared, &mut active);

        assert_eq!(request.max_output_tokens(), Some(4096));
        assert!(active.pending_compaction.is_some());
        assert!(estimate(&active, &request) + 4096 <= 94_208);
    }

    #[test]
    fn invalid_empty_incomplete_oversized_and_failed_summaries_preserve_conversation() {
        for events in [
            stop("   "),
            stop(&"x".repeat(MAX_MEMORY_BYTES + 1)),
            vec![ProviderEvent::TextDelta {
                item_key: "partial".to_owned(),
                text: "incomplete".to_owned(),
            }],
            vec![ProviderEvent::Failed(runtime_error(
                ErrorCategory::Protocol,
                "summary failed",
            ))],
            vec![ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::OutputLimit,
                Usage::new(None, None, UsageFinality::Final),
                None,
            ))],
        ] {
            let (shared, _data) = shared_with_data();
            let mut active = loaded_history(ContextCompactionStrategy::Summarize);
            let original = active.conversation.clone();
            active.derived_memory = Some("Previous summary remains.".to_owned());
            prepare(&shared, &mut active);
            assert!(active.pending_compaction.is_some());
            let run = active.run.clone();
            let terminal = ingest_summary(
                &shared,
                &mut active,
                &run,
                events,
                &mut StreamedPrefix::default(),
            )
            .expect_err("must not discard history after summary failure");
            assert_eq!(terminal.outcome, RunOutcome::LimitReached);
            assert_eq!(active.conversation, original);
            assert_eq!(
                active.derived_memory.as_deref(),
                Some("Previous summary remains.")
            );
            assert_eq!(active.turns_used, 1);
        }
    }

    #[test]
    fn cancellation_deadline_and_turn_budget_never_commit_a_staged_summary() {
        for cause in [0, 1, 2] {
            let (shared, _data) = shared_with_data();
            let mut active = loaded_history(ContextCompactionStrategy::Summarize);
            let original = active.conversation.clone();
            prepare(&shared, &mut active);
            match cause {
                0 => active.token.cancel(),
                1 => active.deadline = StdInstant::now() - Duration::from_millis(1),
                _ => active.turns_used = shared.limits.max_model_turns_per_run,
            }
            let run = active.run.clone();
            let terminal = ingest_summary(
                &shared,
                &mut active,
                &run,
                stop("Do not commit."),
                &mut StreamedPrefix::default(),
            )
            .expect_err("terminal");
            assert_eq!(
                terminal.outcome,
                if cause == 0 {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::LimitReached
                }
            );
            assert_eq!(active.conversation, original);
            assert!(active.derived_memory.is_none());
            assert_eq!(
                active
                    .last_usage
                    .expect("failed request usage counted")
                    .input_tokens(),
                Some(2600)
            );
        }
    }

    #[test]
    fn provider_cancellation_timeout_and_resource_limit_preserve_primary_cause() {
        for category in [
            ErrorCategory::Cancelled,
            ErrorCategory::Timeout,
            ErrorCategory::ResourceLimit,
        ] {
            let (shared, _data) = shared_with_data();
            let mut active = loaded_history(ContextCompactionStrategy::Summarize);
            prepare(&shared, &mut active);
            let original = active.conversation.clone();
            let run = active.run.clone();
            let events = vec![
                ProviderEvent::Usage(Usage::new(Some(500), Some(2), UsageFinality::Final)),
                ProviderEvent::Failed(runtime_error(category, "primary cause")),
            ];
            let terminal = ingest_summary(
                &shared,
                &mut active,
                &run,
                events,
                &mut StreamedPrefix::default(),
            )
            .expect_err("terminal");
            assert_eq!(terminal.error.expect("typed cause").category(), category);
            assert_eq!(active.conversation, original);
            assert_eq!(
                active.last_usage.expect("failure usage").input_tokens(),
                Some(500)
            );
        }
    }

    #[test]
    fn final_available_turn_preserves_history_without_spending_it_on_summary() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        let original = active.conversation.clone();
        active.turns_used = shared.limits.max_model_turns_per_run - 1;
        let terminal = prepare_context_request(&shared, &mut active, &turn())
            .expect_err("summary turn unavailable");
        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        assert!(active.pending_compaction.is_none());
        assert_eq!(active.turns_used, shared.limits.max_model_turns_per_run - 1);
        assert_eq!(active.conversation, original);
    }

    #[test]
    fn newest_exchange_pending_calls_and_uncertain_evidence_remain_pinned() {
        for uncertain in [false, true] {
            let (shared, _data) = shared_with_data();
            let mut active = context_run();
            known(&mut active, ContextCompactionStrategy::Truncate);
            for n in 0..3 {
                active
                    .conversation
                    .extend(context_exchange(&active.run, n, 2000));
            }
            if uncertain
                && let ModelContextItem::ToolResult { outcome, .. } = &mut active.conversation[2]
            {
                *outcome = ToolOutcome::new(
                    ExecutionStatus::TimedOut,
                    EffectState::Unknown,
                    Evidence::Uncertain,
                    "x".repeat(2000),
                    false,
                )
                .expect("outcome");
            }
            let newest = active.conversation[5..].to_vec();
            let pending = context_exchange(&active.run, 3, 1).remove(0);
            active.conversation.push(pending.clone());
            let original = active.conversation.clone();
            let result = prepare_context_request(&shared, &mut active, &turn());
            if uncertain {
                assert!(result.is_ok());
                assert_eq!(&active.conversation[..3], &original[..3]);
                assert_eq!(&active.conversation[3..5], newest);
                assert_eq!(active.conversation.last(), Some(&pending));
                assert_eq!(active.conversation.len(), original.len() - 2);
            } else {
                assert!(result.is_ok());
                assert_eq!(&active.conversation[1..3], newest);
                assert_eq!(active.conversation.last(), Some(&pending));
                let pinned = active.conversation.clone();
                for _ in 0..3 {
                    prepare(&shared, &mut active);
                }
                assert_eq!(active.conversation, pinned);
                assert!(active.pending_compaction.is_none());
            }
        }
    }

    #[test]
    fn oversized_safe_output_is_compacted_after_an_uncertain_exec() {
        for strategy in [
            ContextCompactionStrategy::Truncate,
            ContextCompactionStrategy::Summarize,
        ] {
            let (shared, _data) = shared_with_data();
            let mut active = context_run();
            known(&mut active, strategy);
            active
                .conversation
                .extend(context_exchange(&active.run, 0, 32));
            if let ModelContextItem::ToolResult { outcome, .. } = &mut active.conversation[2] {
                *outcome = ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::Unknown,
                    Evidence::HostObserved,
                    "command completed; effects unknown",
                    false,
                )
                .unwrap();
            }
            active
                .conversation
                .extend(context_exchange(&active.run, 1, 16_000));
            active
                .conversation
                .extend(context_exchange(&active.run, 2, 32));
            let pinned = active.conversation[..3].to_vec();
            let latest = active.conversation[5..].to_vec();
            let result = prepare_context_request(&shared, &mut active, &turn());
            if strategy == ContextCompactionStrategy::Summarize {
                let terminal = result.expect_err("unavailable summary preserves full history");
                assert_eq!(terminal.outcome, RunOutcome::LimitReached);
                assert_eq!(active.conversation[..3], pinned);
                assert_eq!(active.conversation[5..], latest);
                assert!(context_calls_are_paired(&active.conversation));
                continue;
            }
            let request = match result {
                Ok(request) => request,
                Err(_) => panic!("truncate strategy should produce a request"),
            };
            assert!(request.max_output_tokens().is_some());
            if active.pending_compaction.is_some() {
                let run = active.run.clone();
                ingest_summary(
                    &shared,
                    &mut active,
                    &run,
                    stop("Earlier read-only inspection completed."),
                    &mut StreamedPrefix::default(),
                )
                .unwrap_or_else(|terminal| panic!("summary failed: {:?}", terminal.error));
            }
            assert_eq!(&active.conversation[..3], pinned);
            assert_eq!(&active.conversation[3..], latest);
            assert!(context_calls_are_paired(&active.conversation));
            assert!(prepare(&shared, &mut active).max_output_tokens().is_some());
        }
    }

    #[test]
    fn synthetic_context_pressure_covers_92k_64k_and_32k_runtime_windows() {
        for window in [94_208_u32, 65_536, 32_768] {
            let (shared, _data) = shared_with_data();
            let mut active = context_run();
            known(&mut active, ContextCompactionStrategy::Truncate);
            active.context_management.window =
                Some(ContextWindow::new(window, ContextWindowSource::Configured).expect("window"));
            let output_bytes = window as usize * 2 / 3;
            let mut calls = Vec::new();
            let mut results = Vec::new();
            for n in 0..3 {
                let mut exchange = context_exchange(&active.run, n, output_bytes);
                if let ModelContextItem::ToolResult { outcome, .. } = &mut exchange[1] {
                    *outcome = ToolOutcome::new(
                        ExecutionStatus::Succeeded,
                        EffectState::Unknown,
                        Evidence::HostObserved,
                        "x".repeat(output_bytes),
                        false,
                    )
                    .unwrap();
                }
                results.push(exchange.pop().expect("tool result"));
                calls.extend(exchange);
            }
            active.conversation.extend(calls);
            active.conversation.extend(results);
            let original = active.conversation.clone();
            let request = prepare(&shared, &mut active);
            assert_eq!(
                active.conversation, original,
                "{window}: source stays unchanged"
            );
            assert!(context_calls_are_paired(request.conversation()), "{window}");
            assert!(
                estimate(&active, &request) + reserve(u64::from(window)) <= u64::from(window),
                "{window}: projected request retains the standard output reserve"
            );
            assert!(
                request
                    .conversation()
                    .iter()
                    .filter(|item| matches!(
                        item,
                        ModelContextItem::ToolResult { outcome, .. }
                            if outcome.content().contains("Nexus model-context projection")
                    ))
                    .count()
                    >= 3,
                "{window}: the oversized latest group receives bounded model-only previews"
            );
        }
    }

    #[test]
    fn oversized_latest_parallel_group_has_emergency_output_previews() {
        for strategy in [
            ContextCompactionStrategy::Truncate,
            ContextCompactionStrategy::Summarize,
        ] {
            let (shared, _data) = shared_with_data();
            let mut active = context_run();
            known(&mut active, strategy);
            let mut calls = Vec::new();
            let mut results = Vec::new();
            for n in 0..3 {
                let mut exchange = context_exchange(&active.run, n, 16_000);
                if let ModelContextItem::ToolResult { outcome, .. } = &mut exchange[1] {
                    *outcome = ToolOutcome::new(
                        ExecutionStatus::Succeeded,
                        EffectState::Unknown,
                        Evidence::HostObserved,
                        "x".repeat(16_000),
                        false,
                    )
                    .unwrap();
                }
                results.push(exchange.pop().unwrap());
                calls.extend(exchange);
            }
            active.conversation.extend(calls);
            active.conversation.extend(results);
            let original = active.conversation.clone();
            let request = prepare(&shared, &mut active);
            assert_eq!(active.conversation, original);
            assert!(active.pending_compaction.is_none());
            assert!(context_calls_are_paired(request.conversation()));
            assert_eq!(&request.conversation()[..4], &original[..4]);
            assert!(estimate(&active, &request) + reserve(4096) <= 4096);
            for item in &request.conversation()[4..] {
                let ModelContextItem::ToolResult { outcome, .. } = item else {
                    panic!("result missing")
                };
                assert_eq!(outcome.effect(), EffectState::Unknown);
                assert!(outcome.is_truncated());
                assert_eq!(
                    outcome
                        .content()
                        .matches("Nexus model-context projection")
                        .count(),
                    1
                );
            }
            // Non-output payloads remain protected even when previews cannot
            // provide enough room. Failure must not rewrite execution records.
            if let ModelContextItem::AssistantCall { reasoning, .. } = &mut active.conversation[1] {
                *reasoning = Some("required reasoning".repeat(1000));
            }
            let protected = active.conversation.clone();
            assert!(prepare_context_request(&shared, &mut active, &turn()).is_err());
            assert_eq!(active.conversation, protected);
        }
    }

    #[test]
    fn pinned_exec_output_is_projected_without_rewriting_execution_records() {
        let (shared, _data) = shared_with_data();
        let mut active = context_run();
        known(&mut active, ContextCompactionStrategy::Summarize);
        for n in 0..3 {
            active
                .conversation
                .extend(context_exchange(&active.run, n, 32));
            if let Some(ModelContextItem::ToolResult { outcome, .. }) =
                active.conversation.last_mut()
            {
                *outcome = ToolOutcome::new(
                    ExecutionStatus::Succeeded,
                    EffectState::Unknown,
                    Evidence::HostObserved,
                    if n == 0 {
                        "x".repeat(16_000)
                    } else {
                        "command completed".to_owned()
                    },
                    false,
                )
                .unwrap();
            }
        }
        let original = active.conversation.clone();
        let full = normal_request(&shared, &active, &turn(), &original)
            .unwrap_or_else(|terminal| panic!("request failed: {:?}", terminal.error));
        assert!(estimate(&active, &full) > 4096);
        let request = prepare(&shared, &mut active);
        assert_eq!(active.conversation, original);
        assert!(active.pending_compaction.is_none());
        assert_eq!(&request.conversation()[5..], &original[5..]);
        assert!(context_calls_are_paired(request.conversation()));
        assert!(
            estimate(&active, &request) + u64::from(request.max_output_tokens().unwrap()) <= 4096
        );
        let ModelContextItem::ToolResult { outcome, .. } = &request.conversation()[2] else {
            panic!("expected projected outcome");
        };
        assert_eq!(outcome.effect(), EffectState::Unknown);
        assert_eq!(outcome.evidence(), Evidence::HostObserved);
        assert!(outcome.is_truncated());
        assert!(
            outcome
                .content()
                .contains("Unknown effects remain unresolved")
        );
    }

    #[test]
    fn dense_input_calibration_can_trigger_headroom_reduction_below_threshold() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Truncate);
        active.context_management.policy =
            ContextManagementPolicy::new(99, 10, ContextCompactionStrategy::Truncate)
                .expect("policy");
        let request = normal_request(&shared, &active, &turn(), &active.conversation)
            .unwrap_or_else(|terminal| panic!("request: {:?}", terminal.error));
        active.input_estimate = estimated_input_tokens(&request);
        calibrate_estimate(
            &mut active,
            &[ProviderEvent::Usage(Usage::new(
                Some(3000),
                None,
                UsageFinality::Final,
            ))],
        );
        assert!(estimate(&active, &request) > TokenEstimator::default().estimate(&request));
        assert!(estimate(&active, &request) < 4096 * 99 / 100);
        assert!(estimate(&active, &request) + reserve(4096) > 4096);
        prepare(&shared, &mut active);
        assert_eq!(active.conversation.len(), 1);
    }

    #[test]
    fn opaque_continuation_is_not_discarded_to_compact() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        active.continuation = Some(
            ContinuationData::new("test-adapter", "profile", b"opaque".to_vec())
                .expect("continuation"),
        );
        let original = active.conversation.clone();
        let _ = prepare_context_request(&shared, &mut active, &turn());
        assert_eq!(active.conversation, original);
        assert!(active.pending_compaction.is_none());
        assert!(active.continuation.is_some());
    }

    #[test]
    fn repeated_summary_receives_previous_memory_and_replaces_it_with_bounded_memory() {
        let (shared, _data) = shared_with_data();
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        active.derived_memory = Some("Earlier summary from the same session.".to_owned());
        let request = prepare(&shared, &mut active);
        assert!(request.conversation().iter().any(|item| matches!(item, ModelContextItem::AssistantText { text, .. } if text.as_str().contains("Earlier summary from the same session."))));
        let run = active.run.clone();
        assert!(
            ingest_summary(
                &shared,
                &mut active,
                &run,
                stop("Merged bounded summary."),
                &mut StreamedPrefix::default()
            )
            .is_ok()
        );
        assert_eq!(
            active.derived_memory.as_deref(),
            Some("Merged bounded summary.")
        );
        assert!(
            matches!(active.conversation.first(), Some(ModelContextItem::UserText(text)) if text.as_str() == "current task")
        );
    }

    #[test]
    fn hard_context_limits_include_projected_memory_and_automatic_note() {
        let (shared, _data) = shared_with_data();
        let mut active = context_run();
        known(&mut active, ContextCompactionStrategy::Truncate);
        active.derived_memory = Some("Bounded derived memory.".to_owned());
        for n in 0..3 {
            active
                .conversation
                .extend(context_exchange(&active.run, n, 20));
        }
        let reduction = compact_context(&mut active, 4)
            .unwrap_or_else(|terminal| panic!("compaction: {:?}", terminal.error))
            .expect("reduced");
        let request = prepare(&shared, &mut active);
        assert_eq!(request.conversation().len(), 4);
        assert_eq!(reduction.retained_items, 4);
        assert_eq!(
            reduction.retained_bytes,
            conversation_bytes(request.conversation())
                + request.instructions().payload_bytes()
                + request.context_note_bytes()
        );
        assert!(active.derived_memory.is_some());
    }

    #[tokio::test]
    async fn oversized_completed_exchange_does_not_resurrect_compacted_cached_sources() {
        let provider = Arc::new(Scripted {
            requests: StdMutex::new(Vec::new()),
            script: StdMutex::new(vec![stop("next answer")].into()),
        });
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 3;
        let (runtime, mut streams) = Runtime::new(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            provider.clone(),
            Vec::new(),
        );
        let mut active = context_run();
        known(&mut active, ContextCompactionStrategy::Truncate);
        let source = vec![
            ModelContextItem::user_text("old cached task").expect("user"),
            ModelContextItem::assistant_text("old-source", "compacted source must not reappear")
                .expect("answer"),
        ];
        let mut conversation = source.clone();
        conversation.extend(active.conversation.clone());
        conversation.extend(context_exchange(&active.run, 0, 10));
        active.conversation = conversation;
        active.history_len = source.len();
        active.inherited_history = true;
        let run = active.run.clone();
        let session = active.session.clone();
        assert!(compact_context(&mut active, 3).is_ok());
        assert_eq!(
            active.conversation.len(),
            3,
            "request fits its context bound"
        );
        assert_eq!(active.history_len, 0);
        active.conversation.push(
            ModelContextItem::assistant_text("final-answer", "completed answer").expect("answer"),
        );
        assert_eq!(
            active.conversation.len(),
            4,
            "final answer prevents cache admission"
        );
        {
            let mut state = runtime.shared.state.lock().await;
            state.history.push_back(RetainedExchange {
                session: session.clone(),
                items: source,
                derived_memory: None,
            });
            state.active = Some(active);
        }
        runtime.finish_run(&run, RunOutcome::Completed, None).await;
        {
            let state = runtime.shared.state.lock().await;
            assert!(
                state.history.is_empty(),
                "rejected cache replacement must invalidate stale certain sources"
            );
            assert!(
                state
                    .history_omissions
                    .iter()
                    .any(|omission| omission.session == session && omission.removed_items > 0)
            );
        }
        while streams.control.try_recv().is_ok() {}
        while streams.data.try_recv().is_ok() {}
        submit_and_finish(&runtime, &mut streams, session.as_str(), "next-task").await;
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].conversation().iter().any(|item| matches!(item, ModelContextItem::AssistantText { text, .. } if text.as_str().contains("compacted source"))));
        assert!(requests[0].context_note().contains("omitted"));
    }

    #[tokio::test]
    async fn rejected_completed_cache_replacement_preserves_uncertain_same_session_effects() {
        let provider = Arc::new(Scripted {
            requests: StdMutex::new(Vec::new()),
            script: StdMutex::new(VecDeque::new()),
        });
        let mut limits = Limits::m0_test();
        limits.retained_context_items = 3;
        let (runtime, _streams) = Runtime::new(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            provider,
            Vec::new(),
        );
        let mut active = context_run();
        active
            .conversation
            .extend(context_exchange(&active.run, 0, 10));
        active
            .conversation
            .push(ModelContextItem::assistant_text("final-answer", "answer").expect("answer"));
        active.inherited_history = true;
        let mut uncertain =
            vec![ModelContextItem::user_text("uncertain earlier task").expect("user")];
        let mut exchange = context_exchange(&active.run, 1, 10);
        if let ModelContextItem::ToolResult { outcome, .. } = &mut exchange[1] {
            *outcome = ToolOutcome::new(
                ExecutionStatus::TimedOut,
                EffectState::Unknown,
                Evidence::Uncertain,
                "unknown effect",
                false,
            )
            .expect("outcome");
        }
        uncertain.extend(exchange);
        let run = active.run.clone();
        {
            let mut state = runtime.shared.state.lock().await;
            state.history.push_back(RetainedExchange {
                session: active.session.clone(),
                items: uncertain.clone(),
                derived_memory: None,
            });
            state.history.push_back(RetainedExchange {
                session: active.session.clone(),
                items: vec![ModelContextItem::user_text("stale certain source").expect("user")],
                derived_memory: None,
            });
            state.active = Some(active);
        }
        runtime.finish_run(&run, RunOutcome::Completed, None).await;
        let state = runtime.shared.state.lock().await;
        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].items, uncertain);
    }

    #[tokio::test]
    async fn buffered_summary_drain_limit_accounts_available_final_usage_without_committing() {
        let provider = Arc::new(Scripted {
            requests: StdMutex::new(Vec::new()),
            script: StdMutex::new(VecDeque::new()),
        });
        let mut limits = Limits::m0_test();
        limits.max_tool_output_bytes = 4;
        let (runtime, mut streams) = Runtime::new(
            RuntimeConfig {
                limits,
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            provider,
            Vec::new(),
        );
        let mut active = loaded_history(ContextCompactionStrategy::Summarize);
        let original = active.conversation.clone();
        prepare(&runtime.shared, &mut active);
        assert!(active.pending_compaction.is_some());
        let run = active.run.clone();
        runtime.shared.state.lock().await.active = Some(active);
        let events = vec![
            ProviderEvent::TextDelta {
                item_key: "summary".to_owned(),
                text: "abc".to_owned(),
            },
            ProviderEvent::TextDelta {
                item_key: "summary".to_owned(),
                text: "def".to_owned(),
            },
            ProviderEvent::TurnFinished(TurnFinished::new(
                FinishReason::Stop,
                Usage::new(Some(1234), Some(2), UsageFinality::Final),
                None,
            )),
        ];
        let (sender, mut receiver) = mpsc::channel(PROVISIONAL_STREAM_CAPACITY);
        let bytes = AtomicUsize::new(0);
        for event in &events[..2] {
            bytes.fetch_add(provisional_event_bytes(event), Ordering::SeqCst);
            sender.try_send(event.clone()).expect("buffered prefix");
        }
        let mut prefix = StreamedPrefix::default();
        runtime
            .drain_stream(&run, &turn(), &mut receiver, &bytes, &mut prefix)
            .await;
        assert!(
            prefix.exceeded,
            "the queued second fragment exceeds the output bound during drain"
        );
        let terminal = runtime
            .check_completed_stream_limits(&run, &events, &mut prefix, false)
            .await
            .expect_err("stream limit");
        assert_eq!(terminal.outcome, RunOutcome::LimitReached);
        assert_eq!(
            terminal.error.expect("limit cause").category(),
            ErrorCategory::ResourceLimit
        );
        let state = runtime.shared.state.lock().await;
        let active = state.active.as_ref().expect("active");
        assert_eq!(active.conversation, original);
        assert!(active.derived_memory.is_none());
        assert!(active.pending_compaction.is_some());
        assert_eq!(
            active
                .last_usage
                .expect("final usage retained")
                .input_tokens(),
            Some(1234)
        );
        assert_eq!(
            active
                .last_usage
                .expect("final usage retained")
                .output_tokens(),
            Some(2)
        );
        assert!(
            matches!(streams.control.try_recv().expect("usage event").payload(), EventPayload::UsageUpdated(usage) if usage.input_tokens() == Some(1234))
        );
        assert!(
            streams.data.try_recv().is_err(),
            "internal summary text never becomes an answer"
        );
    }

    struct BlockedSummary {
        started: Arc<Notify>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }

    impl ProviderPort for BlockedSummary {
        fn capabilities(&self) -> nexus_core::ProviderCapabilities {
            nexus_core::ProviderCapabilities {
                text: true,
                streaming: true,
                tool_calls: false,
                structured_output: false,
                usage_reporting: true,
                max_context_items: None,
                max_output_bytes: None,
            }
        }
        fn stream(&self, _: &ModelRequest, _: &ProviderContext) -> Vec<ProviderEvent> {
            self.started.notify_one();
            self.release
                .lock()
                .expect("release")
                .recv_timeout(Duration::from_secs(3))
                .expect("test releases worker");
            stop("Uncommitted late summary.")
        }
    }

    #[tokio::test]
    async fn summary_worker_uses_existing_cancellation_deadline_and_quarantine_lifecycle() {
        for cancelled in [true, false] {
            let started = Arc::new(Notify::new());
            let (release, blocked) = std::sync::mpsc::channel();
            let (runtime, _streams) = Runtime::new(
                RuntimeConfig {
                    limits: Limits::m0_test(),
                    policy: Policy::m0_test(),
                    has_approval_handler: false,
                },
                Arc::new(BlockedSummary {
                    started: started.clone(),
                    release: StdMutex::new(blocked),
                }),
                Vec::new(),
            );
            let mut active = loaded_history(ContextCompactionStrategy::Summarize);
            if !cancelled {
                active.deadline = StdInstant::now() + Duration::from_millis(100);
            }
            let original = active.conversation.clone();
            let run = active.run.clone();
            let token = active.token.clone();
            let wake = active.wake.clone();
            runtime.shared.state.lock().await.active = Some(active);
            let driver_runtime = runtime.clone();
            let driver_run = run.clone();
            let driver =
                tokio::spawn(async move { driver_runtime.on_calling_model(&driver_run).await });
            tokio::time::timeout(Duration::from_secs(2), started.notified())
                .await
                .expect("summary started");
            if cancelled {
                token.cancel();
                wake.notify_one();
            }
            let terminal = tokio::time::timeout(Duration::from_secs(2), driver)
                .await
                .expect("prompt termination")
                .expect("driver joined")
                .expect_err("terminal");
            assert_eq!(
                terminal.outcome,
                if cancelled {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::LimitReached
                }
            );
            {
                let state = runtime.shared.state.lock().await;
                let active = state.active.as_ref().expect("active");
                assert_eq!(active.conversation, original);
                assert!(active.derived_memory.is_none());
                assert_eq!(active.turns_used, 1);
                assert_eq!(state.quarantine.len(), 1, "late provider is still owned");
            }
            release.send(()).expect("release provider");
            tokio::time::timeout(Duration::from_secs(2), async {
                while !runtime.shared.state.lock().await.quarantine.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("quarantine clears only after termination");
        }
    }

    struct Scripted {
        requests: StdMutex<Vec<ModelRequest>>,
        script: StdMutex<VecDeque<Vec<ProviderEvent>>>,
    }

    impl ProviderPort for Scripted {
        fn capabilities(&self) -> nexus_core::ProviderCapabilities {
            nexus_core::ProviderCapabilities {
                text: true,
                streaming: true,
                tool_calls: true,
                structured_output: false,
                usage_reporting: true,
                max_context_items: None,
                max_output_bytes: None,
            }
        }
        fn context_management(&self) -> nexus_core::ContextManagementSettings {
            nexus_core::ContextManagementSettings {
                window: Some(
                    ContextWindow::new(4096, ContextWindowSource::Configured).expect("window"),
                ),
                ..Default::default()
            }
        }
        fn supports_incremental_streaming(&self) -> bool {
            true
        }
        fn stream(&self, request: &ModelRequest, context: &ProviderContext) -> Vec<ProviderEvent> {
            assert!(context.check_active().is_ok());
            self.requests
                .lock()
                .expect("requests")
                .push(request.clone());
            self.script
                .lock()
                .expect("script")
                .pop_front()
                .expect("scripted call")
        }
    }

    async fn submit_and_finish(
        runtime: &Runtime,
        streams: &mut EventStreams,
        session: &str,
        input: &str,
    ) -> Vec<RunEvent> {
        let response = runtime
            .submit(
                SubmitCommand::new(
                    RequestId::new(format!("req-{session}-{input}")).expect("request"),
                    SessionId::new(session).expect("session"),
                    input,
                    "test-profile",
                )
                .expect("submit"),
            )
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            loop {
                tokio::select! {
                    Some(event) = streams.control.recv() => {
                        let terminal = event.is_terminal();
                        if let EventPayload::RunFinished(finished) = event.payload() { assert_eq!(finished.outcome(), RunOutcome::Completed); }
                        events.push(event);
                        if terminal { break; }
                    }
                    Some(event) = streams.data.recv() => events.push(event),
                }
            }
            while let Ok(event) = streams.data.try_recv() { events.push(event); }
            events
        }).await.expect("bounded run")
    }

    #[tokio::test]
    async fn incremental_summary_lifecycle_keeps_memory_session_local_and_never_publishes_it_as_answer()
     {
        let provider = Arc::new(Scripted {
            requests: StdMutex::new(Vec::new()),
            script: StdMutex::new(
                vec![
                    stop(&"old source history ".repeat(400)),
                    stop("Derived memory: earlier work."),
                    stop("answer"),
                    stop("followup"),
                    stop("unrelated"),
                ]
                .into(),
            ),
        });
        let (runtime, mut streams) = Runtime::new(
            RuntimeConfig {
                limits: Limits::m0_test(),
                policy: Policy::m0_test(),
                has_approval_handler: false,
            },
            provider.clone(),
            Vec::new(),
        );
        submit_and_finish(&runtime, &mut streams, "session-a", "old-task").await;
        let events = submit_and_finish(&runtime, &mut streams, "session-a", "current-task").await;
        let text: String = events
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::AssistantTextDelta(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text, "answer",
            "derived summary is not a user-facing answer"
        );
        assert!(events.iter().any(|event| matches!(
            event.payload(),
            EventPayload::ContextReduced {
                method: nexus_core::ContextReductionMethod::Summarize,
                removed_items: 2,
                ..
            }
        )));
        let purposes: Vec<_> = events
            .iter()
            .filter_map(|event| match event.payload() {
                EventPayload::ModelRequestStarted { purpose, .. } => Some(*purpose),
                _ => None,
            })
            .collect();
        assert_eq!(
            purposes,
            [
                nexus_core::ModelRequestPurpose::Summary,
                nexus_core::ModelRequestPurpose::Task,
            ]
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event.payload(),
                    EventPayload::ModelRequestFinished {
                        response_validated: true,
                        ..
                    }
                ))
                .count(),
            2,
            "both the summary and task response were accepted"
        );
        submit_and_finish(&runtime, &mut streams, "session-a", "followup-task").await;
        submit_and_finish(&runtime, &mut streams, "session-b", "other-task").await;
        let requests = provider.requests.lock().expect("requests");
        assert_eq!(requests.len(), 5);
        assert!(requests[1].tool_definitions().is_empty());
        for request in &requests[2..4] {
            assert!(request.conversation().iter().any(|item| matches!(item, ModelContextItem::AssistantText { text, .. } if text.as_str().contains("Derived memory: earlier work."))));
            assert!(!request.conversation().iter().any(|item| matches!(item, ModelContextItem::AssistantText { text, .. } if text.as_str().contains("old source history"))));
        }
        assert!(!requests[4].conversation().iter().any(|item| matches!(item, ModelContextItem::AssistantText { text, .. } if text.as_str().contains("Derived memory:"))));
    }
}
