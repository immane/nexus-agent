//! `nexus-tools`: real tool executors behind narrow filesystem jails.
//!
//! This crate owns the real [`nexus_core::ToolPort`] implementations:
//! [`ScopedReader`] (`host_read`), [`ScopedLister`] (`host_list`),
//! [`ScopedSearcher`] (`host_search`), [`ScopedWriter`] (`host_write`), and
//! [`ScopedPatcher`] (`host_patch`), and [`SandboxedExecutor`] (`host_exec`,
//! OS-sandboxed argv), all at the M0 revision. Strict adapters use one jail;
//! [`development_file_tools`] accepts exact canonical runtime scopes for
//! external paths too. Runtime policy decides approvals, not the adapters.
//! `host_exec` runs only behind an available mandatory
//! platform sandbox and never falls back to an unsandboxed process.
//!
//! # Jail model
//!
//! - The root is canonicalized once at construction (symlinks in the root
//!   itself are resolved there, not per call).
//! - Every call joins the root with the admitted relative-or-inside path,
//!   canonicalizes the result (following symlinks), and requires the root
//!   prefix. A path that escapes is refused with `Denied`/`NotStarted`
//!   before any read, never executed.
//! - Only regular files are read. Missing files, directories, special
//!   files, and non-UTF-8 content fail with `Failed`/`Unknown`/`Uncertain`
//!   and a static diagnostic that never echoes the path.
//! - Content is cut to the effective output budget on a UTF-8 boundary
//!   with the truncation flag set, exactly like bounded fake output.
//! - Reads are chunked and observe live cancellation between chunks.
//!
//! # Residual risks (documented, not hidden)
//!
//! Canonicalize-then-read has a TOCTOU window: a path swapped between the
//! prefix check and the open could escape. Readers that need hard
//! guarantees must use OS-scoped opens (`openat2` on Linux) instead.
//! Reads never write, so the worst case is bounded disclosure, never
//! modification.
//!
//! # Dependency justification
//!
//! `nexus-core` supplies the port, outcome, and budget types. `serde_json`
//! only decodes the already-validated argument object; schema validation
//! stays at registration. `nexus-permissions` shares canonical directory and
//! protected-path rules with runtime policy without a runtime/tools dependency.

#![forbid(unsafe_code)]

mod development;
mod exec;
pub use development::development_file_tools;
pub mod fs;

pub use exec::SandboxedExecutor;
pub use fs::{ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter};
