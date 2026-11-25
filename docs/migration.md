# Migration from the Go/Next.js implementation

This document is the Phase 0 inventory: every component of the legacy
implementation, what replaced it, and what was deliberately left out. The legacy
code remains available in git history (the `rust` branch before the rewrite and the
upstream repository); this tree no longer contains it.

## Legacy inventory and disposition

| Legacy component | What it did | Disposition |
| --- | --- | --- |
| `cmd/artex` | Entry point: HTTP server, engine, password reset | Replaced by `apps/arustex` (`daemon`, `tui`, `config`, `db`, `import`, `doctor`). No web login, so no password reset. |
| `server/` | HTTP API, auth, WebSocket broadcast, chat uploads, MCP/skill wiring | Replaced by the daemon and the Unix-socket IPC in `arustex-app`. Skill and MCP wiring **not migrated**. |
| `web/` | Next.js web interface | Replaced by the terminal interface in `arustex-tui`. |
| `db/` | Schema and repositories (assets, companies, tasks, agents, prompts, tools, intercept rules, conversations, settings, ...) | Replaced by `arustex-store` and the new schema in `migrations/`. Data moves only through the allowlisted legacy import below. |
| `config/`, `.env.example` | Environment and JSON configuration | Replaced by TOML configuration with secrets from environment variables or owner-only files (`config.example.toml`). |
| `agent/` planner, worker, main agent, chat | Multi-agent loop over the norma SDK | Rewritten in `arustex-runtime` as planner, worker, reviewer and assistant agents over the static operation registry. |
| `agent/` tool catalog, tool inserts, capture, retester, prompt catalog | Model-selected tools, request capture, re-testing, operational prompts | **Not migrated.** Tools are replaced by nine data-review operations (see [operation-boundaries.md](operation-boundaries.md)). |
| `skills/` | Skill prompt packs loaded at run time | **Not migrated.** No dynamically loaded instructions or code. |
| `mcphttp/` | MCP over HTTP | **Not migrated.** |
| `intercept/`, `guard/` | Traffic interception rules, pending intercepts, tool-call guard | **Not migrated** (interception). The approval idea survives as operator approvals for `propose_status_update`, and the guard's role is taken by `arustex-policy`. |
| `traffic/`, `evidence/` | Traffic recording and evidence snapshots | **Not migrated.** Records are imported explicitly as JSON Lines. |
| `llmpool/` | Provider failover chain and health | Replaced by `arustex-llm` with named providers, retries and rate limits. Automatic failover between providers is not implemented. |
| `llmrec/` | Raw wire capture of model calls | **Not migrated.** Session transcripts store messages and operation calls, not raw HTTP bodies. |
| `sidequestion/` | Checkpointed side questions to the main agent | Replaced by the assistant agent (ask about a task from the TUI). |
| `notify/` | Notification channels with filters and SSRF checks | Replaced by Telegram only, disabled by default, one fixed chat. |
| `report/` | Markdown and CSV export of findings | Replaced by `arustex.report.v1` reports with provenance ([report-schema.md](report-schema.md)). CSV export is not implemented. |
| `build.sh`, `dev.sh`, `install.sh`, `update.sh`, `start.sh`, `reset-password.sh` | Build, install and service scripts | Replaced by `cargo build --release --locked` and the instructions in the README. |
| `.github/workflows/source-build.yml` | Go and Node build | Replaced by `.github/workflows/ci.yml` (format, lint, tests with PostgreSQL, x86_64 and aarch64 builds). |
| `screenshots/`, `docs/漏洞流量证据.md` | Web screenshots, traffic-evidence feature notes | Removed with the features they describe. |
| `go.mod`, `go.sum` | Go modules | Replaced by `Cargo.toml` and the committed `Cargo.lock`. |

## Importing legacy data

```sh
export ARUSTEX_LEGACY_DATABASE_URL='postgres://reader@legacy-host/artex'
arustex import legacy --dry-run   # counts only, writes nothing
arustex import legacy
```

The legacy database is opened read-only. The import is a single transaction into an
empty ARUSTEX database and refuses to run twice.

**Imported** (allowlist): organizations (`companies`), asset identities (`assets`),
task metadata (name, status, timestamps) with their exploration graph structure
(`explorations`, `exploration_nodes`, `exploration_edges`, `exploration_anchors`), and
finding summaries (`findings`).

- Exploration node payloads are replaced by neutral titles, because legacy payloads
  contain model-written action plans and captured output.
- Finding evidence and detailed reports are dropped; summaries are redacted and
  capped at 4 KiB.
- Every imported row names its legacy table as its source (`legacy:companies`, ...). Organizations and assets are `supplied`; findings are `derived`, because their summaries were rewritten during import.
- Model profiles are counted but not imported: their names are reported so the
  operator can configure providers again; API keys are never copied (only whether one was set is reported).

**Not imported**, with row counts reported: agents, prompts and prompt variables,
tools and tool usage, MCP servers, skills, conversations and activity transcripts,
intercept rules and pending intercepts, traffic evidence and bindings, re-tests,
scheduler state, settings, server logs, side questions, notification channels and
events, task templates and constraints. The exact list is `EXCLUDED_LEGACY_TABLES`
in `crates/arustex-store/src/legacy.rs`.

## Moving an installation

1. Install PostgreSQL 16 or later and create an empty database.
2. Write `~/.config/arustex/config.toml` from `config.example.toml`; put the database
   URL in `ARUSTEX_DATABASE_URL` and provider keys in environment variables or `0600`
   files.
3. `arustex config validate`, then `arustex db migrate`.
4. Optionally `arustex import legacy`.
5. Import records: `arustex import records --file data.jsonl --new-dataset NAME`.
6. Run `arustex daemon` (for example as a systemd user service) and open `arustex`.
7. `arustex doctor` checks configuration, database, schema and daemon reachability.
