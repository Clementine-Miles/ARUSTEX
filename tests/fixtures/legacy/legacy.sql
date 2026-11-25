-- Trimmed legacy (Go-era) schema with synthetic data, used to test the allowlisted
-- importer. Every value is invented; names use reserved example domains.

CREATE TABLE companies (
    id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL, nkey TEXT NOT NULL UNIQUE, logo TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now());

CREATE TABLE assets (
    id BIGSERIAL PRIMARY KEY, type TEXT NOT NULL, company_id BIGINT REFERENCES companies(id),
    company_source TEXT NOT NULL DEFAULT 'explicit', task_ids BIGINT[] NOT NULL DEFAULT '{}',
    domain TEXT, root_domain TEXT, ip TEXT, port INTEGER, bundle_id TEXT, app_name TEXT, url TEXT,
    service_type TEXT, service_name TEXT, method TEXT,
    auth JSONB[] NOT NULL DEFAULT '{}', params JSONB[] NOT NULL DEFAULT '{}',
    extra JSONB NOT NULL DEFAULT '{}', open_ports JSONB[] NOT NULL DEFAULT '{}',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), last_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now());

CREATE TABLE explorations (
    id BIGSERIAL PRIMARY KEY, description TEXT, goal TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open', round_no BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now());

CREATE TABLE exploration_nodes (
    id BIGSERIAL PRIMARY KEY, exploration_id BIGINT NOT NULL REFERENCES explorations(id),
    kind TEXT NOT NULL, payload JSONB NOT NULL DEFAULT '{}', priority INT NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'open', origin TEXT, owner TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ);

CREATE TABLE exploration_edges (
    exploration_id BIGINT NOT NULL, src_id BIGINT NOT NULL, dst_id BIGINT NOT NULL, rel TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (exploration_id, src_id, rel, dst_id));

CREATE TABLE exploration_anchors (node_id BIGINT NOT NULL, asset_id BIGINT NOT NULL, PRIMARY KEY (node_id, asset_id));

CREATE TABLE llm_profiles (
    id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL UNIQUE, format TEXT NOT NULL, base_url TEXT,
    proxy TEXT, model TEXT NOT NULL, api_key TEXT);

CREATE TABLE tasks (
    id BIGSERIAL PRIMARY KEY, name TEXT NOT NULL DEFAULT '', description TEXT NOT NULL, goal TEXT NOT NULL,
    exploration_id BIGINT NOT NULL UNIQUE REFERENCES explorations(id),
    status TEXT NOT NULL DEFAULT 'created', paused BOOLEAN NOT NULL DEFAULT false,
    timeout_seconds INTEGER NOT NULL DEFAULT 0, deleted_at TIMESTAMPTZ, completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(), updated_at TIMESTAMPTZ NOT NULL DEFAULT now());

CREATE TABLE findings (
    id BIGSERIAL PRIMARY KEY, task_id BIGINT, node_id BIGINT, vulnclass TEXT NOT NULL DEFAULT '',
    name TEXT NOT NULL DEFAULT '', severity TEXT NOT NULL DEFAULT '', summary TEXT NOT NULL DEFAULT '',
    evidence TEXT NOT NULL DEFAULT '', worker TEXT NOT NULL DEFAULT '', asset_ids JSONB NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'pending', report TEXT NOT NULL DEFAULT '',
    created_at TIMESTAMPTZ NOT NULL DEFAULT now());

-- Excluded tables (must never be read into the new schema).
CREATE TABLE agents (id BIGSERIAL PRIMARY KEY, name TEXT, system_prompt TEXT);
CREATE TABLE activity (id BIGSERIAL PRIMARY KEY, body TEXT);
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE notification_channels (id BIGSERIAL PRIMARY KEY, token TEXT);

INSERT INTO companies (id, name, nkey) VALUES (1, 'Example Org', 'example-org'), (2, 'Sample Co', 'sample-co');
INSERT INTO assets (id, type, company_id, domain, root_domain, auth, extra) VALUES
    (10, 'root_domain', 1, 'example.org', 'example.org', ARRAY['{"user":"SYNTHETIC-AUTH"}'::jsonb], '{"note":"SYNTHETIC-EXTRA"}'),
    (11, 'subdomain', 1, 'www.example.org', 'example.org', '{}', '{}');
INSERT INTO assets (id, type, company_id, url, method, params) VALUES
    (12, 'endpoint', 2, 'https://sample.example/api', 'GET', ARRAY['{"q":"SYNTHETIC-PARAM"}'::jsonb]);

INSERT INTO explorations (id, goal, status) VALUES (100, 'Synthetic objective A', 'open'), (101, 'Deleted task objective', 'open');
INSERT INTO exploration_nodes (id, exploration_id, kind, state, payload) VALUES
    (1000, 100, 'begin', 'open', '{}'),
    (1001, 100, 'goal', 'open', '{"text":"SYNTHETIC-GOAL-PAYLOAD"}'),
    (1002, 100, 'intent', 'done', '{"plan":"SYNTHETIC-OPERATIONAL-INSTRUCTION"}'),
    (1003, 100, 'intent', 'running', '{}'),
    (1004, 100, 'fact', 'confirmed', '{"output":"SYNTHETIC-CAPTURED-OUTPUT"}'),
    (1100, 101, 'begin', 'open', '{}');
INSERT INTO exploration_edges (exploration_id, src_id, dst_id, rel) VALUES
    (100, 1001, 1000, 'derived_from'),
    (100, 1000, 1002, 'spawns'),
    (100, 1000, 1003, 'spawns'),
    (100, 1001, 1003, 'spawns'),
    (100, 1002, 1004, 'yields');
INSERT INTO exploration_anchors (node_id, asset_id) VALUES (1002, 10), (1004, 11);

INSERT INTO tasks (id, name, description, goal, exploration_id, status) VALUES
    (7, 'Synthetic task', 'desc', 'Synthetic objective A', 100, 'running');
INSERT INTO tasks (id, name, description, goal, exploration_id, status, deleted_at) VALUES
    (8, 'Deleted', 'desc', 'Deleted task objective', 101, 'done', now());

INSERT INTO findings (id, task_id, node_id, vulnclass, name, severity, summary, evidence, report, asset_ids, status) VALUES
    (50, 7, 1004, 'misconfiguration', 'Synthetic finding', 'HIGH',
     'Summary mentions admin@example.org and token=SYNTHETICSECRET123',
     'SYNTHETIC-EVIDENCE', 'SYNTHETIC-REPORT', '[10, 999]', 'false_positive'),
    (51, 8, NULL, 'info-leak', '', '', 'orphaned', '', '', '[]', 'pending');

INSERT INTO llm_profiles (name, format, base_url, proxy, model, api_key) VALUES
    ('primary', 'anthropic', NULL, 'http://SYNTHETIC-PROXY', 'model-a', 'SYNTHETIC-API-KEY');

INSERT INTO agents (name, system_prompt) VALUES ('worker', 'SYNTHETIC-PROMPT');
INSERT INTO activity (body) VALUES ('SYNTHETIC-TRANSCRIPT');
INSERT INTO settings VALUES ('telegram_token', 'SYNTHETIC-TOKEN');
INSERT INTO notification_channels (token) VALUES ('SYNTHETIC-CHANNEL-TOKEN');
