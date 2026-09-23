use crate::{
    chart::{self, ChartData, ChartKind},
    client::{Client, Metadata, normalize_schema},
    config::{self, Config, Overrides, Target, UiState},
    export::{self, Format},
    lsp::{self, TokenSpan},
    model::{FilterOp, QueryResult, ViewSpec, text},
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
    io::{IsTerminal, Write},
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
Ctrl-P or :   Command prompt (: only outside editor)
Ctrl-O/S/E    Open / save / export command prompt

Clusters: Up/Down selects; Enter activates target/discovers databases.
Editor: multiline, Shift-arrows selection, Ctrl-A select all,
Ctrl-Z undo, Ctrl-Y redo, Ctrl-X/C/V internal clipboard.
Ctrl-C copies only when no query is running.
Results: Up/Down/PgUp/PgDn, Left/Right columns, [/] primary table,
s toggle typed stable sort, / text filter, f column filter,
Enter large-cell detail (Up/Down scroll; Esc close).

Commands (quote paths/names containing spaces):
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
    Loaded {
        path: PathBuf,
        text: String,
        expected_version: i64,
    },
    Saved {
        path: PathBuf,
        version: i64,
    },
    Notice(String),
    Error(String),
}
struct Popup {
    title: String,
    text: String,
    scroll: u16,
}

struct App {
    config: Config,
    overrides: Overrides,
    target: Option<Target>,
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
    tokens: Vec<TokenSpan>,
    diagnostics: Value,
    lsp_status: String,
    result: Option<Arc<QueryResult>>,
    result_target: Option<Target>,
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
    prompt: Option<TextArea<'static>>,
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
            .map(|name| (name.clone(), None))
            .collect();
        let mut editor = TextArea::default();
        editor.set_tab_length(4);
        let mut app = Self {
            config,
            overrides,
            target,
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
            tokens: Vec::new(),
            diagnostics: json!([]),
            lsp_status: "starting language server".into(),
            result: None,
            result_target: None,
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
        if let Err(e) = app.config.resolve(&app.overrides) {
            app.notice(format!("No active target: {e}. Use target command or select a configured cluster. F1 for help."));
        }
        app
    }
    fn notice(&mut self, text: impl Into<String>) {
        self.messages.push(safe_text(&text.into()));
        if self.messages.len() > 100 {
            self.messages.remove(0);
        }
    }
    fn changed(&mut self) {
        self.version += 1;
        self.edited_at = Instant::now();
        self.dirty = true;
        self.tokens.clear();
        self.diagnostics = json!([]);
        self.completions.clear();
    }
    fn command_prompt(&mut self, prefix: &str) {
        self.popup = None;
        self.completions.clear();
        self.prompt = Some(TextArea::from([prefix]));
        if let Some(prompt) = &mut self.prompt {
            prompt.move_cursor(CursorMove::End);
        }
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
        let id = Client::request_id();
        let cancel = CancellationToken::new();
        self.active = Some(Active {
            id: id.clone(),
            target: target.clone(),
            cancel: cancel.clone(),
        });
        self.notice(format!(
            "RUNNING {} / {} [{id}]",
            target.label, target.database
        ));
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = client.query(&target, &query, &id, cancel).await;
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
                self.active = None;
                match result {
                    Ok(result) => {
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
            Job::Loaded {
                path,
                text,
                expected_version,
            } => {
                if self.version != expected_version {
                    self.notice("Open discarded: document changed while file was loading. Retry open to replace it.");
                    return;
                }
                self.editor.select_all();
                self.editor.insert_str(text);
                self.file = Some(path);
                self.changed();
                self.dirty = false;
                self.editor_top = 0;
                self.editor_left = 0;
            }
            Job::Saved { path, version } => {
                self.file = Some(path.clone());
                if version == self.version {
                    self.dirty = false;
                }
                self.notice(format!("Saved {}", path.display()));
            }
            Job::Error(error) => self.notice(format!("ERROR: {error}")),
            Job::Notice(notice) => self.notice(notice),
            _ => {}
        }
    }
    fn handle_lsp(&mut self, event: lsp::Event, handle: &lsp::Handle) {
        match event {
            lsp::Event::Ready => {
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
            lsp::Event::Completion { version, value } if version == self.version => {
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
            lsp::Event::Hover { version, value } if version == self.version => {
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

#[derive(Parser)]
#[command(no_binary_name = true, disable_help_flag = true)]
enum Command {
    Target {
        cluster: String,
        #[arg(short, long)]
        database: Option<String>,
        #[arg(long)]
        tenant: Option<String>,
    },
    Database {
        name: String,
    },
    Open {
        path: PathBuf,
        #[arg(long)]
        force: bool,
    },
    Save {
        path: PathBuf,
        #[arg(long)]
        force: bool,
    },
    Filter {
        #[arg(num_args = 0.., allow_hyphen_values = true)]
        text: Vec<String>,
    },
    Column {
        index: usize,
        op: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    Clear,
    Export {
        #[arg(value_enum)]
        format: Format,
        scope: String,
        path: PathBuf,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        accept_partial: bool,
    },
    Quit {
        #[arg(long)]
        force: bool,
    },
}
fn command(
    app: &mut App,
    text: &str,
    client: &Client,
    tx: &mpsc::Sender<Job>,
    lsp: &lsp::Handle,
) -> Result<()> {
    match Command::try_parse_from(shell_words::split(text)?)? {
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
        Command::Open { path, force } => {
            ensure!(
                !app.dirty || force,
                "editor has unsaved changes; save first or open PATH --force"
            );
            let tx = tx.clone();
            let expected_version = app.version;
            tokio::spawn(async move {
                let result = async {
                    let size = tokio::fs::metadata(&path).await?.len();
                    ensure!(
                        size <= 4 * 1024 * 1024,
                        "query file exceeds 4 MiB editor safety limit"
                    );
                    let text = tokio::fs::read_to_string(&path).await?;
                    Ok::<_, anyhow::Error>(Job::Loaded {
                        path,
                        text,
                        expected_version,
                    })
                }
                .await;
                let _ = tx
                    .send(result.unwrap_or_else(|e| Job::Error(format!("Open failed: {e}"))))
                    .await;
            });
        }
        Command::Save { path, force } => {
            let text = app.editor.lines().join("\n");
            let version = app.version;
            let tx = tx.clone();
            tokio::task::spawn_blocking(move || {
                let result = config::atomic_write(&path, force, |w| {
                    w.write_all(text.as_bytes())?;
                    Ok(())
                });
                let job = match result {
                    Ok(()) => Job::Saved { path, version },
                    Err(e) => Job::Error(format!("Save failed: {e:#}")),
                };
                let _ = tx.blocking_send(job);
            });
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
                !app.dirty || force,
                "editor has unsaved changes; save or quit --force"
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
    if let Err(e) = state {
        app.notice(format!(
            "UI state could not be loaded: {e:#}; using default layout"
        ));
    }
    let client = Client::new()?;
    let (tx, rx) = mpsc::channel(64);
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
                            if let Some(prompt) = &mut app.prompt { prompt.insert_str(text.replace(['\n','\r'], " ")); }
                            else if app.focus == 1 && app.popup.is_none() { app.editor.insert_str(text); app.changed(); }
                        }
                        Event::Mouse(mouse) => {
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
                    Some(event) => app.handle_lsp(event, &lsp),
                    None => { lsp_alive = false; app.lsp_status = "LSP offline (editor usable)".into(); }
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

fn key_event(
    app: &mut App,
    key: KeyEvent,
    client: &Client,
    tx: &mpsc::Sender<Job>,
    lsp: &lsp::Handle,
) -> Result<()> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if ctrl && key.code == KeyCode::Char('q') {
        if app.dirty {
            app.command_prompt("quit ");
            app.notice("Unsaved changes: save or enter quit --force.");
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
    if let Some(prompt) = &mut app.prompt {
        match key.code {
            KeyCode::Esc => app.prompt = None,
            KeyCode::Enter => {
                let text = prompt.lines().join(" ");
                app.prompt = None;
                command(app, &text, client, tx, lsp)?;
            }
            _ => {
                prompt.input(key);
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
            KeyCode::Char('o') => app.command_prompt("open "),
            KeyCode::Char('s') => {
                let prefix = app
                    .file
                    .as_ref()
                    .map(|p| format!("save {} ", shell_words::quote(&p.to_string_lossy())))
                    .unwrap_or("save ".into());
                app.command_prompt(&prefix);
            }
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
                    app.overrides.tenant = None;
                    app.overrides.database = db.or_else(|| {
                        app.config
                            .clusters
                            .get(&alias)
                            .and_then(|c| c.database.clone())
                    });
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
fn editor(f: &mut Frame, app: &mut App, area: Rect) {
    if area.width < 3 || area.height < 3 {
        return;
    }
    let title = format!(
        "Query{} {} | F5 run | {}",
        if app.dirty { "*" } else { "" },
        app.file
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or("untitled".into()),
        app.lsp_status
    );
    let b = block(safe_text(&title), app.focus == 1);
    let inner = b.inner(area);
    f.render_widget(b, area);
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
        let tokens: Vec<_> = app.tokens.iter().filter(|t| t.line == line_no).collect();
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
            if display_col > app.editor_left + inner.width as usize {
                break;
            }
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, inner.y + visible as u16, inner.width, 1),
        );
    }
    if app.focus == 1 && app.prompt.is_none() && app.popup.is_none() && app.completions.is_empty() {
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
        "{} T{}/{} {} | {}/{} LOCAL rows{} | {} | col {} | F7 chart",
        if result.partial { "PARTIAL" } else { "Results" },
        app.table + 1,
        result.tables.len(),
        t.name,
        app.rows.len(),
        t.rows.len(),
        if app.view_pending { " (analyzing)" } else { "" },
        target,
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
                    &db.as_ref()
                        .map(|d| format!("  {d}"))
                        .unwrap_or(alias.clone()),
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
        "{}\nF1 help | F5 run | Ctrl-P commands | Tab focus | {diagnostic_count} language diagnostics (F8)",
        app.messages.last().map(String::as_str).unwrap_or("Ready.")
    );
    f.render_widget(Paragraph::new(status), chunks[2]);
    if let Some(prompt) = &mut app.prompt {
        let rect = Rect::new(area.x, area.bottom() - 3, area.width, 3);
        f.render_widget(Clear, rect);
        prompt.set_block(block("Command (Enter / Esc)".into(), true));
        f.render_widget(&*prompt, rect);
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
    use ratatui::backend::TestBackend;
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
            Job::Loaded {
                path: "old.kql".into(),
                text: "stale".into(),
                expected_version: 2,
            },
            &tx,
            &server,
        );
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
