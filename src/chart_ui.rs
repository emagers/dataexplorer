use crate::{
    chart::{self, ChartColumns, ChartKind},
    model::{Table, numeric_type},
    safe_text,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

pub struct ChartOptions {
    pub columns: ChartColumns,
    kind: ChartKind,
    field: usize,
    selected: [usize; 3],
    pub error: Option<String>,
}

#[derive(Clone, Copy)]
pub enum Action {
    Stay,
    Cancel,
    Apply,
    Reset,
}

impl ChartOptions {
    pub fn new(table: &Table, current: Option<&ChartColumns>) -> Result<Self> {
        let kind = chart::kind(table)?;
        let (columns, error) = match current
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| chart::default_columns(table))
        {
            Ok(columns) => (columns, None),
            Err(error) => (
                ChartColumns::infer(table, kind)?,
                Some(format!("Metadata mapping: {error}. Choose columns below.")),
            ),
        };
        let selected = [
            columns.x,
            columns.y.first().copied().unwrap_or(0),
            columns.series.first().copied().unwrap_or(0),
        ];
        Ok(Self {
            columns,
            kind,
            field: 0,
            selected,
            error,
        })
    }

    fn enabled(&self, table: &Table, field: usize, index: usize) -> bool {
        let column = &table.columns[index];
        match field {
            0 => chart::x_compatible(self.kind, &column.kind),
            1 => index != self.columns.x && numeric_type(&column.kind),
            _ => index != self.columns.x && !self.columns.y.contains(&index),
        }
    }

    pub fn key(&mut self, key: KeyEvent, table: &Table) -> Action {
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Char('r') => return Action::Reset,
            KeyCode::Tab | KeyCode::BackTab => {
                let backwards =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.field = (self.field + if backwards { 2 } else { 1 }) % 3;
            }
            KeyCode::Left => self.field = (self.field + 2) % 3,
            KeyCode::Right => self.field = (self.field + 1) % 3,
            KeyCode::Up => self.selected[self.field] = self.selected[self.field].saturating_sub(1),
            KeyCode::Down => {
                self.selected[self.field] =
                    (self.selected[self.field] + 1).min(table.columns.len().saturating_sub(1))
            }
            KeyCode::Char(' ') => {
                let index = self.selected[self.field];
                if !self.enabled(table, self.field, index) {
                    self.error = Some(
                        "Column unavailable for this role: check its type and other selections."
                            .into(),
                    );
                    return Action::Stay;
                }
                match self.field {
                    0 => {
                        self.columns.x = index;
                        self.columns.y.retain(|&i| i != index);
                        self.columns.series.retain(|&i| i != index);
                    }
                    1 => {
                        toggle(&mut self.columns.y, index);
                        self.columns.series.retain(|&i| i != index);
                    }
                    _ => toggle(&mut self.columns.series, index),
                }
                self.error = None;
            }
            KeyCode::Enter => match self.columns.validate(table, self.kind) {
                Ok(()) => return Action::Apply,
                Err(error) => self.error = Some(error.to_string()),
            },
            _ => {}
        }
        Action::Stay
    }

    pub fn draw(&self, f: &mut Frame, table: &Table) {
        let area = f.area();
        let width = area.width.saturating_sub(4).min(120);
        let height = area.height.saturating_sub(2).min(26);
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        f.render_widget(Clear, rect);
        let border = Block::default()
            .borders(Borders::ALL)
            .title("Chart columns - current result table");
        let inner = border.inner(rect);
        f.render_widget(border, rect);
        let parts = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(2),
        ])
        .split(inner);
        f.render_widget(
            Paragraph::new(
                "X: one column; Y: numeric measures.\nSeries: optional grouping columns.",
            )
            .wrap(Wrap { trim: false }),
            parts[0],
        );
        let lists = Layout::horizontal([Constraint::Ratio(1, 3); 3]).split(parts[1]);
        for field in 0..3 {
            let items = table
                .columns
                .iter()
                .enumerate()
                .map(|(index, column)| {
                    let selected = match field {
                        0 => self.columns.x == index,
                        1 => self.columns.y.contains(&index),
                        _ => self.columns.series.contains(&index),
                    };
                    let enabled = self.enabled(table, field, index);
                    ListItem::new(safe_text(&format!(
                        "[{}] {index}: {} ({})",
                        if selected { "x" } else { " " },
                        column.name,
                        column.kind
                    )))
                    .style(Style::default().fg(if enabled {
                        Color::White
                    } else {
                        Color::DarkGray
                    }))
                })
                .collect::<Vec<_>>();
            f.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(["X axis", "Y measures", "Series groups"][field])
                            .border_style(Style::default().fg(if field == self.field {
                                Color::Cyan
                            } else {
                                Color::DarkGray
                            })),
                    )
                    .highlight_style(Style::default().bg(if field == self.field {
                        Color::DarkGray
                    } else {
                        Color::Reset
                    })),
                lists[field],
                &mut ListState::default().with_selected(Some(self.selected[field])),
            );
        }
        let message = self.error.as_deref().unwrap_or("Changes affect only this fetched table, not the query or exports.\nTime/line points are sorted by X; selections reset on new results.");
        f.render_widget(
            Paragraph::new(safe_text(message))
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(if self.error.is_some() {
                    Color::Yellow
                } else {
                    Color::Gray
                })),
            parts[2],
        );
        f.render_widget(
            Paragraph::new(
                "Tab roles | Up/Down column | Space select\nEnter apply | R metadata | Esc cancel",
            )
            .wrap(Wrap { trim: false }),
            parts[3],
        );
    }
}

fn toggle(indices: &mut Vec<usize>, index: usize) {
    if indices.contains(&index) {
        indices.retain(|&i| i != index);
    } else {
        indices.push(index);
        indices.sort_unstable();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn press(options: &mut ChartOptions, table: &Table, code: KeyCode) -> Action {
        options.key(KeyEvent::new(code, KeyModifiers::NONE), table)
    }

    #[test]
    fn choose_axes_groups_validate_cancel_and_reset() {
        let table = chart::tests::time_table();
        let mut options = ChartOptions::new(&table, None).unwrap();
        assert_eq!(options.columns.x, 1);
        for _ in 0..3 {
            press(&mut options, &table, KeyCode::Down);
        }
        press(&mut options, &table, KeyCode::Char(' '));
        assert_eq!(options.columns.x, 4);
        press(&mut options, &table, KeyCode::Tab);
        press(&mut options, &table, KeyCode::Char(' '));
        assert!(matches!(
            press(&mut options, &table, KeyCode::Enter),
            Action::Stay
        ));
        assert!(options.error.as_ref().unwrap().contains("at least one"));
        for _ in 0..3 {
            press(&mut options, &table, KeyCode::Down);
        }
        press(&mut options, &table, KeyCode::Char(' '));
        press(&mut options, &table, KeyCode::Tab);
        press(&mut options, &table, KeyCode::Char(' '));
        assert_eq!(
            options.columns,
            ChartColumns {
                x: 4,
                y: vec![3],
                series: vec![]
            }
        );
        assert!(matches!(
            press(&mut options, &table, KeyCode::Enter),
            Action::Apply
        ));
        assert!(matches!(
            press(&mut options, &table, KeyCode::Esc),
            Action::Cancel
        ));
        assert!(matches!(
            press(&mut options, &table, KeyCode::Char('r')),
            Action::Reset
        ));
    }

    #[test]
    fn invalid_metadata_is_visible_and_popup_renders_at_supported_sizes() {
        let mut table = chart::tests::time_table();
        table.visualization.as_mut().unwrap()["XColumn"] = serde_json::json!("missing");
        let mut options = ChartOptions::new(&table, None).unwrap();
        assert!(options.error.as_ref().unwrap().contains("missing"));
        options.columns.series = vec![2];
        options.field = 2;
        for (width, height) in [(120, 30), (80, 24), (50, 14)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| options.draw(f, &table)).unwrap();
            let buffer: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(buffer.contains("Chart columns"));
            assert!(buffer.contains("X axis"));
            assert!(buffer.contains("Y measures"));
            assert!(buffer.contains("Series groups"));
            assert!(buffer.contains("Enter apply | R metadata | Esc cancel"));
        }
        options.selected[2] = 0;
        press(&mut options, &table, KeyCode::Char(' '));
        assert!(options.error.as_ref().unwrap().contains("unavailable"));
        assert_eq!(options.columns.series, [2]);
    }
}
