# Running the Rust inspection console alongside ARTEX

The `rust` branch introduces a PostgreSQL inspection console. Treat the Go application at `da50908c73b2b97b1a8dec843d3475e03780e1d4` as the authority for active deployments and writes. Its architecture and source paths are listed in the [compatibility matrix](compatibility.md).

## Isolate the database and process

1. Keep the legacy source, database connection and filesystem data available. Provision a disposable PostgreSQL database and restore a sanitized backup or create synthetic records there. An existing database can be inspected with a separate least-privilege login that has `CONNECT`, schema `USAGE` and `SELECT` on the specific relations used by the Rust reader. Keep schema ownership and write privileges with the legacy application.
2. Use a separate private TOML configuration for the Rust process. Restrict the file and any referenced credential material to the service user. Run the daemon and terminal client under the same Unix identity in a private runtime directory.
3. Check representative task, asset and finding IDs against the disposable database. Compare deleted-row visibility, sort order and empty results. Test the daemon and terminal client independently and terminate each in turn. Keep the original PostgreSQL and filesystem data available throughout these checks.

The inspection console reads existing PostgreSQL records. It performs no import, ownership transfer or schema conversion. Run migration and restore exercises against disposable databases only. PostgreSQL backups contain sensitive material and must be stored and transferred under the same protections as the source database.

## Connect the terminal client

Copy `config.rust.example.toml` to a private file with mode `0600`. Set `[database].dsn` to the PostgreSQL URI for the inspection login. Set `[local].socket` to an absolute path inside an existing directory owned by the service user with mode `0700`. Both processes use that same pathname. `--config` takes precedence over `ARTEX_CONFIG`, followed by `config.rust.toml` in the working directory. `ARTEX_PG_DSN` overrides the file's connection string. The TUI can use a configuration containing only `[local]`. Store any credential-bearing URI outside the checkout.

```toml
[database]
dsn = "postgresql://arustex_reader@127.0.0.1/arustex_fixture"

[local]
socket = "/run/user/1000/arustex-inspector/arustex.sock"
```

Replace the sample database name, login, and numeric user directory with the values for your fixture. Validate the chosen settings, then start the daemon. Open the terminal client in a separate terminal using the same Linux user and config.

```sh
cargo run --locked --bin arustex -- --config /path/to/inspector.toml config validate
cargo run --locked --bin arustex -- --config /path/to/inspector.toml daemon
```

```sh
cargo run --locked --bin arustex -- --config /path/to/inspector.toml tui
```

## Verification commands

From the Rust workspace root:

```sh
rustc --version
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --workspace
cargo build --locked --workspace
```

Run PostgreSQL integration tests against a disposable cluster. The fixture creates a cluster-wide test role and inserts synthetic records. Apply both files as an administrator of that cluster and point the test at its read-only fixture login.

```sh
psql "$FIXTURE_ADMIN_DSN" -v ON_ERROR_STOP=1 -f db/schema.sql
psql "$FIXTURE_ADMIN_DSN" -v ON_ERROR_STOP=1 -f crates/arustex-store/tests/legacy_fixture.sql
ARUSTEX_TEST_DATABASE_URL="$FIXTURE_READER_DSN" \
    cargo test --locked -p arustex-store --test legacy_read -- --ignored
```

Repeat the workspace checks on Linux x86_64 and Linux aarch64. The verification scope is the read-only console and its documented projections.

## Source material for a future cutover

PostgreSQL holds task and graph state, configurations and provider profiles, sessions and activities, controls and approval records, findings, notification records and evidence bindings. The legacy Go application also uses SQLite for traffic indexing and the filesystem for traffic bodies, evidence, workspace files, archive packages, skills and the JWT signing key. See `db/schema.sql`, `traffic/traffic.go`, `evidence/store.go`, `server/task_archive_package.go` and `server/auth.go` in the pinned source.

An eventual migration would require a consistent snapshot of these sources plus verified ID and relationship preservation, record counts, and payload integrity. Bulk importing an existing production database, migrating secret material, and consolidating SQLite and filesystem stores into PostgreSQL are outside the current console. Keep backups of both PostgreSQL and the legacy filesystem for any deployment that uses those features.

## Rollback

Stop the Rust daemon with ordinary Linux process management and close any clients. Revoke the separate inspection login or its read grants if they were created for the trial. The legacy application uses its original datastore and config throughout, so removing the inspection process requires no database restore. Test restoration from backup separately before using it as a recovery option for a future write-enabled cutover.

The legacy source remains on `main` and the pinned local `legacy/go-nextjs` reference. The [root AGPL-3.0 license](../LICENSE) and [frontend MIT license and notices](../web/LICENSE) remain in effect. ARTEX-Desktop supplied design context. Its application code and fixtures were not imported.
