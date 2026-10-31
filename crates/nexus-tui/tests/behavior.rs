//! Behavioral integration tests share one executable to avoid repeated linking.

mod cov_decisions;
mod cov_render;
mod cov_render_layout;
mod cov_sanitize;
mod cov_sanitize_edges;
mod cov_state_retention;
mod cov_state_sequences;
mod cov_state_viewport;
mod cov_terminal;
mod credential_startup;
mod demo_startup;
mod discovery_footer;
mod file_tool_presentation;
#[cfg(feature = "demo")]
mod pty_render;
