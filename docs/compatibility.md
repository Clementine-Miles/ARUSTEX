# Compatibility and migration matrix

The Go and Next.js baseline is `da50908c73b2b97b1a8dec843d3475e03780e1d4`. The local `legacy/go-nextjs` reference pins that commit. The Rust branch starts from the same snapshot and retains the original code while the read-only inspection console is verified independently.

The ARTEX-Desktop reference was reviewed at `ad9a20c1f1ea46580b6624ebd40c192a2d388569`. Its workspace separates domain records, adapters, and contracts. Its native interface and bundled SQLite store have different deployment requirements. The Rust console uses its own contracts and PostgreSQL queries. See [ARTEX-Desktop](https://github.com/froiibn/ARTEX-Desktop/tree/ad9a20c1f1ea46580b6624ebd40c192a2d388569/desktop).

## Source-to-module matrix

The Rust coverage column describes the inspection feature within each domain. Legacy tests identify behavior to preserve and establish no Rust parity by themselves.

| Domain | Go and frontend baseline | Rust module and dependency | Compatibility checks | Rust coverage |
| --- | --- | --- | --- | --- |
| Entrypoints and process lifecycle | `cmd/artex/main.go`, `server/manager.go`, `start.sh`, `build.sh`, `.github/workflows/source-build.yml` | `apps/arustex`, `arustex-app`, `arustex-tui` | Source build, CLI dispatch, separate daemon and terminal lifetimes | Inspection process and terminal client |
| Configuration | `config/config.go`, `config.example.toml` and `config/config_test.go` | `arustex-app`, Serde and TOML | Precedence, bounded inputs, invalid settings, and credential-safe errors | Settings for the inspector. Legacy execution settings stay with Go. |
| Tasks | `db/tasks.go`, `db/task_categories.go`, `server/task_control.go` | `arustex-core` read models, `arustex-store` with SQLx | Preserve IDs, states, ordering and soft-delete semantics in read fixtures. Legacy `db/tasks_test.go` covers mutation. | Existing summaries. Creation, mutation, and scheduling remain legacy. |
| Global asset graph | `db/assets.go`, `db/companies.go`, `db/task_scope.go`, `db/task_assets.go`, `server/assets.go` | `arustex-core` read models and `arustex-store` | Check asset identities, nullable company IDs, and omitted URL data. Legacy `db/assets_test.go` and `db/company_scope_consistency_test.go` cover deduplication and scope consistency. | Asset summaries. Scope evaluation, source links, deduplication, and graph updates remain legacy. |
| Task exploration graph | `db/schema.sql`, `db/exploration*.go`, `db/task_assets_context.go`, `server/engine.go` | `arustex-core` and `arustex-store` read boundary | Check node identities and task association in a fixture that also includes edges and anchors. Legacy `db/exploration_sources_test.go` covers source inheritance. | Task-scoped node summaries. Edges, anchors, full graph traversal, and coverage calculations remain legacy. |
| Findings and evidence records | `db/findings.go`, `db/finding_retests.go`, `db/finding_traffic.go`, `evidence/store.go` | `arustex-store` for read projections | Preserve finding IDs, task association, severity and row ordering. Legacy `db/findings_test.go` and `evidence/store_test.go` cover history and evidence binding. | Finding inspection. Filesystem evidence, binary payloads, and retest history migration remain pending. |
| Sessions and approvals | `db/schema.sql`, `db/conversation.go`, `intercept/`, `guard/`, `server/auth.go` | `arustex-app` typed local IPC and `arustex-tui` | Verify socket ownership and permissions, peer identity, bounded IPC, and terminal restoration. Legacy `server/auth_test.go` covers failures in the HTTP authentication path. | The local read protocol uses a separate trust boundary. Existing HTTP, authorization records, and approval decisions remain legacy. |
| Scheduling, agent execution, and tool integrations | `server/engine.go`, `server/manager.go`, `agent/`, `mcphttp/`, `skills/`, Norma dependency in `go.mod` | Outside the Rust inspection console | No parity gate claimed | Legacy code and data stay in place. |
| Traffic, logs, reporting, transfers, and notifications | `traffic/`, `evidence/`, `report/`, `notify/`, `server/task_archives*.go`, `server/sync_scopesentry.go` | PostgreSQL inspection boundary only | Legacy `traffic/archive_test.go`, `evidence/store_test.go`, and `notify/http_test.go` cover independent behavior. | Capture, export, ingestion, retention and delivery remain legacy. |

## Persistent identities

The Go implementation has two related graphs. Assets and organizations are shared across tasks. The per-task exploration graph has its own nodes and edges, with anchors linking records to an asset ID. `db/schema.sql` defines `companies`, `assets`, `company_scope`, `task_scope`, `task_asset_links`, `explorations`, `exploration_nodes`, `exploration_edges`, `exploration_anchors`, and `activity`. Task membership, ownership and graph links have distinct meanings. A future importer must preserve this distinction and validate referential integrity, ordering and ancestry before the source can be retired.

See [migration and rollback](migration.md) for the backing stores and acceptance checks.

## Verified inspection coverage

- Rust 1.100.0 built the workspace and passed formatting, strict Clippy, and 13 workspace tests. Tests ran on native Linux x86_64 and under QEMU on Linux aarch64. The repository includes a workflow for native builds on both architectures.
- On a disposable PostgreSQL 17 cluster, the legacy schema applied twice. The separately enabled fixture test passed on both the original synthetic data and its restored backup. The restricted role could read all seven projections and PostgreSQL rejected writes and reads of provider credentials.
- The real daemon served the fixture through same-user IPC. The terminal client rendered data and restored the terminal on exit while the daemon kept responding. Graceful shutdown cleared the socket.
- Both Linux dependency graphs used the same PostgreSQL-only SQLx feature set. Cargo-audit 0.23.0 reported zero known vulnerability matches or warnings against RustSec database commit `10db6998b5d74ec438bcf0763125bd425a068a8f`.
