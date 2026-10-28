//! Development file adapters re-use the jailed implementations after checking
//! the runtime's exact canonical target scope. They never infer approval from
//! arguments or maintain an independent session permission cache.

use std::path::Path;
use std::sync::Arc;

use crate::{
    SandboxedExecutor, ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter,
};
use nexus_config::ToolsSettings;
use nexus_core::{
    AgentError, EffectState, Evidence, ExecutionStatus, ToolAccess, ToolCall, ToolContext,
    ToolOutcome, ToolPort, ToolRegistration, ToolSpec,
};
use nexus_permissions::DirectoryPolicy;

/// Constructs the real-file strict tool set shared by the TUI and server.
/// Runtime policy and provider composition remain owned by each entrypoint.
pub fn strict_tools(root: &Path) -> Result<Vec<ToolRegistration>, AgentError> {
    strict_tools_with_settings(root, &ToolsSettings::default())
}

/// Constructs strict tools with validated filesystem operational ceilings.
pub fn strict_tools_with_settings(
    root: &Path,
    settings: &ToolsSettings,
) -> Result<Vec<ToolRegistration>, AgentError> {
    Ok(vec![
        ToolRegistration::read(Arc::new(ScopedReader::with_root(root)?)),
        ToolRegistration::read(Arc::new(
            ScopedLister::with_root(root)?.with_settings(settings)?,
        )),
        ToolRegistration::read(Arc::new(
            ScopedSearcher::with_root(root)?.with_settings(settings)?,
        )),
        ToolRegistration::mutation(Arc::new(ScopedWriter::with_root(root)?)),
        ToolRegistration::mutation(Arc::new(ScopedPatcher::with_root(root)?)),
        ToolRegistration::process(Arc::new(SandboxedExecutor::with_root(root)?)),
    ])
}

/// Constructs development file and exec tools shared by the TUI and server.
/// Runtime permission policy is still configured by the caller.
pub fn development_tools(root: &Path) -> Result<Vec<ToolRegistration>, AgentError> {
    development_tools_with_settings(root, &ToolsSettings::default())
}

/// Constructs development tools with validated filesystem operational ceilings.
pub fn development_tools_with_settings(
    root: &Path,
    settings: &ToolsSettings,
) -> Result<Vec<ToolRegistration>, AgentError> {
    let mut tools = development_file_tools_with_settings(root, settings)?;
    tools.push(ToolRegistration::process(Arc::new(
        SandboxedExecutor::development(root)?,
    )));
    Ok(tools)
}

pub fn development_file_tools(root: &Path) -> Result<Vec<ToolRegistration>, AgentError> {
    development_file_tools_with_settings(root, &ToolsSettings::default())
}

/// Constructs development file tools with validated operational ceilings.
pub fn development_file_tools_with_settings(
    root: &Path,
    settings: &ToolsSettings,
) -> Result<Vec<ToolRegistration>, AgentError> {
    let policy = DirectoryPolicy::new(root).map_err(|_| {
        AgentError::new(
            nexus_core::ErrorCategory::InvalidInput,
            "permissions root is invalid",
            nexus_core::RetryGuidance::DoNotRetry,
        )
        .expect("static permission error")
    })?;
    // The trusted dispatcher checks the exact resolved path before forwarding
    // to these preconstructed concrete operations. Their descriptor-bound
    // root is `/` so the operation can cover project and temporary scopes;
    // symlinks are still rejected during the descriptor walk.
    let operations = vec![
        (
            ToolAccess::Read,
            DevelopmentOperation::Read(ScopedReader::with_root(Path::new("/"))?),
        ),
        (
            ToolAccess::Read,
            DevelopmentOperation::List(
                ScopedLister::with_root(Path::new("/"))?
                    .with_settings(settings)?
                    .with_protected_paths(policy.clone()),
            ),
        ),
        (
            ToolAccess::Read,
            DevelopmentOperation::Search(
                ScopedSearcher::with_root(Path::new("/"))?
                    .with_settings(settings)?
                    .with_protected_paths(policy.clone()),
            ),
        ),
        (
            ToolAccess::Mutation,
            DevelopmentOperation::Write(ScopedWriter::with_root(Path::new("/"))?),
        ),
        (
            ToolAccess::Mutation,
            DevelopmentOperation::Patch(ScopedPatcher::with_root(Path::new("/"))?),
        ),
    ];
    operations
        .into_iter()
        .map(|(access, operation)| {
            let (original, description) = operation.describe();
            let spec = ToolSpec::new(
                original.id().clone(),
                description,
                original.input_schema_json(),
            )?;
            Ok(ToolRegistration::classified(
                Arc::new(DevelopmentFile {
                    policy: policy.clone(),
                    operation,
                    spec,
                }) as Arc<dyn ToolPort + Send + Sync>,
                access,
            ))
        })
        .collect()
}

struct DevelopmentFile {
    policy: DirectoryPolicy,
    operation: DevelopmentOperation,
    spec: ToolSpec,
}

enum DevelopmentOperation {
    Read(ScopedReader),
    List(ScopedLister),
    Search(ScopedSearcher),
    Write(ScopedWriter),
    Patch(ScopedPatcher),
}

impl DevelopmentOperation {
    fn describe(&self) -> (ToolSpec, &'static str) {
        match self {
            Self::Read(port) => (
                port.describe(),
                "Read a bounded UTF-8 byte page from a runtime-authorized file. Defaults: offset=0, limit=8192; both must be integers, limit is 4..16384. Follow next_offset on truncation. Offsets are zero-based bytes, not lines. Protected credential paths are unavailable.",
            ),
            Self::List(port) => (
                port.describe(),
                "List direct children of a runtime-authorized directory. Protected credential paths are omitted.",
            ),
            Self::Search(port) => (
                port.describe(),
                "Search UTF-8 text files beneath a runtime-authorized directory. Protected credential paths are omitted.",
            ),
            Self::Write(port) => (
                port.describe(),
                "Write a runtime-authorized file path, creating it when missing and replacing its whole content otherwise. Parent directories must already exist. Protected credential paths are unavailable.",
            ),
            Self::Patch(port) => (
                port.describe(),
                "Replace one unique exact text occurrence in an existing runtime-authorized UTF-8 file. Protected credential paths are unavailable.",
            ),
        }
    }

    fn execute(
        &self,
        target: &std::path::Path,
        args: &serde_json::Value,
        context: &ToolContext,
    ) -> ToolOutcome {
        match self {
            Self::Read(port) => port.execute_resolved(target, args, context),
            Self::List(port) => port.execute_resolved(target, context),
            Self::Search(port) => args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .map_or_else(denied, |query| {
                    port.execute_resolved(target, query, context)
                }),
            Self::Write(port) => args
                .get("content")
                .and_then(serde_json::Value::as_str)
                .map_or_else(denied, |content| {
                    port.execute_development(target, content, context)
                }),
            Self::Patch(port) => match (
                args.get("old_text").and_then(serde_json::Value::as_str),
                args.get("new_text").and_then(serde_json::Value::as_str),
            ) {
                (Some(old), Some(new)) => port.execute_development(target, old, new, context),
                _ => denied(),
            },
        }
    }
}

fn denied() -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Denied,
        EffectState::NotStarted,
        Evidence::HostObserved,
        "filesystem target is not authorized",
        false,
    )
    .expect("static denial")
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
            let args: serde_json::Value =
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
            if self.policy.is_protected(&target) {
                return Err(());
            }
            Ok(self.operation.execute(&target, &args, context))
        })();
        result.unwrap_or_else(|_| denied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{
        ApprovedScope, CallId, M0_REVISION, NormalizedArgs, RunId, ToolAccess, ToolId, TurnId,
    };
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
        let page = run(
            "host_read",
            serde_json::json!({"path":"plain.txt", "offset":9, "limit":4}),
            format!("path:{}", plain.display()),
        );
        assert_eq!(page.content(), "data");
        assert!(!page.is_truncated());
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
        let protected_write = run(
            "host_write",
            serde_json::json!({"path":".env", "content":"must not appear"}),
            format!("path:{}", root.join(".env").display()),
        );
        assert_eq!(protected_write.status(), ExecutionStatus::Denied);
        assert_eq!(
            std::fs::read_to_string(root.join(".env")).unwrap(),
            "hidden data"
        );
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

    #[test]
    fn development_operations_reuse_bound_tools_for_external_scopes() {
        let root =
            std::env::temp_dir().join(format!("nexus-development-root-{}", std::process::id()));
        let external =
            std::env::temp_dir().join(format!("nexus-development-external-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        let root = root.canonicalize().unwrap();
        let external = external.canonicalize().unwrap();
        struct Cleanup(std::path::PathBuf, std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
                let _ = std::fs::remove_dir_all(&self.1);
            }
        }
        let _cleanup = Cleanup(root.clone(), external.clone());
        std::fs::write(external.join("input.txt"), "before token after\n").unwrap();
        let policy = DirectoryPolicy::new(&root).unwrap();
        let tools = development_file_tools(&root).unwrap();
        let invoke = |tool: &str, args: serde_json::Value, target: &std::path::Path| {
            let call = ToolCall::new(
                RunId::new("run-external").unwrap(),
                TurnId::new("turn-external").unwrap(),
                CallId::new(format!("call-{tool}")).unwrap(),
                ToolId::new(tool, M0_REVISION).unwrap(),
                NormalizedArgs::new(args.to_string()).unwrap(),
            );
            let context = ToolContext::new(
                4096,
                Duration::from_secs(60),
                false,
                ApprovedScope::new(format!("path:{}", target.display())).unwrap(),
            )
            .unwrap();
            tools
                .iter()
                .find(|registration| registration.port().describe().id().name() == tool)
                .unwrap()
                .port()
                .execute(&call, &context)
        };
        let file = external.join("input.txt").canonicalize().unwrap();
        let read = invoke("host_read", serde_json::json!({"path":file}), &file);
        assert_eq!(
            read.status(),
            ExecutionStatus::Succeeded,
            "{}",
            read.content()
        );
        assert!(read.content().contains("before token after"));
        let directory = external.canonicalize().unwrap();
        let search = invoke(
            "host_search",
            serde_json::json!({"path":directory,"query":"token"}),
            &directory,
        );
        assert_eq!(
            search.status(),
            ExecutionStatus::Succeeded,
            "{}",
            search.content()
        );
        assert!(search.content().contains("input.txt:1:before token after"));
        let written_path = external.join("new.txt");
        let written = invoke(
            "host_write",
            serde_json::json!({"path":written_path,"content":"created"}),
            &written_path,
        );
        assert_eq!(
            written.status(),
            ExecutionStatus::Succeeded,
            "{}",
            written.content()
        );
        assert_eq!(std::fs::read_to_string(written_path).unwrap(), "created");
        let patched = invoke(
            "host_patch",
            serde_json::json!({"path":file,"old_text":"token","new_text":"patched"}),
            &file,
        );
        assert_eq!(
            patched.status(),
            ExecutionStatus::Succeeded,
            "{}",
            patched.content()
        );
        assert_eq!(
            std::fs::read_to_string(file).unwrap(),
            "before patched after\n"
        );
        let cancelled_call = ToolCall::new(
            RunId::new("run-external").unwrap(),
            TurnId::new("turn-external").unwrap(),
            CallId::new("call-cancelled").unwrap(),
            ToolId::new("host_read", M0_REVISION).unwrap(),
            NormalizedArgs::new(serde_json::json!({"path":directory}).to_string()).unwrap(),
        );
        let token = nexus_core::CancellationToken::new();
        let cancelled = ToolContext::new(
            4096,
            Duration::ZERO,
            false,
            ApprovedScope::new(format!("path:{}", directory.display())).unwrap(),
        )
        .unwrap()
        .with_control(
            token.clone(),
            std::time::Instant::now() + Duration::from_secs(60),
        );
        token.cancel();
        let outcome = tools
            .iter()
            .find(|registration| registration.port().describe().id().name() == "host_read")
            .unwrap()
            .port()
            .execute(&cancelled_call, &cancelled);
        assert_eq!(outcome.status(), ExecutionStatus::Cancelled);
        let expired = ToolContext::new(
            4096,
            Duration::ZERO,
            false,
            ApprovedScope::new(format!("path:{}", directory.display())).unwrap(),
        )
        .unwrap()
        .with_control(
            nexus_core::CancellationToken::new(),
            std::time::Instant::now() - Duration::from_millis(1),
        );
        let outcome = tools
            .iter()
            .find(|registration| registration.port().describe().id().name() == "host_read")
            .unwrap()
            .port()
            .execute(&cancelled_call, &expired);
        assert_eq!(outcome.status(), ExecutionStatus::TimedOut);
        let write_path = external.join("deadline.txt");
        let expired_write = ToolCall::new(
            RunId::new("run-external").unwrap(),
            TurnId::new("turn-external").unwrap(),
            CallId::new("call-expired-write").unwrap(),
            ToolId::new("host_write", M0_REVISION).unwrap(),
            NormalizedArgs::new(
                serde_json::json!({"path":write_path,"content":"must not publish"}).to_string(),
            )
            .unwrap(),
        );
        let expired_outcome = tools
            .iter()
            .find(|registration| registration.port().describe().id().name() == "host_write")
            .unwrap()
            .port()
            .execute(&expired_write, &expired);
        assert_eq!(expired_outcome.status(), ExecutionStatus::TimedOut);
        assert!(!write_path.exists());
        assert!(!policy.is_protected(&directory));
    }

    #[test]
    fn strict_and_development_file_operations_keep_matching_outcomes() {
        let base = std::env::temp_dir().join(format!("nexus-file-parity-{}", std::process::id()));
        let strict_root = base.join("strict");
        let development_root = base.join("development");
        std::fs::create_dir_all(&strict_root).unwrap();
        std::fs::create_dir_all(&development_root).unwrap();
        let strict_root = strict_root.canonicalize().unwrap();
        let development_root = development_root.canonicalize().unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(base);
        for root in [&strict_root, &development_root] {
            std::fs::write(root.join("input.txt"), "before token after\n").unwrap();
            std::fs::write(root.join("peer.txt"), "token peer\n").unwrap();
        }
        let strict = strict_tools(&strict_root).unwrap();
        let development = development_file_tools(&development_root).unwrap();
        let invoke =
            |tools: &[ToolRegistration], tool: &str, args: serde_json::Value, scope: String| {
                let call = ToolCall::new(
                    RunId::new("run-parity").unwrap(),
                    TurnId::new("turn-parity").unwrap(),
                    CallId::new(format!("call-{tool}")).unwrap(),
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
        for tool in [
            "host_read",
            "host_list",
            "host_search",
            "host_write",
            "host_patch",
        ] {
            let strict_path = match tool {
                "host_list" | "host_search" => strict_root.clone(),
                "host_write" => strict_root.join("created.txt"),
                _ => strict_root.join("input.txt"),
            };
            let development_path = match tool {
                "host_list" | "host_search" => development_root.clone(),
                "host_write" => development_root.join("created.txt"),
                _ => development_root.join("input.txt"),
            };
            let strict_args = match tool {
                "host_list" => serde_json::json!({"path":"."}),
                "host_search" => serde_json::json!({"path":".","query":"token"}),
                "host_write" => serde_json::json!({"path":"created.txt","content":"created"}),
                "host_patch" => {
                    serde_json::json!({"path":"input.txt","old_text":"token","new_text":"patched"})
                }
                _ => serde_json::json!({"path":"input.txt"}),
            };
            let development_args = match tool {
                "host_list" => serde_json::json!({"path":development_path}),
                "host_search" => serde_json::json!({"path":development_path,"query":"token"}),
                "host_write" => serde_json::json!({"path":development_path,"content":"created"}),
                "host_patch" => {
                    serde_json::json!({"path":development_path,"old_text":"token","new_text":"patched"})
                }
                _ => serde_json::json!({"path":development_path}),
            };
            let strict_outcome = invoke(
                &strict,
                tool,
                strict_args,
                format!("path:{}", strict_path.display()),
            );
            let development_outcome = invoke(
                &development,
                tool,
                development_args,
                format!("path:{}", development_path.display()),
            );
            assert_eq!(
                strict_outcome.status(),
                development_outcome.status(),
                "{tool}"
            );
            assert_eq!(
                strict_outcome.effect(),
                development_outcome.effect(),
                "{tool}"
            );
            assert_eq!(
                strict_outcome.content(),
                development_outcome.content(),
                "{tool}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(strict_root.join("input.txt")).unwrap(),
            std::fs::read_to_string(development_root.join("input.txt")).unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(strict_root.join("created.txt")).unwrap(),
            std::fs::read_to_string(development_root.join("created.txt")).unwrap()
        );
    }

    #[test]
    fn development_patch_rejects_an_empty_match_without_changing_file() {
        let root = std::env::temp_dir().join(format!(
            "nexus-development-empty-patch-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        std::fs::write(root.join("empty.txt"), "").unwrap();
        let root = root.canonicalize().unwrap();
        let tools = development_file_tools(&root).unwrap();
        let call = ToolCall::new(
            RunId::new("run-empty-patch").unwrap(),
            TurnId::new("turn-empty-patch").unwrap(),
            CallId::new("call-empty-patch").unwrap(),
            ToolId::new("host_patch", M0_REVISION).unwrap(),
            NormalizedArgs::new(
                serde_json::json!({
                    "path":root.join("empty.txt"),
                    "old_text":"",
                    "new_text":"inserted"
                })
                .to_string(),
            )
            .unwrap(),
        );
        let context = ToolContext::new(
            4096,
            Duration::ZERO,
            false,
            ApprovedScope::new(format!("path:{}", root.join("empty.txt").display())).unwrap(),
        )
        .unwrap();
        let result = tools
            .iter()
            .find(|registration| registration.port().describe().id().name() == "host_patch")
            .unwrap()
            .port()
            .execute(&call, &context);
        assert_eq!(result.status(), ExecutionStatus::Failed);
        assert_eq!(result.effect(), EffectState::Unknown);
        assert_eq!(std::fs::read_to_string(root.join("empty.txt")).unwrap(), "");
    }
}
