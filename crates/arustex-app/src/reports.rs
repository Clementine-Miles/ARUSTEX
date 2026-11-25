//! Deterministic task reports with provenance.
//!
//! A generated report is computed from stored data only. Every finding carries its
//! origin; model-generated statements are labelled as such and keep their pending,
//! confirmed or dismissed review status. Record bodies are never copied into reports.

use std::collections::BTreeMap;

use arustex_core::domain::TaskId;
use arustex_core::domain::finding::{Finding, Severity};
use arustex_core::domain::report::Report;
use arustex_core::domain::{Origin, Provenance};
use arustex_core::ports::TaskStore;
use arustex_core::{CoreResult, Page};
use arustex_store::Store;
use serde_json::{Value, json};

pub const REPORT_FORMAT: &str = "arustex.report.v1";

fn origin_label(p: &Provenance) -> &'static str {
    match p.origin {
        Origin::Supplied => "imported",
        Origin::ModelGenerated => "model-generated",
        Origin::Operator => "operator",
        Origin::Derived => "derived",
    }
}

/// Escapes text for a Markdown table cell or paragraph.
fn md(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace(['\r', '\n'], " ")
}

fn finding_json(f: &Finding) -> Value {
    json!({
        "id": f.id,
        "title": f.title,
        "severity": f.severity,
        "status": f.status,
        "category": f.category,
        "summary": f.summary,
        "record_ids": f.record_ids,
        "provenance": {
            "origin": f.provenance.origin,
            "source": f.provenance.source,
            "session_id": f.provenance.session_id,
        },
    })
}

/// Builds the report body and JSON document for a task.
pub async fn build_report(store: &Store, task_id: TaskId) -> CoreResult<(String, String, Value)> {
    let task = store.get_task(task_id).await?;
    let coverage = store.task_coverage(task_id).await?;
    let findings = store.task_findings(task_id).await?;
    let mut verdicts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut offset = 0;
    loop {
        let page = store
            .list_review_results(Some(task_id), Page::new(offset, 200))
            .await?;
        for r in &page.items {
            *verdicts.entry(r.verdict.as_str()).or_default() += 1;
        }
        offset += page.items.len() as u32;
        if page.items.is_empty() || u64::from(offset) >= page.total {
            break;
        }
    }
    let mut by_severity: BTreeMap<String, u64> = BTreeMap::new();
    for f in &findings {
        *by_severity.entry(f.severity.to_string()).or_default() += 1;
    }
    let generated_at = chrono::Utc::now();
    let title = format!("Report: {}", task.display_name());

    let mut body = String::new();
    body.push_str(&format!("# {}\n\n", md(&title)));
    body.push_str(&format!(
        "- Task: #{} ({})\n- Objective: {}\n- Status: {}\n- Generated: {}\n- Review coverage: {}/{} records\n- Tokens used: {}\n\n",
        task.id,
        md(task.display_name()),
        md(&task.objective),
        task.status,
        generated_at.format("%Y-%m-%d %H:%M:%S UTC"),
        coverage.reviewed,
        coverage.total,
        task.tokens_used
    ));
    body.push_str(
        "> Findings marked *model-generated* were produced by a workflow agent from imported \
         records and are not verified unless their status is `confirmed`.\n\n",
    );
    body.push_str("## Summary\n\n");
    if findings.is_empty() {
        body.push_str("No findings were recorded.\n\n");
    } else {
        for sev in Severity::ALL.iter().rev() {
            if let Some(n) = by_severity.get(sev.as_str()) {
                body.push_str(&format!("- {}: {n}\n", sev.as_str()));
            }
        }
        body.push('\n');
    }
    if !verdicts.is_empty() {
        body.push_str("Reviewer verdicts: ");
        let parts: Vec<String> = verdicts.iter().map(|(k, v)| format!("{k} {v}")).collect();
        body.push_str(&parts.join(", "));
        body.push_str("\n\n");
    }
    if !findings.is_empty() {
        body.push_str("## Findings\n\n| # | Severity | Status | Title | Records | Origin |\n|---|---|---|---|---|---|\n");
        for f in &findings {
            let records: Vec<String> = f.record_ids.iter().map(ToString::to_string).collect();
            body.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                f.id,
                f.severity,
                f.status,
                md(&f.title),
                records.join(", "),
                origin_label(&f.provenance)
            ));
        }
        body.push_str("\n## Details\n\n");
        for f in &findings {
            body.push_str(&format!("### #{} {}\n\n", f.id, md(&f.title)));
            body.push_str(&format!(
                "*{} · {} · {}*",
                f.severity,
                f.status,
                origin_label(&f.provenance)
            ));
            if let Some(s) = f.provenance.session_id {
                body.push_str(&format!(" · session `{s}`"));
            }
            body.push_str("\n\n");
            if !f.summary.is_empty() {
                body.push_str(&md(&f.summary));
                body.push_str("\n\n");
            }
        }
    }

    let document = json!({
        "format": REPORT_FORMAT,
        "generated_at": generated_at,
        "task": {
            "id": task.id,
            "name": task.name,
            "objective": task.objective,
            "status": task.status,
            "tokens_used": task.tokens_used,
        },
        "coverage": {"total": coverage.total, "reviewed": coverage.reviewed},
        "findings": findings.iter().map(finding_json).collect::<Vec<_>>(),
        "severity_counts": by_severity,
        "review_verdicts": verdicts,
        "provenance": {
            "generator": "arustex",
            "origin": "derived",
            "note": "Computed from stored findings and review results; record bodies are not included.",
        },
    });
    Ok((title, body, document))
}

/// Generates and stores a report for a task.
pub async fn generate_report(store: &Store, task_id: TaskId) -> CoreResult<Report> {
    let (title, body, document) = build_report(store, task_id).await?;
    store
        .save_generated_report(task_id, &title, &body, document)
        .await
}

#[cfg(test)]
mod tests {
    use super::md;

    #[test]
    fn markdown_escaping_prevents_table_and_html_injection() {
        assert_eq!(md("a|b<script>\nc"), "a\\|b&lt;script&gt; c");
    }
}
