# Operation boundaries

Agents act only through operations compiled into `arustex-operations`. Nothing a
model says can add an operation, change one, or reach anything an operation cannot
reach.

## Scope

ARUSTEX reviews **operator-imported records** with model assistance. It does not
assess live systems. The rewrite deliberately does **not** implement, and does not
migrate from the legacy code:

- autonomous assessment of live systems, active probing or scanning;
- traffic recording, interception, replay or proxying;
- credential collection, exploitation or payload generation;
- arbitrary command, shell or PTY execution;
- generic HTTP fetching, browser automation, or any network access from an operation;
- MCP servers, external command adapters, dynamically loaded code, skills or plugin
  protocols;
- filesystem paths or external destinations chosen by a model.

Demonstrations and tests use synthetic fixtures (`tests/fixtures/`) and records an
operator explicitly imports.

## The operations

| Operation | Capability | Effect | Timeout | Output cap |
| --- | --- | --- | --- | --- |
| `search_records` | `read_records` | read-only | 10 s | 32 KiB |
| `read_records` | `read_records` | read-only | 10 s | 48 KiB |
| `summarize_scope` | `compute_summary` | read-only | 10 s | 8 KiB |
| `field_statistics` | `compute_summary` | read-only | 15 s | 16 KiB |
| `validate_graph` | `validate_graph` | read-only | 15 s | 16 KiB |
| `compare_outputs` | `compare_outputs` | read-only | 5 s | 32 KiB |
| `record_finding` | `propose_finding` | proposal | 10 s | 4 KiB |
| `propose_report_draft` | `propose_report` | proposal | 10 s | 4 KiB |
| `propose_status_update` | `propose_update` | approval required | 10 s | 4 KiB |

- **read-only**: reads records in the session's approved datasets, or the task's graph.
- **proposal**: stores something that starts as pending (a finding) or labelled
  model-generated (a report draft). An operator confirms or dismisses it.
- **approval required**: only creates an approval request. The change is applied by
  the daemon after an operator approves it in the Approvals view, and is checked
  again then (for example, that the finding belongs to the same task).

`tests/integration/contracts.rs` fails if an operation is added or removed, or if an
input schema accepts unknown fields or has a parameter named like a command, shell,
URL, path, host or exec.

## Policy

`arustex-policy` decides every call. It fails closed.

| Role | Granted capabilities |
| --- | --- |
| planner | `read_records`, `read_graph`, `compute_summary`, `validate_graph` |
| worker | `read_records`, `compute_summary`, `propose_finding` |
| reviewer | `read_records`, `compare_outputs`, `compute_summary` |
| assistant | `read_records`, `read_graph`, `compute_summary`, `validate_graph`, `propose_report`, `propose_update` |

A call is denied when:

- the operation name is unknown;
- the role is not granted the operation's capability;
- the operation is disabled;
- the session has used its call budget (64 calls by default);
- the input is larger than 64 KiB or does not match the typed input (unknown fields
  are rejected);
- a record or dataset outside the task's approved datasets is referenced.

Policies can only be narrowed in code (`revoke`, `disable`); there is no way to grant
a role more than the defaults. Operations run under their timeout, and output past
the cap is truncated and marked.

## What reaches a model

Only tool definitions, the task's objective, and record excerpts from the approved
datasets. Excerpts are size-limited and redacted at import unless the operator
imported with `--no-redact`. Record text is passed as quoted data with an
instruction that it is not to be followed. A model never receives configuration,
secrets, socket paths or database details.

## Notifications

Telegram is the only notifier and is disabled by default. When enabled it sends to
the single chat in the configuration. Agents cannot choose a destination or trigger
a message directly. Messages contain a task's id, name, status and report number, or an
approval request's operation name; imported record contents are never sent. The bot
token never appears in logs or errors.
