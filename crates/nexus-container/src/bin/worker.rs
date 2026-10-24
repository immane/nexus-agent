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
    // stdin framing is blocking: cancellation requires the supervising parent
    // to terminate this process. The tool deadline is cooperative once execute
    // starts; this worker is not a complete container backend.
    let request = read_request(&mut input)?;
    validate_workspace_paths(&request.operation, &request.arguments)?;
    let port: Arc<dyn ToolPort + Send + Sync> = match request.operation.as_str() {
        "host_read" => Arc::new(ScopedReader::with_root(Path::new("/workspace"))?),
        "host_list" => Arc::new(ScopedLister::with_root(Path::new("/workspace"))?),
        "host_search" => Arc::new(ScopedSearcher::with_root(Path::new("/workspace"))?),
        "host_write" => Arc::new(ScopedWriter::with_shared_workspace_root(Path::new(
            "/workspace",
        ))?),
        "host_patch" => Arc::new(ScopedPatcher::with_shared_workspace_root(Path::new(
            "/workspace",
        ))?),
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
        ApprovedScope::new("/workspace")?,
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
