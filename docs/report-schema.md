# Report schema (`arustex.report.v1`)

`arustex` generates a report when a task finishes, and on request (`g` in the Tasks
view). A report is stored with origin `derived` and has two parts: a Markdown body
for reading, and a JSON document for machines. Both are computed only from stored
findings, review verdicts and coverage. **Record bodies are never included.**

Model-authored report drafts (`propose_report_draft`) are stored separately, are
labelled `model_generated`, and are not exported until an operator approves them.

## JSON document

```json
{
  "format": "arustex.report.v1",
  "generated_at": "2026-11-25T08:00:00Z",
  "task": {
    "id": 7,
    "name": "Staging inventory review",
    "objective": "Find configuration records that disagree between environments.",
    "status": "needs_review",
    "tokens_used": 18234
  },
  "coverage": { "total": 5, "reviewed": 5 },
  "findings": [
    {
      "id": 3,
      "title": "Retention differs between staging and production",
      "severity": "medium",
      "status": "pending",
      "category": "consistency",
      "summary": "inv-002 keeps 30 days, inv-003 keeps 7 days.",
      "record_ids": [2, 3],
      "provenance": {
        "origin": "model_generated",
        "source": "model",
        "session_id": "4f1c7f8e-0f0a-4a3b-9d3e-2b6f0e2d1c55"
      }
    }
  ],
  "severity_counts": { "medium": 1 },
  "review_verdicts": { "match": 4, "mismatch": 1 },
  "provenance": {
    "generator": "arustex",
    "origin": "derived",
    "note": "Computed from stored findings and review results; record bodies are not included."
  }
}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `format` | string | Always `arustex.report.v1`. A breaking change gets a new value. |
| `generated_at` | RFC 3339 UTC | When the report was computed. |
| `task.id` | integer | Task id. |
| `task.name` | string | Operator-supplied name. |
| `task.objective` | string | Operator-supplied objective. |
| `task.status` | string | `created`, `queued`, `running`, `paused`, `needs_review`, `completed`, `failed`, `cancelled` or `timed_out`. |
| `task.tokens_used` | integer | Model tokens charged to the task. |
| `coverage.total` / `coverage.reviewed` | integer | Records in the task's approved datasets / records whose review item finished or whose finding an operator closed. |
| `findings[]` | array | Every finding of the task, ordered by id. |
| `findings[].severity` | string | `info`, `low`, `medium`, `high`, `critical`. |
| `findings[].status` | string | `pending`, `confirmed`, `dismissed`, `resolved`, `duplicate`. Only an operator moves a finding out of `pending`. |
| `findings[].category` | string | Free-form classification chosen by whoever recorded the finding. |
| `findings[].summary` | string | Redacted, at most 4 KiB. |
| `findings[].record_ids` | integer[] | Records the finding refers to. Look them up in ARUSTEX; their content is not copied. |
| `findings[].provenance.origin` | string | `supplied` (imported), `model_generated`, `operator` or `derived`. |
| `findings[].provenance.source` | string | Where it came from, for example `model`, an operator such as `operator:uid1000`, an import file name or a legacy table name. |
| `findings[].provenance.session_id` | UUID or null | The agent session that produced it, if any. Its transcript is in the Sessions view. |
| `severity_counts` | object | Number of findings per severity; absent severities are omitted. |
| `review_verdicts` | object | Number of reviewer verdicts per value: `match`, `mismatch`, `inconclusive`. |
| `provenance` | object | Provenance of the report itself. |

Readers must ignore unknown fields so that additive changes stay compatible.

## Markdown body

The body has a header with the task, status, coverage and verdict counts, a findings
table (id, severity, status, title, records, origin) and a details section with each
finding's summary and origin. Text from findings is escaped (`|`, `<`, `>` and line
breaks) so that it cannot change the table structure or inject markup.
