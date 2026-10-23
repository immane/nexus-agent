//! Behavioral integration tests share one executable to avoid repeated linking.

#[cfg(feature = "container-worker")]
mod container_worker;
mod exec_sandbox;
mod fs_patch;
mod fs_read_tools;
mod fs_scope;
mod fs_write;
