#![forbid(unsafe_code)]

//! End-to-end API flows over the real loopback HTTP transport: a refused
//! grant never dispatches, a cancel during the approval wait settles the live
//! stream as `cancelled`, the snapshot's bounded content tracks the live run
//! (pending grants, known outcomes, terminal sequence), a live run slot and a
//! settled run answer conflicting commands, a session owns one event stream,
//! and rejected submit input never mints a run or echoes its body.
//!
//! Each test drives one real run over a real socket, so the only ordering it
//! relies on is published by the runtime before the corresponding wire event:
//! a pending grant is inserted before `approval-required` is emitted, a
//! decided grant leaves the pending set before the approve reply is sent, and
//! the settled run is recorded before the terminal event reaches a subscriber.
//! Every socket carries a bounded read timeout, so a stalled or closed run
//! fails the test instead of hanging.

use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

/// Read timeout for one framed request/response round trip. Backstop only:
/// no request here waits on work the runtime is not already driving.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Read timeout for the event stream. It matches the server's idle window, so
/// a `: ping` comment can never be mistaken for progress.
const STREAM_TIMEOUT: Duration = Duration::from_secs(15);
/// Consecutive stream read timeouts tolerated before a wait is stalled.
const STREAM_LIMIT: usize = 3;

/// Loopback address of the test server.
fn address(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Sends one raw request and reads the framed response (the server always
/// closes outside SSE). Returns the status plus the raw body.
fn round_trip(port: u16, raw: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(address(port)).expect("loopback connects");
    stream
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .expect("timeout sets");
    stream.write_all(raw.as_bytes()).expect("request writes");
    let mut body = Vec::new();
    stream.read_to_end(&mut body).expect("response reads");
    let text = String::from_utf8_lossy(&body);
    let (head, payload) = text.split_once("\r\n\r\n").expect("framed response");
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .expect("status line")
        .parse()
        .expect("numeric status");
    (status, payload.as_bytes().to_vec())
}

fn get(port: u16, path: &str) -> (u16, Value) {
    let (status, body) = round_trip(port, &format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n"));
    (status, serde_json::from_slice(&body).expect("JSON body"))
}

fn post(port: u16, path: &str, body: &str) -> (u16, Value) {
    let (status, raw) = round_trip(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
    );
    (status, serde_json::from_slice(&raw).expect("JSON body"))
}

/// Mints a session and returns its token.
fn create_session(port: u16) -> String {
    let (status, reply) = post(port, "/sessions", "{}");
    assert_eq!(status, 201, "a session is created: {reply}");
    reply["session"].as_str().expect("session token").to_owned()
}

/// Path of the run-submit route for a session.
fn runs_path(session: &str) -> String {
    format!("/sessions/{session}/runs")
}

/// Path of the snapshot route for one run.
fn snapshot_path(session: &str, run: &str) -> String {
    format!("/sessions/{session}/snapshot?run={run}")
}

/// Path of one run's cancel route.
fn cancel_path(session: &str, run: &str) -> String {
    format!("/sessions/{session}/runs/{run}/cancel")
}

/// Path of one decision route for a run (`approve` or `deny`).
fn decision_path(session: &str, run: &str, decision: &str) -> String {
    format!("/sessions/{session}/runs/{run}/{decision}")
}

/// Submits one task body and returns the host-issued run id. Every caller here
/// submits work the demo script can serve.
fn submit(port: u16, session: &str, body: &str) -> String {
    let (status, reply) = post(port, &runs_path(session), body);
    assert_eq!(status, 201, "valid work is accepted: {reply}");
    assert_eq!(reply["reply"], "accepted");
    reply["run"]
        .as_str()
        .expect("an accepted submit issues a run")
        .to_owned()
}

/// Body binding a decision to one exact grant identity.
fn identity(approval: &str, call: &str) -> String {
    format!(r#"{{"approval":{approval:?},"call":{call:?}}}"#)
}

/// Reads the exact `(approval, call)` tuple a decision must bind, taken from
/// the `approval-required` event. The server never infers these identities, so
/// a test must never hand-build one either.
fn grant_identity(notice: &Value) -> (&str, &str) {
    assert_eq!(notice["kind"], "approval-required");
    (
        notice["detail"]["approval"].as_str().expect("grant id"),
        notice["detail"]["call"].as_str().expect("call id"),
    )
}

/// The snapshot's pending grant ids, in the order the snapshot lists them.
fn pending_approvals(snapshot: &Value) -> Vec<&str> {
    snapshot["pending_approvals"]
        .as_array()
        .expect("pending list")
        .iter()
        .map(|id| id.as_str().expect("approval id"))
        .collect()
}

/// Finds one run's recorded outcome summary inside a snapshot payload.
fn outcome_summary<'a>(snapshot: &'a Value, call: &str) -> &'a Value {
    snapshot["known_outcomes"]
        .as_array()
        .expect("known outcome list")
        .iter()
        .find(|summary| summary["call"].as_str() == Some(call))
        .unwrap_or_else(|| panic!("the snapshot records an outcome for {call}"))
}

/// True when the event names `call` in its detail. Approvals and tool records
/// both do; text and usage events do not.
fn names_call(event: &Value, call: &str) -> bool {
    event["detail"].get("call").and_then(Value::as_str) == Some(call)
}

/// Every observed event of one kind that names `call`, in stream order.
fn records_of<'a>(events: &'a [Value], kind: &str, call: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|event| event["kind"] == kind && names_call(event, call))
        .collect()
}

/// The run's terminal record, which the transport always sends last.
fn terminal_event(events: &[Value]) -> &Value {
    let last = events
        .last()
        .expect("the stream always ends on the terminal event");
    assert_eq!(last["terminal"], true, "the last event closes the run");
    assert_eq!(last["kind"], "run-finished");
    last
}

/// Asserts no execution event in the stream names `call`. A call that was
/// never dispatched can only be named by its decision and its final record.
fn assert_never_dispatched(events: &[Value], call: &str) {
    for event in events {
        let kind = event["kind"].as_str().expect("event kind");
        if matches!(kind, "tool-started" | "tool-output") {
            assert!(!names_call(event, call), "no {kind} for {call}");
        }
    }
}

/// True when a failed socket read is the bounded-wait backstop firing rather
/// than a real transport error.
fn is_timeout(error: &std::io::Error) -> bool {
    matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
}

/// Finds the end of one `\n\n`-terminated frame, if complete.
fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
}

/// One attached SSE subscriber. The session owns a single subscriber slot, so
/// a test keeps the live stream open on this value while it interleaves the
/// snapshot, submit, cancel, and decision requests its flow requires.
struct Stream {
    socket: TcpStream,
    buffer: Vec<u8>,
}

impl Stream {
    /// Attaches to one run's event stream. The socket stays open until the
    /// run's terminal event, so the caller decides when to stop reading.
    fn attach(port: u16, session: &str, run: &str) -> Self {
        let mut socket = TcpStream::connect(address(port)).expect("loopback connects");
        socket
            .set_read_timeout(Some(STREAM_TIMEOUT))
            .expect("timeout sets");
        socket
            .write_all(
                format!("GET /sessions/{session}/runs/{run}/events HTTP/1.1\r\nhost: x\r\n\r\n")
                    .as_bytes(),
            )
            .expect("SSE subscribes");
        Self {
            socket,
            buffer: Vec::new(),
        }
    }

    /// Returns the next published event, blocking until the runtime emits one.
    /// The read timeout is a backstop, not a verdict: the server keeps an idle
    /// stream alive with a `: ping` comment once per idle window, so several
    /// consecutive timeouts are required before the wait counts as stalled.
    /// The bound is still hard, so no wait here can hang the suite.
    fn next(&mut self) -> Value {
        for attempt in 0..STREAM_LIMIT {
            if let Some(event) = self.take_event() {
                return event;
            }
            let mut chunk = [0u8; 4096];
            match self.socket.read(&mut chunk) {
                Ok(0) => panic!("stream closed before the expected event"),
                Ok(read) => self.buffer.extend_from_slice(&chunk[..read]),
                Err(error) if is_timeout(&error) && attempt + 1 < STREAM_LIMIT => {}
                Err(error) => panic!("stream read failed: {error}"),
            }
        }
        panic!("no event arrived within {STREAM_TIMEOUT:?} of waiting");
    }

    /// Reads events until `stop` accepts one, returning the accepted event and
    /// everything observed before it. Lets a test act on a published event (a
    /// grant, say) while the same stream stays open for the rest of the run.
    fn take_until(&mut self, stop: impl Fn(&Value) -> bool) -> (Value, Vec<Value>) {
        let mut seen = Vec::new();
        loop {
            let event = self.next();
            if stop(&event) {
                return (event, seen);
            }
            seen.push(event);
        }
    }

    /// Drives the stream to the terminal event, invoking `on_event` for each
    /// event as it arrives. Returns every observed event, terminal included.
    fn drive_to_terminal(&mut self, mut on_event: impl FnMut(&Value)) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let event = self.next();
            let terminal = event["terminal"] == true;
            on_event(&event);
            events.push(event);
            if terminal {
                return events;
            }
        }
    }

    /// Takes the first event out of the frames already buffered.
    fn take_event(&mut self) -> Option<Value> {
        while let Some(end) = find_frame_end(&self.buffer) {
            let frame = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
            self.buffer.drain(..end);
            for line in frame.lines() {
                if let Some(payload) = line.strip_prefix("data: ") {
                    return Some(serde_json::from_str(payload).expect("event JSON"));
                }
            }
        }
        None
    }
}

/// Drives one run to its terminal event, granting every live grant with the
/// exact identity the stream published. Returns all observed events.
fn drive_granted(port: u16, session: &str, run: &str, stream: &mut Stream) -> Vec<Value> {
    stream.drive_to_terminal(|event| {
        if event["kind"] == "approval-required" {
            let (approval, call) = grant_identity(event);
            let (status, reply) = post(
                port,
                &decision_path(session, run, "approve"),
                &identity(approval, call),
            );
            assert_eq!(status, 200, "an exact grant is accepted: {reply}");
            assert_eq!(reply["reply"], "accepted");
        }
    })
}

/// Opens a second attach to one run's event stream and returns everything the
/// server writes before closing, if anything. An accepted attach never closes
/// on its own, so a bounded read that ends at the peer's close proves the
/// attach claimed nothing.
fn second_attach(port: u16, session: &str, run: &str) -> Vec<u8> {
    let mut socket = TcpStream::connect(address(port)).expect("loopback connects");
    socket
        .set_read_timeout(Some(REQUEST_TIMEOUT))
        .expect("timeout sets");
    socket
        .write_all(
            format!("GET /sessions/{session}/runs/{run}/events HTTP/1.1\r\nhost: x\r\n\r\n")
                .as_bytes(),
        )
        .expect("SSE subscribes");
    let mut written = Vec::new();
    // A timeout means the attach was accepted and the stream is live, which is
    // exactly the outcome the caller asserts against, so the error is dropped.
    let _ = socket.read_to_end(&mut written);
    written
}

/// Starts the demo server on an ephemeral loopback port.
fn spawn_server() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port binds");
    let port = listener.local_addr().expect("port known").port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let server = std::sync::Arc::new(nexus_server::Server::new(runtime.handle().clone()));
    std::thread::spawn(move || {
        // The executor outlives the test: it is reclaimed at process exit,
        // after the last assertion runs.
        std::mem::forget(runtime);
        for stream in listener.incoming().flatten() {
            let server = std::sync::Arc::clone(&server);
            std::thread::spawn(move || server.handle_connection(stream));
        }
    });
    port
}

/// A refused grant is a decision, not a failure: the run never dispatches the
/// refused call, records the refusal as a not-started outcome, and still
/// reaches its own terminal outcome. The stream carries the refusal and no
/// execution record for that call.
#[test]
fn a_refused_grant_never_dispatches_and_the_run_still_completes() {
    let port = spawn_server();
    let session = create_session(port);
    let run = submit(port, &session, r#"{"input":"deny flow"}"#);

    let mut stream = Stream::attach(port, &session, &run);
    let mut refused = None;
    let events = stream.drive_to_terminal(|event| {
        if event["kind"] == "approval-required" && refused.is_none() {
            let (approval, call) = grant_identity(event);
            let (status, reply) = post(
                port,
                &decision_path(&session, &run, "deny"),
                &identity(approval, call),
            );
            assert_eq!(status, 200, "an exact refusal is accepted: {reply}");
            assert_eq!(reply["reply"], "accepted");
            refused = Some(call.to_owned());
        }
    });
    let refused = refused.expect("the demo script asks for one grant");

    // A refused call is a decided call, not a failed run.
    let terminal = terminal_event(&events);
    let outcome = &terminal["detail"]["outcome"];
    assert_eq!(outcome, "completed", "a refusal is not a run failure");

    // Only the automatic read call ever started, and the refused call never
    // appears as a start or as progress, because it was never dispatched.
    let started: Vec<&Value> = events
        .iter()
        .filter(|event| event["kind"] == "tool-started")
        .collect();
    assert_eq!(started.len(), 1, "only the automatic call dispatched");
    assert_never_dispatched(&events, &refused);

    // The refusal is recorded exactly once, as an honest denial.
    let denials = records_of(&events, "tool-finished", &refused);
    assert_eq!(denials.len(), 1, "the refused call is recorded once");
    assert_eq!(denials[0]["detail"]["outcome"]["status"], "Denied");
    assert_eq!(denials[0]["detail"]["outcome"]["effect"], "NotStarted");
    assert_eq!(denials[0]["detail"]["outcome"]["evidence"], "HostObserved");

    let (status, snapshot) = get(port, &snapshot_path(&session, &run));
    assert_eq!(status, 200);
    assert_eq!(snapshot["run"], run);
    assert_eq!(snapshot["lifecycle"], "finalized");
    assert_eq!(snapshot["outcome"], "completed");
    assert_eq!(pending_approvals(&snapshot), Vec::<&str>::new());
    let summary = outcome_summary(&snapshot, &refused);
    assert_eq!(summary["status"], "Denied");
    assert_eq!(summary["effect"], "NotStarted");
}

/// Cancelling while the run is parked on a grant settles it as `cancelled`
/// through the same live stream, records the interrupted call with unknown
/// effects rather than a rewritten success, and leaves the run settled for
/// every later command.
#[test]
fn a_cancel_during_the_approval_wait_terminates_the_run_as_cancelled() {
    let port = spawn_server();
    let session = create_session(port);
    let run = submit(port, &session, r#"{"input":"cancel flow"}"#);

    let mut stream = Stream::attach(port, &session, &run);
    // Waiting for the live grant is the deterministic point to cancel from:
    // the runtime is parked in its approval wait with nothing else in flight.
    let (notice, _) = stream.take_until(|event| event["kind"] == "approval-required");
    let interrupted = notice["detail"]["call"].as_str().expect("call id");

    let (status, reply) = post(port, &cancel_path(&session, &run), "{}");
    assert_eq!(status, 200, "a live run accepts cancellation: {reply}");
    assert_eq!(reply["reply"], "accepted");
    assert_eq!(reply["run"], run, "it names the cancelled run");

    let events = stream.drive_to_terminal(|_| {});
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "cancelled");

    // Cancellation stopped future dispatch: the interrupted call never ran.
    assert_never_dispatched(&events, interrupted);

    // The honest record is a cancellation with uncertain effects, never a
    // success: the call was interrupted, so what it changed is unknown.
    let recorded = records_of(&events, "tool-finished", interrupted);
    assert_eq!(recorded.len(), 1, "the interrupted call is recorded once");
    assert_eq!(recorded[0]["detail"]["outcome"]["status"], "Cancelled");
    assert_eq!(recorded[0]["detail"]["outcome"]["effect"], "Unknown");
    assert_eq!(recorded[0]["detail"]["outcome"]["evidence"], "Uncertain");

    let (status, snapshot) = get(port, &snapshot_path(&session, &run));
    assert_eq!(status, 200);
    assert_eq!(snapshot["lifecycle"], "finalized");
    assert_eq!(snapshot["outcome"], "cancelled");
    assert_eq!(pending_approvals(&snapshot), Vec::<&str>::new());
    let summary = outcome_summary(&snapshot, interrupted);
    assert_eq!(summary["status"], "Cancelled");
    assert_eq!(summary["effect"], "Unknown");

    // Cancellation is a decision about a live run only: once terminal, a
    // repeat cancel is reported against the settled run.
    let (status, late) = post(port, &cancel_path(&session, &run), "{}");
    assert_eq!(status, 409, "a settled run is not cancellable: {late}");
    assert_eq!(late["reply"], "already-finalized");
    assert_eq!(late["run"], run, "it names the settled run");
}

/// The snapshot is the sync source of truth a reconnecting client reads: it
/// lists the live grant while the run waits, drops it the moment the grant is
/// answered, gains each recorded outcome as the run settles, and ends with the
/// terminal event's own sequence so a resume point is exact.
#[test]
fn the_snapshot_tracks_pending_grants_known_outcomes_and_the_terminal_sequence() {
    let port = spawn_server();
    let session = create_session(port);
    let run = submit(port, &session, r#"{"input":"snapshot flow"}"#);

    let mut stream = Stream::attach(port, &session, &run);
    let (notice, before) = stream.take_until(|event| event["kind"] == "approval-required");
    let (approval, call) = grant_identity(&notice);

    let (status, waiting) = get(port, &snapshot_path(&session, &run));
    assert_eq!(status, 200);
    assert_eq!(waiting["run"], run);
    assert_eq!(waiting["lifecycle"], "active");
    assert_eq!(waiting["outcome"], Value::Null);
    // Exactly the live grant is pending, so a reconnecting client can bind a
    // decision to it without waiting for another event.
    assert_eq!(pending_approvals(&waiting), [approval]);

    // The automatic read call finishes before the run reaches the grant, so
    // its outcome is already part of the snapshot's bounded history.
    let read_call = before
        .iter()
        .filter(|event| event["kind"] == "tool-finished")
        .map(|event| {
            event["detail"]["call"]
                .as_str()
                .expect("call id")
                .to_owned()
        })
        .find(|finished| finished != call)
        .expect("the automatic call finished before the wait");
    let summary = outcome_summary(&waiting, &read_call);
    assert_eq!(summary["status"], "Succeeded");
    assert_eq!(summary["effect"], "KnownNotApplied");

    // Granting removes the grant from the pending set before the reply is
    // sent, so the next snapshot cannot still offer a decided grant.
    let (status, reply) = post(
        port,
        &decision_path(&session, &run, "approve"),
        &identity(approval, call),
    );
    assert_eq!(status, 200, "an exact grant is accepted: {reply}");
    assert_eq!(reply["reply"], "accepted");
    let (status, decided) = get(port, &snapshot_path(&session, &run));
    assert_eq!(status, 200);
    assert_eq!(pending_approvals(&decided), Vec::<&str>::new());

    let events = stream.drive_to_terminal(|_| {});
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");

    let (status, settled) = get(port, &snapshot_path(&session, &run));
    assert_eq!(status, 200);
    assert_eq!(settled["lifecycle"], "finalized");
    assert_eq!(settled["outcome"], "completed");
    assert_eq!(
        settled["content_truncated"], false,
        "a demo run drops nothing"
    );
    // The granted call's outcome is known only once the run recorded it.
    let summary = outcome_summary(&settled, call);
    assert_eq!(summary["status"], "Succeeded");
    assert_eq!(summary["effect"], "KnownApplied");
    // The sequence cursor is the terminal event itself, so a client that
    // reconnects can resume exactly after what it already holds.
    assert_eq!(settled["last_sequence"], terminal["seq"]);
    assert!(settled["last_sequence"].as_u64().is_some());
}

/// One session owns one run slot: a second submit while a run is live is a
/// conflict that names the run holding the slot, mints nothing, and is
/// accepted again once that run reaches its terminal outcome.
#[test]
fn a_live_run_slot_rejects_the_second_submit_and_frees_after_the_terminal() {
    let port = spawn_server();
    let session = create_session(port);
    let run = submit(port, &session, r#"{"input":"busy flow"}"#);

    let mut stream = Stream::attach(port, &session, &run);
    let (notice, _) = stream.take_until(|event| event["kind"] == "approval-required");
    let (approval, call) = grant_identity(&notice);

    // The slot is still owned by the parked run, so the second submit is
    // refused instead of racing a competing run in the same session.
    let (status, busy) = post(port, &runs_path(&session), r#"{"input":"more"}"#);
    assert_eq!(status, 409, "a live slot is a conflict: {busy}");
    assert_eq!(busy["reply"], "busy");
    assert_eq!(busy["run"], run, "the conflict names the slot owner");

    let (status, reply) = post(
        port,
        &decision_path(&session, &run, "approve"),
        &identity(approval, call),
    );
    assert_eq!(status, 200, "an exact grant is accepted: {reply}");
    let events = stream.drive_to_terminal(|_| {});
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");

    // The terminal freed the slot, so the same session accepts work again.
    let second = submit(port, &session, r#"{"input":"third task"}"#);
    assert_ne!(second, run, "a new submit issues a new run");
    let mut stream = Stream::attach(port, &session, &second);
    let events = drive_granted(port, &session, &second, &mut stream);
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");
    let (status, snapshot) = get(port, &snapshot_path(&session, &second));
    assert_eq!(status, 200);
    assert_eq!(snapshot["lifecycle"], "finalized");
}

/// A decision that arrives after the terminal is reported against the settled
/// run, on both the grant and the refusal path: the runtime reports the
/// conflict instead of re-opening it, and never dispatches the call twice.
#[test]
fn a_decision_after_the_terminal_is_an_already_finalized_conflict() {
    let port = spawn_server();
    let session = create_session(port);
    let run = submit(port, &session, r#"{"input":"stale grant flow"}"#);

    let mut stream = Stream::attach(port, &session, &run);
    let (notice, _) = stream.take_until(|event| event["kind"] == "approval-required");
    let (approval, call) = grant_identity(&notice);

    let (status, reply) = post(
        port,
        &decision_path(&session, &run, "approve"),
        &identity(approval, call),
    );
    assert_eq!(status, 200, "an exact grant is accepted: {reply}");
    let events = stream.drive_to_terminal(|_| {});
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");
    let dispatched = records_of(&events, "tool-started", call);
    assert_eq!(dispatched.len(), 1, "the granted call dispatched once");

    // The grant was already decided and the run is settled, so replaying the
    // exact identity cannot dispatch it again.
    for decision in ["approve", "deny"] {
        let path = decision_path(&session, &run, decision);
        let (status, late) = post(port, &path, &identity(approval, call));
        assert_eq!(status, 409, "a late decision is a conflict: {late}");
        assert_eq!(late["reply"], "already-finalized");
        assert_eq!(late["run"], run, "it names the settled run");
    }

    // The settled outcome is untouched by the late commands.
    let (status, snapshot) = get(port, &snapshot_path(&session, &run));
    assert_eq!(status, 200);
    assert_eq!(snapshot["lifecycle"], "finalized");
    assert_eq!(snapshot["outcome"], "completed");
    let summary = outcome_summary(&snapshot, call);
    assert_eq!(summary["status"], "Succeeded");
    assert_eq!(
        summary["effect"], "KnownApplied",
        "a late decision is inert"
    );
}

/// A session owns one event-stream subscriber: while a stream is attached, a
/// second attach to the same session claims nothing and leaves the live stream
/// intact.
///
/// The refusal is observable today only as an immediate close with no response
/// at all: `stream_events` builds a `409` for it, but the `/events` route arm
/// discards that response and closes, so no status line reaches the wire. This
/// test therefore pins the refusal that is on the wire (no SSE headers, no
/// frames, peer closed) rather than the `409` the handler builds.
#[test]
fn a_second_event_stream_claims_nothing_while_one_is_attached() {
    let port = spawn_server();
    let session = create_session(port);
    let run = submit(port, &session, r#"{"input":"stream slot flow"}"#);

    let mut stream = Stream::attach(port, &session, &run);
    // Receiving this frame is itself proof that the first attach owns the
    // session's subscriber slot: only an accepted stream carries events.
    let (notice, _) = stream.take_until(|event| event["kind"] == "approval-required");
    let (approval, call) = grant_identity(&notice);

    let written = second_attach(port, &session, &run);
    let text = String::from_utf8_lossy(&written);
    assert!(
        text.starts_with("HTTP/1.1 409 "),
        "a refused attach is a framed conflict, got: {text}"
    );
    let (_, body) = text.split_once("\r\n\r\n").expect("framed error body");
    let reply: serde_json::Value = serde_json::from_str(body).expect("JSON error");
    assert_eq!(reply["error"], "event stream already attached");

    // The live stream is unharmed by the refused attach.
    let (status, reply) = post(
        port,
        &decision_path(&session, &run, "approve"),
        &identity(approval, call),
    );
    assert_eq!(status, 200, "an exact grant is accepted: {reply}");
    let events = stream.drive_to_terminal(|_| {});
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");
}

/// An empty submit input is invalid input: it is refused before any run is
/// minted, and the rejected submit leaves the run slot free for real work.
#[test]
fn an_empty_submit_input_is_rejected_without_minting_a_run() {
    let port = spawn_server();
    let session = create_session(port);
    let path = runs_path(&session);

    let (status, reply) = post(port, &path, r#"{"input":""}"#);
    assert_eq!(status, 400, "empty input is invalid: {reply}");
    assert_eq!(reply["error"], "submit input is invalid");
    assert!(reply.get("run").is_none(), "no run was minted");

    // A body that names no input at all is the same trust boundary.
    let (status, reply) = post(port, &path, "{}");
    assert_eq!(status, 400, "a missing input is invalid: {reply}");
    assert!(reply.get("run").is_none(), "no run was minted");

    // Nothing above disturbed the session, so real work still runs there.
    let run = submit(port, &session, r#"{"input":"after empty"}"#);
    let mut stream = Stream::attach(port, &session, &run);
    let events = drive_granted(port, &session, &run, &mut stream);
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");
}

/// A submit body that is not parseable is invalid input, and the server answers
/// with its own static diagnostic instead of anything the caller sent.
#[test]
fn an_unparsable_submit_body_is_rejected_without_echoing_it() {
    let port = spawn_server();
    let session = create_session(port);
    let path = runs_path(&session);

    let (status, reply) = post(port, &path, r#"{"input":"unterminated"#);
    assert_eq!(status, 400, "an unparsable body is invalid: {reply}");
    assert_eq!(reply["error"], "request body is invalid");
    assert!(reply.get("run").is_none(), "no run was minted");
    let body = reply.to_string();
    assert!(!body.contains("unterminated"), "the input is not echoed");

    // Well-formed JSON that is not a submit body is refused the same way, and
    // a non-string input cannot be smuggled through the string field either.
    for body in [r#"["input"]"#, r#"{"input":42}"#, r#"{"input":null}"#] {
        let (status, reply) = post(port, &path, body);
        assert_eq!(status, 400, "{body} is invalid input: {reply}");
        assert!(reply.get("run").is_none(), "{body} mints no run");
    }

    // The session is still usable after every rejection.
    let run = submit(port, &session, r#"{"input":"after malformed"}"#);
    let mut stream = Stream::attach(port, &session, &run);
    let events = drive_granted(port, &session, &run, &mut stream);
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");
}

/// Unknown runs are refused before SSE claims the session's shared receivers.
#[test]
fn an_unknown_run_event_subscription_is_rejected_immediately() {
    let port = spawn_server();
    let session = create_session(port);
    let path = format!("/sessions/{session}/runs/no-such-run/events");
    let (status, body) = round_trip(port, &format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n"));
    assert_eq!(status, 404);
    let body: Value = serde_json::from_slice(&body).expect("static JSON error");
    assert_eq!(body["error"], "unknown run");

    // Rejection did not leave a subscriber reservation behind.
    let run = submit(port, &session, r#"{"input":"still available"}"#);
    let mut stream = Stream::attach(port, &session, &run);
    let events = drive_granted(port, &session, &run, &mut stream);
    assert_eq!(terminal_event(&events)["detail"]["outcome"], "completed");
}

/// A submit may select its execution profile, and the override is accepted: the
/// run is minted and completes on the demo script like any other task.
///
/// The profile value is not observable anywhere in this API (it is not echoed
/// in the submit reply, the events, or the snapshot, and the demo provider
/// ignores it), so acceptance plus a completed run is the whole observable
/// contract; asserting which profile the runtime used would need a
/// provider-side hook the demo wiring does not offer. What is observable is the
/// boundary: a profile must be non-empty, and an empty one is refused without
/// minting a run, sharing the submit validation diagnostic.
#[test]
fn a_profile_override_is_accepted_and_the_run_still_completes() {
    let port = spawn_server();
    let session = create_session(port);

    let body = r#"{"input":"profile flow","profile":"operator-override"}"#;
    let run = submit(port, &session, body);
    let mut stream = Stream::attach(port, &session, &run);
    let events = drive_granted(port, &session, &run, &mut stream);
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");

    // An empty profile is not a profile: the submit is refused and no run is
    // minted. The status is pinned, not the shared diagnostic text.
    let empty = r#"{"input":"profile flow","profile":""}"#;
    let (status, reply) = post(port, &runs_path(&session), empty);
    assert_eq!(status, 400, "an empty profile is invalid: {reply}");
    assert!(reply.get("run").is_none(), "no run was minted");

    // A submit that names no profile falls back to the server default and is
    // accepted the same way.
    let run = submit(port, &session, r#"{"input":"default profile"}"#);
    let mut stream = Stream::attach(port, &session, &run);
    let events = drive_granted(port, &session, &run, &mut stream);
    let terminal = terminal_event(&events);
    assert_eq!(terminal["detail"]["outcome"], "completed");
}
