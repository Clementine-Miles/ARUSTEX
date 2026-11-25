use arustex_core::{MAX_PAGE_SIZE, Rows, View};
use arustex_store::{Store, StoreError};

#[test]
fn validates_effective_database_endpoint_and_tls() {
    for dsn in [
        "postgres://fixture@127.0.0.1/records?sslmode=disable",
        "postgresql://fixture@[::1]:5433/records?sslmode=disable",
        "postgres://fixture@db.invalid/records?ssl-mode=verify-full",
    ] {
        assert_eq!(Store::validate_dsn(dsn), Ok(()));
    }
    for dsn in [
        "postgres://fixture@db.invalid/records?sslmode=verify-full&ssl-mode=disable",
        "postgres://fixture@127.0.0.1/records?host=db.invalid",
        "postgres://fixture@127.0.0.1/records?hostaddr=192.0.2.1",
        "postgres://fixture@127.0.0.1/records?port=6543",
    ] {
        assert_eq!(
            Store::validate_dsn(dsn),
            Err(StoreError::InvalidDatabaseOptions)
        );
    }
    assert_eq!(
        Store::validate_dsn("postgres://fixture@db.invalid/records?sslmode=disable"),
        Err(StoreError::InsecureDatabaseTransport)
    );
    assert_eq!(
        Store::validate_dsn("postgresql:///records"),
        Err(StoreError::InvalidDatabaseUrl)
    );
}

#[tokio::test]
async fn invalid_connection_options_do_not_expose_input() {
    assert!(matches!(
        Store::connect("not_a_postgres_uri=synthetic_value", 1).await,
        Err(StoreError::InvalidDatabaseUrl)
    ));
    assert!(matches!(
        Store::connect("postgres://localhost/fixture", 0).await,
        Err(StoreError::InvalidConnectionCount)
    ));
    assert!(
        !StoreError::InvalidDatabaseUrl
            .to_string()
            .contains("synthetic_value")
    );
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database with db/schema.sql and legacy_fixture.sql applied"]
async fn reads_legacy_rows_with_column_limited_role() {
    let url = std::env::var("ARUSTEX_TEST_DATABASE_URL").expect("set a disposable fixture URL");
    let store = Store::connect(&url, 2).await.unwrap();

    let latest = store.browse(View::Tasks, None, 1).await.unwrap();
    assert_eq!(latest.next_before, Some(3000000012));
    let Rows::Tasks(tasks) = latest.rows else {
        panic!("wrong projection");
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, 3000000012);
    assert_eq!(tasks[0].exploration_id, 3000000002);
    assert_eq!(tasks[0].name.chars().count(), 128);
    assert!(tasks[0].archived);

    let previous = store
        .browse(View::Tasks, latest.next_before, 1)
        .await
        .unwrap();
    assert_eq!(previous.next_before, None);
    let Rows::Tasks(tasks) = previous.rows else {
        panic!("wrong projection");
    };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, 3000000011);
    assert!(!tasks[0].archived);

    let Rows::Assets(assets) = store.browse(View::Assets, None, 10).await.unwrap().rows else {
        panic!("wrong projection");
    };
    assert_eq!(assets.len(), 2);
    assert_eq!(assets[0].id, 3000000022);
    assert_eq!(assets[0].label, None);
    assert_eq!(assets[1].label.as_deref(), Some("example.invalid"));
    assert_eq!(assets[1].company_id, None);

    let Rows::Findings(findings) = store.browse(View::Findings, None, 10).await.unwrap().rows
    else {
        panic!("wrong projection");
    };
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].name, "Historical label");
    assert_eq!(findings[0].task_id, None);
    assert_eq!(findings[1].task_id, Some(3000000011));

    let Rows::Sessions(sessions) = store.browse(View::Sessions, None, 10).await.unwrap().rows
    else {
        panic!("wrong projection");
    };
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, 3000000051);

    let Rows::Approvals(approvals) = store.browse(View::Approvals, None, 10).await.unwrap().rows
    else {
        panic!("wrong projection");
    };
    assert_eq!(approvals.len(), 1);
    assert_eq!(approvals[0].id, 3000000061);
    assert_eq!(approvals[0].task_ref, None);

    let Rows::TrafficEvidence(evidence) = store
        .browse(View::TrafficEvidence, None, 10)
        .await
        .unwrap()
        .rows
    else {
        panic!("wrong projection");
    };
    assert_eq!(evidence.len(), 1);
    assert_eq!(evidence[0].id, 3000000071);
    assert_eq!(evidence[0].finding_id, 3000000041);
    assert_eq!(evidence[0].request_bytes, 8);
    assert_eq!(evidence[0].response_bytes, 16);

    let task_view = View::Exploration {
        task_id: 3000000011,
    };
    let nodes = store.browse(task_view, None, 1).await.unwrap();
    assert_eq!(nodes.next_before, Some(3000000032));
    let Rows::Exploration(nodes) = nodes.rows else {
        panic!("wrong projection");
    };
    assert_eq!(nodes.len(), 1);
    assert_eq!(nodes[0].id, 3000000032);
    assert_eq!(nodes[0].priority, 2);

    assert!(matches!(
        store.browse(View::Tasks, Some(0), 1).await,
        Err(StoreError::InvalidCursor)
    ));
    assert!(matches!(
        store.browse(View::Tasks, None, MAX_PAGE_SIZE + 1).await,
        Err(StoreError::InvalidLimit)
    ));
    assert!(matches!(
        store
            .browse(View::Exploration { task_id: -1 }, None, 1)
            .await,
        Err(StoreError::InvalidTask)
    ));
}
