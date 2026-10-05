//! `nexus-tools`: real tool executors behind narrow filesystem jails.
//!
//! This crate owns the real [`nexus_core::ToolPort`] implementations:
//! [`ScopedReader`] (read-only `host_read`) and [`ScopedWriter`] (jailed
//! `host_write`, approval-gated like any mutation), both at the M0
//! revision so the existing scoped policy authorizes reads automatically
//! and routes writes through approval. Every path is resolved against one
//! canonical root and refused outside it; admission-time policy checks
//! stay the outer boundary, the jail is the inner one.
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
//! stays at registration.

#![forbid(unsafe_code)]

pub mod fs;

pub use fs::{ScopedReader, ScopedWriter};
