/// TUI — split-panel layout matching the original oxideclaw (React/Ink) look.
/// Left panel: file listing.  Right panel: chat / welcome.
/// Uses ratatui + crossterm with alternate screen.
pub mod app;
pub mod diff;
pub mod events;
pub mod markdown;
pub mod render;
pub mod run;

pub use run::run_tui;
