//! Model-facing controls for the single parent run that invoked them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use nexus_config::{
    ExplorationThinking, ModelEntry, ProviderProfile, ProviderSettings, ToolsSettings, UserConfig,
    resolve_credential_at,
};
use nexus_core::{
    AgentError, EffectState, Evidence, ExecutionStatus, M0_REVISION, RequestId, RunOutcome,
    ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolRegistration, ToolSpec,
};
use nexus_runtime::runtime::WeakRuntime;
use nexus_runtime::{ExplorationWait, Runtime};
use serde_json::{Value, json};

pub(super) struct Controls {
    handle: tokio::runtime::Handle,
    runtime: OnceLock<WeakRuntime>,
    config: Arc<Mutex<UserConfig>>,
    binding: Option<(ProviderProfile, ModelEntry)>,
    allowed_models: Vec<String>,
    provider_settings: ProviderSettings,
    credentials: Option<PathBuf>,
    reads: Vec<ToolRegistration>,
}

impl Controls {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        handle: tokio::runtime::Handle,
        config: Arc<Mutex<UserConfig>>,
        binding: Option<(ProviderProfile, ModelEntry)>,
        allowed_models: Vec<String>,
        provider_settings: ProviderSettings,
        credentials: Option<PathBuf>,
        root: &Path,
        settings: &ToolsSettings,
    ) -> Result<Arc<Self>, AgentError> {
        Ok(Arc::new(Self {
            handle,
            runtime: OnceLock::new(),
            config,
            binding,
            allowed_models,
            provider_settings,
            credentials,
            reads: nexus_tools::project_read_tools(root, settings)?,
        }))
    }

    pub(super) fn bind(&self, runtime: &Runtime) {
        assert!(self.runtime.set(runtime.downgrade()).is_ok());
    }

    pub(super) fn registrations(self: &Arc<Self>) -> [ToolRegistration; 2] {
        [true, false].map(|start| {
            let (name, description, schema) = if start {
                (
                    "delegate_start",
                    "Start a background read-only task in this project. Role must be explore. Thinking may be inherit, off, or low. Starts immediately; use delegate_await to retrieve the result.",
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
                    "Wait for and collect one Explore result started in this run.",
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
            .expect("static exploration definition");
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
        let result = self.execute_control(call, context);
        match result {
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
            return Err("exploration call is not active or authorized");
        }
        let runtime = controls
            .runtime
            .get()
            .and_then(WeakRuntime::upgrade)
            .ok_or("exploration runtime is unavailable")?;
        let args: Value = serde_json::from_str(call.args().as_str())
            .map_err(|_| "invalid exploration arguments")?;
        if self.start {
            if args["role"].as_str() != Some("explore") {
                return Err("unsupported exploration role");
            }
            let task = args["task"].as_str().ok_or("missing exploration task")?;
            let thinking = match args.get("thinking") {
                None => ExplorationThinking::Inherit,
                Some(value) => match value.as_str() {
                    Some("inherit") => ExplorationThinking::Inherit,
                    Some("off") => ExplorationThinking::Off,
                    Some("low") => ExplorationThinking::Low,
                    _ => return Err("invalid exploration thinking"),
                },
            };
            let model = args
                .get("model")
                .map(|value| value.as_str().ok_or("invalid exploration model"))
                .transpose()?;
            let selected = controls
                .config
                .lock()
                .expect("config lockable")
                .exploration_model(
                    controls.binding.as_ref().map(|(p, m)| (p, m)),
                    model,
                    &controls.allowed_models,
                    thinking,
                )
                .map_err(|_| "exploration model or thinking is not allowed")?;
            let adapter = nexus_openai::OpenAiProvider::for_exploration(
                &selected,
                &controls.provider_settings,
            )
            .map_err(|_| "exploration model or thinking is unsupported")?;
            let credential = resolve_credential_at(
                &selected.provider.credential,
                controls.credentials.as_deref(),
            )
            .map_err(|_| "exploration provider credential is unavailable")?;
            let job_id =
                RequestId::new(call.call().as_str()).map_err(|_| "invalid job identity")?;
            let accepted = bounded_json(
                json!({"job_id": job_id.as_str(), "status":"accepted"}),
                context.output_budget_bytes(),
            )?;
            let tools = controls
                .reads
                .iter()
                .map(|tool| ToolRegistration::read(tool.port().clone()))
                .collect();
            controls.handle.block_on(async {
                context
                    .check_active()
                    .map_err(|_| "exploration call stopped")?;
                runtime
                    .start_exploration(
                        call.run(),
                        job_id.clone(),
                        task,
                        Arc::new(adapter.with_resolved_credential(credential)),
                        tools,
                    )
                    .await
                    .map_err(|_| "exploration could not start")
            })?;
            return Ok(accepted);
        }
        let id = args["job_id"].as_str().ok_or("missing job identity")?;
        let job = RequestId::new(id).map_err(|_| "invalid job identity")?;
        bounded_json(
            json!({"job_id":id,"status":"limit-reached","text":"","truncated":false}),
            context.output_budget_bytes(),
        )?;
        let deadline = context.deadline().ok_or("missing exploration deadline")?;
        let result = controls.handle.block_on(async {
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(deadline.into()) => Err("exploration wait timed out"),
                result = runtime.await_exploration(call.run(), &job) => Ok(result),
            }
        })?;
        let value = match result {
            ExplorationWait::Finished(result) => {
                let status = if result.timed_out {
                    "timed-out"
                } else {
                    match result.outcome {
                        RunOutcome::Completed => "completed",
                        RunOutcome::Cancelled => "cancelled",
                        RunOutcome::Refused => "refused",
                        RunOutcome::Failed => "failed",
                        RunOutcome::LimitReached => "limit-reached",
                    }
                };
                json!({
                    "job_id": id, "status": status, "text": result.text, "truncated": result.truncated,
                })
            }
            ExplorationWait::Collected => json!({"job_id":id,"status":"collected"}),
            ExplorationWait::Unknown => return Err("exploration job does not belong to this run"),
            ExplorationWait::ParentStopped => return Err("parent run stopped"),
        };
        bounded_json(value, context.output_budget_bytes())
    }
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
            .ok_or("exploration output budget is too small")?;
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
            "exploration output exceeded its budget",
            false,
        )
        .expect("static exploration error")
    })
}
