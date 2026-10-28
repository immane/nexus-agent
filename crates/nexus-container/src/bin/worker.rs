#![forbid(unsafe_code)]

use std::io::{self, BufReader, BufWriter};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_container::worker_protocol::{read_request, validate_workspace_paths, write_response};
use nexus_core::{
    ApprovedScope, CallId, CancellationToken, NormalizedArgs, RunId, ToolCall, ToolContext, ToolId,
    ToolPort, TurnId,
};
use nexus_tools::{ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter};

fn main() {
    let input = io::stdin();
    let output = io::stdout();
    let result = run(BufReader::new(input.lock()), BufWriter::new(output.lock()));
    if result.is_err() {
        std::process::exit(2);
    }
}

fn run<R: io::Read, W: io::Write>(
    mut input: R,
    mut output: W,
) -> Result<(), Box<dyn std::error::Error>> {
    run_with_root(&mut input, &mut output, Path::new("/workspace"))
}

fn run_with_root<R: io::Read, W: io::Write>(
    mut input: R,
    mut output: W,
    root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    // stdin framing is blocking: cancellation requires the supervising parent
    // to terminate this process. The tool deadline is cooperative once execute
    // starts; this worker is not a complete container backend.
    let request = read_request(&mut input)?;
    validate_workspace_paths(&request.operation, &request.arguments)?;
    let port: Arc<dyn ToolPort + Send + Sync> = match request.operation.as_str() {
        "host_read" => Arc::new(ScopedReader::with_root(root)?),
        "host_list" => Arc::new(ScopedLister::with_root(root)?.with_settings(&request.tools)?),
        "host_search" => Arc::new(ScopedSearcher::with_root(root)?.with_settings(&request.tools)?),
        "host_write" => Arc::new(ScopedWriter::with_shared_workspace_root(root)?),
        "host_patch" => Arc::new(ScopedPatcher::with_shared_workspace_root(root)?),
        _ => return Err("unknown operation".into()),
    };
    let spec = port.describe();
    let call = ToolCall::new(
        RunId::new("container-worker").map_err(|_| "invalid worker identity")?,
        TurnId::new("container-worker").map_err(|_| "invalid worker identity")?,
        CallId::new("container-worker").map_err(|_| "invalid worker identity")?,
        ToolId::new(spec.id().name(), spec.id().revision()).map_err(|_| "invalid tool identity")?,
        NormalizedArgs::new(request.arguments.to_string())?,
    );
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(10))
        .ok_or("worker deadline overflow")?;
    let context = ToolContext::new(
        request.output_limit,
        Duration::from_secs(10),
        false,
        ApprovedScope::new(root.to_string_lossy().into_owned())?,
    )?
    .with_control(CancellationToken::new(), deadline);
    let outcome = port
        .execute(&call, &context)
        .enforce_budget(request.output_limit)?;
    write_response(
        &mut output,
        serde_json::json!({
            "status": format!("{:?}", outcome.status()),
            "effect": format!("{:?}", outcome.effect()),
            "content": outcome.content(),
            "truncated": outcome.is_truncated(),
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_config::ToolsSettings;
    use serde_json::{Value, json};

    #[test]
    fn worker_enforces_reduced_list_and_search_budgets_without_docker() {
        let directory = std::env::temp_dir().join(format!(
            "nexus-worker-budgets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let root = directory.canonicalize().unwrap();
        std::fs::write(root.join("a"), "early\nlate\n").unwrap();
        std::fs::write(root.join("b"), "other\n").unwrap();
        let execute = |operation: &str, arguments: Value, settings: &ToolsSettings| {
            let wire = json!({"version":2,"operation":operation,"arguments":arguments,"output_limit":4096,"tools":settings});
            let mut output = Vec::new();
            run_with_root(wire.to_string().as_bytes(), &mut output, &root).unwrap();
            serde_json::from_slice::<Value>(&output).unwrap()["result"].clone()
        };
        let reduced = ToolsSettings {
            list_entry_limit: 1,
            search_file_bytes: 6,
        };
        let list = execute("host_list", json!({"path":"."}), &reduced);
        assert_eq!(list["status"], "Succeeded");
        assert_eq!(list["content"].as_str().unwrap().lines().count(), 1);
        assert_eq!(list["truncated"], true);
        let full = execute("host_list", json!({"path":"."}), &ToolsSettings::default());
        assert_eq!(full["content"].as_str().unwrap().lines().count(), 2);
        let prefix = execute("host_search", json!({"path":".","query":"early"}), &reduced);
        assert_eq!(prefix["status"], "Succeeded");
        assert!(prefix["content"].as_str().unwrap().contains("early"));
        let tail = execute("host_search", json!({"path":".","query":"late"}), &reduced);
        assert_eq!(tail["content"], "");
        assert_eq!(tail["truncated"], true);
        let full = execute(
            "host_search",
            json!({"path":".","query":"late"}),
            &ToolsSettings::default(),
        );
        assert!(full["content"].as_str().unwrap().contains("late"));
        std::fs::remove_file(root.join("a")).unwrap();
        std::fs::remove_file(root.join("b")).unwrap();
        std::fs::remove_dir(&directory).unwrap();
    }
}
