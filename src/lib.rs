//! RustyClaw library crate — exposes the SDK module for integration tests and embedding.
//!
//! Modules that depend on the TUI (`tui`, `session`, `spawn`) and compile-time
//! binary-only env vars (`deeplink`) are excluded — they only compile as part
//! of the `rustyclaw` binary.

pub mod api;
pub mod auth;
pub mod autocommit;
pub mod autofix;
pub mod browser;
pub mod commands;
pub mod compact;
pub mod config;
pub mod cost;
pub mod distro;
pub mod hooks;
pub mod mcp;
pub mod memory;
pub mod permissions;
pub mod query_engine;
pub mod rag;
pub mod router;
pub mod sandbox;
pub mod settings;
pub mod skills;
pub mod tools;
pub mod voice;
pub mod watch;

pub mod sdk;

/// Minimal re-export of the `tui::diff` submodule for integration tests.
/// The rest of `tui` is binary-only and not exposed here.
pub mod tui {
    #[path = "diff.rs"]
    pub mod diff;
}
