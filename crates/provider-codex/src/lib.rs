use std::{pin::Pin, time::Duration};

use async_stream::try_stream;
use futures_core::Stream;
use futures_util::StreamExt;
use harness_core::{
    MessagePart, ModelInfo, ModelMessage, ProviderCapabilities, SessionEvent, SessionStatus,
    StopReason, ToolCallId, TurnRequest,
};
use provider_core::{
    PermissionResponses, ProviderAuth, ProviderAuthKind, ProviderPlugin, ProviderToolResponse,
};
use reqwest::header::{ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::{Value, json};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CODEX_MODEL: &str = "gpt-5.5";
const DEFAULT_CODEX_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_CODEX_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
            .connect_timeout(DEFAULT_CODEX_CONNECT_TIMEOUT)
            .build()?;

        Ok(Self {
            client,
            base_url: trim_trailing_slash(base_url.into()),
        })
    }

    fn responses_url(&self) -> String {
        format!("{}/responses", self.base_url)
    }

    #[cfg(test)]
    fn request_body(&self, req: &TurnRequest) -> Value {
        self.request_body_with_input(req, codex_input_messages(req))
    }

    fn request_body_with_input(&self, req: &TurnRequest, input: Vec<Value>) -> Value {
        let model = normalize_codex_model(&req.model);
        let mut body = json!({
            "model": model,
            "instructions": req.system_prompt.as_deref().unwrap_or("You are an Inductor coding agent working in the user's workspace. \
                Use the provided tools to read, edit, and create files and run commands. Don't \
                describe a tool-call format — just call the tools. Keep the user informed with \
                brief milestone updates, especially before new phases, after tool failures, and \
                before verification. In progress updates, share a concise public reasoning \
                summary: what you are checking, what evidence you found, why the next step \
                follows, and any uncertainty or blocker. Do not reveal hidden chain-of-thought. \
                Keep tool output small: prefer read_file with start_line/end_line over large reads, \
                use focused grep regex patterns instead of broad dumps, and avoid large sed commands. \
                If a tool result says it was truncated and gives a blob_id, call read_blob with a \
                bounded start_byte/limit_bytes range to inspect more without rerunning the tool. \
                Prefer app-owned code first. Inspect dependency or generated code only to resolve a \
                specific unknown; once resolved, stop rereading dependency internals and patch the app code. \
                Once you have a concrete cause, do not restate it in another progress message; apply the fix, \
                run verification, or report a blocker. \
                For file edits, use apply_patch line-aware operations. For pure insertions, use \
                insert_before or insert_after with an adjacent line from a recent read_file result. \
                For replacing existing lines, use exact path, 1-based inclusive start_line/end_line, \
                old text, and new text from a recent read_file result; do not use anchor-only patches."),
            "input": input,
            "tools": codex_tools(),
            "tool_choice": "auto",
            "parallel_tool_calls": true,
            "stream": true,
            "store": false
        });
        if let Some(effort) = req.metadata.get("model_effort").and_then(Value::as_str) {
            // The Responses API nests effort under `reasoning`; a top-level
            // `reasoning_effort` is rejected as an unsupported parameter.
            body["reasoning"] = json!({
                "effort": normalize_codex_reasoning_effort(model, effort)
            });
        }
        body
    }
}

fn normalize_codex_reasoning_effort(model: &str, effort: &str) -> &'static str {
    let canonical = match effort
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-', ' '], "")
        .as_str()
    {
        "none" => "none",
        "minimal" => "minimal",
        "low" => "low",
        "medium" => "medium",
        "high" => "high",
        "xhigh" | "extrahigh" => "xhigh",
        // Codex/OpenAI Responses does not accept "max"; use the strongest
        // supported value instead of sending an invalid request.
        "max" | "ultracode" => "xhigh",
        _ => "medium",
    };

    if model.eq_ignore_ascii_case("gpt-5-pro") {
        return "high";
    }

    if model.starts_with("gpt-5.1") && canonical == "xhigh" {
        return "high";
    }

    if (model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
        || model.starts_with("codex-mini"))
        && matches!(canonical, "none" | "minimal" | "xhigh")
    {
        return "medium";
    }

    canonical
}

fn codex_input_messages(req: &TurnRequest) -> Vec<Value> {
    if req.messages.is_empty() {
        legacy_input_messages(req)
    } else {
        req.messages.iter().filter_map(codex_message).collect()
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

fn codex_message(message: &ModelMessage) -> Option<Value> {
    let (role, parts) = codex_message_role_and_parts(message);
    let is_assistant = role == "assistant";
    let content = parts
        .iter()
        .map(|part| codex_part(part, is_assistant))
        .collect::<Vec<_>>();
    if content.iter().all(codex_part_is_empty_text) {
        return None;
    }
    Some(json!({
        "role": role,
        "content": content,
    }))
}

fn codex_part_is_empty_text(part: &Value) -> bool {
    part.get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.trim().is_empty())
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

fn codex_part(part: &MessagePart, is_assistant: bool) -> Value {
    match part {
        MessagePart::Text { text } if is_assistant => json!({
            "type": "output_text",
            "text": text,
            "annotations": [],
        }),
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
        mut tool_responses: provider_core::ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, bearer_header(auth)?);
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

        let session_id = req.session_id;
        let provider = self.clone();
        let responses_url = self.responses_url();
        let idle_timeout = codex_idle_timeout();
        let stream = try_stream! {
            yield SessionEvent::Status {
                session_id,
                status: SessionStatus::Starting,
            };

            let mut input = codex_input_messages(&req);
            loop {
                let mut safe_stream_retries_remaining = 1usize;
                'request_attempt: loop {
                let request = provider
                    .client
                    .post(&responses_url)
                    .headers(headers.clone())
                    .json(&provider.request_body_with_input(&req, input.clone()));

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
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);

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
                let mut output_items = Vec::new();
                let mut pending_function_calls = Vec::new();
                let mut bytes_read = 0usize;
                let mut events_parsed = 0usize;
                let mut last_event_type: Option<String> = None;
                let mut emitted_visible_event = false;
                let mut retry_request = false;

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

                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            let diagnostic = format_stream_decode_error(
                                "Codex",
                                &error,
                                status,
                                content_type.as_deref(),
                                bytes_read,
                                events_parsed,
                                last_event_type.as_deref(),
                                emitted_visible_event,
                            );
                            if !emitted_visible_event && safe_stream_retries_remaining > 0 {
                                safe_stream_retries_remaining -= 1;
                                retry_request = true;
                                break;
                            }
                            Err(anyhow::anyhow!(diagnostic))?;
                            unreachable!();
                        }
                    };
                    bytes_read += chunk.len();
                    buffer.push_str(&String::from_utf8_lossy(&chunk));

                    for event in drain_sse_events(&mut buffer) {
                        events_parsed += 1;
                        last_event_type = sse_event_type(&event);
                        let parsed = parse_response_stream_event_detail(session_id, &event);
                        let _response_completed = parsed.completed;
                        output_items.extend(parsed.output_items);
                        pending_function_calls.extend(parsed.function_calls);
                        for mapped in parsed.events {
                            emitted_visible_event |= provider_event_is_visible(&mapped);
                            yield mapped;
                        }
                    }
                }

                if retry_request {
                    continue 'request_attempt;
                }

                for event in drain_sse_events_at_eof(&mut buffer) {
                    let parsed = parse_response_stream_event_detail(session_id, &event);
                    let _response_completed = parsed.completed;
                    output_items.extend(parsed.output_items);
                    pending_function_calls.extend(parsed.function_calls);
                    for mapped in parsed.events {
                        yield mapped;
                    }
                }

                if pending_function_calls.is_empty() {
                    yield SessionEvent::Result {
                        session_id,
                        stop_reason: StopReason::EndTurn,
                    };
                    return;
                }

                input.extend(output_items);
                for pending in pending_function_calls {
                    let tool_result = loop {
                        let response = tokio::select! {
                            _ = cancel.cancelled() => {
                                yield SessionEvent::Result {
                                    session_id,
                                    stop_reason: StopReason::Interrupted,
                                };
                                return;
                            }
                            response = tool_responses.recv() => response,
                        };
                        let Some(response) = response else {
                            yield SessionEvent::Error {
                                session_id,
                                message: format!(
                                    "codex provider lost local tool result for {}",
                                    pending.name
                                ),
                            };
                            yield SessionEvent::Result {
                                session_id,
                                stop_reason: StopReason::Error,
                            };
                            return;
                        };
                        if response.tool_call_id == pending.tool_call_id {
                            break response;
                        }
                    };
                    input.push(function_call_output_item(&pending, tool_result));
                }
                break 'request_attempt;
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

fn provider_event_is_visible(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::TextStart { .. }
            | SessionEvent::TextDelta { .. }
            | SessionEvent::TextEnd { .. }
            | SessionEvent::ToolCallRequested { .. }
            | SessionEvent::ToolInputStart { .. }
            | SessionEvent::ToolInputEnd { .. }
            | SessionEvent::ToolCallStart { .. }
            | SessionEvent::ToolCallResult { .. }
            | SessionEvent::ToolCallError { .. }
            | SessionEvent::Patch { .. }
            | SessionEvent::Diagnostics { .. }
    )
}

fn format_stream_decode_error(
    provider: &str,
    error: &reqwest::Error,
    status: reqwest::StatusCode,
    content_type: Option<&str>,
    bytes_read: usize,
    events_parsed: usize,
    last_event_type: Option<&str>,
    emitted_visible_event: bool,
) -> String {
    let phase = if emitted_visible_event {
        "stream dropped after partial assistant output; resume to continue"
    } else {
        "stream failed before visible output; safe retry already attempted"
    };
    format!(
        "{provider} provider {phase}: {error}; http_status={status}; content_type={}; bytes_read={bytes_read}; events_parsed={events_parsed}; last_event_type={}; visible_output_emitted={emitted_visible_event}; source_chain={}",
        content_type.unwrap_or("<unknown>"),
        last_event_type.unwrap_or("<none>"),
        error_source_chain(error)
    )
}

fn error_source_chain(error: &dyn std::error::Error) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(err) = source {
        parts.push(err.to_string());
        source = err.source();
    }
    parts.join(" -> ")
}

fn sse_event_type(raw: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let data = line.strip_prefix("data:")?.trim();
        if data == "[DONE]" {
            return Some("[DONE]".to_string());
        }
        let value = serde_json::from_str::<Value>(data).ok()?;
        value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
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

#[cfg(test)]
fn parse_response_stream_event(
    session_id: harness_core::SessionId,
    raw: &str,
) -> Vec<SessionEvent> {
    let parsed = parse_response_stream_event_detail(session_id, raw);
    let mut events = parsed.events;
    if parsed.completed {
        events.push(SessionEvent::Result {
            session_id,
            stop_reason: StopReason::EndTurn,
        });
    }
    events
}

#[derive(Debug, Default)]
struct ParsedResponseStreamEvent {
    events: Vec<SessionEvent>,
    output_items: Vec<Value>,
    function_calls: Vec<PendingCodexFunctionCall>,
    completed: bool,
}

#[derive(Debug, Clone)]
struct PendingCodexFunctionCall {
    tool_call_id: ToolCallId,
    call_id: String,
    name: String,
}

fn parse_response_stream_event_detail(
    session_id: harness_core::SessionId,
    raw: &str,
) -> ParsedResponseStreamEvent {
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
        return ParsedResponseStreamEvent::default();
    }

    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return ParsedResponseStreamEvent::default();
    };
    let Some(event_type) = value
        .get("type")
        .and_then(Value::as_str)
        .or(event_name.as_deref())
    else {
        return ParsedResponseStreamEvent::default();
    };

    match event_type {
        "response.output_text.delta" | "output_text.delta" | "text_delta" => {
            let events = value
                .get("delta")
                .or_else(|| value.get("text"))
                .and_then(Value::as_str)
                .map(|text| SessionEvent::TextDelta {
                    session_id,
                    text: text.to_string(),
                })
                .into_iter()
                .collect();
            ParsedResponseStreamEvent {
                events,
                ..ParsedResponseStreamEvent::default()
            }
        }
        // A completed native function call. Emit a structured harness request;
        // the harness executes and permission-gates it without a text envelope.
        "response.output_item.done" | "response.output_item.added" => {
            let Some(item) = value.get("item") else {
                return ParsedResponseStreamEvent::default();
            };
            // `added` carries no arguments yet; only act on `done`.
            if event_type == "response.output_item.added" {
                return ParsedResponseStreamEvent::default();
            }
            let mut parsed = ParsedResponseStreamEvent::default();
            match item.get("type").and_then(Value::as_str) {
                // Our function tools -> structured harness execution.
                Some("function_call") => {
                    let Some(name) = item.get("name").and_then(Value::as_str) else {
                        return parsed;
                    };
                    let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                        parsed.events.push(SessionEvent::Error {
                            session_id,
                            message: "codex provider function_call is missing call_id".to_string(),
                        });
                        return parsed;
                    };
                    parsed
                        .output_items
                        .push(function_call_input_item(name, call_id, item));
                    let tool_call_id = ToolCallId::new();
                    let input = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|a| serde_json::from_str::<Value>(a).ok())
                        .unwrap_or_else(|| json!({}));
                    parsed.events.push(SessionEvent::ToolCallRequested {
                        session_id,
                        tool_call_id: tool_call_id.clone(),
                        name: name.to_string(),
                        input_json: input,
                    });
                    parsed.function_calls.push(PendingCodexFunctionCall {
                        tool_call_id,
                        call_id: call_id.to_string(),
                        name: name.to_string(),
                    });
                    parsed
                }
                // OpenAI-hosted web search runs server-side — surface it for
                // display only (no local execution / permission needed).
                Some("web_search_call") => {
                    let query = item
                        .pointer("/action/query")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    parsed.events.push(SessionEvent::ToolCallStart {
                        session_id,
                        tool_call_id: ToolCallId::new(),
                        name: "web_search".to_string(),
                        input_json: json!({ "query": query }),
                    });
                    parsed
                }
                _ => parsed,
            }
        }
        "response.completed" | "completed" | "done" => {
            // The completed event carries the real usage totals.
            let mut events = Vec::new();
            if let Some(usage) = usage_event(session_id, &value) {
                events.push(usage);
            }
            ParsedResponseStreamEvent {
                events,
                completed: true,
                ..ParsedResponseStreamEvent::default()
            }
        }
        "response.failed" | "error" => ParsedResponseStreamEvent {
            events: vec![SessionEvent::Error {
                session_id,
                message: value
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str)
                    .or_else(|| value.get("message").and_then(Value::as_str))
                    .unwrap_or("codex provider stream failed")
                    .to_string(),
            }],
            ..ParsedResponseStreamEvent::default()
        },
        _ => ParsedResponseStreamEvent::default(),
    }
}

fn function_call_input_item(name: &str, call_id: &str, item: &Value) -> Value {
    let arguments = item
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    json!({
        "type": "function_call",
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
    })
}

fn function_call_output_item(
    pending: &PendingCodexFunctionCall,
    response: ProviderToolResponse,
) -> Value {
    let output = if response.is_error {
        format!("error: {}", response.output)
    } else {
        response.output
    };
    json!({
        "type": "function_call_output",
        "call_id": pending.call_id,
        "output": output,
    })
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
    use futures_util::StreamExt;
    use harness_core::ImageAttachment;
    use harness_core::SessionId;
    use secrecy::SecretString;
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    };

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

    async fn read_http_body(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before headers completed");
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(index) = find_subsequence(&bytes, b"\r\n\r\n") {
                break index + 4;
            }
        };

        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        while bytes.len() < header_end + content_length {
            let mut chunk = [0u8; 1024];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before body completed");
            bytes.extend_from_slice(&chunk[..n]);
        }

        String::from_utf8(bytes[header_end..header_end + content_length].to_vec()).unwrap()
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    async fn write_sse_response(stream: &mut TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
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
    fn request_body_normalizes_reasoning_effort_aliases() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        for (input, expected) in [
            ("x_high", "xhigh"),
            ("x-high", "xhigh"),
            ("extra high", "xhigh"),
            ("max", "xhigh"),
            ("ultracode", "xhigh"),
            ("minimal", "minimal"),
            ("unknown", "medium"),
        ] {
            let mut request = text_request("think");
            request.metadata = json!({ "model_effort": input });
            let body = provider.request_body(&request);
            assert_eq!(body["reasoning"]["effort"], expected);
        }
    }

    #[test]
    fn request_body_clamps_effort_for_models_with_narrower_support() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        for (model, input, expected) in [
            ("gpt-5.1", "x_high", "high"),
            ("gpt-5-pro", "medium", "high"),
            ("o3", "minimal", "medium"),
            ("codex-mini-latest", "xhigh", "medium"),
            (DEFAULT_CODEX_MODEL, "x_high", "xhigh"),
        ] {
            let mut request = text_request("think");
            request.model = model.to_string();
            request.metadata = json!({ "model_effort": input });
            let body = provider.request_body(&request);
            assert_eq!(
                body["reasoning"]["effort"], expected,
                "unexpected effort for {model}"
            );
        }
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
        assert_eq!(body["input"][0]["content"][0]["type"], "output_text");
        assert_eq!(body["input"][0]["content"][0]["annotations"], json!([]));
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
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
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][2]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][3]["content"][0]["type"], "output_text");
    }

    #[test]
    fn request_body_skips_empty_stored_messages() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let mut request = text_request("legacy prompt");
        request.messages = vec![
            ModelMessage::text("user", "resume"),
            ModelMessage::text("assistant", ""),
            ModelMessage::text("assistant", "done"),
        ];

        let body = provider.request_body(&request);
        let input = body["input"].as_array().unwrap();

        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "done");
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
    fn function_call_detail_sanitizes_response_item_and_retains_call_id() {
        let session_id = SessionId::new();
        let raw = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","name":"write_file","call_id":"call_1","arguments":"{\"path\":\"a.txt\",\"content\":\"hi\"}"}}
"#;

        let parsed = parse_response_stream_event_detail(session_id, raw);

        assert_eq!(parsed.output_items.len(), 1);
        assert_eq!(parsed.output_items[0]["type"], "function_call");
        assert!(parsed.output_items[0].get("id").is_none());
        assert_eq!(parsed.function_calls.len(), 1);
        assert_eq!(parsed.function_calls[0].call_id, "call_1");
        assert_eq!(parsed.function_calls[0].name, "write_file");
        assert!(matches!(
            parsed.events.first().unwrap(),
            SessionEvent::ToolCallRequested { tool_call_id, .. }
                if tool_call_id == &parsed.function_calls[0].tool_call_id
        ));
    }

    #[test]
    fn reasoning_output_items_are_not_replayed_with_store_false() {
        let session_id = SessionId::new();
        let raw = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_0601405c3a9a119e016a30d89a680c8199a65219609d8df619","summary":[]}}
"#;

        let parsed = parse_response_stream_event_detail(session_id, raw);

        assert!(parsed.output_items.is_empty());
        assert!(parsed.function_calls.is_empty());
        assert!(parsed.events.is_empty());
    }

    #[test]
    fn function_call_output_uses_codex_call_id() {
        let pending = PendingCodexFunctionCall {
            tool_call_id: ToolCallId::new(),
            call_id: "call_1".to_string(),
            name: "read_file".to_string(),
        };
        let output = function_call_output_item(
            &pending,
            ProviderToolResponse {
                tool_call_id: pending.tool_call_id.clone(),
                output: "file contents".to_string(),
                is_error: false,
            },
        );

        assert_eq!(output["type"], "function_call_output");
        assert_eq!(output["call_id"], "call_1");
        assert_eq!(output["output"], "file contents");
    }

    #[test]
    fn function_call_output_marks_tool_errors() {
        let pending = PendingCodexFunctionCall {
            tool_call_id: ToolCallId::new(),
            call_id: "call_1".to_string(),
            name: "read_file".to_string(),
        };
        let output = function_call_output_item(
            &pending,
            ProviderToolResponse {
                tool_call_id: pending.tool_call_id.clone(),
                output: "not found".to_string(),
                is_error: true,
            },
        );

        assert_eq!(output["output"], "error: not found");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stream_turn_continues_after_failed_tool_result() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let server = tokio::spawn(async move {
            let responses = [
                r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"checking"}

event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"reasoning","id":"rs_0601405c3a9a119e016a30d89a680c8199a65219609d8df619","summary":[]}}

event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","name":"list_dir","call_id":"call_1","arguments":"{}"}}

event: response.completed
data: {"type":"response.completed","response":{"usage":{"input_tokens":10,"output_tokens":3}}}

data: [DONE]

"#,
                r#"event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"continued after failure"}

event: response.completed
data: {"type":"response.completed","response":{"usage":{"input_tokens":12,"output_tokens":4}}}

data: [DONE]

"#,
            ];

            for body in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request_body = read_http_body(&mut stream).await;
                request_tx.send(request_body).unwrap();
                write_sse_response(&mut stream, body).await;
            }
        });

        let provider = CodexProvider::with_base_url(format!("http://{addr}")).unwrap();
        let auth = ProviderAuth::new(
            ProviderAuthKind::SessionToken,
            SecretString::from("secret-token".to_string()),
        );
        let (tool_tx, tool_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut stream = provider
            .stream_turn(
                &auth,
                text_request("check all tools"),
                CancellationToken::new(),
                provider_core::empty_permission_responses(),
                tool_rx,
                provider_core::empty_question_responses(),
                provider_core::empty_question_requests(),
            )
            .await
            .unwrap();

        let mut saw_tool_call = false;
        let mut saw_continuation = false;
        let mut saw_result = false;
        timeout(Duration::from_secs(5), async {
            while let Some(event) = stream.next().await {
                let event = event.unwrap();
                match event {
                    SessionEvent::ToolCallRequested {
                        tool_call_id,
                        name,
                        input_json,
                        ..
                    } => {
                        assert_eq!(name, "list_dir");
                        assert_eq!(input_json, json!({}));
                        saw_tool_call = true;
                        tool_tx
                            .send(ProviderToolResponse {
                                tool_call_id,
                                output: "list_dir failed".to_string(),
                                is_error: true,
                            })
                            .unwrap();
                    }
                    SessionEvent::TextDelta { text, .. } => {
                        if text == "continued after failure" {
                            saw_continuation = true;
                        }
                    }
                    SessionEvent::Result { .. } => {
                        saw_result = true;
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();

        assert!(saw_tool_call);
        assert!(saw_continuation);
        assert!(saw_result);

        let first_request: Value = serde_json::from_str(&request_rx.recv().await.unwrap()).unwrap();
        let second_request: Value =
            serde_json::from_str(&request_rx.recv().await.unwrap()).unwrap();
        assert_eq!(first_request["input"][0]["role"], "user");

        let second_input = second_request["input"].as_array().unwrap();
        assert!(second_input.iter().any(|item| {
            item["type"] == "function_call"
                && item["name"] == "list_dir"
                && item["call_id"] == "call_1"
                && item.get("id").is_none()
        }));
        assert!(
            !second_input
                .iter()
                .any(|item| item["type"] == "reasoning" || item["id"].as_str().is_some())
        );
        let output = second_input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["call_id"], "call_1");
        assert_eq!(output["output"], "error: list_dir failed");

        server.await.unwrap();
    }

    #[test]
    fn request_body_advertises_function_tools() {
        let provider = CodexProvider::with_base_url("https://example.test").unwrap();
        let body = provider.request_body(&text_request("hi"));
        let tools = body["tools"].as_array().unwrap();
        assert!(tools.iter().any(|t| t["name"] == "list_dir"));
        assert!(tools.iter().any(|t| t["name"] == "glob"));
        assert!(tools.iter().any(|t| t["name"] == "apply_patch"));
        assert!(!tools.iter().any(|t| t["name"] == "write_file"));
        assert!(!tools.iter().any(|t| t["name"] == "edit_file"));
        assert!(!tools.iter().any(|t| t["name"] == "multi_edit"));
        assert!(!tools.iter().any(|t| t["name"] == "apply_patch_structured"));
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
    fn completion_detail_does_not_emit_final_result() {
        let session_id = SessionId::new();
        let raw = r#"event: response.completed
data: {"type":"response.completed"}
"#;

        let parsed = parse_response_stream_event_detail(session_id, raw);

        assert!(parsed.completed);
        assert!(
            !parsed
                .events
                .iter()
                .any(|event| matches!(event, SessionEvent::Result { .. }))
        );
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
