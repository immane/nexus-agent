#![forbid(unsafe_code)]

//! Project catalog over the real loopback transport: canonical path
//! admission, persistence, per-session binding, and fake-mode refusal.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nexus_server::Server;
use nexus_server::projects::ProjectCatalog;
use serde_json::Value;

const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn address(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

fn round_trip(port: u16, raw: &str) -> (u16, Value) {
    let mut stream = TcpStream::connect(address(port)).expect("loopback connects");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout sets");
    stream.write_all(raw.as_bytes()).expect("request writes");
    let mut body = Vec::new();
    stream.read_to_end(&mut body).expect("response reads");
    let text = String::from_utf8_lossy(&body);
    let (head, payload) = text.split_once("\r\n\r\n").expect("framed response");
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("status line")
        .parse()
        .expect("numeric status");
    (
        status,
        serde_json::from_str(payload).expect("JSON response body"),
    )
}

fn get(port: u16, path: &str, authenticated: bool) -> (u16, Value) {
    let auth = if authenticated {
        format!("authorization: Bearer {TOKEN}\r\n")
    } else {
        String::new()
    };
    round_trip(
        port,
        &format!("GET {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\n{auth}\r\n"),
    )
}

fn post(port: u16, path: &str, body: &str) -> (u16, Value) {
    round_trip(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nauthorization: Bearer {TOKEN}\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn delete(port: u16, path: &str) -> (u16, Value) {
    round_trip(
        port,
        &format!(
            "DELETE {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nauthorization: Bearer {TOKEN}\r\n\r\n"
        ),
    )
}

fn temp_root(tag: &str) -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("nexus-project-flow-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp root exists");
    path
}

fn spawn(server: Server) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("ephemeral port binds");
    let port = listener.local_addr().expect("port known").port();
    let server = Arc::new(server);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let server = Arc::clone(&server);
            std::thread::spawn(move || server.handle_connection(stream));
        }
    });
    port
}

fn development_server(root: &Path, catalog_path: Option<PathBuf>) -> Server {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let mut server = Server::new(runtime.handle().clone());
    std::mem::forget(runtime);
    server
        .set_tools_mode(nexus_server::ToolsMode::Development {
            root: root.to_path_buf(),
        })
        .expect("development root is valid");
    server.set_auth_token(TOKEN).expect("token validates");
    server.set_projects(ProjectCatalog::default(), catalog_path);
    server
}

fn fake_server() -> Server {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let mut server = Server::new(runtime.handle().clone());
    std::mem::forget(runtime);
    server.set_auth_token(TOKEN).expect("token validates");
    server
}

#[test]
fn projects_are_validated_persisted_and_bound_to_sessions() {
    let root = temp_root("catalog");
    let extra = root.join("extra");
    let second = root.join("second");
    std::fs::create_dir_all(&extra).expect("extra root exists");
    std::fs::create_dir_all(&second).expect("second root exists");
    let catalog_path = root.join("projects.json");
    let port = spawn(development_server(&root, Some(catalog_path.clone())));

    let (status, body) = get(port, "/projects", false);
    assert_eq!(status, 401, "projects require authentication: {body}");

    let (status, body) = get(port, "/projects", true);
    assert_eq!(status, 200, "projects are reported: {body}");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["mode"], "development");
    assert_eq!(body["default"]["id"], "default");
    assert_eq!(
        body["default"]["path"].as_str().expect("default path"),
        std::fs::canonicalize(&root).unwrap().to_str().unwrap()
    );
    assert_eq!(body["projects"].as_array().unwrap().len(), 0);
    assert_eq!(body["projects"].as_array().unwrap().len(), 0);

    let add = format!(
        r#"{{"name":"Extra workspace","path":{:?}}}"#,
        std::fs::canonicalize(&extra).unwrap().to_str().unwrap()
    );
    let (status, body) = post(port, "/projects", &add);
    assert_eq!(status, 201, "project admits: {body}");
    let project = body["project"]["id"]
        .as_str()
        .expect("project id")
        .to_owned();
    assert_eq!(project, "project-1");
    assert_eq!(body["projects"].as_array().unwrap().len(), 1);
    assert_eq!(body["projects"][0]["name"], "Extra workspace");

    let (status, body) = post(port, "/projects", &add);
    assert_eq!(status, 409, "duplicate canonical path is refused: {body}");
    let default_duplicate = format!(
        r#"{{"name":"Startup again","path":{:?}}}"#,
        std::fs::canonicalize(&root).unwrap().to_str().unwrap()
    );
    let (status, body) = post(port, "/projects", &default_duplicate);
    assert_eq!(
        status, 409,
        "the startup workspace cannot be registered twice: {body}"
    );
    let (status, body) = post(port, "/projects", r#"{"name":"Root","path":"/"}"#);
    assert_eq!(status, 400, "the filesystem root is refused: {body}");
    let (status, body) = post(
        port,
        "/projects",
        r#"{"name":"Relative","path":"relative/path"}"#,
    );
    assert_eq!(status, 400, "relative paths are refused: {body}");

    let persisted = ProjectCatalog::load(&catalog_path)
        .expect("catalog loads")
        .expect("catalog exists");
    assert_eq!(persisted.projects().len(), 1);
    assert_eq!(persisted.projects()[0].id, project);

    let (status, body) = post(port, "/sessions", &format!(r#"{{"project":{project:?}}}"#));
    assert_eq!(status, 201, "a session binds the project: {body}");
    assert_eq!(body["project"], project);
    assert!(body["session"].is_string());

    let (status, body) = delete(port, &format!("/projects/{project}"));
    assert_eq!(status, 409, "an in-use project cannot be removed: {body}");
    let (status, body) = delete(port, "/projects/default");
    assert_eq!(status, 400, "the startup project is reserved: {body}");

    let second_add = format!(
        r#"{{"name":"Second","path":{:?}}}"#,
        std::fs::canonicalize(&second).unwrap().to_str().unwrap()
    );
    let (status, body) = post(port, "/projects", &second_add);
    assert_eq!(status, 201, "second project admits: {body}");
    let second_id = body["project"]["id"]
        .as_str()
        .expect("project id")
        .to_owned();
    let (status, body) = delete(port, &format!("/projects/{second_id}"));
    assert_eq!(status, 200, "an unused project is removed: {body}");
    assert_eq!(body["projects"].as_array().unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn fake_or_container_modes_refuse_projects_without_exposing_paths() {
    let port = spawn(fake_server());
    let (status, body) = get(port, "/projects", true);
    assert_eq!(status, 200, "fake mode reports availability: {body}");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["mode"], "fakes");
    assert!(body.get("default").is_none());
    assert_eq!(body["projects"].as_array().unwrap().len(), 0);

    let (status, body) = post(port, "/projects", r#"{"name":"Nope","path":"/tmp"}"#);
    assert_eq!(status, 409, "fake mode refuses additions: {body}");
    let (status, body) = post(port, "/sessions", r#"{"project":"default"}"#);
    assert_eq!(status, 400, "fake mode refuses session binding: {body}");
    assert!(body["error"].is_string());
}

#[test]
fn configuration_and_project_writes_require_configured_authentication() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("test executor builds");
    let server = Server::new(runtime.handle().clone());
    std::mem::forget(runtime);
    let port = spawn(server);

    let (status, body) = post(port, "/projects", r#"{"name":"Nope","path":"/tmp"}"#);
    assert_eq!(
        status, 503,
        "project writes require configured auth: {body}"
    );
    let (status, body) = post(
        port,
        "/config/providers",
        r#"{"id":"p","display_name":"P","adapter":"direct","protocol":"chat_completions","endpoint":null,"credential":{"env":"KEY"},"default_model":"m"}"#,
    );
    assert_eq!(
        status, 503,
        "provider writes require configured auth: {body}"
    );
    let (status, body) = post(
        port,
        "/config/models",
        r#"{"id":"m","provider":"p","name":"m"}"#,
    );
    assert_eq!(status, 503, "model writes require configured auth: {body}");
    let (status, body) = get(port, "/projects", false);
    assert_eq!(
        status, 200,
        "the read-only project summary remains public: {body}"
    );
    assert_eq!(body["enabled"], false);
}
