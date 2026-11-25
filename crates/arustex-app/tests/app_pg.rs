//! Application tests against PostgreSQL: IPC over a real Unix socket, the daemon with
//! the offline mock provider, reports, approvals, JSONL import and Telegram delivery.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arustex_app::client::{Client, ClientEvent};
use arustex_app::config::{Config, Loaded, ProviderConfig, ProviderKind, TelegramConfig};
use arustex_app::daemon::Daemon;
use arustex_app::notify::{TelegramSender, deliver_one, task_text};
use arustex_app::server::{Server, ServerSettings};
use arustex_app::services::{Caller, Services};
use arustex_app::{ingest, reports};
use arustex_core::Secret;
use arustex_core::domain::finding::{FindingStatus, NewFinding, Severity};
use arustex_core::domain::record::IngestLimits;
use arustex_core::domain::task::{NewTask, TaskAction, TaskStatus};
use arustex_core::domain::work::{ApprovalStatus, NewApproval};
use arustex_core::domain::{DatasetId, Origin, Provenance};
use arustex_core::events::DomainEvent;
use arustex_core::ipc::{Request, Response, SettingsView};
use arustex_core::ports::{ReviewSink, TaskStore};
use arustex_core::{ErrorCode, Page};
use arustex_operations::PROPOSE_STATUS_UPDATE;
use arustex_runtime::TaskHooks;
use arustex_store::Store;
use arustex_store::repository::records::IngestOptions;
use arustex_store::testing::TestDb;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

macro_rules! test_db {
    () => {
        match TestDb::create().await {
            Some(db) => db,
            None => return,
        }
    };
}

#[derive(Debug)]
struct NoHooks;
impl TaskHooks for NoHooks {}

fn opts() -> IngestOptions {
    IngestOptions {
        source: "fixture.jsonl".into(),
        redact: true,
        limits: IngestLimits::default(),
    }
}

async fn dataset(store: &Store, n: usize) -> DatasetId {
    let ds = store
        .create_dataset("synthetic", "", "fixture", 0, "test")
        .await
        .unwrap();
    let mut text = String::new();
    for i in 0..n {
        let line = json!({
            "external_id": format!("r{i}"),
            "title": format!("synthetic record {i}"),
            "body": format!("SECRET-BODY-{i}"),
            "fields": {"level": if i % 2 == 0 { "warn" } else { "info" }},
        });
        text.push_str(&line.to_string());
        text.push('\n');
    }
    ingest::import_jsonl(store, ds.id, text.as_bytes(), &opts())
        .await
        .unwrap();
    ds.id
}

fn new_task(ds: DatasetId) -> NewTask {
    NewTask {
        name: "synthetic | review".into(),
        objective: "Review synthetic records".into(),
        description: String::new(),
        dataset_ids: vec![ds],
        batch_size: 2,
        max_workers: 2,
        timeout_seconds: 0,
        token_budget: 0,
    }
}

fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("run").join("arustex.sock")
}

async fn start_server(
    store: &Store,
    socket: PathBuf,
) -> (
    Arc<Services>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let shutdown = CancellationToken::new();
    let services = Arc::new(Services::new(
        Arc::new(store.clone()),
        None,
        Arc::new(NoHooks),
        SettingsView {
            entries: vec![("telegram.enabled".into(), "false".into())],
        },
        shutdown.clone(),
    ));
    let server = Server::bind(ServerSettings {
        socket,
        max_clients: 2,
        client_queue: 64,
        request_timeout: Duration::from_secs(10),
    })
    .await
    .unwrap();
    let handle = tokio::spawn(server.serve(services.clone(), shutdown.clone()));
    (services, shutdown, handle)
}

async fn connect(socket: &std::path::Path) -> (Client, tokio::sync::mpsc::Receiver<ClientEvent>) {
    Client::connect(socket, "test", Duration::from_secs(10), 256)
        .await
        .unwrap()
}

async fn next_event(
    rx: &mut tokio::sync::mpsc::Receiver<ClientEvent>,
    want: impl Fn(&DomainEvent) -> bool,
) -> DomainEvent {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match rx.recv().await {
                Some(ClientEvent::Event(e)) if want(&e.event) => return e.event,
                Some(ClientEvent::Disconnected) | None => panic!("disconnected"),
                _ => {}
            }
        }
    })
    .await
    .expect("event in time")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ipc_round_trip_subscribe_and_socket_hardening() {
    let db = test_db!();
    let ds = dataset(&db.store, 4).await;
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let (_services, shutdown, handle) = start_server(&db.store, socket.clone()).await;

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(socket.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);

    // A second daemon on the same socket is refused.
    let second = Server::bind(ServerSettings {
        socket: socket.clone(),
        max_clients: 1,
        client_queue: 8,
        request_timeout: Duration::from_secs(1),
    })
    .await;
    assert!(second.is_err());

    let (client, mut events) = connect(&socket).await;
    assert!(client.server.starts_with("arustex"));
    assert_eq!(client.request(Request::Ping).await.unwrap(), Response::Pong);

    let Response::Subscribed { latest_seq } = client
        .request(Request::Subscribe { after_seq: 0 })
        .await
        .unwrap()
    else {
        panic!("expected subscribed");
    };
    assert!(latest_seq > 0, "import recorded events");
    // Stored events are replayed first.
    next_event(&mut events, |e| {
        matches!(e, DomainEvent::RecordsImported { .. })
    })
    .await;

    let Response::Task(detail) = client
        .request(Request::CreateTask { task: new_task(ds) })
        .await
        .unwrap()
    else {
        panic!("expected task");
    };
    assert_eq!(detail.task.status, TaskStatus::Created);
    next_event(&mut events, |e| {
        matches!(e, DomainEvent::TaskCreated { .. })
    })
    .await;

    let Response::Task(started) = client
        .request(Request::ControlTask {
            id: detail.task.id,
            action: TaskAction::Start,
        })
        .await
        .unwrap()
    else {
        panic!("expected task");
    };
    assert_eq!(started.task.status, TaskStatus::Queued);
    let err = client
        .request(Request::ControlTask {
            id: detail.task.id,
            action: TaskAction::Accept,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Conflict);

    let Response::Dashboard(dash) = client.request(Request::Dashboard).await.unwrap() else {
        panic!("expected dashboard");
    };
    assert_eq!(dash.records, 4);
    assert_eq!(dash.datasets, 1);
    let Response::Records(records) = client
        .request(Request::ListRecords {
            dataset_id: Some(ds),
            query: None,
            page: Page::new(0, 10),
        })
        .await
        .unwrap()
    else {
        panic!("expected records");
    };
    assert_eq!(records.total, 4);
    let Response::Settings(settings) = client.request(Request::GetSettings).await.unwrap() else {
        panic!("expected settings");
    };
    assert_eq!(settings.entries.len(), 1);

    // Raw protocol violations: bad JSON, unknown methods and extra fields are answered
    // with errors and the connection stays usable; an oversized frame closes it.
    let stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    for bad in [
        "not json",
        r#"{"v":1,"id":5,"request":{"method":"run_shell","params":{"cmd":"id"}}}"#,
        r#"{"v":9,"id":6,"request":{"method":"ping"}}"#,
    ] {
        w.write_all(format!("{bad}\n").as_bytes()).await.unwrap();
        let reply = lines.next_line().await.unwrap().unwrap();
        assert!(reply.contains("\"Err\""), "{reply}");
    }
    w.write_all(b"{\"v\":1,\"id\":7,\"request\":{\"method\":\"ping\"}}\n")
        .await
        .unwrap();
    let reply = lines.next_line().await.unwrap().unwrap();
    assert!(reply.contains("pong"), "{reply}");
    let huge = vec![b'a'; arustex_core::ipc::MAX_FRAME_BYTES + 10];
    w.write_all(&huge).await.unwrap();
    let reply = lines.next_line().await.unwrap().unwrap();
    assert!(
        reply.contains("limit_exceeded") || reply.contains("too large"),
        "{reply}"
    );
    assert!(
        lines.next_line().await.unwrap().is_none(),
        "connection closed"
    );

    drop(client);
    shutdown.cancel();
    handle.await.unwrap();
    assert!(!socket.exists(), "socket removed on shutdown");
    db.drop_db().await;
}

#[tokio::test]
async fn approvals_apply_only_after_operator_decision() {
    let db = test_db!();
    let store = Arc::new(db.store.clone());
    let ds = dataset(&store, 2).await;
    let task = store.create_task(&new_task(ds), "test").await.unwrap();
    let finding = store
        .record_finding(NewFinding {
            task_id: Some(task.id),
            node_id: None,
            title: "synthetic observation".into(),
            category: "synthetic".into(),
            severity: Severity::Low,
            summary: "summary".into(),
            record_ids: vec![],
            asset_ids: vec![],
            provenance: Provenance {
                origin: Origin::ModelGenerated,
                source: "worker".into(),
                locator: None,
                session_id: None,
            },
            status: None,
            preserve_id: None,
        })
        .await
        .unwrap();
    let approval = store
        .request_approval(NewApproval {
            task_id: Some(task.id),
            session_id: None,
            operation: PROPOSE_STATUS_UPDATE.into(),
            summary: "dismiss".into(),
            request: json!({"finding_id": finding.id, "status": "dismissed"}),
        })
        .await
        .unwrap();
    assert_eq!(
        store.get_finding(finding.id).await.unwrap().status,
        FindingStatus::Pending
    );
    let services = Services::new(
        store.clone(),
        None,
        Arc::new(NoHooks),
        SettingsView { entries: vec![] },
        CancellationToken::new(),
    );
    let caller = Caller { uid: 1000 };
    let Response::Approval(done) = services
        .handle(
            &caller,
            Request::DecideApproval {
                id: approval.id,
                approve: true,
                reason: None,
            },
        )
        .await
        .unwrap()
    else {
        panic!("expected approval");
    };
    assert_eq!(done.status, ApprovalStatus::Applied);
    assert_eq!(
        store.get_finding(finding.id).await.unwrap().status,
        FindingStatus::Dismissed
    );
    // A decided approval cannot be decided again.
    assert!(
        services
            .handle(
                &caller,
                Request::DecideApproval {
                    id: approval.id,
                    approve: true,
                    reason: None,
                },
            )
            .await
            .is_err()
    );
    // The assistant needs the scheduler; without it the request is refused cleanly.
    let err = services
        .handle(
            &caller,
            Request::AssistantMessage {
                task_id: task.id,
                text: "hi".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::Unavailable);
    db.drop_db().await;
}

#[tokio::test]
async fn reports_carry_provenance_and_omit_record_bodies() {
    let db = test_db!();
    let store = db.store.clone();
    let ds = dataset(&store, 3).await;
    let task = store.create_task(&new_task(ds), "test").await.unwrap();
    let record = store
        .list_records(Some(ds), None, Page::new(0, 1))
        .await
        .unwrap()
        .items[0]
        .id;
    store
        .record_finding(NewFinding {
            task_id: Some(task.id),
            node_id: None,
            title: "<b>odd</b> | value".into(),
            category: "synthetic".into(),
            severity: Severity::Medium,
            summary: "Two records disagree.".into(),
            record_ids: vec![record],
            asset_ids: vec![],
            provenance: Provenance {
                origin: Origin::ModelGenerated,
                source: "worker".into(),
                locator: None,
                session_id: Some(uuid::Uuid::now_v7()),
            },
            status: None,
            preserve_id: None,
        })
        .await
        .unwrap();
    let report = reports::generate_report(&store, task.id).await.unwrap();
    assert_eq!(report.origin, Origin::Derived);
    assert_eq!(report.document["format"], reports::REPORT_FORMAT);
    let f = &report.document["findings"][0];
    assert_eq!(f["provenance"]["origin"], "model_generated");
    assert_eq!(f["record_ids"][0], json!(record));
    assert!(report.body_markdown.contains("model-generated"));
    assert!(
        report
            .body_markdown
            .contains("&lt;b&gt;odd&lt;/b&gt; \\| value")
    );
    let all = format!("{}{}", report.body_markdown, report.document);
    assert!(
        !all.contains("SECRET-BODY"),
        "record bodies never enter reports"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn jsonl_import_validates_lines_and_is_idempotent() {
    let db = test_db!();
    let store = db.store.clone();
    let ds = store
        .create_dataset("import", "", "fixture", 0, "test")
        .await
        .unwrap();
    let good = "{\"external_id\":\"a\",\"body\":\"x\"}\n\n{\"external_id\":\"b\"}\n";
    let r = ingest::import_jsonl(&store, ds.id, good.as_bytes(), &opts())
        .await
        .unwrap();
    assert_eq!(r.inserted, 2);
    let again = ingest::import_jsonl(&store, ds.id, good.as_bytes(), &opts())
        .await
        .unwrap();
    assert_eq!(again.inserted, 0);
    assert_eq!(again.duplicates, 2);

    let bad = "{\"external_id\":\"c\"}\n{\"external_id\":\"d\",\"shell\":\"id\"}\n";
    let err = ingest::import_jsonl(&store, ds.id, bad.as_bytes(), &opts())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("line 2"), "{err}");

    let mut small = opts();
    small.limits.max_line_bytes = 64;
    let long = format!(
        "{{\"external_id\":\"e\",\"body\":\"{}\"}}\n",
        "x".repeat(200)
    );
    let err = ingest::import_jsonl(&store, ds.id, long.as_bytes(), &small)
        .await
        .unwrap_err();
    assert_eq!(err.code(), ErrorCode::LimitExceeded);

    let dir = tempfile::tempdir().unwrap();
    let link = dir.path().join("link.jsonl");
    std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
    assert!(
        ingest::import_jsonl_file(&store, ds.id, &link, &opts())
            .await
            .is_err()
    );
    db.drop_db().await;
}

#[tokio::test]
async fn telegram_delivery_uses_fixed_chat_and_hides_token() {
    let db = test_db!();
    let store = db.store.clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/botTEST-TOKEN/sendMessage"))
        .and(body_partial_json(json!({"chat_id": 4242, "text": "hello"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let cfg = TelegramConfig {
        enabled: true,
        chat_id: Some(4242),
        api_base: server.uri(),
        ..TelegramConfig::default()
    };
    let sender = TelegramSender::new(&cfg, &Secret::new("TEST-TOKEN")).unwrap();
    store
        .enqueue_notification("task", None, "hello")
        .await
        .unwrap();
    assert!(deliver_one(&store, &sender, 3).await);
    assert!(!deliver_one(&store, &sender, 3).await, "outbox drained");

    store
        .enqueue_notification("task", None, "second")
        .await
        .unwrap();
    assert!(deliver_one(&store, &sender, 3).await);
    let err = sender.send("x").await.unwrap_err();
    assert!(!err.contains("TEST-TOKEN"));
    let (state, last_error): (String, String) = sqlx_row(&store).await;
    assert_eq!(state, "pending", "failed delivery is retried later");
    assert!(last_error.contains("500") && !last_error.contains("TEST-TOKEN"));

    // Disabled by default, and only enabled events produce texts.
    assert!(!TelegramConfig::default().enabled);
    let task = store
        .create_task(&new_task(dataset(&store, 1).await), "test")
        .await
        .unwrap();
    assert!(
        task_text(&cfg, &task, None).is_none(),
        "created tasks are not announced"
    );
    let done = store
        .transition_task(task.id, TaskStatus::Cancelled, None)
        .await
        .unwrap();
    assert!(task_text(&cfg, &done, None).is_none());
    db.drop_db().await;
}

async fn sqlx_row(store: &Store) -> (String, String) {
    use sqlx::Row;
    let row = sqlx::query("SELECT state, last_error FROM notifications ORDER BY id DESC LIMIT 1")
        .fetch_one(store.pool())
        .await
        .unwrap();
    (row.get("state"), row.get("last_error"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_runs_a_task_end_to_end_with_the_mock_provider() {
    let db = test_db!();
    let ds = dataset(&db.store, 4).await;
    let dir = tempfile::tempdir().unwrap();
    let socket = socket_in(&dir);
    let mut config = Config {
        providers: vec![ProviderConfig {
            name: "mock".into(),
            kind: ProviderKind::Mock,
            model: "mock-1".into(),
            base_url: None,
            api_key: Default::default(),
            default: true,
            timeout_secs: 10,
            connect_timeout_secs: 5,
            max_retries: 0,
            requests_per_minute: 0,
            max_output_tokens: 1024,
            context_window: 32_000,
        }],
        ..Config::default()
    };
    config.scheduler.poll_interval_ms = 50;
    config.database.max_connections = 8;
    let loaded = Loaded {
        config,
        source: None,
        database_url: Secret::new(db.url.clone()),
        socket: socket.clone(),
        provider_keys: vec![("mock".into(), None)],
        telegram_token: None,
    };
    let shutdown = CancellationToken::new();
    let daemon = Daemon::build(loaded, shutdown.clone()).await.unwrap();
    let run = tokio::spawn(daemon.run());

    let (client, mut events) = connect(&socket).await;
    client
        .request(Request::Subscribe {
            after_seq: i64::MAX,
        })
        .await
        .unwrap();
    let Response::Task(detail) = client
        .request(Request::CreateTask { task: new_task(ds) })
        .await
        .unwrap()
    else {
        panic!("expected task");
    };
    let id = detail.task.id;
    client
        .request(Request::ControlTask {
            id,
            action: TaskAction::Start,
        })
        .await
        .unwrap();
    next_event(&mut events, |e| {
        matches!(e, DomainEvent::TaskStatusChanged { task_id, to: TaskStatus::Completed, .. } if *task_id == id)
    })
    .await;
    // The completion hook generated a report.
    next_event(&mut events, |e| {
        matches!(e, DomainEvent::ReportGenerated { .. })
    })
    .await;
    let Response::Reports(reports) = client
        .request(Request::ListReports {
            task_id: Some(id),
            page: Page::new(0, 10),
        })
        .await
        .unwrap()
    else {
        panic!("expected reports");
    };
    assert_eq!(reports.total, 1);

    // The assistant streams output for an operator question.
    client
        .request(Request::AssistantMessage {
            task_id: id,
            text: "What did the review cover?".into(),
        })
        .await
        .unwrap();
    next_event(&mut events, |e| {
        matches!(e, DomainEvent::SessionStarted { .. })
    })
    .await;

    drop(client);
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("daemon stops")
        .unwrap()
        .unwrap();
    db.drop_db().await;
}
