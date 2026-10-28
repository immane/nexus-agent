//! Host-side framing and validation for a container worker transport.
//! This module does not start workloads. Controlled cancellation depends on
//! the selected transport's `exchange_controlled` implementation.

use std::{
    error::Error,
    fmt,
    io::{self, Write},
    time::Instant,
};

use nexus_config::ToolsSettings;
use nexus_core::{EffectState, Evidence, ExecutionStatus, ToolOutcome};
use serde_json::Value;

use crate::worker_protocol::{
    MAX_ARGUMENT_BYTES, MAX_OUTPUT_BYTES, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PROTOCOL_VERSION,
    parse_unique, valid_arguments, validate_workspace_paths,
};

pub trait WorkerTransport {
    type Error: Error + Send + Sync + 'static;

    /// Identifies a transport failure that proves no request was dispatched.
    /// The default is conservative: transport errors are considered ambiguous.
    fn pre_dispatch_failure(_error: &Self::Error) -> Option<PreDispatchFailure> {
        None
    }

    /// Performs exactly one exchange for `request` and returns exactly one
    /// response followed by EOF. Implementations must honor the absolute
    /// `deadline`, cap bytes while collecting the response (not after it has
    /// already been buffered), and must never retry or replay the request.
    /// A transport/protocol failure may occur after the worker applied effects;
    /// it is not rollback-safe and does not imply that the request was unapplied.
    fn exchange(
        &mut self,
        request: &[u8],
        max_response_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, Self::Error>;

    /// Performs one exchange while observing live cancellation. Existing test
    /// transports may use the default; the Docker transport overrides this to
    /// stop its local CLI and report an uncertain remote outcome.
    fn exchange_controlled(
        &mut self,
        request: &[u8],
        max_response_bytes: usize,
        deadline: Instant,
        _cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<u8>, Self::Error> {
        self.exchange(request, max_response_bytes, deadline)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreDispatchFailure {
    Deadline,
    InvalidRequest,
}

#[derive(Debug)]
pub enum ClientError<E> {
    InvalidRequest,
    InvalidResponse,
    DeadlineExpired,
    Transport(E),
}

impl<E: fmt::Display> fmt::Display for ClientError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest => f.write_str("worker request is invalid"),
            Self::InvalidResponse => f.write_str("worker response is invalid"),
            Self::DeadlineExpired => f.write_str("worker deadline has expired"),
            Self::Transport(error) => write!(f, "worker transport failed: {error}"),
        }
    }
}

impl<E: Error + 'static> Error for ClientError<E> {}

pub struct ContainerWorkerClient<T> {
    transport: T,
    settings: ToolsSettings,
}

impl<T: WorkerTransport> ContainerWorkerClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            settings: ToolsSettings::default(),
        }
    }

    pub fn with_settings(
        mut self,
        settings: &ToolsSettings,
    ) -> Result<Self, ClientError<T::Error>> {
        settings
            .validate()
            .map_err(|_| ClientError::InvalidRequest)?;
        self.settings = settings.clone();
        Ok(self)
    }

    pub fn execute(
        &mut self,
        operation: &str,
        arguments: &Value,
        output_limit: usize,
        deadline: Instant,
    ) -> Result<ToolOutcome, ClientError<T::Error>> {
        Self::execute_using(
            &mut self.transport,
            &self.settings,
            operation,
            arguments,
            output_limit,
            deadline,
        )
    }

    pub(crate) fn into_parts(self) -> (T, ToolsSettings) {
        (self.transport, self.settings)
    }

    pub(crate) fn execute_using(
        transport: &mut T,
        settings: &ToolsSettings,
        operation: &str,
        arguments: &Value,
        output_limit: usize,
        deadline: Instant,
    ) -> Result<ToolOutcome, ClientError<T::Error>> {
        Self::execute_using_controlled(
            transport,
            settings,
            operation,
            arguments,
            output_limit,
            deadline,
            &|| false,
        )
    }

    pub(crate) fn execute_using_controlled(
        transport: &mut T,
        settings: &ToolsSettings,
        operation: &str,
        arguments: &Value,
        output_limit: usize,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<ToolOutcome, ClientError<T::Error>> {
        let request = encode_request(operation, arguments, output_limit, settings)?;
        if Instant::now() >= deadline {
            return Err(ClientError::DeadlineExpired);
        }
        let response = transport
            .exchange_controlled(&request, MAX_RESPONSE_BYTES, deadline, cancelled)
            .map_err(ClientError::Transport)?;
        if response.len() > MAX_RESPONSE_BYTES {
            return Err(ClientError::InvalidResponse);
        }
        decode_response::<T::Error>(&response, output_limit)
            .map_err(|_| ClientError::InvalidResponse)
    }
}

fn encode_request<E>(
    operation: &str,
    arguments: &Value,
    output_limit: usize,
    settings: &ToolsSettings,
) -> Result<Vec<u8>, ClientError<E>> {
    settings
        .validate()
        .map_err(|_| ClientError::InvalidRequest)?;
    if output_limit == 0
        || output_limit > MAX_OUTPUT_BYTES
        || !valid_arguments(operation, arguments)
        || validate_workspace_paths(operation, arguments).is_err()
    {
        return Err(ClientError::InvalidRequest);
    }
    let mut argument_bytes = Vec::with_capacity(MAX_ARGUMENT_BYTES.min(4096));
    let mut writer = BoundedWriter {
        output: &mut argument_bytes,
        limit: MAX_ARGUMENT_BYTES,
    };
    serde_json::to_writer(&mut writer, arguments).map_err(|_| ClientError::InvalidRequest)?;
    let envelope = RequestEnvelope {
        version: PROTOCOL_VERSION,
        operation,
        arguments,
        output_limit,
        tools: settings,
    };
    let mut bytes = Vec::with_capacity(MAX_REQUEST_BYTES.min(4096));
    let mut writer = BoundedWriter {
        output: &mut bytes,
        limit: MAX_REQUEST_BYTES,
    };
    serde_json::to_writer(&mut writer, &envelope).map_err(|_| ClientError::InvalidRequest)?;
    Ok(bytes)
}

struct RequestEnvelope<'a> {
    version: u16,
    operation: &'a str,
    arguments: &'a Value,
    output_limit: usize,
    tools: &'a ToolsSettings,
}
impl serde::Serialize for RequestEnvelope<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("RequestEnvelope", 5)?;
        state.serialize_field("version", &self.version)?;
        state.serialize_field("operation", self.operation)?;
        state.serialize_field("arguments", self.arguments)?;
        state.serialize_field("output_limit", &self.output_limit)?;
        state.serialize_field("tools", self.tools)?;
        state.end()
    }
}

struct BoundedWriter<'a> {
    output: &'a mut Vec<u8>,
    limit: usize,
}
impl Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.output.len().saturating_add(bytes.len()) > self.limit {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "frame limit"));
        }
        self.output.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn decode_response<E>(bytes: &[u8], output_limit: usize) -> Result<ToolOutcome, ClientError<E>> {
    let value = parse_unique(bytes).map_err(|_| ClientError::InvalidResponse)?;
    let top = value.as_object().ok_or(ClientError::InvalidResponse)?;
    if top.len() != 2
        || top.get("version").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION))
    {
        return Err(ClientError::InvalidResponse);
    }
    let result = top
        .get("result")
        .and_then(Value::as_object)
        .ok_or(ClientError::InvalidResponse)?;
    if result.len() != 4 {
        return Err(ClientError::InvalidResponse);
    }
    let status = match result.get("status").and_then(Value::as_str) {
        Some("Succeeded") => ExecutionStatus::Succeeded,
        Some("Failed") => ExecutionStatus::Failed,
        Some("Denied") => ExecutionStatus::Denied,
        Some("Cancelled") => ExecutionStatus::Cancelled,
        Some("TimedOut") => ExecutionStatus::TimedOut,
        Some("Interrupted") => ExecutionStatus::Interrupted,
        _ => return Err(ClientError::InvalidResponse),
    };
    let effect = match result.get("effect").and_then(Value::as_str) {
        Some("NotStarted") => EffectState::NotStarted,
        Some("KnownNotApplied") => EffectState::KnownNotApplied,
        Some("KnownApplied") => EffectState::KnownApplied,
        Some("Unknown") => EffectState::Unknown,
        _ => return Err(ClientError::InvalidResponse),
    };
    let content = result
        .get("content")
        .and_then(Value::as_str)
        .ok_or(ClientError::InvalidResponse)?;
    let truncated = result
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or(ClientError::InvalidResponse)?;
    if content.len() > output_limit {
        return Err(ClientError::InvalidResponse);
    }
    ToolOutcome::new(status, effect, Evidence::PluginReported, content, truncated)
        .map_err(|_| ClientError::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{io, time::Duration};

    #[derive(Default)]
    struct Fake {
        response: Vec<u8>,
        calls: usize,
        fail: bool,
        request: Vec<u8>,
        allowance: usize,
        deadline: Option<Instant>,
    }
    impl WorkerTransport for Fake {
        type Error = io::Error;
        fn exchange(
            &mut self,
            request: &[u8],
            max: usize,
            deadline: Instant,
        ) -> Result<Vec<u8>, Self::Error> {
            self.calls += 1;
            self.request = request.to_vec();
            self.allowance = max;
            self.deadline = Some(deadline);
            if self.fail {
                Err(io::Error::new(io::ErrorKind::TimedOut, "deadline"))
            } else {
                Ok(self.response.clone())
            }
        }
    }
    fn frame(result: &str) -> Vec<u8> {
        format!(r#"{{"version":2,"result":{result}}}"#).into_bytes()
    }
    const GOOD: &str =
        r#"{"status":"Succeeded","effect":"KnownApplied","content":"ok","truncated":false}"#;

    #[test]
    fn reduced_settings_are_retained_and_sent_for_every_operation() {
        let mut settings = ToolsSettings {
            list_entry_limit: 2,
            search_file_bytes: 7,
        };
        let mut client = ContainerWorkerClient::new(Fake {
            response: frame(GOOD),
            ..Fake::default()
        })
        .with_settings(&settings)
        .unwrap();
        settings.list_entry_limit = 9;
        assert_eq!(settings.list_entry_limit, 9);
        for (operation, arguments) in [
            ("host_read", json!({"path":"a"})),
            ("host_list", json!({"path":"a"})),
            ("host_search", json!({"path":"a","query":"q"})),
            ("host_write", json!({"path":"a","content":"x"})),
            (
                "host_patch",
                json!({"path":"a","old_text":"x","new_text":"y"}),
            ),
        ] {
            client
                .execute(
                    operation,
                    &arguments,
                    64,
                    Instant::now() + Duration::from_secs(2),
                )
                .unwrap();
            let request =
                crate::worker_protocol::read_request(&mut client.transport.request.as_slice())
                    .unwrap();
            assert_eq!(request.version, 2);
            assert_eq!(
                request.tools,
                ToolsSettings {
                    list_entry_limit: 2,
                    search_file_bytes: 7
                }
            );
        }
        assert_eq!(client.transport.calls, 5);
    }

    #[test]
    fn invalid_settings_are_rejected_before_dispatch() {
        for settings in [
            ToolsSettings {
                list_entry_limit: 0,
                search_file_bytes: 1,
            },
            ToolsSettings {
                list_entry_limit: 10001,
                search_file_bytes: 1,
            },
            ToolsSettings {
                list_entry_limit: 1,
                search_file_bytes: 0,
            },
            ToolsSettings {
                list_entry_limit: 1,
                search_file_bytes: 1048577,
            },
        ] {
            assert!(matches!(
                ContainerWorkerClient::new(Fake::default()).with_settings(&settings),
                Err(ClientError::InvalidRequest)
            ));
            assert!(
                encode_request::<io::Error>("host_read", &json!({"path":"a"}), 64, &settings)
                    .is_err()
            );
        }
    }

    #[test]
    fn good_round_trip_and_file_schemas() {
        let mut client = ContainerWorkerClient::new(Fake {
            response: frame(GOOD),
            ..Fake::default()
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let outcome = client
            .execute(
                "host_write",
                &json!({"path":"a","content":"x"}),
                12,
                deadline,
            )
            .unwrap();
        assert_eq!(outcome.evidence(), Evidence::PluginReported);
        assert_eq!(outcome.effect(), EffectState::KnownApplied);
        assert_eq!(client.transport.allowance, MAX_RESPONSE_BYTES);
        assert_eq!(client.transport.deadline, Some(deadline));
        let decoded: Value = serde_json::from_slice(&client.transport.request).unwrap();
        assert_eq!(
            decoded,
            json!({"version":PROTOCOL_VERSION,"operation":"host_write","arguments":{"path":"a","content":"x"},"output_limit":12,"tools":ToolsSettings::default()})
        );
        for (op, args) in [
            ("host_read", json!({"path":"a"})),
            ("host_read", json!({"path":"a","offset":8192,"limit":16384})),
            (
                "host_read",
                json!({"path":"a","offset":8192.0,"limit":16384.0}),
            ),
            ("host_list", json!({"path":"a"})),
            ("host_search", json!({"path":"a","query":"q"})),
            ("host_write", json!({"path":"a","content":"x"})),
            (
                "host_patch",
                json!({"path":"a","old_text":"x","new_text":"y"}),
            ),
        ] {
            assert!(
                encode_request::<io::Error>(op, &args, 10, &ToolsSettings::default()).is_ok(),
                "{op}"
            );
        }
    }

    #[test]
    fn read_page_cursor_survives_the_client_transport() {
        let content = "[more; next_offset=8196]\n界a";
        let mut client = ContainerWorkerClient::new(Fake {
            response: frame(&json!({"status":"Succeeded","effect":"KnownNotApplied","content":content,"truncated":true}).to_string()),
            ..Fake::default()
        });
        let args = json!({"path":"a","offset":8192,"limit":4});
        let outcome = client
            .execute(
                "host_read",
                &args,
                4096,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!(outcome.content(), content);
        assert!(outcome.is_truncated());
        let request: Value = serde_json::from_slice(&client.transport.request).unwrap();
        assert_eq!(request["arguments"], args);
        assert_eq!(client.transport.calls, 1);
    }

    #[test]
    fn malformed_response_matrix_is_rejected() {
        for bytes in [
            frame(r#"{"status":"Succeeded","effect":"Unknown","content":"","truncated":false,"extra":1}"#),
            br#"{"version":2,"result":{"status":"Succeeded","status":"Failed","effect":"Unknown","content":"","truncated":false}}"#.to_vec(),
            [frame(GOOD), b" {}".to_vec()].concat(), frame(r#"{"status":"Succeeded","effect":"Unknown","content":"","truncated":false}garbage"#),
            format!(r#"{{"version":1,"result":{GOOD}}}"#).into_bytes(),
            frame(r#"{"status":"Bogus","effect":"Unknown","content":"","truncated":false}"#),
            frame(r#"{"status":"Succeeded","effect":"Bogus","content":"","truncated":false}"#),
            frame(r#"{"status":"Denied","effect":"Unknown","content":"","truncated":false}"#),
            frame(r#"{"status":"Succeeded","effect":"Unknown","content":"12345","truncated":false}"#),
        ] { assert!(decode_response::<io::Error>(&bytes, 4).is_err()); }
    }

    #[test]
    fn transport_failure_propagates_and_mutation_is_never_retried() {
        let mut client = ContainerWorkerClient::new(Fake {
            fail: true,
            ..Fake::default()
        });
        assert!(matches!(
            client.execute(
                "host_write",
                &json!({"path":"a","content":"x"}),
                10,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(ClientError::Transport(_))
        ));
        assert_eq!(client.transport.calls, 1);
    }

    #[test]
    fn rejects_oversize_actual_response_and_invalid_requests() {
        let mut client = ContainerWorkerClient::new(Fake {
            response: vec![0; MAX_RESPONSE_BYTES + 1],
            ..Fake::default()
        });
        assert!(matches!(
            client.execute(
                "host_read",
                &json!({"path":"a"}),
                1,
                Instant::now() + Duration::from_secs(1)
            ),
            Err(ClientError::InvalidResponse)
        ));
        assert!(
            encode_request::<io::Error>(
                "host_write",
                &json!({"path":"../x","content":"y"}),
                1,
                &ToolsSettings::default()
            )
            .is_err()
        );
        for (operation, args, limit) in [
            ("unknown", json!({"path":"a"}), 1),
            ("host_read", json!({"path":1}), 1),
            ("host_read", json!({"path":"a"}), MAX_OUTPUT_BYTES + 1),
        ] {
            assert!(
                encode_request::<io::Error>(operation, &args, limit, &ToolsSettings::default())
                    .is_err()
            );
        }
    }

    #[test]
    fn argument_byte_limit_counts_utf8_and_accepts_boundary() {
        let base = serde_json::to_vec(&json!({"path":"a","content":""}))
            .unwrap()
            .len();
        let boundary_bytes = MAX_ARGUMENT_BYTES - base;
        let exact = json!({"path":"a","content":"x".repeat(boundary_bytes)});
        let unicode_fit = json!({"path":"a","content":"é".repeat(boundary_bytes / 2)});
        let unicode_over = json!({"path":"a","content":"é".repeat((boundary_bytes / 2) + 1)});
        assert_eq!(
            serde_json::to_vec(&exact).unwrap().len(),
            MAX_ARGUMENT_BYTES
        );
        assert!(
            encode_request::<io::Error>("host_write", &exact, 1, &ToolsSettings::default()).is_ok()
        );
        assert!(
            encode_request::<io::Error>("host_write", &unicode_fit, 1, &ToolsSettings::default())
                .is_ok()
        );
        assert!(
            encode_request::<io::Error>("host_write", &unicode_over, 1, &ToolsSettings::default())
                .is_err()
        );
    }

    #[test]
    fn elapsed_deadline_does_not_call_transport() {
        let mut client = ContainerWorkerClient::new(Fake::default());
        assert!(matches!(
            client.execute("host_read", &json!({"path":"a"}), 1, Instant::now()),
            Err(ClientError::DeadlineExpired)
        ));
        assert_eq!(client.transport.calls, 0);
    }

    #[test]
    fn accepts_valid_non_success_outcomes_and_truncation() {
        for (status, effect, truncated) in [
            ("Failed", "Unknown", false),
            ("Cancelled", "Unknown", false),
            ("TimedOut", "Unknown", false),
            ("Interrupted", "Unknown", false),
            ("Succeeded", "KnownApplied", true),
        ] {
            let bytes = frame(&format!(
                r#"{{"status":"{status}","effect":"{effect}","content":"ok","truncated":{truncated}}}"#
            ));
            assert!(decode_response::<io::Error>(&bytes, 10).is_ok(), "{status}");
        }
    }
}
