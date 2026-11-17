#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use nexus_core::{
    ApprovedScope, CallId, ExecutionStatus, M0_REVISION, NormalizedArgs, RunId, ToolCall,
    ToolContext, ToolId, ToolPort, TurnId,
};
use nexus_tools::SandboxedExecutor;

static ROOTS: AtomicUsize = AtomicUsize::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "nexus-exec-test-{}-{}",
            std::process::id(),
            ROOTS.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("test root is created");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(args: &str) -> ToolCall {
    ToolCall::new(
        RunId::new("run-exec").unwrap(),
        TurnId::new("turn-exec").unwrap(),
        CallId::new("call-exec").unwrap(),
        ToolId::new("host_exec", M0_REVISION).unwrap(),
        NormalizedArgs::new(args).unwrap(),
    )
}

fn context() -> ToolContext {
    context_with_scope("command:/usr/bin/printf sandbox-ok")
}

fn context_with_scope(scope: &str) -> ToolContext {
    ToolContext::new(
        4096,
        Duration::ZERO,
        false,
        ApprovedScope::new(scope).unwrap(),
    )
    .unwrap()
}

fn read_fixture_headers(reader: &mut impl std::io::Read) -> std::io::Result<()> {
    let mut headers = [0; 4096];
    let mut used = 0;
    while used < headers.len() {
        let count = reader.read(&mut headers[used..])?;
        if count == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        used += count;
        if headers[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            return Ok(());
        }
    }
    Err(std::io::ErrorKind::InvalidData.into())
}

#[test]
fn loopback_fixture_waits_for_a_request_that_arrives_after_accept() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let worker = std::thread::spawn(move || {
        loop {
            match listener.accept() {
                // BSD/macOS inherits the listener's nonblocking flag into the
                // accepted socket; a timed read must be blocking first.
                Ok((mut socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    read_fixture_headers(&mut socket).unwrap();
                    socket.write_all(b"ok").unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        }
    });
    let mut control = std::net::TcpStream::connect(address).unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    std::thread::sleep(Duration::from_millis(50));
    control
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    control.read_to_string(&mut response).unwrap();
    assert_eq!(response, "ok");
    worker.join().unwrap();
}

#[test]
fn loopback_fixture_reads_fragmented_headers_and_rejects_incomplete_or_oversized_requests() {
    use std::io::{Cursor, Read};
    let first = Cursor::new(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r");
    let last = Cursor::new(b"\n");
    read_fixture_headers(&mut first.chain(last)).unwrap();
    assert_eq!(
        read_fixture_headers(&mut Cursor::new(b"GET / HTTP/1.1\r\n"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    assert_eq!(
        read_fixture_headers(&mut Cursor::new([b'x'; 4097]))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}

/// A bounded loopback fixture proves success and denial against the same live
/// service, without touching external DNS, proxies, or a public endpoint.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires an explicitly provisioned Linux sandbox and curl"
)]
fn network_policy_reaches_live_service_only_when_enabled() {
    use std::io::{Read, Write};
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::Instant;
    for bind in ["127.0.0.1:0", "[::1]:0"] {
        let listener = match std::net::TcpListener::bind(bind) {
            Ok(listener) => listener,
            Err(error)
                if bind.starts_with('[')
                    && error.kind() == std::io::ErrorKind::AddrNotAvailable =>
            {
                eprintln!("IPv6 loopback is unavailable; IPv4 checks still apply");
                continue;
            }
            Err(error) => panic!("loopback fixture unavailable: {error}"),
        };
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // macOS inherits the listener's nonblocking flag. Reset
                        // it before the timed read; otherwise WouldBlock closes
                        // the socket while the client's request is still unread.
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        // Drain the complete request before closing: unread TCP
                        // bytes can turn the response's EOF into a reset.
                        if read_fixture_headers(&mut socket).is_ok() {
                            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nnetwork-ok");
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
        });
        // Control request demonstrates that the server is actually reachable.
        let mut control = std::net::TcpStream::connect(address).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        control
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        control.read_to_string(&mut response).unwrap();
        assert!(response.ends_with("network-ok"));
        for development in [false, true] {
            let root = TempRoot::new();
            std::fs::write(root.0.join(".env"), "network-test-secret").unwrap();
            for enabled in [false, true] {
                let executor = if development {
                    SandboxedExecutor::development(&root.0)
                } else {
                    SandboxedExecutor::with_root(&root.0)
                }
                .unwrap()
                .with_network_access(enabled);
                assert!(
                    executor.sandbox_available(),
                    "required sandbox must initialize"
                );
                if development {
                    let protected = executor.execute(
                        &call(r#"{"argv":["/bin/cat",".env"]}"#),
                        &context_with_scope("exec:workspace"),
                    );
                    assert!(!protected.content().contains("network-test-secret"));
                }
                // Explicitly launching a shell here also checks that a child gets
                // the same network policy; the tool does not insert this shell.
                let destination = if address.is_ipv4() {
                    format!("localhost:{}", address.port())
                } else {
                    address.to_string()
                };
                let command = format!(
                    "/usr/bin/curl --noproxy '*' --max-time 1 --fail --silent http://{destination}/"
                );
                let outcome = executor.execute(
                    &call(&serde_json::json!({"argv":["/bin/sh","-c",command]}).to_string()),
                    &context_with_scope(if development {
                        "exec:workspace"
                    } else {
                        "command:network-test"
                    }),
                );
                if enabled {
                    assert_eq!(
                        outcome.status(),
                        ExecutionStatus::Succeeded,
                        "{}",
                        outcome.content()
                    );
                    assert!(outcome.content().contains("network-ok"));
                } else {
                    assert_eq!(
                        outcome.status(),
                        ExecutionStatus::Failed,
                        "{}",
                        outcome.content()
                    );
                    assert!(!outcome.content().contains("network-ok"));
                }
            }
        }
        stop.store(true, Ordering::Release);
        worker.join().unwrap();
    }
}

#[test]
fn tool_network_is_an_operator_snapshot_not_a_model_argument() {
    let root = TempRoot::new();
    for tools in [
        nexus_tools::strict_tools_with_settings(
            &root.0,
            &nexus_config::ToolsSettings {
                network_access: false,
                ..Default::default()
            },
        )
        .unwrap(),
        nexus_tools::development_tools_with_settings(
            &root.0,
            &nexus_config::ToolsSettings {
                network_access: false,
                ..Default::default()
            },
        )
        .unwrap(),
    ] {
        let spec = tools.last().unwrap().port().describe();
        assert!(spec.description().contains("Network disabled"));
    }
    let executor = SandboxedExecutor::development(&root.0)
        .unwrap()
        .with_network_access(false);
    let result = executor.execute(
        &call(r#"{"argv":["/usr/bin/true"],"network_access":true}"#),
        &context_with_scope("exec:workspace"),
    );
    assert_eq!(result.status(), ExecutionStatus::Failed);
    let enabled = executor.with_network_access(true);
    assert!(enabled.describe().description().contains("Network enabled"));
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires an explicitly provisioned Linux sandbox and C compiler"
)]
fn network_policy_controls_udp_replies_as_well_as_tcp() {
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::Instant;
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let address = socket.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut bytes = [0; 64];
        while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
            match socket.recv_from(&mut bytes) {
                Ok((count, peer)) => {
                    socket.send_to(&bytes[..count], peer).unwrap();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("UDP fixture failed: {error}"),
            }
        }
    });
    let control = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    control
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    control.send_to(b"control", address).unwrap();
    assert_eq!(control.recv(&mut [0; 64]).unwrap(), 7);
    for development in [false, true] {
        let root = TempRoot::new();
        // A project-local native client avoids interpreter/toolchain-launcher
        // reads outside strict-mode roots. Rust already requires a C linker.
        let source = root.0.join("udp.c");
        let client = root.0.join("udp-client");
        std::fs::write(&source, r#"
#include <arpa/inet.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc != 2) return 2;
    int s = socket(AF_INET, SOCK_DGRAM, 0);
    struct timeval timeout = {0, 500000};
    if (s < 0 || setsockopt(s, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout))) return 3;
    struct sockaddr_in peer = {0};
    peer.sin_family = AF_INET;
    peer.sin_port = htons(atoi(argv[1]));
    peer.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (connect(s, (struct sockaddr *)&peer, sizeof(peer)) || send(s, "udp-ok", 6, 0) != 6) return 4;
    char response[64];
    if (recv(s, response, sizeof(response), 0) != 6 || memcmp(response, "udp-ok", 6)) return 5;
    close(s);
    puts("udp-ok");
    return 0;
}
"#).unwrap();
        let compiled = std::process::Command::new("cc")
            .arg(&source)
            .arg("-o")
            .arg(&client)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "{}",
            String::from_utf8_lossy(&compiled.stderr)
        );
        let client = client.canonicalize().unwrap();
        for enabled in [false, true] {
            let executor = if development {
                SandboxedExecutor::development(&root.0)
            } else {
                SandboxedExecutor::with_root(&root.0)
            }
            .unwrap()
            .with_network_access(enabled);
            let outcome = executor.execute(
                &call(&serde_json::json!({"argv":[client.to_str().unwrap(),address.port().to_string()]}).to_string()),
                &context_with_scope(if development { "exec:workspace" } else { "command:network-test" }),
            );
            if enabled {
                assert_eq!(
                    outcome.status(),
                    ExecutionStatus::Succeeded,
                    "{}",
                    outcome.content()
                );
                assert!(outcome.content().contains("udp-ok"));
            } else {
                assert_eq!(outcome.status(), ExecutionStatus::Failed);
                assert!(!outcome.content().contains("udp-ok"));
            }
        }
    }
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn argv_runs_inside_the_configured_sandbox_root() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).expect("valid root binds");
    #[cfg(target_os = "macos")]
    assert!(
        executor.sandbox_available(),
        "the installed macOS sandbox must pass its startup probe"
    );
    let result = executor.execute(
        &call(r#"{"argv":["/usr/bin/printf","sandbox-ok"]}"#),
        &context(),
    );
    if executor.sandbox_available() {
        assert_eq!(
            result.status(),
            ExecutionStatus::Succeeded,
            "{}",
            result.content()
        );
        assert_eq!(result.content(), "sandbox-ok");
    } else {
        assert_eq!(result.status(), ExecutionStatus::Denied);
        assert_eq!(result.effect(), nexus_core::EffectState::NotStarted);
        assert!(matches!(
            result.content(),
            "required exec sandbox program was not found"
                | "exec sandbox program was found but could not be started"
                | "exec sandbox program was found but its initialization probe failed"
        ));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
#[ignore = "P7 measurement: requires the real OS sandbox backend"]
fn measure_first_command_with_deferred_sandbox_probe() {
    const SAMPLES: usize = 20;
    let mut elapsed_ms = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let root = TempRoot::new();
        let executor = SandboxedExecutor::with_root(&root.0).expect("valid root binds");
        let started = std::time::Instant::now();
        let result = executor.execute(
            &call(r#"{"argv":["/usr/bin/printf","first-command-ok"]}"#),
            &context(),
        );
        let elapsed = started.elapsed();
        assert_eq!(
            result.status(),
            ExecutionStatus::Succeeded,
            "{}",
            result.content()
        );
        assert_eq!(result.content(), "first-command-ok");
        elapsed_ms.push(elapsed.as_secs_f64() * 1000.0);
        println!(
            "first_command_sample={index} elapsed_ms={:.3}",
            elapsed_ms[index]
        );
    }
    elapsed_ms.sort_by(f64::total_cmp);
    let percentile = |q: f64| elapsed_ms[((SAMPLES as f64 * q).ceil() as usize).saturating_sub(1)];
    println!(
        "first_command_probe_latency_ms n={SAMPLES} p50={:.3} p95={:.3} max={:.3}",
        percentile(0.50),
        percentile(0.95),
        elapsed_ms[SAMPLES - 1]
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox and curl"]
fn linux_sandbox_confines_files_and_network() {
    let root = TempRoot::new();
    let outside = TempRoot::new();
    let secret = outside.0.join("secret.txt");
    std::fs::write(&secret, "outside-secret").unwrap();
    let executor = SandboxedExecutor::with_root(&root.0)
        .unwrap()
        .with_network_access(false);
    assert!(
        executor.sandbox_available(),
        "Linux sandbox backend is required"
    );
    let run = |argv: Vec<String>| {
        executor.execute(
            &call(&serde_json::json!({"argv": argv}).to_string()),
            &context(),
        )
    };

    let written = run(vec!["/usr/bin/touch".into(), "created.txt".into()]);
    assert_eq!(
        written.status(),
        ExecutionStatus::Succeeded,
        "{}",
        written.content()
    );
    assert!(root.0.join("created.txt").exists());

    let read = run(vec!["/bin/cat".into(), secret.display().to_string()]);
    assert_eq!(read.status(), ExecutionStatus::Failed);
    assert!(!read.content().contains("outside-secret"));
    let refused = outside.0.join("refused.txt");
    let write = run(vec!["/usr/bin/touch".into(), refused.display().to_string()]);
    assert_eq!(write.status(), ExecutionStatus::Failed);
    assert!(!refused.exists());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    assert!(std::path::Path::new("/usr/bin/curl").is_file());
    let url = format!("http://{}", listener.local_addr().unwrap());
    let network = run(vec![
        "/usr/bin/curl".into(),
        "--noproxy".into(),
        "*".into(),
        "--max-time".into(),
        "1".into(),
        url,
    ]);
    assert_eq!(network.status(), ExecutionStatus::Failed);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox"]
fn linux_development_sandbox_masks_project_credentials() {
    let root = TempRoot::new();
    std::fs::write(root.0.join(".env"), "sandbox-secret").unwrap();
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(
        executor.sandbox_available(),
        "Linux sandbox backend is required"
    );
    let result = executor.execute(
        &call(r#"{"argv":["/bin/cat",".env"]}"#),
        &context_with_scope("exec:workspace"),
    );
    match result.status() {
        ExecutionStatus::Succeeded => assert!(result.content().is_empty()),
        ExecutionStatus::Failed => assert!(result.content().contains("Permission denied")),
        status => panic!("unexpected credential read status: {status:?}"),
    }
    assert!(
        !result.content().contains("sandbox-secret"),
        "credential was exposed"
    );
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox"]
fn linux_development_external_write_requires_approved_scope() {
    let root = TempRoot::new();
    let outside = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let file = outside.join(format!("nexus-sandbox-write-{}", std::process::id()));
    assert!(!file.exists());
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file]});
    let denied = executor.execute(
        &call(&args.to_string()),
        &context_with_scope("exec:workspace"),
    );
    assert_eq!(
        denied.status(),
        ExecutionStatus::Failed,
        "{}",
        denied.content()
    );
    assert!(!file.exists());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file], "write_dir":outside});
    let denied = executor.execute(
        &call(&args.to_string()),
        &context_with_scope("exec:workspace"),
    );
    assert_eq!(denied.status(), ExecutionStatus::Denied);
    let approved = context_with_scope(&format!("exec-directory:{}", outside.display()));
    let result = executor.execute(&call(&args.to_string()), &approved);
    let exists = file.exists();
    if exists {
        std::fs::remove_file(&file).unwrap();
    }
    assert_eq!(
        result.status(),
        ExecutionStatus::Succeeded,
        "{}",
        result.content()
    );
    assert!(exists);
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "requires an explicitly provisioned Linux sandbox"]
fn linux_sandbox_timeout_cleans_up_and_accepts_another_call() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let started = std::time::Instant::now();
    let context = context().with_control(
        nexus_core::CancellationToken::new(),
        started + Duration::from_millis(100),
    );
    let result = executor.execute(
        &call(r#"{"argv":["/bin/sh","-c","printf started; exec /bin/sleep 30"]}"#),
        &context,
    );
    assert_eq!(
        result.status(),
        ExecutionStatus::TimedOut,
        "{}",
        result.content()
    );
    assert_eq!(result.effect(), nexus_core::EffectState::Unknown);
    assert!(started.elapsed() < Duration::from_secs(2));
    let next = executor.execute(
        &call(r#"{"argv":["/usr/bin/printf","sandbox-ok"]}"#),
        &context_with_scope("command:/usr/bin/printf sandbox-ok"),
    );
    assert_eq!(
        next.status(),
        ExecutionStatus::Succeeded,
        "{}",
        next.content()
    );
    assert_eq!(next.content(), "sandbox-ok");
}

#[cfg(target_os = "macos")]
#[test]
fn expired_command_cleans_up_and_executor_accepts_the_next_call() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).expect("valid root binds");
    assert!(executor.sandbox_available());
    let started = std::time::Instant::now();
    let context = context().with_control(
        nexus_core::CancellationToken::new(),
        started + Duration::from_millis(100),
    );
    let result = executor.execute(
        &call(r#"{"argv":["/bin/sh","-c","printf started; exec /bin/sleep 30"]}"#),
        &context,
    );
    assert_eq!(
        result.status(),
        ExecutionStatus::TimedOut,
        "{}",
        result.content()
    );
    assert_eq!(result.effect(), nexus_core::EffectState::Unknown);
    assert!(started.elapsed() < Duration::from_secs(2));
    let next = executor.execute(
        &call(r#"{"argv":["/usr/bin/printf","sandbox-ok"]}"#),
        &context_with_scope("command:/usr/bin/printf sandbox-ok"),
    );
    assert_eq!(
        next.status(),
        ExecutionStatus::Succeeded,
        "{}",
        next.content()
    );
    assert_eq!(next.content(), "sandbox-ok");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[cfg_attr(
    target_os = "linux",
    ignore = "requires an explicitly provisioned Linux sandbox"
)]
#[test]
fn descendant_holding_output_pipe_does_not_extend_cleanup_unboundedly() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0).expect("valid root binds");
    assert!(executor.sandbox_available());
    let started = std::time::Instant::now();
    let result = executor.execute(
        &call(
            r#"{"argv":["/bin/sh","-c","(/usr/bin/touch spawned; /bin/sleep 5) & while [ ! -e spawned ]; do /bin/sleep 0.01; done; exit 0"]}"#,
        ),
        &context(),
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cleanup exceeded its bound"
    );
    assert_eq!(
        result.status(),
        ExecutionStatus::Succeeded,
        "{}",
        result.content()
    );
    assert!(
        root.0.join("spawned").exists(),
        "descendant handshake completed"
    );
    assert_eq!(result.effect(), nexus_core::EffectState::Unknown);
}

#[cfg(target_os = "macos")]
#[test]
fn mac_sandbox_allows_project_writes_but_denies_other_project_reads_and_writes() {
    let root = TempRoot::new();
    let outside = TempRoot::new();
    let outside_file = outside.0.join("private.txt");
    std::fs::write(&outside_file, "outside-secret").unwrap();
    let executor = SandboxedExecutor::with_root(&root.0).unwrap();
    assert!(executor.sandbox_available());

    let run = |argv: Vec<String>| {
        executor.execute(
            &call(&serde_json::json!({"argv": argv}).to_string()),
            &context(),
        )
    };
    let written = root.0.join("created.txt");
    let outcome = run(vec!["/usr/bin/touch".to_owned(), "created.txt".to_owned()]);
    assert_eq!(
        outcome.status(),
        ExecutionStatus::Succeeded,
        "{}",
        outcome.content()
    );
    assert!(written.exists());

    let outcome = run(vec![
        "/bin/cat".to_owned(),
        outside_file.to_string_lossy().into_owned(),
    ]);
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert!(!outcome.content().contains("outside-secret"));
    let refused = outside.0.join("refused.txt");
    let outcome = run(vec![
        "/usr/bin/touch".to_owned(),
        refused.to_string_lossy().into_owned(),
    ]);
    assert_eq!(outcome.status(), ExecutionStatus::Failed);
    assert!(!refused.exists());
}

#[cfg(target_os = "macos")]
#[test]
fn mac_sandbox_denies_network_access() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::with_root(&root.0)
        .unwrap()
        .with_network_access(false);
    assert!(executor.sandbox_available());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let args =
        serde_json::json!({"argv": ["/usr/bin/curl", "--noproxy", "*", "--max-time", "1", url]});
    let result = executor.execute(&call(&args.to_string()), &context());
    assert_eq!(result.status(), ExecutionStatus::Failed);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(target_os = "macos")]
#[test]
fn development_python_runs_with_stdlib_temp_files_and_protected_credentials() {
    let root = TempRoot::new();
    std::fs::write(root.0.join(".env"), "sandbox-secret").unwrap();
    let executor = SandboxedExecutor::development(&root.0)
        .unwrap()
        .with_network_access(false);
    assert!(executor.sandbox_available());
    let mut interpreters = 0;
    for python in ["python3", "python"] {
        if !std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|path| path.join(python).is_file())
        {
            continue;
        }
        interpreters += 1;
        let script = "import pathlib,tempfile,ssl,json,socket; pathlib.Path('ok.txt').write_text('ok'); f=tempfile.TemporaryFile(); f.write(b'ok'); f.close();\ntry: pathlib.Path('.env').read_text(); raise AssertionError('credential readable')\nexcept PermissionError: pass\ntry: s=socket.socket(); s.connect(('127.0.0.1',9)); raise AssertionError('network permitted')\nexcept PermissionError: pass\nprint('python-ok')";
        let result = executor.execute(
            &call(&serde_json::json!({"argv":[python,"-c",script]}).to_string()),
            &context_with_scope("exec:workspace"),
        );
        assert_eq!(
            result.status(),
            ExecutionStatus::Succeeded,
            "{python}: {}",
            result.content()
        );
        assert!(result.content().contains("python-ok"));
    }
    assert!(
        interpreters > 0,
        "Python regression requires an installed interpreter"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn development_sandbox_allows_dev_null_but_not_other_device_writes() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(executor.sandbox_available());
    let context = context_with_scope("exec:workspace");

    let null = executor.execute(&call(r#"{"argv":["/usr/bin/tee","/dev/null"]}"#), &context);
    assert_eq!(
        null.status(),
        ExecutionStatus::Succeeded,
        "{}",
        null.content()
    );

    let zero = executor.execute(&call(r#"{"argv":["/usr/bin/tee","/dev/zero"]}"#), &context);
    assert_eq!(zero.status(), ExecutionStatus::Failed, "{}", zero.content());
}

#[cfg(target_os = "macos")]
#[test]
fn development_external_writes_require_a_bound_declared_directory() {
    let root = TempRoot::new();
    let executor = SandboxedExecutor::development(&root.0).unwrap();
    assert!(executor.sandbox_available());
    // A project outside both /tmp and the process temp directory.
    let outside = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .unwrap();
    let file = outside.join(format!("nexus-sandbox-write-{}", std::process::id()));
    assert!(!file.exists());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file]});
    let workspace_context = context_with_scope("exec:workspace");
    let refused = executor.execute(&call(&args.to_string()), &workspace_context);
    assert_eq!(
        refused.status(),
        ExecutionStatus::Failed,
        "{}",
        refused.content()
    );
    assert!(!file.exists());
    let args = serde_json::json!({"argv":["/usr/bin/touch",file], "write_dir":outside});
    let refused = executor.execute(&call(&args.to_string()), &workspace_context);
    assert_eq!(refused.status(), ExecutionStatus::Denied);
    let context = ToolContext::new(
        4096,
        Duration::ZERO,
        false,
        ApprovedScope::new(format!("exec-directory:{}", outside.display())).unwrap(),
    )
    .unwrap();
    let allowed = executor.execute(&call(&args.to_string()), &context);
    let exists = file.exists();
    if exists {
        std::fs::remove_file(&file).unwrap();
    }
    assert_eq!(
        allowed.status(),
        ExecutionStatus::Succeeded,
        "{}",
        allowed.content()
    );
    assert!(exists);
}
