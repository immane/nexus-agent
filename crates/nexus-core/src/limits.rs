//! Finite effective limits (lock section 1).
//!
//! Every field is a plain number: an omitted value can never mean infinity
//! because there is no optional field. [`Limits::validate`] rejects zero
//! budgets and durations beyond the documented practical M0-test ceiling,
//! and each `check_*` method maps exhaustion to an explicit
//! [`AgentError`] with category [`ErrorCategory::ResourceLimit`], never to a
//! silent fallback. Validated durations stay far below `Duration`/`Instant`
//! overflow, but callers still build deadlines with checked arithmetic and
//! treat overflow as exhaustion.
//!
//! The associated `M0_TEST_*` constants are M0-test stand-ins only, not
//! product defaults.

use std::time::Duration;

use crate::error::{AgentError, ErrorCategory, RetryGuidance};

/// Effective policy budgets. All fields required; zero is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Model turns admitted per run.
    pub max_model_turns_per_run: u32,
    /// Tool calls admitted per run.
    pub max_tool_calls_per_run: u32,
    /// Tool calls admitted per turn, in declared order, sequential.
    pub max_tool_calls_per_turn: u32,
    /// Total run duration, measured on a monotonic clock.
    pub run_duration: Duration,
    /// Deadline per tool execution.
    pub per_tool_timeout: Duration,
    /// Retained context items.
    pub retained_context_items: usize,
    /// Streamed tool-argument assembly budget in bytes.
    pub max_arg_assembly_bytes: usize,
    /// Progress plus final tool output share this budget in bytes.
    pub max_tool_output_bytes: usize,
    /// Event data channel capacity in events.
    pub event_data_capacity: usize,
    /// Event control channel capacity in events.
    pub event_control_capacity: usize,
    /// Concurrent active operations.
    pub max_concurrent_ops: usize,
    /// Default approval lifetime.
    pub approval_expiry: Duration,
}

impl Limits {
    /// M0-TEST: model turns per run. Not a product default.
    pub const M0_TEST_MODEL_TURNS_PER_RUN: u32 = 8;
    /// M0-TEST: tool calls per run. Not a product default.
    pub const M0_TEST_TOOL_CALLS_PER_RUN: u32 = 16;
    /// M0-TEST: tool calls per turn. Not a product default.
    pub const M0_TEST_TOOL_CALLS_PER_TURN: u32 = 8;
    /// M0-TEST: run duration in seconds. Not a product default.
    pub const M0_TEST_RUN_DURATION_SECS: u64 = 300;
    /// M0-TEST: per-tool timeout in seconds. Not a product default.
    pub const M0_TEST_PER_TOOL_TIMEOUT_SECS: u64 = 60;
    /// M0-TEST: retained context items. Not a product default.
    pub const M0_TEST_RETAINED_CONTEXT_ITEMS: usize = 128;
    /// M0-TEST: streamed tool-argument assembly budget in bytes. Not a product default.
    pub const M0_TEST_ARG_ASSEMBLY_BYTES: usize = 65_536;
    /// M0-TEST: tool output budget in bytes, shared by progress and final. Not a product default.
    pub const M0_TEST_TOOL_OUTPUT_BYTES: usize = 262_144;
    /// M0-TEST: event data channel capacity. Not a product default.
    pub const M0_TEST_EVENT_DATA_CAPACITY: usize = 1_024;
    /// M0-TEST: event control channel capacity. Not a product default.
    pub const M0_TEST_EVENT_CONTROL_CAPACITY: usize = 128;
    /// M0-TEST: concurrent active operations. Not a product default.
    pub const M0_TEST_MAX_CONCURRENT_OPS: usize = 8;
    /// M0-TEST: default approval expiry in seconds. Not a product default.
    pub const M0_TEST_APPROVAL_EXPIRY_SECS: u64 = 120;
    /// M0-TEST: practical ceiling for any single validated duration (run,
    /// per-tool timeout, approval expiry), 24 hours. A [`Duration`] can hold
    /// values that are effectively unbounded; this ceiling keeps deadline
    /// arithmetic far from `Duration`/`Instant` overflow. Not a product
    /// default.
    pub const M0_TEST_MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);

    /// Returns the M0-test budget set from the lock table.
    #[must_use]
    pub fn m0_test() -> Self {
        Self {
            max_model_turns_per_run: Self::M0_TEST_MODEL_TURNS_PER_RUN,
            max_tool_calls_per_run: Self::M0_TEST_TOOL_CALLS_PER_RUN,
            max_tool_calls_per_turn: Self::M0_TEST_TOOL_CALLS_PER_TURN,
            run_duration: Duration::from_secs(Self::M0_TEST_RUN_DURATION_SECS),
            per_tool_timeout: Duration::from_secs(Self::M0_TEST_PER_TOOL_TIMEOUT_SECS),
            retained_context_items: Self::M0_TEST_RETAINED_CONTEXT_ITEMS,
            max_arg_assembly_bytes: Self::M0_TEST_ARG_ASSEMBLY_BYTES,
            max_tool_output_bytes: Self::M0_TEST_TOOL_OUTPUT_BYTES,
            event_data_capacity: Self::M0_TEST_EVENT_DATA_CAPACITY,
            event_control_capacity: Self::M0_TEST_EVENT_CONTROL_CAPACITY,
            max_concurrent_ops: Self::M0_TEST_MAX_CONCURRENT_OPS,
            approval_expiry: Duration::from_secs(Self::M0_TEST_APPROVAL_EXPIRY_SECS),
        }
    }

    /// Rejects zero budgets, an argument-assembly budget above the global M0
    /// cap ([`Self::M0_TEST_ARG_ASSEMBLY_BYTES`]), and any individual duration
    /// above the documented practical M0-test ceiling. Durations are
    /// validated independently: a short run budget may legitimately carry the
    /// longer default per-tool timeout or approval expiry, because the
    /// effective deadline is the minimum of the run deadline and the
    /// operation deadline, evaluated by the runtime. Values that pass this
    /// check stay subject to checked runtime arithmetic: callers build
    /// deadlines with `Instant::checked_add` and treat a failed add as budget
    /// exhaustion, never as a panic.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.max_model_turns_per_run == 0
            || self.max_tool_calls_per_run == 0
            || self.max_tool_calls_per_turn == 0
            || self.retained_context_items == 0
            || self.max_arg_assembly_bytes == 0
            || self.max_tool_output_bytes == 0
            || self.event_data_capacity == 0
            || self.event_control_capacity == 0
            || self.max_concurrent_ops == 0
        {
            return Err(limit_error("limit budget must be nonzero"));
        }
        if self.max_arg_assembly_bytes > Self::M0_TEST_ARG_ASSEMBLY_BYTES {
            return Err(limit_error("argument assembly budget exceeds M0 maximum"));
        }
        if self.run_duration.is_zero()
            || self.per_tool_timeout.is_zero()
            || self.approval_expiry.is_zero()
        {
            return Err(limit_error("limit duration must be nonzero"));
        }
        if self.run_duration > Self::M0_TEST_MAX_DURATION {
            return Err(limit_error("run duration exceeds practical maximum"));
        }
        if self.per_tool_timeout > Self::M0_TEST_MAX_DURATION {
            return Err(limit_error("tool timeout exceeds practical maximum"));
        }
        if self.approval_expiry > Self::M0_TEST_MAX_DURATION {
            return Err(limit_error("approval expiry exceeds practical maximum"));
        }
        Ok(())
    }

    /// Maps model-turn exhaustion to an explicit outcome.
    pub fn check_model_turns(&self, used: u32) -> Result<(), AgentError> {
        if used >= self.max_model_turns_per_run {
            return Err(limit_error("model turn budget exhausted"));
        }
        Ok(())
    }

    /// Maps per-run tool-call exhaustion to an explicit outcome.
    pub fn check_tool_calls_for_run(&self, used: u32) -> Result<(), AgentError> {
        if used >= self.max_tool_calls_per_run {
            return Err(limit_error("tool call budget for run exhausted"));
        }
        Ok(())
    }

    /// Maps per-turn tool-call exhaustion to an explicit outcome.
    pub fn check_tool_calls_for_turn(&self, used: u32) -> Result<(), AgentError> {
        if used >= self.max_tool_calls_per_turn {
            return Err(limit_error("tool call budget for turn exhausted"));
        }
        Ok(())
    }

    /// Maps argument-assembly exhaustion to an explicit outcome.
    pub fn check_arg_assembly_bytes(&self, len: usize) -> Result<(), AgentError> {
        if len > self.max_arg_assembly_bytes {
            return Err(limit_error("tool argument assembly budget exhausted"));
        }
        Ok(())
    }

    /// Maps tool-output exhaustion to an explicit outcome.
    pub fn check_tool_output_bytes(&self, len: usize) -> Result<(), AgentError> {
        if len > self.max_tool_output_bytes {
            return Err(limit_error("tool output budget exhausted"));
        }
        Ok(())
    }

    /// Maps retained-context exhaustion to an explicit outcome.
    pub fn check_context_items(&self, count: usize) -> Result<(), AgentError> {
        if count > self.retained_context_items {
            return Err(limit_error("retained context budget exhausted"));
        }
        Ok(())
    }

    /// Maps concurrency exhaustion to an explicit outcome.
    pub fn check_concurrent_ops(&self, active: usize) -> Result<(), AgentError> {
        if active >= self.max_concurrent_ops {
            return Err(limit_error("concurrent operation budget exhausted"));
        }
        Ok(())
    }

    /// Maps data-channel exhaustion to an explicit outcome.
    pub fn check_event_data_buffered(&self, buffered: usize) -> Result<(), AgentError> {
        if buffered >= self.event_data_capacity {
            return Err(limit_error("event data channel exhausted"));
        }
        Ok(())
    }

    /// Maps control-channel exhaustion to an explicit outcome.
    pub fn check_event_control_buffered(&self, buffered: usize) -> Result<(), AgentError> {
        if buffered >= self.event_control_capacity {
            return Err(limit_error("event control channel exhausted"));
        }
        Ok(())
    }

    /// Maps run-duration exhaustion to an explicit outcome.
    /// `elapsed` must come from a monotonic clock; wall-clock
    /// timestamps are for records, not timeout arithmetic. Callers combine
    /// this with `Instant::checked_add` and treat a failed checked add as
    /// exhaustion rather than panicking.
    pub fn check_run_elapsed(&self, elapsed: Duration) -> Result<(), AgentError> {
        if elapsed >= self.run_duration {
            return Err(limit_error("run duration exhausted"));
        }
        Ok(())
    }
}

fn limit_error(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::ResourceLimit,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static safe limit message builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m0_test_constants_match_lock_table() {
        let limits = Limits::m0_test();
        assert_eq!(limits.max_model_turns_per_run, 8);
        assert_eq!(limits.max_tool_calls_per_run, 16);
        assert_eq!(limits.max_tool_calls_per_turn, 8);
        assert_eq!(limits.run_duration, Duration::from_secs(300));
        assert_eq!(limits.per_tool_timeout, Duration::from_secs(60));
        assert_eq!(limits.retained_context_items, 128);
        assert_eq!(limits.max_arg_assembly_bytes, 65_536);
        assert_eq!(limits.max_tool_output_bytes, 262_144);
        assert_eq!(limits.event_data_capacity, 1_024);
        assert_eq!(limits.event_control_capacity, 128);
        assert_eq!(limits.max_concurrent_ops, 8);
        assert_eq!(limits.approval_expiry, Duration::from_secs(120));
        limits.validate().expect("M0-test budgets are valid");
    }

    #[test]
    fn zero_budget_never_means_infinity() {
        let mut limits = Limits::m0_test();
        limits.max_tool_calls_per_run = 0;
        assert!(limits.validate().is_err());
        let mut limits = Limits::m0_test();
        limits.run_duration = Duration::ZERO;
        assert!(limits.validate().is_err());
    }

    #[test]
    fn arg_assembly_budget_is_capped_at_the_m0_maximum() {
        let mut limits = Limits::m0_test();
        limits.max_arg_assembly_bytes = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;
        limits
            .validate()
            .expect("the M0 assembly maximum is accepted");

        limits.max_arg_assembly_bytes = Limits::M0_TEST_ARG_ASSEMBLY_BYTES + 1;
        let error = limits
            .validate()
            .expect_err("above the M0 assembly maximum is rejected");
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
        assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    }

    #[test]
    fn duration_maxima_are_per_duration_and_short_runs_stay_valid() {
        assert_eq!(
            Limits::M0_TEST_MAX_DURATION,
            Duration::from_secs(24 * 60 * 60)
        );

        let mut limits = Limits::m0_test();
        limits.run_duration = Limits::M0_TEST_MAX_DURATION;
        limits.per_tool_timeout = Limits::M0_TEST_MAX_DURATION;
        limits.approval_expiry = Limits::M0_TEST_MAX_DURATION;
        limits
            .validate()
            .expect("each duration at the practical maximum is accepted");

        let mut limits = Limits::m0_test();
        limits.run_duration = Limits::M0_TEST_MAX_DURATION + Duration::from_secs(1);
        assert!(
            limits.validate().is_err(),
            "run duration above the practical maximum is rejected"
        );

        let mut limits = Limits::m0_test();
        limits.per_tool_timeout = Limits::M0_TEST_MAX_DURATION + Duration::from_secs(1);
        assert!(
            limits.validate().is_err(),
            "tool timeout above the practical maximum is rejected"
        );

        let mut limits = Limits::m0_test();
        limits.approval_expiry = Limits::M0_TEST_MAX_DURATION + Duration::from_secs(1);
        assert!(
            limits.validate().is_err(),
            "approval expiry above the practical maximum is rejected"
        );

        let mut limits = Limits::m0_test();
        limits.run_duration = Duration::MAX;
        assert!(
            limits.validate().is_err(),
            "a type-finite but effectively unbounded duration is rejected"
        );

        // A short run budget may carry the longer default operation
        // timeouts: durations are validated independently and the effective
        // deadline is the minimum, so the runtime must expire this run at
        // 10 ms, never extend it to the tool or approval defaults.
        let mut limits = Limits::m0_test();
        limits.run_duration = Duration::from_millis(10);
        limits.per_tool_timeout = Duration::from_secs(60);
        limits.approval_expiry = Duration::from_secs(120);
        limits
            .validate()
            .expect("longer operation defaults under a short run are valid");
        assert!(limits.check_run_elapsed(Duration::from_millis(9)).is_ok());
        assert!(
            limits.check_run_elapsed(Duration::from_millis(10)).is_err(),
            "the run budget wins over the longer operation defaults"
        );
        assert!(limits.check_run_elapsed(Duration::from_secs(60)).is_err());
    }

    #[test]
    fn limit_exhaustion_maps_to_resource_limit_without_retry() {
        let limits = Limits::m0_test();
        let cases: Vec<(&str, Result<(), AgentError>)> = vec![
            ("turns", limits.check_model_turns(8)),
            ("calls-run", limits.check_tool_calls_for_run(16)),
            ("calls-turn", limits.check_tool_calls_for_turn(8)),
            ("assembly", limits.check_arg_assembly_bytes(65_537)),
            ("output", limits.check_tool_output_bytes(262_145)),
            ("context", limits.check_context_items(129)),
            ("concurrency", limits.check_concurrent_ops(8)),
            ("data-channel", limits.check_event_data_buffered(1_024)),
            ("control-channel", limits.check_event_control_buffered(128)),
            (
                "duration",
                limits.check_run_elapsed(Duration::from_secs(300)),
            ),
        ];
        for (name, result) in cases {
            let error = result.unwrap_err();
            assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{name}");
            assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{name}");
        }
    }

    #[test]
    fn budget_boundary_below_limit_passes() {
        let limits = Limits::m0_test();
        assert!(limits.check_model_turns(7).is_ok());
        assert!(limits.check_tool_calls_for_run(15).is_ok());
        assert!(limits.check_tool_calls_for_turn(7).is_ok());
        assert!(limits.check_arg_assembly_bytes(65_536).is_ok());
        assert!(limits.check_tool_output_bytes(262_144).is_ok());
        assert!(limits.check_concurrent_ops(7).is_ok());
        assert!(limits.check_run_elapsed(Duration::from_secs(299)).is_ok());
    }
}
