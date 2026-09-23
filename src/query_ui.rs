use crate::{
    palette::fuzzy_score,
    query_library::{Documentation, Entry, Parameter, Values},
    safe_text,
};
use anyhow::{Result, ensure};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use tui_textarea::{CursorMove, TextArea};

fn area(f: &Frame) -> Rect {
    let a = f.area();
    Rect::new(
        a.x + 1,
        a.y + 1,
        a.width.saturating_sub(2),
        a.height.saturating_sub(2),
    )
}

#[derive(Default)]
pub struct Browser {
    pub search: TextArea<'static>,
    pub selected: usize,
    pub scroll: u16,
    matched: Vec<usize>,
    last_search: Option<String>,
}

impl Browser {
    pub fn invalidate(&mut self) {
        self.last_search = None;
    }
    fn refresh(&mut self, entries: &[Entry]) {
        let search = self.search.lines().join(" ");
        if self.last_search.as_ref() != Some(&search) {
            self.matched = self.matches(entries);
            self.last_search = Some(search);
        }
    }
    pub fn matches(&self, entries: &[Entry]) -> Vec<usize> {
        let needle = self.search.lines().join(" ").to_lowercase();
        let mut matches = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                let path = entry.relative.to_string_lossy().to_lowercase();
                let prose = entry.documentation.preview().to_lowercase();
                fuzzy_score(&path, &needle)
                    .or_else(|| fuzzy_score(&prose, &needle).map(|s| s + 10000))
                    .map(|score| (score, index))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|&(score, index)| (score, index));
        matches.into_iter().map(|(_, index)| index).collect()
    }
    pub fn key(&mut self, key: KeyEvent, entries: &[Entry]) -> Option<usize> {
        self.refresh(entries);
        let matches = &self.matched;
        match key.code {
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.scroll = 0;
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(matches.len().saturating_sub(1));
                self.scroll = 0;
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(5),
            KeyCode::Enter => return matches.get(self.selected).copied(),
            _ => {
                if self.search.input(key) {
                    self.selected = 0;
                    self.scroll = 0;
                }
            }
        }
        None
    }
    pub fn paste(&mut self, text: &str) {
        self.search.insert_str(text.replace(['\r', '\n'], " "));
        self.selected = 0;
        self.scroll = 0;
    }
    pub fn draw(&mut self, f: &mut Frame, entries: &[Entry], loading: bool) {
        let rect = area(f);
        f.render_widget(Clear, rect);
        let parts = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(2),
        ])
        .split(rect);
        self.search.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title("Query library - fuzzy search path, description, parameters"),
        );
        f.render_widget(&self.search, parts[0]);
        let body = Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(parts[1]);
        self.refresh(entries);
        let matches = &self.matched;
        self.selected = self.selected.min(matches.len().saturating_sub(1));
        let items = matches
            .iter()
            .map(|&i| ListItem::new(safe_text(&entries[i].relative.to_string_lossy())))
            .collect::<Vec<_>>();
        f.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "{} queries{}",
                    matches.len(),
                    if loading { " (loading)" } else { "" }
                )))
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan)),
            body[0],
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        let preview = matches
            .get(self.selected)
            .map(|&i| entries[i].documentation.preview())
            .unwrap_or_else(|| {
                if loading {
                    "Scanning query_path recursively..."
                } else {
                    "No matching queries. Ctrl-S saves a new query; Ctrl-L reloads the library."
                }
                .into()
            });
        let widget = Paragraph::new(safe_text(&preview)).wrap(Wrap { trim: false });
        let max = widget
            .line_count(body[1].width.saturating_sub(2))
            .saturating_sub(body[1].height.saturating_sub(2) as usize);
        self.scroll = self.scroll.min(max.min(u16::MAX as usize) as u16);
        f.render_widget(
            widget
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Description and parameter documentation"),
                )
                .scroll((self.scroll, 0)),
            body[1],
        );
        f.render_widget(Paragraph::new("Up/Down highlight | Enter open/focus tab | PgUp/PgDn preview\nCtrl-L reload | Esc cancel"), parts[2]);
    }
}

pub struct Field {
    pub label: String,
    pub input: TextArea<'static>,
    pub enabled: Option<bool>,
}
impl Field {
    pub fn new(label: &str, text: &str) -> Self {
        let mut input = TextArea::from(text.lines().map(str::to_owned).collect::<Vec<_>>());
        input.move_cursor(CursorMove::End);
        Self {
            label: label.into(),
            input,
            enabled: None,
        }
    }
    fn optional(label: &str, text: Option<&str>) -> Self {
        let mut field = Self::new(label, text.unwrap_or(""));
        field.enabled = Some(text.is_some());
        field
    }
    pub fn text(&self) -> String {
        self.input.lines().join("\n")
    }
    fn optional_text(&self) -> Option<String> {
        self.enabled.unwrap_or(true).then(|| self.text())
    }
}

pub struct Form {
    pub title: String,
    pub fields: Vec<Field>,
    pub selected: usize,
    pub error: Option<String>,
}
impl Form {
    pub fn new(title: &str, fields: Vec<Field>) -> Self {
        Self {
            title: title.into(),
            fields,
            selected: 0,
            error: None,
        }
    }
    pub fn key(&mut self, key: KeyEvent) {
        let count = self.fields.len();
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('a') {
            self.fields[self.selected].input.select_all();
            return;
        }
        match key.code {
            KeyCode::BackTab => self.selected = (self.selected + count - 1) % count,
            KeyCode::Tab => self.selected = (self.selected + 1) % count,
            KeyCode::F(4) => {
                if let Some(enabled) = &mut self.fields[self.selected].enabled {
                    *enabled = !*enabled;
                }
            }
            _ => {
                let field = &mut self.fields[self.selected];
                if field.input.input(key) {
                    if let Some(enabled) = &mut field.enabled {
                        *enabled = true;
                    }
                    self.error = None;
                }
            }
        }
    }
    pub fn paste(&mut self, text: &str) {
        let field = &mut self.fields[self.selected];
        field.input.insert_str(text);
        if let Some(enabled) = &mut field.enabled {
            *enabled = true;
        }
    }
    pub fn draw(&mut self, f: &mut Frame) {
        let rect = area(f);
        f.render_widget(Clear, rect);
        let parts = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
        ])
        .split(rect);
        f.render_widget(
            Paragraph::new(safe_text(&self.title)).style(Style::default().fg(Color::Cyan)),
            parts[0],
        );
        // Show the focused field at usable height even on small terminals.
        let capacity = (parts[1].height as usize / 3).max(1).min(self.fields.len());
        let total = self.fields.len();
        let start = self.selected.saturating_sub(capacity - 1);
        let rows = Layout::vertical(vec![Constraint::Min(3); capacity]).split(parts[1]);
        for (i, field) in self
            .fields
            .iter_mut()
            .enumerate()
            .skip(start)
            .take(capacity)
        {
            field.input.set_block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(if i == self.selected {
                        Color::Cyan
                    } else {
                        Color::DarkGray
                    }))
                    .title(format!(
                        "{} / {}: {}{}",
                        i + 1,
                        total,
                        field.label,
                        match field.enabled {
                            Some(false) => " [unset; F4 enable]",
                            Some(true) => " [set; F4 unset]",
                            None => "",
                        }
                    )),
            );
            f.render_widget(&field.input, rows[i - start]);
        }
        f.render_widget(Paragraph::new(safe_text(self.error.as_deref().unwrap_or(
            "Tab/Shift-Tab fields | Enter newline | Ctrl-S accept | Esc cancel\nDefault/value fields: F4 toggles unset vs set (including empty string).\nValues are plain text: ISO datetime, [-][days.]hh:mm:ss timespan, JSON dynamic."
        ))).wrap(Wrap { trim: false }).style(Style::default().fg(if self.error.is_some() { Color::Red } else { Color::Yellow })), parts[2]);
    }
}

pub struct Parameters {
    pub documentation: Documentation,
    pub values: Values,
    pub selected: usize,
    pub form: Option<Form>,
    editing: Option<usize>,
    pub error: Option<String>,
}
impl Parameters {
    pub fn new(documentation: Documentation, mut values: Values) -> Self {
        values.retain(|name, _| documentation.parameters.iter().any(|p| p.name == *name));
        Self {
            documentation,
            values,
            selected: 0,
            form: None,
            editing: None,
            error: None,
        }
    }
    fn edit(&mut self, index: Option<usize>) {
        let parameter = index
            .map(|i| self.documentation.parameters[i].clone())
            .unwrap_or(Parameter {
                kind: "string".into(),
                ..Default::default()
            });
        self.form = Some(Form::new(
            "Parameter definition and per-tab value",
            vec![
                Field::new("Name (ASCII identifier)", &parameter.name),
                Field::new(
                    "Kusto type (string/bool/int/long/real/decimal/datetime/timespan/guid/dynamic)",
                    &parameter.kind,
                ),
                Field::new("Description", &parameter.description),
                Field::optional(
                    "Default (saved in query; no secrets)",
                    parameter.default.as_deref(),
                ),
                Field::optional(
                    "Execution value (memory only)",
                    self.values.get(&parameter.name).map(String::as_str),
                ),
            ],
        ));
        self.editing = index;
    }
    fn accept_form(&mut self) -> Result<()> {
        let form = self.form.as_ref().expect("active parameter form");
        let parameter = Parameter {
            name: form.fields[0].text().trim().into(),
            kind: form.fields[1].text().trim().into(),
            description: form.fields[2].text(),
            default: form.fields[3].optional_text(),
        };
        parameter.validate()?;
        ensure!(
            !self
                .documentation
                .parameters
                .iter()
                .enumerate()
                .any(|(i, p)| Some(i) != self.editing && p.name == parameter.name),
            "duplicate parameter {}",
            parameter.name
        );
        let value = form.fields[4].optional_text();
        if let Some(value) = &value {
            parameter.literal(value)?;
        }
        if let Some(index) = self.editing {
            let old = &self.documentation.parameters[index];
            self.values.remove(&old.name);
            self.documentation.parameters[index] = parameter.clone();
        } else {
            self.documentation.parameters.push(parameter.clone());
            self.selected = self.documentation.parameters.len() - 1;
        }
        if let Some(value) = value {
            self.values.insert(parameter.name, value);
        }
        self.form = None;
        Ok(())
    }
    // True means apply the complete parameter list. Esc is handled by the outer dialog.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let accept =
            key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s');
        if self.form.is_some() {
            if key.code == KeyCode::Esc {
                self.form = None;
            } else if accept {
                if let Err(e) = self.accept_form() {
                    self.form.as_mut().unwrap().error = Some(format!("{e:#}"));
                }
            } else {
                self.form.as_mut().unwrap().key(key);
            }
            return false;
        }
        let count = self.documentation.parameters.len();
        match key.code {
            KeyCode::F(3) => self.edit(None),
            KeyCode::Enter if count > 0 => self.edit(Some(self.selected)),
            KeyCode::Delete if count > 0 => {
                let parameter = self.documentation.parameters.remove(self.selected);
                self.values.remove(&parameter.name);
                self.selected = self.selected.min(count.saturating_sub(2));
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(count.saturating_sub(1)),
            _ => {}
        }
        accept
    }
    pub fn draw(&mut self, f: &mut Frame, saving: bool) {
        if let Some(form) = &mut self.form {
            form.draw(f);
            return;
        }
        let rect = area(f);
        f.render_widget(Clear, rect);
        let parts = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(6),
            Constraint::Length(3),
        ])
        .split(rect);
        let items = self
            .documentation
            .parameters
            .iter()
            .map(|p| {
                ListItem::new(safe_text(&format!(
                    "{} : {} | default: {} | value: {}",
                    p.name,
                    p.kind,
                    p.default.as_deref().unwrap_or("(required)"),
                    if self.values.contains_key(&p.name) {
                        "(set in memory)"
                    } else {
                        "(not set)"
                    }
                )))
            })
            .collect::<Vec<_>>();
        f.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::ALL).title(if saving {
                    "Save query: define/document parameters (optional)"
                } else {
                    "Current tab parameters - definitions and execution values"
                }))
                .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan)),
            parts[0],
            &mut ListState::default().with_selected(Some(self.selected)),
        );
        let help = self.documentation.parameters.get(self.selected).map(|p| p.description.as_str())
            .unwrap_or("No parameters. F3 adds a name, type, description and optional default.\nRequired execution values can be supplied later with F4.");
        f.render_widget(
            Paragraph::new(safe_text(help))
                .wrap(Wrap { trim: false })
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("Parameter description"),
                ),
            parts[1],
        );
        f.render_widget(Paragraph::new(safe_text(self.error.as_deref().unwrap_or(
            "F3 add | Enter edit | Delete remove | Up/Down select\nCtrl-S apply/continue | Esc cancel\nDefaults are saved; execution values stay in this tab's memory only."
        ))).wrap(Wrap { trim: false }), parts[2]);
    }
}

pub enum Dialog {
    SaveHeader {
        form: Form,
        documentation: Documentation,
        values: Values,
        in_library: bool,
    },
    Parameters {
        parameters: Parameters,
        save_path: Option<(String, bool)>,
    },
    Overwrite {
        path: String,
        documentation: Documentation,
        values: Values,
        in_library: bool,
    },
    Close {
        quit: bool,
    },
}
impl Dialog {
    pub fn paste(&mut self, text: &str) {
        match self {
            Self::SaveHeader { form, .. } => form.paste(text),
            Self::Parameters { parameters, .. } => {
                if let Some(form) = &mut parameters.form {
                    form.paste(text);
                }
            }
            _ => {}
        }
    }
    pub fn draw(&mut self, f: &mut Frame) {
        match self {
            Self::SaveHeader { form, .. } => form.draw(f),
            Self::Parameters {
                parameters,
                save_path,
            } => parameters.draw(f, save_path.is_some()),
            Self::Overwrite { path, .. } => {
                let rect = area(f);
                f.render_widget(Clear, rect);
                f.render_widget(Paragraph::new(safe_text(&format!("Save {}?\n\nY confirms saving and replacing the file if it already exists. Esc/N cancels.\nYour current tab remains unsaved until the write succeeds.\nOnly documented defaults are persisted, never execution values.", path)))
                    .block(Block::default().borders(Borders::ALL).title("Confirm query save")).wrap(Wrap { trim: false }), rect);
            }
            Self::Close { quit } => {
                let rect = area(f);
                f.render_widget(Clear, rect);
                f.render_widget(Paragraph::new(if *quit {
                    "One or more tabs have unsaved changes.\nY discards ALL unsaved tabs and quits.\nEsc/N returns to the editor; Ctrl-S saves the current tab."
                } else {
                    "This tab has unsaved changes.\nS opens Save; Y discards this tab; Esc/N cancels."
                }).block(Block::default().borders(Borders::ALL).title("Unsaved queries")).wrap(Wrap { trim: false }), rect);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_preview_renders_description_and_parameter_definitions() {
        let entries = vec![Entry {
            path: "team/query.kql".into(),
            relative: "team/query.kql".into(),
            text: "print limit".into(),
            documentation: Documentation {
                description: "Investigate user events".into(),
                parameters: vec![Parameter {
                    name: "limit".into(),
                    kind: "long".into(),
                    description: "Maximum records to return".into(),
                    default: Some("25".into()),
                }],
            },
        }];
        let mut browser = Browser::default();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| browser.draw(f, &entries, false)).unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        for expected in [
            "team/query.kql",
            "Investigate user events",
            "limit : long",
            "Maximum records to return",
            "Default: 25",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
    }
    #[test]
    fn browser_finds_nested_paths_and_documentation() {
        let entries = vec![Entry {
            path: "teams/users.kql".into(),
            relative: "teams/users.kql".into(),
            text: "print 1".into(),
            documentation: Documentation {
                description: "Audit suspicious logins".into(),
                ..Default::default()
            },
        }];
        let mut browser = Browser::default();
        browser.paste("tmusr");
        assert_eq!(browser.matches(&entries), vec![0]);
        browser.search = TextArea::from(["suspicious"]);
        assert_eq!(browser.matches(&entries), vec![0]);
    }
    #[test]
    fn optional_empty_string_and_cancelled_edits() {
        let mut parameters = Parameters::new(Documentation::default(), Values::new());
        parameters.edit(None);
        let form = parameters.form.as_mut().unwrap();
        form.fields[0].input = TextArea::from(["region"]);
        form.fields[3].enabled = Some(true);
        parameters.accept_form().unwrap();
        assert_eq!(
            parameters.documentation.parameters[0].default,
            Some(String::new())
        );
        parameters.edit(Some(0));
        parameters.form.as_mut().unwrap().fields[0].input = TextArea::from(["other"]);
        parameters.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(parameters.documentation.parameters[0].name, "region");
    }
}
