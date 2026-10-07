#![forbid(unsafe_code)]

//! Coverage hardening for the command/reply boundary in `nexus-core`.
//!
//! These tests pin the public carrier contract the runtime adapter relies
//! on: every `Command` variant exposes its correlation identity, every
//! `CommandReply` is distinct and round-trips through `CommandResponse`,
//! and a response never rewrites the request, reply, or optional run it
//! was built with. Public API only; no runtime, clock, randomness, or
//! filesystem access, so the checks stay deterministic.

use nexus_core::{
    ApprovalId, ApproveCommand, CallId, CancelCommand, Command, CommandReply, CommandResponse,
    DenyCommand, GetSnapshotCommand, ListSessionsCommand, RequestId, RestoreSessionCommand, RunId,
    SessionId, SubmitCommand,
};

/// The complete reply taxonomy, kept explicit so the tests enumerate every
/// variant rather than sampling it.
const ALL_REPLIES: [CommandReply; 5] = [
    CommandReply::Accepted,
    CommandReply::Busy,
    CommandReply::Rejected,
    CommandReply::AlreadyFinalized,
    CommandReply::StaleOrUnknownTarget,
];

fn request(raw: &str) -> RequestId {
    RequestId::new(raw).expect("test request id is valid")
}

fn run(raw: &str) -> RunId {
    RunId::new(raw).expect("test run id is valid")
}

fn session(raw: &str) -> SessionId {
    SessionId::new(raw).expect("test session id is valid")
}

/// Compile-time exhaustiveness guard: adding a `CommandReply` variant
/// breaks this match (there is no wildcard), forcing `ALL_REPLIES` and the
/// taxonomy test to be updated.
fn reply_label(reply: CommandReply) -> &'static str {
    match reply {
        CommandReply::Accepted => "accepted",
        CommandReply::Busy => "busy",
        CommandReply::Rejected => "rejected",
        CommandReply::AlreadyFinalized => "already-finalized",
        CommandReply::StaleOrUnknownTarget => "stale-or-unknown-target",
    }
}

/// Compile-time exhaustiveness guard for `Command`: a new variant must be
/// added here and to `distinct_commands`.
fn command_kind(command: &Command) -> &'static str {
    match command {
        Command::Submit(_) => "submit",
        Command::Cancel(_) => "cancel",
        Command::Approve(_) => "approve",
        Command::ApproveSessionDirectory(_) => "approve-session-directory",
        Command::Deny(_) => "deny",
        Command::GetSnapshot(_) => "get-snapshot",
        Command::ListSessions(_) => "list-sessions",
        Command::RestoreSession(_) => "restore-session",
    }
}

/// Builds one command of every variant, each correlated to a distinct
/// request id, with the variant's human label for assertion messages.
fn distinct_commands() -> Vec<(&'static str, &'static str, Command)> {
    let session = session("sess-1");
    let run = run("run-1");
    let approval = ApprovalId::new("appr-1").expect("valid approval id");
    let call = CallId::new("call-1").expect("valid call id");
    vec![
        (
            "req-submit",
            "submit",
            Command::Submit(
                SubmitCommand::new(request("req-submit"), session.clone(), "do work", "m0-test")
                    .expect("valid submit builds"),
            ),
        ),
        (
            "req-cancel",
            "cancel",
            Command::Cancel(CancelCommand {
                request: request("req-cancel"),
                run: run.clone(),
            }),
        ),
        (
            "req-approve",
            "approve",
            Command::Approve(ApproveCommand {
                request: request("req-approve"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            }),
        ),
        (
            "req-approve-directory",
            "approve-session-directory",
            Command::ApproveSessionDirectory(ApproveCommand {
                request: request("req-approve-directory"),
                approval: approval.clone(),
                run: run.clone(),
                call: call.clone(),
            }),
        ),
        (
            "req-deny",
            "deny",
            Command::Deny(DenyCommand {
                request: request("req-deny"),
                approval,
                run: run.clone(),
                call,
            }),
        ),
        (
            "req-snapshot",
            "get-snapshot",
            Command::GetSnapshot(GetSnapshotCommand {
                request: request("req-snapshot"),
                run,
            }),
        ),
        (
            "req-list",
            "list-sessions",
            Command::ListSessions(
                ListSessionsCommand::new(request("req-list"), 16).expect("valid list builds"),
            ),
        ),
        (
            "req-restore",
            "restore-session",
            Command::RestoreSession(RestoreSessionCommand {
                request: request("req-restore"),
                session,
            }),
        ),
    ]
}

/// Rewrites the public request field of any command variant; used to prove
/// the accessor reads the live field rather than a construction snapshot.
fn set_request(command: &mut Command, replacement: RequestId) {
    match command {
        Command::Submit(command) => command.request = replacement,
        Command::Cancel(command) => command.request = replacement,
        Command::Approve(command) => command.request = replacement,
        Command::ApproveSessionDirectory(command) => command.request = replacement,
        Command::Deny(command) => command.request = replacement,
        Command::GetSnapshot(command) => command.request = replacement,
        Command::ListSessions(command) => command.request = replacement,
        Command::RestoreSession(command) => command.request = replacement,
    }
}

#[test]
fn reply_taxonomy_is_exhaustive_and_pairwise_distinct() {
    assert_eq!(
        ALL_REPLIES.len(),
        5,
        "the taxonomy has exactly five replies"
    );
    let labels: Vec<&str> = ALL_REPLIES.iter().copied().map(reply_label).collect();
    for (index, reply) in ALL_REPLIES.iter().enumerate() {
        let copied = *reply;
        assert_eq!(copied, *reply, "CommandReply is Copy and value-stable");
        for other in &ALL_REPLIES[index + 1..] {
            assert_ne!(reply, other, "reply variants never alias");
        }
    }
    for (index, label) in labels.iter().enumerate() {
        assert!(
            !labels[index + 1..].contains(label),
            "reply label {label:?} is unique"
        );
    }
}

#[test]
fn every_command_variant_exposes_its_own_request() {
    let cases = distinct_commands();
    assert_eq!(cases.len(), 8, "one case per Command variant");
    let mut seen = Vec::new();
    for (raw, kind, command) in &cases {
        assert_eq!(
            command.request().as_str(),
            *raw,
            "accessor returns the command's own request"
        );
        assert_eq!(
            command.request(),
            &request(raw),
            "correlation compares by value"
        );
        assert_eq!(
            command_kind(command),
            *kind,
            "kind label matches the variant"
        );
        assert!(!seen.contains(raw), "each case uses a distinct request id");
        seen.push(*raw);
        let cloned = command.clone();
        assert_eq!(&cloned, command, "cloning preserves command identity");
        assert_eq!(
            cloned.request(),
            command.request(),
            "the clone carries the same request"
        );
    }
    // Request identity participates in command equality: two otherwise
    // identical submits with different requests are not equal.
    let left = Command::Submit(
        SubmitCommand::new(request("req-left"), session("sess-1"), "do work", "m0-test")
            .expect("valid submit builds"),
    );
    let right = Command::Submit(
        SubmitCommand::new(
            request("req-right"),
            session("sess-1"),
            "do work",
            "m0-test",
        )
        .expect("valid submit builds"),
    );
    assert_ne!(left, right, "different requests make different commands");
}

#[test]
fn response_round_trips_request_reply_and_run_for_every_command() {
    let active = run("run-active");
    let run_options = [None, Some(active.clone())];
    for (raw, kind, command) in &distinct_commands() {
        for reply in ALL_REPLIES {
            for run_option in &run_options {
                let response =
                    CommandResponse::new(command.request().clone(), reply, run_option.clone());
                assert_eq!(
                    response.request(),
                    command.request(),
                    "{kind} response carries the command request"
                );
                assert_eq!(
                    response.request().as_str(),
                    *raw,
                    "{kind} response keeps the raw id"
                );
                assert_eq!(response.reply(), reply, "{kind} response keeps the reply");
                assert_eq!(
                    response.run(),
                    run_option.as_ref(),
                    "{kind} response keeps the run carriage"
                );
                assert_eq!(response.clone(), response, "correlation survives cloning");
            }
        }
    }
}

/// The runtime maps command outcomes onto these carriers; pin the exact
/// shapes so no path silently fabricates or drops a run.
#[test]
fn run_carriage_distinguishes_accepted_busy_finalized_and_stale() {
    let request = request("req-1");
    let target = run("run-1");
    let accepted = CommandResponse::new(
        request.clone(),
        CommandReply::Accepted,
        Some(target.clone()),
    );
    assert_eq!(
        accepted.run(),
        Some(&target),
        "accepted names the issued run"
    );
    let busy = CommandResponse::new(request.clone(), CommandReply::Busy, Some(target.clone()));
    assert_eq!(
        busy.run(),
        Some(&target),
        "busy names the colliding run when known"
    );
    let busy_unknown = CommandResponse::new(request.clone(), CommandReply::Busy, None);
    assert_eq!(busy_unknown.run(), None, "busy never fabricates a run");
    let finalized = CommandResponse::new(
        request.clone(),
        CommandReply::AlreadyFinalized,
        Some(target.clone()),
    );
    assert_eq!(
        finalized.run(),
        Some(&target),
        "finalized names the terminal run"
    );
    let rejected = CommandResponse::new(request.clone(), CommandReply::Rejected, None);
    assert_eq!(rejected.run(), None, "rejection carries no run");
    let stale_unknown =
        CommandResponse::new(request.clone(), CommandReply::StaleOrUnknownTarget, None);
    assert_eq!(stale_unknown.run(), None, "unknown target carries no run");
    let stale_known = CommandResponse::new(
        request,
        CommandReply::StaleOrUnknownTarget,
        Some(target.clone()),
    );
    assert_eq!(
        stale_known.run(),
        Some(&target),
        "stale grant on a known run still names it"
    );
}

#[test]
fn request_identity_is_the_correlation_key() {
    let first = request("req-1");
    let second = request("req-2");
    let target = run("run-1");
    let base = CommandResponse::new(first.clone(), CommandReply::Accepted, Some(target.clone()));
    let same = CommandResponse::new(request("req-1"), CommandReply::Accepted, Some(run("run-1")));
    assert_eq!(base, same, "value-equal correlation triples compare equal");
    assert_ne!(
        base,
        CommandResponse::new(second.clone(), CommandReply::Accepted, Some(target.clone())),
        "request identity distinguishes responses"
    );
    assert_ne!(
        base,
        CommandResponse::new(first.clone(), CommandReply::Busy, Some(target.clone())),
        "reply distinguishes responses"
    );
    assert_ne!(
        base,
        CommandResponse::new(first.clone(), CommandReply::Accepted, None),
        "run carriage distinguishes responses"
    );
    assert_eq!(base.request(), &first);
    assert_ne!(base.request(), &second);
    assert_eq!(base.request().as_str(), "req-1");
}

#[test]
fn accessor_follows_the_public_request_field_for_every_command() {
    let replacement = request("req-replaced");
    for (_, kind, mut command) in distinct_commands() {
        set_request(&mut command, replacement.clone());
        assert_eq!(
            command.request(),
            &replacement,
            "{kind} accessor reads the live public field"
        );
        let response =
            CommandResponse::new(command.request().clone(), CommandReply::Accepted, None);
        assert_eq!(response.request(), &replacement);
    }
}
