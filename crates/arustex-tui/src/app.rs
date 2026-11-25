//! TUI state and input handling. Pure: no terminal or socket access, so the whole
//! interaction model is unit-testable. Key presses and daemon messages update the
//! state and return the requests the runtime should send.

use std::collections::BTreeSet;

use arustex_core::domain::agent::{Session, SessionId, TranscriptEntry};
use arustex_core::domain::asset::Asset;
use arustex_core::domain::finding::{Finding, FindingStatus, ReviewResult};
use arustex_core::domain::record::Dataset;
use arustex_core::domain::report::ReportSummary;
use arustex_core::domain::task::{NewTask, Task, TaskAction};
use arustex_core::domain::work::{Approval, ApprovalStatus};
use arustex_core::domain::{DatasetId, TaskId};
use arustex_core::events::{DomainEvent, EventEnvelope};
use arustex_core::ipc::{DashboardSnapshot, Request, Response, SettingsView, TaskDetail};
use arustex_core::ports::RecordSummary;
use arustex_core::{Page, Paged};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::text;

pub const PAGE_SIZE: u32 = 50;
/// Live output kept for the open session, in characters.
const MAX_LIVE_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum View {
    Dashboard,
    Tasks,
    Sessions,
    Approvals,
    Assets,
    Records,
    Reviews,
    Reports,
    Settings,
}

impl View {
    pub const ALL: [View; 9] = [
        View::Dashboard,
        View::Tasks,
        View::Sessions,
        View::Approvals,
        View::Assets,
        View::Records,
        View::Reviews,
        View::Reports,
        View::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            View::Dashboard => "Dashboard",
            View::Tasks => "Tasks",
            View::Sessions => "Sessions",
            View::Approvals => "Approvals",
            View::Assets => "Assets",
            View::Records => "Records",
            View::Reviews => "Review Results",
            View::Reports => "Reports",
            View::Settings => "Settings",
        }
    }

    /// Compact label for the tab bar.
    pub fn tab(self) -> &'static str {
        match self {
            View::Dashboard => "Home",
            View::Reviews => "Reviews",
            other => other.title(),
        }
    }

    pub fn index(self) -> usize {
        View::ALL.iter().position(|v| *v == self).unwrap_or(0)
    }
}

/// One page of a list plus the cursor.
#[derive(Debug, Clone)]
pub struct List<T> {
    pub items: Vec<T>,
    pub total: u64,
    pub offset: u32,
    pub selected: usize,
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            total: 0,
            offset: 0,
            selected: 0,
        }
    }
}

impl<T> List<T> {
    pub fn set(&mut self, page: Paged<T>) {
        self.items = page.items;
        self.total = page.total;
        self.offset = page.page.offset;
        self.selected = self.selected.min(self.items.len().saturating_sub(1));
    }

    pub fn current(&self) -> Option<&T> {
        self.items.get(self.selected)
    }

    pub fn page(&self) -> Page {
        Page::new(self.offset, PAGE_SIZE)
    }

    fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn down(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// Moves to the next page if there is one; returns whether it moved.
    fn next_page(&mut self) -> bool {
        let next = self.offset + PAGE_SIZE;
        if u64::from(next) < self.total {
            self.offset = next;
            self.selected = 0;
            true
        } else {
            false
        }
    }

    fn prev_page(&mut self) -> bool {
        if self.offset == 0 {
            return false;
        }
        self.offset = self.offset.saturating_sub(PAGE_SIZE);
        self.selected = 0;
        true
    }

    fn replace(&mut self, item: T, same: impl Fn(&T) -> bool) {
        if let Some(slot) = self.items.iter_mut().find(|i| same(i)) {
            *slot = item;
        }
    }
}

/// Which list the Review Results view shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewsTab {
    Findings,
    Verdicts,
}

/// What a text prompt is collecting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputPurpose {
    AssetSearch,
    RecordSearch,
    Assistant(TaskId),
    NewTaskName,
    NewTaskObjective { name: String },
    NewTaskDatasets { name: String, objective: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    None,
    Help,
    /// Scrollable read-only text.
    Detail {
        title: String,
        lines: Vec<String>,
        scroll: u16,
        /// When set, live output and new transcript entries of this session append.
        session: Option<SessionId>,
    },
    Confirm {
        prompt: String,
        request: Box<Request>,
    },
    Input {
        prompt: String,
        buffer: String,
        purpose: InputPurpose,
    },
}

#[derive(Debug)]
pub struct App {
    pub view: View,
    pub connected: bool,
    pub server: String,
    pub last_seq: i64,
    pub status: String,
    pub status_error: bool,
    pub overlay: Overlay,
    pub quit: bool,
    /// Views whose data changed on the daemon and should be refetched.
    pub dirty: BTreeSet<View>,

    pub dashboard: Option<DashboardSnapshot>,
    pub tasks: List<Task>,
    pub task_detail: Option<TaskDetail>,
    pub sessions: List<Session>,
    pub approvals: List<Approval>,
    pub approvals_pending_only: bool,
    pub assets: List<Asset>,
    pub asset_query: String,
    pub datasets: List<Dataset>,
    pub records: List<RecordSummary>,
    pub record_query: String,
    pub reviews_tab: ReviewsTab,
    pub findings: List<Finding>,
    pub verdicts: List<ReviewResult>,
    pub reports: List<ReportSummary>,
    pub settings: Option<SettingsView>,
    /// Transcript seq already shown for the session open in the detail overlay.
    transcript_seq: i64,
    live_output: String,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            view: View::Dashboard,
            connected: false,
            server: String::new(),
            last_seq: 0,
            status: "connecting…".into(),
            status_error: false,
            overlay: Overlay::None,
            quit: false,
            dirty: BTreeSet::new(),
            dashboard: None,
            tasks: List::default(),
            task_detail: None,
            sessions: List::default(),
            approvals: List::default(),
            approvals_pending_only: true,
            assets: List::default(),
            asset_query: String::new(),
            datasets: List::default(),
            records: List::default(),
            record_query: String::new(),
            reviews_tab: ReviewsTab::Findings,
            findings: List::default(),
            verdicts: List::default(),
            reports: List::default(),
            settings: None,
            transcript_seq: 0,
            live_output: String::new(),
        }
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.status = text::short(&msg.into(), 200);
        self.status_error = false;
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.status = text::short(&msg.into(), 200);
        self.status_error = true;
    }

    fn selected_dataset(&self) -> Option<DatasetId> {
        self.datasets.current().map(|d| d.id)
    }

    /// Requests that load the data of `view`.
    pub fn refresh(&self, view: View) -> Vec<Request> {
        match view {
            View::Dashboard => vec![Request::Dashboard],
            View::Tasks => {
                let mut r = vec![Request::ListTasks {
                    page: self.tasks.page(),
                }];
                if let Some(t) = self.tasks.current() {
                    r.push(Request::GetTask { id: t.id });
                }
                r
            }
            View::Sessions => vec![Request::ListSessions {
                task_id: None,
                page: self.sessions.page(),
            }],
            View::Approvals => vec![Request::ListApprovals {
                status: self
                    .approvals_pending_only
                    .then_some(ApprovalStatus::Pending),
                page: self.approvals.page(),
            }],
            View::Assets => vec![Request::ListAssets {
                query: (!self.asset_query.is_empty()).then(|| self.asset_query.clone()),
                page: self.assets.page(),
            }],
            View::Records => vec![
                Request::ListDatasets {
                    page: self.datasets.page(),
                },
                Request::ListRecords {
                    dataset_id: self.selected_dataset(),
                    query: (!self.record_query.is_empty()).then(|| self.record_query.clone()),
                    page: self.records.page(),
                },
            ],
            View::Reviews => match self.reviews_tab {
                ReviewsTab::Findings => vec![Request::ListFindings {
                    task_id: None,
                    page: self.findings.page(),
                }],
                ReviewsTab::Verdicts => vec![Request::ListReviewResults {
                    task_id: None,
                    page: self.verdicts.page(),
                }],
            },
            View::Reports => vec![Request::ListReports {
                task_id: None,
                page: self.reports.page(),
            }],
            View::Settings => vec![Request::GetSettings],
        }
    }

    /// Requests for the current view plus the dashboard header counts.
    pub fn refresh_current(&self) -> Vec<Request> {
        let mut r = self.refresh(self.view);
        if self.view != View::Dashboard {
            r.push(Request::Dashboard);
        }
        r
    }

    /// Requests for dirty views that are visible; clears their flags.
    pub fn take_dirty(&mut self) -> Vec<Request> {
        let mut out = Vec::new();
        let dirty = std::mem::take(&mut self.dirty);
        for view in dirty {
            if view == self.view || view == View::Dashboard {
                out.extend(self.refresh(view));
            } else {
                self.dirty.insert(view);
            }
        }
        out
    }

    fn switch(&mut self, view: View) -> Vec<Request> {
        self.view = view;
        self.dirty.remove(&view);
        self.refresh(view)
    }

    /// Handles one key press.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Request> {
        if key.kind == KeyEventKind::Release {
            return Vec::new();
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return Vec::new();
        }
        match &mut self.overlay {
            Overlay::None => {}
            Overlay::Help => {
                self.overlay = Overlay::None;
                return Vec::new();
            }
            Overlay::Detail { scroll, lines, .. } => {
                let max = u16::try_from(lines.len().saturating_sub(1)).unwrap_or(u16::MAX);
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                        self.overlay = Overlay::None;
                        self.live_output.clear();
                    }
                    KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(max),
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown | KeyCode::Char(' ') => *scroll = (*scroll + 20).min(max),
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(20),
                    KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => *scroll = max,
                    _ => {}
                }
                return Vec::new();
            }
            Overlay::Confirm { .. } => {
                let Overlay::Confirm { request, .. } =
                    std::mem::replace(&mut self.overlay, Overlay::None)
                else {
                    unreachable!()
                };
                return match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => vec![*request],
                    _ => {
                        self.info("cancelled");
                        Vec::new()
                    }
                };
            }
            Overlay::Input { buffer, .. } => {
                match key.code {
                    KeyCode::Esc => {
                        self.overlay = Overlay::None;
                        self.info("cancelled");
                    }
                    KeyCode::Enter => {
                        let Overlay::Input {
                            buffer, purpose, ..
                        } = std::mem::replace(&mut self.overlay, Overlay::None)
                        else {
                            unreachable!()
                        };
                        return self.submit_input(purpose, buffer);
                    }
                    KeyCode::Backspace => {
                        buffer.pop();
                    }
                    KeyCode::Char(c) if !c.is_control() && buffer.len() < 8 * 1024 => {
                        buffer.push(c);
                    }
                    _ => {}
                }
                return Vec::new();
            }
        }

        if let KeyCode::Char(c @ '1'..='9') = key.code {
            let i = (c as usize) - ('1' as usize);
            return self.switch(View::ALL[i]);
        }
        match key.code {
            KeyCode::Char('q') => {
                self.quit = true;
                Vec::new()
            }
            KeyCode::Char('?') => {
                self.overlay = Overlay::Help;
                Vec::new()
            }
            KeyCode::Tab => self.switch(View::ALL[(self.view.index() + 1) % View::ALL.len()]),
            KeyCode::BackTab => {
                self.switch(View::ALL[(self.view.index() + View::ALL.len() - 1) % View::ALL.len()])
            }
            KeyCode::Char('R') | KeyCode::F(5) => self.refresh_current(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor(false);
                self.after_move()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor(true);
                self.after_move()
            }
            KeyCode::Char(']') | KeyCode::PageDown => {
                if self.page(true) {
                    self.refresh(self.view)
                } else {
                    Vec::new()
                }
            }
            KeyCode::Char('[') | KeyCode::PageUp => {
                if self.page(false) {
                    self.refresh(self.view)
                } else {
                    Vec::new()
                }
            }
            _ => self.view_key(key.code),
        }
    }

    fn move_cursor(&mut self, down: bool) {
        macro_rules! mv {
            ($l:expr) => {
                if down { $l.down() } else { $l.up() }
            };
        }
        match self.view {
            View::Tasks => mv!(self.tasks),
            View::Sessions => mv!(self.sessions),
            View::Approvals => mv!(self.approvals),
            View::Assets => mv!(self.assets),
            View::Records => mv!(self.records),
            View::Reviews => match self.reviews_tab {
                ReviewsTab::Findings => mv!(self.findings),
                ReviewsTab::Verdicts => mv!(self.verdicts),
            },
            View::Reports => mv!(self.reports),
            View::Dashboard | View::Settings => {}
        }
    }

    fn after_move(&self) -> Vec<Request> {
        match (self.view, self.tasks.current()) {
            (View::Tasks, Some(t))
                if self.task_detail.as_ref().map(|d| d.task.id) != Some(t.id) =>
            {
                vec![Request::GetTask { id: t.id }]
            }
            _ => Vec::new(),
        }
    }

    fn page(&mut self, next: bool) -> bool {
        macro_rules! pg {
            ($l:expr) => {
                if next { $l.next_page() } else { $l.prev_page() }
            };
        }
        match self.view {
            View::Tasks => pg!(self.tasks),
            View::Sessions => pg!(self.sessions),
            View::Approvals => pg!(self.approvals),
            View::Assets => pg!(self.assets),
            View::Records => pg!(self.records),
            View::Reviews => match self.reviews_tab {
                ReviewsTab::Findings => pg!(self.findings),
                ReviewsTab::Verdicts => pg!(self.verdicts),
            },
            View::Reports => pg!(self.reports),
            View::Dashboard | View::Settings => false,
        }
    }

    fn confirm(&mut self, prompt: String, request: Request) -> Vec<Request> {
        self.overlay = Overlay::Confirm {
            prompt,
            request: Box::new(request),
        };
        Vec::new()
    }

    fn input(&mut self, prompt: &str, buffer: String, purpose: InputPurpose) -> Vec<Request> {
        self.overlay = Overlay::Input {
            prompt: prompt.into(),
            buffer,
            purpose,
        };
        Vec::new()
    }

    fn view_key(&mut self, code: KeyCode) -> Vec<Request> {
        match self.view {
            View::Tasks => self.tasks_key(code),
            View::Sessions => match (code, self.sessions.current()) {
                (KeyCode::Enter, Some(s)) => {
                    let id = s.id;
                    self.transcript_seq = 0;
                    self.live_output.clear();
                    self.overlay = Overlay::Detail {
                        title: format!("Session {id}"),
                        lines: vec!["loading…".into()],
                        scroll: 0,
                        session: Some(id),
                    };
                    vec![Request::GetTranscript {
                        session_id: id,
                        after_seq: 0,
                        limit: 500,
                    }]
                }
                _ => Vec::new(),
            },
            View::Approvals => match (code, self.approvals.current()) {
                (KeyCode::Char('f'), _) => {
                    self.approvals_pending_only = !self.approvals_pending_only;
                    self.approvals.offset = 0;
                    self.refresh(View::Approvals)
                }
                (KeyCode::Enter, Some(a)) => {
                    let mut lines = vec![
                        format!("Operation: {}", text::clean(&a.operation)),
                        format!("Status:    {}", a.status),
                        String::new(),
                        "Summary:".into(),
                    ];
                    lines.extend(text::lines(&a.summary));
                    lines.push(String::new());
                    lines.push("Request:".into());
                    lines.extend(text::json_lines(&a.request));
                    self.overlay = Overlay::Detail {
                        title: format!("Approval #{}", a.id),
                        lines,
                        scroll: 0,
                        session: None,
                    };
                    Vec::new()
                }
                (KeyCode::Char('y'), Some(a)) if a.status == ApprovalStatus::Pending => {
                    let (id, op) = (a.id, text::short(&a.operation, 40));
                    self.confirm(
                        format!("Approve #{id} ({op})? It is applied immediately. [y/N]"),
                        Request::DecideApproval {
                            id,
                            approve: true,
                            reason: None,
                        },
                    )
                }
                (KeyCode::Char('x'), Some(a)) if a.status == ApprovalStatus::Pending => {
                    let id = a.id;
                    self.confirm(
                        format!("Reject approval #{id}? [y/N]"),
                        Request::DecideApproval {
                            id,
                            approve: false,
                            reason: None,
                        },
                    )
                }
                _ => Vec::new(),
            },
            View::Assets => match (code, self.assets.current()) {
                (KeyCode::Char('/'), _) => {
                    let q = self.asset_query.clone();
                    self.input("Search assets:", q, InputPurpose::AssetSearch)
                }
                (KeyCode::Enter, Some(a)) => vec![Request::GetAsset { id: a.id }],
                _ => Vec::new(),
            },
            View::Records => match (code, self.records.current()) {
                (KeyCode::Char('/'), _) => {
                    let q = self.record_query.clone();
                    self.input("Search records:", q, InputPurpose::RecordSearch)
                }
                (KeyCode::Char('d'), _) if !self.datasets.items.is_empty() => {
                    // Cycle the dataset filter.
                    self.datasets.selected =
                        (self.datasets.selected + 1) % self.datasets.items.len();
                    self.records.offset = 0;
                    self.records.selected = 0;
                    self.refresh(View::Records)
                }
                (KeyCode::Enter, Some(r)) => vec![Request::GetRecord { id: r.id }],
                _ => Vec::new(),
            },
            View::Reviews => self.reviews_key(code),
            View::Reports => match (code, self.reports.current()) {
                (KeyCode::Enter, Some(r)) => vec![Request::GetReport { id: r.id }],
                _ => Vec::new(),
            },
            View::Dashboard | View::Settings => Vec::new(),
        }
    }

    fn tasks_key(&mut self, code: KeyCode) -> Vec<Request> {
        if code == KeyCode::Char('n') {
            return self.input("New task name:", String::new(), InputPurpose::NewTaskName);
        }
        let Some(task) = self.tasks.current() else {
            return Vec::new();
        };
        let (id, name) = (task.id, text::short(task.display_name(), 40));
        let control = |action| Request::ControlTask { id, action };
        match code {
            KeyCode::Enter => vec![Request::GetTask { id }],
            KeyCode::Char('s') => vec![control(TaskAction::Start)],
            KeyCode::Char('p') => vec![control(TaskAction::Pause)],
            KeyCode::Char('r') => vec![control(TaskAction::Resume)],
            KeyCode::Char('c') => self.confirm(
                format!("Cancel task #{id} \"{name}\"? Queued work is dropped. [y/N]"),
                control(TaskAction::Cancel),
            ),
            KeyCode::Char('a') => self.confirm(
                format!("Accept the results of task #{id} \"{name}\"? [y/N]"),
                control(TaskAction::Accept),
            ),
            KeyCode::Char('g') => vec![Request::GenerateReport { task_id: id }],
            KeyCode::Char('m') => self.input(
                &format!("Ask the assistant about #{id}:"),
                String::new(),
                InputPurpose::Assistant(id),
            ),
            _ => Vec::new(),
        }
    }

    fn reviews_key(&mut self, code: KeyCode) -> Vec<Request> {
        if code == KeyCode::Char('t') {
            self.reviews_tab = match self.reviews_tab {
                ReviewsTab::Findings => ReviewsTab::Verdicts,
                ReviewsTab::Verdicts => ReviewsTab::Findings,
            };
            return self.refresh(View::Reviews);
        }
        match self.reviews_tab {
            ReviewsTab::Findings => {
                let Some(f) = self.findings.current() else {
                    return Vec::new();
                };
                let id = f.id;
                let set = |status| vec![Request::SetFindingStatus { id, status }];
                match code {
                    KeyCode::Enter => {
                        let mut lines = vec![
                            format!("Severity: {}   Status: {}", f.severity, f.status),
                            format!(
                                "Origin:   {} ({})",
                                f.provenance.origin,
                                text::clean(&f.provenance.source)
                            ),
                            format!(
                                "Records:  {}",
                                f.record_ids
                                    .iter()
                                    .map(ToString::to_string)
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            String::new(),
                        ];
                        lines.extend(text::lines(&f.summary));
                        self.overlay = Overlay::Detail {
                            title: format!("Finding #{id}: {}", text::short(&f.title, 60)),
                            lines,
                            scroll: 0,
                            session: None,
                        };
                        Vec::new()
                    }
                    KeyCode::Char('y') => set(FindingStatus::Confirmed),
                    KeyCode::Char('x') => set(FindingStatus::Dismissed),
                    KeyCode::Char('v') => set(FindingStatus::Resolved),
                    KeyCode::Char('u') => set(FindingStatus::Duplicate),
                    KeyCode::Char('o') => set(FindingStatus::Pending),
                    _ => Vec::new(),
                }
            }
            ReviewsTab::Verdicts => {
                let Some(r) = self.verdicts.current() else {
                    return Vec::new();
                };
                let id = r.id;
                match code {
                    KeyCode::Enter => {
                        let mut lines = vec![
                            format!("Verdict: {}   Status: {}", r.verdict, r.status),
                            String::new(),
                        ];
                        lines.extend(text::json_lines(&r.details));
                        self.overlay = Overlay::Detail {
                            title: format!("Review result #{id}"),
                            lines,
                            scroll: 0,
                            session: None,
                        };
                        Vec::new()
                    }
                    KeyCode::Char('y') => vec![Request::DecideReviewResult { id, confirm: true }],
                    KeyCode::Char('x') => vec![Request::DecideReviewResult { id, confirm: false }],
                    _ => Vec::new(),
                }
            }
        }
    }

    fn submit_input(&mut self, purpose: InputPurpose, buffer: String) -> Vec<Request> {
        let value = buffer.trim().to_owned();
        match purpose {
            InputPurpose::AssetSearch => {
                self.asset_query = value;
                self.assets.offset = 0;
                self.assets.selected = 0;
                self.refresh(View::Assets)
            }
            InputPurpose::RecordSearch => {
                self.record_query = value;
                self.records.offset = 0;
                self.records.selected = 0;
                self.refresh(View::Records)
            }
            InputPurpose::Assistant(task_id) if !value.is_empty() => {
                self.info("asking the assistant; its session appears under Sessions");
                vec![Request::AssistantMessage {
                    task_id,
                    text: value,
                }]
            }
            InputPurpose::NewTaskName if !value.is_empty() => self.input(
                "Objective:",
                String::new(),
                InputPurpose::NewTaskObjective { name: value },
            ),
            InputPurpose::NewTaskObjective { name } if !value.is_empty() => self.input(
                "Dataset ids (comma-separated):",
                String::new(),
                InputPurpose::NewTaskDatasets {
                    name,
                    objective: value,
                },
            ),
            InputPurpose::NewTaskDatasets { name, objective } => {
                let ids: Result<Vec<DatasetId>, _> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.parse::<i64>().map(DatasetId))
                    .collect();
                match ids {
                    Ok(ids) if !ids.is_empty() => vec![Request::CreateTask {
                        task: NewTask {
                            name,
                            objective,
                            description: String::new(),
                            dataset_ids: ids,
                            batch_size: 25,
                            max_workers: 2,
                            timeout_seconds: 0,
                            token_budget: 0,
                        },
                    }],
                    _ => {
                        self.error("dataset ids must be numbers, e.g. 1,2");
                        Vec::new()
                    }
                }
            }
            _ => {
                self.error("nothing entered");
                Vec::new()
            }
        }
    }

    /// Applies a successful response.
    pub fn on_response(&mut self, response: Response) -> Vec<Request> {
        match response {
            Response::Hello { server, .. } => self.server = server,
            Response::Pong | Response::Subscribed { .. } => {}
            Response::Dashboard(d) => {
                self.last_seq = self.last_seq.max(d.latest_seq);
                self.dashboard = Some(*d);
            }
            Response::Tasks(p) => {
                self.tasks.set(p);
                if let Some(t) = self.tasks.current()
                    && self.task_detail.as_ref().map(|d| d.task.id) != Some(t.id)
                {
                    return vec![Request::GetTask { id: t.id }];
                }
            }
            Response::Task(d) => {
                let task = d.task.clone();
                if self.tasks.items.iter().any(|t| t.id == task.id) {
                    self.tasks.replace(task, |t| t.id == d.task.id);
                } else {
                    // A new task: reload the list so it shows up.
                    self.dirty.insert(View::Tasks);
                }
                self.task_detail = Some(*d);
            }
            Response::Sessions(p) => self.sessions.set(p),
            Response::Transcript(entries) => self.append_transcript(entries),
            Response::Approvals(p) => self.approvals.set(p),
            Response::Approval(a) => {
                self.info(format!("approval #{} is now {}", a.id, a.status));
                let id = a.id;
                self.approvals.replace(a, |x| x.id == id);
                self.dirty.insert(View::Approvals);
            }
            Response::Assets(p) => self.assets.set(p),
            Response::Asset(d) => {
                let a = &d.asset;
                let mut lines = vec![
                    format!("Kind:     {}", a.kind),
                    format!("Identity: {}", text::clean(&a.identity_key)),
                    format!(
                        "Origin:   {} ({})",
                        a.provenance.origin,
                        text::clean(&a.provenance.source)
                    ),
                    String::new(),
                    "Contained in:".into(),
                ];
                if d.ancestry.is_empty() {
                    lines.push("  (top level)".into());
                }
                for up in &d.ancestry {
                    lines.push(format!(
                        "  #{} {} {}",
                        up.id,
                        up.kind,
                        text::short(&up.label, 60)
                    ));
                }
                lines.push(String::new());
                lines.push(format!("Edges ({}):", d.edges.len()));
                for e in d.edges.iter().take(200) {
                    lines.push(format!("  #{} -{}-> #{}", e.src, e.rel, e.dst));
                }
                lines.push(String::new());
                lines.push("Attributes:".into());
                lines.extend(text::json_lines(&a.attributes));
                self.overlay = Overlay::Detail {
                    title: format!("Asset #{}: {}", a.id, text::short(&a.label, 60)),
                    lines,
                    scroll: 0,
                    session: None,
                };
            }
            Response::Datasets(p) => {
                let before = self.selected_dataset();
                self.datasets.set(p);
                // The record list depends on the dataset filter; refetch if it moved.
                if before.is_some() && before != self.selected_dataset() {
                    self.dirty.insert(View::Records);
                }
            }
            Response::Records(p) => self.records.set(p),
            Response::Record(r) => {
                let mut lines = vec![
                    format!("External id:  {}", text::clean(&r.external_id)),
                    format!("Dataset:      #{}", r.dataset_id),
                    format!("Content type: {}", text::clean(&r.content_type)),
                    format!("Redacted:     {}", if r.redacted { "yes" } else { "no" }),
                    format!("Source:       {}", text::clean(&r.provenance.source)),
                    format!("SHA-256:      {}", r.content_hash),
                    String::new(),
                    "Fields:".into(),
                ];
                lines.extend(text::json_lines(&r.fields));
                lines.push(String::new());
                lines.push("Body (imported data, shown as text):".into());
                lines.extend(text::lines(&r.body));
                self.overlay = Overlay::Detail {
                    title: format!("Record #{}: {}", r.id, text::short(&r.title, 60)),
                    lines,
                    scroll: 0,
                    session: None,
                };
            }
            Response::Findings(p) => self.findings.set(p),
            Response::Finding(f) => {
                self.info(format!("finding #{} is now {}", f.id, f.status));
                let id = f.id;
                self.findings.replace(*f, |x| x.id == id);
            }
            Response::ReviewResults(p) => self.verdicts.set(p),
            Response::ReviewResult(r) => {
                self.info(format!("review result #{} is now {}", r.id, r.status));
                let id = r.id;
                self.verdicts.replace(*r, |x| x.id == id);
            }
            Response::Note(_) => self.info("note added"),
            Response::Notes(_) => {}
            Response::Reports(p) => self.reports.set(p),
            Response::Report(r) => {
                self.info(format!("report #{} ready", r.id));
                self.dirty.insert(View::Reports);
                let mut lines = vec![
                    format!("Status: {}   Origin: {}", r.status, r.origin),
                    String::new(),
                ];
                lines.extend(text::lines(&r.body_markdown));
                self.overlay = Overlay::Detail {
                    title: format!("Report #{}: {}", r.id, text::short(&r.title, 60)),
                    lines,
                    scroll: 0,
                    session: None,
                };
            }
            Response::Settings(s) => self.settings = Some(s),
            Response::Accepted => self.info("accepted"),
        }
        Vec::new()
    }

    fn append_transcript(&mut self, entries: Vec<TranscriptEntry>) {
        let Overlay::Detail {
            lines,
            session: Some(open),
            ..
        } = &mut self.overlay
        else {
            return;
        };
        if lines.len() == 1 && lines[0] == "loading…" {
            lines.clear();
        }
        for e in entries.into_iter().filter(|e| e.session_id == *open) {
            if e.seq <= self.transcript_seq {
                continue;
            }
            self.transcript_seq = e.seq;
            // Persisted output replaces the streamed preview.
            self.live_output.clear();
            lines.push(format!(
                "── {} #{} · {} in / {} out tokens",
                e.kind, e.seq, e.input_tokens, e.output_tokens
            ));
            match &e.content {
                serde_json::Value::String(s) => lines.extend(text::lines(s)),
                other => lines.extend(text::json_lines(other)),
            }
        }
    }

    /// Applies a pushed event and marks affected views for refresh.
    pub fn on_event(&mut self, envelope: EventEnvelope) -> Vec<Request> {
        if envelope.seq > 0 {
            self.last_seq = self.last_seq.max(envelope.seq);
        }
        self.dirty.insert(View::Dashboard);
        match &envelope.event {
            DomainEvent::TaskCreated { .. } | DomainEvent::TaskStatusChanged { .. } => {
                self.dirty.insert(View::Tasks);
            }
            DomainEvent::WorkItemsPlanned { .. } | DomainEvent::WorkItemFinished { .. } => {
                self.dirty.insert(View::Tasks);
            }
            DomainEvent::SessionStarted { .. } | DomainEvent::SessionStatusChanged { .. } => {
                self.dirty.insert(View::Sessions);
            }
            DomainEvent::TranscriptAppended { session_id, .. } => {
                if let Overlay::Detail {
                    session: Some(open),
                    ..
                } = &self.overlay
                    && open == session_id
                {
                    return vec![Request::GetTranscript {
                        session_id: *session_id,
                        after_seq: self.transcript_seq,
                        limit: 500,
                    }];
                }
            }
            DomainEvent::OutputDelta { session_id, text } => {
                if let Overlay::Detail {
                    lines,
                    session: Some(open),
                    ..
                } = &mut self.overlay
                    && open == session_id
                {
                    if self.live_output.len() < MAX_LIVE_OUTPUT {
                        self.live_output.push_str(text);
                    }
                    // Keep the streamed preview as the trailing lines.
                    while lines.last().is_some_and(|l| l.starts_with("▸ ")) {
                        lines.pop();
                    }
                    for l in text::lines(&self.live_output) {
                        lines.push(format!("▸ {l}"));
                    }
                }
                self.dirty.remove(&View::Dashboard);
            }
            DomainEvent::ApprovalRequested { approval_id, .. } => {
                self.info(format!("approval #{approval_id} requested (view 4)"));
                self.dirty.insert(View::Approvals);
            }
            DomainEvent::ApprovalDecided { .. } => {
                self.dirty.insert(View::Approvals);
            }
            DomainEvent::FindingRecorded { .. } | DomainEvent::FindingStatusChanged { .. } => {
                self.dirty.insert(View::Reviews);
            }
            DomainEvent::RecordsImported { .. } => {
                self.dirty.insert(View::Records);
                self.dirty.insert(View::Assets);
            }
            DomainEvent::ReportGenerated { .. } => {
                self.dirty.insert(View::Reports);
            }
            DomainEvent::WorkItemClaimed { .. } | DomainEvent::DaemonStarted { .. } => {}
        }
        Vec::new()
    }

    /// Called after (re)connecting: resubscribe and reload what is visible.
    pub fn on_connected(&mut self, server: String) -> Vec<Request> {
        self.connected = true;
        self.server = server;
        self.info("connected");
        let mut r = vec![Request::Subscribe {
            after_seq: self.last_seq,
        }];
        r.extend(self.refresh_current());
        r
    }

    pub fn on_disconnected(&mut self) {
        if self.connected {
            self.error("disconnected from the daemon; reconnecting…");
        }
        self.connected = false;
    }

    /// The daemon dropped events for us: reload everything visible and resubscribe.
    pub fn on_resync(&mut self, latest_seq: i64) -> Vec<Request> {
        self.last_seq = latest_seq;
        let mut r = vec![Request::Subscribe {
            after_seq: latest_seq,
        }];
        r.extend(self.refresh_current());
        r
    }
}
