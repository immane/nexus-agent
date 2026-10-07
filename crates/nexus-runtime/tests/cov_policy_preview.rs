//! Public-boundary hardening for `Policy::approval_scope` and
//! `Policy::approval_preview`.
//!
//! The unit tests inside `nexus-runtime::policy` exercise these helpers from
//! inside the module; this suite pins the documented contract through the
//! public API only:
//!
//! - exact path/command/tool scope resolution, command precedence, and
//!   control escaping;
//! - preview control escaping, conservative key redaction, and best-effort
//!   value-marker refusal;
//! - secret-bearing command labels refused without echoing the input;
//! - exact byte bounds with static diagnostics, refused rather than
//!   truncated into ambiguous labels;
//! - duplicate-key, depth, node, and malformed JSON rejected before any
//!   policy decision, with policy validation ahead of argument parsing.
//!
//! Determinism: fixed literals only; no clock, filesystem, randomness, or
//! concurrency. Every assertion goes through public constructors and
//! accessors.

#![forbid(unsafe_code)]

use nexus_core::approval::MAX_SCOPE_BYTES;
use nexus_core::commands::MAX_SUMMARY_BYTES;
use nexus_core::{
    AgentError, CallId, ErrorCategory, Limits, M0_REVISION, NormalizedArgs, RetryGuidance, RunId,
    ToolCall, ToolId, TurnId,
};
use nexus_runtime::Policy;
use nexus_validation::{MAX_ARGS_DEPTH, MAX_ARGS_NODES};

/// Placeholder replacing a redacted value (private const in `policy.rs`).
const REDACTED_VALUE: &str = "[redacted]";
/// Placeholder replacing a redacted object key (private const in `policy.rs`).
const REDACTED_KEY: &str = "[redacted-key]";

const SECRET_MESSAGE: &str = "tool arguments may contain secret material";
const SCOPE_BOUND_MESSAGE: &str = "tool resource scope exceeds its bound";
const PREVIEW_BOUND_MESSAGE: &str = "approval preview exceeds its bound";
const DUPLICATE_MESSAGE: &str = "arguments contain a duplicate object key";
const MALFORMED_MESSAGE: &str = "arguments are not valid JSON";
const DEPTH_MESSAGE: &str = "argument depth budget exhausted";
const NODE_MESSAGE: &str = "argument node budget exhausted";

fn call_at(tool: &str, revision: u32, args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-1").expect("fixed run id is valid"),
        TurnId::new("turn-1").expect("fixed turn id is valid"),
        CallId::new("call-1").expect("fixed call id is valid"),
        ToolId::new(tool, revision).expect("fixed tool id is valid"),
        NormalizedArgs::new(args).expect("test arguments are object-root within budget"),
    )
}

fn call(tool: &str, args: &str) -> ToolCall {
    call_at(tool, M0_REVISION, args)
}

fn resolved_scope(policy: &Policy, call: &ToolCall) -> String {
    policy
        .approval_scope(call)
        .expect("scope resolves")
        .as_str()
        .to_owned()
}

fn assert_refused<T: std::fmt::Debug>(
    result: Result<T, AgentError>,
    category: ErrorCategory,
    message: &str,
) -> AgentError {
    let error = result.expect_err("the action must be refused");
    assert_eq!(error.category(), category, "category for {message:?}");
    assert_eq!(error.message(), message);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        error.correlation().is_empty(),
        "policy diagnostics carry no correlation data"
    );
    error
}

fn assert_static(error: &AgentError, forbidden: &[&str]) {
    for text in forbidden {
        assert!(
            !error.message().contains(text),
            "diagnostic {:?} must not echo {text:?}",
            error.message()
        );
    }
}

#[test]
fn approval_scope_resolves_path_command_and_tool_fallback_exactly() {
    let policy = Policy::m0_test();

    // Path scope: canonical project-relative path.
    let write = call("host_write", r#"{"path":"dst/file.txt","content":"hi"}"#);
    assert_eq!(resolved_scope(&policy, &write), "path:dst/file.txt");
    assert_eq!(
        resolved_scope(&policy, &call("host_list", r#"{"path":"."}"#)),
        "path:."
    );
    // Scope resolution describes a resource, not adapter input validation or
    // authorization to mutate the jail root.
    assert_eq!(
        resolved_scope(&policy, &call("host_write", r#"{"path":"."}"#)),
        "path:."
    );

    // Path resolution precedes command extraction: the command text must not
    // leak into the label of a call that also names a path.
    let both = call("host_exec", r#"{"path":"src","command":"rm -rf /"}"#);
    assert_eq!(resolved_scope(&policy, &both), "path:src");

    // Command scope: a `command` string.
    let exec = call("host_exec", r#"{"command":"git status"}"#);
    assert_eq!(resolved_scope(&policy, &exec), "command:git status");

    // Command scope: an `argv` array joined with single spaces.
    let array = call("host_exec", r#"{"argv":["git","status","--short"]}"#);
    assert_eq!(
        resolved_scope(&policy, &array),
        "command:git status --short"
    );

    // `command` is read before `argv` when both are present.
    let both_commands = call("host_exec", r#"{"command":"first","argv":["second"]}"#);
    assert_eq!(resolved_scope(&policy, &both_commands), "command:first");

    // The extraction helper is key-agnostic: a string under `argv` and a
    // string array under `command` resolve through the same branch.
    let argv_string = call("host_exec", r#"{"argv":"git log"}"#);
    assert_eq!(resolved_scope(&policy, &argv_string), "command:git log");
    let command_array = call("host_exec", r#"{"command":["git","log"]}"#);
    assert_eq!(resolved_scope(&policy, &command_array), "command:git log");

    // Control characters are escaped in the command label, never raw.
    let control = call("host_exec", r#"{"command":"a\u0001b"}"#);
    let control_scope = resolved_scope(&policy, &control);
    assert_eq!(control_scope, "command:a\\u0001b");
    assert!(!control_scope.chars().any(char::is_control));

    // Fallback: no path and no command means a tool-labelled scope.
    assert_eq!(
        resolved_scope(&policy, &call("host_write", r#"{"content":"hi"}"#)),
        "tool:host_write"
    );
    assert_eq!(
        resolved_scope(&policy, &call("host_write", "{}")),
        "tool:host_write"
    );
    assert_eq!(
        resolved_scope(&policy, &call("host_exec", r#"{"env":{"A":"1"}}"#)),
        "tool:host_exec"
    );

    // Scope labels carry the tool name only; the exact revision is bound into
    // the approval tuple, not the label. Unlike `authorize`, the helpers do
    // not require the call revision to equal the policy revision.
    let newer = call_at("host_write", M0_REVISION + 1, "{}");
    assert_eq!(resolved_scope(&policy, &newer), "tool:host_write");

    // Commands always require confirmation; a resolved label is not a grant.
    assert!(policy.requires_approval(exec.tool()));
    assert!(!policy.requires_approval(&ToolId::new("host_read", M0_REVISION).expect("valid")));
}

#[test]
fn approval_scope_refuses_invalid_path_and_command_arguments() {
    let policy = Policy::m0_test();

    // A present path of the wrong type is refused; it never falls through to
    // the command or the tool fallback.
    for args in [
        r#"{"path":5}"#,
        r#"{"path":null}"#,
        r#"{"path":["src"]}"#,
        r#"{"path":{},"command":"echo hi"}"#,
    ] {
        let error = assert_refused(
            policy.approval_scope(&call("host_write", args)),
            ErrorCategory::PermissionDenied,
            "tool path argument is invalid",
        );
        assert_static(&error, &["src", "echo"]);
    }

    // Path normalization refusals each carry their own static diagnostic.
    for (args, message) in [
        (r#"{"path":""}"#, "tool path is empty"),
        (r#"{"path":"","command":"echo hi"}"#, "tool path is empty"),
        (r#"{"path":"/etc/passwd"}"#, "tool path is absolute"),
        (
            r#"{"path":"~/secrets"}"#,
            "tool path is outside the project scope",
        ),
        (r#"{"path":"a\\b"}"#, "tool path uses backslash separators"),
        (
            r#"{"path":"C:\\Users"}"#,
            "tool path uses backslash separators",
        ),
        (r#"{"path":"a:b"}"#, "tool path contains invalid characters"),
        (r#"{"path":"a<b"}"#, "tool path contains invalid characters"),
        (
            r#"{"path":"a\u0001b"}"#,
            "tool path contains invalid characters",
        ),
        (
            r#"{"path":"a//b"}"#,
            "tool path contains an empty component",
        ),
        (
            r#"{"path":"src/"}"#,
            "tool path contains an empty component",
        ),
        (r#"{"path":".."}"#, "tool path contains a parent traversal"),
        (
            r#"{"path":"../x"}"#,
            "tool path contains a parent traversal",
        ),
        (
            r#"{"path":"a/../b"}"#,
            "tool path contains a parent traversal",
        ),
        (r#"{"path":"./x"}"#, "tool path contains a parent traversal"),
    ] {
        let error = assert_refused(
            policy.approval_scope(&call("host_write", args)),
            ErrorCategory::PermissionDenied,
            message,
        );
        assert_static(&error, &["passwd", "secrets", "Users", "src", "\\"]);
    }

    // Present but malformed or empty command/argv values are explicit
    // refusals; a present-but-empty `command` never falls back to `argv`.
    for args in [
        r#"{"command":""}"#,
        r#"{"command":5}"#,
        r#"{"command":null}"#,
        r#"{"argv":[]}"#,
        r#"{"argv":[1]}"#,
        r#"{"argv":["git",2]}"#,
        r#"{"argv":{"a":"b"}}"#,
        r#"{"command":"","argv":["git","status"]}"#,
    ] {
        assert_refused(
            policy.approval_scope(&call("host_exec", args)),
            ErrorCategory::PermissionDenied,
            "tool command argument is invalid",
        );
    }
}

#[test]
fn approval_scope_refuses_secret_bearing_commands_without_echoing() {
    let policy = Policy::m0_test();

    let secrets = [
        "-----BEGIN PRIVATE KEY-----",
        "Bearer abc123",
        "sk-live-0000",
        "AKIAIOSFODNN7EXAMPLE",
        "ghp_abcdef",
        "xoxb-12345",
        "password=hunter2",
        "passwd=hunter2",
        "secret=value",
        "api_key=value",
        "apikey=value",
        "client_secret=value",
    ];
    for secret in secrets {
        for args in [
            format!(r#"{{"command":"{secret}"}}"#),
            format!(r#"{{"argv":["echo","{secret}"]}}"#),
        ] {
            let error = assert_refused(
                policy.approval_scope(&call("host_exec", &args)),
                ErrorCategory::PermissionDenied,
                SECRET_MESSAGE,
            );
            assert_static(&error, &[secret, "echo"]);
        }
    }

    // Matching is ASCII case-insensitive.
    for secret in [
        "SK-LIVE-0000",
        "BEARER ABC123",
        "Password=Hunter2",
        "CLIENT_SECRET=1",
    ] {
        assert_refused(
            policy.approval_scope(&call("host_exec", &format!(r#"{{"command":"{secret}"}}"#))),
            ErrorCategory::PermissionDenied,
            SECRET_MESSAGE,
        );
    }

    // Near misses that the best-effort marker net does not match stay
    // resolvable and are shown exactly.
    for command in ["echo sk", "bearer", "password", "topsecret", "task list"] {
        let args = format!(r#"{{"command":"{command}"}}"#);
        assert_eq!(
            resolved_scope(&policy, &call("host_exec", &args)),
            format!("command:{command}")
        );
    }
}

#[test]
fn approval_preview_redacts_secret_keys_and_escapes_controls() {
    let policy = Policy::m0_test();

    // Single-member exact shape: both the key and the value are replaced.
    let password = call("host_write", r#"{"password":"hunter2"}"#);
    assert_eq!(
        policy
            .approval_preview(&password)
            .expect("redacted preview builds"),
        format!("host_write {{\"{REDACTED_KEY}\":\"{REDACTED_VALUE}\"}}")
    );

    // Every documented key marker triggers redaction; neither the key name
    // nor the value appears in the preview.
    for key in [
        "password",
        "passwd",
        "secret",
        "token",
        "credential",
        "auth",
        "bearer",
        "cookie",
        "private",
        "apikey",
        "api_key",
        "session",
        "key",
    ] {
        let args = format!(r#"{{"{key}":"sensitive-value"}}"#);
        let preview = policy
            .approval_preview(&call("host_write", &args))
            .expect("redacted preview builds");
        assert!(preview.contains(REDACTED_KEY), "key {key:?}");
        assert!(preview.contains(REDACTED_VALUE), "key {key:?}");
        assert!(
            !preview.contains("sensitive-value"),
            "key {key:?} leaked its value"
        );
        assert!(
            !preview.contains(&format!("\"{key}\"")),
            "key {key:?} leaked its name"
        );
    }

    // Conservative over-redaction: any key containing a marker substring is
    // replaced even when the member is harmless.
    for key in ["monkey", "keynote", "authentication", "tokenizer"] {
        let args = format!(r#"{{"{key}":"banana"}}"#);
        let preview = policy
            .approval_preview(&call("host_write", &args))
            .expect("over-redacted preview builds");
        assert!(
            !preview.contains("banana"),
            "key {key:?} must be over-redacted"
        );
        assert!(preview.contains(REDACTED_VALUE), "key {key:?}");
    }

    // Key matching is ASCII case-insensitive.
    let upper = call("host_write", r#"{"PASSWORD":"hunter2","TOKEN":"abc"}"#);
    let preview = policy
        .approval_preview(&upper)
        .expect("uppercase keys redact");
    assert!(!preview.contains("hunter2"));
    assert!(!preview.contains("abc"));

    // Key redaction runs before the value marker net: a secret-shaped value
    // under a secret key is redacted rather than refused.
    for args in [
        r#"{"token":"sk-live-0000"}"#,
        r#"{"authorization":"Bearer abc123"}"#,
        r#"{"cookie":"session=abc"}"#,
    ] {
        let preview = policy
            .approval_preview(&call("host_write", args))
            .expect("redaction hides the marker");
        assert!(preview.contains(REDACTED_KEY), "args {args}");
        assert!(preview.contains(REDACTED_VALUE), "args {args}");
        assert!(!preview.contains("sk-live-0000"));
        assert!(!preview.contains("abc123"));
    }

    // Nested objects and arrays recurse; values under secret keys are
    // replaced whole.
    let nested = call(
        "host_write",
        r#"{"headers":{"authorization":"Bearer abc123"},"path":"dst"}"#,
    );
    let preview = policy
        .approval_preview(&nested)
        .expect("nested preview builds");
    assert!(preview.contains("\"path\":\"dst\""));
    assert!(preview.contains("\"headers\""));
    assert!(preview.contains(REDACTED_VALUE));
    assert!(!preview.contains("abc123"));
    assert!(!preview.contains("authorization"));

    let array = call(
        "host_write",
        r#"{"items":[{"password":"hunter2"},{"path":"ok"}]}"#,
    );
    let preview = policy
        .approval_preview(&array)
        .expect("array preview builds");
    assert!(!preview.contains("hunter2"));
    assert!(preview.contains("\"path\":\"ok\""));
    assert!(preview.contains(REDACTED_VALUE));

    let whole_object = call("host_write", r#"{"token":{"nested":"hunter2"}}"#);
    let preview = policy
        .approval_preview(&whole_object)
        .expect("whole-object redaction builds");
    assert!(!preview.contains("hunter2"));
    assert!(preview.contains(REDACTED_VALUE));

    // Compact JSON: insignificant input whitespace does not reach the preview.
    let spaced = call("host_write", r#"{ "path" : "dst" }"#);
    assert_eq!(
        policy
            .approval_preview(&spaced)
            .expect("compact preview builds"),
        "host_write {\"path\":\"dst\"}"
    );

    // Control characters (including NUL, DEL, and C1 controls) are escaped,
    // so the preview never carries a raw control byte.
    let control = call("host_write", r#"{"note":"x\u0000y\u007fz\u0085w"}"#);
    let preview = policy
        .approval_preview(&control)
        .expect("escaped preview builds");
    for escape in ["\\u0000", "\\u007f", "\\u0085"] {
        assert!(preview.contains(escape), "missing escape {escape:?}");
    }
    assert!(!preview.chars().any(char::is_control));
    assert!(preview.starts_with("host_write "));
}

#[test]
fn approval_preview_refuses_secret_value_markers_without_echoing_them() {
    let policy = Policy::m0_test();

    let markers = [
        "-----begin",
        "bearer ",
        "sk-",
        "akia",
        "ghp_",
        "xoxb-",
        "password=",
        "passwd=",
        "secret=",
        "api_key=",
        "apikey=",
        "client_secret",
    ];
    for marker in markers {
        for args in [
            format!(r#"{{"note":"{marker}"}}"#),
            format!(r#"{{"outer":{{"inner":"{marker}"}}}}"#),
        ] {
            let error = assert_refused(
                policy.approval_preview(&call("host_write", &args)),
                ErrorCategory::PermissionDenied,
                SECRET_MESSAGE,
            );
            assert_static(&error, &[marker]);
        }
    }

    // Matching is ASCII case-insensitive.
    for value in [
        "SK-LIVE-0000",
        "BEARER ABC",
        "PASSWORD=HUNTER2",
        "Client_Secret=x",
    ] {
        assert_refused(
            policy.approval_preview(&call("host_write", &format!(r#"{{"note":"{value}"}}"#))),
            ErrorCategory::PermissionDenied,
            SECRET_MESSAGE,
        );
    }

    // Near misses that the best-effort marker net does not match still build.
    for value in [
        "sk",
        "bearer",
        "password",
        "topsecret",
        "ghp",
        "xoxb",
        "api_key",
    ] {
        let preview = policy
            .approval_preview(&call("host_write", &format!(r#"{{"note":"{value}"}}"#)))
            .expect("near-miss value builds");
        assert!(preview.contains(value), "value {value:?}");
    }
}

#[test]
fn scope_and_preview_bounds_are_exact_and_over_bound_is_refused() {
    let policy = Policy::m0_test();

    // Command scope boundary.
    let prefix = "command:";
    let exact = "q".repeat(MAX_SCOPE_BYTES - prefix.len());
    let accepted = call("host_exec", &format!(r#"{{"argv":"{exact}"}}"#));
    let scope = policy
        .approval_scope(&accepted)
        .expect("exact command bound accepted");
    assert_eq!(scope.as_str().len(), MAX_SCOPE_BYTES);
    assert_eq!(scope.as_str(), format!("{prefix}{exact}"));

    let over = "q".repeat(MAX_SCOPE_BYTES - prefix.len() + 1);
    let error = assert_refused(
        policy.approval_scope(&call("host_exec", &format!(r#"{{"argv":"{over}"}}"#))),
        ErrorCategory::ResourceLimit,
        SCOPE_BOUND_MESSAGE,
    );
    assert_static(&error, &["q"]);

    // Path scope boundary shares the same static diagnostic.
    let prefix = "path:";
    let exact = "q".repeat(MAX_SCOPE_BYTES - prefix.len());
    let scope = policy
        .approval_scope(&call("host_write", &format!(r#"{{"path":"{exact}"}}"#)))
        .expect("exact path bound accepted");
    assert_eq!(scope.as_str().len(), MAX_SCOPE_BYTES);
    let over = "q".repeat(MAX_SCOPE_BYTES - prefix.len() + 1);
    let error = assert_refused(
        policy.approval_scope(&call("host_write", &format!(r#"{{"path":"{over}"}}"#))),
        ErrorCategory::ResourceLimit,
        SCOPE_BOUND_MESSAGE,
    );
    assert_static(&error, &["q"]);

    // The scope bound counts bytes, not characters.
    let multibyte = "é".repeat((MAX_SCOPE_BYTES - prefix.len()) / 2);
    let scope = policy
        .approval_scope(&call("host_write", &format!(r#"{{"path":"{multibyte}"}}"#)))
        .expect("multibyte path under the byte bound accepted");
    assert!(scope.as_str().len() <= MAX_SCOPE_BYTES);
    let over = "é".repeat((MAX_SCOPE_BYTES - prefix.len()) / 2 + 1);
    assert_refused(
        policy.approval_scope(&call("host_write", &format!(r#"{{"path":"{over}"}}"#))),
        ErrorCategory::ResourceLimit,
        SCOPE_BOUND_MESSAGE,
    );

    // Preview boundary: "host_exec " + `{"argv":"<n>"}`.
    let fixed = "host_exec ".len() + r#"{"argv":""}"#.len();
    let exact_n = MAX_SUMMARY_BYTES - fixed;
    let exact_args = format!(r#"{{"argv":"{}"}}"#, "q".repeat(exact_n));
    let preview = policy
        .approval_preview(&call("host_exec", &exact_args))
        .expect("exact preview bound accepted");
    assert_eq!(preview.len(), MAX_SUMMARY_BYTES);
    assert_eq!(
        preview,
        format!("host_exec {{\"argv\":\"{}\"}}", "q".repeat(exact_n))
    );

    let over_args = format!(r#"{{"argv":"{}"}}"#, "q".repeat(exact_n + 1));
    let error = assert_refused(
        policy.approval_preview(&call("host_exec", &over_args)),
        ErrorCategory::ResourceLimit,
        PREVIEW_BOUND_MESSAGE,
    );
    assert_static(&error, &["q"]);

    // Preview boundary for the path shape.
    let fixed = "host_write ".len() + r#"{"path":""}"#.len();
    let exact_n = MAX_SUMMARY_BYTES - fixed;
    let exact_args = format!(r#"{{"path":"{}"}}"#, "q".repeat(exact_n));
    assert_eq!(
        policy
            .approval_preview(&call("host_write", &exact_args))
            .expect("exact preview bound accepted")
            .len(),
        MAX_SUMMARY_BYTES
    );
    let over_args = format!(r#"{{"path":"{}"}}"#, "q".repeat(exact_n + 1));
    assert_refused(
        policy.approval_preview(&call("host_write", &over_args)),
        ErrorCategory::ResourceLimit,
        PREVIEW_BOUND_MESSAGE,
    );

    // The preview bound counts bytes too.
    let fixed = "host_write ".len() + r#"{"path":""}"#.len();
    let multibyte_n = (MAX_SUMMARY_BYTES - fixed) / 2;
    let exact_args = format!(r#"{{"path":"{}"}}"#, "é".repeat(multibyte_n));
    let preview = policy
        .approval_preview(&call("host_write", &exact_args))
        .expect("multibyte preview under the byte bound accepted");
    assert!(preview.len() <= MAX_SUMMARY_BYTES);
    let over_args = format!(r#"{{"path":"{}"}}"#, "é".repeat(multibyte_n + 1));
    assert_refused(
        policy.approval_preview(&call("host_write", &over_args)),
        ErrorCategory::ResourceLimit,
        PREVIEW_BOUND_MESSAGE,
    );

    // Two actions sharing their whole visible prefix and differing only past
    // the bound produce byte-identical refusals: a truncating implementation
    // would have shown the same label for both and made them
    // indistinguishable.
    let shared = "q".repeat(MAX_SCOPE_BYTES);
    let first = assert_refused(
        policy.approval_scope(&call("host_exec", &format!(r#"{{"argv":"{shared}A"}}"#))),
        ErrorCategory::ResourceLimit,
        SCOPE_BOUND_MESSAGE,
    );
    let second = assert_refused(
        policy.approval_scope(&call("host_exec", &format!(r#"{{"argv":"{shared}B"}}"#))),
        ErrorCategory::ResourceLimit,
        SCOPE_BOUND_MESSAGE,
    );
    assert_eq!(first, second);

    let shared = "q".repeat(MAX_SUMMARY_BYTES);
    let first = assert_refused(
        policy.approval_preview(&call(
            "host_write",
            &format!(r#"{{"content":"{shared}A"}}"#),
        )),
        ErrorCategory::ResourceLimit,
        PREVIEW_BOUND_MESSAGE,
    );
    let second = assert_refused(
        policy.approval_preview(&call(
            "host_write",
            &format!(r#"{{"content":"{shared}B"}}"#),
        )),
        ErrorCategory::ResourceLimit,
        PREVIEW_BOUND_MESSAGE,
    );
    assert_eq!(first, second);

    // Argument text at the whole assembly budget is still refused by the much
    // smaller preview bound, never truncated.
    let filler = "a".repeat(Limits::M0_TEST_ARG_ASSEMBLY_BYTES - r#"{"a":""}"#.len());
    let args = format!(r#"{{"a":"{filler}"}}"#);
    assert_eq!(args.len(), Limits::M0_TEST_ARG_ASSEMBLY_BYTES);
    assert_refused(
        policy.approval_preview(&call("host_write", &args)),
        ErrorCategory::ResourceLimit,
        PREVIEW_BOUND_MESSAGE,
    );
}

#[test]
fn duplicate_path_keys_are_rejected_never_last_wins() {
    let policy = Policy::m0_test();

    // Both orderings: the out-of-scope value first and last. A last-key-wins
    // parser would authorize `src` in one case and `/etc/passwd` in the other;
    // a first-key-wins parser would do the reverse. Both are refused.
    for args in [
        r#"{"path":"/etc/passwd","path":"src"}"#,
        r#"{"path":"src","path":"/etc/passwd"}"#,
    ] {
        let error = assert_refused(
            policy.authorize(&call("host_read", args)),
            ErrorCategory::InvalidInput,
            DUPLICATE_MESSAGE,
        );
        assert_static(&error, &["path", "src", "etc", "passwd"]);

        assert_refused(
            policy.approval_scope(&call("host_write", args)),
            ErrorCategory::InvalidInput,
            DUPLICATE_MESSAGE,
        );
        assert_refused(
            policy.approval_preview(&call("host_write", args)),
            ErrorCategory::InvalidInput,
            DUPLICATE_MESSAGE,
        );
    }

    // Duplicate detection is global, not path-specific: nested objects and
    // command/argv keys are rejected the same way.
    for args in [
        r#"{"content":{"a":1,"a":2}}"#,
        r#"{"command":"echo safe","command":"echo other"}"#,
        r#"{"command":"sk-live-0000","command":"echo safe"}"#,
        r#"{"argv":["git"],"argv":["ls"]}"#,
    ] {
        assert_refused(
            policy.approval_scope(&call("host_write", args)),
            ErrorCategory::InvalidInput,
            DUPLICATE_MESSAGE,
        );
        assert_refused(
            policy.approval_preview(&call("host_write", args)),
            ErrorCategory::InvalidInput,
            DUPLICATE_MESSAGE,
        );
    }

    // A duplicate key is ambiguous before any secret check: the secret-shaped
    // duplicate is rejected as a duplicate, not as secret text.
    assert_refused(
        policy.approval_scope(&call(
            "host_exec",
            r#"{"command":"sk-live-0000","command":"echo safe"}"#,
        )),
        ErrorCategory::InvalidInput,
        DUPLICATE_MESSAGE,
    );
}

#[test]
fn ambiguous_and_oversized_json_is_rejected_before_any_decision() {
    let policy = Policy::m0_test();

    // Malformed JSON is rejected as invalid input for authorize, scope, and
    // preview alike; it never reaches a fallback label.
    for args in [
        r#"{oops}"#,
        r#"{"path":}"#,
        r#"{"path":"src",}"#,
        r#"{"a":1} {"b":2}"#,
    ] {
        let error = assert_refused(
            policy.authorize(&call("host_read", args)),
            ErrorCategory::InvalidInput,
            MALFORMED_MESSAGE,
        );
        assert_static(&error, &["src", "oops", "path"]);
        assert_refused(
            policy.approval_scope(&call("host_write", args)),
            ErrorCategory::InvalidInput,
            MALFORMED_MESSAGE,
        );
        assert_refused(
            policy.approval_preview(&call("host_write", args)),
            ErrorCategory::InvalidInput,
            MALFORMED_MESSAGE,
        );
    }

    // Depth: exactly the budget parses and resolves to the tool fallback; one
    // level deeper is a resource-limit refusal for every entry point.
    let exact_nesting = format!(
        "{}{}",
        "[".repeat(MAX_ARGS_DEPTH),
        "]".repeat(MAX_ARGS_DEPTH)
    );
    let exact_deep = format!(r#"{{"a":{}}}"#, exact_nesting);
    let exact = call("host_write", &exact_deep);
    assert_eq!(resolved_scope(&policy, &exact), "tool:host_write");
    assert!(policy.approval_preview(&exact).is_ok());

    let over_nesting = format!(
        "{}{}",
        "[".repeat(MAX_ARGS_DEPTH + 1),
        "]".repeat(MAX_ARGS_DEPTH + 1)
    );
    let over_deep = format!(r#"{{"a":{}}}"#, over_nesting);
    let error = assert_refused(
        policy.authorize(&call("host_read", &over_deep)),
        ErrorCategory::ResourceLimit,
        DEPTH_MESSAGE,
    );
    assert_static(&error, &["[", "]"]);
    assert_refused(
        policy.approval_scope(&call("host_write", &over_deep)),
        ErrorCategory::ResourceLimit,
        DEPTH_MESSAGE,
    );
    assert_refused(
        policy.approval_preview(&call("host_write", &over_deep)),
        ErrorCategory::ResourceLimit,
        DEPTH_MESSAGE,
    );

    // Node budget: the root object and the array count as two nodes, so an
    // array of exactly MAX_ARGS_NODES - 2 items parses; one more item is a
    // resource-limit refusal. The accepted text already exceeds the preview
    // bound, which refuses it separately, proving parse and bound are
    // distinct gates.
    let exact_nodes = format!(r#"{{"a":[{}]}}"#, vec!["0"; MAX_ARGS_NODES - 2].join(","));
    let exact = call("host_write", &exact_nodes);
    assert_eq!(resolved_scope(&policy, &exact), "tool:host_write");
    assert_refused(
        policy.approval_preview(&exact),
        ErrorCategory::ResourceLimit,
        PREVIEW_BOUND_MESSAGE,
    );

    let over_nodes = format!(r#"{{"a":[{}]}}"#, vec!["0"; MAX_ARGS_NODES - 1].join(","));
    let error = assert_refused(
        policy.authorize(&call("host_read", &over_nodes)),
        ErrorCategory::ResourceLimit,
        NODE_MESSAGE,
    );
    assert_static(&error, &["0"]);
    assert_refused(
        policy.approval_scope(&call("host_write", &over_nodes)),
        ErrorCategory::ResourceLimit,
        NODE_MESSAGE,
    );
    assert_refused(
        policy.approval_preview(&call("host_write", &over_nodes)),
        ErrorCategory::ResourceLimit,
        NODE_MESSAGE,
    );
}

#[test]
fn approval_helpers_validate_the_policy_before_inspecting_arguments() {
    let invalid_auto = Policy::new(vec!["host_write".to_owned()], M0_REVISION);
    let wrong_revision = Policy::new(Vec::new(), M0_REVISION + 1);

    let valid = call("host_write", r#"{"path":"dst"}"#);
    let malformed = call("host_write", r#"{oops}"#);
    let duplicate = call("host_write", r#"{"path":"a","path":"b"}"#);

    assert_refused(
        invalid_auto.approval_scope(&valid),
        ErrorCategory::InvalidInput,
        "policy auto-approved tool set is invalid",
    );
    assert_refused(
        invalid_auto.approval_preview(&valid),
        ErrorCategory::InvalidInput,
        "policy auto-approved tool set is invalid",
    );
    assert_refused(
        wrong_revision.approval_scope(&valid),
        ErrorCategory::InvalidInput,
        "policy revision is not the M0 revision",
    );
    assert_refused(
        wrong_revision.approval_preview(&valid),
        ErrorCategory::InvalidInput,
        "policy revision is not the M0 revision",
    );

    // Policy validation precedes argument parsing: even malformed or
    // ambiguous arguments report the policy diagnostic, so an invalid policy
    // can never be probed through argument differences.
    assert_refused(
        invalid_auto.approval_scope(&malformed),
        ErrorCategory::InvalidInput,
        "policy auto-approved tool set is invalid",
    );
    assert_refused(
        invalid_auto.approval_preview(&duplicate),
        ErrorCategory::InvalidInput,
        "policy auto-approved tool set is invalid",
    );
    assert_refused(
        wrong_revision.approval_scope(&duplicate),
        ErrorCategory::InvalidInput,
        "policy revision is not the M0 revision",
    );

    Policy::m0_test()
        .validate()
        .expect("the M0-test policy validates");
    assert!(
        Policy::try_new(
            vec!["host_read".to_owned(), "host_search".to_owned()],
            M0_REVISION
        )
        .is_ok()
    );
    assert!(Policy::try_new(vec!["host_write".to_owned()], M0_REVISION).is_err());
}
