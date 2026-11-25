# Rust inspection architecture

The inspection console reads selected records from the legacy PostgreSQL schema.
The Go application still owns task execution and all legacy writes. See the
[compatibility matrix](compatibility.md) for the source baseline and current
coverage.

| Module | Responsibility |
| --- | --- |
| `arustex-core` | Typed summaries with signed 64-bit record IDs, bounded page requests, and versioned IPC messages |
| `arustex-store` | SQLx PostgreSQL pool, read-only session settings, fixed column projections, statement timeouts, and keyset pagination |
| `arustex-app` | Strict TOML, credential-safe configuration errors, and a bounded Unix socket daemon |
| `arustex-tui` | Ratatui input, navigation and rendering, using a request callback supplied by the executable |
| `apps/arustex` | Linux x86_64 and aarch64 executable with daemon, client, diagnostics, and config validation subcommands |

The CLI connects the TUI to a Unix socket. Each request carries a version
number and reads at most one page. The daemon runs independently of the
terminal client. A refresh creates a new connection and uses the same socket
path after a daemon restart. Pages are ordered by descending record ID. Because
the client displays live records, separate pages can reflect updates made by
the legacy application between requests.

The adapter projects metadata from tasks, assets, findings, conversations,
approvals, finding-linked traffic evidence, and exploration nodes within a
selected task. It preserves existing IDs. The graph view is a node list; graph
edges, anchors, and coverage remain in the legacy application. The traffic view
contains findings' evidence links and body sizes, with no raw payloads or
headers. The Rust console creates no database tables or persistent files
during ordinary reads. The Go application's separate persistence remains in
place.

Use a dedicated PostgreSQL account with SELECT access to the projected columns.
The [security notes](security.md) describe the local trust boundaries. The
[runbook](migration.md) covers configuration and disposable database tests.
