//! Provider batch validation: pure pre-publication checks for one invocation.
//!
//! `validate_batch` is the runtime-side trust boundary between an adapter's
//! normalized event list and publication/admission. The adapter owns wire
//! parsing; this module owns only shape, identity, ordering, and finite-budget
//! checks over already-normalized [`ProviderEvent`] values. It performs no
//! JSON parsing (candidate argument JSON is validated by the admission path),
//! no I/O, and no capability negotiation: the runtime checks declared
//! capabilities and any request-lowered output budget separately before
//! invoking or admitting.
//!
//! # Contract enforced
//!
//! - Exactly one terminal event, and it must be last: `TurnFinished` or
//!   `Failed` followed by nothing. Missing, duplicated, or non-final
//!   terminals are protocol failures.
//! - A `Failed` terminal never dispatches: partial text, progress, usage, and
//!   candidates may remain for presentation, but the caller discards every
//!   candidate. No success-only agreement checks run.
//! - A successful `TurnFinished` agrees with its candidates: `Stop` carries no
//!   candidate; `ToolCalls` carries at least one; `Refusal`, `Incomplete`, and
//!   `OutputLimit` never dispatch, so candidates and unfinished progress are
//!   tolerated there and the caller must discard them.
//! - Dispatchable candidates are unambiguous: `ToolCallReady` item keys and
//!   provider references are unique, text item keys never collide with call
//!   item keys, every `ToolCallDelta` key resolves to a candidate, progress is
//!   nondecreasing per key, and no progress follows a completed candidate.
//! - Identities are bounded: text/progress item keys, candidate item keys, and
//!   candidate provider references pass [`ItemKey`]/[`ProviderRef`] bounds.
//! - Finite budgets: at most [`MAX_BATCH_EVENTS`] events and
//!   [`MAX_BATCH_BYTES`] aggregate payload bytes; aggregate text stays within
//!   `limits.max_tool_output_bytes`; every advertised argument progress and
//!   candidate argument text stays within `limits.max_arg_assembly_bytes`.
//! - Usage is honest: terminal usage is final ([`TurnFinished::validate`]),
//!   no `Usage` event follows a final one, and a terminal record never
//!   contradicts already-final counters. Missing counters stay unknown and
//!   are never treated as zero.
//!
//! Interleaving across items is preserved as received: text fragments for one
//! item may repeat and may interleave with other items, and this validator
//! never reorders events. It only rejects a per-item progress sequence that
//! moves backwards or continues after completion.
//!
//! [`TurnFinished::validate`]: nexus_core::TurnFinished::validate

use std::collections::{HashMap, HashSet};

use nexus_core::{
    AgentError, ErrorCategory, FinishReason, ItemKey, Limits, ProviderEvent, ProviderRef,
    RetryGuidance, Usage, UsageFinality,
};

/// Maximum events accepted in one provider batch (M0 representation choice,
/// not a product default): one finite bound over the adapter's whole event
/// list, including the terminal event.
pub const MAX_BATCH_EVENTS: usize = 4_096;

/// Maximum aggregate payload bytes accepted in one provider batch (M0
/// representation choice, not a product default): item keys, text, candidate
/// identities and argument text, and terminal continuation/error text. One
/// MiB comfortably admits a maximal declared-order turn (per-turn call bound
/// times the per-call assembly budget) while still bounding everything else.
pub const MAX_BATCH_BYTES: usize = 1_048_576;

/// Validates one fully returned provider batch before the runtime publishes
/// or admits anything from it.
///
/// `limits` supplies the effective finite budgets. Aggregate text is checked
/// against [`Limits::max_tool_output_bytes`] (the runtime may additionally
/// lower the output budget per request and checks that separately), argument
/// progress and candidate argument text against
/// [`Limits::max_arg_assembly_bytes`], and the batch as a whole against
/// [`MAX_BATCH_EVENTS`] and [`MAX_BATCH_BYTES`].
///
/// Errors are pre-redacted static diagnostics: malformed framing, identity,
/// ordering, or finish/usage agreement is [`ErrorCategory::Protocol`];
/// exhausted finite budgets are [`ErrorCategory::ResourceLimit`]; an invalid
/// bounded identity keeps the core constructor's
/// [`ErrorCategory::InvalidInput`]. Nothing is published, dispatched, or
/// mutated here; callers must discard candidates from a rejected batch and
/// from any `Failed` or non-dispatching terminal.
pub(crate) fn validate_batch(events: &[ProviderEvent], limits: &Limits) -> Result<(), AgentError> {
    if events.len() > MAX_BATCH_EVENTS {
        return Err(limit_error("provider batch exceeds the event-count budget"));
    }
    let Some((terminal, prefix)) = events.split_last() else {
        return Err(protocol_error("provider batch is empty"));
    };
    if !terminal.is_terminal() {
        return Err(protocol_error(
            "provider batch does not end with a terminal event",
        ));
    }
    if prefix.iter().any(ProviderEvent::is_terminal) {
        return Err(protocol_error(
            "provider batch contains more than one terminal event",
        ));
    }

    let mut scan = Scan::default();
    for event in events {
        scan.observe(event, limits)?;
    }
    scan.finish(terminal)
}

/// One linear-pass accumulation over a batch. Dispatch-safety violations are
/// recorded as flags and only rejected once the terminal turns out to be a
/// dispatchable success; a failed or non-dispatching terminal never needs
/// them.
#[derive(Default)]
struct Scan {
    text_bytes: usize,
    payload_bytes: usize,
    text_keys: HashSet<String>,
    call_keys: HashSet<String>,
    progress: HashMap<String, Progress>,
    ready_keys: HashSet<String>,
    ready_refs: HashSet<String>,
    duplicate_ready_key: bool,
    duplicate_ready_ref: bool,
    text_call_collision: bool,
    post_ready_progress: bool,
    nonmonotonic_progress: bool,
    final_usage: Option<Usage>,
    usage_after_final: bool,
}

/// Per-call-item argument progress state.
#[derive(Default)]
struct Progress {
    last_bytes: usize,
    ready: bool,
}

impl Scan {
    /// Checks and records one event. Only universal checks (bounds,
    /// identities, usage finality, batch payload) fail here; per-terminal
    /// agreement is evaluated by [`Scan::finish`].
    fn observe(&mut self, event: &ProviderEvent, limits: &Limits) -> Result<(), AgentError> {
        self.payload_bytes = self
            .payload_bytes
            .saturating_add(event_payload_bytes(event));
        if self.payload_bytes > MAX_BATCH_BYTES {
            return Err(limit_error(
                "provider batch exceeds the payload byte budget",
            ));
        }
        match event {
            ProviderEvent::TextDelta { item_key, text } => {
                ItemKey::new(item_key.as_str())?;
                self.text_bytes = self.text_bytes.saturating_add(text.len());
                if self.text_bytes > limits.max_tool_output_bytes {
                    return Err(limit_error("provider text exceeds the output byte budget"));
                }
                if self.call_keys.contains(item_key.as_str()) {
                    self.text_call_collision = true;
                }
                self.text_keys.insert(item_key.clone());
            }
            ProviderEvent::ToolCallDelta {
                item_key,
                assembled_bytes,
            } => {
                ItemKey::new(item_key.as_str())?;
                if *assembled_bytes > limits.max_arg_assembly_bytes {
                    return Err(limit_error(
                        "tool argument progress exceeds the assembly byte budget",
                    ));
                }
                if self.text_keys.contains(item_key.as_str()) {
                    self.text_call_collision = true;
                }
                self.call_keys.insert(item_key.clone());
                let progress = self.progress.entry(item_key.clone()).or_default();
                if *assembled_bytes < progress.last_bytes {
                    self.nonmonotonic_progress = true;
                }
                if progress.ready {
                    self.post_ready_progress = true;
                }
                progress.last_bytes = *assembled_bytes;
            }
            ProviderEvent::ToolCallReady(candidate) => {
                ItemKey::new(candidate.item_key())?;
                ProviderRef::new(candidate.provider_ref())?;
                if candidate.arguments_json().len() > limits.max_arg_assembly_bytes {
                    return Err(limit_error(
                        "tool call candidate exceeds the assembly byte budget",
                    ));
                }
                if self.text_keys.contains(candidate.item_key()) {
                    self.text_call_collision = true;
                }
                self.call_keys.insert(candidate.item_key().to_owned());
                self.progress
                    .entry(candidate.item_key().to_owned())
                    .or_default()
                    .ready = true;
                if !self.ready_keys.insert(candidate.item_key().to_owned()) {
                    self.duplicate_ready_key = true;
                }
                if !self.ready_refs.insert(candidate.provider_ref().to_owned()) {
                    self.duplicate_ready_ref = true;
                }
            }
            ProviderEvent::Usage(usage) => {
                if self.final_usage.is_some() {
                    self.usage_after_final = true;
                }
                if usage.finality() == UsageFinality::Final {
                    self.final_usage = Some(*usage);
                }
            }
            ProviderEvent::TurnFinished(_) | ProviderEvent::Failed(_) => {}
        }
        Ok(())
    }

    /// Evaluates the batch against its terminal event.
    fn finish(&self, terminal: &ProviderEvent) -> Result<(), AgentError> {
        if self.usage_after_final {
            return Err(protocol_error("usage update follows a final usage record"));
        }
        match terminal {
            ProviderEvent::Failed(_) => {
                // A failed invocation never dispatches. Partial text,
                // progress, usage, and candidates stay caller-discarded
                // presentation data, so no success-only agreement applies.
                Ok(())
            }
            ProviderEvent::TurnFinished(finished) => {
                finished.validate()?;
                if let Some(final_usage) = self.final_usage
                    && counters_contradict(final_usage, finished.usage())
                {
                    return Err(protocol_error(
                        "terminal usage contradicts already-final counters",
                    ));
                }
                match finished.reason() {
                    FinishReason::Stop => self.finish_stop(),
                    FinishReason::ToolCalls => self.finish_tool_calls(),
                    FinishReason::Refusal
                    | FinishReason::Incomplete
                    | FinishReason::OutputLimit => Ok(()),
                }
            }
            ProviderEvent::TextDelta { .. }
            | ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCallReady(_)
            | ProviderEvent::Usage(_) => Err(protocol_error(
                "provider batch does not end with a terminal event",
            )),
        }
    }

    /// A successful stop is complete text only: no candidate may dispatch and
    /// no argument progress may remain unresolved.
    fn finish_stop(&self) -> Result<(), AgentError> {
        if !self.ready_keys.is_empty() {
            return Err(protocol_error(
                "stop finish reason must not carry tool call candidates",
            ));
        }
        if !self.call_keys.is_empty() {
            return Err(protocol_error(
                "stop finish reason leaves tool argument progress unresolved",
            ));
        }
        Ok(())
    }

    /// A tool-calls success dispatches its candidates, so identity and
    /// assembly must be unambiguous and complete.
    fn finish_tool_calls(&self) -> Result<(), AgentError> {
        if self.ready_keys.is_empty() {
            return Err(protocol_error(
                "tool-calls finish reason requires at least one candidate",
            ));
        }
        if self.duplicate_ready_key {
            return Err(protocol_error(
                "provider batch repeats a candidate item key",
            ));
        }
        if self.duplicate_ready_ref {
            return Err(protocol_error(
                "provider batch repeats a provider call reference",
            ));
        }
        if self.text_call_collision {
            return Err(protocol_error(
                "provider item key is used for both text and a tool call",
            ));
        }
        for key in &self.call_keys {
            if !self.ready_keys.contains(key) {
                return Err(protocol_error(
                    "tool argument progress never resolves to a candidate",
                ));
            }
        }
        if self.post_ready_progress {
            return Err(protocol_error(
                "tool argument progress follows a completed candidate",
            ));
        }
        if self.nonmonotonic_progress {
            return Err(protocol_error("tool argument progress is not monotonic"));
        }
        Ok(())
    }
}

/// Returns true only when both readings are known and unequal. [`None`] means
/// unknown and is never coerced to zero.
fn counters_contradict(previous: Usage, terminal: Usage) -> bool {
    known_counter_differs(previous.input_tokens(), terminal.input_tokens())
        || known_counter_differs(previous.output_tokens(), terminal.output_tokens())
}

fn known_counter_differs(previous: Option<u64>, terminal: Option<u64>) -> bool {
    matches!((previous, terminal), (Some(previous), Some(terminal)) if previous != terminal)
}

/// Bytes one event contributes to the aggregate batch payload. Usage counters
/// are numbers, not payload bytes; progress values likewise add no bytes.
fn event_payload_bytes(event: &ProviderEvent) -> usize {
    match event {
        ProviderEvent::TextDelta { item_key, text } => item_key.len() + text.len(),
        ProviderEvent::ToolCallDelta { item_key, .. } => item_key.len(),
        ProviderEvent::ToolCallReady(candidate) => {
            candidate.item_key().len()
                + candidate.provider_ref().len()
                + candidate.tool_name().len()
                + candidate.arguments_json().len()
        }
        ProviderEvent::Usage(_) => 0,
        ProviderEvent::TurnFinished(finished) => {
            finished.continuation().map_or(0, |continuation| {
                continuation.adapter().len()
                    + continuation.scope().len()
                    + continuation.bytes().len()
            })
        }
        ProviderEvent::Failed(error) => {
            error.message().len()
                + error
                    .correlation()
                    .iter()
                    .fold(0usize, |total, (key, value)| {
                        total.saturating_add(key.len()).saturating_add(value.len())
                    })
        }
    }
}

fn protocol_error(message: &'static str) -> AgentError {
    AgentError::new(ErrorCategory::Protocol, message, RetryGuidance::DoNotRetry)
        .expect("static safe protocol message builds")
}

fn limit_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::ResourceLimit,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe protocol message builds")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{CallCandidate, ContinuationData, MAX_ITEM_KEY_LEN, TurnFinished};

    fn limits() -> Limits {
        Limits::m0_test()
    }

    fn final_usage() -> Usage {
        Usage::new(None, None, UsageFinality::Final)
    }

    fn text(item: &str, body: &str) -> ProviderEvent {
        ProviderEvent::TextDelta {
            item_key: item.to_owned(),
            text: body.to_owned(),
        }
    }

    fn delta(item: &str, assembled_bytes: usize) -> ProviderEvent {
        ProviderEvent::ToolCallDelta {
            item_key: item.to_owned(),
            assembled_bytes,
        }
    }

    fn ready(item: &str, provider_ref: &str, args: &str) -> ProviderEvent {
        ProviderEvent::ToolCallReady(
            CallCandidate::new(item, provider_ref, "host_read", args).expect("candidate builds"),
        )
    }

    fn finished(reason: FinishReason, usage: Usage) -> ProviderEvent {
        ProviderEvent::TurnFinished(TurnFinished::new(reason, usage, None))
    }

    fn failed() -> ProviderEvent {
        ProviderEvent::Failed(
            AgentError::new(
                ErrorCategory::Protocol,
                "provider stream ended unexpectedly",
                RetryGuidance::DoNotRetry,
            )
            .expect("static safe message builds"),
        )
    }

    #[test]
    fn valid_stop_with_repeated_fragments_and_usage_passes() {
        let events = vec![
            text("item-0", "héllo "),
            text("item-0", "🌍"),
            ProviderEvent::Usage(Usage::new(Some(10), Some(5), UsageFinality::Provisional)),
            ProviderEvent::Usage(Usage::new(Some(10), Some(8), UsageFinality::Final)),
            finished(
                FinishReason::Stop,
                Usage::new(Some(10), Some(8), UsageFinality::Final),
            ),
        ];
        validate_batch(&events, &limits()).expect("valid stop batch passes");
    }

    #[test]
    fn valid_interleaved_tool_calls_pass() {
        let events = vec![
            text("item-0", "first "),
            delta("item-1", 4),
            text("item-0", "second"),
            delta("item-2", 4),
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            ready("item-2", "prov-ref-2", r#"{"path":"b"}"#),
            text("item-0", " end"),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        validate_batch(&events, &limits()).expect("valid tool-calls batch passes");
    }

    #[test]
    fn candidate_without_progress_passes() {
        let events = vec![
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        validate_batch(&events, &limits()).expect("progress is optional");
    }

    #[test]
    fn failed_terminal_allows_partial_text_and_discarded_candidates() {
        let events = vec![
            text("item-0", "partial"),
            delta("item-1", 4),
            ready("item-1", "prov-dup", r#"{"path":"a"}"#),
            ready("item-2", "prov-dup", r#"{"path":"a"}"#),
            failed(),
        ];
        validate_batch(&events, &limits())
            .expect("a failed batch never dispatches; the caller discards candidates");
        validate_batch(&[failed()], &limits()).expect("a bare failure is a valid batch");
    }

    #[test]
    fn empty_and_unterminated_batches_are_rejected() {
        let empty = validate_batch(&[], &limits()).expect_err("empty batch rejects");
        assert_eq!(empty.category(), ErrorCategory::Protocol);

        let unterminated = vec![text("item-0", "x")];
        let error = validate_batch(&unterminated, &limits()).expect_err("missing terminal");
        assert_eq!(error.category(), ErrorCategory::Protocol);
    }

    #[test]
    fn terminal_must_be_unique_and_last() {
        let trailing = vec![
            finished(FinishReason::Stop, final_usage()),
            text("item-0", "after"),
        ];
        assert_eq!(
            validate_batch(&trailing, &limits())
                .expect_err("terminal not last")
                .category(),
            ErrorCategory::Protocol
        );

        let duplicated = vec![
            finished(FinishReason::Stop, final_usage()),
            finished(FinishReason::Stop, final_usage()),
        ];
        assert_eq!(
            validate_batch(&duplicated, &limits())
                .expect_err("duplicate terminal")
                .category(),
            ErrorCategory::Protocol
        );
    }

    #[test]
    fn stop_rejects_candidates_and_unresolved_progress() {
        let with_candidate = vec![
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            finished(FinishReason::Stop, final_usage()),
        ];
        assert_eq!(
            validate_batch(&with_candidate, &limits())
                .expect_err("stop never dispatches")
                .category(),
            ErrorCategory::Protocol
        );

        let unresolved = vec![
            delta("item-1", 8),
            finished(FinishReason::Stop, final_usage()),
        ];
        assert_eq!(
            validate_batch(&unresolved, &limits())
                .expect_err("unfinished item is not a clean stop")
                .category(),
            ErrorCategory::Protocol
        );
    }

    #[test]
    fn tool_calls_requires_at_least_one_candidate() {
        let events = vec![
            text("item-0", "thinking"),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&events, &limits())
                .expect_err("tool calls need a candidate")
                .category(),
            ErrorCategory::Protocol
        );
    }

    #[test]
    fn non_dispatching_finish_reasons_tolerate_candidates_and_progress() {
        for reason in [
            FinishReason::Refusal,
            FinishReason::Incomplete,
            FinishReason::OutputLimit,
        ] {
            let with_candidate = vec![
                delta("item-1", 4),
                ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
                finished(reason, final_usage()),
            ];
            validate_batch(&with_candidate, &limits())
                .unwrap_or_else(|_| panic!("{reason:?} never dispatches, caller discards"));

            let truncated = vec![
                text("item-0", "partial"),
                delta("item-1", 8),
                finished(reason, final_usage()),
            ];
            validate_batch(&truncated, &limits())
                .unwrap_or_else(|_| panic!("{reason:?} may leave assembly unfinished"));
        }
    }

    #[test]
    fn duplicate_candidate_identities_are_rejected() {
        let duplicate_key = vec![
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            ready("item-1", "prov-ref-2", r#"{"path":"b"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&duplicate_key, &limits())
                .expect_err("item keys are unique")
                .category(),
            ErrorCategory::Protocol
        );

        let duplicate_ref = vec![
            ready("item-1", "prov-dup", r#"{"path":"a"}"#),
            ready("item-2", "prov-dup", r#"{"path":"a"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&duplicate_ref, &limits())
                .expect_err("provider references are unique")
                .category(),
            ErrorCategory::Protocol
        );
    }

    #[test]
    fn text_and_call_item_keys_never_collide() {
        let events = vec![
            text("item-0", "answer"),
            ready("item-0", "prov-ref-0", r#"{"path":"a"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&events, &limits())
                .expect_err("one key cannot be text and a call")
                .category(),
            ErrorCategory::Protocol
        );
    }

    #[test]
    fn invalid_item_keys_are_rejected() {
        let empty = vec![text("", "x"), finished(FinishReason::Stop, final_usage())];
        assert_eq!(
            validate_batch(&empty, &limits())
                .expect_err("empty key")
                .category(),
            ErrorCategory::InvalidInput
        );

        let oversize = "k".repeat(MAX_ITEM_KEY_LEN + 1);
        let long_delta = vec![
            delta(&oversize, 0),
            finished(FinishReason::Stop, final_usage()),
        ];
        assert_eq!(
            validate_batch(&long_delta, &limits())
                .expect_err("oversize progress key")
                .category(),
            ErrorCategory::InvalidInput
        );

        let long_text = vec![
            text(&oversize, "x"),
            finished(FinishReason::Stop, final_usage()),
        ];
        assert_eq!(
            validate_batch(&long_text, &limits())
                .expect_err("oversize text key")
                .category(),
            ErrorCategory::InvalidInput
        );

        // Candidate identities are validated by `CallCandidate::new`, so an
        // invalid candidate cannot even reach the batch validator.
        assert!(CallCandidate::new(&oversize, "prov-ref-1", "host_read", "{}").is_err());
    }

    #[test]
    fn unresolved_progress_is_rejected_for_dispatchable_success() {
        let events = vec![
            delta("item-1", 4),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&events, &limits())
                .expect_err("progress without a candidate")
                .category(),
            ErrorCategory::Protocol
        );
    }

    #[test]
    fn progress_after_ready_and_nonmonotonic_progress_are_rejected() {
        let after_ready = vec![
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            delta("item-1", 4),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&after_ready, &limits())
                .expect_err("progress after completion")
                .category(),
            ErrorCategory::Protocol
        );

        let backwards = vec![
            delta("item-1", 8),
            delta("item-1", 4),
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&backwards, &limits())
                .expect_err("progress moved backwards")
                .category(),
            ErrorCategory::Protocol
        );

        let repeated = vec![
            delta("item-1", 4),
            delta("item-1", 4),
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        validate_batch(&repeated, &limits()).expect("nondecreasing progress may repeat");
    }

    #[test]
    fn assembly_budget_is_effective_not_only_the_m0_constant() {
        let mut tight = limits();
        tight.max_arg_assembly_bytes = 4;
        let over_progress = vec![
            delta("item-1", 5),
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&over_progress, &tight)
                .expect_err("progress exceeds the effective assembly budget")
                .category(),
            ErrorCategory::ResourceLimit
        );

        let over_candidate = vec![
            ready("item-1", "prov-ref-1", r#"{"path":"a"}"#),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        assert_eq!(
            validate_batch(&over_candidate, &tight)
                .expect_err("candidate exceeds the effective assembly budget")
                .category(),
            ErrorCategory::ResourceLimit
        );

        let args = r#"{"path":"a"}"#;
        let mut exact = limits();
        exact.max_arg_assembly_bytes = args.len();
        let boundary = vec![
            delta("item-1", args.len()),
            ready("item-1", "prov-ref-1", args),
            finished(FinishReason::ToolCalls, final_usage()),
        ];
        validate_batch(&boundary, &exact).expect("the effective budget boundary passes");
    }

    #[test]
    fn aggregate_text_budget_is_effective_and_boundary_exact() {
        let mut tight = limits();
        tight.max_tool_output_bytes = 8;
        let exact = vec![
            text("item-0", "1234"),
            text("item-0", "5678"),
            finished(FinishReason::Stop, final_usage()),
        ];
        validate_batch(&exact, &tight).expect("exact output budget passes");

        let over = vec![
            text("item-0", "1234"),
            text("item-0", "56789"),
            finished(FinishReason::Stop, final_usage()),
        ];
        assert_eq!(
            validate_batch(&over, &tight)
                .expect_err("aggregate text exceeds the output budget")
                .category(),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn event_count_budget_is_boundary_exact() {
        let mut events: Vec<ProviderEvent> = (0..MAX_BATCH_EVENTS - 1)
            .map(|index| text(&format!("item-{index}"), "x"))
            .collect();
        events.push(finished(FinishReason::Stop, final_usage()));
        assert_eq!(events.len(), MAX_BATCH_EVENTS);
        validate_batch(&events, &limits()).expect("event-count boundary passes");

        events.insert(MAX_BATCH_EVENTS - 1, text("item-extra", "x"));
        assert_eq!(events.len(), MAX_BATCH_EVENTS + 1);
        assert_eq!(
            validate_batch(&events, &limits())
                .expect_err("over the event-count budget")
                .category(),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn batch_payload_budget_is_finite() {
        let mut roomy = limits();
        roomy.max_tool_output_bytes = 2 * MAX_BATCH_BYTES;
        let half = "x".repeat(MAX_BATCH_BYTES / 2 + 1);
        let events = vec![
            text("item-0", &half),
            text("item-1", &half),
            finished(FinishReason::Stop, final_usage()),
        ];
        assert!(events.iter().map(event_payload_bytes).sum::<usize>() > MAX_BATCH_BYTES);
        assert_eq!(
            validate_batch(&events, &roomy)
                .expect_err("batch payload budget is finite")
                .category(),
            ErrorCategory::ResourceLimit
        );
    }

    #[test]
    fn terminal_usage_must_be_final() {
        let provisional = vec![finished(
            FinishReason::Stop,
            Usage::new(Some(10), Some(5), UsageFinality::Provisional),
        )];
        assert_eq!(
            validate_batch(&provisional, &limits())
                .expect_err("terminal usage is final only")
                .category(),
            ErrorCategory::InvalidInput
        );
    }

    #[test]
    fn usage_updates_never_follow_a_final_record() {
        let repeated_final = vec![
            ProviderEvent::Usage(Usage::new(Some(10), Some(8), UsageFinality::Final)),
            ProviderEvent::Usage(Usage::new(Some(10), Some(8), UsageFinality::Final)),
            finished(
                FinishReason::Stop,
                Usage::new(Some(10), Some(8), UsageFinality::Final),
            ),
        ];
        assert_eq!(
            validate_batch(&repeated_final, &limits())
                .expect_err("finality ends updates")
                .category(),
            ErrorCategory::Protocol
        );

        let late_provisional = vec![
            ProviderEvent::Usage(Usage::new(Some(10), Some(8), UsageFinality::Final)),
            ProviderEvent::Usage(Usage::new(Some(10), Some(9), UsageFinality::Provisional)),
            finished(
                FinishReason::Stop,
                Usage::new(Some(10), Some(9), UsageFinality::Final),
            ),
        ];
        assert_eq!(
            validate_batch(&late_provisional, &limits())
                .expect_err("provisional after final")
                .category(),
            ErrorCategory::Protocol
        );
    }

    #[test]
    fn terminal_usage_never_contradicts_final_counters() {
        let contradictory = vec![
            ProviderEvent::Usage(Usage::new(Some(10), Some(8), UsageFinality::Final)),
            finished(
                FinishReason::Stop,
                Usage::new(Some(10), Some(9), UsageFinality::Final),
            ),
        ];
        assert_eq!(
            validate_batch(&contradictory, &limits())
                .expect_err("terminal counters disagree with final counters")
                .category(),
            ErrorCategory::Protocol
        );

        let matching = vec![
            ProviderEvent::Usage(Usage::new(Some(10), Some(5), UsageFinality::Provisional)),
            ProviderEvent::Usage(Usage::new(Some(10), Some(8), UsageFinality::Final)),
            finished(
                FinishReason::Stop,
                Usage::new(Some(10), Some(8), UsageFinality::Final),
            ),
        ];
        validate_batch(&matching, &limits()).expect("consistent final usage passes");
    }

    #[test]
    fn unknown_usage_counters_are_not_zero() {
        let final_unknown_then_zero = vec![
            ProviderEvent::Usage(Usage::new(None, None, UsageFinality::Final)),
            finished(
                FinishReason::Stop,
                Usage::new(Some(0), Some(0), UsageFinality::Final),
            ),
        ];
        validate_batch(&final_unknown_then_zero, &limits())
            .expect("unknown is not zero, so a known zero cannot contradict it");

        let known_then_unknown = vec![
            ProviderEvent::Usage(Usage::new(Some(10), Some(8), UsageFinality::Final)),
            finished(
                FinishReason::Stop,
                Usage::new(Some(10), None, UsageFinality::Final),
            ),
        ];
        validate_batch(&known_then_unknown, &limits())
            .expect("an unknown terminal counter cannot contradict a known final counter");
    }

    #[test]
    fn terminal_continuation_counts_against_the_payload_budget() {
        let within = vec![ProviderEvent::TurnFinished(TurnFinished::new(
            FinishReason::Stop,
            final_usage(),
            Some(ContinuationData::new("adapter", "scope", vec![0u8; 32]).expect("bounded")),
        ))];
        validate_batch(&within, &limits()).expect("bounded continuation passes");
    }
}
