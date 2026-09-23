use crate::{
    chart::{self, ChartData, ChartKind},
    client::{Client, Metadata, normalize_schema},
    config::{self, Config, Overrides, Target, UiState},
    export,
    lsp::{self, TokenSpan},
    model::{FilterOp, QueryResult, ViewSpec, text},
    palette::{self, Command, Palette},
    query_library::{self, Documentation, Library, Values},
    query_ui::{Browser, Dialog, Field, Form, Parameters},
    safe_text,
};
use anyhow::{Context, Result, ensure};
use clap::Parser;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Axis, Block, Borders, Cell, Chart, Clear, Dataset, GraphType, List, ListItem, ListState,
        Paragraph, Row, Table, TableState, Wrap,
        canvas::{Canvas, Line as CanvasLine},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::IsTerminal,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tui_textarea::{CursorMove, TextArea};
use unicode_width::UnicodeWidthChar;

const HELP: &str = "\
DataExplorer - read-only Kusto workspace

F5 / Ctrl-R    Run entire editor (one active query)
Ctrl-C        Cancel active request locally + best-effort server cancel
Ctrl-Q        Quit (confirmation if editor modified)
Tab/Shift-Tab Cycle pane focus
Ctrl arrows   Resize dividers; mouse drag also supported
Alt-1/2/3     Collapse or restore pane
F9            Maximize / restore focused pane
F1            This help
F6            Discover databases and schema for current target
F7            Table / chart; F8 results / diagnostics
Ctrl-Space    LSP completion; F2 LSP hover
Ctrl-P or :   Fuzzy command palette (: only outside editor)
Ctrl-E        Export command prompt
Ctrl-O        Fuzzy query library (query_path); Ctrl-L reload
Ctrl-S        Save query: path, description, parameter definitions
Ctrl-N/W      New / close tab (unsaved changes protected)
Alt-Left/Right or Ctrl-PageUp/PageDown  Previous / next tab
F4            Edit current tab's parameters and execution values

Clusters: Up/Down selects; Enter activates target/discovers databases.
Editor: multiline, Shift-arrows selection, Ctrl-A select all,
Ctrl-Z undo, Ctrl-Y redo, Ctrl-X/C/V internal clipboard.
Ctrl-C copies only when no query is running.
Results: Up/Down/PgUp/PgDn, Left/Right columns, [/] primary table,
s toggle typed stable sort, / text filter, f column filter,
Enter large-cell detail (Up/Down scroll; Esc close).

Palette: type to fuzzy search; Up/Down selects; Tab/Enter picks.
Once a command name is exact, syntax/argument help is shown.
Enter runs it; PageUp/PageDown scroll help; Esc closes.

Commands (quote paths/names containing spaces):
run / cancel          metadata / chart / diagnostics
setup                 Active config path and LSP setup help
target ALIAS_OR_HTTPS --database DB [--tenant TENANT]
database DB
open PATH             save PATH [--force]
filter TEXT           column INDEX eq|lt|gt|contains VALUE
clear                 export csv|json|jsonl all|view PATH
export csv view PATH --force --accept-partial
quit                  quit --force

All/view means ALL fetched / CURRENT local view of selected table.
Sort/filter never changes server query. JSON is schema-bearing;
JSONL uses positional arrays. Partial exports require explicit acceptance.
Charts use fetched values, floating-point display coordinates, no sampling.
Secrets/credentials are never sent to the language server.";

struct TerminalGuard;
impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        )?;
        Ok(guard)
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

#[derive(Clone, Debug)]
pub struct Panes {
    pub widths: UiState,
    pub collapsed: [bool; 3],
    pub maximized: Option<usize>,
}
impl Panes {
    pub fn areas(&self, area: Rect) -> [Rect; 3] {
        let mut panes = [Rect::default(); 3];
        if let Some(i) = self.maximized {
            panes[i] = area;
            return panes;
        }
        let left = if self.collapsed[0] {
            0
        } else {
            self.widths
                .cluster_width
                .clamp(16, area.width.saturating_sub(30).max(16))
                .min(area.width)
        };
        panes[0] = Rect::new(area.x, area.y, left, area.height);
        let right = Rect::new(
            area.x + left,
            area.y,
            area.width.saturating_sub(left),
            area.height,
        );
        if self.collapsed[1] && self.collapsed[2] {
            return panes;
        }
        if self.collapsed[1] {
            panes[2] = right;
            return panes;
        }
        if self.collapsed[2] {
            panes[1] = right;
            return panes;
        }
        let top =
            ((right.height as u32 * self.widths.editor_percent.clamp(15, 85) as u32) / 100) as u16;
        let top = top.clamp(
            5.min(right.height),
            right.height.saturating_sub(5).max(5.min(right.height)),
        );
        panes[1] = Rect::new(right.x, right.y, right.width, top);
        panes[2] = Rect::new(
            right.x,
            right.y + top,
            right.width,
            right.height.saturating_sub(top),
        );
        panes
    }
}

struct Active {
    id: String,
    target: Target,
    cancel: CancellationToken,
    query_name: String,
}
enum Job {
    Query {
        id: String,
        target: Target,
        result: Result<QueryResult>,
    },
    View {
        generation: u64,
        rows: Result<Vec<usize>>,
        chart: Result<ChartData>,
    },
    Databases {
        endpoint: String,
        names: Vec<String>,
    },
    Schema {
        endpoint: String,
        database: String,
        schema: Value,
    },
    Opened {
        path: PathBuf,
        text: String,
    },
    Saved {
        path: PathBuf,
        tab_id: u64,
        revision: u64,
        text: String,
    },
    SaveFailed {
        tab_id: u64,
        error: String,
    },
    Library {
        generation: u64,
        result: Result<Library>,
    },
    Notice(String),
    Error(String),
}
struct Popup {
    title: String,
    text: String,
    scroll: u16,
}

struct TabState {
    editor: TextArea<'static>,
    top: usize,
    left: usize,
    dirty: bool,
    file: Option<PathBuf>,
    revision: u64,
    values: Values,
    disk_text: Option<String>,
}

struct App {
    config: Config,
    config_path: PathBuf,
    overrides: Overrides,
    target: Option<Target>,
    session_targets: std::collections::BTreeMap<String, Target>,
    cluster_items: Vec<(String, Option<String>)>,
    cluster_selected: usize,
    panes: Panes,
    focus: usize,
    editor: TextArea<'static>,
    editor_top: usize,
    editor_left: usize,
    version: i64,
    sent_version: i64,
    edited_at: Instant,
    dirty: bool,
    file: Option<PathBuf>,
    tab_id: u64,
    next_tab_id: u64,
    tab_order: Vec<u64>,
    inactive_tabs: BTreeMap<u64, TabState>,
    revision: u64,
    values: Values,
    disk_text: Option<String>,
    saving_tabs: BTreeSet<u64>,
    library: Library,
    library_generation: u64,
    library_loading: bool,
    browser: Option<Browser>,
    dialog: Option<Dialog>,
    tokens: Vec<TokenSpan>,
    diagnostics: Value,
    lsp_status: String,
    lsp_ready: bool,
    result: Option<Arc<QueryResult>>,
    result_target: Option<Target>,
    result_query_name: String,
    table: usize,
    view: ViewSpec,
    rows: Vec<usize>,
    view_generation: u64,
    view_pending: bool,
    view_valid: bool,
    chart: Option<ChartData>,
    chart_error: String,
    show_chart: bool,
    show_diagnostics: bool,
    selected: usize,
    column: usize,
    table_state: TableState,
    active: Option<Active>,
    messages: Vec<String>,
    prompt: Option<Palette>,
    popup: Option<Popup>,
    completions: Vec<Value>,
    completion_selected: usize,
    completion_version: i64,
    drag: Option<usize>,
    quit: bool,
}
impl App {
    fn new(config: Config, overrides: Overrides, widths: UiState) -> Self {
        let target = config.resolve(&overrides).ok();
        let cluster_items = config
            .clusters
            .keys()
            .flat_map(|name| {
                let mut items = vec![(name.clone(), None)];
                if let Some(db) = &config.clusters[name].database {
                    items.push((name.clone(), Some(db.clone())));
                }
                items
            })
            .collect();
        let mut editor = TextArea::default();
        editor.set_tab_length(4);
        let mut app = Self {
            config,
            config_path: PathBuf::new(),
            overrides,
            target,
            session_targets: Default::default(),
            cluster_items,
            cluster_selected: 0,
            panes: Panes {
                widths,
                collapsed: [false; 3],
                maximized: None,
            },
            focus: 1,
            editor,
            editor_top: 0,
            editor_left: 0,
            version: 1,
            sent_version: -1,
            edited_at: Instant::now(),
            dirty: false,
            file: None,
            tab_id: 1,
            next_tab_id: 2,
            tab_order: vec![1],
            inactive_tabs: BTreeMap::new(),
            revision: 0,
            values: Values::new(),
            disk_text: None,
            saving_tabs: BTreeSet::new(),
            library: Library::default(),
            library_generation: 0,
            library_loading: false,
            browser: None,
            dialog: None,
            tokens: Vec::new(),
            diagnostics: json!([]),
            lsp_status: "starting language server".into(),
            lsp_ready: false,
            result: None,
            result_target: None,
            result_query_name: String::new(),
            table: 0,
            view: ViewSpec::default(),
            rows: Vec::new(),
            view_generation: 0,
            view_pending: false,
            view_valid: false,
            chart: None,
            chart_error: String::new(),
            show_chart: false,
            show_diagnostics: false,
            selected: 0,
            column: 0,
            table_state: TableState::default(),
            active: None,
            messages: Vec::new(),
            prompt: None,
            popup: None,
            completions: Vec::new(),
            completion_selected: 0,
            completion_version: -1,
            drag: None,
            quit: false,
        };
        app.ensure_target_visible();
        if let Err(e) = app.config.resolve(&app.overrides) {
            app.notice(format!("No active target: {e}. Use target command or select a configured cluster. F1 for help."));
        }
        app
    }
    fn ensure_target_visible(&mut self) {
        if let Some(target) = &self.target {
            if !self.config.clusters.contains_key(&target.label) {
                self.session_targets
                    .insert(target.label.clone(), target.clone());
            }
            let cluster = (target.label.clone(), None);
            if !self.cluster_items.contains(&cluster) {
                self.cluster_items.push(cluster);
            }
            if !target.database.is_empty() {
                let db = (target.label.clone(), Some(target.database.clone()));
                if !self.cluster_items.contains(&db) {
                    self.cluster_items.push(db.clone());
                }
                self.cluster_items.sort();
                self.cluster_items.dedup();
                self.cluster_selected = self
                    .cluster_items
                    .iter()
                    .position(|i| i == &db)
                    .unwrap_or(0);
            }
        }
    }
    fn setup_help(&self) -> String {
        format!(
            "Configuration file: {}\n\n\
             Language server command: {}\nArguments: {:?}\nStatus: {}\n\n\
             Configure all clusters in one TOML file, using a separate alias for each:\n\n\
             version = 1\ndefault_cluster = \"dev\"\nquery_path = \"queries\"\n\n\
             [clusters.dev]\nendpoint = \"https://YOUR_CLUSTER.REGION.kusto.windows.net\"\ndatabase = \"Logs\"\nauth = \"azure-cli\"\n\
             # tenant = \"YOUR_TENANT_ID\"  # optional: otherwise Azure CLI's current tenant\n\n\
             [clusters.production]\nendpoint = \"https://OTHER_CLUSTER.REGION.kusto.windows.net\"\ndatabase = \"ProductionLogs\"\nauth = \"azure-cli\"\n\n\
             [language_server]\ncommand = \"/absolute/path/to/kusto-lsp\"\nargs = [\"--stdio\"]\n\n\
             Launch: cargo run -- --config /absolute/path/config.toml\n\
             Or: dataexplorer --config /absolute/path/config.toml\n\
             -c alias-or-HTTPS and -d DB override the initial target. Explicit endpoints appear\n\
             in the explorer for this session; they are not saved to the config file.\n\n\
             Highlighting and error help require the separately installed kusto-lsp executable.\n\
             Put it on PATH or set an absolute command path above (do not put ~ in TOML paths).\n\
             The command must speak stdio LSP with --stdio. See README: External language server.\n\
             Restart DataExplorer after editing configuration. An unavailable LSP does not block queries.\n\
             query_path is relative to this config file (or absolute). Queries load recursively.\n\
             Ctrl-O searches the library; Ctrl-S saves with documentation; Ctrl-N/W manages tabs.\n\
             Alt-Left/Right switches tabs. F4 edits parameters; execution values stay in memory.\n\
             Syntax checking works offline. Schema is fetched when the server starts for an active\n\
             target; F6 or the metadata command refreshes database/table/column checking.\n\
             Language diagnostics are underlined and summarized in the query pane; F8 shows details.\n\n\
             Authenticate outside this app: az login [--tenant YOUR_TENANT_ID].\n\
             Never put access tokens or client secrets in config.toml.\n\n\
             Outside the TUI, use --config PATH config init and --config PATH clusters add\n\
             ALIAS HTTPS_ENDPOINT --database DB [--tenant TENANT] [--default].\n\
             Use --config PATH clusters list to verify aliases. These are CLI commands,\n\
             not palette commands. Quote paths with spaces. Configuration is read at startup.",
            self.config_path.display(),
            self.config.language_server.command,
            self.config.language_server.args,
            self.lsp_status
        )
    }
    fn notice(&mut self, text: impl Into<String>) {
        self.messages.push(safe_text(&text.into()));
        if self.messages.len() > 100 {
            self.messages.remove(0);
        }
    }
    fn changed(&mut self) {
        self.revision += 1;
        self.version += 1;
        self.edited_at = Instant::now();
        self.dirty = true;
        self.tokens.clear();
        self.diagnostics = json!([]);
        self.completions.clear();
    }
    fn command_prompt(&mut self, prefix: &str) {
        self.popup = None;
        self.browser = None;
        self.dialog = None;
        self.completions.clear();
        self.prompt = Some(Palette::new(prefix));
    }
    fn tab_name(&self, id: u64) -> String {
        let (file, dirty) = if id == self.tab_id {
            (self.file.as_ref(), self.dirty)
        } else {
            let tab = &self.inactive_tabs[&id];
            (tab.file.as_ref(), tab.dirty)
        };
        format!(
            "{id}: {}{}",
            file.and_then(|p| p.file_name())
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "untitled".into()),
            if dirty { "*" } else { "" }
        )
    }
    fn any_dirty(&self) -> bool {
        self.dirty || self.inactive_tabs.values().any(|tab| tab.dirty)
    }
    fn switch_tab(&mut self, id: u64) {
        if id == self.tab_id {
            return;
        }
        let Some(tab) = self.inactive_tabs.remove(&id) else {
            return;
        };
        let old = TabState {
            editor: std::mem::replace(&mut self.editor, tab.editor),
            top: std::mem::replace(&mut self.editor_top, tab.top),
            left: std::mem::replace(&mut self.editor_left, tab.left),
            dirty: std::mem::replace(&mut self.dirty, tab.dirty),
            file: std::mem::replace(&mut self.file, tab.file),
            revision: std::mem::replace(&mut self.revision, tab.revision),
            values: std::mem::replace(&mut self.values, tab.values),
            disk_text: std::mem::replace(&mut self.disk_text, tab.disk_text),
        };
        self.inactive_tabs.insert(self.tab_id, old);
        self.tab_id = id;
        self.version += 1;
        self.tokens.clear();
        self.diagnostics = json!([]);
        self.completions.clear();
        self.edited_at = Instant::now() - Duration::from_millis(200);
        self.focus = 1;
        self.panes.collapsed[1] = false;
        if self.panes.maximized.is_some() {
            self.panes.maximized = Some(1);
        }
    }
    fn next_tab(&mut self, backwards: bool) {
        let index = self
            .tab_order
            .iter()
            .position(|&id| id == self.tab_id)
            .unwrap_or(0);
        let count = self.tab_order.len();
        self.switch_tab(self.tab_order[(index + if backwards { count - 1 } else { 1 }) % count]);
    }
    fn new_tab(&mut self) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let mut editor = TextArea::default();
        editor.set_tab_length(4);
        self.inactive_tabs.insert(
            id,
            TabState {
                editor,
                top: 0,
                left: 0,
                dirty: false,
                file: None,
                revision: 0,
                values: Values::new(),
                disk_text: None,
            },
        );
        self.tab_order.push(id);
        self.switch_tab(id);
    }
    fn close_tab(&mut self, force: bool) -> Result<()> {
        ensure!(
            !self.saving_tabs.contains(&self.tab_id),
            "save is still running; wait before closing this tab"
        );
        if self.dirty && !force {
            self.dialog = Some(Dialog::Close { quit: false });
            return Ok(());
        }
        let old = self.tab_id;
        if self.tab_order.len() == 1 {
            self.new_tab();
        } else {
            self.next_tab(false);
        }
        self.inactive_tabs.remove(&old);
        self.tab_order.retain(|&id| id != old);
        Ok(())
    }
    fn open_tab(&mut self, path: PathBuf, text: String) -> Result<()> {
        query_library::parse(&text)?;
        if self.file.as_ref() == Some(&path) {
            return Ok(());
        }
        if let Some((&id, _)) = self
            .inactive_tabs
            .iter()
            .find(|(_, tab)| tab.file.as_ref() == Some(&path))
        {
            self.switch_tab(id);
            return Ok(());
        }
        if self.dirty || self.file.is_some() || self.editor.lines().iter().any(|s| !s.is_empty()) {
            self.new_tab();
        }
        self.editor = TextArea::from(text.lines().map(str::to_owned).collect::<Vec<_>>());
        self.editor.set_tab_length(4);
        self.file = Some(path);
        self.disk_text = Some(text);
        self.values.clear();
        self.changed();
        self.dirty = false;
        self.editor_top = 0;
        self.editor_left = 0;
        self.focus = 1;
        Ok(())
    }
    fn query_root(&self) -> Result<PathBuf> {
        let path = self.config.query_directory(&self.config_path).context(
            "Set query_path = \"/path/to/queries\" in config.toml and restart (Ctrl-P setup)",
        )?;
        Ok(if path.is_absolute() {
            path
        } else {
            std::env::current_dir()?.join(path)
        })
    }
    fn reload_library(&mut self, tx: &mpsc::Sender<Job>) -> Result<()> {
        let root = self.query_root()?;
        self.library_generation += 1;
        self.library_loading = true;
        let generation = self.library_generation;
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(Job::Library {
                generation,
                result: query_library::scan(&root),
            });
        });
        Ok(())
    }
    fn browse_queries(&mut self) -> Result<()> {
        self.query_root()?;
        self.prompt = None;
        self.popup = None;
        self.completions.clear();
        self.browser = Some(Browser::default());
        Ok(())
    }
    fn save_dialog(&mut self) -> Result<()> {
        let root = self
            .config
            .query_path
            .as_ref()
            .map(|_| self.query_root())
            .transpose()?;
        let (documentation, _) = query_library::parse(&self.editor.lines().join("\n"))?;
        let path = self
            .library
            .entries
            .iter()
            .find(|entry| self.file.as_ref() == Some(&entry.path))
            .map(|entry| entry.relative.to_string_lossy().into_owned())
            .or_else(|| {
                self.file
                    .as_ref()
                    .and_then(|p| root.as_ref().and_then(|root| p.strip_prefix(root).ok()))
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .or_else(|| {
                self.file.as_ref().map(|p| {
                    if root.is_some() {
                        p.file_name()
                            .unwrap_or(p.as_os_str())
                            .to_string_lossy()
                            .into_owned()
                    } else {
                        p.to_string_lossy().into_owned()
                    }
                })
            })
            .unwrap_or_else(|| "query.kql".into());
        self.dialog = Some(Dialog::SaveHeader {
            form: Form::new(
                "Save query - Ctrl-S continues to parameter definitions",
                vec![
                    Field::new(
                        if root.is_some() {
                            "Path relative to query_path (nested directories allowed)"
                        } else {
                            "Destination path (no query_path configured; relative to launch directory)"
                        },
                        &path,
                    ),
                    Field::new(
                        "Query description (shown in query library)",
                        &documentation.description,
                    ),
                ],
            ),
            documentation,
            values: self.values.clone(),
            in_library: root.is_some(),
        });
        self.completions.clear();
        Ok(())
    }
    fn parameter_dialog(&mut self) -> Result<()> {
        let (documentation, _) = query_library::parse(&self.editor.lines().join("\n"))?;
        self.dialog = Some(Dialog::Parameters {
            parameters: Parameters::new(documentation, self.values.clone()),
            save_path: None,
        });
        self.completions.clear();
        Ok(())
    }
    fn apply_documentation(
        &mut self,
        documentation: &Documentation,
        values: Values,
    ) -> Result<String> {
        let source = self.editor.lines().join("\n");
        let (previous, body) = query_library::parse(&source)?;
        let text = if previous == *documentation {
            source.clone()
        } else {
            query_library::document(documentation, &body)?
        };
        if text != source {
            let cursor = self.editor.cursor();
            let old_lines = source.lines().count();
            let new_lines = text.lines().count();
            self.editor.select_all();
            self.editor.insert_str(&text);
            self.editor.cancel_selection();
            let row = cursor
                .0
                .saturating_add(new_lines)
                .saturating_sub(old_lines)
                .min(new_lines.saturating_sub(1));
            self.editor.move_cursor(CursorMove::Jump(
                row.min(u16::MAX as usize) as u16,
                cursor.1.min(u16::MAX as usize) as u16,
            ));
            self.changed();
        }
        self.values = values;
        Ok(text)
    }
    fn save_query(
        &mut self,
        destination: (String, bool),
        documentation: Documentation,
        values: Values,
        overwrite: bool,
        tx: &mpsc::Sender<Job>,
    ) -> Result<()> {
        ensure!(
            self.saving_tabs.is_empty(),
            "another query save is running; wait before saving again"
        );
        let (relative, in_library) = destination;
        let root = if in_library {
            Some(self.query_root()?)
        } else {
            None
        };
        let other_paths = self
            .inactive_tabs
            .values()
            .filter_map(|tab| tab.file.clone())
            .collect::<Vec<_>>();
        let text = self.apply_documentation(&documentation, values)?;
        let tab_id = self.tab_id;
        let revision = self.revision;
        let previous_path = self.file.clone();
        let previous_text = self.disk_text.clone();
        self.saving_tabs.insert(tab_id);
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = (|| {
                let path = match &root {
                    Some(root) => query_library::save_path(root, std::path::Path::new(&relative))?,
                    None => query_library::explicit_save_path(std::path::Path::new(&relative))?,
                };
                ensure!(
                    !other_paths.contains(&path),
                    "this file is open in another tab; switch to that tab or save under another name"
                );
                if previous_path.as_ref() == Some(&path)
                    && let Some(previous) = previous_text
                {
                    ensure!(
                        query_library::read_query(&path)? == previous,
                        "query changed on disk since it was loaded; save under a different name or reload it"
                    );
                }
                query_library::write_query(&path, &text, overwrite)?;
                Ok::<_, anyhow::Error>(path)
            })();
            let job = match result {
                Ok(path) => Job::Saved {
                    path,
                    tab_id,
                    revision,
                    text,
                },
                Err(e) => Job::SaveFailed {
                    tab_id,
                    error: format!("Query save failed: {e:#}"),
                },
            };
            let _ = tx.blocking_send(job);
        });
        Ok(())
    }
    fn current_table(&self) -> Option<&crate::model::Table> {
        self.result.as_ref()?.tables.get(self.table)
    }
    fn analyze(&mut self, tx: &mpsc::Sender<Job>) {
        self.view_generation += 1;
        self.view_pending = true;
        self.view_valid = false;
        let generation = self.view_generation;
        self.selected = 0;
        self.rows.clear();
        self.chart = None;
        let Some(result) = self.result.clone() else {
            self.view_pending = false;
            return;
        };
        let index = self.table;
        if result.tables.get(index).is_none() {
            self.view_pending = false;
            return;
        }
        let spec = self.view.clone();
        let tx = tx.clone();
        tokio::task::spawn_blocking(move || {
            let Some(table) = result.tables.get(index) else {
                return;
            };
            let rows = spec.indices(table);
            let chart = rows
                .as_ref()
                .map_err(|e| anyhow::anyhow!("{e}"))
                .and_then(|rows| chart::prepare(table, rows));
            let _ = tx.blocking_send(Job::View {
                generation,
                rows,
                chart,
            });
        });
    }
    fn run_query(&mut self, client: &Client, tx: &mpsc::Sender<Job>) {
        if self.active.is_some() {
            self.notice("A query is already active; cancel or wait before running again.");
            return;
        }
        let Some(target) = self.target.clone() else {
            self.notice("Choose a cluster and database before running.");
            return;
        };
        if target.database.is_empty() {
            self.notice("Select a database before running queries.");
            return;
        }
        let query = self.editor.lines().join("\n");
        if query.trim().is_empty() {
            self.notice("Query is empty.");
            return;
        }
        let parameters = match query_library::parse(&query)
            .and_then(|(doc, _)| doc.request_values(&self.values))
        {
            Ok(parameters) => parameters,
            Err(e) => {
                self.notice(format!("Cannot run query: {e:#}"));
                if let Err(error) = self.parameter_dialog() {
                    self.notice(format!("{error:#}"));
                }
                return;
            }
        };
        let id = Client::request_id();
        let cancel = CancellationToken::new();
        self.active = Some(Active {
            id: id.clone(),
            target: target.clone(),
            cancel: cancel.clone(),
            query_name: self.tab_name(self.tab_id),
        });
        self.notice(format!(
            "RUNNING {} / {} [{id}]",
            target.label, target.database
        ));
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = client
                .query_with_parameters(&target, &query, &id, cancel, &parameters)
                .await;
            let _ = tx.send(Job::Query { id, target, result }).await;
        });
    }
    fn cancel(&mut self, client: &Client, tx: &mpsc::Sender<Job>) {
        if let Some(active) = &self.active {
            active.cancel.cancel();
            let client = client.clone();
            let target = active.target.clone();
            let id = active.id.clone();
            let tx = tx.clone();
            self.notice("LOCAL cancellation requested. Server cancellation pending (best effort).");
            tokio::spawn(async move {
                let job = match client.cancel_server(&target, &id).await {
                    Ok(()) => Job::Notice(format!(
                        "Server cancellation request accepted for {id}; query may already have finished."
                    )),
                    Err(e) => Job::Error(format!("Server cancellation unconfirmed: {e:#}")),
                };
                let _ = tx.send(job).await;
            });
        }
    }
    fn metadata(&mut self, client: &Client, tx: &mpsc::Sender<Job>, schema: bool) {
        let Some(target) = self.target.clone() else {
            self.notice("Choose a target before requesting metadata.");
            return;
        };
        self.notice("Fetching read-only metadata...");
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = async {
                let tables = client
                    .metadata(
                        &target,
                        if schema {
                            Metadata::Schema
                        } else {
                            Metadata::Databases
                        },
                    )
                    .await?;
                if schema {
                    Ok(Job::Schema {
                        endpoint: target.endpoint.clone(),
                        database: target.database.clone(),
                        schema: normalize_schema(&tables, &target)?,
                    })
                } else {
                    let table = tables.first().context("database list is empty")?;
                    let col = table
                        .columns
                        .iter()
                        .position(|c| c.name == "DatabaseName")
                        .context("metadata lacks DatabaseName column")?;
                    let names = table
                        .rows
                        .iter()
                        .filter_map(|r| r[col].as_str().map(str::to_owned))
                        .collect();
                    Ok::<_, anyhow::Error>(Job::Databases {
                        endpoint: target.endpoint.clone(),
                        names,
                    })
                }
            }
            .await;
            let _ = tx
                .send(result.unwrap_or_else(|e| Job::Error(format!("Metadata failed: {e:#}"))))
                .await;
        });
    }
    fn apply_target(
        &mut self,
        client: &Client,
        tx: &mpsc::Sender<Job>,
        lsp: &lsp::Handle,
    ) -> Result<()> {
        let target = self.config.resolve(&self.overrides)?;
        self.target = Some(target);
        self.ensure_target_visible();
        self.tokens.clear();
        self.diagnostics = json!([]);
        // Advance document version so in-flight old-schema analysis cannot paint the editor.
        self.version += 1;
        let language_update = lsp
            .document(self.version, self.editor.lines().join("\n"))
            .and_then(|()| lsp.schema(Value::Null));
        if language_update.is_ok() {
            self.sent_version = self.version;
        }
        self.metadata(client, tx, true);
        language_update
    }
    fn handle_job(&mut self, job: Job, tx: &mpsc::Sender<Job>, lsp: &lsp::Handle) {
        match job {
            Job::Query { id, target, result } => {
                if self.active.as_ref().is_none_or(|a| a.id != id) {
                    return;
                }
                let query_name = self
                    .active
                    .as_ref()
                    .map(|a| a.query_name.clone())
                    .unwrap_or_default();
                self.active = None;
                match result {
                    Ok(result) => {
                        self.result_query_name = query_name;
                        let count: usize = result.tables.iter().map(|t| t.rows.len()).sum();
                        self.notice(format!(
                            "{}: {} tables, {count} fetched rows. Request {id}, activity {}",
                            if result.partial { "PARTIAL" } else { "DONE" },
                            result.tables.len(),
                            result.activity_id.as_deref().unwrap_or("unavailable")
                        ));
                        for d in &result.diagnostics {
                            self.notice(d);
                        }
                        self.result = Some(Arc::new(result));
                        self.result_target = Some(target);
                        self.table = 0;
                        self.column = 0;
                        self.view = ViewSpec::default();
                        self.analyze(tx);
                    }
                    Err(e) => {
                        self.notice(format!("Query failed: {e:#}. Previous results retained."))
                    }
                }
            }
            Job::View {
                generation,
                rows,
                chart,
            } if generation == self.view_generation => {
                self.view_pending = false;
                match rows {
                    Ok(rows) => {
                        self.rows = rows;
                        self.view_valid = true;
                    }
                    Err(e) => self.notice(format!("Invalid local view: {e:#}")),
                }
                match chart {
                    Ok(chart) => {
                        self.chart = Some(chart);
                        self.chart_error.clear();
                    }
                    Err(e) => {
                        self.chart = None;
                        self.chart_error = e.to_string();
                    }
                }
                if self.show_chart && self.chart.is_none() {
                    self.notice(format!(
                        "Chart unavailable: {}. Table retained.",
                        self.chart_error
                    ));
                }
            }
            Job::Databases { endpoint, names }
                if self.target.as_ref().is_some_and(|t| t.endpoint == endpoint) =>
            {
                let alias = self
                    .target
                    .as_ref()
                    .map(|t| t.label.clone())
                    .unwrap_or_default();
                self.cluster_items
                    .retain(|(a, db)| a != &alias || db.is_none());
                for name in names {
                    self.cluster_items.push((alias.clone(), Some(name)));
                }
                self.ensure_target_visible();
                self.notice("Database discovery complete. Select a database in the left pane.");
            }
            Job::Schema {
                endpoint,
                database,
                schema,
            } if self
                .target
                .as_ref()
                .is_some_and(|t| t.endpoint == endpoint && t.database == database) =>
            {
                // Bump version for schema changes even when document text is unchanged.
                self.version += 1;
                self.tokens.clear();
                self.diagnostics = json!([]);
                let sent = lsp
                    .document(self.version, self.editor.lines().join("\n"))
                    .and_then(|()| lsp.schema(schema));
                match sent {
                    Ok(()) => {
                        self.sent_version = self.version;
                        self.notice("Database schema sent to language server (no credentials).");
                    }
                    Err(e) => self.notice(e.to_string()),
                }
            }
            Job::Opened { path, text } => {
                if let Err(e) = self.open_tab(path, text) {
                    self.notice(format!("Open failed: {e:#}"));
                }
            }
            Job::Saved {
                path,
                tab_id,
                revision,
                text,
            } => {
                self.saving_tabs.remove(&tab_id);
                if tab_id == self.tab_id {
                    self.file = Some(path.clone());
                    self.disk_text = Some(text);
                    if revision == self.revision {
                        self.dirty = false;
                    }
                } else if let Some(tab) = self.inactive_tabs.get_mut(&tab_id) {
                    tab.file = Some(path.clone());
                    tab.disk_text = Some(text);
                    if revision == tab.revision {
                        tab.dirty = false;
                    }
                }
                self.notice(format!("Saved {}", path.display()));
                if self.config.query_path.is_some()
                    && let Err(e) = self.reload_library(tx)
                {
                    self.notice(format!("{e:#}"));
                }
            }
            Job::SaveFailed { tab_id, error } => {
                self.saving_tabs.remove(&tab_id);
                self.notice(error);
            }
            Job::Library { generation, result } if generation == self.library_generation => {
                self.library_loading = false;
                match result {
                    Ok(library) => {
                        for error in &library.errors {
                            self.notice(format!("Query library: {error}"));
                        }
                        self.notice(format!("Loaded {} queries recursively ({} warnings). Ctrl-O opens the library.", library.entries.len(), library.errors.len()));
                        self.library = library;
                        if let Some(browser) = &mut self.browser {
                            browser.invalidate();
                        }
                    }
                    Err(e) => self.notice(format!(
                        "Query library failed: {e:#}. Ctrl-S can create the configured directory."
                    )),
                }
            }
            Job::Error(error) => self.notice(format!("ERROR: {error}")),
            Job::Notice(notice) => self.notice(notice),
            _ => {}
        }
    }
    fn handle_lsp(&mut self, event: lsp::Event, handle: &lsp::Handle) {
        match event {
            lsp::Event::Ready => {
                self.lsp_ready = true;
                self.lsp_status = "LSP ready".into();
                if let Err(e) = handle.document(self.version, self.editor.lines().join("\n")) {
                    self.notice(e.to_string());
                } else {
                    self.sent_version = self.version;
                }
            }
            lsp::Event::Status(s) => {
                self.lsp_status = s.clone();
                self.notice(s);
            }
            lsp::Event::Tokens {
                version,
                data,
                legend,
            } if version == self.version => {
                match lsp::decode_tokens(self.editor.lines(), &data, &legend) {
                    Ok(tokens) => self.tokens = tokens,
                    Err(e) => self.notice(format!("Invalid LSP tokens: {e}")),
                }
            }
            lsp::Event::Diagnostics {
                version,
                diagnostics,
            } if version == self.version => self.diagnostics = diagnostics,
            lsp::Event::Completion { version, value }
                if version == self.version && self.dialog.is_none() && self.browser.is_none() =>
            {
                self.completions = value
                    .as_array()
                    .or_else(|| value["items"].as_array())
                    .cloned()
                    .unwrap_or_default();
                self.completions.truncate(100);
                self.completion_selected = 0;
                self.completion_version = version;
                if self.completions.is_empty() {
                    self.notice("No completion suggestions at cursor.");
                }
            }
            lsp::Event::Hover { version, value }
                if version == self.version && self.dialog.is_none() && self.browser.is_none() =>
            {
                let contents = &value["contents"];
                let text = contents["value"]
                    .as_str()
                    .or(contents.as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| contents.to_string());
                self.popup = Some(Popup {
                    title: "Language-service hover".into(),
                    text: safe_text(&text),
                    scroll: 0,
                });
            }
            _ => {}
        }
    }
    fn completion(&mut self) -> Result<()> {
        ensure!(
            self.completion_version == self.version,
            "completion is stale"
        );
        let item = self
            .completions
            .get(self.completion_selected)
            .context("no selected completion")?
            .clone();
        ensure!(
            item["insertTextFormat"] != 2,
            "snippet completion is unsupported"
        );
        ensure!(
            item["additionalTextEdits"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "additional completion edits are unsupported"
        );
        let edit = &item["textEdit"];
        let insert = edit["newText"]
            .as_str()
            .or(item["insertText"].as_str())
            .or(item["label"].as_str())
            .context("completion has no insertion text")?;
        if edit.is_object() {
            let range = if edit.get("range").is_some() {
                &edit["range"]
            } else {
                &edit["replace"]
            };
            let position = |v: &Value| -> Result<(u16, u16)> {
                let row = v["line"].as_u64().context("invalid edit line")? as usize;
                let col = v["character"].as_u64().context("invalid edit column")? as usize;
                let line = self
                    .editor
                    .lines()
                    .get(row)
                    .context("edit line outside document")?;
                let byte = lsp::utf16_to_byte(line, col).context("invalid UTF16 edit position")?;
                Ok((
                    u16::try_from(row)?,
                    u16::try_from(line[..byte].chars().count())?,
                ))
            };
            let start = position(&range["start"])?;
            let end = position(&range["end"])?;
            ensure!(start <= end, "completion edit range reversed");
            self.editor.cancel_selection();
            self.editor.move_cursor(CursorMove::Jump(start.0, start.1));
            self.editor.start_selection();
            self.editor.move_cursor(CursorMove::Jump(end.0, end.1));
        }
        self.editor.insert_str(insert);
        self.changed();
        Ok(())
    }
}

fn command(
    app: &mut App,
    text: &str,
    client: &Client,
    tx: &mpsc::Sender<Job>,
    lsp: &lsp::Handle,
) -> Result<()> {
    match Command::try_parse_from(shell_words::split(text)?)? {
        Command::Run => app.run_query(client, tx),
        Command::Cancel => {
            ensure!(app.active.is_some(), "No query is running.");
            app.cancel(client, tx);
        }
        Command::Metadata => {
            app.metadata(client, tx, false);
            if app.target.as_ref().is_some_and(|t| !t.database.is_empty()) {
                app.metadata(client, tx, true);
            }
        }
        Command::Chart => {
            app.show_chart = !app.show_chart;
            if app.show_chart && app.chart.is_none() {
                app.notice(format!(
                    "Chart unavailable: {}. Table retained.",
                    app.chart_error
                ));
            }
        }
        Command::Diagnostics => app.show_diagnostics = !app.show_diagnostics,
        Command::Setup => {
            app.popup = Some(Popup {
                title: "Configuration and language-server setup (Esc closes)".into(),
                text: safe_text(&app.setup_help()),
                scroll: 0,
            })
        }
        Command::Help => {
            app.popup = Some(Popup {
                title: "Help (Esc closes)".into(),
                text: HELP.into(),
                scroll: 0,
            })
        }
        Command::Target {
            cluster,
            database,
            tenant,
        } => {
            let old = app.overrides.cluster.replace(cluster);
            let old_db = std::mem::replace(&mut app.overrides.database, database);
            let old_tenant = std::mem::replace(&mut app.overrides.tenant, tenant);
            if let Err(e) = app.config.resolve(&app.overrides) {
                app.overrides.cluster = old;
                app.overrides.database = old_db;
                app.overrides.tenant = old_tenant;
                return Err(e);
            }
            if let Err(e) = app.apply_target(client, tx, lsp) {
                app.notice(format!("Target selected; language service: {e}"));
            }
        }
        Command::Database { name } => {
            let old = app.overrides.database.replace(name);
            if let Err(e) = app.config.resolve(&app.overrides) {
                app.overrides.database = old;
                return Err(e);
            }
            if let Err(e) = app.apply_target(client, tx, lsp) {
                app.notice(format!("Target selected; language service: {e}"));
            }
        }
        Command::Queries => app.browse_queries()?,
        Command::ReloadQueries => app.reload_library(tx)?,
        Command::New => app.new_tab(),
        Command::NextTab => app.next_tab(false),
        Command::PreviousTab => app.next_tab(true),
        Command::Close { force } => app.close_tab(force)?,
        Command::Parameters => app.parameter_dialog()?,
        Command::SaveQuery => app.save_dialog()?,
        Command::Open { path, force: _ } => {
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = (|| {
                    let path = path.canonicalize()?;
                    let text = query_library::read_query(&path)?;
                    Ok::<_, anyhow::Error>(Job::Opened { path, text })
                })();
                let _ = tx.blocking_send(
                    result.unwrap_or_else(|e| Job::Error(format!("Open failed: {e}"))),
                );
            });
        }
        Command::Save { path, force: _ } => {
            app.save_dialog()?;
            if let Some(Dialog::SaveHeader {
                form, in_library, ..
            }) = &mut app.dialog
            {
                form.fields[0].input = TextArea::from([path.to_string_lossy().into_owned()]);
                form.fields[0].label = "Explicit destination (relative to launch directory)".into();
                *in_library = false;
            }
        }
        Command::Filter { text } => {
            app.view.filter = text.join(" ");
            app.analyze(tx);
        }
        Command::Column { index, op, value } => {
            let op = match op.as_str() {
                "eq" => FilterOp::Eq,
                "lt" => FilterOp::Lt,
                "gt" => FilterOp::Gt,
                "contains" => FilterOp::Contains,
                _ => anyhow::bail!("operator must be eq, lt, gt, contains"),
            };
            app.view.column_filter = Some((index, op, value));
            app.analyze(tx);
        }
        Command::Clear => {
            app.view = ViewSpec::default();
            app.analyze(tx);
        }
        Command::Export {
            format,
            scope,
            path,
            force,
            accept_partial,
        } => {
            let result = app.result.clone().context("no fetched results")?;
            let table = app.table;
            let source = result.tables.get(table).context("no primary table")?;
            ensure!(
                !app.view_pending || scope == "all",
                "local view analysis is still running"
            );
            ensure!(
                app.view_valid || scope == "all",
                "local view is invalid; clear or correct filters before exporting"
            );
            ensure!(
                !result.partial || accept_partial,
                "PARTIAL results: add --accept-partial to export explicitly"
            );
            let rows = match scope.as_str() {
                "all" => (0..source.rows.len()).collect(),
                "view" => app.rows.clone(),
                _ => anyhow::bail!("export scope must be all or view"),
            };
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let job = match export::file(
                    &path,
                    force,
                    &result,
                    &result.tables[table],
                    &rows,
                    format,
                    accept_partial,
                ) {
                    Ok(()) => Job::Notice(format!(
                        "Exported {} rows ({scope}, selected table) to {}",
                        rows.len(),
                        path.display()
                    )),
                    Err(e) => Job::Error(format!("Export failed: {e:#}")),
                };
                let _ = tx.blocking_send(job);
            });
        }
        Command::Quit { force } => {
            ensure!(
                !app.any_dirty() || force,
                "one or more tabs have unsaved changes; save or quit --force"
            );
            ensure!(
                app.saving_tabs.is_empty(),
                "wait for in-progress query saves before quitting"
            );
            app.quit = true;
        }
    }
    Ok(())
}

pub async fn run(config: Config, path: PathBuf, overrides: Overrides) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "TUI requires a terminal; use -r FILE for headless queries"
    );
    let state_path = config::state_path(&path);
    let state = config::UiState::load(&state_path);
    let lsp = lsp::Handle::start(config.language_server.clone());
    let mut app = App::new(
        config,
        overrides,
        state.as_ref().cloned().unwrap_or(UiState {
            cluster_width: 25,
            editor_percent: 45,
        }),
    );
    app.config_path = path;
    if let Err(e) = state {
        app.notice(format!(
            "UI state could not be loaded: {e:#}; using default layout"
        ));
    }
    let client = Client::new()?;
    let (tx, rx) = mpsc::channel(64);
    if app.config.query_path.is_some() {
        app.reload_library(&tx)?;
    }
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        previous_hook(info);
    }));
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let (result, lsp) = event_loop(&mut terminal, &mut app, &client, tx.clone(), rx, lsp).await;
    let active = app.active.take();
    if let Some(active) = &active {
        active.cancel.cancel();
    }
    drop(terminal);
    drop(guard);
    lsp.shutdown().await;
    if let Some(active) = active {
        match client.cancel_server(&active.target, &active.id).await {
            Ok(()) => eprintln!("server cancellation request accepted on exit"),
            Err(e) => eprintln!(
                "server cancellation unconfirmed on exit: {}",
                safe_text(&e.to_string())
            ),
        }
    }
    let widths = app.panes.widths;
    tokio::task::spawn_blocking(move || widths.save(&state_path)).await??;
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    client: &Client,
    tx: mpsc::Sender<Job>,
    mut rx: mpsc::Receiver<Job>,
    mut lsp: lsp::Handle,
) -> (Result<()>, lsp::Handle) {
    let result = async {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut lsp_alive = true;
        while !app.quit {
            terminal.draw(|f| draw(f, app))?;
            tokio::select! {
                event = events.next() => {
                    let event = event.context("terminal input ended")??;
                    match event {
                        Event::Key(key) if key.kind != KeyEventKind::Release => {
                            if let Err(e) = key_event(app, key, client, &tx, &lsp) { app.notice(format!("{e:#}")); }
                        }
                        Event::Paste(text) => {
                            if let Some(dialog) = &mut app.dialog { dialog.paste(&text); }
                            else if let Some(browser) = &mut app.browser { browser.paste(&text); }
                            else if let Some(prompt) = &mut app.prompt { prompt.paste(&text); }
                            else if app.focus == 1 && app.popup.is_none() { app.editor.insert_str(text); app.changed(); }
                        }
                        Event::Mouse(mouse) => {
                            if app.prompt.is_some() || app.popup.is_some() || app.browser.is_some() || app.dialog.is_some() { continue; }
                            let area = terminal.size()?;
                            let body = Rect::new(0, 1, area.width, area.height.saturating_sub(3));
                            let panes = app.panes.areas(body);
                            match mouse.kind {
                                MouseEventKind::Down(_) => {
                                    if mouse.column.abs_diff(panes[0].right()) <= 1 && !app.panes.collapsed[0] { app.drag = Some(0); }
                                    else if mouse.row.abs_diff(panes[1].bottom()) <= 1 && mouse.column >= panes[1].x { app.drag = Some(1); }
                                    else if let Some(i) = panes.iter().position(|r| r.contains((mouse.column, mouse.row).into())) { app.focus = i; }
                                }
                                MouseEventKind::Drag(_) => match app.drag {
                                    Some(0) => app.panes.widths.cluster_width = mouse.column.clamp(16, area.width.saturating_sub(30).max(16)),
                                    Some(1) if body.height > 0 => app.panes.widths.editor_percent = ((mouse.row.saturating_sub(body.y) as u32 * 100 / body.height as u32) as u16).clamp(15,85),
                                    _ => {}
                                },
                                MouseEventKind::Up(_) => app.drag = None,
                                MouseEventKind::ScrollDown if app.focus == 2 => app.selected = (app.selected + 3).min(app.rows.len().saturating_sub(1)),
                                MouseEventKind::ScrollUp if app.focus == 2 => app.selected = app.selected.saturating_sub(3),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                Some(job) = rx.recv() => app.handle_job(job, &tx, &lsp),
                event = lsp.events.recv(), if lsp_alive => match event {
                    Some(event) => {
                        let ready = matches!(event,lsp::Event::Ready);
                        app.handle_lsp(event, &lsp);
                        if ready && app.target.as_ref().is_some_and(|t|!t.database.is_empty()) { app.metadata(client,&tx,true); }
                    }
                    None => { lsp_alive = false; app.lsp_ready = false; }
                },
                _ = tick.tick() => {
                    if lsp_alive && app.version != app.sent_version && app.edited_at.elapsed() > Duration::from_millis(180) {
                        if let Err(e) = lsp.document(app.version, app.editor.lines().join("\n")) { app.notice(e.to_string()); }
                        app.sent_version = app.version;
                    }
                }
            }
        }
        Ok(())
    }.await;
    (result, lsp)
}

fn dialog_key(app: &mut App, key: KeyEvent, tx: &mpsc::Sender<Job>) -> Result<()> {
    let Some(mut dialog) = app.dialog.take() else {
        return Ok(());
    };
    let accept = key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s');
    if key.code == KeyCode::Esc {
        if let Dialog::Parameters { parameters, .. } = &mut dialog
            && parameters.form.is_some()
        {
            parameters.key(key);
            app.dialog = Some(dialog);
        }
        return Ok(());
    }
    match &mut dialog {
        Dialog::SaveHeader {
            form,
            documentation,
            values,
            in_library,
        } => {
            if accept {
                let path = form.fields[0].text().trim().to_owned();
                documentation.description = form.fields[1].text();
                if path.is_empty()
                    || (*in_library && !query_library::is_query(std::path::Path::new(&path)))
                {
                    form.error = Some("Choose a relative .kql, .csl or .kusto filename (nested directories allowed)".into());
                } else if *in_library
                    && !std::path::Path::new(&path)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_)))
                {
                    form.error =
                        Some("Use a path within query_path, without .. or an absolute path".into());
                } else if documentation.description.trim().is_empty() {
                    form.error = Some("Add a query description so others can discover it".into());
                    form.selected = 1;
                } else {
                    app.dialog = Some(Dialog::Parameters {
                        parameters: Parameters::new(documentation.clone(), values.clone()),
                        save_path: Some((path, *in_library)),
                    });
                    return Ok(());
                }
            } else {
                form.key(key);
            }
        }
        Dialog::Parameters {
            parameters,
            save_path,
        } => {
            if parameters.key(key) {
                if let Some((path, in_library)) = save_path {
                    app.dialog = Some(Dialog::Overwrite {
                        path: path.clone(),
                        documentation: parameters.documentation.clone(),
                        values: parameters.values.clone(),
                        in_library: *in_library,
                    });
                    return Ok(());
                }
                match app.apply_documentation(&parameters.documentation, parameters.values.clone())
                {
                    Ok(_) => {
                        app.notice("Parameter definitions applied; Ctrl-S saves documentation/defaults. Execution values remain in memory.");
                        return Ok(());
                    }
                    Err(e) => parameters.error = Some(format!("{e:#}")),
                }
            }
        }
        Dialog::Overwrite {
            path,
            documentation,
            values,
            in_library,
        } => {
            if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                let result = app.save_query(
                    (path.clone(), *in_library),
                    documentation.clone(),
                    values.clone(),
                    true,
                    tx,
                );
                if result.is_ok() {
                    app.notice("Saving query and documentation...");
                    return Ok(());
                }
                app.dialog = Some(dialog);
                return result;
            }
            if matches!(key.code, KeyCode::Char('n' | 'N')) {
                return Ok(());
            }
        }
        Dialog::Close { quit } => {
            if matches!(key.code, KeyCode::Char('n' | 'N')) {
                return Ok(());
            }
            if matches!(key.code, KeyCode::Char('s' | 'S')) && !*quit {
                app.save_dialog()?;
                return Ok(());
            }
            if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                if *quit {
                    ensure!(
                        app.saving_tabs.is_empty(),
                        "wait for pending saves before quitting"
                    );
                    app.quit = true;
                } else {
                    app.close_tab(true)?;
                }
                return Ok(());
            }
        }
    }
    app.dialog = Some(dialog);
    Ok(())
}

fn key_event(
    app: &mut App,
    key: KeyEvent,
    client: &Client,
    tx: &mpsc::Sender<Job>,
    lsp: &lsp::Handle,
) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if app.dialog.is_some() {
        return dialog_key(app, key, tx);
    }
    if let Some(mut browser) = app.browser.take() {
        if key.code == KeyCode::Esc {
            return Ok(());
        }
        if ctrl && key.code == KeyCode::Char('l') {
            app.browser = Some(browser);
            return app.reload_library(tx);
        }
        if let Some(index) = browser.key(key, &app.library.entries) {
            let entry = &app.library.entries[index];
            app.open_tab(entry.path.clone(), entry.text.clone())?;
        } else {
            app.browser = Some(browser);
        }
        return Ok(());
    }
    if ctrl && key.code == KeyCode::Char('p') {
        app.command_prompt("");
        return Ok(());
    }
    if ctrl && key.code == KeyCode::Char('q') {
        ensure!(
            app.saving_tabs.is_empty(),
            "wait for pending saves before quitting"
        );
        if app.any_dirty() {
            app.prompt = None;
            app.popup = None;
            app.dialog = Some(Dialog::Close { quit: true });
        } else {
            app.quit = true;
        }
        return Ok(());
    }
    if ctrl && key.code == KeyCode::Char('c') && app.active.is_some() {
        app.cancel(client, tx);
        return Ok(());
    }
    if let Some(popup) = &mut app.popup {
        match key.code {
            KeyCode::Esc | KeyCode::F(1) => app.popup = None,
            KeyCode::Down => popup.scroll = popup.scroll.saturating_add(1),
            KeyCode::Up => popup.scroll = popup.scroll.saturating_sub(1),
            KeyCode::PageDown => popup.scroll = popup.scroll.saturating_add(10),
            KeyCode::PageUp => popup.scroll = popup.scroll.saturating_sub(10),
            _ => {}
        }
        return Ok(());
    }
    if let Some(mut prompt) = app.prompt.take() {
        match prompt.key(key) {
            palette::Action::Close => {}
            palette::Action::Stay => app.prompt = Some(prompt),
            palette::Action::Submit(text) => {
                if let Err(e) = command(app, &text, client, tx, lsp) {
                    prompt.error = Some(format!("{e:#}"));
                    app.prompt = Some(prompt);
                }
            }
        }
        return Ok(());
    }
    if !app.completions.is_empty() {
        match key.code {
            KeyCode::Esc => {
                app.completions.clear();
                return Ok(());
            }
            KeyCode::Down => {
                app.completion_selected =
                    (app.completion_selected + 1).min(app.completions.len() - 1);
                return Ok(());
            }
            KeyCode::Up => {
                app.completion_selected = app.completion_selected.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Enter => {
                app.completion()?;
                return Ok(());
            }
            _ => app.completions.clear(),
        }
    }
    if (alt && matches!(key.code, KeyCode::Left | KeyCode::Right))
        || (ctrl && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown))
    {
        app.next_tab(matches!(key.code, KeyCode::Left | KeyCode::PageUp));
        return Ok(());
    }
    if alt && let KeyCode::Char(c @ '1'..='3') = key.code {
        let i = c as usize - '1' as usize;
        app.panes.collapsed[i] = !app.panes.collapsed[i];
        if app.panes.collapsed.iter().all(|c| *c) {
            app.panes.collapsed[i] = false;
        }
        app.panes.maximized = None;
        if app.panes.collapsed[app.focus] {
            app.focus = app.panes.collapsed.iter().position(|c| !*c).unwrap_or(1);
        }
        return Ok(());
    }
    if ctrl {
        match key.code {
            KeyCode::Left => {
                app.panes.widths.cluster_width =
                    app.panes.widths.cluster_width.saturating_sub(2).max(16)
            }
            KeyCode::Right => {
                app.panes.widths.cluster_width = app.panes.widths.cluster_width.saturating_add(2)
            }
            KeyCode::Up => {
                app.panes.widths.editor_percent =
                    app.panes.widths.editor_percent.saturating_sub(5).max(15)
            }
            KeyCode::Down => {
                app.panes.widths.editor_percent =
                    app.panes.widths.editor_percent.saturating_add(5).min(85)
            }
            KeyCode::Char('m') => {
                app.panes.maximized = if app.panes.maximized.is_some() {
                    None
                } else {
                    Some(app.focus)
                }
            }
            KeyCode::Char('r') => app.run_query(client, tx),
            KeyCode::Char('p') => app.command_prompt(""),
            KeyCode::Char('o') => app.browse_queries()?,
            KeyCode::Char('s') => app.save_dialog()?,
            KeyCode::Char('l') => app.reload_library(tx)?,
            KeyCode::Char('n') => app.new_tab(),
            KeyCode::Char('w') => app.close_tab(false)?,
            KeyCode::Char('e') => app.command_prompt("export csv view "),
            KeyCode::Char(' ') => {
                lsp.document(app.version, app.editor.lines().join("\n"))?;
                app.sent_version = app.version;
                let (row, col) = app.editor.cursor();
                lsp.completion(
                    app.version,
                    row,
                    lsp::char_to_utf16(&app.editor.lines()[row], col),
                )?;
            }
            _ => {
                if app.focus == 1 && app.editor.input(key) {
                    app.changed();
                }
            }
        }
        return Ok(());
    }
    match key.code {
        KeyCode::F(1) => {
            app.popup = Some(Popup {
                title: "Help (Esc to close)".into(),
                text: HELP.into(),
                scroll: 0,
            })
        }
        KeyCode::F(2) => {
            lsp.document(app.version, app.editor.lines().join("\n"))?;
            app.sent_version = app.version;
            let (row, col) = app.editor.cursor();
            lsp.hover(
                app.version,
                row,
                lsp::char_to_utf16(&app.editor.lines()[row], col),
            )?;
        }
        KeyCode::F(4) => app.parameter_dialog()?,
        KeyCode::F(5) => app.run_query(client, tx),
        KeyCode::F(6) => {
            app.metadata(client, tx, false);
            app.metadata(client, tx, true);
        }
        KeyCode::F(7) => {
            app.show_chart = !app.show_chart;
            if app.show_chart && app.chart.is_none() {
                app.notice(format!(
                    "Chart unavailable: {}. Table retained.",
                    app.chart_error
                ));
            }
        }
        KeyCode::F(8) => app.show_diagnostics = !app.show_diagnostics,
        KeyCode::F(9) => {
            app.panes.maximized = if app.panes.maximized.is_some() {
                None
            } else {
                Some(app.focus)
            }
        }
        KeyCode::Tab | KeyCode::BackTab => {
            let step =
                if key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT) {
                    2
                } else {
                    1
                };
            for _ in 0..3 {
                app.focus = (app.focus + step) % 3;
                if !app.panes.collapsed[app.focus] {
                    break;
                }
            }
            if app.panes.maximized.is_some() {
                app.panes.maximized = Some(app.focus);
            }
        }
        KeyCode::Char(':') if app.focus != 1 => app.command_prompt(""),
        _ => match app.focus {
            0 => match key.code {
                KeyCode::Up => app.cluster_selected = app.cluster_selected.saturating_sub(1),
                KeyCode::Down => {
                    app.cluster_selected =
                        (app.cluster_selected + 1).min(app.cluster_items.len().saturating_sub(1))
                }
                KeyCode::Enter => {
                    let (alias, db) = app
                        .cluster_items
                        .get(app.cluster_selected)
                        .context("no configured cluster; use target command")?
                        .clone();
                    app.overrides.cluster = Some(alias.clone());
                    let transient = app.session_targets.get(&alias);
                    app.overrides.tenant = transient.and_then(|t| t.tenant.clone());
                    app.overrides.database = db
                        .or_else(|| {
                            app.config
                                .clusters
                                .get(&alias)
                                .and_then(|c| c.database.clone())
                        })
                        .or_else(|| transient.map(|t| t.database.clone()));
                    if app.overrides.database.is_none() {
                        // .show databases does not need a database. Use an explicit temporary target.
                        let c = app
                            .config
                            .clusters
                            .get(&alias)
                            .context("cluster alias missing")?;
                        app.target = Some(Target {
                            label: alias,
                            endpoint: config::endpoint(&c.endpoint)?,
                            database: String::new(),
                            tenant: c.tenant.clone(),
                            limits: app.config.defaults.clone(),
                        });
                        app.version += 1;
                        app.tokens.clear();
                        app.diagnostics = json!([]);
                        if let Err(e) = lsp
                            .document(app.version, app.editor.lines().join("\n"))
                            .and_then(|()| lsp.schema(Value::Null))
                        {
                            app.notice(e.to_string());
                        }
                        app.metadata(client, tx, false);
                    } else if let Err(e) = app.apply_target(client, tx, lsp) {
                        app.notice(e.to_string());
                    }
                }
                _ => {}
            },
            1 => {
                if app.editor.input(key) {
                    app.changed();
                }
            }
            2 => match key.code {
                KeyCode::Up => app.selected = app.selected.saturating_sub(1),
                KeyCode::Down => {
                    app.selected = (app.selected + 1).min(app.rows.len().saturating_sub(1))
                }
                KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
                KeyCode::PageDown => {
                    app.selected = (app.selected + 10).min(app.rows.len().saturating_sub(1))
                }
                KeyCode::Left => app.column = app.column.saturating_sub(1),
                KeyCode::Right => {
                    app.column = (app.column + 1).min(
                        app.current_table()
                            .map_or(0, |t| t.columns.len().saturating_sub(1)),
                    )
                }
                KeyCode::Char('[') | KeyCode::Char(']') => {
                    let count = app.result.as_ref().map_or(0, |r| r.tables.len());
                    if count > 0 {
                        app.table = if key.code == KeyCode::Char('[') {
                            (app.table + count - 1) % count
                        } else {
                            (app.table + 1) % count
                        };
                        app.view = ViewSpec::default();
                        app.column = 0;
                        app.analyze(tx);
                    }
                }
                KeyCode::Char('s') => {
                    let desc = app.view.sort.is_some_and(|(c, d)| c == app.column && !d);
                    app.view.sort = Some((app.column, desc));
                    app.analyze(tx);
                }
                KeyCode::Char('/') => app.command_prompt("filter "),
                KeyCode::Char('f') => app.command_prompt(&format!("column {} eq ", app.column)),
                KeyCode::Enter => {
                    if let Some(t) = app.current_table()
                        && let Some(row) = app.rows.get(app.selected).and_then(|i| t.rows.get(*i))
                        && let Some(value) = row.get(app.column)
                    {
                        app.popup = Some(Popup {
                            title: format!(
                                "{} ({}) - Esc closes",
                                t.columns[app.column].name, t.columns[app.column].kind
                            ),
                            text: safe_text(&text(value)),
                            scroll: 0,
                        });
                    }
                }
                _ => {}
            },
            _ => {}
        },
    }
    Ok(())
}

fn block(title: String, focused: bool) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }))
}
fn token_color(kind: &str) -> Color {
    match kind {
        "keyword" => Color::Magenta,
        "string" => Color::Green,
        "number" => Color::Yellow,
        "comment" => Color::DarkGray,
        "function" => Color::Blue,
        "type" | "class" => Color::Cyan,
        "operator" => Color::LightRed,
        _ => Color::White,
    }
}
fn diagnostic_range(diagnostic: &Value) -> Option<((usize, usize), (usize, usize))> {
    let position = |v: &Value| {
        Some((
            usize::try_from(v["line"].as_u64()?).ok()?,
            usize::try_from(v["character"].as_u64()?).ok()?,
        ))
    };
    Some((
        position(&diagnostic["range"]["start"])?,
        position(&diagnostic["range"]["end"])?,
    ))
}
fn editor(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < 3 || area.height < 3 {
        return;
    }
    let title = format!(
        "Query{} {} | {} tabs | F5 run | {}",
        if app.dirty { "*" } else { "" },
        app.file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or("untitled".into()),
        app.tab_order.len(),
        if app.lsp_ready {
            "LSP ready"
        } else {
            "LSP unavailable - Ctrl-P setup"
        }
    );
    let b = block(safe_text(&title), app.focus == 1);
    let mut inner = b.inner(area);
    f.render_widget(b, area);
    if inner.height >= 3 {
        let selected = app
            .tab_order
            .iter()
            .position(|&id| id == app.tab_id)
            .unwrap_or(0);
        let tabs = app
            .tab_order
            .iter()
            .skip(selected)
            .map(|&id| {
                Span::styled(
                    format!(
                        " {}{} ",
                        if id == app.tab_id { "> " } else { "" },
                        safe_text(&app.tab_name(id))
                    ),
                    if id == app.tab_id {
                        Style::default().fg(Color::Black).bg(Color::Cyan)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                )
            })
            .collect::<Vec<_>>();
        f.render_widget(
            Paragraph::new(Line::from(tabs)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        inner.y += 1;
        inner.height -= 1;
    }
    if inner.height >= 2 {
        let (cursor_row, _) = app.editor.cursor();
        let diagnostic = app.diagnostics.as_array().and_then(|a| {
            a.iter()
                .find(|d| {
                    diagnostic_range(d)
                        .is_some_and(|(start, end)| cursor_row >= start.0 && cursor_row <= end.0)
                })
                .or_else(|| a.first())
        });
        let message = if !app.lsp_ready {
            "Highlighting/errors unavailable. Ctrl-P > setup for LSP configuration.".to_owned()
        } else if let Some(d) = diagnostic {
            let (line, col) = diagnostic_range(d).map(|(s, _)| s).unwrap_or((0, 0));
            format!(
                "{}:{} {} | F8 details",
                line + 1,
                col + 1,
                d["message"].as_str().unwrap_or("language diagnostic")
            )
        } else {
            "LSP ready | Ctrl-Space completion | F2 hover | F6 refresh schema".into()
        };
        f.render_widget(
            Paragraph::new(safe_text(&message)).style(Style::default().fg(
                if diagnostic.is_some() || !app.lsp_ready {
                    Color::Yellow
                } else {
                    Color::DarkGray
                },
            )),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
        inner.height -= 1;
    }
    let (row, col) = app.editor.cursor();
    if row < app.editor_top {
        app.editor_top = row;
    }
    if row >= app.editor_top + inner.height as usize {
        app.editor_top = row.saturating_sub(inner.height as usize - 1);
    }
    let line = &app.editor.lines()[row];
    let byte = line.char_indices().nth(col).map_or(line.len(), |(i, _)| i);
    let display = lsp::byte_to_display(line, byte).unwrap_or(0);
    if display < app.editor_left {
        app.editor_left = display;
    }
    if display >= app.editor_left + inner.width as usize {
        app.editor_left = display.saturating_sub(inner.width as usize - 1);
    }
    let selection = app.editor.selection_range();
    for (visible, (line_no, source)) in app
        .editor
        .lines()
        .iter()
        .enumerate()
        .skip(app.editor_top)
        .take(inner.height as usize)
        .enumerate()
    {
        let mut spans = Vec::new();
        let mut display_col = 0;
        let mut utf16_col = 0;
        let tokens: Vec<_> = app.tokens.iter().filter(|t| t.line == line_no).collect();
        let ranges: Vec<_> = app
            .diagnostics
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(diagnostic_range)
            .filter(|(start, end)| line_no >= start.0 && line_no <= end.0)
            .collect();
        for (char_col, (byte, c)) in source.char_indices().enumerate() {
            let width = if c == '\t' {
                4 - display_col % 4
            } else {
                c.width().unwrap_or(0)
            };
            if display_col >= app.editor_left
                && display_col + width <= app.editor_left + inner.width as usize
            {
                let color = tokens
                    .iter()
                    .find(|t| byte >= t.start_byte && byte < t.end_byte)
                    .map_or(Color::White, |t| token_color(&t.kind));
                let mut style = Style::default().fg(color);
                if ranges.iter().any(|(start, end)| {
                    let pos = (line_no, utf16_col);
                    pos >= *start && (pos < *end || (start == end && pos == *start))
                }) {
                    style = style.fg(Color::LightRed).add_modifier(Modifier::UNDERLINED);
                }
                if selection.is_some_and(|(start, end)| {
                    (line_no, char_col) >= start && (line_no, char_col) < end
                }) {
                    style = style.bg(Color::DarkGray);
                }
                let value = if c == '\t' {
                    " ".repeat(width)
                } else if c.is_control() {
                    "\u{fffd}".into()
                } else {
                    c.to_string()
                };
                spans.push(Span::styled(value, style));
            } else if display_col < app.editor_left && display_col + width > app.editor_left {
                spans.push(Span::raw(" ".repeat(display_col + width - app.editor_left)));
            }
            display_col += width;
            utf16_col += c.len_utf16();
            if display_col > app.editor_left + inner.width as usize {
                break;
            }
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y + visible as u16, inner.width, 1),
        );
    }
    if app.focus == 1
        && app.prompt.is_none()
        && app.popup.is_none()
        && app.completions.is_empty()
        && app.dialog.is_none()
        && app.browser.is_none()
    {
        f.set_cursor_position((
            inner.x + (display.saturating_sub(app.editor_left) as u16).min(inner.width - 1),
            inner.y + (row - app.editor_top) as u16,
        ));
    }
}
fn result_table(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(result) = &app.result else {
        f.render_widget(
            Paragraph::new(
                "No results yet. F5 runs the complete editor against the active target.",
            )
            .block(block("Results".into(), app.focus == 2))
            .wrap(Wrap { trim: false }),
            area,
        );
        return;
    };
    let Some(t) = result.tables.get(app.table) else {
        f.render_widget(
            Paragraph::new("Query returned no primary tables. F8 shows diagnostics.")
                .block(block("Results".into(), app.focus == 2)),
            area,
        );
        return;
    };
    let target = app
        .result_target
        .as_ref()
        .map(|t| format!("{} / {}", t.label, t.database))
        .unwrap_or_default();
    let title = format!(
        "{} T{}/{} {} | {}/{} LOCAL rows{} | {} | query {} | col {} | F7 chart",
        if result.partial { "PARTIAL" } else { "Results" },
        app.table + 1,
        result.tables.len(),
        t.name,
        app.rows.len(),
        t.rows.len(),
        if app.view_pending { " (analyzing)" } else { "" },
        target,
        app.result_query_name,
        app.column
    );
    let count = ((area.width.saturating_sub(2) as usize / 20).max(1))
        .min(t.columns.len().saturating_sub(app.column));
    let header = Row::new(
        t.columns
            .iter()
            .skip(app.column)
            .take(count)
            .map(|c| Cell::from(safe_text(&format!("{}:{}", c.name, c.kind)))),
    )
    .style(Style::default().fg(Color::Yellow));
    let visible = area.height.saturating_sub(3) as usize;
    let start = app.selected.saturating_sub(visible.saturating_sub(1));
    let rows = app.rows.iter().skip(start).take(visible).map(|i| {
        Row::new(
            t.rows[*i]
                .iter()
                .skip(app.column)
                .take(count)
                .map(|v| Cell::from(cell_preview(v))),
        )
    });
    let widths = vec![Constraint::Length(20); count];
    let table = Table::new(rows, widths)
        .header(header)
        .block(block(safe_text(&title), app.focus == 2))
        .row_highlight_style(Style::default().bg(Color::DarkGray))
        .highlight_symbol("> ");
    app.table_state
        .select((!app.rows.is_empty()).then_some(app.selected.saturating_sub(start)));
    *app.table_state.offset_mut() = 0;
    f.render_stateful_widget(table, area, &mut app.table_state);
}
fn cell_preview(value: &Value) -> String {
    let preview = match value {
        Value::String(s) => s.chars().take(200).collect::<String>(),
        Value::Array(_) => "[dynamic array: Enter for detail]".into(),
        Value::Object(_) => "{dynamic object: Enter for detail}".into(),
        other => text(other),
    };
    safe_text(&preview).replace('\n', "\\n")
}
const COLORS: [Color; 6] = [
    Color::Cyan,
    Color::Yellow,
    Color::Green,
    Color::Magenta,
    Color::Blue,
    Color::Red,
];
fn chart_widget(f: &mut Frame, data: &ChartData, area: Rect, focused: bool) {
    let title = format!(
        "{} | LOCAL view; f64 display, no sampling | F7 table",
        data.title
    );
    if data.kind == ChartKind::Bar {
        let parts = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        let legend: Vec<_> = data
            .series
            .iter()
            .enumerate()
            .map(|(i, s)| {
                Span::styled(
                    format!(" {} ", safe_text(&s.name)),
                    Style::default().fg(COLORS[i % COLORS.len()]),
                )
            })
            .collect();
        let canvas = Canvas::default()
            .block(block(
                format!(
                    "{} | value range {} .. {}",
                    safe_text(&title),
                    data.y_bounds[0],
                    data.y_bounds[1]
                ),
                focused,
            ))
            .x_bounds(data.y_bounds)
            .y_bounds(data.x_bounds)
            .marker(Marker::Braille)
            .paint(|ctx| {
                ctx.draw(&CanvasLine {
                    x1: 0.,
                    x2: 0.,
                    y1: data.x_bounds[0],
                    y2: data.x_bounds[1],
                    color: Color::DarkGray,
                });
                for (series, s) in data.series.iter().enumerate() {
                    for &(category, value) in &s.points {
                        let offset = (series as f64 - (data.series.len() - 1) as f64 / 2.) * 0.6
                            / data.series.len() as f64;
                        ctx.draw(&CanvasLine {
                            x1: 0.,
                            x2: value,
                            y1: category + offset,
                            y2: category + offset,
                            color: COLORS[series % COLORS.len()],
                        });
                    }
                }
                for (i, category) in data.categories.iter().enumerate() {
                    ctx.print(
                        data.y_bounds[0],
                        i as f64,
                        Span::styled(safe_text(category), Style::default().fg(Color::White)),
                    );
                }
                ctx.print(
                    data.y_bounds[0],
                    data.x_bounds[0],
                    format!("{:.3}", data.y_bounds[0]),
                );
                ctx.print(
                    data.y_bounds[1],
                    data.x_bounds[0],
                    format!("{:.3}", data.y_bounds[1]),
                );
            });
        f.render_widget(canvas, parts[0]);
        f.render_widget(Paragraph::new(Line::from(legend)), parts[1]);
    } else {
        let datasets = data
            .series
            .iter()
            .enumerate()
            .map(|(i, s)| {
                Dataset::default()
                    .name(safe_text(&s.name))
                    .marker(Marker::Braille)
                    .style(Style::default().fg(COLORS[i % COLORS.len()]))
                    .graph_type(if data.kind == ChartKind::Scatter {
                        GraphType::Scatter
                    } else {
                        GraphType::Line
                    })
                    .data(&s.points)
            })
            .collect();
        let labels = |bounds: [f64; 2]| {
            bounds
                .into_iter()
                .map(|v| Line::from(format!("{v:.3}")))
                .collect::<Vec<_>>()
        };
        let x_labels = if data.kind == ChartKind::Time {
            data.x_bounds
                .iter()
                .map(|v| {
                    Line::from(
                        chrono::DateTime::from_timestamp(*v as i64, 0)
                            .map(|t| t.format("%m-%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| format!("{v:.0}")),
                    )
                })
                .collect()
        } else {
            labels(data.x_bounds)
        };
        let chart = Chart::new(datasets)
            .block(block(safe_text(&title), focused))
            .x_axis(
                Axis::default()
                    .title(safe_text(&data.x_title))
                    .bounds(data.x_bounds)
                    .labels(x_labels),
            )
            .y_axis(
                Axis::default()
                    .title(safe_text(&data.y_title))
                    .bounds(data.y_bounds)
                    .labels(labels(data.y_bounds)),
            );
        f.render_widget(chart, area);
    }
}
fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if area.width < 50 || area.height < 14 {
        f.render_widget(
            Paragraph::new("Terminal too small (minimum 50x14).\nResize terminal; Ctrl-Q quits.")
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    let target = app
        .target
        .as_ref()
        .map(|t| {
            format!(
                "{} / {} tenant={}",
                t.label,
                t.database,
                t.tenant.as_deref().unwrap_or("az current")
            )
        })
        .unwrap_or("NO TARGET".into());
    let run = app
        .active
        .as_ref()
        .map(|a| format!("RUNNING {} / {}", a.target.label, a.target.database))
        .unwrap_or_else(|| "IDLE".into());
    f.render_widget(
        Paragraph::new(safe_text(&format!("DataExplorer | {target} | {run}")))
            .style(Style::default().fg(Color::Black).bg(Color::Cyan)),
        chunks[0],
    );
    let panes = app.panes.areas(chunks[1]);
    if panes[0].width > 0 {
        let items = app
            .cluster_items
            .iter()
            .map(|(alias, db)| {
                ListItem::new(safe_text(
                    &db.as_ref().map(|d| format!("  {d}")).unwrap_or_else(|| {
                        let label = url::Url::parse(alias)
                            .ok()
                            .and_then(|u| u.host_str().map(str::to_owned))
                            .unwrap_or(alias.clone());
                        format!(
                            "{}{label}",
                            if app.target.as_ref().is_some_and(|t| t.label == *alias) {
                                "* "
                            } else {
                                ""
                            }
                        )
                    }),
                ))
            })
            .collect::<Vec<_>>();
        let mut state = ListState::default().with_selected(Some(app.cluster_selected));
        f.render_stateful_widget(
            List::new(items)
                .block(block("Clusters / DB (F6)".into(), app.focus == 0))
                .highlight_style(Style::default().bg(Color::DarkGray)),
            panes[0],
            &mut state,
        );
        if app.cluster_items.is_empty() {
            let inner = Block::default().borders(Borders::ALL).inner(panes[0]);
            f.render_widget(Paragraph::new("No clusters configured.\n\nCtrl-P > setup\nfor config examples.\n\nUse --config PATH\nor -c HTTPS -d DB.").wrap(Wrap { trim:false }),inner);
        }
    }
    editor(f, app, panes[1]);
    if panes[2].height > 0 && panes[2].width > 0 {
        if app.show_diagnostics {
            let mut lines = app
                .messages
                .iter()
                .rev()
                .take(30)
                .cloned()
                .collect::<Vec<_>>();
            if let Some(diagnostics) = app.diagnostics.as_array() {
                for d in diagnostics.iter().rev() {
                    lines.insert(
                        0,
                        format!(
                            "LSP {}:{} {}",
                            d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
                            d["range"]["start"]["character"].as_u64().unwrap_or(0) + 1,
                            d["message"].as_str().unwrap_or("diagnostic")
                        ),
                    );
                }
            }
            f.render_widget(
                Paragraph::new(safe_text(&lines.join("\n")))
                    .block(block(
                        "Diagnostics / language errors (F8)".into(),
                        app.focus == 2,
                    ))
                    .wrap(Wrap { trim: false }),
                panes[2],
            );
        } else if app.show_chart
            && let Some(chart) = &app.chart
        {
            chart_widget(f, chart, panes[2], app.focus == 2);
        } else {
            result_table(f, app, panes[2]);
        }
    }
    let diagnostic_count = app.diagnostics.as_array().map_or(0, Vec::len);
    let status = format!(
        "{}\nCtrl-O queries | Ctrl-N/W tabs | Alt-arrows switch | Ctrl-S save | F4 params | F1 help | {diagnostic_count} diagnostics",
        app.messages.last().map(String::as_str).unwrap_or("Ready.")
    );
    f.render_widget(Paragraph::new(status), chunks[2]);
    if let Some(prompt) = &mut app.prompt {
        prompt.draw(f);
    }
    if let Some(browser) = &mut app.browser {
        browser.draw(f, &app.library.entries, app.library_loading);
    }
    if let Some(dialog) = &mut app.dialog {
        dialog.draw(f);
    }
    if !app.completions.is_empty() {
        let rect = Rect::new(
            area.width / 4,
            area.height / 4,
            area.width / 2,
            (app.completions.len() as u16 + 2).min(area.height / 2),
        );
        f.render_widget(Clear, rect);
        let items: Vec<_> = app
            .completions
            .iter()
            .map(|c| ListItem::new(safe_text(c["label"].as_str().unwrap_or("?"))))
            .collect();
        let mut state = ListState::default().with_selected(Some(app.completion_selected));
        f.render_stateful_widget(
            List::new(items)
                .block(block("Completion (Enter inserts / Esc)".into(), true))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            rect,
            &mut state,
        );
    }
    if let Some(popup) = &app.popup {
        let rect = Rect::new(
            area.x + 2,
            area.y + 2,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(popup.text.as_str())
                .block(block(popup.title.clone(), true))
                .wrap(Wrap { trim: false })
                .scroll((popup.scroll, 0)),
            rect,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tabs_preserve_editor_undo_values_and_unsaved_guards() {
        let mut app = App::new(Config::default(), Overrides::default(), UiState::default());
        app.editor.insert_str("print first=1");
        app.changed();
        app.values.insert("region".into(), "west".into());
        let first = app.tab_id;
        let old_version = app.version;
        app.new_tab();
        assert!(app.version > old_version);
        assert!(app.values.is_empty());
        app.editor.insert_str("print second=2");
        app.changed();
        let second = app.tab_id;
        app.next_tab(true);
        assert_eq!(app.tab_id, first);
        assert_eq!(app.editor.lines(), ["print first=1"]);
        assert_eq!(app.values["region"], "west");
        assert!(app.editor.undo());
        app.changed();
        assert_eq!(app.editor.lines(), [""]);
        app.switch_tab(second);
        assert_eq!(app.editor.lines(), ["print second=2"]);
        app.close_tab(false).unwrap();
        assert!(matches!(app.dialog, Some(Dialog::Close { quit: false })));
        assert_eq!(app.tab_order.len(), 2);
        app.dialog = None;
        app.close_tab(true).unwrap();
        assert_eq!(app.tab_id, first);
        assert!(app.any_dirty());
        app.close_tab(true).unwrap();
        assert_eq!(app.tab_order.len(), 1);
        assert!(!app.any_dirty());
    }
    #[tokio::test]
    async fn save_wizard_documents_parameters_without_persisting_values() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("queries");
        let mut app = App::new(
            Config {
                query_path: Some(root.clone()),
                ..Default::default()
            },
            Overrides::default(),
            UiState::default(),
        );
        app.editor.insert_str("print count=limit");
        app.changed();
        let (tx, mut rx) = mpsc::channel(16);
        let server = lsp::Handle::start(crate::config::LanguageServer {
            command: "dataexplorer-intentionally-missing-lsp".into(),
            args: vec![],
        });
        app.save_dialog().unwrap();
        let Some(Dialog::SaveHeader { form, .. }) = &mut app.dialog else {
            panic!("save header");
        };
        form.fields[0].input = TextArea::from(["team/nested/count.kql"]);
        form.fields[1].input = TextArea::from(["Count records for investigation"]);
        let accept = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        dialog_key(&mut app, accept, &tx).unwrap();
        let Some(Dialog::Parameters { parameters, .. }) = &mut app.dialog else {
            panic!("parameters");
        };
        parameters
            .documentation
            .parameters
            .push(query_library::Parameter {
                name: "limit".into(),
                kind: "long".into(),
                description: "Maximum records".into(),
                default: Some("10".into()),
            });
        parameters.values.insert("limit".into(), "87654321".into());
        dialog_key(&mut app, accept, &tx).unwrap();
        assert!(matches!(app.dialog, Some(Dialog::Overwrite { .. })));
        dialog_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
            &tx,
        )
        .unwrap();
        let origin = app.tab_id;
        app.new_tab();
        let saved = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(saved, Job::Saved { .. }));
        app.handle_job(saved, &tx, &server);
        assert!(
            app.file.is_none(),
            "background save must not rename the newly active tab"
        );
        app.switch_tab(origin);
        assert!(!app.dirty);
        assert_eq!(app.values["limit"], "87654321");
        let saved_path = root.join("team/nested/count.kql").canonicalize().unwrap();
        assert_eq!(app.file.as_ref(), Some(&saved_path));
        let saved = std::fs::read_to_string(&saved_path).unwrap();
        assert!(!saved.contains("87654321"));
        let (doc, body) = query_library::parse(&saved).unwrap();
        assert_eq!(doc.parameters[0].default.as_deref(), Some("10"));
        assert_eq!(doc.description, "Count records for investigation");
        assert_eq!(body, "print count=limit");
        let count = app.tab_order.len();
        app.open_tab(saved_path, saved).unwrap();
        assert_eq!(
            app.tab_order.len(),
            count,
            "existing path focuses its existing tab"
        );
        assert_eq!(app.values["limit"], "87654321");
        server.shutdown().await;
    }
    #[tokio::test]
    async fn save_revision_and_external_edits_are_protected() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_owned();
        let path = root.join("query.kql");
        std::fs::write(&path, "print x=1").unwrap();
        let path = path.canonicalize().unwrap();
        let mut app = App::new(
            Config {
                query_path: Some(root.clone()),
                ..Default::default()
            },
            Overrides::default(),
            UiState::default(),
        );
        app.open_tab(path.clone(), "print x=1".into()).unwrap();
        let revision = app.revision;
        app.editor.insert_str(" ");
        app.changed();
        let (tx, mut rx) = mpsc::channel(16);
        let server = lsp::Handle::start(crate::config::LanguageServer {
            command: "dataexplorer-intentionally-missing-lsp".into(),
            args: vec![],
        });
        app.handle_job(
            Job::Saved {
                path: path.clone(),
                tab_id: app.tab_id,
                revision,
                text: "print x=1".into(),
            },
            &tx,
            &server,
        );
        assert!(app.dirty, "an older save must not clear newer edits");
        std::fs::write(&path, "print external=42").unwrap();
        app.save_query(
            ("query.kql".into(), true),
            Documentation {
                description: "description".into(),
                ..Default::default()
            },
            Values::new(),
            true,
            &tx,
        )
        .unwrap();
        loop {
            let job = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            if let Job::SaveFailed { ref error, .. } = job {
                assert!(error.contains("changed on disk"));
                app.handle_job(job, &tx, &server);
                break;
            }
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), "print external=42");
        assert!(app.dirty);
        assert!(app.saving_tabs.is_empty());
        server.shutdown().await;
    }
    #[test]
    fn editing_only_parameter_values_does_not_dirty_query_text() {
        let mut app = App::new(Config::default(), Overrides::default(), UiState::default());
        let doc = Documentation {
            description: "Example".into(),
            parameters: vec![query_library::Parameter {
                name: "name".into(),
                kind: "string".into(),
                description: "Person".into(),
                default: None,
            }],
        };
        let source = query_library::document(&doc, "print name").unwrap();
        app.open_tab("query.kql".into(), source.clone()).unwrap();
        app.apply_documentation(
            &doc,
            Values::from([("name".into(), "private-value".into())]),
        )
        .unwrap();
        assert!(!app.dirty);
        assert_eq!(app.editor.lines().join("\n"), source);
        assert!(!app.editor.lines().join("\n").contains("private-value"));
    }
    #[tokio::test]
    async fn explicit_save_without_library_and_inactive_dirty_quit_guard() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("query.txt");
        let mut app = App::new(Config::default(), Overrides::default(), UiState::default());
        app.editor.insert_str("print x=1");
        app.changed();
        app.new_tab();
        let (tx, mut rx) = mpsc::channel(16);
        let server = lsp::Handle::start(crate::config::LanguageServer {
            command: "dataexplorer-intentionally-missing-lsp".into(),
            args: vec![],
        });
        let client = Client::new().unwrap();
        assert!(command(&mut app, "quit", &client, &tx, &server).is_err());
        assert!(!app.quit);
        app.switch_tab(1);
        app.save_dialog().unwrap();
        assert!(matches!(
            app.dialog,
            Some(Dialog::SaveHeader {
                in_library: false,
                ..
            })
        ));
        app.dialog = None;
        app.save_query(
            (path.to_string_lossy().into_owned(), false),
            Documentation {
                description: "Explicit destination".into(),
                ..Default::default()
            },
            Values::new(),
            false,
            &tx,
        )
        .unwrap();
        let job = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(job, Job::Saved { .. }));
        app.handle_job(job, &tx, &server);
        assert_eq!(
            query_library::parse(&std::fs::read_to_string(&path).unwrap())
                .unwrap()
                .1,
            "print x=1"
        );
        assert!(!app.dirty);
        server.shutdown().await;
    }
    use ratatui::backend::TestBackend;
    #[test]
    fn explicit_endpoint_and_configured_profiles_appear_with_databases() {
        let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        let endpoint = "https://session.example";
        let app = App::new(
            config,
            Overrides {
                cluster: Some(endpoint.into()),
                database: Some("SessionDb".into()),
                ..Default::default()
            },
            UiState::default(),
        );
        for item in [
            ("dev", None),
            ("dev", Some("Logs")),
            ("production", None),
            ("production", Some("ProductionLogs")),
            (endpoint, None),
            (endpoint, Some("SessionDb")),
        ] {
            assert!(
                app.cluster_items
                    .contains(&(item.0.into(), item.1.map(str::to_owned)))
            );
        }
        assert!(!app.config.clusters.contains_key(endpoint));
        assert!(app.session_targets.contains_key(endpoint));
        let override_profile = App::new(
            app.config,
            Overrides {
                cluster: Some("dev".into()),
                database: Some("OverrideDb".into()),
                ..Default::default()
            },
            UiState::default(),
        );
        assert!(
            override_profile
                .cluster_items
                .contains(&("dev".into(), Some("OverrideDb".into())))
        );
    }
    #[test]
    fn query_pane_highlights_tokens_and_underlines_utf16_diagnostics() {
        let mut app = App::new(Config::default(), Overrides::default(), UiState::default());
        app.lsp_ready = true;
        let source = "print x='😀'; bad";
        app.editor = TextArea::from([source]);
        app.tokens =
            lsp::decode_tokens(app.editor.lines(), &[0, 0, 5, 0, 0], &["keyword".into()]).unwrap();
        let byte = source.find("bad").unwrap();
        let start = source[..byte].encode_utf16().count();
        app.diagnostics = json!([{"range":{"start":{"line":0,"character":start},"end":{"line":0,"character":start+3}},"message":"Unknown name","severity":1}]);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal
            .draw(|f| {
                let a = f.area();
                editor(f, &mut app, a);
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(1, 2)].fg, Color::Magenta);
        let x = 1 + lsp::byte_to_display(source, byte).unwrap() as u16;
        assert!(buffer[(x, 2)].modifier.contains(Modifier::UNDERLINED));
        let rendered = buffer
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(rendered.contains("Unknown name"));
        app.lsp_ready = false;
        terminal
            .draw(|f| {
                let a = f.area();
                editor(f, &mut app, a);
            })
            .unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(rendered.contains("Ctrl-P > setup"));
        app.config_path = "/tmp/my-config.toml".into();
        let setup = app.setup_help();
        for expected in [
            "/tmp/my-config.toml",
            "[clusters.production]",
            "[language_server]",
            "--config",
            "az login",
            "Restart",
        ] {
            assert!(setup.contains(expected));
        }
    }
    #[tokio::test]
    async fn palette_keeps_invalid_input_and_does_not_execute_fuzzy_selection() {
        let mut app = App::new(Config::default(), Overrides::default(), UiState::default());
        let client = Client::new().unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let server = lsp::Handle::start(crate::config::LanguageServer {
            command: "dataexplorer-intentionally-missing-lsp".into(),
            args: vec![],
        });
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.command_prompt("xpt");
        key_event(&mut app, enter, &client, &tx, &server).unwrap();
        assert_eq!(app.prompt.as_ref().unwrap().text(), "export ");
        assert!(app.prompt.as_ref().unwrap().error.is_none());
        key_event(&mut app, enter, &client, &tx, &server).unwrap();
        assert!(
            app.prompt
                .as_ref()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .contains("required")
        );
        assert_eq!(app.prompt.as_ref().unwrap().text(), "export ");
        key_event(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &client,
            &tx,
            &server,
        )
        .unwrap();
        assert!(app.prompt.is_none());
        app.command_prompt("setup");
        key_event(&mut app, enter, &client, &tx, &server).unwrap();
        assert!(
            app.popup
                .as_ref()
                .unwrap()
                .text
                .contains("[language_server]")
        );
        server.shutdown().await;
    }
    #[tokio::test]
    async fn stale_jobs_cannot_overwrite_document_results_or_views() {
        let mut app = App::new(
            Config::default(),
            Overrides::default(),
            UiState {
                cluster_width: 25,
                editor_percent: 45,
            },
        );
        let server = lsp::Handle::start(crate::config::LanguageServer {
            command: "dataexplorer-intentionally-missing-lsp".into(),
            args: vec![],
        });
        let (tx, _rx) = mpsc::channel(8);
        app.version = 3;
        app.editor.insert_str("print x=1");
        app.handle_job(
            Job::Opened {
                path: "old.kql".into(),
                text: "print y=2".into(),
            },
            &tx,
            &server,
        );
        assert_eq!(app.inactive_tabs[&1].editor.lines(), ["print x=1"]);
        app.switch_tab(1);
        assert_eq!(app.editor.lines(), ["print x=1"]);
        app.handle_lsp(
            lsp::Event::Diagnostics {
                version: 2,
                diagnostics: json!([{"message":"old"}]),
            },
            &server,
        );
        assert_eq!(app.diagnostics, json!([]));
        app.handle_lsp(
            lsp::Event::Tokens {
                version: 2,
                data: vec![0, 0, 5, 0, 0],
                legend: vec!["keyword".into()],
            },
            &server,
        );
        assert!(app.tokens.is_empty());
        app.view_generation = 5;
        app.rows = vec![1];
        app.handle_job(
            Job::View {
                generation: 4,
                rows: Ok(vec![0]),
                chart: Err(anyhow::anyhow!("no chart")),
            },
            &tx,
            &server,
        );
        assert_eq!(app.rows, [1]);
        let target = Target {
            label: "fixture".into(),
            endpoint: "https://example.invalid".into(),
            database: "db".into(),
            tenant: None,
            limits: Default::default(),
        };
        app.handle_job(
            Job::Query {
                id: "old".into(),
                target,
                result: Ok(QueryResult::default()),
            },
            &tx,
            &server,
        );
        assert!(app.result.is_none());
        server.shutdown().await;
    }
    #[test]
    fn layout_resizing_collapse_maximize_snapshot() {
        let mut app = App::new(
            Config::default(),
            Overrides::default(),
            UiState {
                cluster_width: 25,
                editor_percent: 45,
            },
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let snapshot = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(snapshot.contains("DataExplorer | NO TARGET | IDLE"));
        assert!(snapshot.contains("Clusters / DB"));
        assert!(snapshot.contains("No results yet"));
        let panes = app.panes.areas(Rect::new(0, 1, 100, 27));
        assert_eq!(panes[0], Rect::new(0, 1, 25, 27));
        assert_eq!(panes[1], Rect::new(25, 1, 75, 12));
        assert_eq!(panes[2], Rect::new(25, 13, 75, 15));
        app.panes.collapsed[0] = true;
        assert_eq!(app.panes.areas(Rect::new(0, 0, 80, 20))[1].width, 80);
        app.panes.maximized = Some(1);
        assert_eq!(
            app.panes.areas(Rect::new(0, 0, 80, 20)),
            [Rect::default(), Rect::new(0, 0, 80, 20), Rect::default()]
        );
        for w in 0..100 {
            for h in 0..40 {
                app.panes.maximized = None;
                for p in app.panes.areas(Rect::new(0, 0, w, h)) {
                    assert!(p.right() <= w && p.bottom() <= h);
                }
            }
        }
    }
    #[test]
    fn unicode_editor_and_partial_results_snapshot() {
        let mut app = App::new(
            Config::default(),
            Overrides::default(),
            UiState {
                cluster_width: 20,
                editor_percent: 40,
            },
        );
        app.editor = TextArea::from(["print x='😀界'"]);
        app.tokens =
            lsp::decode_tokens(app.editor.lines(), &[0, 0, 5, 0, 0], &["keyword".into()]).unwrap();
        app.result = Some(Arc::new(
            crate::model::decode_v2(&crate::model::fixture(), 1).unwrap(),
        ));
        app.rows = vec![0];
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let snapshot = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(snapshot.contains("PARTIAL"));
        assert!(snapshot.contains("print x='😀"));
        assert!(snapshot.contains("9007199254740993"));
    }
}
