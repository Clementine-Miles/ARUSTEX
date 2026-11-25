//! Adapter tests against a local mock HTTP server (no external network).

use std::time::Duration;

use arustex_core::Secret;
use arustex_llm::*;
use serde_json::json;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn settings(base: String) -> HttpSettings {
    HttpSettings {
        base_url: base,
        timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(2),
        max_retries: 2,
        requests_per_minute: 0,
        max_event_bytes: 1 << 20,
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        system: "You analyse synthetic records.".into(),
        messages: vec![
            Message::user("Summarise the scope."),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: "c0".into(),
                    name: "summarize_scope".into(),
                    input: json!({}),
                }],
            },
            Message::tool_result("c0", "{\"records\":3}", false),
        ],
        tools: vec![ToolDefinition {
            name: "submit_result".into(),
            description: "Finish".into(),
            input_schema: json!({"type": "object", "properties": {"summary": {"type": "string"}}}),
        }],
        max_output_tokens: 256,
    }
}

fn sse(events: &[&str]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn key() -> Option<Secret> {
    Some(Secret::new("sk-test-not-real"))
}

#[tokio::test]
async fn openai_chat_streams_text_and_tool_calls() {
    let server = MockServer::start().await;
    let body = sse(&[
        r#"{"model":"m-1","choices":[{"delta":{"content":"Three "}}]}"#,
        r#"{"choices":[{"delta":{"content":"records."}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"submit_result","arguments":"{\"summ"}}]}}]}"#,
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ary\":\"ok\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        r#"{"choices":[],"usage":{"prompt_tokens":40,"completion_tokens":9}}"#,
        "[DONE]",
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test-not-real"))
        .and(body_partial_json(json!({"stream": true, "messages": [
            {"role": "system"}, {"role": "user"}, {"role": "assistant"}, {"role": "tool", "tool_call_id": "c0"}
        ]})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let p = OpenAiChatProvider::new(
        "p",
        "m",
        key(),
        settings(format!("{}/v1", server.uri())),
        8000,
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let r = p
        .complete(&request(), &CancellationToken::new(), Some(tx))
        .await
        .unwrap();
    assert_eq!(r.stop_reason, StopReason::ToolUse);
    assert_eq!(r.message.text(), "Three records.");
    let (id, name, input) = r.message.tool_uses().next().unwrap();
    assert_eq!(
        (id, name, input),
        ("call_1", "submit_result", &json!({"summary": "ok"}))
    );
    assert_eq!(
        r.usage,
        Usage {
            input_tokens: 40,
            output_tokens: 9
        }
    );
    assert_eq!(r.model, "m-1");
    assert_eq!(rx.recv().await, Some(StreamDelta::Text("Three ".into())));
}

#[tokio::test]
async fn anthropic_streams_tool_use_and_merges_tool_results() {
    let server = MockServer::start().await;
    let body = [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"a-1\",\"usage\":{\"input_tokens\":30}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Done.\"}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"submit_result\",\"input\":{}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"summary\\\":\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"x\\\"}\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":12}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]
    .concat();
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "sk-test-not-real"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(body_partial_json(json!({"system": "You analyse synthetic records.", "messages": [
            {"role": "user"}, {"role": "assistant"}, {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "c0"}]}
        ]})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let p = AnthropicProvider::new("a", "a", key(), settings(server.uri()), 8000).unwrap();
    let r = p
        .complete(&request(), &CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(r.stop_reason, StopReason::ToolUse);
    assert_eq!(r.message.text(), "Done.");
    assert_eq!(
        r.message.tool_uses().next().unwrap().2,
        &json!({"summary": "x"})
    );
    assert_eq!(r.usage.total(), 42);
}

#[tokio::test]
async fn openai_responses_reads_completed_output() {
    let server = MockServer::start().await;
    let completed = json!({"type": "response.completed", "response": {
        "model": "r-1", "status": "completed",
        "output": [
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "Hello"}]},
            {"type": "function_call", "call_id": "fc_1", "name": "submit_result", "arguments": "{\"summary\":\"s\"}"}
        ],
        "usage": {"input_tokens": 11, "output_tokens": 5}
    }});
    let body = format!(
        "event: response.output_text.delta\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.output_text.delta", "delta": "Hello"}),
        completed
    );
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(body_partial_json(json!({"store": false, "instructions": "You analyse synthetic records.",
            "input": [{"type": "message"}, {"type": "function_call", "call_id": "c0"}, {"type": "function_call_output", "call_id": "c0"}]})))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let p = OpenAiResponsesProvider::new(
        "r",
        "r",
        key(),
        settings(format!("{}/v1", server.uri())),
        8000,
    )
    .unwrap();
    let r = p
        .complete(&request(), &CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(r.stop_reason, StopReason::ToolUse);
    assert_eq!(r.message.text(), "Hello");
    assert_eq!(r.usage.total(), 16);
}

#[tokio::test]
async fn retries_rate_limits_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_json(json!({"error": {"message": "slow down"}})),
        )
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[
                r#"{"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]),
            "text/event-stream",
        ))
        .with_priority(2)
        .mount(&server)
        .await;
    let p = OpenAiChatProvider::new(
        "p",
        "m",
        key(),
        settings(format!("{}/v1", server.uri())),
        8000,
    )
    .unwrap();
    let r = p
        .complete(&request(), &CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(r.message.text(), "ok");
    assert_eq!(r.stop_reason, StopReason::EndTurn);
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn client_errors_are_not_retried_and_hide_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400).set_body_json(json!({"error": {"message": "bad request"}})),
        )
        .mount(&server)
        .await;
    let p = AnthropicProvider::new("a", "a", key(), settings(server.uri()), 8000).unwrap();
    let err = p
        .complete(&request(), &CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Http { status: 400, .. }), "{err:?}");
    assert!(!format!("{err} {p:?}").contains("sk-test-not-real"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn timeouts_and_cancellation_are_enforced() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(3))
                .set_body_raw("", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let mut s = settings(server.uri());
    s.timeout = Duration::from_millis(300);
    s.max_retries = 0;
    let p = AnthropicProvider::new("a", "a", key(), s, 8000).unwrap();
    let err = p
        .complete(&request(), &CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Timeout(_)), "{err:?}");

    let p = AnthropicProvider::new("a", "a", key(), settings(server.uri()), 8000).unwrap();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        c2.cancel();
    });
    let started = std::time::Instant::now();
    let err = p.complete(&request(), &cancel, None).await.unwrap_err();
    assert!(matches!(err, LlmError::Cancelled), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn truncated_streams_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse(&[r#"{"choices":[{"delta":{"content":"partial"}}]}"#]),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    let p = OpenAiChatProvider::new(
        "p",
        "m",
        key(),
        settings(format!("{}/v1", server.uri())),
        8000,
    )
    .unwrap();
    let err = p
        .complete(&request(), &CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, LlmError::Protocol(_)), "{err:?}");
}

#[test]
fn router_requires_keys_for_remote_providers() {
    let base = ProviderSettings {
        name: "x".into(),
        kind: ProviderKind::Anthropic,
        model: "m".into(),
        base_url: None,
        api_key: None,
        timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(1),
        max_retries: 0,
        requests_per_minute: 0,
        context_window: 1000,
    };
    assert!(build_provider(base.clone()).is_err());
    let mut router = ProviderRouter::new();
    router.insert(
        build_provider(ProviderSettings {
            kind: ProviderKind::Mock,
            name: "mock".into(),
            ..base.clone()
        })
        .unwrap(),
        false,
    );
    router.insert(
        build_provider(ProviderSettings {
            api_key: key(),
            ..base
        })
        .unwrap(),
        true,
    );
    assert_eq!(router.get(None).unwrap().name(), "x");
    assert_eq!(router.get(Some("mock")).unwrap().name(), "mock");
    assert!(router.get(Some("nope")).is_err());
}
