//! Rendering. Plain borders and colours only, so it works over SSH, in foot and in
//! zellij; with `NO_COLOR` set everything is monochrome.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Tabs, Wrap,
};

use crate::app::{App, List, Overlay, ReviewsTab, View};
use crate::text::{clean, short};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub color: bool,
}

impl Theme {
    pub fn from_env() -> Self {
        Self {
            color: std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        }
    }

    fn fg(self, c: Color) -> Style {
        if self.color {
            Style::default().fg(c)
        } else {
            Style::default()
        }
    }

    fn selected(self) -> Style {
        if self.color {
            Style::default().bg(Color::Blue).fg(Color::White)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    fn header(self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
}

pub fn draw(f: &mut Frame, app: &App, theme: Theme) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
    draw_tabs(f, app, theme, chunks[0]);
    match app.view {
        View::Dashboard => draw_dashboard(f, app, theme, chunks[1]),
        View::Tasks => draw_tasks(f, app, theme, chunks[1]),
        View::Sessions => draw_sessions(f, app, theme, chunks[1]),
        View::Approvals => draw_approvals(f, app, theme, chunks[1]),
        View::Assets => draw_assets(f, app, theme, chunks[1]),
        View::Records => draw_records(f, app, theme, chunks[1]),
        View::Reviews => draw_reviews(f, app, theme, chunks[1]),
        View::Reports => draw_reports(f, app, theme, chunks[1]),
        View::Settings => draw_settings(f, app, theme, chunks[1]),
    }
    draw_status(f, app, theme, chunks[2]);
    draw_overlay(f, app, theme, area);
}

fn draw_tabs(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let titles: Vec<Line> = View::ALL
        .iter()
        .enumerate()
        .map(|(i, v)| Line::from(format!("{} {}", i + 1, v.tab())))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.view.index())
        .highlight_style(theme.selected())
        .divider("│");
    f.render_widget(tabs, area);
}

fn draw_status(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let conn = if app.connected {
        Span::styled(" ● connected ", theme.fg(Color::Green))
    } else {
        Span::styled(" ○ offline ", theme.fg(Color::Red))
    };
    let msg_style = if app.status_error {
        theme.fg(Color::Red)
    } else {
        Style::default()
    };
    let line = Line::from(vec![
        conn,
        Span::raw("│ "),
        Span::styled(clean(&app.status), msg_style),
        Span::raw("   ? help · q quit"),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn block(title: impl Into<String>) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", title.into()))
}

fn page_title<T>(name: &str, list: &List<T>) -> String {
    if list.total == 0 {
        format!("{name} (none)")
    } else {
        let first = u64::from(list.offset) + 1;
        let last = u64::from(list.offset) + list.items.len() as u64;
        format!("{name} {first}-{last} of {}", list.total)
    }
}

#[allow(clippy::too_many_arguments)]
fn table<T>(
    f: &mut Frame,
    theme: Theme,
    area: Rect,
    title: String,
    list: &List<T>,
    header: &[&'static str],
    widths: &[Constraint],
    row: impl Fn(&T) -> Vec<String>,
) {
    let rows = list
        .items
        .iter()
        .map(|item| Row::new(row(item).into_iter().map(Cell::from).collect::<Vec<_>>()));
    let t = Table::new(rows, widths.to_vec())
        .header(
            Row::new(header.iter().map(|h| Cell::from(*h)).collect::<Vec<_>>())
                .style(theme.header()),
        )
        .block(block(title))
        .row_highlight_style(theme.selected());
    let mut state = TableState::default();
    if !list.items.is_empty() {
        state.select(Some(list.selected));
    }
    f.render_stateful_widget(t, area, &mut state);
}

fn draw_dashboard(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let Some(d) = &app.dashboard else {
        f.render_widget(
            Paragraph::new("No data yet.").block(block("Dashboard")),
            area,
        );
        return;
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(area);
    let mut lines = vec![
        Line::from(format!(
            "Daemon        {} (since {})",
            clean(&d.daemon_version),
            d.started_at.format("%Y-%m-%d %H:%M UTC")
        )),
        Line::from(format!("Datasets      {}", d.datasets)),
        Line::from(format!("Records       {}", d.records)),
        Line::from(format!("Sessions      {} active", d.active_sessions)),
        Line::from(Span::styled(
            format!("Approvals     {} pending", d.pending_approvals),
            if d.pending_approvals > 0 {
                theme.fg(Color::Yellow)
            } else {
                Style::default()
            },
        )),
        Line::from(format!(
            "Findings      {} pending review",
            d.pending_findings
        )),
        Line::from(format!("Tokens today  {}", d.tokens_today)),
        Line::from(""),
        Line::from(Span::styled("Tasks by status", theme.header())),
    ];
    if d.tasks_by_status.is_empty() {
        lines.push(Line::from("  none"));
    }
    for (status, n) in &d.tasks_by_status {
        lines.push(Line::from(format!("  {:<14}{n}", clean(status))));
    }
    f.render_widget(Paragraph::new(lines).block(block("Overview")), cols[0]);
    let events: Vec<Line> = d
        .recent_events
        .iter()
        .rev()
        .map(|e| {
            Line::from(format!(
                "{:>6} {} {}",
                e.seq,
                e.at.format("%H:%M:%S"),
                e.event.kind()
            ))
        })
        .collect();
    f.render_widget(
        Paragraph::new(events).block(block("Recent events")),
        cols[1],
    );
}

fn draw_tasks(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);
    table(
        f,
        theme,
        cols[0],
        page_title("Tasks", &app.tasks),
        &app.tasks,
        &["#", "Name", "Status", "Tokens"],
        &[
            Constraint::Length(6),
            Constraint::Min(10),
            Constraint::Length(13),
            Constraint::Length(9),
        ],
        |t| {
            vec![
                t.id.to_string(),
                short(t.display_name(), 60),
                t.status.to_string(),
                t.tokens_used.to_string(),
            ]
        },
    );
    let lines = match &app.task_detail {
        Some(d) if app.tasks.current().is_some_and(|t| t.id == d.task.id) => {
            let t = &d.task;
            let mut l = vec![
                Line::from(Span::styled(short(t.display_name(), 80), theme.header())),
                Line::from(format!("Status     {}", t.status)),
                Line::from(format!(
                    "Datasets   {}",
                    t.dataset_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
                Line::from(format!(
                    "Coverage   {}/{} records reviewed",
                    d.coverage.reviewed, d.coverage.total
                )),
                Line::from(format!(
                    "Queue      {} queued · {} running · {} done · {} failed",
                    d.queue.queued, d.queue.leased, d.queue.done, d.queue.failed
                )),
                Line::from(format!("Findings   {}", d.findings)),
                Line::from(format!(
                    "Tokens     {}{}",
                    t.tokens_used,
                    if t.token_budget > 0 {
                        format!(" / {}", t.token_budget)
                    } else {
                        String::new()
                    }
                )),
                Line::from(""),
                Line::from(Span::styled("Objective", theme.header())),
            ];
            l.extend(crate::text::lines(&t.objective).into_iter().map(Line::from));
            l.push(Line::from(""));
            l.push(Line::from(Span::styled(
                "s start · p pause · r resume · c cancel · a accept · g report · m ask · n new",
                theme.fg(Color::DarkGray),
            )));
            l
        }
        _ => vec![Line::from("Select a task; n creates one.")],
    };
    f.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block("Detail")),
        cols[1],
    );
}

fn draw_sessions(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    table(
        f,
        theme,
        area,
        format!(
            "{} · Enter: transcript",
            page_title("Sessions", &app.sessions)
        ),
        &app.sessions,
        &[
            "Started", "Task", "Agent", "Status", "Turn", "Tokens", "Model",
        ],
        &[
            Constraint::Length(9),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(18),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(8),
        ],
        |s| {
            vec![
                s.created_at.format("%H:%M:%S").to_string(),
                s.task_id.map(|t| t.to_string()).unwrap_or_default(),
                s.role.to_string(),
                s.status.to_string(),
                format!("{}/{}", s.turn, s.max_turns),
                s.tokens_used.to_string(),
                short(&s.model, 30),
            ]
        },
    );
}

fn draw_approvals(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let filter = if app.approvals_pending_only {
        "pending"
    } else {
        "all"
    };
    table(
        f,
        theme,
        area,
        format!(
            "{} [{filter}] · y approve · x reject · f filter · Enter detail",
            page_title("Approvals", &app.approvals)
        ),
        &app.approvals,
        &["#", "Task", "Operation", "Status", "Summary"],
        &[
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(24),
            Constraint::Length(10),
            Constraint::Min(10),
        ],
        |a| {
            vec![
                a.id.to_string(),
                a.task_id.map(|t| t.to_string()).unwrap_or_default(),
                short(&a.operation, 24),
                a.status.to_string(),
                short(&a.summary, 120),
            ]
        },
    );
}

fn draw_assets(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let q = if app.asset_query.is_empty() {
        String::new()
    } else {
        format!(" [{}]", short(&app.asset_query, 30))
    };
    table(
        f,
        theme,
        area,
        format!(
            "{}{q} · / search · Enter detail",
            page_title("Assets", &app.assets)
        ),
        &app.assets,
        &["#", "Kind", "Label", "Origin"],
        &[
            Constraint::Length(7),
            Constraint::Length(12),
            Constraint::Min(10),
            Constraint::Length(16),
        ],
        |a| {
            vec![
                a.id.to_string(),
                a.kind.to_string(),
                short(&a.label, 100),
                a.provenance.origin.to_string(),
            ]
        },
    );
}

fn draw_records(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);
    let ds = app
        .datasets
        .current()
        .map(|d| {
            format!(
                "#{} {} · {} records · source {}",
                d.id,
                short(&d.name, 40),
                d.record_count,
                short(&d.source, 30)
            )
        })
        .unwrap_or_else(|| "no datasets; import one with `arustex import records`".into());
    f.render_widget(
        Paragraph::new(ds).block(block(format!(
            "Dataset {}/{} · d next",
            app.datasets.selected + usize::from(!app.datasets.items.is_empty()),
            app.datasets.items.len()
        ))),
        rows[0],
    );
    let q = if app.record_query.is_empty() {
        String::new()
    } else {
        format!(" [{}]", short(&app.record_query, 30))
    };
    table(
        f,
        theme,
        rows[1],
        format!(
            "{}{q} · / search · Enter view",
            page_title("Records", &app.records)
        ),
        &app.records,
        &["#", "External id", "Title", "Type", "Bytes"],
        &[
            Constraint::Length(8),
            Constraint::Length(20),
            Constraint::Min(10),
            Constraint::Length(16),
            Constraint::Length(9),
        ],
        |r| {
            vec![
                r.id.to_string(),
                short(&r.external_id, 20),
                short(&r.title, 100),
                short(&r.content_type, 16),
                r.body_bytes.to_string(),
            ]
        },
    );
}

fn draw_reviews(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    match app.reviews_tab {
        ReviewsTab::Findings => table(
            f,
            theme,
            area,
            format!(
                "{} · t verdicts · y confirm · x dismiss · v resolved · u duplicate · o reopen",
                page_title("Findings", &app.findings)
            ),
            &app.findings,
            &["#", "Task", "Severity", "Status", "Origin", "Title"],
            &[
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Length(9),
                Constraint::Length(10),
                Constraint::Length(16),
                Constraint::Min(10),
            ],
            |x| {
                vec![
                    x.id.to_string(),
                    x.task_id.map(|t| t.to_string()).unwrap_or_default(),
                    x.severity.to_string(),
                    x.status.to_string(),
                    x.provenance.origin.to_string(),
                    short(&x.title, 120),
                ]
            },
        ),
        ReviewsTab::Verdicts => table(
            f,
            theme,
            area,
            format!(
                "{} · t findings · y confirm · x reject",
                page_title("Reviewer verdicts", &app.verdicts)
            ),
            &app.verdicts,
            &["#", "Task", "Verdict", "Status", "Created"],
            &[
                Constraint::Length(6),
                Constraint::Length(6),
                Constraint::Length(13),
                Constraint::Length(22),
                Constraint::Min(10),
            ],
            |r| {
                vec![
                    r.id.to_string(),
                    r.task_id.to_string(),
                    r.verdict.to_string(),
                    r.status.to_string(),
                    r.created_at.format("%Y-%m-%d %H:%M").to_string(),
                ]
            },
        ),
    }
}

fn draw_reports(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    table(
        f,
        theme,
        area,
        format!("{} · Enter view", page_title("Reports", &app.reports)),
        &app.reports,
        &["#", "Task", "Status", "Origin", "Created", "Title"],
        &[
            Constraint::Length(6),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(17),
            Constraint::Min(10),
        ],
        |r| {
            vec![
                r.id.to_string(),
                r.task_id.to_string(),
                r.status.to_string(),
                r.origin.to_string(),
                r.created_at.format("%Y-%m-%d %H:%M").to_string(),
                short(&r.title, 100),
            ]
        },
    );
}

fn draw_settings(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    let rows: Vec<Row> = app
        .settings
        .as_ref()
        .map(|s| {
            s.entries
                .iter()
                .map(|(k, v)| Row::new(vec![Cell::from(clean(k)), Cell::from(short(v, 200))]))
                .collect()
        })
        .unwrap_or_default();
    let t = Table::new(rows, [Constraint::Length(40), Constraint::Min(10)])
        .header(Row::new(vec!["Key", "Value"]).style(theme.header()))
        .block(block(
            "Effective settings (secrets redacted; edit the config file and restart)",
        ));
    f.render_widget(t, area);
}

fn centered(area: Rect, w_pct: u16, h_pct: u16) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - h_pct) / 2),
            Constraint::Percentage(h_pct),
            Constraint::Percentage((100 - h_pct) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - w_pct) / 2),
            Constraint::Percentage(w_pct),
            Constraint::Percentage((100 - w_pct) / 2),
        ])
        .split(v[1])[1]
}

const HELP: &[&str] = &[
    "1-9 / Tab / Shift-Tab   switch view",
    "j k / arrows            move",
    "[ ] / PgUp PgDn         previous / next page",
    "Enter                   open details",
    "R / F5                  refresh",
    "Esc                     close a popup",
    "q / Ctrl-C              quit",
    "",
    "Tasks      s start · p pause · r resume · c cancel · a accept",
    "           g generate report · m ask the assistant · n new task",
    "Approvals  y approve · x reject · f pending/all",
    "Assets     / search",
    "Records    / search · d next dataset",
    "Reviews    t switch findings/verdicts · y confirm · x dismiss/reject",
    "           v resolved · u duplicate · o reopen",
    "",
    "Imported records are shown as text; control characters appear as �.",
];

fn draw_overlay(f: &mut Frame, app: &App, theme: Theme, area: Rect) {
    match &app.overlay {
        Overlay::None => {}
        Overlay::Help => {
            let r = centered(area, 80, 80);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(HELP.iter().map(|l| Line::from(*l)).collect::<Vec<_>>())
                    .block(block("Help · any key closes")),
                r,
            );
        }
        Overlay::Detail {
            title,
            lines,
            scroll,
            ..
        } => {
            let r = centered(area, 90, 90);
            f.render_widget(Clear, r);
            let text: Vec<Line> = lines.iter().map(|l| Line::from(l.as_str())).collect();
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((*scroll, 0))
                    .block(block(format!("{} · j/k scroll · Esc close", clean(title)))),
                r,
            );
        }
        Overlay::Confirm { prompt, .. } => {
            let r = centered(area, 60, 20);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(clean(prompt))
                    .wrap(Wrap { trim: false })
                    .style(theme.fg(Color::Yellow))
                    .block(block("Confirm")),
                r,
            );
        }
        Overlay::Input { prompt, buffer, .. } => {
            let r = centered(area, 70, 20);
            f.render_widget(Clear, r);
            let line = Line::from(vec![
                Span::raw(clean(buffer)),
                Span::styled("▏", theme.fg(Color::Cyan)),
            ]);
            f.render_widget(
                Paragraph::new(vec![Line::from(clean(prompt)), line])
                    .wrap(Wrap { trim: false })
                    .block(block("Enter submit · Esc cancel")),
                r,
            );
        }
    }
}
