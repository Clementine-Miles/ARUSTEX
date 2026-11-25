# Rust inspection security

The daemon serves only read-only queries over a Linux Unix socket. Use an
existing runtime directory owned by the daemon's effective UID and private to
that user. Place it under a trusted path such as `$XDG_RUNTIME_DIR` whose
ancestors cannot be replaced by other users. The daemon creates the socket
with mode 0600. Both sides verify the effective UID of their peers with Linux
peer credentials. Any process running
under that UID can access the console. Run the daemon under the account you
intend to authorize for local database inspection.

The socket protocol uses length-prefixed frames, request size and duration
limits, and a finite number of concurrent handlers. Invalid frames, unknown
requests, and unsupported protocol versions produce a generic error. Routine
responses omit credentials, command arguments, transcripts, report bodies,
and raw traffic. Treat report titles, asset labels, and other projected
metadata as sensitive operator data. The TUI sanitizes control characters
before rendering values from the database.

Use a dedicated read-only PostgreSQL login even though SQLx sessions default
to read-only transactions. The SQL adapter uses parameterized queries,
schema-qualified relations, capped projections, and database statement
timeouts. It never invokes the legacy application's schema setup or seeds.
Non-loopback database URIs require certificate and hostname validation with
`sslmode=verify-full`. Keep any file containing the database DSN owner-only.
Credentials can also be passed through the daemon's process environment.

A graceful stop removes the socket if it still belongs to this daemon.
After an ungraceful stop, confirm the daemon has exited before removing its
stale socket. If an existing process owns the socket, stop it through its
normal supervisor or start the new process on a separate socket.

For setup, testing, and the legacy data boundaries, see the [runbook](migration.md).
