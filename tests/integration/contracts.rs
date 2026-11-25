//! Contracts that other components and operators rely on.

use arustex_app::config::Config;
use arustex_core::domain::agent::AgentRole;
use arustex_core::ipc::{self, PROTOCOL_VERSION, ServerFrame};
use arustex_operations::Registry;
use arustex_policy::Policy;

fn root(path: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(path)
}

/// The version-1 request frames in `contracts/ipc_v1.jsonl` must keep decoding and
/// re-encode to the same JSON, so existing clients keep working.
#[test]
fn ipc_v1_golden_frames_round_trip() {
    let text = std::fs::read_to_string(root("tests/contracts/ipc_v1.jsonl")).unwrap();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let frame = ipc::decode_client(line).unwrap_or_else(|e| panic!("{line}: {e}"));
        assert_eq!(frame.v, PROTOCOL_VERSION);
        let again: serde_json::Value = serde_json::from_str(&ipc::encode(&frame).unwrap()).unwrap();
        let original: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(again, original, "{line}");
    }
    let resync = ipc::encode(&ServerFrame::Resync {
        v: 1,
        latest_seq: 9,
    })
    .unwrap();
    assert_eq!(resync, r#"{"frame":"resync","v":1,"latest_seq":9}"#);
}

#[test]
fn example_configuration_is_valid_and_safe_by_default() {
    let path = root("config.example.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    let config = Config::parse(&text, &path).unwrap();
    config.validate().unwrap();
    assert!(!config.telegram.enabled, "notifications are opt-in");
    assert!(
        config
            .providers
            .iter()
            .all(|p| p.kind == arustex_app::config::ProviderKind::Mock)
    );
}

/// The operation surface is closed: exactly these operations exist, every input
/// schema rejects unknown fields, and nothing can execute commands, touch files or
/// reach the network.
#[test]
fn operation_registry_is_closed_and_strict() {
    let registry = Registry::standard(Policy::standard());
    let mut names: Vec<_> = registry.descriptors().map(|d| d.name).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "compare_outputs",
            "field_statistics",
            "propose_report_draft",
            "propose_status_update",
            "read_records",
            "record_finding",
            "search_records",
            "summarize_scope",
            "validate_graph",
        ]
    );
    for role in [
        AgentRole::Planner,
        AgentRole::Worker,
        AgentRole::Assistant,
        AgentRole::Reviewer,
    ] {
        for tool in registry.tools_for(role) {
            let schema = tool.input_schema.to_string();
            assert!(
                !schema.contains("$ref"),
                "{}: schema must be self-contained",
                tool.name
            );
            assert_eq!(
                tool.input_schema["additionalProperties"],
                serde_json::Value::Bool(false),
                "{} must reject unknown fields",
                tool.name
            );
            for forbidden in ["command", "shell", "url", "path", "host", "exec"] {
                assert!(
                    !tool.input_schema["properties"]
                        .as_object()
                        .is_some_and(|p| p.contains_key(forbidden)),
                    "{} exposes a {forbidden} parameter",
                    tool.name
                );
            }
        }
    }
}
