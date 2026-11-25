# Compatibility and known limits

## Platform

- Linux only. The daemon relies on Unix domain sockets with `SO_PEERCRED`, and the
  client checks socket ownership through `/proc/self`.
- Release targets: `x86_64-unknown-linux-gnu` and `aarch64-unknown-linux-gnu`.
- Rust 1.100.0 (pinned in `rust-toolchain.toml`).
- PostgreSQL 16 is what the test suite runs against. Older versions are untested.
- Terminals: the TUI uses only Crossterm's portable escape sequences, honours
  `NO_COLOR`, and restores the terminal on exit, panic,
  SIGTERM and SIGHUP (for example a dropped SSH connection). It has been tested with
  Ratatui's test backend and in a pseudo-terminal; it has not yet been tried by hand
  in foot or zellij.

## Compatibility with the legacy implementation

- There is no web interface and no HTTP API. Scripts that called the legacy REST
  endpoints must use the CLI or the IPC protocol instead.
- The database schema is new. Legacy databases are not upgraded in place; data moves
  through `arustex import legacy` (see [migration.md](migration.md)), which brings
  over organizations, asset identities, task and graph structure, and finding
  summaries only.
- Legacy configuration files and environment variables are not read.
- Legacy agents, prompts, tools, skills, MCP servers, intercept rules and traffic
  evidence have no equivalent and are not imported.
- Notification channels other than Telegram are gone.
- Finding export is JSON and Markdown (`arustex.report.v1`); CSV export is gone.

## Protocol and data formats

- IPC protocol version 1. A client and daemon with different versions refuse each
  other; there is no negotiation yet.
- Report format `arustex.report.v1`. New fields may be added; readers must ignore
  unknown fields.
- Backups are tied to the schema version they were taken at: restore into a database
  migrated to the same version.

## Known limits and open defects

- If a work item is retried after a worker already recorded findings, the retry can
  record the same finding again. Operators can mark duplicates (`u` in the Reviews
  view).
- Assets come only from the legacy import; there is no asset import or editor yet.
- A session interrupted by a daemon crash resumes only after its lease expires
  (`scheduler.lease_secs`, 120 s by default).
- Operation timeouts and output caps are fixed per operation; they are not
  configurable.
- The policy cannot be narrowed from configuration yet; agents can be disabled
  per role (`[[agents]] enabled = false`).
- Re-importing the same JSON Lines file is idempotent, but the summary still counts
  redactions for lines that were skipped as duplicates.
- There is no automatic failover between model providers.
