use arustex_core::View;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Paragraph, Row, Table, TableState, Tabs, Wrap},
};

use crate::app::{Browser, Record, TABS};

pub(super) fn render(frame: &mut Frame<'_>, browser: &Browser) {
    let area = frame.area();
    if area.height < 9 || area.width < 34 {
        frame.render_widget(Paragraph::new("Resize the terminal  [q] Quit"), area);
        return;
    }
    let [title, tabs, content, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("ARUSTEX", bold()),
            Span::raw("  PostgreSQL case review"),
        ])),
        title,
    );
    frame.render_widget(
        Tabs::new(TABS.iter().map(|(_, label)| *label))
            .select(browser.tab_index())
            .highlight_style(accent())
            .divider("·"),
        tabs,
    );

    let records = browser.visible_records();
    let record = records.get(browser.selected).copied();
    if browser.details
        && let Some(record) = record
    {
        let [table, detail] = if content.width >= 100 {
            Layout::horizontal([Constraint::Percentage(61), Constraint::Percentage(39)])
                .areas(content)
        } else {
            Layout::vertical([Constraint::Fill(3), Constraint::Fill(2)]).areas(content)
        };
        render_table(frame, table, browser, &records);
        render_details(frame, detail, browser.view, record);
    } else {
        render_table(frame, content, browser, &records);
    }
    render_footer(frame, footer, browser, records.len());
}

fn render_table(frame: &mut Frame<'_>, area: Rect, browser: &Browser, records: &[&Record]) {
    let table_title = match browser.view {
        View::Exploration { task_id } => format!(" Exploration nodes · task #{task_id} "),
        View::TrafficEvidence => " Traffic evidence snapshots ".into(),
        _ => format!(" {} ", TABS[browser.tab_index()].1),
    };
    let block = Block::bordered().title(table_title);

    if browser.loading || browser.failure.is_some() || records.is_empty() {
        let message = if browser.loading {
            "Loading records…"
        } else if let Some(error) = &browser.failure {
            error
        } else if browser.record_count() != 0 {
            "No records on this page match the filter."
        } else {
            "No records on this page."
        };
        let style = if browser.failure.is_some() {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(style)
                .wrap(Wrap { trim: true })
                .block(block),
            area,
        );
        return;
    }

    let columns = columns(browser.view);
    let id_width = records
        .iter()
        .map(|record| record.id.to_string().len() as u16)
        .max()
        .unwrap_or(2)
        .max(2);
    let widths = std::iter::once(Constraint::Length(id_width))
        .chain(columns.iter().map(|&(_, width)| Constraint::Fill(width)));
    let rows = records.iter().map(|record| {
        let values = std::iter::once(Cell::from(record.id.to_string()))
            .chain(record.fields.iter().map(|field| Cell::from(field.as_str())));
        Row::new(values)
    });
    let headers = std::iter::once("ID").chain(columns.iter().map(|&(label, _)| label));
    let table = Table::new(rows, widths)
        .header(Row::new(headers).style(bold()))
        .block(block)
        .row_highlight_style(accent())
        .highlight_symbol("› ");
    let mut state = TableState::default().with_selected(Some(browser.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_details(frame: &mut Frame<'_>, area: Rect, view: View, record: &Record) {
    let entries = columns(view)
        .iter()
        .zip(&record.fields)
        .map(|(&(label, _), value)| {
            Line::from(vec![
                Span::styled(label, bold()),
                Span::raw("  "),
                Span::raw(value),
            ])
        });
    let lines = std::iter::once(Line::from(format!("#{}", record.id)))
        .chain(entries)
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(Block::bordered().title(" Record ")),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, browser: &Browser, count: usize) {
    let [status, controls, view_controls] =
        Layout::vertical([Constraint::Length(1); 3]).areas(area);
    let (previous, next) = browser.page_directions();
    let status_text = if browser.editing_filter() {
        format!("Filter this page  {}▏", browser.filter)
    } else if !browser.filter.is_empty() {
        format!(
            "{count}/{} records  ·  filter: {}",
            browser.record_count(),
            browser.filter
        )
    } else {
        format!("{} records on this page", browser.record_count())
    };
    frame.render_widget(Paragraph::new(status_text), status);

    if browser.editing_filter() {
        frame.render_widget(Paragraph::new("[Enter] Apply  [Esc] Cancel"), controls);
        return;
    }
    frame.render_widget(
        Paragraph::new("[Tab] Views  [↑/↓] Move  [/] Filter  [d] Detail  [r] Refresh  [q] Quit"),
        controls,
    );
    let mut help = Vec::new();
    if previous {
        help.push("[p] Previous page");
    }
    if next {
        help.push("[n] Next page");
    }
    if browser.view == View::Tasks {
        help.push("[Enter] Exploration nodes");
    }
    if matches!(browser.view, View::Exploration { .. }) {
        help.push("[Esc] Back to tasks");
    }
    frame.render_widget(Paragraph::new(help.join("  ")), view_controls);
}

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn accent() -> Style {
    bold().fg(Color::Cyan)
}

fn columns(view: View) -> &'static [(&'static str, u16)] {
    match view {
        View::Tasks => &[("Name", 3), ("Status", 2), ("Exploration", 1)],
        View::Assets => &[("Label", 3), ("Kind", 2), ("Company", 1)],
        View::Findings => &[("Name", 3), ("Severity", 1), ("Status", 1), ("Task", 1)],
        View::Sessions => &[("Title", 3), ("Agent", 2)],
        View::Approvals => &[("Status", 2), ("Task reference", 3)],
        View::TrafficEvidence => &[
            ("Finding", 1),
            ("Method", 1),
            ("HTTP", 1),
            ("Request bytes", 2),
            ("Response bytes", 2),
        ],
        View::Exploration { .. } => &[("Kind", 3), ("State", 2), ("Priority", 1)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arustex_core::{Page, Reply, Rows, TaskSummary};
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn renders_a_task_and_the_detail_action() {
        let mut browser = Browser::default();
        browser.complete(Ok(Reply::Page(Page {
            rows: Rows::Tasks(vec![TaskSummary {
                id: i64::MAX,
                name: "Synthetic case".into(),
                status: "ready".into(),
                exploration_id: 52,
                archived: false,
            }]),
            next_before: None,
        })));
        browser.details = true;
        let mut terminal = Terminal::new(TestBackend::new(105, 16)).unwrap();
        terminal.draw(|frame| render(frame, &browser)).unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..buffer.area.height)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Synthetic case"));
        assert!(text.contains("Exploration nodes"));
        assert!(text.contains("Record"));
        assert_eq!(text.matches("9223372036854775807").count(), 2);
    }
}
