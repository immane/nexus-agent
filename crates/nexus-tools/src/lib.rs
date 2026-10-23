//! `nexus-tools`: real tool executors behind narrow filesystem jails.
//!
//! This crate owns the real [`nexus_core::ToolPort`] implementations:
//! [`ScopedReader`] (`host_read`), [`ScopedLister`] (`host_list`),
//! [`ScopedSearcher`] (`host_search`), [`ScopedWriter`] (`host_write`), and
//! [`ScopedPatcher`] (`host_patch`), and [`SandboxedExecutor`] (`host_exec`,
//! OS-sandboxed argv), all at the M0 revision. Strict adapters use one jail;
//! [`strict_tools`] and [`development_tools`] provide the shared host
//! compositions used by frontends. [`development_file_tools`] accepts exact
//! canonical runtime scopes for external paths too. Runtime policy decides
//! approvals, not the adapters.
//! `host_exec` runs only behind an available mandatory
//! platform sandbox and never falls back to an unsandboxed process.
//!
//! # Jail model
//!
//! - The root is canonicalized and bound to a directory descriptor at
//!   construction (symlinks in the root itself are resolved there).
//! - Calls resolve policy paths first, then walk the resulting relative path
//!   from the bound descriptor without following changed symlink components.
//!   Swapped components fail closed rather than redirecting access outside
//!   the root.
//! - Only regular files are read. Missing files, directories, special
//!   files, and non-UTF-8 content fail with `Failed`/`Unknown`/`Uncertain`
//!   and a static diagnostic that never echoes the path.
//! - Content is cut to the effective output budget on a UTF-8 boundary
//!   with the truncation flag set, exactly like bounded fake output.
//! - Reads are chunked and observe live cancellation between chunks.
//!
//! # Residual risks (documented, not hidden)
//!
//! Descriptor-relative access binds each opened object, but external writers
//! can still race the final compare-and-rename window. Updates are staged in
//! the destination directory and are atomically visible on rename; no
//! crash-durability guarantee is made. Replacements preserve supported
//! metadata and fail closed when required metadata cannot be retained.
//!
//! # Dependency justification
//!
//! `nexus-core` supplies the port, outcome, and budget types. `serde_json`
//! only decodes the already-validated argument object; schema validation
//! stays at registration. `nexus-permissions` shares canonical directory and
//! protected-path rules with runtime policy without a runtime/tools dependency.

#![forbid(unsafe_code)]

#[cfg(feature = "container")]
pub mod container;
#[cfg(all(feature = "container-worker", unix))]
pub mod container_worker_protocol;
mod development;
mod exec;
mod fs_access;
pub use development::{development_file_tools, development_tools, strict_tools};
pub mod fs;

pub use exec::SandboxedExecutor;
pub use fs::{ScopedLister, ScopedPatcher, ScopedReader, ScopedSearcher, ScopedWriter};
