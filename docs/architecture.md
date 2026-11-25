# Architecture

ARUSTEX is a single Linux binary, `arustex`, built from a Rust 2024 Cargo workspace.
It runs as a long-lived **daemon** that owns the database, the scheduler and all
external connections, and a **terminal interface** (TUI) that talks to the daemon
over a Unix domain socket. Administrative commands (`config`, `db`, `import`,
`doctor`) run in the same binary without the daemon.

The structure follows the layering of ARTEX-Desktop (domain core, ports, storage
adapter, runtime, composition root, presentation), translated to a Linux service
and a terminal client.

```
            ┌───────────────────────────── arustex (apps/arustex) ─────────────────────────────┐
            │  tui ──► arustex-tui ──(Unix socket, JSON lines)──► daemon ──► arustex-app        │
            │  config / db / import / doctor ───────────────────────────────► arustex-app       │
            └───────────────────────────────────────────────────────────────────────────────────┘
arustex-app ─┬─► arustex-runtime ─┬─► arustex-llm ─────► configured model providers (HTTPS)
             │                    └─► arustex-operations ─► arustex-policy
             ├─► arustex-store ─────► PostgreSQL
             └─► notify ────────────► Telegram Bot API (only when enabled)
all crates ──► arustex-core (domain types, errors, events, IPC messages, ports)
```

## Crates

| Crate | Responsibility |
| --- | --- |
| `arustex-core` | Domain model (tasks, work items, sessions, datasets, records, findings, approvals, reports, notifications), errors, domain events, the versioned IPC protocol, storage and record ports, `Secret`. No I/O. |
| `arustex-policy` | Capability policy for model-selected operations. Fails closed. |
| `arustex-operations` | The static operation registry: nine compiled-in operations with typed inputs, timeouts and output caps. |
| `arustex-store` | PostgreSQL adapter (sqlx): migrations, repositories, the event log, attachments, backup/restore, the allowlisted legacy importer. |
| `arustex-llm` | Model providers: Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, and a deterministic mock. Streaming, retries, rate limits, context-window accounting. |
| `arustex-runtime` | Agent runner (tool loop, transcripts, token budgets) and the scheduler (leases, retries, cancellation, planner/worker/reviewer pipeline). |
| `arustex-app` | Composition root: TOML configuration and secrets, the IPC server and client, request handling, record import, reports, the Telegram notifier, daemon bootstrap. |
| `arustex-tui` | Ratatui/Crossterm terminal interface. |
| `apps/arustex` | The `arustex` binary and its command line. |
| `tests` | Cross-crate contract tests and synthetic fixtures. |

## Process model

- **Daemon** (`arustex daemon`). Connects to PostgreSQL, refuses to start against an
  unmigrated schema, builds the provider router from configuration, starts the IPC
  server, the scheduler, the notifier and an hourly retention pass. SIGINT/SIGTERM
  cancel a shared token; every loop drains and the socket is removed on exit.
  It is meant to run under a systemd user unit or in the foreground.
- **TUI** (`arustex` or `arustex tui`). Connects to the socket, takes snapshots by
  request and follows a live event stream. It holds no database connection and no
  secrets.
- **Administrative commands** connect to PostgreSQL directly and exit.

## Storage

PostgreSQL is the only datastore. Schema changes are versioned SQL files in
`migrations/`, embedded in the binary and applied only by `arustex db migrate`; each
applied file's checksum is recorded and a later change to it is refused. All state changes append
to an `events` table with a monotonically increasing sequence number, written in
the same transaction as the change. Large payloads go to a content-addressed
attachment store (chunked, SHA-256 verified on read) with size limits and garbage
collection.

Backups (`arustex db backup DIR`) are logical, per-table JSON lines with a manifest
of row counts and SHA-256 checksums; `db verify-backup` checks them offline and
`db restore` refuses to run against a database that already holds data.

## Local IPC

- Socket: `$XDG_RUNTIME_DIR/arustex/arustex.sock` by default. The directory must be
  owned by the daemon's user and not group/world writable (it is created `0700`); the
  socket is `0600`.
- Peer credentials (`SO_PEERCRED`) are checked on every connection: only the socket
  owner's uid is accepted. The client refuses a socket owned by another uid.
- Framing: one JSON object per line, at most 1 MiB. Every frame carries the protocol
  version (`v`, currently 1) and a frame with another version is refused. Clients
  open with `hello`, which returns the daemon's version and protocol.
- Requests carry a client-chosen `id`; the response echoes it. Each request has a
  server-side timeout.
- Events carry the persisted sequence number. `subscribe {after_seq}` replays the
  gap from the event log in pages and then follows live events, deduplicated by
  sequence. Per-client queues are bounded: a slow reader of responses is
  disconnected, and a subscriber whose event queue fills receives `resync
  {latest_seq}` and must take a fresh snapshot. Streaming model output
  (`output_delta`) is transient and carries sequence 0.
- The TUI reconnects with exponential backoff (0.5 s to 10 s) and resubscribes from
  the last sequence it applied.

The golden frames in `tests/contracts/ipc_v1.jsonl` pin the version-1 wire format.

## Workflow

1. An operator creates a task with an objective and a set of approved datasets.
2. The scheduler splits the task's records into work items in batches. If enabled, the
   **planner** agent can suggest which records to look at first; every record is still
   planned exactly once.
3. **Worker** agents run each item under a lease. They can read records in the
   task's approved datasets and propose findings; every finding starts as `pending`.
4. If enabled, the **reviewer** agent compares worker output with the item's
   expectations and records a verdict (`match`, `mismatch`, `inconclusive`).
5. The task moves to `needs_review` or `completed`. A report with provenance is
   generated, and a notification is queued if Telegram is enabled.
6. The **assistant** agent answers operator questions in the TUI. It can propose report
   drafts and status changes; status changes become approval requests.

Every model-selected action goes through the operation registry and the policy (see
[operation-boundaries.md](operation-boundaries.md)).

## External communication

Only three kinds of outbound connection exist:

1. the configured model providers (HTTPS, or HTTP to a loopback host);
2. PostgreSQL;
3. the Telegram Bot API, when the operator enables it, to one configured chat.

None of them is reachable from an operation. Providers receive tool definitions and
scoped, redacted record excerpts; they never receive configuration or secrets.

## Configuration and secrets

Configuration is TOML (see `config.example.toml`), looked up via `--config`,
`$ARUSTEX_CONFIG`, `$XDG_CONFIG_HOME/arustex/config.toml`, then
`/etc/arustex/config.toml`. Unknown keys are rejected. Secrets are never written in
the file: each names an environment variable or a file that must be owned by the
user and not readable by group or others. Secrets are held in a `Secret` type that
never prints its value, and `config show` redacts them.
