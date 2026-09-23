pub mod chart;
pub mod cli;
pub mod client;
pub mod config;
pub mod export;
pub mod lsp;
pub mod model;
pub mod palette;
pub mod query_library;
pub mod query_ui;
pub mod tui;

pub fn safe_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}
