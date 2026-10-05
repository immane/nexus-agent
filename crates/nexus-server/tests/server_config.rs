#![forbid(unsafe_code)]

//! Configuration wiring over the real loopback transport: submit selection
//! validation, credential gating, favourites CRUD, recents persistence, and
//! session isolation of the selected model.
//!
//! Servers bind an ephemeral port per test, so nothing here races for a
//! fixed port and a leftover listener cannot affect a later case.
//!
//! # Why credential coverage is asymmetric
//!
//! `resolve_credential` reads the process environment through
//! `std::env::var`, and `std::env::set_var` is `unsafe` on this toolchain
//! (and forbidden by this crate's `#![forbid(unsafe_code)]`). A test
//! therefore cannot inject a credential value it controls, and a shared
//! variable would make the suite order-dependent and racy across the
//! concurrently running test binaries. The two cases are covered as
//! follows instead:
//!
//! - *Missing credential*: a variable name that no environment can plausibly
//!   contain is used, so `resolve_credential` must fail deterministically.
//!   This asserts the whole `503` path including that the diagnostic names
//!   the provider and never a value.
//! - *Present credential*: the probe below selects a variable that the test
//!   process is guaranteed to have (asserted, never assumed) and uses its
//!   name as the provider's credential reference. Only resolvability is
//!   asserted; the value is never read, logged, or compared, so this stays
//!   deterministic while still proving a resolvable selection is accepted
//!   rather than refused.
//!
//! Every socket carries a bounded read timeout, so a stalled server fails a
//! test instead of hanging it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::Duration;

use nexus_config::{
    AdapterKind, CredentialRef, ModelEntry, ProviderProfile, UserConfig, load, save,
};
use serde_json::Value;

/// Read timeout for one framed request/response round trip.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Read timeout for the event stream. A `: ping` arrives on the server's
/// idle window, so this is a backstop rather than a verdict.
const STREAM_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment variables a normal test process always has. One of them is
/// used purely as a *name* for a resolvable credential reference; the value
/// is never read or asserted.
const RESOLVABLE_PROBES: [&str; 4] = ["PATH", "HOME", "USER", "TMPDIR"];

fn address(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// Sends one raw request and reads the framed response. Returns the status
/// and the parsed JSON body.
fn round_trip(port: u16, raw: &str) -> (u16, Value) {
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
    (
        status,
        serde_json::from_slice(payload.as_bytes()).expect("JSON body"),
    )
}

fn get(port: u16, path: &str) -> (u16, Value) {
    round_trip(port, &format!("GET {path} HTTP/1.1\r\nhost: x\r\n\r\n"))
}

fn post(port: u16, path: &str, body: &str) -> (u16, Value) {
    round_trip(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nhost: x\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn delete(port: u16, path: &str) -> (u16, Value) {
    round_trip(port, &format!("DELETE {path} HTTP/1.1\r\nhost: x\r\n\r\n"))
}

/// Mints a session and returns its token.
fn create_session(port: u16) -> String {
    let (status, body) = post(port, "/sessions", "{}");
    assert_eq!(status, 201, "a session is created: {body}");
    body["session"].as_str().expect("session token").to_owned()
}

/// Submits one task and returns the accepted run id, asserting acceptance.
fn submit(port: u16, session: &str, body: &str) -> String {
    let (status, reply) = post(port, &format!("/sessions/{session}/runs"), body);
    assert_eq!(status, 201, "the task is accepted: {reply}");
    reply["run"].as_str().expect("run id").to_owned()
}

/// Finds the end of one `\n\n`-terminated SSE frame, if complete.
fn find_frame_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2)
}

/// Drives a run's stream to its terminal event, approving the live grant
/// with the exact identity from the event, as the demo script requires.
fn drive_to_terminal(port: u16, session: &str, run: &str) {
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
    let mut buffer = Vec::new();
    let mut approved = false;
    loop {
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).expect("stream reads");
        assert!(read > 0, "the stream closes only after the terminal event");
        buffer.extend_from_slice(&chunk[..read]);
        while let Some(end) = find_frame_end(&buffer) {
            let frame = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..end);
            for line in frame.lines() {
                let Some(payload) = line.strip_prefix("data: ") else {
                    continue;
                };
                let event: Value = serde_json::from_str(payload).expect("event JSON");
                if event["kind"] == "approval-required" && !approved {
                    approved = true;
                    let approval = event["detail"]["approval"].as_str().expect("grant id");
                    let call = event["detail"]["call"].as_str().expect("call id");
                    let (status, reply) = post(
                        port,
                        &format!("/sessions/{session}/runs/{run}/approve"),
                        &format!(r#"{{"approval":{approval:?},"call":{call:?}}}"#),
                    );
                    assert_eq!(status, 200, "the exact grant approves: {reply}");
                }
                if event["terminal"] == true {
                    assert_eq!(event["kind"], "run-finished");
                    return;
                }
            }
        }
    }
}

/// Returns a unique temporary directory for one test case. The process id
/// keeps parallel test binaries apart; the `tag` keeps cases within this
/// binary apart. Nothing is shared between tests, so no case can observe
/// another's document.
fn temp_config(tag: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("nexus-server-config-{}-{tag}", process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("temp directory is creatable");
    directory.join("config.json")
}

/// Returns an environment variable name that the test process actually has.
/// Asserted rather than assumed: an empty value would resolve as missing and
/// silently turn a present-credential case into the `503` case.
fn resolvable_probe() -> &'static str {
    for name in RESOLVABLE_PROBES {
        if std::env::var(name).is_ok_and(|value| !value.is_empty()) {
            return name;
        }
    }
    panic!("a test process is expected to have one of {RESOLVABLE_PROBES:?} set and non-empty");
}

/// A credential variable name that no plausible environment defines. The
/// `NEXUS_SERVER_CONFIG_ABSENT_` prefix plus a fixed suffix keeps it inside
/// the configured charset while making accidental presence implausible.
const ABSENT_CREDENTIAL: &str = "NEXUS_SERVER_CONFIG_ABSENT_9F3A";

/// Builds a provider whose credential reference is `credential`.
fn provider(id: &str, credential: &str) -> ProviderProfile {
    ProviderProfile::new(
        id,
        format!("{id} display"),
        AdapterKind::Direct,
        Some("https://api.example.com".to_owned()),
        CredentialRef::env_var(credential).expect("valid credential reference"),
        "default-model",
    )
    .expect("valid provider builds")
}

/// Builds a model bound to `provider_id`.
fn model(id: &str, provider_id: &str) -> ModelEntry {
    ModelEntry::new(id, provider_id, format!("{id}-model")).expect("valid model builds")
}

/// A document with one provider and two models, all gated behind
/// `credential`.
fn configured(credential: &str) -> UserConfig {
    let mut config = UserConfig::default_config();
    config
        .add_provider(provider("acme", credential))
        .expect("provider admits");
    config
        .add_model(model("fast", "acme"))
        .expect("model admits");
    config
        .add_model(model("strong", "acme"))
        .expect("model admits");
    config
}

/// A document with `count` models, for the bounded favourites list.
fn many_models(count: usize) -> UserConfig {
    let mut config = UserConfig::default_config();
    config
        .add_provider(provider("acme", ABSENT_CREDENTIAL))
        .expect("provider admits");
    for index in 0..count {
        config
            .add_model(model(&format!("m{index}"), "acme"))
            .expect("model admits");
    }
    config
}

/// Starts a server on an ephemeral loopback port with the given
/// configuration and optional persistence path.
fn spawn_server(config: UserConfig, path: Option<PathBuf>) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port binds");
    let port = listener.local_addr().expect("port known").port();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let mut server = nexus_server::Server::new(runtime.handle().clone());
    server.set_config(config, path);
    let server = Arc::new(server);
    std::thread::spawn(move || {
        // The executor outlives the test: it is reclaimed at process exit,
        // after the last assertion runs.
        std::mem::forget(runtime);
        for stream in listener.incoming().flatten() {
            let server = Arc::clone(&server);
            std::thread::spawn(move || server.handle_connection(stream));
        }
    });
    port
}

/// Reads back a persisted document, asserting it exists.
fn read_back(path: &Path) -> UserConfig {
    load(path)
        .expect("document loads")
        .expect("document exists")
}

/// A resolvable selection is accepted and its run completes; the refusal
/// paths that a missing credential produces are the subject of the next
/// test.
#[test]
fn a_resolvable_selection_is_accepted_and_its_run_completes() {
    let probe = resolvable_probe();
    let path = temp_config("resolvable");
    let port = spawn_server(configured(probe), Some(path.clone()));
    let session = create_session(port);

    let (status, body) = post(
        port,
        &format!("/sessions/{session}/runs"),
        r#"{"input":"selected task","provider":"acme","model":"fast"}"#,
    );
    assert_eq!(status, 201, "a resolvable selection is accepted: {body}");
    let run = body["run"].as_str().expect("run id");
    drive_to_terminal(port, &session, run);

    let (status, snapshot) = get(port, &format!("/sessions/{session}/snapshot?run={run}"));
    assert_eq!(status, 200);
    assert_eq!(snapshot["lifecycle"], "finalized");
    assert_eq!(snapshot["outcome"], "completed");

    // Selection is recorded at the terminal event and survives a
    // save/load round trip through the configured file.
    let persisted = read_back(&path);
    assert_eq!(
        persisted.recent(),
        ["fast".to_owned()],
        "the run's model is the one recently used"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// A provider whose credential cannot be resolved is a readiness failure,
/// not bad input: the request is well-formed and would succeed once the
/// environment is set. The run slot stays untouched and nothing is recorded.
#[test]
fn an_unresolvable_credential_is_a_readiness_failure_naming_only_the_provider() {
    let path = temp_config("absent-credential");
    let port = spawn_server(configured(ABSENT_CREDENTIAL), Some(path.clone()));
    let session = create_session(port);

    for (case, body) in [
        ("model only", r#"{"input":"t","model":"fast"}"#.to_owned()),
        (
            "provider and model",
            r#"{"input":"t","provider":"acme","model":"fast"}"#.to_owned(),
        ),
        (
            "provider only",
            r#"{"input":"t","provider":"acme"}"#.to_owned(),
        ),
    ] {
        let (status, reply) = post(port, &format!("/sessions/{session}/runs"), &body);
        assert_eq!(status, 503, "{case} is unavailable, not invalid: {reply}");
        let message = reply["error"].as_str().expect("diagnostic");
        assert!(
            message.contains("acme"),
            "{case} names the provider so the user can act: {message}"
        );
        assert!(
            !message.contains(ABSENT_CREDENTIAL),
            "{case} diagnostic carries no credential detail: {message}"
        );
        assert!(reply.get("run").is_none(), "{case} mints no run: {reply}");
    }

    // The refused submits never occupied the slot, so a run with no
    // selection is still accepted afterwards.
    let run = submit(port, &session, r#"{"input":"unselected task"}"#);
    drive_to_terminal(port, &session, &run);
    let (status, summary) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(
        summary["recent"],
        serde_json::json!([]),
        "a refused selection and an unselected run record no usage"
    );
    // Nothing was ever recorded, so nothing was ever written either: the
    // refusals must not create a document as a side effect.
    assert!(
        !path.exists(),
        "no usage means no file: refusals do not write a document"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// Every unknown or inconsistent selection is invalid input, refused before
/// the runtime is touched so it can never mint a run.
#[test]
fn unknown_or_inconsistent_selections_are_invalid_input() {
    let probe = resolvable_probe();
    let path = temp_config("selection-validation");
    let port = spawn_server(configured(probe), Some(path.clone()));
    let session = create_session(port);

    // The served document is the configured one, so the summary check
    // below is about a loaded file rather than about defaults.
    let (status, summary) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(summary["providers"][0]["id"], "acme");

    for (case, body) in [
        (
            "unknown provider",
            r#"{"input":"t","provider":"nope"}"#.to_owned(),
        ),
        (
            "unknown model",
            r#"{"input":"t","model":"nope"}"#.to_owned(),
        ),
        (
            "unknown provider with a known model",
            r#"{"input":"t","provider":"nope","model":"fast"}"#.to_owned(),
        ),
        (
            "unknown model with a known provider",
            r#"{"input":"t","provider":"acme","model":"nope"}"#.to_owned(),
        ),
        (
            "a model field that is not text",
            r#"{"input":"t","model":12}"#.to_owned(),
        ),
        (
            "a provider field that is not text",
            r#"{"input":"t","provider":["acme"]}"#.to_owned(),
        ),
    ] {
        let (status, reply) = post(port, &format!("/sessions/{session}/runs"), &body);
        assert_eq!(status, 400, "{case} is invalid input: {reply}");
        assert!(reply.get("run").is_none(), "{case} mints no run: {reply}");
    }

    // None of the refusals recorded usage or consumed the single slot.
    assert!(
        !path.exists(),
        "no usage was recorded, so nothing was saved"
    );
    let run = submit(port, &session, r#"{"input":"still available"}"#);
    drive_to_terminal(port, &session, &run);
    assert!(
        !path.exists(),
        "an unselected run records nothing and writes nothing"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// A model that belongs to a different provider is refused rather than
/// silently resolved through whichever field the server read first.
#[test]
fn a_model_from_another_provider_is_refused() {
    let probe = resolvable_probe();
    let path = temp_config("cross-provider");
    let mut config = configured(probe);
    config
        .add_provider(provider("other", probe))
        .expect("second provider admits");
    config
        .add_model(model("foreign", "other"))
        .expect("foreign model admits");
    let port = spawn_server(config, Some(path.clone()));

    let session = create_session(port);
    let (status, reply) = post(
        port,
        &format!("/sessions/{session}/runs"),
        r#"{"input":"t","provider":"acme","model":"foreign"}"#,
    );
    assert_eq!(status, 400, "a mismatched pair is invalid: {reply}");
    assert!(reply.get("run").is_none(), "no run is minted: {reply}");

    // The matching pair for each provider is accepted, so the refusal is
    // about the mismatch and not about either identity being unknown.
    let run = submit(
        port,
        &session,
        r#"{"input":"t","provider":"other","model":"foreign"}"#,
    );
    drive_to_terminal(port, &session, &run);
    assert_eq!(read_back(&path).recent(), ["foreign".to_owned()]);
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// `GET /config` reports the redacted summary: identities, the credential
/// *reference*, favourites, and recents, and never a credential value.
#[test]
fn the_config_summary_is_redacted_and_complete() {
    let probe = resolvable_probe();
    let path = temp_config("summary");
    let mut config = configured(probe);
    config.add_favourite("fast").expect("favourite admits");
    config.record_use("fast").expect("use records");
    save(&config, &path).expect("document saves");
    let port = spawn_server(config, Some(path.clone()));

    let (status, body) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(body["revision"], 1);
    assert_eq!(body["providers"][0]["id"], "acme");
    assert_eq!(body["providers"][0]["adapter"], "direct");
    assert_eq!(body["providers"][0]["endpoint"], "https://api.example.com");
    assert_eq!(
        body["providers"][0]["credential"]["env"], probe,
        "the reference names the variable, which is safe to show"
    );
    assert_eq!(body["models"][0]["id"], "fast");
    assert_eq!(body["models"][0]["provider"], "acme");
    assert_eq!(body["favourites"], serde_json::json!(["fast"]));
    assert_eq!(body["recent"], serde_json::json!(["fast"]));

    // No field anywhere carries a value: the document format has no place
    // to put one, so the summary cannot introduce one either.
    let rendered = body.to_string();
    assert!(
        !rendered.contains(&std::env::var(probe).expect("probe value")),
        "the summary never carries a credential value"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// Favourites are added and reported back, each addition reaching the
/// configured file. Removal is covered in
/// `removing_a_favourite_cannot_arrive_over_the_wire`, which records why it
/// cannot yet be driven through a socket.
#[test]
fn favourites_can_be_added_listed_and_persisted() {
    let probe = resolvable_probe();
    let path = temp_config("favourites-crud");
    let port = spawn_server(configured(probe), Some(path.clone()));

    let (status, summary) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(summary["favourites"], serde_json::json!([]));

    let (status, body) = post(port, "/config/favourites", r#"{"id":"fast"}"#);
    assert_eq!(status, 200, "a known model becomes a favourite: {body}");
    assert_eq!(body["favourites"], serde_json::json!(["fast"]));

    let (status, body) = post(port, "/config/favourites", r#"{"id":"strong"}"#);
    assert_eq!(status, 200, "a second favourite admits: {body}");
    assert_eq!(body["favourites"], serde_json::json!(["fast", "strong"]));

    // Admission order, not alphabetical order, and the file agrees.
    let (status, summary) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(summary["favourites"], serde_json::json!(["fast", "strong"]));
    assert_eq!(
        read_back(&path).favourites(),
        ["fast".to_owned(), "strong".to_owned()],
        "both additions persisted"
    );

    let (status, body) = post(port, "/config/favourites", r#"{"id":"fast"}"#);
    assert_eq!(
        status, 200,
        "re-adding an existing favourite is idempotent: {body}"
    );
    assert_eq!(body["favourites"], serde_json::json!(["fast", "strong"]));

    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// The three documented favourites refusals: an unknown model is invalid
/// input, a full list is a conflict, and a malformed identity is invalid.
/// None of them changes the stored list.
#[test]
fn favourites_refuse_unknown_models_and_a_full_list_without_changing_state() {
    let path = temp_config("favourites-refusals");
    // Sixteen is the configuration's own bound, so the seventeenth is a
    // conflict by the model's rule rather than a server-specific limit.
    let port = spawn_server(many_models(16), Some(path.clone()));

    let (status, body) = post(port, "/config/favourites", r#"{"id":"nope"}"#);
    assert_eq!(status, 400, "an unknown model is invalid input: {body}");
    let (status, body) = post(port, "/config/favourites", r#"{}"#);
    assert_eq!(status, 400, "a missing identity is invalid input: {body}");
    let (status, body) = post(port, "/config/favourites", r#"{"id":123}"#);
    assert_eq!(
        status, 400,
        "a non-string identity is invalid input: {body}"
    );

    for index in 0..16 {
        let (status, body) = post(
            port,
            "/config/favourites",
            &format!(r#"{{"id":"m{index}"}}"#),
        );
        assert_eq!(status, 200, "favourite {index} admits: {body}");
    }
    let (status, body) = post(port, "/config/favourites", r#"{"id":"m16"}"#);
    assert_eq!(status, 400, "that id is not a configured model: {body}");

    // With every slot taken, the next known model conflicts.
    let port = spawn_server(many_models(17), Some(path.clone()));
    for index in 0..16 {
        let (status, _) = post(
            port,
            "/config/favourites",
            &format!(r#"{{"id":"m{index}"}}"#),
        );
        assert_eq!(status, 200);
    }
    let (status, body) = post(port, "/config/favourites", r#"{"id":"m16"}"#);
    assert_eq!(status, 409, "a full list is a conflict: {body}");
    assert_eq!(
        read_back(&path).favourites().len(),
        16,
        "the refused addition was not persisted"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// Removing a favourite arrives over the wire as `DELETE`: the stored
/// list loses the entry, the file persists it, and a repeat removal is an
/// unknown favourite.
#[test]
fn removing_a_favourite_arrives_over_the_wire() {
    let probe = resolvable_probe();
    let path = temp_config("delete-wire");
    let mut config = configured(probe);
    config.add_favourite("fast").expect("favourite admits");
    config.add_favourite("strong").expect("favourite admits");
    save(&config, &path).expect("document saves");
    let port = spawn_server(config, Some(path.clone()));

    let (status, body) = delete(port, "/config/favourites/fast");
    assert_eq!(status, 200, "removal succeeds: {body}");
    assert_eq!(body["favourites"], serde_json::json!(["strong"]));
    assert_eq!(
        read_back(&path).favourites(),
        ["strong".to_owned()],
        "removal persists"
    );

    let (status, _) = delete(port, "/config/favourites/fast");
    assert_eq!(status, 404, "repeat removal is unknown");
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// Sessions are independent: each records the model *it* selected, and one
/// session's run never appears in another's attribution. Recents is shared
/// server state, so both models must appear, and the run ids differ.
#[test]
fn selection_is_isolated_per_session() {
    let probe = resolvable_probe();
    let path = temp_config("session-isolation");
    let port = spawn_server(configured(probe), Some(path.clone()));
    let first = create_session(port);
    let second = create_session(port);

    let first_run = submit(
        port,
        &first,
        r#"{"input":"first","provider":"acme","model":"fast"}"#,
    );
    let second_run = submit(
        port,
        &second,
        r#"{"input":"second","provider":"acme","model":"strong"}"#,
    );
    assert_ne!(first_run, second_run, "sessions mint distinct runs");

    // Both streams are driven to their terminals before either usage is
    // read, so the assertion cannot depend on which run finishes first.
    drive_to_terminal(port, &first, &first_run);
    drive_to_terminal(port, &second, &second_run);

    let recents = read_back(&path).recent().to_vec();
    assert_eq!(
        recents.len(),
        2,
        "each session recorded exactly its own model: {recents:?}"
    );
    assert!(
        recents.contains(&"fast".to_owned()) && recents.contains(&"strong".to_owned()),
        "both attributions are present: {recents:?}"
    );

    // A session's snapshot reflects only its own run.
    let (status, snapshot) = get(port, &format!("/sessions/{first}/snapshot?run={first_run}"));
    assert_eq!(status, 200);
    assert_eq!(snapshot["run"], first_run);
    let (status, _) = get(
        port,
        &format!("/sessions/{second}/snapshot?run={first_run}"),
    );
    assert_eq!(
        status, 404,
        "one session cannot read another's run: {status}"
    );
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// The most-recent-first ordering is the configuration model's rule, and
/// the server preserves it across repeated uses of different models and
/// across a save/load round trip.
#[test]
fn recents_are_ordered_most_recent_first_across_a_save_and_load_round_trip() {
    let probe = resolvable_probe();
    let path = temp_config("recents-order");
    let port = spawn_server(configured(probe), Some(path.clone()));
    let session = create_session(port);

    for model_id in ["fast", "strong", "fast"] {
        let run = submit(
            port,
            &session,
            &format!(r#"{{"input":"t","provider":"acme","model":"{model_id}"}}"#),
        );
        drive_to_terminal(port, &session, &run);
    }

    let on_disk = read_back(&path);
    assert_eq!(
        on_disk.recent(),
        ["fast".to_owned(), "strong".to_owned()],
        "the most recent use is first and the older entry survives"
    );
    let (status, summary) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(summary["recent"], serde_json::json!(["fast", "strong"]));

    // A second server over the same file sees the same history, proving
    // the ordering survived persistence rather than living only in memory.
    let restarted = spawn_server(load(&path).expect("loads").expect("exists"), None);
    let (status, summary) = get(restarted, "/config");
    assert_eq!(status, 200);
    assert_eq!(summary["recent"], serde_json::json!(["fast", "strong"]));
    let _ = std::fs::remove_dir_all(path.parent().expect("temp directory"));
}

/// A server with no configured path still answers the summary and refuses
/// favourites, because an unconfigured server has no model identities. It
/// must report that rather than invent an empty success.
#[test]
fn an_unconfigured_server_reports_no_identities() {
    let port = spawn_server(UserConfig::default_config(), None);
    let (status, body) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(body["providers"], serde_json::json!([]));
    assert_eq!(body["models"], serde_json::json!([]));

    let (status, reply) = post(port, "/config/favourites", r#"{"id":"anything"}"#);
    assert_eq!(
        status, 400,
        "there is no configured model to favourite: {reply}"
    );

    // Submit without a selection is still served by the demo wiring, and
    // records no usage because nothing was selected.
    let session = create_session(port);
    let run = submit(port, &session, r#"{"input":"demo task"}"#);
    drive_to_terminal(port, &session, &run);
    let (status, summary) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(summary["recent"], serde_json::json!([]));
}

/// A save that cannot succeed is reported as a durability failure, not as a
/// lost edit: the change is live in memory and visible in the summary, and
/// the response says the document was not written.
#[test]
fn a_failed_save_reports_500_while_the_edit_stays_live() {
    // A path whose parent does not exist makes the atomic write fail
    // deterministically, without depending on filesystem permissions.
    let directory = temp_config("failed-save");
    let unreachable = directory.join("missing-dir").join("config.json");
    let port = spawn_server(configured(resolvable_probe()), Some(unreachable));

    let (status, reply) = post(port, "/config/favourites", r#"{"id":"fast"}"#);
    assert_eq!(status, 500, "the edit could not be persisted: {reply}");

    let (status, summary) = get(port, "/config");
    assert_eq!(status, 200);
    assert_eq!(
        summary["favourites"],
        serde_json::json!(["fast"]),
        "the edit is live in memory even though the save failed"
    );
    let _ = std::fs::remove_dir_all(&directory);
}
