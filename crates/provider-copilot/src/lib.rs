use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_stream::try_stream;
use futures_core::Stream;
use futures_util::StreamExt;
use harness_core::{
    MessagePart, ModelInfo, ModelMessage, ProviderCapabilities, SessionEvent, SessionStatus,
    StopReason, ToolCallId, TurnRequest,
};
use provider_core::{
    PermissionResponses, ProviderAuth, ProviderAuthKind, ProviderPlugin, ProviderToolResponse,
    ToolResponses,
};
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

const DEFAULT_COPILOT_API_URL: &str = "https://api.githubcopilot.com";
const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";
const DEFAULT_COPILOT_MODEL: &str = "gpt-4.1";
const DEFAULT_EDITOR_VERSION: &str = "vscode/1.95.0";
const DEFAULT_INTEGRATION_ID: &str = "vscode-chat";
const DEFAULT_USER_AGENT: &str = "GithubCopilot/1.0";
const DEFAULT_COPILOT_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_COPILOT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_COPILOT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct CopilotProvider {
    client: reqwest::Client,
    github_api_url: String,
    copilot_api_url: String,
    token_cache: Arc<Mutex<Option<CopilotBearerToken>>>,
}

#[derive(Debug, Clone)]
struct CopilotBearerToken {
    token: String,
    expires_at: u64,
}

impl CopilotProvider {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_urls(
            std::env::var("INDUCTOR_GITHUB_API_URL")
                .unwrap_or_else(|_| DEFAULT_GITHUB_API_URL.to_string()),
            std::env::var("INDUCTOR_COPILOT_API_URL")
                .unwrap_or_else(|_| DEFAULT_COPILOT_API_URL.to_string()),
        )
    }

    pub fn with_urls(
        github_api_url: impl Into<String>,
        copilot_api_url: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(DEFAULT_COPILOT_CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            client,
            github_api_url: trim_trailing_slash(github_api_url.into()),
            copilot_api_url: trim_trailing_slash(copilot_api_url.into()),
            token_cache: Arc::new(Mutex::new(None)),
        })
    }

    fn token_url(&self) -> String {
        format!("{}/copilot_internal/v2/token", self.github_api_url)
    }

    fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.copilot_api_url)
    }

    fn models_url(&self) -> String {
        format!("{}/models", self.copilot_api_url)
    }

    #[cfg(test)]
    fn chat_body(&self, req: &TurnRequest, messages: Vec<Value>) -> Value {
        self.chat_body_with_messages(req, messages)
    }

    fn chat_body_with_messages(&self, req: &TurnRequest, messages: Vec<Value>) -> Value {
        json!({
            "model": normalize_copilot_model(&req.model),
            "messages": messages,
            "tools": copilot_tools(&req.tool_names),
            "tool_choice": "auto",
            "stream": true,
            "temperature": 0.0
        })
    }

    async fn bearer_token(&self, auth: &ProviderAuth) -> anyhow::Result<CopilotBearerToken> {
        if let Some(token) = self.cached_bearer_token() {
            return Ok(token);
        }

        let response = self
            .client
            .get(self.token_url())
            .headers(github_token_headers(auth)?)
            .timeout(DEFAULT_COPILOT_REQUEST_TIMEOUT)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "copilot token exchange failed with HTTP {status}: {}",
                redact_error_body(&body)
            );
        }
        let payload: CopilotTokenResponse = serde_json::from_str(&body)?;
        let token = CopilotBearerToken {
            token: payload.token,
            expires_at: payload.expires_at,
        };
        *self.token_cache.lock().unwrap() = Some(token.clone());
        Ok(token)
    }

    fn cached_bearer_token(&self) -> Option<CopilotBearerToken> {
        let token = self.token_cache.lock().unwrap().clone()?;
        (token.expires_at > unix_now().saturating_add(60)).then_some(token)
    }
}

#[derive(Debug, Deserialize)]
struct CopilotTokenResponse {
    token: String,
    expires_at: u64,
}

#[async_trait::async_trait]
impl ProviderPlugin for CopilotProvider {
    fn id(&self) -> &'static str {
        "copilot"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: true,
        }
    }

    async fn list_models(&self, auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        let bearer = self.bearer_token(auth).await?;
        let response = self
            .client
            .get(self.models_url())
            .headers(copilot_headers(&bearer.token)?)
            .timeout(DEFAULT_COPILOT_REQUEST_TIMEOUT)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "copilot model list failed with HTTP {status}: {}",
                redact_error_body(&body)
            );
        }
        let value = serde_json::from_str::<Value>(&body)?;
        let models = value
            .get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|model| copilot_model_is_chat_capable(model))
            .filter_map(|model| {
                let id = model.get("id").and_then(Value::as_str)?;
                Some(ModelInfo {
                    id: id.to_string(),
                    display_name: copilot_model_display_name(model).unwrap_or(id).to_string(),
                    context_window: model
                        .get("model_picker_context_window")
                        .or_else(|| model.get("context_window"))
                        .and_then(Value::as_u64),
                })
            })
            .collect::<Vec<_>>();
        if models.is_empty() {
            Ok(copilot_model_catalog())
        } else {
            Ok(models)
        }
    }

    async fn stream_turn(
        &self,
        auth: &ProviderAuth,
        req: TurnRequest,
        cancel: CancellationToken,
        _permissions: PermissionResponses,
        mut tool_responses: ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let bearer = self.bearer_token(auth).await?;
        let mut headers = copilot_headers(&bearer.token)?;
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        let provider = self.clone();
        let session_id = req.session_id;
        let idle_timeout = copilot_idle_timeout();

        let stream = try_stream! {
            yield SessionEvent::Status {
                session_id,
                status: SessionStatus::Starting,
            };

            let mut messages = copilot_messages(&req);
            loop {
                let mut safe_stream_retries_remaining = 1usize;
                'request_attempt: loop {
                let request = provider
                    .client
                    .post(provider.chat_url())
                    .headers(headers.clone())
                    .json(&provider.chat_body_with_messages(&req, messages.clone()));

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
                                "Copilot provider produced no response for {} seconds; stopped the stale run",
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
                        message: format!(
                            "copilot provider request failed with HTTP {status}: {}",
                            redact_error_body(&body)
                        ),
                    };
                    return;
                }

                yield SessionEvent::Status {
                    session_id,
                    status: SessionStatus::Streaming,
                };

                let mut bytes = response.bytes_stream();
                let mut buffer = String::new();
                let mut assistant = PendingAssistantMessage::default();
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
                                    "Copilot provider stream produced no events for {} seconds; stopped the stale run",
                                    idle_timeout.as_secs()
                                ),
                            };
                            return;
                        }
                    };

                    let Some(chunk) = chunk else {
                        break;
                    };

                    let chunk = match chunk {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            let diagnostic = format_stream_decode_error(
                                "Copilot",
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
                        let parsed = parse_chat_stream_event(session_id, &event, &mut assistant);
                        for mapped in parsed.events {
                            emitted_visible_event |= provider_event_is_visible(&mapped);
                            yield mapped;
                        }
                        if parsed.done {
                            break;
                        }
                    }
                }

                if retry_request {
                    continue 'request_attempt;
                }

                for event in drain_sse_events_at_eof(&mut buffer) {
                    let parsed = parse_chat_stream_event(session_id, &event, &mut assistant);
                    for mapped in parsed.events {
                        yield mapped;
                    }
                }

                for mapped in assistant.emit_ready_tool_requests(session_id) {
                    yield mapped;
                }

                let pending_tool_calls = assistant.requested_tool_calls();
                if pending_tool_calls.is_empty() {
                    yield SessionEvent::Result {
                        session_id,
                        stop_reason: StopReason::EndTurn,
                    };
                    return;
                }

                messages.push(assistant.to_chat_message());
                for pending in pending_tool_calls {
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
                                message: format!("copilot provider lost local tool result for {}", pending.name),
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
                    messages.push(tool_result_message(&pending, tool_result));
                }
                break 'request_attempt;
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

fn copilot_messages(req: &TurnRequest) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(system_prompt) = req.system_prompt.as_deref() {
        messages.push(json!({
            "role": "system",
            "content": system_prompt,
        }));
    }

    if req.messages.is_empty() {
        let mut content = vec![json!({
            "type": "text",
            "text": req.prompt,
        })];
        content.extend(req.images.iter().map(|image| {
            json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", image.mime_type, image.base64_data)
                }
            })
        }));
        messages.push(json!({
            "role": "user",
            "content": content,
        }));
        return messages;
    }

    messages.extend(req.messages.iter().filter_map(copilot_message));
    messages
}

fn copilot_message(message: &ModelMessage) -> Option<Value> {
    let role = copilot_role(&message.role);
    let content = message.parts.iter().map(copilot_part).collect::<Vec<_>>();
    if content.iter().all(copilot_part_is_empty_text) {
        return None;
    }
    Some(json!({
        "role": role,
        "content": content,
    }))
}

fn copilot_role(role: &str) -> &'static str {
    match role.to_ascii_lowercase().as_str() {
        "assistant" => "assistant",
        "system" | "developer" => "system",
        "tool" | "user" => "user",
        _ => "user",
    }
}

fn copilot_part(part: &MessagePart) -> Value {
    match part {
        MessagePart::Text { text } => json!({
            "type": "text",
            "text": text,
        }),
        MessagePart::Image { image } => json!({
            "type": "image_url",
            "image_url": {
                "url": format!("data:{};base64,{}", image.mime_type, image.base64_data)
            },
        }),
    }
}

fn copilot_part_is_empty_text(part: &Value) -> bool {
    part.get("text")
        .and_then(Value::as_str)
        .is_some_and(|text| text.trim().is_empty())
}

fn copilot_tools(tool_names: &[String]) -> Value {
    let allowed = tool_names
        .iter()
        .map(String::as_str)
        .collect::<std::collections::HashSet<_>>();
    Value::Array(
        tools::tool_definitions()
            .into_iter()
            .filter(|definition| allowed.contains(definition.name.as_str()))
            .map(|definition| {
                json!({
                    "type": "function",
                    "function": {
                        "name": definition.name.as_str(),
                        "description": definition.description,
                        "parameters": definition.input_schema,
                    }
                })
            })
            .collect(),
    )
}

#[derive(Debug, Default)]
struct PendingAssistantMessage {
    content: String,
    tool_calls: Vec<PendingCopilotToolCall>,
}

impl PendingAssistantMessage {
    fn tool_call_mut(&mut self, index: usize, id: Option<&str>) -> &mut PendingCopilotToolCall {
        while self.tool_calls.len() <= index {
            self.tool_calls.push(PendingCopilotToolCall {
                tool_call_id: ToolCallId::new(),
                provider_id: String::new(),
                name: String::new(),
                arguments: String::new(),
                requested: false,
            });
        }
        let call = &mut self.tool_calls[index];
        if let Some(id) = id
            && !id.is_empty()
        {
            call.provider_id = id.to_string();
        }
        call
    }

    fn emit_ready_tool_requests(
        &mut self,
        session_id: harness_core::SessionId,
    ) -> Vec<SessionEvent> {
        self.tool_calls
            .iter_mut()
            .filter_map(|call| {
                if call.requested || !call.is_ready() {
                    return None;
                }
                call.requested = true;
                let input =
                    serde_json::from_str::<Value>(&call.arguments).unwrap_or_else(|_| json!({}));
                Some(SessionEvent::ToolCallRequested {
                    session_id,
                    tool_call_id: call.tool_call_id,
                    name: call.name.clone(),
                    input_json: input,
                })
            })
            .collect()
    }

    fn requested_tool_calls(&self) -> Vec<PendingCopilotToolCall> {
        self.tool_calls
            .iter()
            .filter(|call| call.requested && call.is_ready())
            .cloned()
            .collect()
    }

    fn to_chat_message(&self) -> Value {
        let tool_calls = self.requested_tool_calls();
        json!({
            "role": "assistant",
            "content": if self.content.is_empty() { Value::Null } else { Value::String(self.content.clone()) },
            "tool_calls": tool_calls.iter().map(|call| {
                json!({
                    "id": call.provider_id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": if call.arguments.trim().is_empty() { "{}" } else { call.arguments.as_str() },
                    }
                })
            }).collect::<Vec<_>>()
        })
    }
}

#[derive(Debug, Clone)]
struct PendingCopilotToolCall {
    tool_call_id: ToolCallId,
    provider_id: String,
    name: String,
    arguments: String,
    requested: bool,
}

impl PendingCopilotToolCall {
    fn is_ready(&self) -> bool {
        !self.provider_id.trim().is_empty() && !self.name.trim().is_empty()
    }
}

#[derive(Debug, Default)]
struct ParsedChatStreamEvent {
    events: Vec<SessionEvent>,
    done: bool,
}

fn parse_chat_stream_event(
    session_id: harness_core::SessionId,
    raw: &str,
    assistant: &mut PendingAssistantMessage,
) -> ParsedChatStreamEvent {
    let mut data_lines = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.trim().to_string());
        }
    }

    let data = data_lines.join("\n");
    if data.is_empty() {
        return ParsedChatStreamEvent::default();
    }
    if data == "[DONE]" {
        return ParsedChatStreamEvent {
            done: true,
            ..ParsedChatStreamEvent::default()
        };
    }

    let Ok(value) = serde_json::from_str::<Value>(&data) else {
        return ParsedChatStreamEvent::default();
    };
    if let Some(error) = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        return ParsedChatStreamEvent {
            events: vec![SessionEvent::Error {
                session_id,
                message: error.to_string(),
            }],
            ..ParsedChatStreamEvent::default()
        };
    }

    let mut parsed = ParsedChatStreamEvent::default();
    if let Some(usage) = usage_event(session_id, &value) {
        parsed.events.push(usage);
    }

    for choice in value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                assistant.content.push_str(content);
                parsed.events.push(SessionEvent::TextDelta {
                    session_id,
                    text: content.to_string(),
                });
            }
            for tool_call in delta
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let id = tool_call.get("id").and_then(Value::as_str);
                let call = assistant.tool_call_mut(index, id);
                if let Some(name) = tool_call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                {
                    call.name.push_str(name);
                }
                if let Some(arguments) = tool_call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                {
                    call.arguments.push_str(arguments);
                }
            }
        }
        if choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .is_some_and(|reason| reason == "tool_calls")
        {
            parsed
                .events
                .extend(assistant.emit_ready_tool_requests(session_id));
        }
    }

    parsed
}

fn tool_result_message(pending: &PendingCopilotToolCall, response: ProviderToolResponse) -> Value {
    let content = if response.is_error {
        format!("error: {}", response.output)
    } else {
        response.output
    };
    json!({
        "role": "tool",
        "tool_call_id": pending.provider_id,
        "content": content,
    })
}

fn usage_event(session_id: harness_core::SessionId, value: &Value) -> Option<SessionEvent> {
    let usage = value.get("usage")?;
    let num = |key: &str| usage.get(key).and_then(Value::as_u64);
    Some(SessionEvent::Usage {
        session_id,
        input_tokens: num("prompt_tokens").or_else(|| num("input_tokens")),
        output_tokens: num("completion_tokens").or_else(|| num("output_tokens")),
        cache_read_tokens: None,
        total_cost_usd: None,
    })
}

fn github_token_headers(auth: &ProviderAuth) -> anyhow::Result<HeaderMap> {
    match auth.kind() {
        ProviderAuthKind::ApiKey
        | ProviderAuthKind::BearerToken
        | ProviderAuthKind::SessionToken
        | ProviderAuthKind::Unknown => {}
    }
    let mut headers = shared_headers()?;
    let mut value = HeaderValue::from_str(&format!("token {}", auth.expose_secret()))?;
    value.set_sensitive(true);
    headers.insert(AUTHORIZATION, value);
    Ok(headers)
}

fn copilot_headers(token: &str) -> anyhow::Result<HeaderMap> {
    let mut headers = shared_headers()?;
    let mut value = HeaderValue::from_str(&format!("Bearer {token}"))?;
    value.set_sensitive(true);
    headers.insert(AUTHORIZATION, value);
    Ok(headers)
}

fn shared_headers() -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(USER_AGENT, HeaderValue::from_static(DEFAULT_USER_AGENT));
    headers.insert("Editor-Version", editor_version_header()?);
    headers.insert("Copilot-Integration-Id", copilot_integration_header()?);
    Ok(headers)
}

fn editor_version_header() -> anyhow::Result<HeaderValue> {
    Ok(HeaderValue::from_str(
        &std::env::var("INDUCTOR_COPILOT_EDITOR_VERSION")
            .unwrap_or_else(|_| DEFAULT_EDITOR_VERSION.to_string()),
    )?)
}

fn copilot_integration_header() -> anyhow::Result<HeaderValue> {
    Ok(HeaderValue::from_str(
        &std::env::var("INDUCTOR_COPILOT_INTEGRATION_ID")
            .unwrap_or_else(|_| DEFAULT_INTEGRATION_ID.to_string()),
    )?)
}

fn copilot_model_is_chat_capable(model: &Value) -> bool {
    let policy_enabled = model
        .pointer("/model_policy/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let supports_chat = model
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|capabilities| {
            capabilities
                .iter()
                .filter_map(Value::as_str)
                .any(|capability| capability.eq_ignore_ascii_case("chat"))
        })
        .unwrap_or(true);
    policy_enabled && supports_chat
}

fn copilot_model_display_name(model: &Value) -> Option<&str> {
    model
        .get("name")
        .or_else(|| model.get("display_name"))
        .and_then(Value::as_str)
}

fn copilot_model_catalog() -> Vec<ModelInfo> {
    let mut models = vec![
        ModelInfo {
            id: DEFAULT_COPILOT_MODEL.to_string(),
            display_name: "GPT-4.1".to_string(),
            context_window: None,
        },
        ModelInfo {
            id: "claude-sonnet-4".to_string(),
            display_name: "Claude Sonnet 4".to_string(),
            context_window: None,
        },
        ModelInfo {
            id: "o4-mini".to_string(),
            display_name: "o4-mini".to_string(),
            context_window: None,
        },
    ];
    extend_models_from_env(&mut models, "INDUCTOR_COPILOT_MODELS");
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

fn normalize_copilot_model(model: &str) -> &str {
    let model = model.trim();
    if model.is_empty() {
        DEFAULT_COPILOT_MODEL
    } else {
        model
    }
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

fn copilot_idle_timeout() -> Duration {
    std::env::var("INDUCTOR_COPILOT_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_COPILOT_IDLE_TIMEOUT)
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

#[allow(clippy::too_many_arguments)]
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
            .get("object")
            .or_else(|| value.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn redact_error_body(body: &str) -> String {
    const MAX_ERROR_BODY_CHARS: usize = 2_000;
    body.chars()
        .take(MAX_ERROR_BODY_CHARS)
        .collect::<String>()
        .replace("Bearer ", "Bearer <redacted>")
        .replace("token ", "token <redacted>")
}

fn trim_trailing_slash(value: String) -> String {
    value.trim_end_matches('/').to_string()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::SessionId;

    fn text_request(prompt: &str) -> TurnRequest {
        TurnRequest {
            session_id: SessionId::new(),
            model: "gpt-test".to_string(),
            prompt: prompt.to_string(),
            system_prompt: Some("system".to_string()),
            messages: Vec::new(),
            tool_names: tools::tool_names(),
            metadata: Value::Null,
            images: Vec::new(),
        }
    }

    #[test]
    fn chat_body_uses_openai_compatible_tools() {
        let provider =
            CopilotProvider::with_urls("https://api.github.test", "https://copilot.test").unwrap();
        let body = provider.chat_body(
            &text_request("hello"),
            copilot_messages(&text_request("hello")),
        );

        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["tools"][0]["type"], "function");
        assert!(body["tools"][0]["function"]["name"].is_string());
    }

    #[test]
    fn chat_stream_maps_text_and_tool_calls() {
        let session_id = SessionId::new();
        let mut assistant = PendingAssistantMessage::default();

        let text = r#"data: {"choices":[{"delta":{"content":"hi"}}]}"#;
        let parsed = parse_chat_stream_event(session_id, text, &mut assistant);
        assert_eq!(
            parsed.events,
            vec![SessionEvent::TextDelta {
                session_id,
                text: "hi".to_string(),
            }]
        );

        let call_start = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\""}}]}}]}"#;
        parse_chat_stream_event(session_id, call_start, &mut assistant);
        let call_end = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Cargo.toml\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let parsed = parse_chat_stream_event(session_id, call_end, &mut assistant);

        assert!(matches!(
            parsed.events.first(),
            Some(SessionEvent::ToolCallRequested { name, input_json, .. })
                if name == "read_file" && input_json["path"] == "Cargo.toml"
        ));
    }

    #[test]
    fn pending_tool_calls_are_emitted_at_stream_end_without_finish_reason() {
        let session_id = SessionId::new();
        let mut assistant = PendingAssistantMessage::default();

        let call_start = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{\"path\""}}]}}]}"#;
        parse_chat_stream_event(session_id, call_start, &mut assistant);
        let call_end = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":\"Cargo.toml\"}"}}]}}]}"#;
        let parsed = parse_chat_stream_event(session_id, call_end, &mut assistant);
        assert!(parsed.events.is_empty());

        let events = assistant.emit_ready_tool_requests(session_id);
        assert!(matches!(
            events.first(),
            Some(SessionEvent::ToolCallRequested { name, input_json, .. })
                if name == "read_file" && input_json["path"] == "Cargo.toml"
        ));
        assert!(assistant.emit_ready_tool_requests(session_id).is_empty());
    }

    #[test]
    fn assistant_message_only_includes_requested_complete_tool_calls() {
        let session_id = SessionId::new();
        let mut assistant = PendingAssistantMessage::default();

        let ready = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read_file","arguments":"{}"}}]}}]}"#;
        parse_chat_stream_event(session_id, ready, &mut assistant);
        let incomplete = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":1,"type":"function","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#;
        parse_chat_stream_event(session_id, incomplete, &mut assistant);
        assistant.emit_ready_tool_requests(session_id);

        let message = assistant.to_chat_message();
        let tool_calls = message["tool_calls"].as_array().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["function"]["name"], "read_file");
    }

    #[test]
    fn done_sse_marks_stream_done() {
        let mut assistant = PendingAssistantMessage::default();
        let parsed = parse_chat_stream_event(SessionId::new(), "data: [DONE]", &mut assistant);
        assert!(parsed.done);
    }
}
