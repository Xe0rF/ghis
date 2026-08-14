//! Library implementation for `ghis`.
//!
//! The binary is intentionally thin: keeping the repository, configuration,
//! process and shell layers available from a library makes the safety-critical
//! resolution rules easy to test without spawning an interactive terminal.

pub mod agent;
pub mod agent_context;
pub mod app;
pub mod config;
pub mod credential;
pub mod diagnostics;
pub mod git;
pub mod github;
pub mod onboarding;
pub mod process;
pub mod repo;
pub mod shell;
pub mod signing;
pub mod state;

pub const SCHEMA_VERSION: u32 = 1;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit: ",
    env!("GHIS_BUILD_GIT_COMMIT"),
    "-",
    env!("GHIS_BUILD_GIT_STATE"),
    "\ntag: ",
    env!("GHIS_BUILD_GIT_TAG"),
    "\nbuilt: ",
    env!("GHIS_BUILD_TIME"),
    "\nSOURCE_DATE_EPOCH: ",
    env!("GHIS_BUILD_SOURCE_DATE_EPOCH"),
    "\ntarget: ",
    env!("GHIS_BUILD_TARGET"),
    "\nprofile: ",
    env!("GHIS_BUILD_PROFILE"),
);
