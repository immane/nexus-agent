//! Public-boundary hardening for `Policy::authorize` (M0 automatic reads).
//!
//! The unit tests inside `nexus-runtime::policy` exercise the same behavior
//! from inside the module; these tests pin the documented contract through
//! the public API only: exactly the canonical logical project-relative
//! `path` arguments of `host_read`/`host_search` at exactly [`M0_REVISION`]
//! are auto-approved, every mutation or command is refused automatic
//! execution, a wrong call or policy revision fails closed, and a wrong
//! revision or forbidden automatic set is rejected by `validate`/`try_new`
//! before it can authorize or scope anything.
//!
//! Determinism: fixed inputs only; no clock, no I/O, no threads, no sleeps.

#![forbid(unsafe_code)]

use nexus_core::approval::MAX_SCOPE_BYTES;
use nexus_core::{
    AgentError, CallId, ErrorCategory, M0_REVISION, NormalizedArgs, RetryGuidance, RunId, ToolCall,
    ToolId, TurnId,
};
use nexus_runtime::Policy;
use nexus_validation::{MAX_ARGS_DEPTH, MAX_ARGS_NODES};

const RUN: &str = "run-1";
const TURN: &str = "turn-1";
const CALL: &str = "call-1";

fn call(tool: &str, revision: u32, args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new(RUN).expect("test run id is valid"),
        TurnId::new(TURN).expect("test turn id is valid"),
        CallId::new(CALL).expect("test call id is valid"),
        ToolId::new(tool, revision).expect("test tool identity is valid"),
        NormalizedArgs::new(args).expect("test argument text is valid"),
    )
}

fn assert_invalid<T: std::fmt::Debug>(result: Result<T, AgentError>, message: &str) {
    let error = result.expect_err("invalid input must be rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), message);
}

fn assert_denied<T: std::fmt::Debug>(result: Result<T, AgentError>, message: &str) {
    let error = result.expect_err("a deviation must be denied");
    assert_eq!(error.category(), ErrorCategory::PermissionDenied);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), message);
}

fn assert_limit<T: std::fmt::Debug>(result: Result<T, AgentError>, message: &str) {
    let error = result.expect_err("budget exhaustion must be refused");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), message);
}

#[test]
fn authorize_auto_approves_only_canonical_project_reads_and_searches() {
    let policy = Policy::m0_test();
    assert_eq!(policy.revision(), M0_REVISION);
    policy.validate().expect("the M0 policy validates");
    for tool in ["host_read", "host_list", "host_search"] {
        assert!(
            !policy.requires_approval(&ToolId::new(tool, M0_REVISION).expect("valid")),
            "{tool} is automatic"
        );
    }

    let cases = [
        ("host_read", "src/lib.rs", "read:src/lib.rs"),
        ("host_read", "README.md", "read:README.md"),
        ("host_read", "a-b_c.d/e", "read:a-b_c.d/e"),
        ("host_read", "src/日本語.rs", "read:src/日本語.rs"),
        ("host_read", "src/my file.txt", "read:src/my file.txt"),
        ("host_list", "src", "list:src"),
        ("host_list", ".", "list:."),
        ("host_search", "src", "search:src"),
        (
            "host_search",
            "crates/nexus-core/src",
            "search:crates/nexus-core/src",
        ),
        (
            "host_search",
            "docs/design/10-pi-reference.md",
            "search:docs/design/10-pi-reference.md",
        ),
    ];
    for (tool, path, expected) in cases {
        let args = if tool == "host_search" {
            format!(r#"{{"path":"{path}","query":"needle"}}"#)
        } else {
            format!(r#"{{"path":"{path}"}}"#)
        };
        let scoped = call(tool, M0_REVISION, &args);
        let scope = policy
            .authorize(&scoped)
            .expect("a canonical project path authorizes");
        assert_eq!(scope.as_str(), expected, "{tool} {path}");

        // The decision is pure and repeatable: re-authorizing the same
        // immutable call, or an independently built equal call, yields the
        // identical scope and never mutates the policy.
        let repeated = policy.authorize(&scoped).expect("repeat authorizes");
        assert_eq!(repeated, scope, "{tool} {path}");
        let equal = call(tool, M0_REVISION, &args);
        assert_eq!(
            policy.authorize(&equal).expect("equal call authorizes"),
            scope
        );
    }
}

#[test]
fn authorize_refuses_mutations_and_commands_that_require_approval() {
    let policy = Policy::m0_test();
    let cases = [
        ("host_write", r#"{"path":"dst/file.txt","content":"hi"}"#),
        (
            "host_patch",
            r#"{"path":"src/lib.rs","old_text":"old","new_text":"new"}"#,
        ),
        ("host_exec", r#"{"argv":["git","status"]}"#),
        ("host_exec", r#"{"command":"cargo test"}"#),
        ("host_delete", r#"{"path":"dst/file.txt"}"#),
        ("host_edit", r#"{"path":"src/lib.rs"}"#),
        ("hostread", r#"{"path":"src"}"#),
        ("host-search", r#"{"path":"src"}"#),
        ("HOST_READ", r#"{"path":"src"}"#),
        ("Host_Search", r#"{"path":"src"}"#),
    ];
    for (tool, args) in cases {
        let scoped = call(tool, M0_REVISION, args);
        assert!(
            policy.requires_approval(scoped.tool()),
            "{tool} requires approval"
        );
        assert_denied(
            policy.authorize(&scoped),
            "tool is not authorized for automatic execution",
        );
    }
}

#[test]
fn authorize_requires_the_exact_m0_revision_on_call_and_policy() {
    let policy = Policy::m0_test();
    for revision in [M0_REVISION + 1, M0_REVISION + 2, u32::MAX] {
        let wrong = call("host_read", revision, r#"{"path":"src"}"#);
        assert_denied(
            policy.authorize(&wrong),
            "tool revision does not match the policy revision",
        );
    }

    // A policy whose own revision is wrong fails validation before any call
    // decision, even when the call revision matches that wrong policy.
    for revision in [M0_REVISION + 1, u32::MAX] {
        let wrong_policy = Policy::new(vec!["host_read".to_owned()], revision);
        assert_eq!(wrong_policy.revision(), revision);
        assert_invalid(
            wrong_policy.validate(),
            "policy revision is not the M0 revision",
        );
        assert_invalid(
            wrong_policy.authorize(&call("host_read", revision, r#"{"path":"src"}"#)),
            "policy revision is not the M0 revision",
        );
        assert_invalid(
            wrong_policy.authorize(&call("host_read", M0_REVISION, r#"{"path":"src"}"#)),
            "policy revision is not the M0 revision",
        );
    }

    // A valid policy with a wrong call revision denies at the revision check.
    let custom = Policy::new(vec!["host_read".to_owned()], M0_REVISION);
    assert_denied(
        custom.authorize(&call("host_read", M0_REVISION + 1, r#"{"path":"src"}"#)),
        "tool revision does not match the policy revision",
    );
}

#[test]
fn validate_rejects_wrong_revisions_and_forbidden_automatic_sets() {
    assert!(Policy::m0_test().validate().is_ok());
    for accepted in [
        vec!["host_read".to_owned()],
        vec!["host_search".to_owned()],
        vec!["host_read".to_owned(), "host_search".to_owned()],
        vec!["host_read".to_owned(), "host_read".to_owned()],
        Vec::new(),
    ] {
        assert!(
            Policy::try_new(accepted.clone(), M0_REVISION).is_ok(),
            "a conservative or exact subset is valid: {accepted:?}"
        );
    }

    for revision in [M0_REVISION + 1, M0_REVISION + 2, u32::MAX] {
        assert_invalid(
            Policy::new(vec!["host_read".to_owned()], revision).validate(),
            "policy revision is not the M0 revision",
        );
        assert_invalid(
            Policy::try_new(vec!["host_read".to_owned()], revision),
            "policy revision is not the M0 revision",
        );
    }

    for forbidden in [
        "host_write",
        "host_exec",
        "host_delete",
        "host_edit",
        "host_shell",
        "HOST_READ",
        "Host_Search",
        "host-search",
        "hostread",
        "host_search2",
        "host_read-x",
        "host_read ",
        "host_read\n",
        "",
        "read",
    ] {
        assert_invalid(
            Policy::new(vec![forbidden.to_owned()], M0_REVISION).validate(),
            "policy auto-approved tool set is invalid",
        );
        assert_invalid(
            Policy::try_new(vec![forbidden.to_owned()], M0_REVISION),
            "policy auto-approved tool set is invalid",
        );
    }

    // One forbidden member invalidates a set that also names valid tools.
    assert_invalid(
        Policy::new(
            vec!["host_read".to_owned(), "host_exec".to_owned()],
            M0_REVISION,
        )
        .validate(),
        "policy auto-approved tool set is invalid",
    );
    // The revision diagnostic precedes the automatic-set diagnostic.
    assert_invalid(
        Policy::new(vec!["host_write".to_owned()], M0_REVISION + 1).validate(),
        "policy revision is not the M0 revision",
    );
}

#[test]
fn forbidden_automatic_sets_fail_closed_inside_authorize() {
    for (tool, args) in [
        ("host_write", r#"{"path":"dst"}"#),
        ("host_exec", r#"{"argv":"run"}"#),
    ] {
        let policy = Policy::new(vec![tool.to_owned()], M0_REVISION);
        let scoped = call(tool, M0_REVISION, args);
        // The compatibility classification view stays name-based...
        assert!(!policy.requires_approval(scoped.tool()));
        // ...but the authorization boundary validates the policy first and
        // refuses, so a forbidden set can never widen automatic execution.
        assert_invalid(
            policy.authorize(&scoped),
            "policy auto-approved tool set is invalid",
        );
    }

    // A forbidden member also invalidates the whole set, including reads.
    let mixed = Policy::new(
        vec!["host_read".to_owned(), "host_exec".to_owned()],
        M0_REVISION,
    );
    assert_invalid(
        mixed.authorize(&call("host_read", M0_REVISION, r#"{"path":"src"}"#)),
        "policy auto-approved tool set is invalid",
    );
}

#[test]
fn an_empty_automatic_set_requires_approval_for_every_tool() {
    let closed = Policy::try_new(Vec::new(), M0_REVISION).expect("conservative policy builds");
    assert_eq!(closed.revision(), M0_REVISION);
    closed.validate().expect("the empty set is valid");
    for tool in [
        "host_read",
        "host_list",
        "host_search",
        "host_write",
        "host_patch",
        "host_exec",
    ] {
        let scoped = call(tool, M0_REVISION, r#"{"path":"src"}"#);
        assert!(closed.requires_approval(scoped.tool()), "{tool}");
        assert_denied(
            closed.authorize(&scoped),
            "tool is not authorized for automatic execution",
        );
    }
}

#[test]
fn authorize_refuses_every_non_canonical_path_shape() {
    let cases: &[(&str, &str)] = &[
        (r#"{"path":""}"#, "tool path is empty"),
        (r#"{"path":"/etc/passwd"}"#, "tool path is absolute"),
        (r#"{"path":"/"}"#, "tool path is absolute"),
        (r#"{"path":"~"}"#, "tool path is outside the project scope"),
        (
            r#"{"path":"~/secrets"}"#,
            "tool path is outside the project scope",
        ),
        (
            r#"{"path":"~user/x"}"#,
            "tool path is outside the project scope",
        ),
        (
            r#"{"path":"src\\lib.rs"}"#,
            "tool path uses backslash separators",
        ),
        (
            r#"{"path":"..\\windows"}"#,
            "tool path uses backslash separators",
        ),
        (
            r#"{"path":"\\\\server\\share"}"#,
            "tool path uses backslash separators",
        ),
        (
            r#"{"path":"src/lib.rs:stream"}"#,
            "tool path contains invalid characters",
        ),
        (
            r#"{"path":"C:/Users"}"#,
            "tool path contains invalid characters",
        ),
        (r#"{"path":"a<b"}"#, "tool path contains invalid characters"),
        (r#"{"path":"a>b"}"#, "tool path contains invalid characters"),
        (r#"{"path":"a:b"}"#, "tool path contains invalid characters"),
        (
            r#"{"path":"a\"b"}"#,
            "tool path contains invalid characters",
        ),
        (r#"{"path":"a|b"}"#, "tool path contains invalid characters"),
        (r#"{"path":"a?b"}"#, "tool path contains invalid characters"),
        (r#"{"path":"a*b"}"#, "tool path contains invalid characters"),
        (
            r#"{"path":"src/\u0001lib.rs"}"#,
            "tool path contains invalid characters",
        ),
        (
            r#"{"path":"src/\u007flib.rs"}"#,
            "tool path contains invalid characters",
        ),
        (
            r#"{"path":"src/\u0085lib.rs"}"#,
            "tool path contains invalid characters",
        ),
        (
            r#"{"path":"src//lib.rs"}"#,
            "tool path contains an empty component",
        ),
        (
            r#"{"path":"src/"}"#,
            "tool path contains an empty component",
        ),
        (r#"{"path":"."}"#, "tool path contains a parent traversal"),
        (r#"{"path":".."}"#, "tool path contains a parent traversal"),
        (
            r#"{"path":"src/../etc"}"#,
            "tool path contains a parent traversal",
        ),
        (
            r#"{"path":"src/./lib.rs"}"#,
            "tool path contains a parent traversal",
        ),
    ];
    let policy = Policy::m0_test();
    for &(args, message) in cases {
        assert_denied(
            policy.authorize(&call("host_read", M0_REVISION, args)),
            message,
        );
    }
}

#[test]
fn authorize_refuses_non_canonical_and_ambiguous_argument_objects() {
    let policy = Policy::m0_test();
    for args in [
        r#"{}"#,
        r#"{"path":5}"#,
        r#"{"path":null}"#,
        r#"{"path":["src"]}"#,
        r#"{"path":{"value":"src"}}"#,
        r#"{"path":true}"#,
        r#"{"path":"src","recursive":true}"#,
        r#"{"path":"src","limit":1}"#,
    ] {
        assert_denied(
            policy.authorize(&call("host_read", M0_REVISION, args)),
            "automatic tool arguments are not a canonical path",
        );
    }

    // Duplicate `path` keys are ambiguous and rejected by the strict parser,
    // never resolved last-key-wins, whatever the values are.
    for args in [
        r#"{"path":"/etc/passwd","path":"src"}"#,
        r#"{"path":"src","path":"/etc/passwd"}"#,
        r#"{"path":"src","path":"src"}"#,
    ] {
        assert_invalid(
            policy.authorize(&call("host_read", M0_REVISION, args)),
            "arguments contain a duplicate object key",
        );
    }

    for args in [
        r#"{oops}"#,
        r#"{"path":"src"} {"other":1}"#,
        r#"{"path":1e999}"#,
    ] {
        assert_invalid(
            policy.authorize(&call("host_read", M0_REVISION, args)),
            "arguments are not valid JSON",
        );
    }
}

#[test]
fn authorize_refuses_exhausted_parse_budgets_before_any_decision() {
    let policy = Policy::m0_test();

    let mut nested = String::new();
    for _ in 0..=MAX_ARGS_DEPTH {
        nested.push('[');
    }
    for _ in 0..=MAX_ARGS_DEPTH {
        nested.push(']');
    }
    assert_limit(
        policy.authorize(&call(
            "host_read",
            M0_REVISION,
            &format!(r#"{{"a":{nested}}}"#),
        )),
        "argument depth budget exhausted",
    );

    let items = vec!["0"; MAX_ARGS_NODES].join(",");
    assert_limit(
        policy.authorize(&call(
            "host_read",
            M0_REVISION,
            &format!(r#"{{"a":[{items}]}}"#),
        )),
        "argument node budget exhausted",
    );
}

#[test]
fn authorize_scope_bound_is_exact_bytes_and_refusals_are_static() {
    let policy = Policy::m0_test();

    let read_path = "a".repeat(MAX_SCOPE_BYTES - "read:".len());
    let exact_read = call(
        "host_read",
        M0_REVISION,
        &format!(r#"{{"path":"{read_path}"}}"#),
    );
    let scope = policy
        .authorize(&exact_read)
        .expect("the exact read bound is accepted");
    assert_eq!(scope.as_str().len(), MAX_SCOPE_BYTES);
    assert_eq!(scope.as_str(), format!("read:{read_path}"));

    let over_read = "a".repeat(MAX_SCOPE_BYTES - "read:".len() + 1);
    let error = policy
        .authorize(&call(
            "host_read",
            M0_REVISION,
            &format!(r#"{{"path":"{over_read}"}}"#),
        ))
        .expect_err("one byte over the read bound is refused");
    assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert_eq!(error.message(), "tool resource scope exceeds its bound");
    assert!(!error.message().contains('a'), "refusals never echo input");

    let search_path = "s".repeat(MAX_SCOPE_BYTES - "search:".len());
    let scope = policy
        .authorize(&call(
            "host_search",
            M0_REVISION,
            &format!(r#"{{"path":"{search_path}","query":"x"}}"#),
        ))
        .expect("the exact search bound is accepted");
    assert_eq!(scope.as_str().len(), MAX_SCOPE_BYTES);

    // The bound counts UTF-8 bytes, not characters: this path is well under
    // 1019 characters but exactly at the byte bound, and one byte more fails.
    let multibyte = format!("{}a", "é".repeat((MAX_SCOPE_BYTES - "read:".len() - 1) / 2));
    assert_eq!(multibyte.len(), MAX_SCOPE_BYTES - "read:".len());
    let scope = policy
        .authorize(&call(
            "host_read",
            M0_REVISION,
            &format!(r#"{{"path":"{multibyte}"}}"#),
        ))
        .expect("a byte-exact multibyte path is accepted");
    assert_eq!(scope.as_str().len(), MAX_SCOPE_BYTES);

    let over_multibyte = format!(
        "{}aa",
        "é".repeat((MAX_SCOPE_BYTES - "read:".len() - 1) / 2)
    );
    assert_eq!(over_multibyte.len(), MAX_SCOPE_BYTES - "read:".len() + 1);
    let error = policy
        .authorize(&call(
            "host_read",
            M0_REVISION,
            &format!(r#"{{"path":"{over_multibyte}"}}"#),
        ))
        .expect_err("one byte over the multibyte bound is refused");
    assert_eq!(error.message(), "tool resource scope exceeds its bound");
}

#[test]
fn mutations_and_commands_resolve_approval_scopes_without_authorization() {
    let policy = Policy::m0_test();

    let write = call(
        "host_write",
        M0_REVISION,
        r#"{"path":"dst/file.txt","content":"hi"}"#,
    );
    assert_denied(
        policy.authorize(&write),
        "tool is not authorized for automatic execution",
    );
    assert_eq!(
        policy
            .approval_scope(&write)
            .expect("the mutation still resolves an approval scope")
            .as_str(),
        "path:dst/file.txt"
    );

    let exec = call("host_exec", M0_REVISION, r#"{"argv":["git","status"]}"#);
    assert_denied(
        policy.authorize(&exec),
        "tool is not authorized for automatic execution",
    );
    assert_eq!(
        policy
            .approval_scope(&exec)
            .expect("the command still resolves an approval scope")
            .as_str(),
        "command:git status"
    );
}
