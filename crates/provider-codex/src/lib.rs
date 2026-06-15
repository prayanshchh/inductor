use std::{pin::Pin, time::Duration};

use async_stream::try_stream;
use futures_core::Stream;
use futures_util::StreamExt;
use harness_core::{
    MessagePart, ModelInfo, ModelMessage, ProviderCapabilities, SessionEvent, SessionStatus,
    StopReason, ToolCallId, TurnRequest,
};
use provider_core::{PermissionResponses, ProviderAuth, ProviderAuthKind, ProviderPlugin};
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";
const DEFAULT_CODEX_IDLE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct CodexProvider {
    client: reqwest::Client,
    base_url: String,
}

impl CodexProvider {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_base_url(
            std::env::var("INDUCTOR_CODEX_BASE_URL")
                .unwrap_or_else(|_| DEFAULT_CODEX_BASE_URL.to_string()),
        )
    }

    pub fn with_base_url(base_url: impl Into<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;

        Ok(Self {
            client,
            base_url: trim_trailing_slash(base_url.into()),
        })
    }

    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    fn request_body(&self, req: &TurnRequest) -> Value {
        let input = if req.messages.is_empty() {
            legacy_input_messages(req)
        } else {
            req.messages.iter().map(codex_message).collect::<Vec<_>>()
        };

        let mut body = json!({
            "model": normalize_codex_model(&req.model),
            "instructions": req.system_prompt.as_deref().unwrap_or("You are an Inductor coding agent working in the user's workspace. \
                Use the provided tools to read, edit, and create files and run commands. Don't \
                describe a tool-call format — just call the tools. Keep explanations concise."),
            "input": input,
            "tools": codex_tools(),
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "stream": true,
            "store": false
        });
        if let Some(effort) = req.metadata.get("model_effort").and_then(Value::as_str) {
            // The Responses API nests effort under `reasoning`; a top-level
            // `reasoning_effort` is rejected as an unsupported parameter.
            body["reasoning"] = json!({
                "effort": if effort == "minimal" { "none" } else { effort }
            });
        }
        body
    }
}

fn legacy_input_messages(req: &TurnRequest) -> Vec<Value> {
    let mut content = vec![json!({
        "type": "input_text",
        "text": req.prompt
    })];
    content.extend(req.images.iter().map(|image| {
        json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.base64_data),
            "detail": "auto"
        })
    }));
    vec![json!({
        "role": "user",
        "content": content
    })]
}

fn codex_message(message: &ModelMessage) -> Value {
    let (role, parts) = codex_message_role_and_parts(message);
    let content = parts.iter().map(codex_part).collect::<Vec<_>>();
    json!({
        "role": role,
        "content": content,
    })
}

fn codex_message_role_and_parts(message: &ModelMessage) -> (&'static str, Vec<MessagePart>) {
    match message.role.to_ascii_lowercase().as_str() {
        "assistant" => ("assistant", message.parts.clone()),
        "system" => ("system", message.parts.clone()),
        "developer" => ("developer", message.parts.clone()),
        "user" => ("user", message.parts.clone()),
        "tool" => ("user", prefix_text_parts("Tool", &message.parts)),
        _ => ("user", prefix_text_parts(&message.role, &message.parts)),
    }
}

fn prefix_text_parts(label: &str, parts: &[MessagePart]) -> Vec<MessagePart> {
    let prefix = format!("{label}:\n");
    let mut prefixed = Vec::with_capacity(parts.len().max(1));
    match parts.split_first() {
        Some((MessagePart::Text { text }, rest)) => {
            prefixed.push(MessagePart::Text {
                text: format!("{prefix}{text}"),
            });
            prefixed.extend(rest.iter().cloned());
        }
        Some((first, rest)) => {
            prefixed.push(MessagePart::Text { text: prefix });
            prefixed.push(first.clone());
            prefixed.extend(rest.iter().cloned());
        }
        None => prefixed.push(MessagePart::Text { text: prefix }),
    }
    prefixed
}

fn codex_part(part: &MessagePart) -> Value {
    match part {
        MessagePart::Text { text } => json!({
            "type": "input_text",
            "text": text,
        }),
        MessagePart::Image { image } => json!({
            "type": "input_image",
            "image_url": format!("data:{};base64,{}", image.mime_type, image.base64_data),
            "detail": "auto",
        }),
    }
}

/// Inductor's shared tool registry exposed as OpenAI Responses function tools.
/// OpenAI-hosted web search remains provider-specific because it is executed
/// server-side by OpenAI rather than by Inductor's local tool runtime.
fn codex_tools() -> Value {
    let mut definitions = tools::tool_definitions()
        .into_iter()
        .map(|definition| {
            json!({
                "type": "function",
                "name": definition.name.as_str(),
                "description": definition.description,
                "parameters": definition.input_schema,
            })
        })
        .collect::<Vec<_>>();
    definitions.push(json!({ "type": "web_search" }));
    Value::Array(definitions)
}

#[async_trait::async_trait]
impl ProviderPlugin for CodexProvider {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: true,
        }
    }

    async fn list_models(&self, _auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(codex_model_catalog())
    }

    async fn stream_turn(
        &self,
        auth: &ProviderAuth,
        req: TurnRequest,
        cancel: CancellationToken,
        // Codex emits native function calls, but the harness owns execution.
        _permissions: PermissionResponses,
        _tool_responses: provider_core::ToolResponses,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, bearer_header(auth)?);

        let request = self
            .client
            .post(self.responses_url())
            .headers(headers)
            .json(&self.request_body(&req));

        let session_id = req.session_id;
        let idle_timeout = codex_idle_timeout();
        let stream = try_stream! {
            yield SessionEvent::Status {
                session_id,
                status: SessionStatus::Starting,
            };

            let response = tokio::select! {
                _ = cancel.cancelled() => {
                    yield SessionEvent::Result {
                        session_id,
                        stop_reason: StopReason::Interrupted,
                    };
                    return;
                }
                response = request.send() => response,
                _ = sleep(idle_timeout) => {
                    yield SessionEvent::Error {
                        session_id,
                        message: format!(
                            "Codex provider produced no response for {} seconds; stopped the stale run",
                            idle_timeout.as_secs()
                        ),
                    };
                    return;
                }
            };
            let response = response?;
            let status = response.status();

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                yield SessionEvent::Error {
                    session_id,
                    message: format!("codex provider request failed with HTTP {status}: {}", redact_error_body(&body)),
                };
                return;
            }

            yield SessionEvent::Status {
                session_id,
                status: SessionStatus::Streaming,
            };

            let mut bytes = response.bytes_stream();
            let mut buffer = String::new();

            loop {
                let chunk = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield SessionEvent::Result {
                            session_id,
                            stop_reason: StopReason::Interrupted,
                        };
                        return;
                    }
                    chunk = bytes.next() => chunk,
                    _ = sleep(idle_timeout) => {
                        yield SessionEvent::Error {
                            session_id,
                            message: format!(
                                "Codex provider stream produced no events for {} seconds; stopped the stale run",
                                idle_timeout.as_secs()
                            ),
                        };
                        return;
                    }
                };

                let Some(chunk) = chunk else {
                    break;
                };

                if cancel.is_cancelled() {
                    yield SessionEvent::Result {
                        session_id,
                        stop_reason: StopReason::Interrupted,
                    };
                    return;
                }

                let chunk = chunk?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                for event in drain_sse_events(&mut buffer) {
                    for mapped in parse_response_stream_event(session_id, &event) {
                        yield mapped;
                    }
                }
            }

            for event in drain_sse_events_at_eof(&mut buffer) {
                for mapped in parse_response_stream_event(session_id, &event) {
                    yield mapped;
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn codex_idle_timeout() -> Duration {
    std::env::var("INDUCTOR_CODEX_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CODEX_IDLE_TIMEOUT)
}

fn bearer_header(auth: &ProviderAuth) -> anyhow::Result<HeaderValue> {
    match auth.kind() {
        ProviderAuthKind::ApiKey
        | ProviderAuthKind::BearerToken
        | ProviderAuthKind::SessionToken
        | ProviderAuthKind::Unknown => {}
    }

    let mut value = HeaderValue::from_str(&format!("Bearer {}", auth.expose_secret()))?;
    value.set_sensitive(true);
    Ok(value)
}

fn drain_sse_events(buffer: &mut String) -> Vec<String> {
    let mut events = Vec::new();

    while let Some(index) = buffer.find("\n\n") {
        let event = buffer[..index].to_string();
        buffer.drain(..index + 2);
        events.push(event);
    }

    events
}

fn drain_sse_events_at_eof(buffer: &mut String) -> Vec<String> {
    if buffer.trim().is_empty() {
        return Vec::new();
    }

    vec![std::mem::take(buffer)]
}

fn parse_response_stream_event(
    session_id: harness_core::SessionId,
    raw: &str,
) -> Vec<SessionEvent> {
    let mut event_name = None;
    let mut data_lines = Vec::new();

    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim().to_string());
        }
    }

    let data = data_lines.join("\n");
    if data.is_empty() || data == "[DONE]" {
        return Vec::new();
    }

    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return Vec::new();
    };
    let Some(event_type) = value
        .get("type")
        .and_then(Value::as_str)
        .or(event_name.as_deref())
    else {
        return Vec::new();
    };

    match event_type {
        "response.output_text.delta" | "output_text.delta" | "text_delta" => value
            .get("delta")
            .or_else(|| value.get("text"))
            .and_then(Value::as_str)
            .map(|text| SessionEvent::TextDelta {
                session_id,
                text: text.to_string(),
            })
            .into_iter()
            .collect(),
        // A completed native function call. Emit a structured harness request;
        // the harness executes and permission-gates it without a text envelope.
        "response.output_item.done" | "response.output_item.added" => {
            let Some(item) = value.get("item") else {
                return Vec::new();
            };
            // `added` carries no arguments yet; only act on `done`.
            if event_type == "response.output_item.added" {
                return Vec::new();
            }
            match item.get("type").and_then(Value::as_str) {
                // Our function tools -> structured harness execution.
                Some("function_call") => {
                    let Some(name) = item.get("name").and_then(Value::as_str) else {
                        return Vec::new();
                    };
                    let tool_call_id = ToolCallId::new();
                    let input = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or_else(|| json!({}));
                    vec![SessionEvent::ToolCallRequested {
                        session_id,
                        tool_call_id,
                        name: name.to_string(),
                        input_json: input,
                    }]
                }
                // OpenAI-hosted web search runs server-side — surface it for
                // display only (no local execution / permission needed).
                Some("web_search_call") => {
                    let query = item
                        .pointer("/action/query")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    vec![SessionEvent::ToolCallStart {
                        session_id,
                        tool_call_id: ToolCallId::new(),
                        name: "web_search".to_string(),
                        input_json: json!({ "query": query }),
                    }]
                }
                _ => Vec::new(),
            }
        }
        "response.completed" | "completed" | "done" => {
            // The completed event carries the real usage totals.
            let mut events = Vec::new();
            if let Some(usage) = usage_event(session_id, &value) {
                events.push(usage);
            }
            events.push(SessionEvent::Result {
                session_id,
                stop_reason: StopReason::EndTurn,
            });
            events
        }
        "response.failed" | "error" => vec![SessionEvent::Error {
            session_id,
            message: value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .unwrap_or("codex provider stream failed")
                .to_string(),
        }],
        _ => Vec::new(),
    }
}

/// Extract OpenAI Responses usage (`response.usage`) into a Usage event.
fn usage_event(session_id: harness_core::SessionId, value: &Value) -> Option<SessionEvent> {
    let usage = value
        .pointer("/response/usage")
        .or_else(|| value.get("usage"))?;
    let num = |key: &str| usage.get(key).and_then(Value::as_u64);
    let cache_read = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(Value::as_u64);
    Some(SessionEvent::Usage {
        session_id,
        input_tokens: num("input_tokens").or_else(|| num("prompt_tokens")),
        output_tokens: num("output_tokens").or_else(|| num("completion_tokens")),
        cache_read_tokens: cache_read,
        total_cost_usd: None,
    })
}

fn redact_error_body(body: &str) -> String {
    const MAX_ERROR_BODY_CHARS: usize = 2_000;
    body.chars()
        .take(MAX_ERROR_BODY_CHARS)
        .collect::<String>()
        .replace("Bearer ", "Bearer <redacted>")
}

fn trim_trailing_slash(value: String) -> String {
    value.trim_end_matches('/').to_string()
}

fn normalize_codex_model(model: &str) -> &str {
    let model = model.trim();
    match model {
        "" | "gpt-5" | "gpt-5-mini" | "gpt-4.1" => DEFAULT_CODEX_MODEL,
        _ => model,
    }
}

fn codex_model_catalog() -> Vec<ModelInfo> {
    let mut models = vec![
        ModelInfo {
            id: DEFAULT_CODEX_MODEL.to_string(),
            display_name: "GPT-5.5".to_string(),
            context_window: None,
        },
        ModelInfo {
            id: "gpt-5.4".to_string(),
            display_name: "GPT-5.4".to_string(),
            context_window: None,
        },
        ModelInfo {
            id: "gpt-5.4-mini".to_string(),
            display_name: "GPT-5.4-Mini".to_string(),
            context_window: None,
        },
    ];
    extend_models_from_env(&mut models, "INDUCTOR_CODEX_MODELS");
    models
}

fn extend_models_from_env(models: &mut Vec<ModelInfo>, env_key: &str) {
    let Ok(raw) = std::env::var(env_key) else {
        return;
    };
    for id in raw.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        if models.iter().any(|model| model.id == id) {
            continue;
        }
        models.push(ModelInfo {
            id: id.to_string(),
            display_name: id.to_string(),
            context_window: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::ImageAttachment;
    use harness_core::SessionId;
    use secrecy::SecretString;

    fn text_request(prompt: &str) -> TurnRequest {
        TurnRequest {
            session_id: SessionId::new(),
            model: "gpt-test".to_string(),
            prompt: prompt.to_string(),
            system_prompt: None,
            messages: Vec::new(),
            tool_names: Vec::new(),
            metadata: Value::Null,
            images: Vec::new(),
        }
    }

    #[test]
    fn request_body_contains_prompt_and_stream_flag() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let body = provider.request_body(&text_request("say hello"));

        assert_eq!(body["model"], "gpt-test");
        assert!(body["instructions"].as_str().unwrap().contains("Inductor"));
        assert_eq!(body["stream"], true);
        assert_eq!(body["input"][0]["content"][0]["text"], "say hello");
    }

    #[test]
    fn request_body_normalizes_unsupported_codex_models() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let mut request = text_request("say hello");
        request.model = "gpt-5".to_string();
        let body = provider.request_body(&request);

        assert_eq!(body["model"], DEFAULT_CODEX_MODEL);
    }

    #[test]
    fn request_body_serializes_reasoning_effort() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let mut request = text_request("think briefly");
        request.metadata = json!({ "model_effort": "xhigh" });
        let body = provider.request_body(&request);

        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn request_body_includes_image_attachments() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let body = provider.request_body(&TurnRequest {
            session_id: SessionId::new(),
            model: "gpt-test".to_string(),
            prompt: "describe this".to_string(),
            system_prompt: None,
            messages: Vec::new(),
            tool_names: Vec::new(),
            metadata: Value::Null,
            images: vec![ImageAttachment {
                path: Some("screenshot.png".to_string()),
                mime_type: "image/png".to_string(),
                base64_data: "abc123".to_string(),
                width: Some(10),
                height: Some(20),
                file_size: 6,
            }],
        });

        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[1]["type"], "input_image");
        assert_eq!(content[1]["image_url"], "data:image/png;base64,abc123");
    }

    #[test]
    fn request_body_prefers_typed_messages() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let mut request = text_request("legacy prompt");
        request.system_prompt = Some("custom instructions".to_string());
        request.messages = vec![ModelMessage::text("user", "typed hello")];

        let body = provider.request_body(&request);

        assert_eq!(body["instructions"], "custom instructions");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "typed hello");
    }

    #[test]
    fn request_body_converts_tool_messages_to_valid_codex_input() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let mut request = text_request("legacy prompt");
        request.messages = vec![
            ModelMessage::text("assistant", "tool call requested"),
            ModelMessage::text("tool", "read_file result:\nhello"),
        ];

        let body = provider.request_body(&request);

        assert_eq!(body["input"][0]["role"], "assistant");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            "Tool:\nread_file result:\nhello"
        );
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "tool")
        );
    }

    #[test]
    fn request_body_preserves_supported_codex_roles() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let mut request = text_request("legacy prompt");
        request.messages = vec![
            ModelMessage::text("system", "system context"),
            ModelMessage::text("developer", "developer context"),
            ModelMessage::text("user", "user prompt"),
            ModelMessage::text("assistant", "assistant answer"),
        ];

        let body = provider.request_body(&request);
        let roles = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| message["role"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(roles, vec!["system", "developer", "user", "assistant"]);
    }

    #[test]
    fn sse_parser_converts_text_delta() {
        let session_id = SessionId::new();
        let raw = r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"hello"}
"#;

        let events = parse_response_stream_event(session_id, raw);

        assert!(matches!(
            events.first().unwrap(),
            SessionEvent::TextDelta { text, .. } if text == "hello"
        ));
    }

    #[test]
    fn function_call_emits_structured_tool_request() {
        let session_id = SessionId::new();
        let raw = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"function_call","name":"write_file","call_id":"c1","arguments":"{\"path\":\"a.txt\",\"content\":\"hi\"}"}}
"#;

        let events = parse_response_stream_event(session_id, raw);
        assert!(matches!(
            events.first().unwrap(),
            SessionEvent::ToolCallRequested { name, input_json, .. }
                if name == "write_file" && input_json == &json!({ "path": "a.txt", "content": "hi" })
        ));
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn request_body_advertises_function_tools() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let body = provider.request_body(&text_request("hi"));
        let tools = body["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "write_file"));
        assert!(tools.iter().any(|t| t["name"] == "list_dir"));
        assert!(tools.iter().any(|t| t["name"] == "glob"));
        assert!(tools.iter().any(|t| t["name"] == "multi_edit"));
        assert!(tools.iter().any(|t| t["name"] == "apply_patch_structured"));
        assert!(tools.iter().any(|t| t["name"] == "web_fetch"));
        assert!(tools.iter().any(|t| t["name"] == "todo_write"));
        assert!(tools.iter().any(|t| t["name"] == "bash"));
        assert_eq!(tools[0]["type"], "function");
        // The OpenAI-hosted web search tool is advertised too.
        assert!(tools.iter().any(|t| t["type"] == "web_search"));
    }

    #[test]
    fn codex_model_catalog_includes_current_subscription_models() {
        let models = codex_model_catalog();

        assert!(models.iter().any(|model| model.id == "gpt-5.5"));
        assert!(models.iter().any(|model| model.id == "gpt-5.4"));
        assert!(models.iter().any(|model| model.id == "gpt-5.4-mini"));
        assert!(!models.iter().any(|model| model.id == "gpt-5.3-codex"));
    }

    #[test]
    fn web_search_call_surfaces_for_display_not_execution() {
        let session_id = SessionId::new();
        let raw = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"web_search_call","action":{"query":"rust async streams"}}}
"#;
        let events = parse_response_stream_event(session_id, raw);
        // A display-only ToolCallStart, NOT a tool-call envelope (no local exec).
        match events.first().unwrap() {
            SessionEvent::ToolCallStart {
                name, input_json, ..
            } => {
                assert_eq!(name, "web_search");
                assert_eq!(input_json["query"], "rust async streams");
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
    }

    #[test]
    fn sse_parser_converts_completion() {
        let session_id = SessionId::new();
        let raw = r#"event: response.completed
data: {"type":"response.completed"}
"#;

        let events = parse_response_stream_event(session_id, raw);

        assert!(matches!(
            events.last().unwrap(),
            SessionEvent::Result {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
    }

    #[test]
    fn sse_parser_extracts_usage_on_completion() {
        let session_id = SessionId::new();
        let raw = r#"event: response.completed
data: {"type":"response.completed","response":{"usage":{"input_tokens":120,"output_tokens":45,"input_tokens_details":{"cached_tokens":80}}}}
"#;

        let events = parse_response_stream_event(session_id, raw);

        assert!(events.iter().any(|e| matches!(
            e,
            SessionEvent::Usage {
                input_tokens: Some(120),
                output_tokens: Some(45),
                cache_read_tokens: Some(80),
                ..
            }
        )));
        assert!(matches!(
            events.last().unwrap(),
            SessionEvent::Result { .. }
        ));
    }

    #[test]
    fn auth_header_debug_does_not_expose_secret() {
        let auth = ProviderAuth::new(
            ProviderAuthKind::SessionToken,
            SecretString::from("secret-token".to_string()),
        );
        let header = bearer_header(&auth).unwrap();

        assert!(header.is_sensitive());
        assert_eq!(header.to_str().unwrap(), "Bearer secret-token");
        assert!(!format!("{auth:?}").contains("secret-token"));
    }

    #[test]
    fn drains_complete_sse_frames_and_keeps_partial_buffer() {
        let mut buffer = "data: one\n\ndata: two".to_string();
        let events = drain_sse_events(&mut buffer);

        assert_eq!(events, vec!["data: one"]);
        assert_eq!(buffer, "data: two");
    }
}
