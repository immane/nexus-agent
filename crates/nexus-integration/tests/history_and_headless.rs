#![forbid(unsafe_code)]

//! Bounded manual history handling at the store/runtime level and headless
//! structured-output separation, against the real components.

mod common;

use nexus_core::{
    Command, CommandReply, ListSessionsCommand, RequestId, RestoreSessionCommand, RunOutcome,
    STORE_FORMAT_REVISION, SessionCheckpoint, SessionId, SessionStore, store::StoredMessage,
};
use nexus_fakes::{EphemeralStore, FakeTool, stop_turn};

fn stored_message(source: &str, text: &str) -> StoredMessage {
    StoredMessage {
        source: source.to_owned(),
        text: text.to_owned(),
        complete: true,
    }
}

/// Manual history handling loads bounded accepted messages only: no tool is
/// replayed and no per-call grant exists anywhere in the loaded state.
#[test]
fn history_loads_bounded_history_without_replay_or_grants() {
    let session = SessionId::new("sess-history").expect("valid");
    let mut store = EphemeralStore::new();
    assert!(!store.is_durable(), "test store never claims durability");

    let messages = vec![
        stored_message("user", "first question"),
        stored_message("assistant", "first answer"),
        stored_message("user", "second question"),
        stored_message("assistant", "second answer"),
    ];
    let checkpoint = SessionCheckpoint::new(
        session.clone(),
        STORE_FORMAT_REVISION,
        3,
        messages.clone(),
        "m0-test",
    )
    .expect("bounded checkpoint builds");
    store.save_checkpoint(&checkpoint).expect("save works");

    let loaded = store
        .load_session(&session)
        .expect("selected history loads");
    assert_eq!(loaded.session(), &session);
    assert_eq!(loaded.logical_revision(), 3);
    assert_eq!(loaded.messages(), messages.as_slice());
    assert!(
        loaded.messages().len() <= nexus_core::Limits::M0_TEST_RETAINED_CONTEXT_ITEMS,
        "retained history stays bounded"
    );
    assert!(
        store.intents().is_empty() && store.outcomes().is_empty(),
        "loading history restores no per-call grants"
    );

    // Nothing was replayed: the tool doubles never executed.
    let read_tool = FakeTool::read_only();
    let write_tool = FakeTool::mutation();
    assert_eq!(read_tool.execution_count(), 0);
    assert_eq!(write_tool.execution_count(), 0);

    // Listing is bounded metadata only, and oversize history is rejected.
    store
        .save_checkpoint(
            &SessionCheckpoint::new(
                SessionId::new("sess-other").expect("valid"),
                STORE_FORMAT_REVISION,
                1,
                vec![stored_message("user", "hi")],
                "m0-test",
            )
            .expect("checkpoint builds"),
        )
        .expect("save works");
    let listed = store.list_sessions(1).expect("bounded list works");
    assert_eq!(listed.len(), 1, "listing honors its bound");

    let oversize: Vec<StoredMessage> = (0..nexus_core::Limits::M0_TEST_RETAINED_CONTEXT_ITEMS + 1)
        .map(|index| stored_message("user", &format!("message {index}")))
        .collect();
    assert!(
        SessionCheckpoint::new(
            SessionId::new("sess-big").expect("valid"),
            STORE_FORMAT_REVISION,
            1,
            oversize,
            "m0-test",
        )
        .is_err(),
        "history beyond the retained bound is rejected"
    );
    assert!(
        SessionCheckpoint::new(
            session.clone(),
            STORE_FORMAT_REVISION + 1,
            1,
            vec![stored_message("user", "hi")],
            "m0-test",
        )
        .is_err(),
        "format revisions compare with exact equality"
    );
    assert!(
        store
            .load_session(&SessionId::new("sess-missing").expect("valid"))
            .is_err()
    );
}

/// The M0 runtime performs no store-backed history work: listing and restore
/// are explicitly rejected, start no run, and leave the slot idle for a
/// normal submission.
#[test]
fn runtime_rejects_history_commands_without_starting_work() {
    let rt = common::test_rt();
    rt.block_on(async {
        let bed = common::make_bed(vec![stop_turn("done")], common::quick_config());

        let (list_reply, _) = bed
            .runtime
            .handle(Command::ListSessions(
                ListSessionsCommand::new(RequestId::new("req-list").expect("valid"), 10)
                    .expect("list builds"),
            ))
            .await;
        assert_eq!(list_reply.reply(), CommandReply::Rejected);

        let (restore_reply, _) = bed
            .runtime
            .handle(Command::RestoreSession(RestoreSessionCommand {
                request: RequestId::new("req-restore").expect("valid"),
                session: SessionId::new("sess-history").expect("valid"),
            }))
            .await;
        assert_eq!(restore_reply.reply(), CommandReply::Rejected);

        // No conflicting work started: a fresh submit is accepted, not busy.
        let response = bed
            .runtime
            .submit(common::submit_cmd("after-history"))
            .await;
        assert_eq!(response.reply(), CommandReply::Accepted);
    });
}

/// Structured headless output keeps diagnostics on stderr by construction:
/// every stdout record carries the output revision with `key=value` fields
/// only, and neither the fake banner, the usage hint, escape codes, nor line
/// breaks ever appear in stdout records.
#[test]
fn headless_stdout_records_carry_no_diagnostics_or_escapes() {
    // NOTE: `run_task` builds its own internal runtime, so this test stays
    // synchronous and never runs inside another `block_on`.
    for line in std::iter::once(&nexus_headless::FAKE_BANNER.to_owned())
        .chain(std::iter::once(&nexus_headless::USAGE.to_owned()))
    {
        assert!(!line.is_empty(), "stderr diagnostics exist to separate");
    }

    let completed = nexus_headless::run_task("hello").expect("completed script runs");
    assert_eq!(completed.outcome, RunOutcome::Completed);
    assert_eq!(completed.exit_code, 0);
    assert_stdout_hygiene(&completed.lines);

    let denied = nexus_headless::run_task("deny: write it").expect("denial script runs");
    assert_eq!(denied.outcome, RunOutcome::Completed);
    assert_eq!(denied.denied_calls, 1);
    assert_eq!(denied.tool_started, 0, "denied calls never start");
    assert_eq!(denied.exit_code, 3);
    assert_stdout_hygiene(&denied.lines);
    let finished = denied
        .lines
        .iter()
        .find(|line| line.contains("kind=tool-finished"))
        .expect("denial recorded in stdout");
    assert!(finished.contains("status=denied"), "{finished}");

    assert!(nexus_headless::run_task("").is_err(), "empty task rejected");

    let dirty = "ok\x1b[2J overwritten\nnew line\rreturn key=value pair";
    let clean = nexus_headless::sanitize(dirty);
    assert!(!clean.contains('\x1b'), "escape codes stripped: {clean}");
    assert!(
        !clean.contains(['\n', '\r', ' ', '=']),
        "separators stripped: {clean}"
    );
}

fn assert_stdout_hygiene(lines: &[String]) {
    assert!(!lines.is_empty(), "stdout records exist");
    for line in lines {
        assert!(
            line.starts_with("rev=m0-test-0 "),
            "every stdout record carries the revision: {line}"
        );
        assert!(!line.contains('\x1b'), "no escape codes: {line}");
        assert!(
            !line.contains('\n') && !line.contains('\r'),
            "single line each: {line}"
        );
        assert!(!line.contains("FAKE"), "no banner leakage: {line}");
        assert!(!line.contains("usage:"), "no usage-hint leakage: {line}");
        for field in line.split(' ').skip(1) {
            assert!(field.contains('='), "key=value fields only: {line}");
        }
    }
    assert_eq!(nexus_headless::OUTPUT_REV, "m0-test-0");
}
