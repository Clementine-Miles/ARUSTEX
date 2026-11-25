-- ARUSTEX schema, part 2: prompt templates, workflow sessions, transcripts and usage.

CREATE TABLE prompt_templates (
    key        TEXT NOT NULL CHECK (key ~ '^[a-z][a-z0-9_.]*$'),
    version    INTEGER NOT NULL CHECK (version >= 1),
    body       TEXT NOT NULL CHECK (length(body) BETWEEN 1 AND 65536),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (key, version)
);

CREATE TABLE sessions (
    id             UUID PRIMARY KEY,
    task_id        BIGINT REFERENCES tasks(id) ON DELETE CASCADE,
    work_item_id   BIGINT,
    role           TEXT NOT NULL CHECK (role IN ('planner', 'worker', 'assistant', 'reviewer')),
    agent_key      TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending' CHECK (status IN (
                       'pending', 'running', 'awaiting_approval', 'completed',
                       'needs_review', 'failed', 'cancelled')),
    prompt_key     TEXT NOT NULL,
    prompt_version INTEGER NOT NULL,
    model_profile  TEXT NOT NULL,
    model          TEXT NOT NULL,
    token_budget   BIGINT NOT NULL DEFAULT 0 CHECK (token_budget >= 0),
    tokens_used    BIGINT NOT NULL DEFAULT 0 CHECK (tokens_used >= 0),
    turn           INTEGER NOT NULL DEFAULT 0 CHECK (turn >= 0),
    max_turns      INTEGER NOT NULL CHECK (max_turns >= 1),
    outcome        JSONB,
    checkpoint     JSONB,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_sessions_task ON sessions (task_id, created_at DESC);
CREATE INDEX idx_sessions_active ON sessions (status) WHERE status IN ('pending', 'running', 'awaiting_approval');
CREATE TRIGGER trg_sessions_touch BEFORE UPDATE ON sessions
    FOR EACH ROW EXECUTE FUNCTION arustex_touch_updated_at();

CREATE TABLE transcript_entries (
    session_id    UUID NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq           BIGINT NOT NULL CHECK (seq >= 1),
    kind          TEXT NOT NULL CHECK (kind IN
                      ('system', 'user', 'assistant', 'operation_call', 'operation_result', 'note')),
    content       JSONB NOT NULL,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, seq)
);

CREATE TABLE llm_usage (
    id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at                 TIMESTAMPTZ NOT NULL DEFAULT now(),
    session_id         UUID,
    profile            TEXT NOT NULL,
    model              TEXT NOT NULL,
    input_tokens       BIGINT NOT NULL DEFAULT 0,
    output_tokens      BIGINT NOT NULL DEFAULT 0,
    cache_read_tokens  BIGINT NOT NULL DEFAULT 0,
    cache_write_tokens BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX idx_llm_usage_at ON llm_usage (at DESC);
