//! Library implementation for `ghis`.
//!
//! The binary is intentionally thin: keeping the repository, configuration,
//! process and shell layers available from a library makes the safety-critical
//! resolution rules easy to test without spawning an interactive terminal.

pub mod app;
pub mod config;
pub mod credential;
pub mod git;
pub mod github;
pub mod process;
pub mod repo;
pub mod shell;
pub mod signing;
pub mod state;
pub mod tui;

pub const SCHEMA_VERSION: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
