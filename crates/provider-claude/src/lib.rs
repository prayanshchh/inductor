use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex},
};

use async_stream::try_stream;
use futures_core::Stream;
use harness_core::{
    ImageAttachment, ModelInfo, ModelMessage, PermissionDecision, PermissionRequestId,
    ProviderCapabilities, SessionEvent, SessionId, SessionStatus, StopReason, ToolCallId,
    TurnRequest,
};
use provider_core::{PermissionResponses, ProviderAuth, ProviderPlugin, ProviderToolResponse};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    time::{Duration, Instant, sleep_until, timeout},
};
use tokio_util::sync::CancellationToken;

const DEFAULT_CLAUDE_MODEL: &str = "sonnet";
const DEFAULT_BRIDGE_SCRIPT: &str = "js/claude_agent_sdk_bridge.mjs";
const DEFAULT_CLAUDE_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_CLAUDE_CANCEL_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct ClaudeProvider {
    command: BridgeCommand,
    cwd: PathBuf,
}

impl ClaudeProvider {
    pub fn new() -> anyhow::Result<Self> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let command = BridgeCommand::from_env(&manifest_dir);
        let cwd = std::env::current_dir()?;
        Ok(Self { command, cwd })
    }

    pub fn with_cwd(cwd: PathBuf) -> Self {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        Self {
            command: BridgeCommand::from_env(&manifest_dir),
            cwd,
        }
    }

    pub fn with_command(command: impl Into<String>, args: Vec<String>, cwd: PathBuf) -> Self {
        Self {
            command: BridgeCommand {
                command: command.into(),
                args,
            },
            cwd,
        }
    }

    /// Ask the Claude Agent SDK whether it currently has a usable login.
    ///
    /// This spawns the bridge in `check_auth` mode, which runs a minimal probe
    /// query. Inductor never reads the credential itself; it only observes
    /// whether the SDK can reach a non-error result.
    pub async fn check_auth(&self) -> anyhow::Result<AuthCheck> {
        let mut bridge = SdkBridge::spawn(self.command.clone(), &self.cwd).await?;
        bridge
            .send_value(&json!({ "mode": "check_auth", "cwd": self.cwd }))
            .await?;

        while let Some(value) = bridge.read_event().await? {
            match value.get("type").and_then(Value::as_str) {
                Some("auth_check") => {
                    return Ok(AuthCheck {
                        ok: value.get("ok").and_then(Value::as_bool).unwrap_or(false),
                        error: value
                            .get("error")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
                Some("error") => {
                    return Ok(AuthCheck {
                        ok: false,
                        error: value
                            .get("message")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    });
                }
                // Ignore any text/result chatter from the probe query.
                _ => continue,
            }
        }

        let diagnostics = bridge.exit_diagnostics().await;
        Ok(AuthCheck {
            ok: false,
            error: Some(format!(
                "Claude Agent SDK bridge exited before reporting auth status{diagnostics}"
            )),
        })
    }
}

/// Result of probing the Claude Agent SDK login.
#[derive(Debug, Clone)]
pub struct AuthCheck {
    pub ok: bool,
    pub error: Option<String>,
}

#[async_trait::async_trait]
impl ProviderPlugin for ClaudeProvider {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: true,
        }
    }

    async fn list_models(&self, _auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(claude_model_catalog())
    }

    async fn stream_turn(
        &self,
        _auth: &ProviderAuth,
        req: TurnRequest,
        cancel: CancellationToken,
        mut permissions: PermissionResponses,
        mut tool_responses: provider_core::ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let command = self.command.clone();
        let cwd = self.cwd.clone();
        let session_id = req.session_id;
        let prompt = req.prompt;
        let system_prompt = req.system_prompt;
        let messages = req.messages;
        let images = req.images;
        let tool_names = req.tool_names;
        let approval_policy = req
            .metadata
            .get("approval_policy")
            .and_then(Value::as_str)
            .unwrap_or("never")
            .to_string();
        let model = normalize_claude_model(&req.model).to_string();
        let idle_timeout = claude_idle_timeout();

        let stream = try_stream! {
            yield SessionEvent::Status {
                session_id,
                status: SessionStatus::Starting,
            };

            let mut bridge = SdkBridge::spawn(command, &cwd).await?;
            let request = BridgeRequest {
                prompt,
                cwd,
                model,
                messages,
                images,
                system_prompt,
                approval_policy,
                tool_names,
            };
            if let Err(error) = timeout(idle_timeout, bridge.send_request(&request)).await
                .unwrap_or_else(|_| Err(anyhow::anyhow!(
                    "timed out after {} seconds writing initial request to Claude Agent SDK bridge",
                    idle_timeout.as_secs()
                )))
            {
                bridge.cancel_with_grace(DEFAULT_CLAUDE_CANCEL_GRACE).await;
                yield SessionEvent::Error {
                    session_id,
                    message: error.to_string(),
                };
                return;
            }

            yield SessionEvent::Status {
                session_id,
                status: SessionStatus::Streaming,
            };

            // Maps our minted PermissionRequestId back to the SDK's tool-use id
            // so a decision can be routed to the right paused tool call.
            let mut pending: HashMap<String, String> = HashMap::new();
            let mut pending_tools: HashMap<String, String> = HashMap::new();
            // Maps the SDK's tool-use id to our ToolCallId so start/result pair up.
            let mut tool_ids: HashMap<String, ToolCallId> = HashMap::new();
            let mut perms_open = true;
            let mut tool_results_open = true;
            let mut last_activity = Instant::now();

            loop {
                if cancel.is_cancelled() {
                    bridge.cancel_with_grace(DEFAULT_CLAUDE_CANCEL_GRACE).await;
                    yield SessionEvent::Result {
                        session_id,
                        stop_reason: StopReason::Interrupted,
                    };
                    return;
                }

                // Wait for either the next bridge message or a user decision.
                // `?` is used after the select (try_stream only supports it at the
                // block level, not inside select arms).
                let step = tokio::select! {
                    _ = cancel.cancelled() => Step::Cancelled,
                    read = bridge.read_event() => Step::Read(read),
                    resp = permissions.recv(), if perms_open => Step::Decision(resp),
                    tool = tool_responses.recv(), if tool_results_open => Step::ToolResult(tool),
                    _ = sleep_until(last_activity + idle_timeout) => Step::IdleTimeout,
                };

                match step {
                    // A message from the bridge (text, result, error, usage, or a
                    // permission request raised by the SDK's canUseTool callback).
                    Step::Read(read) => {
                        last_activity = Instant::now();
                        let Some(value) = read? else {
                            let diagnostics = bridge.exit_diagnostics().await;
                            yield SessionEvent::Error {
                                session_id,
                                message: format!(
                                    "Claude Agent SDK bridge exited before returning a result{diagnostics}"
                                ),
                            };
                            return;
                        };

                        if value.get("type").and_then(Value::as_str) == Some("permission_request") {
                            let bridge_id = value
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let tool_name = value
                                .get("tool_name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let reason = value
                                .get("title")
                                .or_else(|| value.get("description"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| format!("Claude wants to use {tool_name}"));
                            let input_json = value.get("input").cloned().unwrap_or(Value::Null);

                            let request_id = PermissionRequestId::new();
                            pending.insert(request_id.to_string(), bridge_id);

                            yield SessionEvent::Status {
                                session_id,
                                status: SessionStatus::WaitingForPermission,
                            };
                            yield SessionEvent::PermissionRequest {
                                session_id,
                                request_id,
                                reason,
                                tool_name,
                                input_json,
                            };
                            continue;
                        }

                        if value.get("type").and_then(Value::as_str) == Some("tool_request") {
                            let bridge_id = value
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let name = value
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let input_json = value.get("input").cloned().unwrap_or(Value::Null);
                            let tool_call_id = ToolCallId::new();
                            pending_tools.insert(tool_call_id.to_string(), bridge_id);
                            yield SessionEvent::ToolCallRequested {
                                session_id,
                                tool_call_id,
                                name,
                                input_json,
                            };
                            continue;
                        }

                        // Native SDK tool use → ToolCallStart for display.
                        if value.get("type").and_then(Value::as_str) == Some("tool_use") {
                            let bridge_id = value
                                .get("id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string();
                            let name = value
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("tool")
                                .to_string();
                            let input_json = value.get("input").cloned().unwrap_or(Value::Null);
                            let tool_call_id = ToolCallId::new();
                            tool_ids.insert(bridge_id, tool_call_id);
                            yield SessionEvent::ToolInputStart {
                                session_id,
                                tool_call_id,
                                name: name.clone(),
                            };
                            yield SessionEvent::ToolInputEnd {
                                session_id,
                                tool_call_id,
                                input_json: input_json.clone(),
                            };
                            yield SessionEvent::ToolCallStart {
                                session_id,
                                tool_call_id,
                                name,
                                input_json,
                            };
                            continue;
                        }

                        // Native SDK tool result -> ToolCallResult / ToolCallError.
                        if value.get("type").and_then(Value::as_str) == Some("tool_result") {
                            if let Some(event) =
                                bridge_tool_result_to_session_event(session_id, &value, &mut tool_ids)
                            {
                                yield event;
                            }
                            continue;
                        }

                        if let Some(event) = bridge_event_to_session_event(session_id, &value) {
                            let is_terminal = matches!(
                                event,
                                SessionEvent::Result { .. } | SessionEvent::Error { .. }
                            );
                            yield event;
                            if is_terminal {
                                return;
                            }
                        }
                    }

                    // A decision from the user; forward it to the paused tool call.
                    Step::Decision(resp) => {
                        last_activity = Instant::now();
                        match resp {
                            Some(resp) => {
                                if let Some(bridge_id) = pending.remove(&resp.request_id.to_string()) {
                                    let decision = match resp.decision {
                                        PermissionDecision::Allow => "allow",
                                        PermissionDecision::AllowAlways => "allow_always",
                                        PermissionDecision::Deny => "deny",
                                    };
                                    if let Err(error) = send_bridge_value_or_timeout(&mut bridge, idle_timeout, json!({
                                        "type": "permission_decision",
                                        "id": bridge_id,
                                        "decision": decision,
                                        "message": resp.message,
                                    })).await {
                                        bridge.cancel_with_grace(DEFAULT_CLAUDE_CANCEL_GRACE).await;
                                        yield SessionEvent::Error {
                                            session_id,
                                            message: error.to_string(),
                                        };
                                        return;
                                    }
                                    yield SessionEvent::Status {
                                        session_id,
                                        status: SessionStatus::Streaming,
                                    };
                                }
                            }
                            // Sender dropped: stop polling this branch.
                            None => perms_open = false,
                        }
                    }

                    Step::ToolResult(resp) => {
                        last_activity = Instant::now();
                        match resp {
                            Some(resp) => {
                                if let Some(bridge_id) = pending_tools.remove(&resp.tool_call_id.to_string())
                                    && let Err(error) = send_bridge_value_or_timeout(&mut bridge, idle_timeout, json!({
                                        "type": "tool_result",
                                        "id": bridge_id,
                                        "output": resp.output,
                                        "is_error": resp.is_error,
                                    })).await
                                {
                                    bridge.cancel_with_grace(DEFAULT_CLAUDE_CANCEL_GRACE).await;
                                    yield SessionEvent::Error {
                                        session_id,
                                        message: error.to_string(),
                                    };
                                    return;
                                }
                            }
                            None => tool_results_open = false,
                        }
                    }

                    Step::IdleTimeout => {
                        bridge.cancel_with_grace(DEFAULT_CLAUDE_CANCEL_GRACE).await;
                        yield SessionEvent::Error {
                            session_id,
                            message: format!(
                                "Claude Agent SDK produced no events for {} seconds; killed the stale run (pending permissions: {}, pending tools: {})",
                                idle_timeout.as_secs(),
                                pending.len(),
                                pending_tools.len()
                            ),
                        };
                        return;
                    }

                    Step::Cancelled => {
                        bridge.cancel_with_grace(DEFAULT_CLAUDE_CANCEL_GRACE).await;
                        yield SessionEvent::Result {
                            session_id,
                            stop_reason: StopReason::Interrupted,
                        };
                        return;
                    }
                }
            }
        };

        Ok(Box::pin(stream))
    }
}

/// One iteration's resolved event in [`ClaudeProvider::stream_turn`]'s select.
enum Step {
    Read(anyhow::Result<Option<Value>>),
    Decision(Option<harness_core::PermissionResponse>),
    ToolResult(Option<ProviderToolResponse>),
    IdleTimeout,
    Cancelled,
}

fn claude_idle_timeout() -> Duration {
    std::env::var("INDUCTOR_CLAUDE_IDLE_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_CLAUDE_IDLE_TIMEOUT)
}

async fn send_bridge_value_or_timeout(
    bridge: &mut SdkBridge,
    idle_timeout: Duration,
    value: Value,
) -> anyhow::Result<()> {
    match timeout(idle_timeout, bridge.send_value(&value)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "timed out after {} seconds writing to Claude Agent SDK bridge: {}",
            idle_timeout.as_secs(),
            value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message")
        )),
    }
}

#[derive(Debug, Clone)]
struct BridgeCommand {
    command: String,
    args: Vec<String>,
}

impl BridgeCommand {
    fn from_env(manifest_dir: &Path) -> Self {
        if let Ok(value) = std::env::var("INDUCTOR_CLAUDE_SDK_COMMAND") {
            return parse_command_line(&value);
        }

        Self {
            command: "node".to_string(),
            args: vec![
                manifest_dir
                    .join(DEFAULT_BRIDGE_SCRIPT)
                    .display()
                    .to_string(),
            ],
        }
    }
}

#[derive(Debug)]
struct BridgeRequest {
    prompt: String,
    cwd: PathBuf,
    model: String,
    messages: Vec<ModelMessage>,
    images: Vec<ImageAttachment>,
    system_prompt: Option<String>,
    approval_policy: String,
    tool_names: Vec<String>,
}

struct SdkBridge {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stderr: Arc<Mutex<String>>,
}

/// Cap on how much bridge stderr we retain for diagnostics.
const MAX_BRIDGE_STDERR: usize = 4096;

impl SdkBridge {
    async fn spawn(command: BridgeCommand, cwd: &Path) -> anyhow::Result<Self> {
        let mut child = Command::new(&command.command)
            .args(&command.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("failed to open Claude SDK bridge stdout"))?;
        let stdin = child.stdin.take();

        // Drain stderr into a bounded buffer so a silent crash (e.g. a missing
        // node module) surfaces a real reason instead of a bare EOF.
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(mut child_stderr) = child.stderr.take() {
            let sink = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut buf = String::new();
                let _ = child_stderr.read_to_string(&mut buf).await;
                if !buf.is_empty()
                    && let Ok(mut guard) = sink.lock()
                {
                    guard.push_str(&buf);
                    if guard.len() > MAX_BRIDGE_STDERR {
                        let start = guard.len() - MAX_BRIDGE_STDERR;
                        *guard = guard[start..].to_string();
                    }
                }
            });
        }

        Ok(Self {
            child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            stderr,
        })
    }

    /// Build a diagnostic suffix describing why the bridge stopped: its exit
    /// status plus any captured stderr. Used to turn a bare EOF into a real
    /// error message.
    async fn exit_diagnostics(&mut self) -> String {
        let status = match timeout(Duration::from_secs(2), self.child.wait()).await {
            Ok(Ok(status)) => status
                .code()
                .map(|code| format!("exit code {code}"))
                .unwrap_or_else(|| "terminated by signal".to_string()),
            _ => "still running".to_string(),
        };
        let stderr = self
            .stderr
            .lock()
            .ok()
            .map(|guard| guard.trim().to_string())
            .unwrap_or_default();
        if stderr.is_empty() {
            format!(" ({status})")
        } else {
            format!(" ({status}): {stderr}")
        }
    }

    async fn send_request(&mut self, request: &BridgeRequest) -> anyhow::Result<()> {
        let allowed = request
            .tool_names
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        self.send_value(&json!({
            "prompt": request.prompt,
            "cwd": request.cwd,
            "model": request.model,
            "messages": request.messages,
            "images": request.images,
            "system_prompt": request.system_prompt,
            "approval_policy": request.approval_policy,
            "tool_definitions": tools::tool_definitions()
                .into_iter()
                .filter(|definition| allowed.contains(definition.name.as_str()))
                .collect::<Vec<_>>(),
        }))
        .await
    }

    /// Write one JSON line to the bridge's stdin, keeping the pipe open so we can
    /// stream later messages (e.g. permission decisions) to the running query.
    async fn send_value(&mut self, value: &Value) -> anyhow::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("failed to open Claude SDK bridge stdin"))?;
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn read_event(&mut self) -> anyhow::Result<Option<Value>> {
        match self.lines.next_line().await? {
            Some(line) => Ok(Some(serde_json::from_str(&line)?)),
            None => Ok(None),
        }
    }

    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    async fn cancel_with_grace(&mut self, grace: Duration) {
        let _ = self.send_value(&json!({ "type": "cancel" })).await;
        self.stdin.take();
        if timeout(grace, self.child.wait()).await.is_err() {
            self.kill();
        }
    }
}

impl Drop for SdkBridge {
    fn drop(&mut self) {
        self.kill();
    }
}

fn bridge_tool_result_to_session_event(
    session_id: SessionId,
    value: &Value,
    tool_ids: &mut HashMap<String, ToolCallId>,
) -> Option<SessionEvent> {
    if value.get("type").and_then(Value::as_str) != Some("tool_result") {
        return None;
    }

    let bridge_id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let output = value
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tool_call_id = tool_ids.remove(&bridge_id)?;

    if value.get("is_error").and_then(Value::as_bool) == Some(true) {
        Some(SessionEvent::ToolCallError {
            session_id,
            tool_call_id,
            message: output,
        })
    } else {
        Some(SessionEvent::ToolCallResult {
            session_id,
            tool_call_id,
            title: None,
            metadata: serde_json::Value::Null,
            output,
            exit_code: None,
        })
    }
}

fn bridge_event_to_session_event(session_id: SessionId, value: &Value) -> Option<SessionEvent> {
    match value.get("type").and_then(Value::as_str)? {
        "text_delta" => value
            .get("text")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(|text| SessionEvent::TextDelta {
                session_id,
                text: text.to_string(),
            }),
        "result" => Some(SessionEvent::Result {
            session_id,
            stop_reason: value
                .get("stop_reason")
                .and_then(Value::as_str)
                .map(sdk_stop_reason)
                .unwrap_or(StopReason::EndTurn),
        }),
        "error" => Some(SessionEvent::Error {
            session_id,
            message: value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Claude Agent SDK bridge failed")
                .to_string(),
        }),
        "usage" => Some(SessionEvent::Usage {
            session_id,
            input_tokens: value.get("input_tokens").and_then(Value::as_u64),
            output_tokens: value.get("output_tokens").and_then(Value::as_u64),
            cache_read_tokens: value.get("cache_read_tokens").and_then(Value::as_u64),
            total_cost_usd: value.get("total_cost_usd").and_then(Value::as_f64),
        }),
        _ => None,
    }
}

fn sdk_stop_reason(value: &str) -> StopReason {
    match value {
        "cancelled" => StopReason::Interrupted,
        "error" | "refusal" => StopReason::Error,
        _ => StopReason::EndTurn,
    }
}

fn normalize_claude_model(model: &str) -> &str {
    let model = model.trim();
    match model {
        "" => DEFAULT_CLAUDE_MODEL,
        "claude-sonnet-4" | "claude-sonnet-4-20250514" | "claude-sonnet-4.6" => "sonnet",
        "claude-fable-5" | "fable-5" => "fable",
        "claude-opus-4" | "claude-opus-4-20250514" | "claude-opus-4.8" => "opus",
        "claude-haiku-3.5"
        | "claude-3-5-haiku"
        | "claude-3-5-haiku-latest"
        | "claude-haiku-4.5" => "haiku",
        _ => model,
    }
}

fn claude_model_catalog() -> Vec<ModelInfo> {
    let mut models = vec![
        ModelInfo {
            id: DEFAULT_CLAUDE_MODEL.to_string(),
            display_name: "Claude Sonnet".to_string(),
            context_window: None,
        },
        ModelInfo {
            id: "fable".to_string(),
            display_name: "Fable".to_string(),
            context_window: None,
        },
        ModelInfo {
            id: "opus".to_string(),
            display_name: "Opus (1M context)".to_string(),
            context_window: None,
        },
        ModelInfo {
            id: "haiku".to_string(),
            display_name: "Haiku".to_string(),
            context_window: None,
        },
    ];
    extend_models_from_env(&mut models, "INDUCTOR_CLAUDE_MODELS");
    models
}

fn extend_models_from_env(models: &mut Vec<ModelInfo>, env_key: &str) {
    let Ok(raw) = std::env::var(env_key) else {
        return;
    };
    for id in raw.split(',').map(str::trim).filter(|id| !id.is_empty()) {
        let normalized = normalize_claude_model(id).to_string();
        if models.iter().any(|model| model.id == normalized) {
            continue;
        }
        models.push(ModelInfo {
            id: normalized.clone(),
            display_name: normalized,
            context_window: None,
        });
    }
}

fn parse_command_line(value: &str) -> BridgeCommand {
    let parts = value
        .split_whitespace()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut parts = parts.into_iter();
    let command = parts.next().unwrap_or_else(|| "node".to_string());

    BridgeCommand {
        command,
        args: parts.collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::SessionId;

    #[test]
    fn bridge_text_delta_converts_to_session_event() {
        let session_id = SessionId::new();
        let value = json!({
            "type": "text_delta",
            "text": "hello"
        });

        let event = bridge_event_to_session_event(session_id, &value).unwrap();

        assert!(matches!(
            event,
            SessionEvent::TextDelta { text, .. } if text == "hello"
        ));
    }

    #[test]
    fn bridge_empty_text_delta_is_ignored() {
        let session_id = SessionId::new();
        let value = json!({
            "type": "text_delta",
            "text": ""
        });

        assert!(bridge_event_to_session_event(session_id, &value).is_none());
    }

    #[test]
    fn bridge_result_converts_to_end_turn() {
        let session_id = SessionId::new();
        let value = json!({
            "type": "result",
            "stop_reason": "end_turn"
        });

        let event = bridge_event_to_session_event(session_id, &value).unwrap();

        assert!(matches!(
            event,
            SessionEvent::Result {
                stop_reason: StopReason::EndTurn,
                ..
            }
        ));
    }

    #[test]
    fn bridge_tool_result_error_converts_to_tool_call_error() {
        let session_id = SessionId::new();
        let bridge_id = "toolu_error".to_string();
        let tool_call_id = ToolCallId::new();
        let mut tool_ids = HashMap::from([(bridge_id.clone(), tool_call_id)]);
        let value = json!({
            "type": "tool_result",
            "id": bridge_id,
            "output": "read_file failed",
            "is_error": true,
        });

        let event = bridge_tool_result_to_session_event(session_id, &value, &mut tool_ids)
            .expect("tool result should map to a session event");

        assert!(tool_ids.is_empty());
        assert!(matches!(
            event,
            SessionEvent::ToolCallError { message, .. } if message == "read_file failed"
        ));
    }

    #[test]
    fn bridge_tool_result_success_converts_to_tool_call_result() {
        let session_id = SessionId::new();
        let bridge_id = "toolu_success".to_string();
        let tool_call_id = ToolCallId::new();
        let mut tool_ids = HashMap::from([(bridge_id.clone(), tool_call_id)]);
        let value = json!({
            "type": "tool_result",
            "id": bridge_id,
            "output": "file contents",
            "is_error": false,
        });

        let event = bridge_tool_result_to_session_event(session_id, &value, &mut tool_ids)
            .expect("tool result should map to a session event");

        assert!(tool_ids.is_empty());
        assert!(matches!(
            event,
            SessionEvent::ToolCallResult { output, .. } if output == "file contents"
        ));
    }

    #[test]
    fn env_command_parser_splits_command_and_args() {
        let command = parse_command_line("node ./bridge.mjs --flag");

        assert_eq!(command.command, "node");
        assert_eq!(command.args, vec!["./bridge.mjs", "--flag"]);
    }

    #[test]
    fn normalizes_legacy_claude_model_ids_to_sdk_aliases() {
        assert_eq!(normalize_claude_model(""), "sonnet");
        assert_eq!(normalize_claude_model("claude-sonnet-4"), "sonnet");
        assert_eq!(normalize_claude_model("claude-fable-5"), "fable");
        assert_eq!(normalize_claude_model("claude-opus-4.8"), "opus");
        assert_eq!(normalize_claude_model("claude-haiku-3.5"), "haiku");
        assert_eq!(normalize_claude_model("opus"), "opus");
    }

    #[test]
    fn claude_model_catalog_includes_subscription_models() {
        let models = claude_model_catalog();

        assert!(models.iter().any(|model| model.id == "sonnet"));
        assert!(models.iter().any(|model| model.id == "fable"));
        assert!(models.iter().any(|model| model.id == "opus"));
        assert!(models.iter().any(|model| model.id == "haiku"));
    }
}
