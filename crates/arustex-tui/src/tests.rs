use arustex_core::Paged;
use arustex_core::domain::task::{Task, TaskAction, TaskStatus};
use arustex_core::domain::work::{Approval, ApprovalStatus};
use arustex_core::domain::{ExplorationId, TaskId};
use arustex_core::events::{DomainEvent, EventEnvelope};
use arustex_core::ipc::{DashboardSnapshot, Request, Response};
use arustex_core::ports::RecordSummary;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use serde_json::json;

use crate::app::{App, Overlay, View};
use crate::ui::{Theme, draw};

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

fn code(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

fn task(id: i64, status: TaskStatus) -> Task {
    let now = chrono::Utc::now();
    Task {
        id: TaskId(id),
        name: format!("task {id}"),
        objective: "Review synthetic records".into(),
        description: String::new(),
        status,
        exploration_id: ExplorationId(1),
        dataset_ids: vec![1.into()],
        batch_size: 25,
        max_workers: 2,
        timeout_seconds: 0,
        token_budget: 0,
        tokens_used: 0,
        legacy_id: None,
        created_at: now,
        updated_at: now,
        started_at: None,
        completed_at: None,
    }
}

fn approval(id: i64) -> Approval {
    Approval {
        id: id.into(),
        task_id: Some(TaskId(1)),
        session_id: None,
        operation: "propose_status_update".into(),
        summary: "Set finding 3 to dismissed".into(),
        request: json!({"finding_id": 3, "status": "dismissed"}),
        status: ApprovalStatus::Pending,
        decided_by: None,
        reason: None,
        created_at: chrono::Utc::now(),
        decided_at: None,
    }
}

fn paged<T>(items: Vec<T>) -> Paged<T> {
    let n = items.len() as u64;
    Paged::new(items, n, arustex_core::Page::new(0, 50))
}

fn render(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal
        .draw(|f| draw(f, app, Theme { color: false }))
        .unwrap();
    let buf = terminal.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn every_view_renders_without_data_and_on_a_tiny_terminal() {
    let mut app = App::new();
    for view in View::ALL {
        app.view = view;
        let screen = render(&app);
        assert!(screen.contains(view.tab()), "{view:?}");
        let mut tiny = Terminal::new(TestBackend::new(20, 5)).unwrap();
        tiny.draw(|f| draw(f, &app, Theme { color: true })).unwrap();
    }
}

#[test]
fn number_keys_switch_views_and_request_their_data() {
    let mut app = App::new();
    let reqs = app.on_key(key('2'));
    assert_eq!(app.view, View::Tasks);
    assert!(matches!(reqs[0], Request::ListTasks { .. }));
    let reqs = app.on_key(key('9'));
    assert_eq!(app.view, View::Settings);
    assert_eq!(reqs, vec![Request::GetSettings]);
    app.on_key(code(KeyCode::Tab));
    assert_eq!(app.view, View::Dashboard);
    app.on_key(code(KeyCode::BackTab));
    assert_eq!(app.view, View::Settings);
    app.on_key(key('q'));
    assert!(app.quit);
}

#[test]
fn task_actions_and_confirmation() {
    let mut app = App::new();
    app.on_key(key('2'));
    app.on_response(Response::Tasks(paged(vec![
        task(1, TaskStatus::Created),
        task(2, TaskStatus::NeedsReview),
    ])));
    assert_eq!(
        app.on_key(key('s')),
        vec![Request::ControlTask {
            id: TaskId(1),
            action: TaskAction::Start
        }]
    );
    let reqs = app.on_key(code(KeyCode::Down));
    assert_eq!(reqs, vec![Request::GetTask { id: TaskId(2) }]);
    // Cancel needs confirmation; anything but y aborts.
    assert!(app.on_key(key('c')).is_empty());
    assert!(matches!(app.overlay, Overlay::Confirm { .. }));
    assert!(app.on_key(key('n')).is_empty());
    assert_eq!(app.overlay, Overlay::None);
    app.on_key(key('a'));
    assert_eq!(
        app.on_key(key('y')),
        vec![Request::ControlTask {
            id: TaskId(2),
            action: TaskAction::Accept
        }]
    );
    let screen = render(&app);
    assert!(screen.contains("task 2"));
    assert!(screen.contains("needs_review"));
}

#[test]
fn new_task_wizard_collects_fields() {
    let mut app = App::new();
    app.on_key(key('2'));
    app.on_key(key('n'));
    for c in "Weekly".chars() {
        app.on_key(key(c));
    }
    app.on_key(code(KeyCode::Enter));
    for c in "Check records".chars() {
        app.on_key(key(c));
    }
    app.on_key(code(KeyCode::Enter));
    for c in "1, x".chars() {
        app.on_key(key(c));
    }
    assert!(app.on_key(code(KeyCode::Enter)).is_empty());
    assert!(app.status_error, "bad dataset ids are rejected");
    app.on_key(key('n'));
    for step in ["Weekly", "Check records", "1,2"] {
        for c in step.chars() {
            app.on_key(key(c));
        }
        let reqs = app.on_key(code(KeyCode::Enter));
        if step == "1,2" {
            let Request::CreateTask { task } = &reqs[0] else {
                panic!("expected create");
            };
            assert_eq!(task.name, "Weekly");
            assert_eq!(task.dataset_ids, vec![1.into(), 2.into()]);
        }
    }
}

#[test]
fn approvals_require_confirmation_before_deciding() {
    let mut app = App::new();
    app.on_key(key('4'));
    app.on_response(Response::Approvals(paged(vec![approval(7)])));
    assert!(app.on_key(key('y')).is_empty());
    assert_eq!(
        app.on_key(key('y')),
        vec![Request::DecideApproval {
            id: 7.into(),
            approve: true,
            reason: None
        }]
    );
    assert!(render(&app).contains("propose_status_update"));
}

#[test]
fn hostile_record_text_is_neutralized_on_screen() {
    let mut app = App::new();
    app.on_key(key('6'));
    app.on_response(Response::Records(paged(vec![RecordSummary {
        id: 1.into(),
        dataset_id: 1.into(),
        external_id: "x\u{1b}[31m".into(),
        title: "evil\u{1b}]0;pwned\u{7}title".into(),
        content_type: "text/plain".into(),
        body_bytes: 10,
    }])));
    let record = serde_json::from_value(json!({
        "id": 1, "dataset_id": 1, "external_id": "x", "title": "t\u{1b}[2J",
        "content_type": "text/plain", "body": "line1\n\u{1b}[2Jline2\u{202E}",
        "fields": {"k": "\u{1b}"}, "content_hash": "ab", "redacted": false,
        "provenance": {"origin": "supplied", "source": "f"},
        "imported_at": chrono::Utc::now(),
    }))
    .unwrap();
    app.on_response(Response::Record(Box::new(record)));
    let screen = render(&app);
    assert!(!screen.contains('\u{1b}'));
    assert!(!screen.contains('\u{7}'));
    assert!(!screen.contains('\u{202E}'));
    assert!(screen.contains("line2"));
}

#[test]
fn events_mark_views_dirty_and_resync_reloads() {
    let mut app = App::new();
    app.on_key(key('2'));
    let env = |seq, event| EventEnvelope {
        seq,
        at: chrono::Utc::now(),
        task_id: None,
        session_id: None,
        event,
    };
    app.on_event(env(
        5,
        DomainEvent::TaskStatusChanged {
            task_id: TaskId(1),
            from: TaskStatus::Queued,
            to: TaskStatus::Running,
            reason: None,
        },
    ));
    app.on_event(env(
        6,
        DomainEvent::ApprovalRequested {
            approval_id: 3.into(),
            operation: "propose_status_update".into(),
        },
    ));
    assert_eq!(app.last_seq, 6);
    let reqs = app.take_dirty();
    assert!(reqs.contains(&Request::Dashboard));
    assert!(reqs.iter().any(|r| matches!(r, Request::ListTasks { .. })));
    assert!(
        !reqs
            .iter()
            .any(|r| matches!(r, Request::ListApprovals { .. })),
        "hidden views wait until shown"
    );
    let reqs = app.on_key(key('4'));
    assert!(
        reqs.iter()
            .any(|r| matches!(r, Request::ListApprovals { .. }))
    );

    let reqs = app.on_resync(40);
    assert_eq!(reqs[0], Request::Subscribe { after_seq: 40 });
    app.on_disconnected();
    let reqs = app.on_connected("arustex test".into());
    assert_eq!(reqs[0], Request::Subscribe { after_seq: 40 });
}

#[test]
fn session_transcript_and_live_output_append() {
    use arustex_core::domain::agent::{SessionId, TranscriptEntry, TranscriptKind};
    let mut app = App::new();
    let sid = SessionId::new();
    app.overlay = Overlay::Detail {
        title: "Session".into(),
        lines: vec!["loading…".into()],
        scroll: 0,
        session: Some(sid),
    };
    app.on_event(EventEnvelope {
        seq: 0,
        at: chrono::Utc::now(),
        task_id: None,
        session_id: Some(sid),
        event: DomainEvent::OutputDelta {
            session_id: sid,
            text: "partial \u{1b}answer".into(),
        },
    });
    let Overlay::Detail { lines, .. } = &app.overlay else {
        panic!()
    };
    assert!(lines.iter().any(|l| l.starts_with("▸ partial")));
    app.on_response(Response::Transcript(vec![TranscriptEntry {
        session_id: sid,
        seq: 1,
        kind: TranscriptKind::Assistant,
        content: json!("final answer"),
        input_tokens: 1,
        output_tokens: 2,
        created_at: chrono::Utc::now(),
    }]));
    let Overlay::Detail { lines, .. } = &app.overlay else {
        panic!()
    };
    assert!(lines.iter().any(|l| l == "final answer"));
    assert!(lines.iter().all(|l| !l.contains('\u{1b}')));
    let reqs = app.on_event(EventEnvelope {
        seq: 9,
        at: chrono::Utc::now(),
        task_id: None,
        session_id: Some(sid),
        event: DomainEvent::TranscriptAppended {
            session_id: sid,
            seq: 2,
        },
    });
    assert_eq!(
        reqs,
        vec![Request::GetTranscript {
            session_id: sid,
            after_seq: 1,
            limit: 500
        }]
    );
}

#[test]
fn dashboard_renders_counts() {
    let mut app = App::new();
    app.on_response(Response::Dashboard(Box::new(DashboardSnapshot {
        latest_seq: 12,
        daemon_version: "0.1.0".into(),
        started_at: chrono::Utc::now(),
        tasks_by_status: vec![("running".into(), 2)],
        active_sessions: 1,
        pending_approvals: 3,
        pending_findings: 4,
        datasets: 1,
        records: 99,
        tokens_today: 1234,
        recent_events: vec![],
    })));
    app.connected = true;
    let screen = render(&app);
    assert!(screen.contains("3 pending"));
    assert!(screen.contains("99"));
    assert!(screen.contains("connected"));
    assert_eq!(app.last_seq, 12);
}
