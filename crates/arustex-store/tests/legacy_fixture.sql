-- Apply after db/schema.sql to a disposable PostgreSQL cluster only.
BEGIN;

INSERT INTO explorations (id, goal)
VALUES (3000000001, 'Synthetic case'), (3000000002, 'Archived case'), (3000000003, 'Deleted case');

INSERT INTO tasks (id, name, description, goal, exploration_id, archived_at, deleted_at)
VALUES
    (3000000011, 'Test case', '', 'Synthetic case', 3000000001, NULL, NULL),
    (3000000012, repeat('Ω', 400), '', 'Archived case', 3000000002, now(), NULL),
    (3000000013, 'Deleted case', '', 'Deleted case', 3000000003, NULL, now());

INSERT INTO assets (id, type, domain, url, task_ids)
VALUES
    (3000000021, 'root_domain', 'example.invalid', NULL, ARRAY[3000000011]::bigint[]),
    (3000000022, 'service', NULL, 'https://example.invalid/?token=synthetic-neutral-fixture', '{}');

INSERT INTO exploration_nodes (id, exploration_id, kind, state, priority)
VALUES
    (3000000031, 3000000001, 'goal', 'open', 3),
    (3000000032, 3000000001, 'fact', 'confirmed', 2),
    (3000000033, 3000000002, 'goal', 'open', 1);

INSERT INTO exploration_edges (exploration_id, src_id, dst_id, rel)
VALUES (3000000001, 3000000032, 3000000031, 'proves');

INSERT INTO exploration_anchors (node_id, asset_id) VALUES (3000000032, 3000000021);

UPDATE task_asset_links SET source_node_id = 3000000032
WHERE task_id = 3000000011 AND asset_id = 3000000021;

INSERT INTO findings (id, task_id, node_id, vulnclass, name, severity)
VALUES
    (3000000041, 3000000011, 3000000032, '', 'Synthetic observation', 'informational'),
    (3000000042, NULL, NULL, 'Historical label', '', 'low');

INSERT INTO conversations (id, agent_key, title)
VALUES (3000000051, 'fixture', 'Recorded test session');

INSERT INTO intercept_pending (id, conversation_id, task_id, tool_name)
VALUES (3000000061, 3000000051, NULL, 'synthetic fixture');

INSERT INTO traffic_evidence_snapshots (id, source_traffic_id, captured_at,
    url, method, status, req_head, resp_head, req_hash, resp_hash, req_len, resp_len)
VALUES ('test-snapshot', 'test-source', 0, 'https://example.invalid/', 'GET', 200,
    'synthetic-private-header', '', '', '', 8, 16);

INSERT INTO finding_traffic_bindings (id, finding_id, snapshot_id, position)
VALUES (3000000071, 3000000041, 'test-snapshot', 0);

CREATE ROLE arustex_fixture_observer LOGIN;
GRANT USAGE ON SCHEMA public TO arustex_fixture_observer;
GRANT SELECT (id, name, status, exploration_id, archived_at, deleted_at)
    ON tasks TO arustex_fixture_observer;
GRANT SELECT (id, type, company_id, domain, ip, app_name)
    ON assets TO arustex_fixture_observer;
GRANT SELECT (id, task_id, name, vulnclass, severity, status)
    ON findings TO arustex_fixture_observer;
GRANT SELECT (id, agent_key, title) ON conversations TO arustex_fixture_observer;
GRANT SELECT (id, task_id, status) ON intercept_pending TO arustex_fixture_observer;
GRANT SELECT (id, finding_id, snapshot_id) ON finding_traffic_bindings TO arustex_fixture_observer;
GRANT SELECT (id, method, status, req_len, resp_len)
    ON traffic_evidence_snapshots TO arustex_fixture_observer;
GRANT SELECT (id, kind, state, priority, exploration_id)
    ON exploration_nodes TO arustex_fixture_observer;

COMMIT;
