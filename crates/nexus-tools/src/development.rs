//! Development file adapters re-use the jailed implementations after checking
//! the runtime's exact canonical target scope. They never infer approval from
//! arguments or maintain an independent session permission cache.

use std::path::Path;
use std::sync::Arc;

use crate::{
    SandboxedExecutor, ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter,
};
use nexus_core::{
    AgentError, EffectState, Evidence, ExecutionStatus, NormalizedArgs, ToolAccess, ToolCall,
    ToolContext, ToolOutcome, ToolPort, ToolRegistration, ToolSpec,
};
use nexus_permissions::DirectoryPolicy;

/// Constructs the real-file strict tool set shared by the TUI and server.
/// Runtime policy and provider composition remain owned by each entrypoint.
pub fn strict_tools(root: &Path) -> Result<Vec<ToolRegistration>, AgentError> {
    Ok(vec![
        ToolRegistration::read(Arc::new(ScopedReader::with_root(root)?)),
        ToolRegistration::read(Arc::new(ScopedLister::with_root(root)?)),
        ToolRegistration::read(Arc::new(ScopedSearcher::with_root(root)?)),
        ToolRegistration::mutation(Arc::new(ScopedWriter::with_root(root)?)),
        ToolRegistration::mutation(Arc::new(ScopedPatcher::with_root(root)?)),
        ToolRegistration::process(Arc::new(SandboxedExecutor::with_root(root)?)),
    ])
}

/// Constructs development file and exec tools shared by the TUI and server.
/// Runtime permission policy is still configured by the caller.
pub fn development_tools(root: &Path) -> Result<Vec<ToolRegistration>, AgentError> {
    let mut tools = development_file_tools(root)?;
    tools.push(ToolRegistration::process(Arc::new(
        SandboxedExecutor::development(root)?,
    )));
    Ok(tools)
}

pub fn development_file_tools(root: &Path) -> Result<Vec<ToolRegistration>, AgentError> {
    let policy = DirectoryPolicy::new(root).map_err(|_| {
        AgentError::new(
            nexus_core::ErrorCategory::InvalidInput,
            "permissions root is invalid",
            nexus_core::RetryGuidance::DoNotRetry,
        )
        .expect("static permission error")
    })?;
    // Trusted host composition: each category is assigned from the concrete
    // adapter constructed here, never read back from `describe()`.
    let ports: Vec<(ToolAccess, Arc<dyn ToolPort + Send + Sync>)> = vec![
        (ToolAccess::Read, Arc::new(ScopedReader::with_root(root)?)),
        (ToolAccess::Read, Arc::new(ScopedLister::with_root(root)?)),
        (ToolAccess::Read, Arc::new(ScopedSearcher::with_root(root)?)),
        (
            ToolAccess::Mutation,
            Arc::new(ScopedWriter::with_root(root)?),
        ),
        (
            ToolAccess::Mutation,
            Arc::new(ScopedPatcher::with_root(root)?),
        ),
    ];
    ports
        .into_iter()
        .map(|(access, port)| {
            let original = port.describe();
            let description = format!("{}. Development mode: project/temp paths are automatic; external paths need runtime approval. Protected credential paths are unavailable.", original.description().replace("inside the tool root", "at a runtime-authorized path").replace(". Requires approval.", "").trim_end_matches('.'));
            let spec =
                ToolSpec::new(original.id().clone(), description, original.input_schema_json())?;
            Ok(ToolRegistration::classified(
                Arc::new(DevelopmentFile {
                    policy: policy.clone(),
                    spec,
                }) as Arc<dyn ToolPort + Send + Sync>,
                access,
            ))
        })
        .collect()
}

struct DevelopmentFile {
    policy: DirectoryPolicy,
    spec: ToolSpec,
}

impl ToolPort for DevelopmentFile {
    fn describe(&self) -> ToolSpec {
        self.spec.clone()
    }
    fn execute(&self, call: &ToolCall, context: &ToolContext) -> ToolOutcome {
        if context.check_active().is_err() {
            let (status, message) = if context.is_cancelled() {
                (
                    ExecutionStatus::Cancelled,
                    "filesystem call was cancelled before access",
                )
            } else {
                (
                    ExecutionStatus::TimedOut,
                    "filesystem deadline passed before access",
                )
            };
            return ToolOutcome::new(
                status,
                EffectState::NotStarted,
                Evidence::HostObserved,
                message,
                false,
            )
            .expect("static inactive outcome");
        }
        let result = (|| -> Result<ToolOutcome, ()> {
            let mut args: serde_json::Value =
                serde_json::from_str(call.args().as_str()).map_err(|_| ())?;
            let target = self
                .policy
                .resolve(
                    args.get("path")
                        .and_then(serde_json::Value::as_str)
                        .ok_or(())?,
                )
                .map_err(|_| ())?;
            if !nexus_permissions::file_scope_matches(context.scope().as_str(), &target) {
                return Err(());
            }
            let root = if target.is_dir() {
                target.as_path()
            } else {
                target.parent().ok_or(())?
            };
            let port: Box<dyn ToolPort> = match self.spec.id().name() {
                "host_read" => Box::new(ScopedReader::with_root(root).map_err(|_| ())?),
                "host_list" => Box::new(
                    ScopedLister::with_root(root)
                        .map_err(|_| ())?
                        .with_protected_paths(self.policy.clone()),
                ),
                "host_search" => Box::new(
                    ScopedSearcher::with_root(root)
                        .map_err(|_| ())?
                        .with_protected_paths(self.policy.clone()),
                ),
                "host_write" => Box::new(ScopedWriter::with_root(root).map_err(|_| ())?),
                "host_patch" => Box::new(ScopedPatcher::with_root(root).map_err(|_| ())?),
                _ => return Err(()),
            };
            args["path"] = serde_json::Value::String(target.to_string_lossy().into_owned());
            let args = NormalizedArgs::new(args.to_string()).map_err(|_| ())?;
            let call = ToolCall::new(
                call.run().clone(),
                call.turn().clone(),
                call.call().clone(),
                call.tool().clone(),
                args,
            );
            Ok(port.execute(&call, context))
        })();
        result.unwrap_or_else(|_| {
            ToolOutcome::new(
                ExecutionStatus::Denied,
                EffectState::NotStarted,
                Evidence::HostObserved,
                "filesystem target is not authorized",
                false,
            )
            .expect("static denial")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{ApprovedScope, CallId, M0_REVISION, RunId, ToolAccess, ToolId, TurnId};
    use std::time::Duration;

    #[test]
    fn development_files_require_exact_scope_and_filter_protected_content() {
        let root =
            std::env::temp_dir().join(format!("nexus-development-files-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        std::fs::write(root.join("plain.txt"), "ordinary data").unwrap();
        std::fs::write(root.join(".env"), "hidden data").unwrap();
        let policy = DirectoryPolicy::new(&root).unwrap();
        let tools = development_file_tools(&root).unwrap();
        let run = |tool: &str, args: serde_json::Value, scope: String| {
            let call = ToolCall::new(
                RunId::new("run-files").unwrap(),
                TurnId::new("turn-files").unwrap(),
                CallId::new("call-files").unwrap(),
                ToolId::new(tool, M0_REVISION).unwrap(),
                NormalizedArgs::new(args.to_string()).unwrap(),
            );
            let context = ToolContext::new(
                4096,
                Duration::ZERO,
                false,
                ApprovedScope::new(scope).unwrap(),
            )
            .unwrap();
            tools
                .iter()
                .find(|registration| registration.port().describe().id().name() == tool)
                .unwrap()
                .port()
                .execute(&call, &context)
        };
        let plain = policy.resolve("plain.txt").unwrap();
        let good = run(
            "host_read",
            serde_json::json!({"path":"plain.txt"}),
            format!("path:{}", plain.display()),
        );
        assert_eq!(
            good.status(),
            ExecutionStatus::Succeeded,
            "{}",
            good.content()
        );
        let wrong = run(
            "host_read",
            serde_json::json!({"path":"plain.txt"}),
            "path:/wrong".to_owned(),
        );
        assert_eq!(wrong.status(), ExecutionStatus::Denied);
        let hidden = run(
            "host_read",
            serde_json::json!({"path":".env"}),
            format!("path:{}", root.join(".env").display()),
        );
        assert_eq!(hidden.status(), ExecutionStatus::Denied);
        let scope = format!("path:{}", policy.root().display());
        let listed = run("host_list", serde_json::json!({"path":"."}), scope.clone());
        assert_eq!(listed.status(), ExecutionStatus::Succeeded);
        assert!(listed.content().contains("plain.txt"));
        assert!(!listed.content().contains(".env"));
        let searched = run(
            "host_search",
            serde_json::json!({"path":".", "query":"data"}),
            scope,
        );
        assert_eq!(
            searched.status(),
            ExecutionStatus::Succeeded,
            "{}",
            searched.content()
        );
        assert!(searched.content().contains("ordinary data"));
        assert!(!searched.content().contains("hidden data"));
        let new = policy.resolve("new.txt").unwrap();
        let written = run(
            "host_write",
            serde_json::json!({"path":"new.txt", "content":"written"}),
            format!("path:{}", new.display()),
        );
        assert_eq!(
            written.status(),
            ExecutionStatus::Succeeded,
            "{}",
            written.content()
        );
        assert_eq!(std::fs::read_to_string(new).unwrap(), "written");
    }

    #[test]
    fn shared_tool_sets_keep_expected_identities_and_host_access_classes() {
        let root =
            std::env::temp_dir().join(format!("nexus-shared-tool-sets-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());

        let strict = strict_tools(&root).unwrap();
        let development = development_tools(&root).unwrap();
        let identities = |tools: &[ToolRegistration]| {
            tools
                .iter()
                .map(|tool| (tool.port().describe().id().name().to_owned(), tool.access()))
                .collect::<Vec<_>>()
        };
        let expected = vec![
            ("host_read".to_owned(), ToolAccess::Read),
            ("host_list".to_owned(), ToolAccess::Read),
            ("host_search".to_owned(), ToolAccess::Read),
            ("host_write".to_owned(), ToolAccess::Mutation),
            ("host_patch".to_owned(), ToolAccess::Mutation),
            ("host_exec".to_owned(), ToolAccess::Process),
        ];
        assert_eq!(identities(&strict), expected);
        assert_eq!(identities(&development), expected);
    }
}
