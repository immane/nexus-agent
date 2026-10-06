#![forbid(unsafe_code)]

//! M0 headless composition root: fake-wired, test-only.
//!
//! The binary owns the M0 composition root for headless mode: it builds the
//! runtime with a scripted [`FakeProvider`] plus [`FakeTool`] set, submits one task taken from
//! argv, drains both event channels to the terminal [`RunOutcome`], and
//! exits. No approval handler is configured, so confirmation-required calls
//! exercise the denial path (denied, never executed, never hung).
//!
//! Output contract (unstable test-only revision `m0-test-0`, see
//! [`OUTPUT_REV`]): stdout carries ONLY machine-readable event/result lines,
//! one record per line, each starting with `rev=m0-test-0 `. Fields are
//! space-separated `key=value` pairs. Every value is percent-encoded over
//! its UTF-8 bytes (see [`sanitize`]): bytes outside the RFC 3986
//! unreserved set `A-Za-z0-9-._~` become `%XX` with uppercase hex, so no
//! raw space, `=`, newline, or ESC byte can appear inside a value and every
//! `%` starts a complete escape. Decode a record by splitting it on spaces,
//! splitting each field at its first `=`, and calling [`percent_decode`] on
//! the value. Encoding is reversible for any UTF-8 text; malformed escapes
//! and non-UTF-8 byte sequences are rejected by the decoder. No `Debug`
//! formatting is used. Event lines are sorted by per-run sequence before
//! printing. Diagnostics and [`FAKE_BANNER`] go to stderr only.
//!
//! Report retention separates mandatory control records (`RunStarted`,
//! approvals, tool lifecycle, usage, terminal) from presentation data (text,
//! previews, progress). Mandatory records are never dropped because
//! presentation filled the budget: presentation lines are bounded by
//! [`MAX_REPORT_EVENTS`] and the exact encoded bytes left under
//! [`MAX_REPORT_BYTES`], and the oldest presentation lines are evicted first
//! when mandatory lines need the space. If mandatory records exceed their own
//! finite reserve ([`MAX_MANDATORY_EVENTS`]/[`MAX_MANDATORY_BYTES`]), the
//! report is refused with an explicit operation error instead of dropping a
//! mandatory record. Every line is formatted on receipt and memory is exactly
//! the sum of the retained encoded lines, including approval args previews
//! and error correlation diagnostics. Dropped presentation sets
//! [`HeadlessReport::truncated`]; counters still describe every observed
//! event.
//!
//! Lifecycle bounds: the consume watchdog is the runtime's remaining
//! run-duration budget plus [`WATCHDOG_SLACK`]. If it expires, the library
//! requests runtime cancellation before dropping the driver and reconciles
//! within [`RECONCILE_TIMEOUT`], so an error path never skips the requested
//! cancellation or waits forever. Runtime teardown waits at most
//! [`SHUTDOWN_TIMEOUT`] for blocking workers; that only stops waiting, it
//! does not kill work, and the runtime owner quarantines workers that
//! outlive the wait.
//!
//! Task routing (test-only stand-in, not a product default): the task string
//! is submitted verbatim as the run input; the fake script is picked by
//! prefix: `deny: ...` proposes a `host_write` call (denial path), `refuse:
//! ...` refuses, anything else answers with a `host_read` call plus text
//! (completed path).
//!
//! Exit-code mapping: 0 = completed with no denied calls; 3 = completed run
//! containing denied calls (headless denial); 1 = failed or refused run, or
//! an operational entry-point failure (runtime setup, rejected submit,
//! watchdog expiry, closed channel); 4 = cancelled; 5 = limit reached;
//! 2 = usage error (missing, empty, oversize, or non-UTF-8 argv input);
//! 6 = the stdout consumer closed early (broken pipe), so the process
//! stopped writing deliberately instead of panicking. A denied tool call
//! surfaces as `Completed` at run level with a `Denied`/`NotStarted` tool
//! outcome, hence the distinct code 3.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::commands::EventSequence;
use nexus_core::{
    CancelCommand, CommandReply, CorrelationData, EventPayload, Limits, RequestId, RunEvent,
    RunFinished, RunId, RunOutcome, SessionId, SubmitCommand,
};
use nexus_fakes::{FakeProvider, FakeTool, candidate, stop_turn, tool_turn};
use nexus_runtime::{EventStreams, Policy, Runtime, RuntimeConfig};

/// Unstable test-only output revision. Parsers must exact-match this.
pub const OUTPUT_REV: &str = "m0-test-0";

/// Self-identifying banner, printed to stderr (never stdout). A missing real
/// configuration must never look like a real successful task.
pub const FAKE_BANNER: &str = "nexus-headless: FAKE TEST-ONLY wiring rev m0-test-0 (scripted FakeProvider + FakeTool, ephemeral, NO approval handler; confirmation-required calls are denied). This is never a real task run.";

/// Usage hint for stderr diagnostics.
pub const USAGE: &str = "usage: nexus-headless [--mode plan|build|<custom>] <task>; prefix with 'deny:' to exercise headless denial, 'refuse:' for refusal";

/// Exit code for a stdout consumer that closed early (broken pipe). The
/// report is incomplete by construction, so this is deliberately distinct
/// from both success and the usage/operation error codes.
pub const EXIT_STDOUT_CLOSED: i32 = 6;

/// Extra wait beyond the runtime's own remaining run-duration budget before
/// the consume watchdog gives up. The runtime enforces its deadline; the
/// slack only covers scheduling and shutdown, so the watchdog can never
/// truncate a run the runtime still considers live.
pub const WATCHDOG_SLACK: Duration = Duration::from_secs(5);

/// Bounded post-cancel reconciliation window. After a watchdog expiry or a
/// closed control channel, the headless driver requests runtime cancellation
/// and waits at most this long (for the cancel request and again for the
/// terminal record) before reporting the error. The error path never skips
/// the requested cancellation and never waits indefinitely.
pub const RECONCILE_TIMEOUT: Duration = Duration::from_secs(1);

/// Finite wait for blocking workers during runtime teardown. Stopping the
/// wait does not cancel or kill the worker; it only stops waiting. Workers
/// that outlive the wait are quarantined by the runtime owner, and process
/// exit must not block on arbitrary blocking code.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// M0-TEST: maximum presentation records retained for one
/// [`HeadlessReport`]. Extra presentation records are counted but not
/// formatted. Mandatory control records use [`MAX_MANDATORY_EVENTS`]
/// instead and never consume this budget.
pub const MAX_REPORT_EVENTS: usize = 1_024;

/// M0-TEST: exact encoded byte budget shared by presentation and mandatory
/// lines. Presentation retention uses whatever remains after mandatory
/// lines; the oldest presentation lines are evicted first when mandatory
/// lines need space. Mandatory lines alone are bounded by
/// [`MAX_MANDATORY_BYTES`].
pub const MAX_REPORT_BYTES: usize = 262_144;

/// M0-TEST: reserved count for mandatory control records (outcomes and the
/// terminal event). Presentation pressure can never consume this reserve.
pub const MAX_MANDATORY_EVENTS: usize = 256;

/// M0-TEST: finite encoded-byte budget for mandatory control lines,
/// measured on the exact formatted line. It covers the M0 worst case (64
/// tool-finished payloads at the 262,144-byte output budget, percent-encoded
/// at three characters per byte) with headroom. Exceeding it refuses the
/// report with an explicit operation error, never a silent drop.
pub const MAX_MANDATORY_BYTES: usize = 64 * 1024 * 1024;

/// Terminal report for one headless task.
pub struct HeadlessReport {
    /// Host-issued run id, for stderr diagnostics.
    pub run: String,
    /// Stdout lines: sequenced event lines plus one trailing result line.
    pub lines: Vec<String>,
    /// Terminal run outcome.
    pub outcome: RunOutcome,
    /// Tool-finished outcomes with `Denied` status.
    pub denied_calls: usize,
    /// `ToolStarted` events observed (must be 0 on the denial path).
    pub tool_started: usize,
    /// `ApprovalRequired` events observed (must be 0 on the denial path).
    pub approval_required: usize,
    /// `ToolFinished` events observed.
    pub tool_finished: usize,
    /// True when presentation records were dropped or evicted to keep the
    /// report within [`MAX_REPORT_EVENTS`]/[`MAX_REPORT_BYTES`]. Mandatory
    /// control records are never dropped; exceeding their reserve refuses
    /// the report with [`HeadlessError::Operation`] instead. The counters
    /// and outcome still describe every observed event.
    pub truncated: bool,
    /// Process exit code per the mapping documented above.
    pub exit_code: i32,
}

/// Entry-point failure with two deliberately distinct classes so the caller
/// can map usage mistakes to exit code 2 and operational failures to exit
/// code 1. Messages are static or runtime-generated but never echo raw argv
/// bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessError {
    /// The caller supplied invalid input (empty, oversize, or non-UTF-8).
    Usage(String),
    /// The run was accepted but did not complete operationally (runtime
    /// setup, rejected submit, watchdog expiry, closed channel).
    Operation(String),
}

impl HeadlessError {
    /// Process exit code for usage mistakes.
    pub const USAGE_EXIT_CODE: i32 = 2;
    /// Process exit code for operational failures.
    pub const OPERATION_EXIT_CODE: i32 = 1;

    /// Returns true when the failure is a usage mistake (exit code 2).
    #[must_use]
    pub fn is_usage(&self) -> bool {
        matches!(self, Self::Usage(_))
    }

    /// Returns the diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Usage(message) | Self::Operation(message) => message,
        }
    }

    /// Returns the process exit code: 2 for usage, 1 for operation.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => Self::USAGE_EXIT_CODE,
            Self::Operation(_) => Self::OPERATION_EXIT_CODE,
        }
    }
}

impl fmt::Display for HeadlessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for HeadlessError {}

/// Maps a terminal outcome (plus denial count) to a process exit code.
#[must_use]
pub fn exit_code_for(outcome: RunOutcome, denied_calls: usize) -> i32 {
    match outcome {
        RunOutcome::Completed if denied_calls == 0 => 0,
        RunOutcome::Completed => 3,
        RunOutcome::Failed | RunOutcome::Refused => 1,
        RunOutcome::Cancelled => 4,
        RunOutcome::LimitReached => 5,
    }
}

/// Lowercase wire name for a run outcome (no `Debug` formatting).
#[must_use]
pub fn outcome_name(outcome: RunOutcome) -> &'static str {
    match outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::Refused => "refused",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
        RunOutcome::LimitReached => "limit-reached",
    }
}

/// Reversible machine encoding for free text in `key=value` stdout lines.
///
/// Encodes the UTF-8 bytes of `raw`: bytes in the RFC 3986 unreserved set
/// `A-Z a-z 0-9 - . _ ~` pass through unchanged, and every other byte is
/// written as `%XX` with uppercase hexadecimal digits. The result is ASCII
/// with no raw space, `=`, newline, or ESC byte; every `%` introduces a
/// complete two-digit escape, and [`percent_decode`] restores the original
/// text losslessly. Percent encoding a byte can expand it to three
/// characters.
#[must_use]
pub fn sanitize(raw: &str) -> String {
    percent_encode(raw.as_bytes())
}

/// Failure modes of [`percent_decode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PercentDecodeError {
    /// A `%` was not followed by exactly two hexadecimal digits.
    MalformedEscape,
    /// The decoded byte sequence is not valid UTF-8.
    InvalidUtf8,
}

impl fmt::Display for PercentDecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedEscape => formatter.write_str("malformed percent escape"),
            Self::InvalidUtf8 => formatter.write_str("decoded bytes are not valid UTF-8"),
        }
    }
}

impl std::error::Error for PercentDecodeError {}

/// Decodes the machine encoding produced by [`sanitize`]. Bytes that are not
/// `%` escapes pass through unchanged, so the decoder also accepts plain
/// ASCII values. Escapes are accepted in either hex case and validated as
/// UTF-8; a malformed escape or invalid byte sequence is rejected.
pub fn percent_decode(encoded: &str) -> Result<String, PercentDecodeError> {
    let decoded = percent_decode_bytes(encoded.as_bytes())?;
    String::from_utf8(decoded).map_err(|_| PercentDecodeError::InvalidUtf8)
}

/// Writes every report line followed by a newline, then flushes. The caller
/// owns failure mapping; [`std::io::ErrorKind::BrokenPipe`] must not be
/// turned into a panic or a silently swallowed success.
pub fn write_report_lines<W: Write>(writer: &mut W, lines: &[String]) -> std::io::Result<()> {
    for line in lines {
        writeln!(writer, "{line}")?;
    }
    writer.flush()
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if is_unreserved(byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(hex_digit(byte >> 4));
            encoded.push(hex_digit(byte & 0x0F));
        }
    }
    encoded
}

fn percent_decode_bytes(bytes: &[u8]) -> Result<Vec<u8>, PercentDecodeError> {
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let high = bytes
                .get(index + 1)
                .copied()
                .and_then(hex_value)
                .ok_or(PercentDecodeError::MalformedEscape)?;
            let low = bytes
                .get(index + 2)
                .copied()
                .and_then(hex_value)
                .ok_or(PercentDecodeError::MalformedEscape)?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(byte);
            index += 1;
        }
    }
    Ok(decoded)
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn hex_digit(nibble: u8) -> char {
    char::from(b"0123456789ABCDEF"[usize::from(nibble & 0x0F)])
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Agent mode for one headless invocation: a validated name plus the tool
/// policy the runtime enforces. `plan`/`build` are built-in; custom names
/// resolve through user configuration by the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessMode {
    /// Mode identity echoed in diagnostics (id charset only).
    pub name: String,
    /// True when confirmation-required tools must be denied without
    /// prompting.
    pub read_only: bool,
}

impl HeadlessMode {
    /// Built-in full-capability mode (the default).
    #[must_use]
    pub fn build() -> Self {
        Self {
            name: "build".to_owned(),
            read_only: false,
        }
    }

    /// Built-in read-only planning mode.
    #[must_use]
    pub fn plan() -> Self {
        Self {
            name: "plan".to_owned(),
            read_only: true,
        }
    }

    /// Custom mode with an explicit policy bit.
    #[must_use]
    pub fn custom(name: impl Into<String>, read_only: bool) -> Self {
        Self {
            name: name.into(),
            read_only,
        }
    }
}

/// Submits `task`, drains both event channels to the terminal outcome, and
/// builds the stdout lines plus exit code. Invalid input returns
/// [`HeadlessError::Usage`]; operational failures return
/// [`HeadlessError::Operation`].
pub fn run_task(task: &str) -> Result<HeadlessReport, HeadlessError> {
    run_task_with_mode(task, &HeadlessMode::build())
}

/// [`run_task`] in an explicit agent mode. The mode's `read_only` bit rides
/// the submit into the runtime, which denies confirmation-required tools
/// without prompting; automatic reads still execute.
pub fn run_task_with_mode(
    task: &str,
    mode: &HeadlessMode,
) -> Result<HeadlessReport, HeadlessError> {
    block_on_with_shutdown(run_task_async(task, mode), SHUTDOWN_TIMEOUT)?
}

/// Runs one future on a fresh current-thread runtime, then stops waiting for
/// blocking workers after `shutdown`. The future's result is captured before
/// teardown and returned unchanged. `shutdown_timeout` does not kill or
/// cancel arbitrary blocking code; it only stops waiting, and any worker
/// that outlives the wait is quarantined by the runtime owner instead of
/// holding process exit hostage.
fn block_on_with_shutdown<F: Future>(
    future: F,
    shutdown: Duration,
) -> Result<F::Output, HeadlessError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| HeadlessError::Operation(format!("test runtime failed: {error}")))?;
    let result = runtime.block_on(future);
    runtime.shutdown_timeout(shutdown);
    Ok(result)
}

/// Counts every observed event and retains a bounded, exactly-sized subset
/// of formatted lines for the report.
struct RunConsumer {
    retention: Retention,
    tool_started: usize,
    approval_required: usize,
    tool_finished: usize,
    denied_calls: usize,
}

impl RunConsumer {
    fn new() -> Self {
        Self {
            retention: Retention::new(),
            tool_started: 0,
            approval_required: 0,
            tool_finished: 0,
            denied_calls: 0,
        }
    }

    fn observe(&mut self, event: RunEvent) {
        match event.payload() {
            EventPayload::ToolStarted(_) => self.tool_started += 1,
            EventPayload::ApprovalRequired(_) => self.approval_required += 1,
            EventPayload::ToolFinished(info) => {
                self.tool_finished += 1;
                if info.outcome.status() == nexus_core::ExecutionStatus::Denied {
                    self.denied_calls += 1;
                }
            }
            _ => {}
        }
        self.retention.retain(event);
    }
}

/// One formatted stdout record with its per-run sequence for final ordering.
struct RetainedLine {
    seq: EventSequence,
    line: String,
}

/// Retention separates mandatory control records from presentation data.
///
/// Mandatory records (`RunStarted`, approvals, tool lifecycle, usage,
/// terminal) are never dropped for presentation pressure: they are bounded
/// only by [`MAX_MANDATORY_EVENTS`]/[`MAX_MANDATORY_BYTES`], and exceeding
/// that finite reserve sets `over_limit` so the caller can refuse the report
/// explicitly instead of dropping a record. Presentation records are bounded
/// by [`MAX_REPORT_EVENTS`] and the exact encoded bytes left under
/// [`MAX_REPORT_BYTES`]; the oldest presentation lines are evicted first
/// when mandatory lines need the space. Formatting happens on receipt, so
/// memory is exactly the sum of the retained encoded lines, including
/// approval args previews and error correlation diagnostics.
struct Retention {
    mandatory: Vec<RetainedLine>,
    mandatory_bytes: usize,
    presentation: VecDeque<RetainedLine>,
    presentation_bytes: usize,
    over_limit: bool,
    truncated: bool,
}

impl Retention {
    fn new() -> Self {
        Self {
            mandatory: Vec::new(),
            mandatory_bytes: 0,
            presentation: VecDeque::new(),
            presentation_bytes: 0,
            over_limit: false,
            truncated: false,
        }
    }

    fn retain(&mut self, event: RunEvent) {
        if self.over_limit {
            return;
        }
        let record = RetainedLine {
            seq: event.seq(),
            line: format_event(&event),
        };
        if is_mandatory(event.payload()) {
            self.retain_mandatory(record);
        } else {
            self.retain_presentation(record);
        }
    }

    fn retain_mandatory(&mut self, record: RetainedLine) {
        if self.mandatory.len() >= MAX_MANDATORY_EVENTS
            || self.mandatory_bytes.saturating_add(record.line.len()) > MAX_MANDATORY_BYTES
        {
            // Mandatory records are never silently dropped; the caller turns
            // this explicit limit into an operation error.
            self.over_limit = true;
            return;
        }
        self.mandatory_bytes += record.line.len();
        self.mandatory.push(record);
        self.evict_presentation_to_fit();
    }

    fn retain_presentation(&mut self, record: RetainedLine) {
        let budget = MAX_REPORT_BYTES.saturating_sub(self.mandatory_bytes);
        if self.presentation.len() >= MAX_REPORT_EVENTS
            || self.presentation_bytes.saturating_add(record.line.len()) > budget
        {
            self.truncated = true;
            return;
        }
        self.presentation_bytes += record.line.len();
        self.presentation.push_back(record);
    }

    /// Evicts the oldest presentation records until the combined encoded
    /// size fits the report budget. Mandatory records are never evicted.
    fn evict_presentation_to_fit(&mut self) {
        while self.presentation_bytes.saturating_add(self.mandatory_bytes) > MAX_REPORT_BYTES {
            let Some(oldest) = self.presentation.pop_front() else {
                break;
            };
            self.presentation_bytes = self.presentation_bytes.saturating_sub(oldest.line.len());
            self.truncated = true;
        }
    }
}

/// Mirrors the runtime transport's control classification: these records are
/// mandatory report content and must never be lost to presentation pressure.
fn is_mandatory(payload: &EventPayload) -> bool {
    matches!(
        payload,
        EventPayload::RunStarted { .. }
            | EventPayload::ApprovalRequired(_)
            | EventPayload::ToolStarted(_)
            | EventPayload::ToolFinished(_)
            | EventPayload::UsageUpdated(_)
            | EventPayload::RunFinished(_)
    )
}

/// Consume watchdog: the runtime's own remaining run-duration budget plus
/// [`WATCHDOG_SLACK`]. It must never cut a run short of the runtime deadline.
fn watchdog_budget(remaining: Duration) -> Duration {
    remaining.saturating_add(WATCHDOG_SLACK)
}

/// Builds the M0-test tool set; [`Runtime::try_new`] validates every
/// registration (revision, duplicate names, schema) before a run can start.
fn m0_tools() -> Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> {
    vec![
        Arc::new(FakeTool::read_only()),
        Arc::new(FakeTool::mutation()),
        Arc::new(FakeTool::command()),
    ]
}

/// Builds the runtime through the fallible constructor so invalid
/// registration is a normal [`HeadlessError::Operation`], never a panic.
fn build_runtime(
    limits: Limits,
    provider: Arc<dyn nexus_core::ProviderPort + Send + Sync>,
    tools: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>>,
) -> Result<(Runtime, EventStreams), HeadlessError> {
    Runtime::try_new(
        RuntimeConfig {
            limits,
            policy: Policy::m0_test(),
            has_approval_handler: false,
        },
        provider,
        tools,
    )
    .map_err(|error| HeadlessError::Operation(format!("runtime registration failed: {error}")))
}

async fn run_task_async(task: &str, mode: &HeadlessMode) -> Result<HeadlessReport, HeadlessError> {
    let limits = Limits::m0_test();
    let provider: Arc<dyn nexus_core::ProviderPort + Send + Sync> =
        Arc::new(FakeProvider::new(script_for(task)));
    let (runtime, mut streams) = build_runtime(limits, provider, m0_tools())?;
    let submit = SubmitCommand::new(
        RequestId::new("req-headless-1")
            .map_err(|_| HeadlessError::Operation("request id invalid".to_owned()))?,
        SessionId::new("sess-headless-1")
            .map_err(|_| HeadlessError::Operation("session id invalid".to_owned()))?,
        task,
        "m0-test",
    )
    .map_err(|_| {
        HeadlessError::Usage("task input is empty or exceeds the input budget".to_owned())
    })?
    .with_read_only(mode.read_only);
    let started = Instant::now();
    let response = runtime.submit(submit).await;
    if response.reply() != CommandReply::Accepted {
        return Err(HeadlessError::Operation(
            "run submit was not accepted".to_owned(),
        ));
    }
    let run = response
        .run()
        .cloned()
        .expect("accepted submit issues a run");

    let mut consumer = RunConsumer::new();
    let watchdog = watchdog_budget(limits.run_duration.saturating_sub(started.elapsed()));
    let terminal = match tokio::time::timeout(
        watchdog,
        consume_until_terminal(&mut streams, &mut consumer),
    )
    .await
    {
        Ok(Some(finished)) => finished,
        Ok(None) => {
            // The control channel closed without a terminal record. Request
            // cancellation so the runtime can stop dispatch and record
            // honest outcomes before its driver is dropped.
            request_cancel(&runtime, &run, RECONCILE_TIMEOUT).await;
            return Err(HeadlessError::Operation(
                "control channel closed before terminal outcome".to_owned(),
            ));
        }
        Err(_) => {
            // Watchdog expiry: request cancellation before dropping the
            // driver, then reconcile within a small finite window. The error
            // path must not skip the requested cancellation.
            request_cancel(&runtime, &run, RECONCILE_TIMEOUT).await;
            let _ = reconcile(&mut streams, &mut consumer, RECONCILE_TIMEOUT).await;
            return Err(HeadlessError::Operation(
                "run did not reach a terminal outcome within the watchdog budget".to_owned(),
            ));
        }
    };
    while let Ok(event) = streams.data.try_recv() {
        consumer.observe(event);
    }
    if consumer.retention.over_limit {
        return Err(HeadlessError::Operation(
            "mandatory control records exceed the finite report retention budget".to_owned(),
        ));
    }

    let RunConsumer {
        retention,
        tool_started,
        approval_required,
        tool_finished,
        denied_calls,
    } = consumer;
    let mut retained: Vec<RetainedLine> = retention
        .mandatory
        .into_iter()
        .chain(retention.presentation)
        .collect();
    retained.sort_by_key(|record| record.seq);
    let mut lines: Vec<String> = Vec::with_capacity(retained.len() + 1);
    for record in retained {
        lines.push(record.line);
    }
    let outcome = terminal.outcome();
    lines.push(format!(
        "rev={OUTPUT_REV} type=result outcome={} denied={denied_calls} tool-started={tool_started} tool-finished={tool_finished} run={}",
        outcome_name(outcome),
        run.as_str(),
    ));
    let exit_code = exit_code_for(outcome, denied_calls);
    Ok(HeadlessReport {
        run: run.as_str().to_owned(),
        lines,
        outcome,
        denied_calls,
        tool_started,
        approval_required,
        tool_finished,
        truncated: retention.truncated,
        exit_code,
    })
}

/// Drains both channels until the terminal record arrives or the control
/// channel closes. Every observed event is counted and retained subject to
/// the retention policy.
async fn consume_until_terminal(
    streams: &mut EventStreams,
    consumer: &mut RunConsumer,
) -> Option<RunFinished> {
    loop {
        tokio::select! {
            event = streams.data.recv() => {
                if let Some(event) = event {
                    consumer.observe(event);
                }
            }
            event = streams.control.recv() => {
                match event {
                    None => return None,
                    Some(event) => {
                        let finished = match event.payload() {
                            EventPayload::RunFinished(finished) => Some(finished.clone()),
                            _ => None,
                        };
                        consumer.observe(event);
                        if let Some(finished) = finished {
                            return Some(finished);
                        }
                    }
                }
            }
        }
    }
}

/// Requests cancellation of `run`, bounded by `budget` so a wedged runtime
/// cannot hold the error path open. The static request id is valid by
/// construction; the reply is intentionally ignored because the caller is
/// already on an error path, but the request itself is never skipped.
async fn request_cancel(runtime: &Runtime, run: &RunId, budget: Duration) {
    let Ok(request) = RequestId::new("req-headless-cancel") else {
        return;
    };
    let command = CancelCommand {
        request,
        run: run.clone(),
    };
    let _ = tokio::time::timeout(budget, runtime.cancel(command)).await;
}

/// Bounded post-cancel reconciliation: drain both channels until the
/// terminal record arrives or `budget` expires. Returns true when a terminal
/// record was observed.
async fn reconcile(
    streams: &mut EventStreams,
    consumer: &mut RunConsumer,
    budget: Duration,
) -> bool {
    tokio::time::timeout(budget, async {
        loop {
            tokio::select! {
                event = streams.data.recv() => {
                    if let Some(event) = event {
                        consumer.observe(event);
                    }
                }
                event = streams.control.recv() => {
                    match event {
                        None => return,
                        Some(event) => {
                            let terminal = matches!(event.payload(), EventPayload::RunFinished(_));
                            consumer.observe(event);
                            if terminal {
                                return;
                            }
                        }
                    }
                }
            }
        }
    })
    .await
    .is_ok()
}

fn script_for(task: &str) -> Vec<Vec<nexus_core::ProviderEvent>> {
    if let Some(_rest) = task.strip_prefix("deny:") {
        vec![
            tool_turn(vec![candidate(
                "item-1",
                "prov-ref-1",
                "host_write",
                r#"{"path":"dst"}"#,
            )]),
            stop_turn("done-after-denial"),
        ]
    } else if task.strip_prefix("refuse:").is_some() {
        vec![vec![nexus_core::ProviderEvent::TurnFinished(
            nexus_core::TurnFinished::new(
                nexus_core::FinishReason::Refusal,
                nexus_core::Usage::new(None, None, nexus_core::UsageFinality::Final),
                None,
            ),
        )]]
    } else {
        vec![
            tool_turn(vec![candidate(
                "item-1",
                "prov-ref-1",
                "host_read",
                r#"{"path":"src"}"#,
            )]),
            stop_turn("fake-answer"),
        ]
    }
}

fn format_event(event: &RunEvent) -> String {
    let head = format!(
        "rev={OUTPUT_REV} type=event seq={} run={} kind=",
        event.seq(),
        event.run().as_str(),
    );
    match event.payload() {
        EventPayload::RunStarted { request } => {
            format!("{head}run-started request={}", request.as_str())
        }
        EventPayload::AssistantTextDelta(text) => format!(
            "{}text item={} text-len={} text={}",
            head,
            sanitize(&text.item_key),
            text.text.len(),
            sanitize(&text.text),
        ),
        EventPayload::ToolCallPreview { item_key } => {
            format!("{head}preview item={}", sanitize(item_key))
        }
        EventPayload::ApprovalRequired(notice) => format!(
            "{head}approval-required approval={} call={} scope={} args-preview={}",
            notice.approval.as_str(),
            notice.call.as_str(),
            sanitize(&notice.scope_summary),
            notice
                .args_preview()
                .map_or_else(|| "none".to_owned(), sanitize),
        ),
        EventPayload::ToolStarted(info) => {
            format!("{head}tool-started call={}", info.call.as_str())
        }
        EventPayload::ToolOutput(progress) => format!(
            "{head}tool-output call={} truncated={} preview-len={}",
            progress.call.as_str(),
            progress.truncated,
            progress.preview.len(),
        ),
        EventPayload::ToolFinished(info) => {
            let outcome = &info.outcome;
            format!(
                "{head}tool-finished call={} status={} effect={} evidence={} truncated={} content-len={} content={}",
                info.call.as_str(),
                execution_name(outcome.status()),
                effect_name(outcome.effect()),
                evidence_name(outcome.evidence()),
                outcome.is_truncated(),
                outcome.content().len(),
                sanitize(outcome.content()),
            )
        }
        EventPayload::UsageUpdated(usage) => format!(
            "{head}usage input={} output={} finality={}",
            usage_tokens(usage.input_tokens()),
            usage_tokens(usage.output_tokens()),
            match usage.finality() {
                nexus_core::UsageFinality::Provisional => "provisional",
                nexus_core::UsageFinality::Final => "final",
            },
        ),
        EventPayload::RunFinished(finished) => {
            // Only static category names and bounded, caller-pre-redacted
            // correlation pairs are exposed; free-form message text is never
            // echoed and no `Debug` formatting is used. Core may attach a
            // typed failure with `RunFinished::with_error`.
            let (error, correlation) = match finished.error() {
                Some(error) => (
                    error.category().as_str(),
                    correlation_name(error.correlation()),
                ),
                None => ("none", "none".to_owned()),
            };
            format!(
                "{head}run-finished outcome={} persistence={} error={error} error-correlation={correlation}",
                outcome_name(finished.outcome()),
                match finished.persistence() {
                    nexus_core::PersistenceState::Ephemeral => "ephemeral",
                    nexus_core::PersistenceState::Saved => "saved",
                    nexus_core::PersistenceState::SaveFailed => "save-failed",
                },
            )
        }
    }
}

/// Encodes safe pre-redacted correlation pairs as one reversible token:
/// `key=value` pairs joined with `,`, then percent-encoded so the field
/// stays a single `key=value` token. Empty correlation is `none`.
fn correlation_name(correlation: &CorrelationData) -> String {
    if correlation.is_empty() {
        return "none".to_owned();
    }
    let joined = correlation
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",");
    sanitize(&joined)
}

fn execution_name(status: nexus_core::ExecutionStatus) -> &'static str {
    match status {
        nexus_core::ExecutionStatus::Succeeded => "succeeded",
        nexus_core::ExecutionStatus::Failed => "failed",
        nexus_core::ExecutionStatus::Denied => "denied",
        nexus_core::ExecutionStatus::Cancelled => "cancelled",
        nexus_core::ExecutionStatus::TimedOut => "timed-out",
    }
}

fn effect_name(effect: nexus_core::EffectState) -> &'static str {
    match effect {
        nexus_core::EffectState::NotStarted => "not-started",
        nexus_core::EffectState::KnownNotApplied => "known-not-applied",
        nexus_core::EffectState::KnownApplied => "known-applied",
        nexus_core::EffectState::Unknown => "unknown",
    }
}

fn evidence_name(evidence: nexus_core::Evidence) -> &'static str {
    match evidence {
        nexus_core::Evidence::HostObserved => "host-observed",
        nexus_core::Evidence::PluginReported => "plugin-reported",
        nexus_core::Evidence::Uncertain => "uncertain",
    }
}

fn usage_tokens(tokens: Option<u64>) -> String {
    tokens.map_or_else(|| "unknown".to_owned(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::{
        AssistantText, CallId, EffectState, Evidence, ExecutionStatus, RunId, ToolFinishedInfo,
        ToolOutcome, TurnId,
    };

    fn assert_stdout_hygiene(lines: &[String]) {
        assert!(!lines.is_empty());
        for line in lines {
            assert!(
                line.starts_with("rev=m0-test-0 "),
                "every stdout line carries the rev: {line}"
            );
            assert!(!line.contains('\x1b'), "no escape codes: {line}");
            assert!(
                !line.contains('\n') && !line.contains('\r'),
                "single line each: {line}"
            );
            assert!(!line.contains("FAKE"), "no banner leakage: {line}");
            for field in line.split(' ').skip(1) {
                let (key, value) = field.split_once('=').expect("key=value fields");
                assert!(!key.is_empty(), "empty key: {line}");
                assert_encoded_value(value, line);
            }
        }
    }

    /// Every value byte is either unreserved or the start of a complete
    /// `%XX` escape; no raw separator or control byte can slip through.
    fn assert_encoded_value(value: &str, line: &str) {
        let bytes = value.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                let pair = value.get(index + 1..index + 3).expect("escape pair");
                assert!(
                    pair.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "escape uses hex digits: {line}"
                );
                index += 3;
            } else {
                assert!(
                    is_unreserved(bytes[index]),
                    "unreserved byte required: {line}"
                );
                index += 1;
            }
        }
    }

    fn text_event(seq: u64, text: &str) -> RunEvent {
        let fragment = AssistantText::new(TurnId::new("t1-0").expect("valid turn"), "item-1", text)
            .expect("bounded fragment");
        RunEvent::new(
            SessionId::new("sess-headless-1").expect("valid session"),
            RunId::new("run-1").expect("valid run"),
            seq,
            EventPayload::AssistantTextDelta(fragment),
        )
    }

    #[test]
    fn completed_path_executes_read_tool() {
        let report = run_task("hello").expect("completed script runs");
        assert_eq!(report.outcome, RunOutcome::Completed);
        assert_eq!(report.denied_calls, 0);
        assert_eq!(report.tool_started, 1);
        assert_eq!(report.tool_finished, 1);
        assert_eq!(report.exit_code, 0);
        assert!(!report.truncated, "small run fits the report budget");
        assert_stdout_hygiene(&report.lines);
        assert!(
            report
                .lines
                .last()
                .expect("result")
                .contains("outcome=completed")
        );
    }

    #[test]
    fn denial_path_has_no_tool_started_or_approval() {
        let report = run_task("deny: write it").expect("denial script runs");
        assert_eq!(report.outcome, RunOutcome::Completed);
        assert_eq!(report.tool_started, 0, "denied calls never start");
        assert_eq!(report.approval_required, 0, "no handler consumes approvals");
        assert_eq!(report.denied_calls, 1);
        assert_eq!(report.tool_finished, 1);
        let finished = report
            .lines
            .iter()
            .find(|line| line.contains("kind=tool-finished"))
            .expect("denial recorded");
        assert!(finished.contains("status=denied"), "{finished}");
        assert!(finished.contains("effect=not-started"), "{finished}");
        assert_eq!(report.exit_code, 3);
        assert_stdout_hygiene(&report.lines);
    }

    #[test]
    fn refusal_path_maps_to_exit_one() {
        let report = run_task("refuse: no").expect("refusal script runs");
        assert_eq!(report.outcome, RunOutcome::Refused);
        assert_eq!(report.exit_code, 1);
        assert_stdout_hygiene(&report.lines);
    }

    #[test]
    fn plan_mode_reads_but_denies_confirmation_tools() {
        // Automatic reads still execute under plan.
        let read = run_task_with_mode("hello", &HeadlessMode::plan()).expect("plan reads");
        assert_eq!(read.outcome, RunOutcome::Completed);
        assert_eq!(read.tool_started, 1);
        assert_eq!(read.denied_calls, 0);
        assert_eq!(read.exit_code, 0);

        // Confirmation-required writes are denied without prompting.
        let denied =
            run_task_with_mode("deny: write it", &HeadlessMode::plan()).expect("plan denies");
        assert_eq!(denied.outcome, RunOutcome::Completed);
        assert_eq!(denied.tool_started, 0, "denied calls never start");
        assert_eq!(denied.approval_required, 0, "plan never prompts");
        assert_eq!(denied.denied_calls, 1);
        assert_eq!(denied.exit_code, 3);
        assert_stdout_hygiene(&denied.lines);
    }

    #[test]
    fn usage_and_operation_errors_map_to_distinct_exit_codes() {
        let usage = run_task("").err().expect("empty task is usage");
        assert!(usage.is_usage());
        assert_eq!(usage.exit_code(), 2);
        let oversize = run_task(&"x".repeat(nexus_core::commands::MAX_INPUT_BYTES + 1))
            .err()
            .expect("oversize task is usage");
        assert!(oversize.is_usage());
        assert_eq!(oversize.exit_code(), 2);
        let operation = HeadlessError::Operation("checked".to_owned());
        assert!(!operation.is_usage());
        assert_eq!(operation.exit_code(), 1);
        assert_eq!(operation.message(), "checked");
    }

    #[test]
    fn runtime_registration_failure_is_a_normal_operation_error() {
        let duplicate: Vec<Arc<dyn nexus_core::ToolPort + Send + Sync>> = vec![
            Arc::new(FakeTool::read_only()),
            Arc::new(FakeTool::read_only()),
        ];
        let error = build_runtime(
            Limits::m0_test(),
            Arc::new(FakeProvider::new(vec![])),
            duplicate,
        )
        .err()
        .expect("duplicate registration is rejected");
        assert!(!error.is_usage());
        assert!(
            error.message().contains("duplicate tool registration"),
            "{error}"
        );
        assert_eq!(
            m0_tools().len(),
            3,
            "M0 composition registers distinct tools"
        );
    }

    #[test]
    fn exit_code_mapping_covers_all_outcomes() {
        assert_eq!(exit_code_for(RunOutcome::Completed, 0), 0);
        assert_eq!(exit_code_for(RunOutcome::Completed, 2), 3);
        assert_eq!(exit_code_for(RunOutcome::Failed, 0), 1);
        assert_eq!(exit_code_for(RunOutcome::Refused, 0), 1);
        assert_eq!(exit_code_for(RunOutcome::Cancelled, 0), 4);
        assert_eq!(exit_code_for(RunOutcome::LimitReached, 0), 5);
        assert_eq!(EXIT_STDOUT_CLOSED, 6);
    }

    #[test]
    fn watchdog_covers_run_duration_with_slack_not_a_fixed_minute() {
        let limits = Limits::m0_test();
        assert_eq!(limits.run_duration, Duration::from_secs(300));
        assert_eq!(
            watchdog_budget(limits.run_duration),
            Duration::from_secs(305)
        );
        assert!(watchdog_budget(limits.run_duration) > Duration::from_secs(60));
        assert_eq!(watchdog_budget(Duration::ZERO), WATCHDOG_SLACK);
    }

    #[test]
    fn percent_encoding_keeps_unreserved_ascii_identity() {
        assert_eq!(sanitize("run-1_item.key~0"), "run-1_item.key~0");
        assert_eq!(sanitize("host_read"), "host_read");
    }

    #[test]
    fn percent_encoding_escapes_controls_separators_and_percent() {
        let dirty = "a\x1bb\nc\rd=e f\"g%h#,";
        let clean = sanitize(dirty);
        assert_eq!(clean, "a%1Bb%0Ac%0Dd%3De%20f%22g%25h%23%2C");
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains(['\n', '\r', ' ', '=']));
    }

    #[test]
    fn percent_encoding_roundtrips_every_byte_value() {
        // Byte-level round-trip: every possible byte survives encode/decode.
        let bytes: Vec<u8> = (0u8..=u8::MAX).collect();
        let encoded = percent_encode(&bytes);
        assert!(encoded.is_ascii());
        assert_eq!(
            percent_decode_bytes(encoded.as_bytes()).expect("byte roundtrip"),
            bytes
        );

        // Text round-trip: every basic code point U+0000..=U+00FF, plus
        // multi-byte scalars, survives the public string API.
        let text: String = (0u32..=0xFF).filter_map(char::from_u32).collect();
        assert_eq!(percent_decode(&sanitize(&text)).expect("roundtrip"), text);
        for scalar in text.chars() {
            let single = scalar.to_string();
            assert_eq!(
                percent_decode(&sanitize(&single)).expect("roundtrip"),
                single
            );
        }
    }

    #[test]
    fn percent_encoding_roundtrips_text_samples() {
        for text in [
            "héllo 🌍 世界",
            "line\nbreak\ttab",
            "%25 already escaped",
            "a=b c",
            "",
        ] {
            assert_eq!(percent_decode(&sanitize(text)).expect("roundtrip"), text);
        }
    }

    #[test]
    fn percent_decode_rejects_malformed_escapes_and_invalid_utf8() {
        assert_eq!(
            percent_decode("%"),
            Err(PercentDecodeError::MalformedEscape)
        );
        assert_eq!(
            percent_decode("%2"),
            Err(PercentDecodeError::MalformedEscape)
        );
        assert_eq!(
            percent_decode("%GG"),
            Err(PercentDecodeError::MalformedEscape)
        );
        assert_eq!(percent_decode("%FF"), Err(PercentDecodeError::InvalidUtf8));
        assert_eq!(percent_decode("%c3%a9").expect("lowercase hex"), "é");
    }

    fn terminal_event(seq: u64) -> RunEvent {
        RunEvent::new(
            SessionId::new("sess-headless-1").expect("valid session"),
            RunId::new("run-1").expect("valid run"),
            seq,
            EventPayload::RunFinished(
                RunFinished::new(
                    RunOutcome::Completed,
                    nexus_core::PersistenceState::Ephemeral,
                    None,
                )
                .expect("terminal record builds"),
            ),
        )
    }

    fn tool_finished_event(seq: u64, content_len: usize) -> RunEvent {
        let outcome = ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "z".repeat(content_len),
            false,
        )
        .expect("bounded outcome builds");
        RunEvent::new(
            SessionId::new("sess-headless-1").expect("valid session"),
            RunId::new("run-1").expect("valid run"),
            seq,
            EventPayload::ToolFinished(ToolFinishedInfo {
                call: CallId::new("c1-0").expect("valid call"),
                outcome,
            }),
        )
    }

    #[test]
    fn presentation_retention_stays_within_count_and_byte_caps() {
        let mut retention = Retention::new();
        for seq in 0..(MAX_REPORT_EVENTS as u64 + 8) {
            retention.retain(text_event(seq, "x"));
        }
        assert_eq!(retention.presentation.len(), MAX_REPORT_EVENTS);
        assert!(retention.mandatory.is_empty());
        assert!(retention.truncated);

        let mut retention = Retention::new();
        for seq in 0..5 {
            retention.retain(text_event(seq, &"y".repeat(65_000)));
        }
        assert!(
            retention.presentation.len() < 5,
            "byte budget bounds presentation"
        );
        assert!(retention.presentation_bytes <= MAX_REPORT_BYTES);
        assert!(retention.truncated);
    }

    #[test]
    fn mandatory_control_records_survive_presentation_pressure() {
        let mut retention = Retention::new();
        for seq in 0..4 {
            retention.retain(text_event(seq, &"y".repeat(65_000)));
        }
        assert_eq!(retention.presentation.len(), 4);
        assert!(!retention.truncated);

        retention.retain(tool_finished_event(10, 65_000));
        assert_eq!(
            retention.mandatory.len(),
            1,
            "tool outcomes are never dropped"
        );
        assert!(retention.truncated, "eviction of presentation is visible");
        assert!(retention.presentation.len() < 4);
        assert!(
            retention.mandatory_bytes + retention.presentation_bytes <= MAX_REPORT_BYTES,
            "combined exact encoded memory stays bounded"
        );

        retention.retain(terminal_event(11));
        assert_eq!(retention.mandatory.len(), 2, "terminal is never dropped");
        assert!(!retention.over_limit);
    }

    #[test]
    fn mandatory_overflow_is_an_explicit_limit_not_a_drop() {
        let mut retention = Retention::new();
        for seq in 0..(MAX_MANDATORY_EVENTS as u64 + 1) {
            retention.retain(terminal_event(seq));
        }
        assert!(
            retention.over_limit,
            "exceeding the mandatory reserve is explicit"
        );
        assert_eq!(retention.mandatory.len(), MAX_MANDATORY_EVENTS);
    }

    #[test]
    fn run_finished_line_reports_error_category_without_debug() {
        let finished = RunFinished::new(
            RunOutcome::Failed,
            nexus_core::PersistenceState::Ephemeral,
            None,
        )
        .expect("terminal record builds")
        .with_error(
            nexus_core::AgentError::new(
                nexus_core::ErrorCategory::Protocol,
                "stream framing failed",
                nexus_core::RetryGuidance::DoNotRetry,
            )
            .expect("static message builds"),
        );
        let event = RunEvent::new(
            SessionId::new("sess-headless-1").expect("valid session"),
            RunId::new("run-1").expect("valid run"),
            7,
            EventPayload::RunFinished(finished),
        );
        let line = format_event(&event);
        assert!(line.contains("kind=run-finished"), "{line}");
        assert!(line.contains("error=protocol"), "{line}");
        assert!(line.contains("error-correlation=none"), "{line}");
        assert!(
            !line.contains("stream framing failed"),
            "message text is never echoed: {line}"
        );
    }

    #[test]
    fn run_finished_line_exposes_only_safe_pre_redacted_correlation() {
        let mut correlation = CorrelationData::new();
        correlation
            .push("call", "c1-0")
            .expect("safe correlation pair");
        correlation
            .push("scope", "m0-test-grant")
            .expect("safe correlation pair");
        let error = nexus_core::AgentError::with_correlation(
            nexus_core::ErrorCategory::ToolFailure,
            "tool failed",
            correlation,
            nexus_core::RetryGuidance::DoNotRetry,
        )
        .expect("safe error builds");
        let finished = RunFinished::new(
            RunOutcome::Failed,
            nexus_core::PersistenceState::Ephemeral,
            None,
        )
        .expect("terminal record builds")
        .with_error(error);
        let event = RunEvent::new(
            SessionId::new("sess-headless-1").expect("valid session"),
            RunId::new("run-1").expect("valid run"),
            8,
            EventPayload::RunFinished(finished),
        );
        let line = format_event(&event);
        assert!(line.contains("error=tool-failure"), "{line}");
        assert!(
            line.contains("error-correlation=call%3Dc1-0%2Cscope%3Dm0-test-grant"),
            "{line}"
        );
        assert!(
            !line.contains("tool failed"),
            "message text is never echoed: {line}"
        );
    }

    #[test]
    fn approval_line_shows_encoded_args_preview() {
        let notice = nexus_core::ApprovalNotice::new(
            nexus_core::ApprovalId::new("a1-0").expect("valid approval"),
            CallId::new("c1-0").expect("valid call"),
            "write dst",
            "m0-test-grant",
            Duration::from_secs(120),
        )
        .expect("notice builds")
        .with_args_preview(r#"{"path":"dst"}"#)
        .expect("bounded preview builds");
        let event = RunEvent::new(
            SessionId::new("sess-headless-1").expect("valid session"),
            RunId::new("run-1").expect("valid run"),
            3,
            EventPayload::ApprovalRequired(notice),
        );
        let line = format_event(&event);
        assert!(
            line.contains("args-preview=%7B%22path%22%3A%22dst%22%7D"),
            "{line}"
        );
        assert!(
            !line.contains(r#"{"path""#),
            "raw arguments are never emitted: {line}"
        );
    }

    #[test]
    fn shutdown_timeout_bounds_wait_for_stuck_blocking_workers() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let started = Instant::now();
        block_on_with_shutdown(
            async move {
                tokio::task::spawn_blocking(move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                });
                // Wait (bounded) until the blocking worker reports that it is
                // parked, so teardown exercises a genuinely stuck worker
                // rather than a queued one.
                tokio::time::timeout(Duration::from_secs(5), started_rx)
                    .await
                    .expect("worker start signal arrives")
                    .expect("worker start send succeeds");
            },
            Duration::from_millis(50),
        )
        .expect("runtime builds and future completes");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(45),
            "teardown waits the finite timeout: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "teardown never waits indefinitely: {elapsed:?}"
        );
        // Release the quarantined worker so this test leaves no live thread.
        release_tx.send(()).expect("worker still reachable");
    }

    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "closed",
            ))
        }
    }

    #[test]
    fn broken_pipe_write_surfaces_as_error_not_panic() {
        let lines = vec!["rev=m0-test-0 type=result outcome=completed".to_owned()];
        let error =
            write_report_lines(&mut ClosedPipe, &lines).expect_err("broken pipe is an error");
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn banner_self_identifies_fake_wiring() {
        assert!(FAKE_BANNER.contains("FAKE"));
        assert!(FAKE_BANNER.contains("TEST-ONLY"));
        assert!(FAKE_BANNER.contains("NO approval handler"));
    }
}

/// Coverage for the private percent-encoding helpers, so the byte-level
/// contracts `sanitize`/`percent_decode` rely on are pinned without exposing
/// them in the public API. Fixed tables and exhaustive byte iteration only:
/// deterministic, and no clock, filesystem, or process involved.
#[cfg(test)]
mod cov_encoding_private {
    use super::{hex_digit, hex_value, is_unreserved, percent_decode_bytes, percent_encode};

    /// Every reserved byte encodes to exactly one three-character escape.
    #[test]
    fn percent_encode_emits_three_characters_per_reserved_byte() {
        for byte in 0u8..=u8::MAX {
            let encoded = percent_encode(&[byte]);
            if is_unreserved(byte) {
                assert_eq!(encoded, char::from(byte).to_string(), "byte {byte:#04X}");
            } else {
                assert_eq!(encoded, format!("%{byte:02X}"), "byte {byte:#04X}");
                assert_eq!(encoded.len(), 3);
            }
            assert!(encoded.is_ascii());
        }
        assert_eq!(percent_encode(&[]), "");
        assert_eq!(percent_encode(b"abc"), "abc");
        assert_eq!(percent_encode("é".as_bytes()), "%C3%A9");
    }

    /// The passthrough set is exactly RFC 3986 unreserved, nothing more.
    #[test]
    fn is_unreserved_matches_the_rfc3986_set_exactly() {
        let expected: Vec<u8> = (b'A'..=b'Z')
            .chain(b'a'..=b'z')
            .chain(b'0'..=b'9')
            .chain(*b"-._~")
            .collect();
        assert_eq!(expected.len(), 66, "26+26+10+4 unreserved bytes");
        for byte in 0u8..=u8::MAX {
            assert_eq!(
                is_unreserved(byte),
                expected.contains(&byte),
                "byte {byte:#04X}"
            );
        }
        for reserved in [
            b' ', b'=', b'%', b'+', b'/', b':', b'@', b'\x1b', 0x7F, 0x80,
        ] {
            assert!(!is_unreserved(reserved), "byte {reserved:#04X} is reserved");
        }
    }

    /// Uppercase hex output, with the low nibble masked off.
    #[test]
    fn hex_digit_emits_uppercase_and_masks_the_low_nibble() {
        let digits = b"0123456789ABCDEF";
        for nibble in 0u8..=0xFF {
            assert_eq!(
                hex_digit(nibble),
                char::from(digits[usize::from(nibble & 0x0F)])
            );
        }
        assert_eq!(hex_digit(0x00), '0');
        assert_eq!(hex_digit(0x0A), 'A');
        assert_eq!(hex_digit(0x0F), 'F');
        assert_eq!(hex_digit(0x10), '0', "only the low nibble is used");
        assert_eq!(hex_digit(0xFF), 'F');
        assert!(hex_digit(0xAB).is_ascii_uppercase());
    }

    /// Both hex cases are accepted for decoding; nothing else is.
    #[test]
    fn hex_value_accepts_exactly_ascii_hex_digits() {
        for byte in 0u8..=u8::MAX {
            let expected = match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            assert_eq!(hex_value(byte), expected, "byte {byte:#04X}");
        }
        for value in 0u8..=0x09 {
            assert_eq!(hex_value(b'0' + value), Some(value), "digit {value}");
        }
        for value in 0u8..=0x05 {
            assert_eq!(hex_value(b'A' + value), Some(10 + value), "upper {value}");
            assert_eq!(hex_value(b'a' + value), Some(10 + value), "lower {value}");
        }
        // The letters immediately after `F` are no longer hex digits.
        for rejected in *b"GHIJKLM" {
            assert_eq!(hex_value(rejected), None, "byte {rejected:#04X}");
            assert_eq!(hex_value(rejected.to_ascii_lowercase()), None);
        }
        for rejected in [b'/', b':', b'@', b'G', b'g', b'z', b' ', 0x80, 0xFF] {
            assert_eq!(hex_value(rejected), None, "byte {rejected:#04X}");
        }
    }

    /// Byte-level reversibility: every byte value survives encode/decode, so
    /// the private decoder is total on well-formed escapes regardless of
    /// whether the bytes happen to be valid UTF-8.
    #[test]
    fn percent_decode_bytes_roundtrips_every_byte_value() {
        let bytes: Vec<u8> = (0u8..=u8::MAX).collect();
        let encoded = percent_encode(&bytes);
        assert!(encoded.is_ascii());
        assert_eq!(
            percent_decode_bytes(encoded.as_bytes()).expect("byte roundtrip"),
            bytes
        );
        for byte in 0u8..=u8::MAX {
            let single = [byte];
            let encoded = percent_encode(&single);
            assert_eq!(
                percent_decode_bytes(encoded.as_bytes()).expect("single byte"),
                single,
                "byte {byte:#04X}"
            );
        }
        assert_eq!(percent_decode_bytes(&[]).expect("empty"), Vec::<u8>::new());
    }

    /// Malformed escapes are rejected before any UTF-8 validation, and
    /// plain bytes pass through untouched.
    #[test]
    fn percent_decode_bytes_rejects_malformed_and_passes_through_plain() {
        assert_eq!(percent_decode_bytes(b"").expect("empty"), Vec::<u8>::new());
        assert_eq!(percent_decode_bytes(b"plain").expect("plain"), b"plain");
        assert_eq!(percent_decode_bytes(b"a=b c").expect("plain"), b"a=b c");
        assert_eq!(percent_decode_bytes(b"%41").expect("escape"), b"A");
        assert_eq!(percent_decode_bytes(b"%41%42").expect("two"), b"AB");
        assert_eq!(percent_decode_bytes(b"%4a%4A").expect("mixed case"), b"JJ");
        for malformed in [
            &b"%"[..],
            b"%2",
            b"%GG",
            b"%G2",
            b"%2G",
            b"abc%",
            b"abc%4",
            b"%%41",
            b"%41%",
            b"%+0",
            b"% 0",
        ] {
            assert_eq!(
                percent_decode_bytes(malformed).expect_err("malformed"),
                super::PercentDecodeError::MalformedEscape,
                "rejects {:?}",
                String::from_utf8_lossy(malformed)
            );
        }
    }

    /// Encoding never emits a raw byte that could break `key=value` line
    /// framing: every byte outside the passthrough set becomes an escape.
    #[test]
    fn encoded_output_is_pure_ascii_unreserved_or_escaped() {
        let all_bytes: Vec<u8> = (0u8..=u8::MAX).collect();
        let encoded = percent_encode(&all_bytes);
        assert!(encoded.is_ascii());
        assert!(!encoded.contains([' ', '=', '\n', '\r', '\x1b']));
        let bytes = encoded.as_bytes();
        let mut index = 0;
        let mut escapes = 0usize;
        while index < bytes.len() {
            if bytes[index] == b'%' {
                let pair = &encoded[index + 1..index + 3];
                assert!(
                    pair.bytes().all(|byte| byte.is_ascii_hexdigit()),
                    "escape uses hex digits: {encoded}"
                );
                assert_eq!(
                    pair,
                    pair.to_ascii_uppercase(),
                    "escape uses uppercase hex: {encoded}"
                );
                escapes += 1;
                index += 3;
            } else {
                assert!(is_unreserved(bytes[index]), "unreserved only: {encoded}");
                index += 1;
            }
        }
        // 66 unreserved bytes stay literal, the other 190 become escapes.
        assert_eq!(escapes, 190);
        assert_eq!(index, encoded.len(), "escape framing is exact");
    }
}

/// Coverage for the private driver arms that the public M0 composition root
/// cannot reach: the approval counter, the over-limit and empty-eviction
/// retention guards, the bounded cancel/reconcile helpers, and the record
/// formatting arms for previews, progress, provisional usage, non-ephemeral
/// persistence, and the execution/effect/evidence classes no registered fake
/// tool produces.
///
/// Every case is a fixed table, an exhaustively built event, or a
/// synthetic in-memory channel pair: no clock, filesystem, process,
/// randomness, stuck worker, or gate wait is involved.
#[cfg(test)]
mod cov_topup_private {
    use std::sync::Arc;
    use std::time::Duration;

    use nexus_core::{
        AgentError, ApprovalId, ApprovalNotice, AssistantText, CallId, EffectState, ErrorCategory,
        EventPayload, Evidence, ExecutionStatus, Limits, PersistenceState, RequestId,
        RetryGuidance, RunEvent, RunFinished, RunId, RunOutcome, SessionId, ToolFinishedInfo,
        ToolOutcome, ToolProgress, ToolStartedInfo, TurnId, Usage, UsageFinality,
    };
    use nexus_fakes::FakeProvider;
    use nexus_runtime::EventStreams;
    use tokio::sync::mpsc;

    use super::{
        MAX_MANDATORY_EVENTS, MAX_REPORT_BYTES, Retention, RunConsumer, block_on_with_shutdown,
        build_runtime, consume_until_terminal, effect_name, evidence_name, execution_name,
        format_event, m0_tools, reconcile, request_cancel,
    };

    /// Bounded wait used for every helper future: long enough that a
    /// deterministic channel hand-off always wins, short enough that a
    /// genuinely stuck helper fails the test instead of hanging it.
    const HELPER_BUDGET: Duration = Duration::from_secs(1);
    const HELPER_SHUTDOWN: Duration = Duration::from_millis(50);

    fn session() -> SessionId {
        SessionId::new("sess-headless-1").expect("valid session")
    }

    fn run() -> RunId {
        RunId::new("run-1").expect("valid run")
    }

    fn event(seq: u64, payload: EventPayload) -> RunEvent {
        RunEvent::new(session(), run(), seq, payload)
    }

    fn text_event(seq: u64, body: &str) -> RunEvent {
        let fragment = AssistantText::new(TurnId::new("t1-0").expect("valid turn"), "item-1", body)
            .expect("bounded fragment");
        event(seq, EventPayload::AssistantTextDelta(fragment))
    }

    fn tool_started_event(seq: u64) -> RunEvent {
        event(
            seq,
            EventPayload::ToolStarted(ToolStartedInfo {
                call: CallId::new("c1-0").expect("valid call"),
            }),
        )
    }

    fn approval_event(seq: u64) -> RunEvent {
        let notice = ApprovalNotice::new(
            ApprovalId::new("a1-0").expect("valid approval"),
            CallId::new("c1-0").expect("valid call"),
            "write dst",
            "m0-test-grant",
            Duration::from_secs(120),
        )
        .expect("notice builds")
        .with_args_preview(r#"{"path":"dst"}"#)
        .expect("bounded preview builds");
        event(seq, EventPayload::ApprovalRequired(notice))
    }

    fn terminal_event(seq: u64) -> RunEvent {
        event(
            seq,
            EventPayload::RunFinished(
                RunFinished::new(RunOutcome::Completed, PersistenceState::Ephemeral, None)
                    .expect("terminal record builds"),
            ),
        )
    }

    fn tool_finished_event(seq: u64, content_len: usize) -> RunEvent {
        let outcome = ToolOutcome::new(
            ExecutionStatus::Succeeded,
            EffectState::KnownApplied,
            Evidence::HostObserved,
            "z".repeat(content_len),
            false,
        )
        .expect("bounded outcome builds");
        event(
            seq,
            EventPayload::ToolFinished(ToolFinishedInfo {
                call: CallId::new("c1-0").expect("valid call"),
                outcome,
            }),
        )
    }

    /// One channel half preloaded with `events`. The sender is returned so a
    /// test decides explicitly whether the channel stays open or closes.
    fn half(events: Vec<RunEvent>) -> (mpsc::Receiver<RunEvent>, mpsc::Sender<RunEvent>) {
        let (sender, receiver) = mpsc::channel(events.len() + 1);
        for event in events {
            sender.try_send(event).expect("preloaded event fits");
        }
        (receiver, sender)
    }

    /// Two open halves with nothing queued: every `recv` stays pending, so a
    /// helper under test can only finish through its own logic or its budget.
    fn idle_streams() -> (EventStreams, mpsc::Sender<RunEvent>, mpsc::Sender<RunEvent>) {
        let (data, data_sender) = half(Vec::new());
        let (control, control_sender) = half(Vec::new());
        (EventStreams { data, control }, data_sender, control_sender)
    }

    fn fresh_runtime() -> nexus_runtime::Runtime {
        let (runtime, _streams) = build_runtime(
            Limits::m0_test(),
            Arc::new(FakeProvider::new(Vec::new())),
            m0_tools(),
        )
        .expect("the M0 composition registers cleanly");
        runtime
    }

    #[test]
    fn an_approval_record_is_counted_and_retained_as_mandatory() {
        // The composition root installs no approval handler, so no public run
        // ever emits this record; the counter arm and the mandatory
        // classification are pinned directly instead.
        let mut consumer = RunConsumer::new();
        consumer.observe(approval_event(1));
        consumer.observe(tool_started_event(2));

        assert_eq!(
            consumer.approval_required, 1,
            "an approval record is counted once"
        );
        assert_eq!(consumer.tool_started, 1);
        assert_eq!(consumer.tool_finished, 0);
        assert_eq!(consumer.denied_calls, 0);
        assert_eq!(
            consumer.retention.mandatory.len(),
            2,
            "approvals are mandatory control content"
        );
        assert!(
            consumer.retention.presentation.is_empty(),
            "no presentation record was observed"
        );
        assert!(!consumer.retention.over_limit && !consumer.retention.truncated);
        assert!(
            consumer
                .retention
                .mandatory
                .iter()
                .any(|record| record.line.contains("kind=approval-required"))
        );
    }

    #[test]
    fn retention_refuses_to_format_anything_once_the_mandatory_reserve_is_exceeded() {
        let mut retention = Retention::new();
        for seq in 0..=(MAX_MANDATORY_EVENTS as u64) {
            retention.retain(terminal_event(seq));
        }
        assert!(
            retention.over_limit,
            "the explicit refusal, never a dropped mandatory record"
        );
        assert_eq!(retention.mandatory.len(), MAX_MANDATORY_EVENTS);
        let bytes = retention.mandatory_bytes;

        // Past the refusal every later record is ignored entirely: neither the
        // presentation budget nor the truncation flag moves.
        retention.retain(text_event(9_000, "ignored"));
        retention.retain(terminal_event(9_001));
        assert_eq!(retention.mandatory.len(), MAX_MANDATORY_EVENTS);
        assert_eq!(retention.mandatory_bytes, bytes);
        assert!(retention.presentation.is_empty());
        assert!(
            !retention.truncated,
            "a refused report is an error value, not a truncated success"
        );
    }

    #[test]
    fn eviction_stops_when_no_presentation_record_is_left_to_drop() {
        // Mandatory records alone outgrow the report budget. Eviction has
        // nothing left to pop, so it stops instead of looping or evicting a
        // mandatory record, and the mandatory reserve is still respected.
        let mut retention = Retention::new();
        for seq in 0..5 {
            retention.retain(tool_finished_event(seq, 65_000));
        }
        assert!(
            retention.presentation_bytes + retention.mandatory_bytes > MAX_REPORT_BYTES,
            "mandatory bytes alone exceed the report budget"
        );
        assert_eq!(
            retention.mandatory.len(),
            5,
            "mandatory records are never evicted"
        );
        assert!(
            !retention.over_limit,
            "the mandatory reserve is a separate, larger budget"
        );
        assert!(retention.presentation.is_empty());
        assert!(
            !retention.truncated,
            "nothing presentation was dropped, so nothing is reported as dropped"
        );
    }

    #[test]
    fn consume_returns_the_terminal_record_and_every_prior_control_record() {
        let (control, _control_sender) = half(vec![
            tool_started_event(1),
            approval_event(2),
            terminal_event(3),
        ]);
        let (data, _data_sender) = half(Vec::new());
        let mut streams = EventStreams { data, control };
        let mut consumer = RunConsumer::new();

        let finished = block_on_with_shutdown(
            consume_until_terminal(&mut streams, &mut consumer),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");
        let finished = finished.expect("a queued terminal record is returned");
        assert_eq!(finished.outcome(), RunOutcome::Completed);
        assert_eq!(consumer.tool_started, 1);
        assert_eq!(consumer.approval_required, 1);
        assert_eq!(
            consumer.retention.mandatory.len(),
            3,
            "every drained record was retained"
        );
        assert_eq!(
            consumer.retention.mandatory[2].line,
            "rev=m0-test-0 type=event seq=3 run=run-1 kind=run-finished outcome=completed persistence=ephemeral error=none error-correlation=none"
        );
    }

    #[test]
    fn consume_reports_a_closed_control_channel_instead_of_waiting_forever() {
        // A closed control channel is the one honest "no terminal record"
        // answer; the helper must return rather than block on a channel that
        // can never produce anything again.
        let (data, _data_sender) = half(Vec::new());
        let (control, control_sender) = half(Vec::new());
        drop(control_sender);
        let mut streams = EventStreams { data, control };
        let mut consumer = RunConsumer::new();

        let finished = block_on_with_shutdown(
            consume_until_terminal(&mut streams, &mut consumer),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");
        assert!(
            finished.is_none(),
            "a closed control channel yields no terminal record"
        );
        assert_eq!(consumer.tool_finished, 0);
        assert!(consumer.retention.mandatory.is_empty());
    }

    #[test]
    fn reconcile_answers_true_on_a_terminal_record_and_false_on_an_expired_budget() {
        // Terminal record present: the helper stops on it and reports success.
        let (control, _control_sender) = half(vec![terminal_event(7)]);
        let (data, _data_sender) = half(Vec::new());
        let mut streams = EventStreams { data, control };
        let mut consumer = RunConsumer::new();
        let reconciled = block_on_with_shutdown(
            reconcile(&mut streams, &mut consumer, HELPER_BUDGET),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");
        assert!(reconciled, "a terminal record was observed");
        assert_eq!(consumer.retention.mandatory.len(), 1);

        // No terminal record and no channel activity: the helper gives up at
        // its own budget instead of waiting indefinitely.
        let (mut streams, _data_sender, _control_sender) = idle_streams();
        let mut consumer = RunConsumer::new();
        let reconciled = block_on_with_shutdown(
            reconcile(&mut streams, &mut consumer, Duration::from_millis(10)),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");
        assert!(!reconciled, "an expired budget is not a terminal record");
        assert!(consumer.retention.mandatory.is_empty());
    }

    #[test]
    fn reconcile_drains_both_channels_and_survives_a_closed_half() {
        // Traffic on both channels is drained even when no terminal record ever
        // arrives: the presentation record and the non-terminal control record
        // are both observed and counted before the budget ends the wait.
        let (data, _data_sender) = half(vec![text_event(1, "late text")]);
        let (control, _control_sender) = half(vec![tool_started_event(2)]);
        let mut streams = EventStreams { data, control };
        let mut consumer = RunConsumer::new();
        let reconciled = block_on_with_shutdown(
            reconcile(&mut streams, &mut consumer, Duration::from_millis(50)),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");
        assert!(!reconciled, "no terminal record arrived before the budget");
        assert_eq!(consumer.tool_started, 1);
        assert_eq!(
            consumer.retention.presentation.len(),
            1,
            "the presentation record was drained"
        );

        // A closed control channel ends reconciliation instead of parking on
        // it, and a closed data channel is simply skipped on every pass.
        let (data, data_sender) = half(Vec::new());
        drop(data_sender);
        let (control, control_sender) = half(Vec::new());
        drop(control_sender);
        let mut streams = EventStreams { data, control };
        let mut consumer = RunConsumer::new();
        let reconciled = block_on_with_shutdown(
            reconcile(&mut streams, &mut consumer, HELPER_BUDGET),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");
        assert!(
            reconciled,
            "a closed control channel ends the drain without an error"
        );
        assert!(consumer.retention.mandatory.is_empty());
    }

    #[test]
    fn cancel_is_requested_for_an_unknown_run_without_panicking_or_waiting() {
        // The caller is already on an error path, so the reply is ignored, but
        // the request is still issued. An unknown run id answers
        // `StaleOrUnknownTarget`; the helper must return normally either way.
        let runtime = fresh_runtime();
        let unknown = RunId::new("run-never-submitted").expect("valid run id");
        let started = std::time::Instant::now();
        block_on_with_shutdown(
            request_cancel(&runtime, &unknown, HELPER_BUDGET),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "cancellation never waits indefinitely"
        );

        // A second request is equally bounded: cancellation is idempotent from
        // the caller's point of view, whether or not it changes the reply.
        block_on_with_shutdown(
            request_cancel(&runtime, &unknown, HELPER_BUDGET),
            HELPER_SHUTDOWN,
        )
        .expect("the helper future completes");

        // The request identity is fixed and valid by construction, which is why
        // the static id is not a source of runtime failures.
        assert!(RequestId::new("req-headless-cancel").is_ok());
    }

    #[test]
    fn preview_and_progress_records_encode_their_bounded_fields() {
        // No registered fake provider emits call-preview deltas or tool
        // progress, so neither record can appear in a public report.
        let preview = format_event(&event(
            1,
            EventPayload::ToolCallPreview {
                item_key: "item-1".to_owned(),
            },
        ));
        assert_eq!(
            preview,
            "rev=m0-test-0 type=event seq=1 run=run-1 kind=preview item=item-1"
        );

        let progress = ToolProgress::new(
            CallId::new("c1-0").expect("valid call"),
            "half a result",
            true,
        )
        .expect("bounded progress builds");
        let output = format_event(&event(2, EventPayload::ToolOutput(progress)));
        assert_eq!(
            output,
            "rev=m0-test-0 type=event seq=2 run=run-1 kind=tool-output call=c1-0 truncated=true preview-len=13"
        );
        // The raw preview text is never emitted, only its decoded length.
        assert!(!output.contains("half"));
    }

    #[test]
    fn every_usage_record_distinguishes_provisional_from_final() {
        // The scripted provider only reports final usage; the provisional arm
        // is pinned here so the two stay distinguishable on the wire.
        let provisional = format_event(&event(
            1,
            EventPayload::UsageUpdated(Usage::new(Some(12), Some(7), UsageFinality::Provisional)),
        ));
        assert_eq!(
            provisional,
            "rev=m0-test-0 type=event seq=1 run=run-1 kind=usage input=12 output=7 finality=provisional"
        );

        let final_usage = format_event(&event(
            2,
            EventPayload::UsageUpdated(Usage::new(None, None, UsageFinality::Final)),
        ));
        assert_eq!(
            final_usage,
            "rev=m0-test-0 type=event seq=2 run=run-1 kind=usage input=unknown output=unknown finality=final"
        );
    }

    #[test]
    fn a_durable_terminal_record_reports_its_persistence_class() {
        // M0 storage is ephemeral, so `saved` and `save-failed` never appear
        // in a public report; both class names are pinned here instead.
        let saved = RunFinished::new(RunOutcome::Completed, PersistenceState::Saved, None)
            .expect("terminal record builds");
        let line = format_event(&event(1, EventPayload::RunFinished(saved)));
        assert!(
            line.ends_with("kind=run-finished outcome=completed persistence=saved error=none error-correlation=none"),
            "{line}"
        );

        let persistence_error = AgentError::new(
            ErrorCategory::StorageFailure,
            "session save failed",
            RetryGuidance::DoNotRetry,
        )
        .expect("static diagnostic builds");
        let save_failed = RunFinished::new(
            RunOutcome::Completed,
            PersistenceState::SaveFailed,
            Some(persistence_error),
        )
        .expect("terminal record builds");
        let line = format_event(&event(2, EventPayload::RunFinished(save_failed)));
        assert!(
            line.ends_with("persistence=save-failed error=none error-correlation=none"),
            "a persistence failure is not an execution failure: {line}"
        );
        assert!(
            !line.contains("session save failed"),
            "message text is never echoed: {line}"
        );
    }

    #[test]
    fn the_outcome_class_tables_are_total_and_stable() {
        // No registered fake tool fails, times out, or is cancelled, so these
        // wire names are only reachable through the formatter itself.
        for (status, name) in [
            (ExecutionStatus::Succeeded, "succeeded"),
            (ExecutionStatus::Failed, "failed"),
            (ExecutionStatus::Denied, "denied"),
            (ExecutionStatus::Cancelled, "cancelled"),
            (ExecutionStatus::TimedOut, "timed-out"),
        ] {
            assert_eq!(execution_name(status), name, "status {status:?}");
        }
        for (effect, name) in [
            (EffectState::NotStarted, "not-started"),
            (EffectState::KnownNotApplied, "known-not-applied"),
            (EffectState::KnownApplied, "known-applied"),
            (EffectState::Unknown, "unknown"),
        ] {
            assert_eq!(effect_name(effect), name, "effect {effect:?}");
        }
        for (evidence, name) in [
            (Evidence::HostObserved, "host-observed"),
            (Evidence::PluginReported, "plugin-reported"),
            (Evidence::Uncertain, "uncertain"),
        ] {
            assert_eq!(evidence_name(evidence), name, "evidence {evidence:?}");
        }
    }

    #[test]
    fn an_unreached_tool_outcome_is_still_formatted_as_static_class_names() {
        // The same table, reached through the formatter: a failure with
        // honestly unknown effects is reported as static class names only.
        let outcome = ToolOutcome::new(
            ExecutionStatus::Failed,
            EffectState::Unknown,
            Evidence::Uncertain,
            "tool reported a failure",
            true,
        )
        .expect("bounded outcome builds");
        let line = format_event(&event(
            4,
            EventPayload::ToolFinished(ToolFinishedInfo {
                call: CallId::new("c1-0").expect("valid call"),
                outcome,
            }),
        ));
        assert!(
            line.ends_with("kind=tool-finished call=c1-0 status=failed effect=unknown evidence=uncertain truncated=true content-len=23 content=tool%20reported%20a%20failure"),
            "{line}"
        );
        // Cancelled and timed-out share the same effect/evidence honesty: the
        // runtime cannot know whether the effect landed.
        for (status, name) in [
            (ExecutionStatus::Cancelled, "cancelled"),
            (ExecutionStatus::TimedOut, "timed-out"),
        ] {
            let outcome = ToolOutcome::new(
                status,
                EffectState::Unknown,
                Evidence::Uncertain,
                "abandoned",
                false,
            )
            .expect("bounded outcome builds");
            let line = format_event(&event(
                5,
                EventPayload::ToolFinished(ToolFinishedInfo {
                    call: CallId::new("c1-0").expect("valid call"),
                    outcome,
                }),
            ));
            assert!(
                line.contains(&format!("status={name} effect=unknown evidence=uncertain")),
                "{line}"
            );
        }
    }
}
