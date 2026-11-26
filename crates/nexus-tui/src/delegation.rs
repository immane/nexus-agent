//! TUI host composition for read-only background delegation.
//!
//! The TUI has no session inbox, so it exposes explicit collection only:
//! `delegate_start` starts a job and `delegate_await` consumes its result.
//! No automatic Steer or Queue delivery is offered.

use std::collections::HashMap;
use std::future::Future;
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use nexus_config::ExplorationThinking;
use nexus_core::{
    AgentError, EffectState, Evidence, ExecutionStatus, M0_REVISION, NormalizedArgs, ProviderPort,
    RequestId, RunId, ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolRegistration,
    ToolSpec,
};
use nexus_runtime::runtime::WeakRuntime;
use nexus_runtime::{
    ExplorationDelivery, ExplorationProposal, ExplorationRole, ExplorationWait, Runtime,
};
use serde_json::{Value, json};

use crate::{LiveSelection, SharedUserConfig};

/// Parent selection captured when a run first reaches its provider.
pub(crate) type RunBindings = Arc<Mutex<HashMap<RunId, LiveSelection>>>;

/// Resolves one child provider from the parent's captured selection.
pub(crate) type ChildResolver = dyn Fn(
        &LiveSelection,
        Option<&str>,
        &str,
    ) -> Result<Arc<dyn ProviderPort + Send + Sync>, &'static str>
    + Send
    + Sync;

pub(crate) struct Controls {
    handle: tokio::runtime::Handle,
    runtime: OnceLock<WeakRuntime>,
    bindings: RunBindings,
    reads: Vec<ToolRegistration>,
    resolve: Arc<ChildResolver>,
}

impl Controls {
    pub(crate) fn new(
        handle: tokio::runtime::Handle,
        bindings: RunBindings,
        reads: Vec<ToolRegistration>,
        resolve: Arc<ChildResolver>,
    ) -> Arc<Self> {
        Arc::new(Self {
            handle,
            runtime: OnceLock::new(),
            bindings,
            reads,
            resolve,
        })
    }

    pub(crate) fn bind(&self, runtime: &Runtime) {
        assert!(self.runtime.set(runtime.downgrade()).is_ok());
    }

    pub(crate) fn registrations(self: &Arc<Self>) -> [ToolRegistration; 2] {
        [true, false].map(|start| {
            let (name, description, schema) = if start {
                (
                    "delegate_start",
                    "Start a read-only background task. Explore can answer directly or investigate the project. Propose suggests one exact text replacement. Collect the result with delegate_await. Omit thinking to inherit configured settings; use off or low only on supported Ollama Direct / Chat Completions endpoints.",
                    json!({
                        "type": "object",
                        "properties": {
                            "role": {"type":"string"},
                            "task": {"type":"string"},
                            "model": {"type":"string"},
                            "thinking": {"type":"string"}
                        },
                        "required": ["role","task"],
                        "additionalProperties": false
                    }),
                )
            } else {
                (
                    "delegate_await",
                    "Wait for and collect one background task result started in this run.",
                    json!({
                        "type": "object",
                        "properties": {"job_id":{"type":"string"}},
                        "required": ["job_id"],
                        "additionalProperties": false
                    }),
                )
            };
            let spec = ToolSpec::new(
                ToolId::new(name, M0_REVISION).expect("static tool id"),
                description,
                schema.to_string(),
            )
            .expect("static delegation definition");
            ToolRegistration::read(Arc::new(Control {
                controls: self.clone(),
                start,
                spec,
            }))
        })
    }
}

struct Control {
    controls: Arc<Controls>,
    start: bool,
    spec: ToolSpec,
}

impl ToolPort for Control {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        match self.execute_control(call, context) {
            Ok((body, truncated)) => outcome(ExecutionStatus::Succeeded, &body, truncated, context),
            Err(message) => outcome(ExecutionStatus::Failed, message, false, context),
        }
    }
}

impl Control {
    fn execute_control(
        &self,
        call: &ToolCall,
        context: &ToolContext,
    ) -> Result<(String, bool), &'static str> {
        let controls = &self.controls;
        if context.scope().as_str() != "delegate:explore"
            || context.run() != Some(call.run())
            || context.check_active().is_err()
        {
            return Err("delegation call is not active or authorized");
        }
        let runtime = controls
            .runtime
            .get()
            .and_then(WeakRuntime::upgrade)
            .ok_or("delegation runtime is unavailable")?;
        let args: Value = serde_json::from_str(call.args().as_str())
            .map_err(|_| "invalid delegation arguments")?;
        let deadline = context
            .deadline()
            .ok_or("delegation deadline is unavailable")?;
        if self.start {
            let role = match args["role"].as_str() {
                Some("explore") => ExplorationRole::Explore,
                Some("propose") => ExplorationRole::Propose,
                _ => return Err("unsupported delegation role"),
            };
            let task = args["task"].as_str().ok_or("missing delegation task")?;
            let model = args
                .get("model")
                .map(|value| value.as_str().ok_or("invalid delegation model"))
                .transpose()?;
            let thinking = args
                .get("thinking")
                .and_then(Value::as_str)
                .unwrap_or("inherit");
            let selection = controls
                .bindings
                .lock()
                .expect("run bindings lockable")
                .get(call.run())
                .cloned()
                .ok_or("delegation parent model is unavailable")?;
            let provider = (controls.resolve)(&selection, model, thinking)?;
            let job_id =
                RequestId::new(call.call().as_str()).map_err(|_| "invalid job identity")?;
            let tools = controls
                .reads
                .iter()
                .map(|tool| ToolRegistration::read(tool.port().clone()))
                .collect();
            let accepted = bounded_json(
                json!({"job_id": job_id.as_str(), "status":"accepted"}),
                context.output_budget_bytes(),
            )?;
            let runtime = runtime.clone();
            let parent = call.run().clone();
            let task = task.to_owned();
            let job = job_id.clone();
            let start = async move {
                runtime
                    .start_exploration_with_delivery(
                        &parent,
                        job,
                        &task,
                        provider,
                        tools,
                        ExplorationDelivery::Await,
                        role,
                    )
                    .await
                    .map(|_| ())
            };
            wait_on(&controls.handle, start, deadline)
                .map_err(|_| "delegation could not start")?
                .map_err(|_| "delegation could not start")?;
            return Ok(accepted);
        }
        let id = args["job_id"].as_str().ok_or("missing job identity")?;
        let job = RequestId::new(id).map_err(|_| "invalid job identity")?;
        let runtime = runtime.clone();
        let parent = call.run().clone();
        let awaited = job.clone();
        let result = wait_on(
            &controls.handle,
            async move { runtime.await_exploration(&parent, &awaited).await },
            deadline,
        )?;
        let value = match result {
            ExplorationWait::Finished(result) => {
                let status = result.status().as_str();
                if result.role == ExplorationRole::Propose && status == "completed" {
                    let proposal = result
                        .proposal()
                        .ok_or("child returned an invalid proposal")?;
                    if !proposal_matches_project(&controls.reads, &proposal, call, context) {
                        return Err(
                            "proposal target is stale, ambiguous, unavailable, or too large to verify",
                        );
                    }
                    json!({
                        "job_id": id, "status": status, "proposal": {
                            "path":proposal.path, "old_text":proposal.old_text, "new_text":proposal.new_text,
                        },
                        "input_tokens":result.usage.and_then(|usage| usage.input_tokens()),
                        "output_tokens":result.usage.and_then(|usage| usage.output_tokens()),
                    })
                } else {
                    json!({
                        "job_id": id, "status": status, "text": result.text, "truncated": result.truncated,
                        "input_tokens":result.usage.and_then(|usage| usage.input_tokens()),
                        "output_tokens":result.usage.and_then(|usage| usage.output_tokens()),
                    })
                }
            }
            ExplorationWait::Collected => json!({"job_id":id,"status":"collected"}),
            ExplorationWait::Unknown => return Err("delegation job does not belong to this run"),
            ExplorationWait::ParentStopped => return Err("parent run stopped"),
        };
        bounded_json(value, context.output_budget_bytes())
    }
}

/// Waits for a runtime future without nesting `block_on` on the TUI's
/// current-thread executor. The tool runs on a blocking worker; the spawned
/// future is driven by the TUI loop.
fn wait_on<T: Send + 'static>(
    handle: &tokio::runtime::Handle,
    future: impl Future<Output = T> + Send + 'static,
    deadline: Instant,
) -> Result<T, &'static str> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("delegation deadline elapsed");
    }
    let (tx, rx) = sync_channel(1);
    handle.spawn(async move {
        let _ = tx.send(future.await);
    });
    rx.recv_timeout(remaining).map_err(|error| match error {
        RecvTimeoutError::Timeout => "delegation wait timed out",
        RecvTimeoutError::Disconnected => "delegation worker stopped",
    })
}

fn proposal_matches_project(
    tools: &[ToolRegistration],
    proposal: &ExplorationProposal,
    call: &ToolCall,
    context: &ToolContext,
) -> bool {
    if context.check_active().is_err() {
        return false;
    }
    let Some(reader) = tools
        .iter()
        .find(|tool| tool.port().describe().id().name() == "host_read")
    else {
        return false;
    };
    let Ok(args) = NormalizedArgs::new(json!({"path":proposal.path,"limit":16_384}).to_string())
    else {
        return false;
    };
    let read = ToolCall::new(
        call.run().clone(),
        call.turn().clone(),
        call.call().clone(),
        reader.port().describe().id().clone(),
        args,
    );
    let outcome = reader.port().execute(&read, context);
    outcome.status() == ExecutionStatus::Succeeded
        && !outcome.is_truncated()
        && outcome
            .content()
            .match_indices(&proposal.old_text)
            .take(2)
            .count()
            == 1
}

fn bounded_json(mut value: Value, budget: usize) -> Result<(String, bool), &'static str> {
    loop {
        let body = value.to_string();
        if body.len() <= budget {
            return Ok((body, value["truncated"] == true));
        }
        let text = value["text"]
            .as_str()
            .filter(|text| !text.is_empty())
            .ok_or("delegation output budget is too small")?;
        value["text"] = json!(&text[..text.floor_char_boundary(text.len() / 2)]);
        value["truncated"] = json!(true);
    }
}

fn outcome(
    status: ExecutionStatus,
    content: &str,
    truncated: bool,
    context: &ToolContext,
) -> ToolOutcome {
    ToolOutcome::new(
        status,
        EffectState::KnownNotApplied,
        Evidence::HostObserved,
        content,
        truncated,
    )
    .and_then(|outcome| outcome.enforce_budget(context.output_budget_bytes()))
    .unwrap_or_else(|_| {
        ToolOutcome::new(
            ExecutionStatus::Failed,
            EffectState::NotStarted,
            Evidence::HostObserved,
            "delegation output exceeded its budget",
            false,
        )
        .expect("static delegation error")
    })
}

/// Production child resolution: select an allowed configured model, validate
/// its thinking setting and adapter before any inference, then resolve the
/// stored or environment credential on the host.
pub(crate) fn child_resolver(
    user_config: SharedUserConfig,
    allowed_models: Vec<String>,
) -> Result<Arc<ChildResolver>, AgentError> {
    let credentials = {
        let document = user_config.lock().expect("user configuration lockable");
        document
            .path
            .as_ref()
            .map(|path| path.with_file_name("credentials.json"))
            .or_else(nexus_config::credentials_path)
    };
    Ok(Arc::new(
        move |selection: &LiveSelection, requested, thinking| {
            let thinking = match thinking {
                "inherit" => ExplorationThinking::Inherit,
                "off" => ExplorationThinking::Off,
                "low" => ExplorationThinking::Low,
                _ => return Err("invalid delegation thinking"),
            };
            let parent = selection
                .active_model
                .as_deref()
                .and_then(|id| selection.config.model(id))
                .and_then(|entry| {
                    selection
                        .config
                        .provider(&entry.provider)
                        .map(|profile| (profile, entry))
                });
            let selected = selection
                .config
                .exploration_model(parent, requested, &allowed_models, thinking)
                .map_err(|error| error.message())?;
            let adapter = nexus_openai::OpenAiProvider::for_exploration(
                &selected,
                &selection.provider_settings,
            )
            .map_err(|error| {
                if error.category() == nexus_core::ErrorCategory::UnsupportedCapability
                    && !matches!(thinking, ExplorationThinking::Inherit)
                {
                    "Selected endpoint does not support explicit thinking. Omit thinking to inherit settings if acceptable."
                } else {
                    "delegation model or inherited thinking is unsupported"
                }
            })?;
            let credential = nexus_config::resolve_credential_at(
                &selected.provider.credential,
                credentials.as_deref(),
            )
            .map_err(|_| "delegation provider credential is unavailable")?;
            Ok(Arc::new(adapter.with_resolved_credential(credential))
                as Arc<dyn ProviderPort + Send + Sync>)
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn registrations_require_explicit_collection_without_delivery() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("current-thread runtime");
        let resolve = child_resolver(
            crate::shared_user_config(nexus_config::UserConfig::default_config(), None),
            Vec::new(),
        )
        .expect("empty resolver builds");
        let controls = Controls::new(
            runtime.handle().clone(),
            RunBindings::default(),
            Vec::new(),
            resolve,
        );
        let specs = controls.registrations().map(|tool| tool.port().describe());
        assert_eq!(specs[0].id().name(), "delegate_start");
        assert_eq!(specs[1].id().name(), "delegate_await");
        let start: Value = serde_json::from_str(specs[0].input_schema_json()).unwrap();
        assert!(start["properties"].get("delivery").is_none());
        let awaited: Value = serde_json::from_str(specs[1].input_schema_json()).unwrap();
        assert_eq!(awaited["required"], json!(["job_id"]));
    }

    #[test]
    fn wait_on_is_driven_by_the_tui_current_thread_loop() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("current-thread runtime");
        let handle = runtime.handle().clone();
        let deadline = Instant::now() + Duration::from_secs(2);
        let worker = std::thread::spawn(move || {
            wait_on(
                &handle,
                async {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    7
                },
                deadline,
            )
        });
        runtime.block_on(async {
            tokio::time::sleep(Duration::from_millis(100)).await;
        });
        assert_eq!(worker.join().expect("worker joins"), Ok(7));
    }
}
