use crate::{export::Format, safe_text};
use clap::{CommandFactory, Parser};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use std::path::PathBuf;
use tui_textarea::{CursorMove, TextArea};

/// The parser is also the command palette's help catalog, so accepted syntax and help stay aligned.
#[derive(Parser)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_help_subcommand = true
)]
pub enum Command {
    /// Run the entire query editor against the active target (F5).
    #[command(
        after_long_help = "Example: run\nRequires a cluster and database. Only one query runs at a time.\nEdits made after execution starts do not change the submitted query."
    )]
    Run,
    /// Cancel the active query locally and request best-effort server cancellation.
    #[command(
        after_long_help = "Example: cancel\nEquivalent to Ctrl-C while a query is running. Server cancellation is not guaranteed."
    )]
    Cancel,
    /// Select a configured cluster alias or an explicit HTTPS endpoint.
    #[command(
        after_long_help = "Examples:\n  target dev --database Logs\n  target https://cluster.region.kusto.windows.net --database Logs --tenant TENANT\nSelecting a target does not modify config.toml. Its schema is fetched for the language server."
    )]
    Target {
        /// Configured alias or HTTPS cluster origin (no URL path or credentials).
        cluster: String,
        /// Database name; defaults to the selected alias's configured database.
        #[arg(short, long)]
        database: Option<String>,
        /// Azure Entra tenant; defaults to the alias tenant, then Azure CLI's current tenant.
        #[arg(long)]
        tenant: Option<String>,
    },
    /// Select a database on the current cluster and refresh its language-service schema.
    #[command(
        after_long_help = "Example: database Logs\nUse metadata to discover databases first. Quote database names containing spaces."
    )]
    Database {
        /// Database name.
        name: String,
    },
    /// Discover databases and refresh the active database's schema (F6).
    #[command(
        after_long_help = "Example: metadata\nUses authenticated read-only management requests. Run az login outside the app first.\nSyntax-only language features work without schema; table/column checking requires schema."
    )]
    Metadata,
    /// Open a UTF-8 query file in a new tab (or focus its existing tab).
    #[command(
        after_long_help = "Examples:\n  open query.kql\n  open \"/path/to/query file.kql\"\nRelative paths use the launch directory. Maximum file size: 4 MiB.\nOther tabs and unsaved edits are retained. Ctrl-O browses query_path."
    )]
    Open {
        /// Query file path. Quote paths containing spaces; use an absolute path instead of ~.
        path: PathBuf,
        /// Accepted for compatibility; opening now preserves existing tabs.
        #[arg(long)]
        force: bool,
    },
    /// Fuzzy-search the recursive query library and preview its documentation (Ctrl-O).
    #[command(
        after_long_help = "Example: queries\nConfigure query_path in config.toml. Enter opens a new tab or focuses an already open file.\nCtrl-L refreshes the library; PageUp/PageDown scroll descriptions and parameter definitions."
    )]
    Queries,
    /// Reload all query files under query_path.
    #[command(
        after_long_help = "Example: reload-queries\nRecursively reads .kql, .csl and .kusto files without following symbolic links."
    )]
    ReloadQueries,
    /// Create a new query tab (Ctrl-N).
    #[command(
        after_long_help = "Example: new\nNew tabs have independent text, undo history and execution parameter values."
    )]
    New,
    /// Switch to the next query tab (Alt-Right or Ctrl-PageDown).
    #[command(
        after_long_help = "Example: next-tab\nUnsaved text and cursor positions are preserved."
    )]
    NextTab,
    /// Switch to the previous query tab (Alt-Left or Ctrl-PageUp).
    #[command(
        after_long_help = "Example: previous-tab\nUnsaved text and cursor positions are preserved."
    )]
    PreviousTab,
    /// Close the current query tab (Ctrl-W), protecting unsaved edits.
    #[command(
        after_long_help = "Example: close\nUse close --force only to discard this tab's unsaved edits. Other tabs are retained."
    )]
    Close {
        #[arg(long)]
        force: bool,
    },
    /// Edit the current query's parameter definitions and in-memory execution values (F4).
    #[command(
        after_long_help = "Example: parameters\nF3 adds a definition; Enter edits; Ctrl-S applies. Native KQL declarations and documentation\nare kept together. Execution values are not saved; optional defaults are saved."
    )]
    Parameters,
    /// Save to query_path with description and parameter prompts (Ctrl-S).
    #[command(
        after_long_help = "Example: save-query\nChoose a relative nested path, describe the query, then define parameters.\nExisting files require explicit confirmation. Execution values are never written."
    )]
    SaveQuery,
    /// Save to an explicit file through the documentation/parameter wizard.
    #[command(
        after_long_help = "Examples:\n  save team/query.kql\n  save /absolute/path/query.kql --force\nRelative paths use the launch directory, even when query_path is configured.\nThe wizard prompts for documentation/parameters and always confirms replacement.\nCtrl-S / save-query uses the configured query library instead."
    )]
    Save {
        /// Explicit destination path; quote paths containing spaces.
        path: PathBuf,
        /// Accepted for compatibility; the wizard still confirms replacement.
        #[arg(long)]
        force: bool,
    },
    /// Filter fetched rows by case-insensitive text across all columns.
    #[command(
        after_long_help = "Examples:\n  filter warning\n  filter \"connection failed\"\n  filter\nWith no text, clears only the text filter. Does not rerun or modify the server query."
    )]
    Filter {
        /// Text to find. Spaces are allowed; matching is case-insensitive.
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        text: Vec<String>,
    },
    /// Apply a typed comparison or substring filter to one fetched column.
    #[command(
        after_long_help = "Examples:\n  column 2 gt -1.25\n  column 0 contains \"customer name\"\n  column 3 eq null\nIndices are zero-based. Numeric comparisons are exact; datetime values use RFC3339.\nNull sorts below non-null. Filtering is local, not a server query rewrite."
    )]
    Column {
        /// Zero-based result column index (shown in the results header).
        index: usize,
        /// Comparison operation; contains performs case-sensitive text matching.
        #[arg(value_parser = ["eq", "lt", "gt", "contains"])]
        op: String,
        /// Typed value, text, or the literal null; quote values containing spaces.
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Clear local text/column filters and sorting; restore fetched row order.
    #[command(
        after_long_help = "Example: clear\nDoes not clear the editor or fetched results and does not execute another query."
    )]
    Clear,
    /// Export all fetched rows or the current local view of the selected primary table.
    #[command(
        after_long_help = "Examples:\n  export csv view \"results.csv\"\n  export json all \"/path/to/results.json\" --force --accept-partial\nCSV has headers; JSON includes schema and completion metadata; JSONL is positional arrays.\nExports never sample chart data or include only the visible screen. Existing files need --force."
    )]
    Export {
        /// File format: csv, schema-bearing json, or one positional array per jsonl line.
        #[arg(value_enum)]
        format: Format,
        /// all: fetched source order; view: current local filters and sort.
        #[arg(value_parser = ["all", "view"])]
        scope: String,
        /// Destination file path; quote paths containing spaces.
        path: PathBuf,
        /// Explicitly allow replacing an existing output file.
        #[arg(long)]
        force: bool,
        /// Explicitly accept incomplete/truncated results. Without this export is refused.
        #[arg(long)]
        accept_partial: bool,
    },
    /// Toggle table/chart display using Kusto Visualization metadata (F7).
    #[command(
        after_long_help = "Example: chart\nSupports timechart, linechart, scatterchart and barchart. Unsupported render options\nexplain the refusal and retain the table. Use a KQL render operator in the query.\nF3 / chart-options selects X, Y and series columns without rerunning the query."
    )]
    Chart,
    /// Select X, Y and series columns for the current result chart (F3).
    #[command(
        after_long_help = "Example: chart-options\nTab changes role; Up/Down highlights a column; Space selects; Enter applies.\nSelect one X, one or more numeric Y columns, and optional series grouping columns.\nR restores render metadata; Esc cancels. Overrides are per fetched table and reset\non new query results. Query text, table sorting, and exports are unchanged."
    )]
    ChartOptions,
    /// Toggle results/diagnostics, including query errors and LSP messages (F8).
    #[command(
        after_long_help = "Example: diagnostics\nLanguage-service errors also appear underlined and summarized in the query pane."
    )]
    Diagnostics,
    /// Show the active config path and language-server setup/troubleshooting.
    #[command(
        after_long_help = "Example: setup\nExplains multi-cluster TOML, --config PATH, az login, and [language_server] command/args.\nIt only displays help; it does not change configuration or credentials."
    )]
    Setup,
    /// Show keyboard shortcuts and workspace help (F1).
    #[command(
        after_long_help = "Example: help\nUse Up/Down or PageUp/PageDown to scroll, Esc to close."
    )]
    Help,
    /// Exit the workspace, protecting unsaved query text.
    #[command(
        after_long_help = "Examples:\n  quit\n  quit --force\nWithout --force, modified editor text must be saved first. Any active query is cancelled."
    )]
    Quit {
        /// Explicitly discard unsaved editor changes.
        #[arg(long)]
        force: bool,
    },
}

struct Entry {
    name: String,
    summary: String,
    help: String,
}

pub struct Palette {
    input: TextArea<'static>,
    entries: Vec<Entry>,
    pub selected: usize,
    scroll: u16,
    pub error: Option<String>,
}

pub enum Action {
    Stay,
    Close,
    Submit(String),
}

impl Palette {
    pub fn new(prefix: &str) -> Self {
        let mut command = Command::command();
        let entries = command
            .get_subcommands_mut()
            .map(|c| {
                let name = c.get_name().to_owned();
                let summary = c.get_about().map(ToString::to_string).unwrap_or_default();
                let help = c.render_long_help().to_string();
                Entry {
                    name,
                    summary,
                    help,
                }
            })
            .collect();
        let mut input = TextArea::from([prefix]);
        input.move_cursor(CursorMove::End);
        Self {
            input,
            entries,
            selected: 0,
            scroll: 0,
            error: None,
        }
    }
    pub fn text(&self) -> &str {
        &self.input.lines()[0]
    }
    fn word(&self) -> &str {
        self.text().split_whitespace().next().unwrap_or("")
    }
    fn exact(&self) -> Option<usize> {
        self.entries.iter().position(|e| e.name == self.word())
    }
    fn matches(&self) -> Vec<usize> {
        let needle = self.word().to_lowercase();
        let mut matches: Vec<_> = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                fuzzy_score(&e.name, &needle)
                    .or_else(|| fuzzy_score(&e.summary.to_lowercase(), &needle).map(|s| s + 10000))
                    .map(|score| (score, i))
            })
            .collect();
        matches.sort_by_key(|m| *m);
        matches.into_iter().map(|(_, i)| i).collect()
    }
    fn pick(&mut self) {
        let matches = self.matches();
        if let Some(i) = matches.get(self.selected.min(matches.len().saturating_sub(1))) {
            let remainder = self
                .text()
                .trim_start()
                .get(self.word().len()..)
                .unwrap_or("")
                .trim_start();
            let text = format!("{} {remainder}", self.entries[*i].name);
            self.input = TextArea::from([text]);
            self.input.move_cursor(CursorMove::End);
            self.selected = 0;
            self.scroll = 0;
            self.error = None;
        } else {
            self.error = Some(
                "No matching command. Backspace to change the search, or Esc to close.".into(),
            );
        }
    }
    pub fn paste(&mut self, text: &str) {
        self.input.insert_str(text.replace(['\n', '\r'], " "));
        self.reset_search();
    }
    fn reset_search(&mut self) {
        self.selected = 0;
        self.scroll = 0;
        self.error = None;
    }
    pub fn key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => return Action::Close,
            KeyCode::Enter if self.exact().is_some() => {
                return Action::Submit(self.text().to_owned());
            }
            KeyCode::Enter | KeyCode::Tab if self.exact().is_none() => self.pick(),
            KeyCode::Tab => {
                if !self.text().ends_with(char::is_whitespace) {
                    self.input.move_cursor(CursorMove::End);
                    self.input.insert_str(" ");
                }
            }
            KeyCode::Down if self.exact().is_none() => {
                self.selected = (self.selected + 1).min(self.matches().len().saturating_sub(1));
                self.scroll = 0;
            }
            KeyCode::Up if self.exact().is_none() => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll = 0;
            }
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(5),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
            _ => {
                if self.input.input(key) {
                    self.reset_search();
                }
            }
        }
        Action::Stay
    }
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let rect = Rect::new(
            area.x + 1,
            area.y + 1,
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        f.render_widget(Clear, rect);
        let border = Block::default()
            .borders(Borders::ALL)
            .title("Command palette")
            .border_style(Style::default().fg(Color::Cyan));
        let inner = border.inner(rect);
        f.render_widget(border, rect);
        let sections = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(if self.error.is_some() { 2 } else { 1 }),
        ])
        .split(inner);
        let exact = self.exact();
        let hint = if exact.is_some() {
            "Enter runs | Up/Down/PgUp/PgDn help | Esc closes"
        } else {
            "Type to fuzzy search | Up/Down choose | Tab/Enter pick | Esc closes"
        };
        f.render_widget(
            Paragraph::new(hint).style(Style::default().fg(Color::Yellow)),
            sections[0],
        );
        self.input.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(if exact.is_some() {
                    "Command + arguments"
                } else {
                    "Search commands"
                }),
        );
        f.render_widget(&self.input, sections[1]);
        let matches = self.matches();
        let chosen = exact.or_else(|| matches.get(self.selected).copied());
        let help_area = if exact.is_none() {
            // Stacked panes remain readable in narrow terminals; wider screens show list + details.
            let pieces = if inner.width < 85 {
                Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).split(sections[2])
            } else {
                Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)])
                    .split(sections[2])
            };
            let items: Vec<_> = matches
                .iter()
                .map(|i| {
                    let e = &self.entries[*i];
                    ListItem::new(Line::from(format!("{}  {}", e.name, e.summary)))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Commands ({})", matches.len())),
                )
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                .highlight_symbol("> ");
            f.render_stateful_widget(
                list,
                pieces[0],
                &mut ListState::default().with_selected(Some(self.selected)),
            );
            pieces[1]
        } else {
            sections[2]
        };
        let help = chosen
            .map(|i| self.entries[i].help.as_str())
            .unwrap_or("No commands match. Try a shorter search (for example: exp, db, run).");
        let max_scroll = Paragraph::new(help)
            .wrap(Wrap { trim: false })
            .line_count(help_area.width.saturating_sub(2))
            .saturating_sub(help_area.height.saturating_sub(2) as usize)
            .min(u16::MAX as usize) as u16;
        self.scroll = self.scroll.min(max_scroll);
        f.render_widget(
            Paragraph::new(help)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Syntax, arguments and examples"),
                )
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            help_area,
        );
        let footer = self.error.as_deref().unwrap_or("Arguments use shell-style quotes, not shell expansion. Paths are relative to the launch directory.");
        f.render_widget(
            Paragraph::new(safe_text(footer))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(if self.error.is_some() {
                    Color::Red
                } else {
                    Color::DarkGray
                })),
            sections[3],
        );
    }
}

pub fn fuzzy_score(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let mut chars = haystack.char_indices();
    let mut score = 0;
    let mut previous = None;
    for wanted in needle.chars() {
        let (index, _) = chars.find(|(_, c)| *c == wanted)?;
        score += match previous {
            Some(before) => index - before - 1,
            None => index * 2,
        };
        previous = Some(index);
    }
    Some(score + haystack.len().saturating_sub(needle.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::{Terminal, backend::TestBackend};
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn fuzzy_pick_does_not_execute_then_exact_submit_does() {
        let mut palette = Palette::new("xpt");
        assert_eq!(palette.entries[palette.matches()[0]].name, "export");
        assert!(matches!(palette.key(key(KeyCode::Enter)), Action::Stay));
        assert_eq!(palette.text(), "export ");
        palette.paste("json view \"my file.json\" --force");
        assert!(matches!(
            palette.key(key(KeyCode::Enter)),
            Action::Submit(_)
        ));
        assert!(matches!(
            Command::try_parse_from(shell_words::split(palette.text()).unwrap()).unwrap(),
            Command::Export { force: true, .. }
        ));
        assert!(Palette::new("unmatchablezz").matches().is_empty());
    }
    #[test]
    fn catalog_covers_parser_and_help_contains_actual_arguments() {
        let p = Palette::new("");
        let names: Vec<_> = p.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names.len(), Command::command().get_subcommands().count());
        for e in &p.entries {
            assert!(!e.summary.is_empty());
            assert!(e.help.contains("Example"));
        }
        let exp = p.entries.iter().find(|e| e.name == "export").unwrap();
        assert!(exp.help.contains("Usage: export "), "{}", exp.help);
        for expected in [
            "--accept-partial",
            "--force",
            "csv",
            "jsonl",
            "all",
            "view",
            "PATH",
        ] {
            assert!(exp.help.contains(expected), "{expected}: {}", exp.help);
        }
    }
    #[test]
    fn palette_navigation_exact_help_and_small_layouts() {
        let mut p = Palette::new("");
        p.key(key(KeyCode::Down));
        assert_eq!(p.selected, 1);
        p.key(key(KeyCode::Tab));
        assert_eq!(p.text(), "cancel ");
        p.key(key(KeyCode::Backspace));
        assert_eq!(p.text(), "cancel");
        assert!(p.exact().is_some());
        for (width, height) in [(120, 30), (50, 14), (80, 24)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut p = Palette::new("export csv");
            terminal.draw(|f| p.draw(f)).unwrap();
            let buffer = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(buffer.contains("Command palette"));
            assert!(buffer.contains("Command + arguments"));
        }
    }
}
