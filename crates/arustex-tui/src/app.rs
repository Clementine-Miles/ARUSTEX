use std::collections::VecDeque;

use arustex_core::{MAX_PAGE_SIZE, Query, Reply, Rows, View};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub(super) const TABS: [(View, &str); 6] = [
    (View::Tasks, "Tasks"),
    (View::Assets, "Assets"),
    (View::Findings, "Findings"),
    (View::Sessions, "Sessions"),
    (View::Approvals, "Approvals"),
    (View::TrafficEvidence, "Traffic evidence"),
];

const PAGE_SIZE: u16 = if MAX_PAGE_SIZE < 50 {
    MAX_PAGE_SIZE
} else {
    50
};
const MAX_PAGE_HISTORY: usize = 256;
const MAX_FILTER_CHARS: usize = 128;
const MAX_FIELD_CHARS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    None,
    Load,
    Quit,
}

#[derive(Default)]
struct Cursor {
    before: Option<i64>,
    previous: VecDeque<Option<i64>>,
    next: Option<i64>,
}

impl Cursor {
    fn advance(&mut self, next: i64) {
        if self.previous.len() == MAX_PAGE_HISTORY {
            self.previous.pop_front();
        }
        self.previous.push_back(self.before);
        self.before = Some(next);
    }
}

struct TaskPosition {
    cursor: Cursor,
    selected: usize,
    filter: String,
    details: bool,
}

pub(super) struct Record {
    pub(super) id: i64,
    pub(super) fields: Vec<String>,
}

impl Record {
    fn new<const N: usize>(id: i64, fields: [String; N]) -> Self {
        Self {
            id,
            fields: fields
                .into_iter()
                .map(|value| terminal_text(&value))
                .collect(),
        }
    }

    fn matches(&self, term: &str) -> bool {
        self.id.to_string().contains(term)
            || self
                .fields
                .iter()
                .any(|field| field.to_lowercase().contains(term))
    }
}

pub(super) struct Browser {
    pub(super) view: View,
    pub(super) selected: usize,
    pub(super) filter: String,
    pub(super) details: bool,
    pub(super) loading: bool,
    pub(super) failure: Option<String>,
    filter_checkpoint: Option<String>,
    cursor: Cursor,
    records: Vec<Record>,
    return_to_task: Option<TaskPosition>,
}

impl Default for Browser {
    fn default() -> Self {
        Self {
            view: View::Tasks,
            selected: 0,
            filter: String::new(),
            details: false,
            loading: false,
            failure: None,
            filter_checkpoint: None,
            cursor: Cursor::default(),
            records: Vec::new(),
            return_to_task: None,
        }
    }
}

impl Browser {
    pub(super) fn load(&mut self) -> Query {
        self.loading = true;
        self.cursor.next = None;
        self.failure = None;
        self.records.clear();
        Query::Browse {
            view: self.view,
            before_id: self.cursor.before,
            limit: PAGE_SIZE,
        }
    }

    pub(super) fn complete(&mut self, reply: Result<Reply, String>) {
        self.loading = false;
        match reply {
            Ok(Reply::Page(page)) => {
                self.records = match records(self.view, page.rows) {
                    Some(records) => records,
                    None => {
                        self.failure =
                            Some("The daemon returned records for a different view.".into());
                        return;
                    }
                };
                self.cursor.next = page.next_before.filter(|&next| {
                    next > 0 && self.cursor.before.is_none_or(|before| next < before)
                });
                self.clamp_selection();
            }
            Ok(Reply::Error { code }) => self.failure = Some(terminal_text(&code.to_string())),
            Ok(Reply::Pong) => {
                self.failure = Some("The daemon returned an unexpected reply.".into())
            }
            Err(error) => self.failure = Some(terminal_text(&error)),
        }
    }

    pub(super) fn key(&mut self, key: KeyEvent) -> Action {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if self.filter_checkpoint.is_some() {
            self.edit_filter(key);
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') => return Action::Quit,
            KeyCode::Char('r') => return Action::Load,
            KeyCode::Tab | KeyCode::BackTab => return self.change_tab(key),
            KeyCode::Char('n') | KeyCode::PageDown if !self.loading => {
                if let Some(next) = self.cursor.next {
                    self.cursor.advance(next);
                    self.selected = 0;
                    return Action::Load;
                }
            }
            KeyCode::Char('p') | KeyCode::PageUp if !self.loading => {
                if let Some(before) = self.cursor.previous.pop_back() {
                    self.cursor.before = before;
                    self.selected = 0;
                    return Action::Load;
                }
            }
            KeyCode::Char('/') => self.filter_checkpoint = Some(self.filter.clone()),
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = self.selected.saturating_add(1);
                self.clamp_selection();
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.visible_records().len().saturating_sub(1),
            KeyCode::Char('d') => self.details = !self.details,
            KeyCode::Enter if self.view == View::Tasks => return self.open_exploration(),
            KeyCode::Esc => return self.back(),
            _ => {}
        }
        Action::None
    }

    fn change_tab(&mut self, key: KeyEvent) -> Action {
        let index = self.tab_index();
        let backward = key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
        let step = if backward { TABS.len() - 1 } else { 1 };
        *self = Self {
            view: TABS[(index + step) % TABS.len()].0,
            ..Self::default()
        };
        Action::Load
    }

    fn edit_filter(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => {
                self.filter_checkpoint = None;
            }
            KeyCode::Esc => {
                self.filter = self.filter_checkpoint.take().unwrap_or_default();
            }
            KeyCode::Backspace => {
                self.filter.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && printable(ch)
                    && self.filter.chars().count() < MAX_FILTER_CHARS =>
            {
                self.filter.push(ch);
            }
            _ => return,
        }
        self.selected = 0;
    }

    fn open_exploration(&mut self) -> Action {
        let Some(task_id) = self
            .visible_records()
            .get(self.selected)
            .map(|record| record.id)
        else {
            return Action::None;
        };
        self.return_to_task = Some(TaskPosition {
            cursor: std::mem::take(&mut self.cursor),
            selected: self.selected,
            filter: std::mem::take(&mut self.filter),
            details: self.details,
        });
        self.view = View::Exploration { task_id };
        self.selected = 0;
        self.details = false;
        Action::Load
    }

    fn back(&mut self) -> Action {
        if self.details {
            self.details = false;
        } else if let Some(task) = self.return_to_task.take() {
            self.view = View::Tasks;
            self.cursor = task.cursor;
            self.selected = task.selected;
            self.filter = task.filter;
            self.details = task.details;
            return Action::Load;
        } else {
            self.filter.clear();
            self.selected = 0;
        }
        Action::None
    }

    fn clamp_selection(&mut self) {
        self.selected = self
            .selected
            .min(self.visible_records().len().saturating_sub(1));
    }

    pub(super) fn tab_index(&self) -> usize {
        TABS.iter()
            .position(|(view, _)| *view == self.view)
            .unwrap_or(0)
    }

    pub(super) fn editing_filter(&self) -> bool {
        self.filter_checkpoint.is_some()
    }

    pub(super) fn visible_records(&self) -> Vec<&Record> {
        let term = self.filter.trim().to_lowercase();
        self.records
            .iter()
            .filter(|record| term.is_empty() || record.matches(&term))
            .collect()
    }

    pub(super) fn record_count(&self) -> usize {
        self.records.len()
    }

    pub(super) fn page_directions(&self) -> (bool, bool) {
        (!self.cursor.previous.is_empty(), self.cursor.next.is_some())
    }
}

fn records(view: View, rows: Rows) -> Option<Vec<Record>> {
    let limit = usize::from(PAGE_SIZE);
    let result = match (view, rows) {
        (View::Tasks, Rows::Tasks(rows)) => rows
            .into_iter()
            .take(limit)
            .map(|row| {
                let status = if row.archived {
                    format!("{} (archived)", row.status)
                } else {
                    row.status
                };
                Record::new(row.id, [row.name, status, row.exploration_id.to_string()])
            })
            .collect(),
        (View::Assets, Rows::Assets(rows)) => rows
            .into_iter()
            .take(limit)
            .map(|row| {
                Record::new(
                    row.id,
                    [present(row.label), row.kind, number(row.company_id)],
                )
            })
            .collect(),
        (View::Findings, Rows::Findings(rows)) => rows
            .into_iter()
            .take(limit)
            .map(|row| {
                Record::new(
                    row.id,
                    [row.name, row.severity, row.status, number(row.task_id)],
                )
            })
            .collect(),
        (View::Sessions, Rows::Sessions(rows)) => rows
            .into_iter()
            .take(limit)
            .map(|row| Record::new(row.id, [row.title, row.agent_key]))
            .collect(),
        (View::Approvals, Rows::Approvals(rows)) => rows
            .into_iter()
            .take(limit)
            .map(|row| Record::new(row.id, [row.status, present(row.task_ref)]))
            .collect(),
        (View::TrafficEvidence, Rows::TrafficEvidence(rows)) => rows
            .into_iter()
            .take(limit)
            .map(|row| {
                Record::new(
                    row.id,
                    [
                        row.finding_id.to_string(),
                        row.method,
                        row.status.to_string(),
                        row.request_bytes.to_string(),
                        row.response_bytes.to_string(),
                    ],
                )
            })
            .collect(),
        (View::Exploration { .. }, Rows::Exploration(rows)) => rows
            .into_iter()
            .take(limit)
            .map(|row| Record::new(row.id, [row.kind, row.state, row.priority.to_string()]))
            .collect(),
        _ => return None,
    };
    Some(result)
}

fn present(value: Option<String>) -> String {
    value.unwrap_or_else(|| "—".into())
}

fn number(value: Option<i64>) -> String {
    present(value.map(|value| value.to_string()))
}

fn printable(ch: char) -> bool {
    !ch.is_control()
        && !matches!(
            ch,
            '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .take(MAX_FIELD_CHARS)
        .map(|ch| if printable(ch) { ch } else { ' ' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arustex_core::{Page, TaskSummary};

    fn task(id: i64, name: &str) -> TaskSummary {
        TaskSummary {
            id,
            name: name.into(),
            status: "ready".into(),
            exploration_id: id + 100,
            archived: false,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn task_page(rows: Vec<TaskSummary>, next_before: Option<i64>) -> Result<Reply, String> {
        Ok(Reply::Page(Page {
            rows: Rows::Tasks(rows),
            next_before,
        }))
    }

    #[test]
    fn filtering_and_exploration_preserve_task_position() {
        let mut browser = Browser::default();
        browser.complete(task_page(
            vec![task(12, "Review A"), task(11, "Review B")],
            Some(11),
        ));
        assert_eq!(browser.key(key(KeyCode::Char('n'))), Action::Load);
        assert!(matches!(
            browser.load(),
            Query::Browse {
                before_id: Some(11),
                ..
            }
        ));
        browser.complete(task_page(
            vec![task(10, "Asset Review"), task(9, "Evidence Review")],
            None,
        ));

        browser.key(key(KeyCode::Char('/')));
        for ch in "evidence".chars() {
            browser.key(key(KeyCode::Char(ch)));
        }
        browser.key(key(KeyCode::Enter));
        assert_eq!(browser.visible_records().len(), 1);
        assert_eq!(browser.key(key(KeyCode::Enter)), Action::Load);
        assert_eq!(browser.view, View::Exploration { task_id: 9 });
        assert_eq!(browser.key(key(KeyCode::Esc)), Action::Load);
        assert_eq!(browser.filter, "evidence");
        assert!(matches!(
            browser.load(),
            Query::Browse {
                view: View::Tasks,
                before_id: Some(11),
                ..
            }
        ));
    }

    #[test]
    fn terminal_text_removes_database_escape_and_bidi_sequences() {
        let mut browser = Browser::default();
        browser.complete(task_page(
            vec![task(1, "\x1b[2J\rforged\u{202e}\nline")],
            None,
        ));
        assert_eq!(browser.visible_records()[0].fields[0], " [2J forged  line");
    }

    #[test]
    fn unexpected_page_is_reported_and_can_be_refreshed() {
        let mut browser = Browser::default();
        browser.load();
        browser.complete(Ok(Reply::Page(Page {
            rows: Rows::Assets(Vec::new()),
            next_before: None,
        })));
        assert!(browser.failure.is_some());
        assert_eq!(browser.key(key(KeyCode::Char('r'))), Action::Load);
        browser.load();
        browser.complete(task_page(vec![task(1, "Review")], None));
        assert!(browser.failure.is_none());
        assert_eq!(browser.record_count(), 1);
    }
}
