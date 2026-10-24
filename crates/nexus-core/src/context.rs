//! Provider-independent context configuration. These values describe policy;
//! they do not imply that token counting or automatic compaction is available.

use crate::{AgentError, ErrorCategory, RetryGuidance};

/// Requested history-reduction method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCompactionStrategy {
    /// Replace eligible completed history with a bounded derived summary.
    Summarize,
    /// Remove eligible completed history without generating a summary.
    Truncate,
}

/// Validated thresholds, expressed as whole percentages of the context window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextManagementPolicy {
    trigger_percent: u8,
    target_percent: u8,
    strategy: ContextCompactionStrategy,
}

impl Default for ContextManagementPolicy {
    fn default() -> Self {
        Self {
            trigger_percent: 65,
            target_percent: 10,
            strategy: ContextCompactionStrategy::Summarize,
        }
    }
}

impl ContextManagementPolicy {
    /// Requires `0 < target < trigger < 100`. Targets are best-effort:
    /// required instructions and execution evidence must never be discarded.
    pub fn new(
        trigger_percent: u8,
        target_percent: u8,
        strategy: ContextCompactionStrategy,
    ) -> Result<Self, AgentError> {
        if target_percent == 0 || target_percent >= trigger_percent || trigger_percent >= 100 {
            return Err(invalid("context management thresholds are invalid"));
        }
        Ok(Self {
            trigger_percent,
            target_percent,
            strategy,
        })
    }

    /// Input occupancy percentage at which reduction is requested.
    #[must_use]
    pub fn trigger_percent(self) -> u8 {
        self.trigger_percent
    }

    /// Desired occupancy after reduction, subordinate to safety requirements.
    #[must_use]
    pub fn target_percent(self) -> u8 {
        self.target_percent
    }

    /// Requested reduction method, not a report of execution support.
    #[must_use]
    pub fn strategy(self) -> ContextCompactionStrategy {
        self.strategy
    }
}

/// Provenance of a context window value, not of token usage measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWindowSource {
    /// User-supplied limit.
    Configured,
    /// Limit reported by the serving provider.
    ProviderReported,
    /// Model metadata; not proof of the serving instance's active window.
    ModelMetadata,
}

/// Nonzero token window with explicit provenance. Unknown windows use `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextWindow {
    tokens: u32,
    source: ContextWindowSource,
}

impl ContextWindow {
    /// Builds a known window without inferring anything from model names.
    pub fn new(tokens: u32, source: ContextWindowSource) -> Result<Self, AgentError> {
        if tokens == 0 {
            return Err(invalid("context window must be positive"));
        }
        Ok(Self { tokens, source })
    }

    /// Window size in tokens, including input and generated output.
    #[must_use]
    pub fn tokens(self) -> u32 {
        self.tokens
    }

    /// Origin of this window value.
    #[must_use]
    pub fn source(self) -> ContextWindowSource {
        self.source
    }
}

/// Immutable per-run settings. Discovery, counting, and execution are separate
/// capabilities; carrying these settings must not claim compaction occurred.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextManagementSettings {
    /// Requested thresholds and method.
    pub policy: ContextManagementPolicy,
    /// Known window, or `None` when no limit has been established.
    pub window: Option<ContextWindow>,
}

fn invalid(message: &'static str) -> AgentError {
    AgentError::new(
        ErrorCategory::InvalidInput,
        message,
        RetryGuidance::DoNotRetry,
    )
    .expect("static context diagnostic is safe")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_do_not_invent_a_window() {
        let settings = ContextManagementSettings::default();
        assert_eq!(settings.window, None);
        assert_eq!(settings.policy.trigger_percent(), 65);
        assert_eq!(settings.policy.target_percent(), 10);
        assert_eq!(
            settings.policy.strategy(),
            ContextCompactionStrategy::Summarize
        );
    }

    #[test]
    fn thresholds_are_ordered_and_support_five_percent() {
        for (trigger, target) in [(0, 0), (65, 0), (65, 65), (65, 66), (100, 5), (255, 5)] {
            assert!(
                ContextManagementPolicy::new(trigger, target, ContextCompactionStrategy::Summarize)
                    .is_err()
            );
        }
        let policy =
            ContextManagementPolicy::new(65, 5, ContextCompactionStrategy::Truncate).unwrap();
        assert_eq!(policy.target_percent(), 5);
        assert_eq!(policy.strategy(), ContextCompactionStrategy::Truncate);
    }

    #[test]
    fn window_retains_provenance_and_rejects_zero() {
        assert!(ContextWindow::new(0, ContextWindowSource::Configured).is_err());
        for source in [
            ContextWindowSource::Configured,
            ContextWindowSource::ProviderReported,
            ContextWindowSource::ModelMetadata,
        ] {
            let window = ContextWindow::new(94_208, source).unwrap();
            assert_eq!(window.tokens(), 94_208);
            assert_eq!(window.source(), source);
        }
    }
}
