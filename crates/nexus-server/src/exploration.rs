//! Model-facing controls for the single parent run that invoked them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use nexus_config::{
    ExplorationThinking, ModelEntry, ProviderProfile, ProviderSettings, ToolsSettings, UserConfig,
    resolve_credential_at,
};
use nexus_core::{
    AgentError, EffectState, Evidence, ExecutionStatus, M0_REVISION, NormalizedArgs, RequestId,
    RunId, ToolCall, ToolContext, ToolId, ToolOutcome, ToolPort, ToolRegistration, ToolSpec,
};
use nexus_runtime::runtime::WeakRuntime;
use nexus_runtime::{
    ExplorationDelivery, ExplorationHandoff, ExplorationProposal, ExplorationRole, ExplorationWait,
    Runtime,
};
use serde_json::{Value, json};

type HandoffHandler = dyn Fn(RunId, RequestId, ExplorationHandoff) + Send + Sync;

pub(super) struct Controls {
    handle: tokio::runtime::Handle,
    runtime: OnceLock<WeakRuntime>,
    config: Arc<Mutex<UserConfig>>,
    binding: Option<(ProviderProfile, ModelEntry)>,
    allowed_models: Vec<String>,
    provider_settings: ProviderSettings,
    credentials: Option<PathBuf>,
    reads: Vec<ToolRegistration>,
    supports_child_completion: bool,
    handoff: OnceLock<Box<HandoffHandler>>,
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
        supports_child_completion: bool,
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
            supports_child_completion,
            handoff: OnceLock::new(),
        }))
    }

    pub(super) fn bind(&self, runtime: &Runtime) {
        assert!(self.runtime.set(runtime.downgrade()).is_ok());
    }

    pub(super) fn bind_handoff(
        &self,
        handler: impl Fn(RunId, RequestId, ExplorationHandoff) + Send + Sync + 'static,
    ) {
        self.handoff.get_or_init(|| Box::new(handler));
    }

    pub(super) fn registrations(self: &Arc<Self>) -> [ToolRegistration; 2] {
        [true, false].map(|start| {
            let (name, mut description, mut schema) = if start {
                (
                    "delegate_start",
                    "Start a read-only background task. Explore can answer directly or investigate the project. Propose suggests one exact text replacement and requires delegate_await. Omit thinking to inherit configured settings; use off or low only on supported Ollama Direct / Chat Completions endpoints.".to_owned(),
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
                    "Wait for and collect one background task result started in this run.".to_owned(),
                    json!({
                        "type": "object",
                        "properties": {"job_id":{"type":"string"}},
                        "required": ["job_id"],
                        "additionalProperties": false
                    }),
                )
            };
            if start && self.supports_child_completion {
                schema["properties"]["delivery"] = json!({"type":"string"});
                description.push_str(" Explore delivery defaults to steer, or choose queue to defer. Set delivery to await when collecting with delegate_await. Propose only supports await.");
            }
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
            let role = match args["role"].as_str() {
                Some("explore") => ExplorationRole::Explore,
                Some("propose") => ExplorationRole::Propose,
                _ => return Err("unsupported exploration role"),
            };
            let task = args["task"].as_str().ok_or("missing exploration task")?;
            let delivery = match args.get("delivery") {
                None if controls.supports_child_completion && role == ExplorationRole::Explore => {
                    ExplorationDelivery::Steer
                }
                None => ExplorationDelivery::Await,
                Some(value) if value.as_str() == Some("await") => ExplorationDelivery::Await,
                Some(value)
                    if controls.supports_child_completion && value.as_str() == Some("steer") =>
                {
                    ExplorationDelivery::Steer
                }
                Some(value)
                    if controls.supports_child_completion && value.as_str() == Some("queue") =>
                {
                    ExplorationDelivery::Queue
                }
                Some(_) => return Err("unsupported exploration delivery"),
            };
            if role == ExplorationRole::Propose && delivery != ExplorationDelivery::Await {
                return Err("proposals require explicit collection");
            }
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
                .map_err(|error| error.message())?;
            let adapter = nexus_openai::OpenAiProvider::for_exploration(
                &selected,
                &controls.provider_settings,
            )
            .map_err(|error| {
                if error.category() == nexus_core::ErrorCategory::UnsupportedCapability
                    && !matches!(thinking, ExplorationThinking::Inherit)
                {
                    "Selected endpoint does not support explicit thinking. Omit thinking to inherit settings if acceptable."
                } else {
                    "exploration model or inherited thinking is unsupported"
                }
            })?;
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
            let handoff = controls.handle.block_on(async {
                context
                    .check_active()
                    .map_err(|_| "exploration call stopped")?;
                runtime
                    .start_exploration_with_delivery(
                        call.run(),
                        job_id.clone(),
                        task,
                        Arc::new(adapter.with_resolved_credential(credential)),
                        tools,
                        delivery,
                        role,
                    )
                    .await
                    .map_err(|_| "exploration could not start")
            })?;
            if let Some(handoff) = handoff
                && let Some(handler) = controls.handoff.get()
            {
                handler(call.run().clone(), job_id, handoff);
            }
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
                    return bounded_json(
                        json!({
                            "job_id": id, "status": status, "proposal": {
                                "path":proposal.path, "old_text":proposal.old_text, "new_text":proposal.new_text,
                            },
                            "input_tokens":result.usage.and_then(|usage| usage.input_tokens()),
                            "output_tokens":result.usage.and_then(|usage| usage.output_tokens()),
                        }),
                        context.output_budget_bytes(),
                    );
                }
                json!({
                    "job_id": id, "status": status, "text": result.text, "truncated": result.truncated,
                    "input_tokens":result.usage.and_then(|usage| usage.input_tokens()),
                    "output_tokens":result.usage.and_then(|usage| usage.output_tokens()),
                })
            }
            ExplorationWait::Collected => json!({"job_id":id,"status":"collected"}),
            ExplorationWait::Unknown => return Err("exploration job does not belong to this run"),
            ExplorationWait::ParentStopped => return Err("parent run stopped"),
        };
        bounded_json(value, context.output_budget_bytes())
    }
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
