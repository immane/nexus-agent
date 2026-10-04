//! Approval policy for the M0 single-run loop.
//!
//! Only host-authorized scoped reads/searches proceed automatically; every
//! model-directed mutation and every command execution requires an explicit
//! grant. The auto-approved tool-name set is an M0-test stand-in (not a
//! product default) and is reviewed at the stage gate.

use nexus_core::{M0_REVISION, ToolId};

/// Policy boundary: which tools need an explicit approval grant.
#[derive(Debug, Clone)]
pub struct Policy {
    auto_tools: Vec<String>,
    revision: u32,
}

impl Policy {
    /// M0-test policy: `host_read`/`host_search` are automatic, everything
    /// else requires confirmation. Revision is [`M0_REVISION`].
    #[must_use]
    pub fn m0_test() -> Self {
        Self {
            auto_tools: vec!["host_read".to_owned(), "host_search".to_owned()],
            revision: M0_REVISION,
        }
    }

    /// Builds a policy with an explicit auto-approved tool-name set.
    #[must_use]
    pub fn new(auto_tools: Vec<String>, revision: u32) -> Self {
        Self {
            auto_tools,
            revision,
        }
    }

    /// Returns true when the tool requires an approval grant before dispatch.
    #[must_use]
    pub fn requires_approval(&self, tool: &ToolId) -> bool {
        !self.auto_tools.iter().any(|name| name == tool.name())
    }

    /// Returns the policy revision bound into approval grants.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_reads_are_automatic_mutations_require_confirmation() {
        let policy = Policy::m0_test();
        let read = ToolId::new("host_read", M0_REVISION).expect("valid");
        let search = ToolId::new("host_search", M0_REVISION).expect("valid");
        let write = ToolId::new("host_write", M0_REVISION).expect("valid");
        let exec = ToolId::new("host_exec", M0_REVISION).expect("valid");
        assert!(!policy.requires_approval(&read));
        assert!(!policy.requires_approval(&search));
        assert!(policy.requires_approval(&write));
        assert!(policy.requires_approval(&exec));
        assert_eq!(policy.revision(), M0_REVISION);
    }
}
