//! Development file adapters re-use the jailed implementations after checking
//! the runtime's exact canonical target scope. They never infer approval from
//! arguments or maintain an independent session permission cache.

use std::path::Path;
use std::sync::Arc;

use crate::{ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter};
use nexus_core::{
    AgentError, EffectState, Evidence, ExecutionStatus, NormalizedArgs, ToolCall, ToolContext,
    ToolOutcome, ToolPort, ToolSpec,
};
use nexus_permissions::DirectoryPolicy;

pub fn development_file_tools(
    root: &Path,
) -> Result<Vec<Arc<dyn ToolPort + Send + Sync>>, AgentError> {
    let policy = DirectoryPolicy::new(root).map_err(|_| {
        AgentError::new(
            nexus_core::ErrorCategory::InvalidInput,
            "permissions root is invalid",
            nexus_core::RetryGuidance::DoNotRetry,
        )
        .expect("static permission error")
    })?;
    let ports: Vec<Arc<dyn ToolPort + Send + Sync>> = vec![
        Arc::new(ScopedReader::with_root(root)?),
        Arc::new(ScopedLister::with_root(root)?),
        Arc::new(ScopedSearcher::with_root(root)?),
        Arc::new(ScopedWriter::with_root(root)?),
        Arc::new(ScopedPatcher::with_root(root)?),
    ];
    ports.into_iter().map(|port| {
        let original = port.describe();
        let description = format!("{}. Development mode: project/temp paths are automatic; external paths need runtime approval. Protected credential paths are unavailable.", original.description().replace("inside the tool root", "at a runtime-authorized path").replace(". Requires approval.", "").trim_end_matches('.'));
        let spec = ToolSpec::new(original.id().clone(), description, original.input_schema_json())?;
        Ok(Arc::new(DevelopmentFile { policy: policy.clone(), spec }) as Arc<dyn ToolPort + Send + Sync>)
    }).collect()
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
            if context.scope().as_str() != format!("path:{}", target.to_string_lossy()) {
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
    use nexus_core::{ApprovedScope, CallId, M0_REVISION, RunId, ToolId, TurnId};
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
                .find(|port| port.describe().id().name() == tool)
                .unwrap()
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
}
