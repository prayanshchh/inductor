use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use ulid::Ulid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(pub Ulid);

impl WorkspaceId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for WorkspaceId {
    type Err = ulid::DecodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Ulid::from_string(value)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Ulid);

impl SessionId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for SessionId {
    type Err = ulid::DecodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Ulid::from_string(value)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCallId(pub Ulid);

impl ToolCallId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ToolCallId {
    type Err = ulid::DecodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Ulid::from_string(value)?))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRequestId(pub Ulid);

impl PermissionRequestId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl fmt::Display for PermissionRequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for PermissionRequestId {
    type Err = ulid::DecodeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self(Ulid::from_string(value)?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProviderId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub streaming: bool,
    pub token_counting: bool,
    pub tool_calling: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub path: Option<String>,
    pub mime_type: String,
    pub base64_data: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub file_size: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MessagePart {
    Text { text: String },
    Image { image: ImageAttachment },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: String,
    #[serde(default)]
    pub parts: Vec<MessagePart>,
}

impl ModelMessage {
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            parts: vec![MessagePart::Text { text: text.into() }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnRequest {
    pub session_id: SessionId,
    pub model: String,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub messages: Vec<ModelMessage>,
    #[serde(default)]
    pub tool_names: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub images: Vec<ImageAttachment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub workspace_id: WorkspaceId,
    pub provider_id: ProviderId,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub session_id: SessionId,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NormalizedStreamEvent {
    TextDelta { text: String },
    Result { stop_reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Starting,
    Idle,
    Streaming,
    RunningTools,
    WaitingForPermission,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    Interrupted,
    Error,
}

/// When the harness should pause a tool call for human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Never ask; auto-approve everything.
    Never,
    /// Ask only when the risk classifier flags the action.
    OnRequest,
    /// Ask before any tool that changes state (writes, edits, patches, bash);
    /// read-only tools (read_file, grep) run without asking.
    Mutating,
    /// Run first; ask only after a tool fails.
    OnFailure,
    /// Ask before every tool call.
    Always,
}

impl Default for ApprovalPolicy {
    fn default() -> Self {
        Self::OnRequest
    }
}

/// A reason a tool call was flagged as risky.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskFlag {
    RecursiveRemove,
    Sudo,
    GitForcePush,
    PackagePublish,
    WriteOutsideWorkspace,
    ReadOutsideWorkspace,
    Dotfile,
    EnvFile,
    GitDirectory,
    NetworkAccess,
}

impl RiskFlag {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecursiveRemove => "recursive_remove",
            Self::Sudo => "sudo",
            Self::GitForcePush => "git_force_push",
            Self::PackagePublish => "package_publish",
            Self::WriteOutsideWorkspace => "write_outside_workspace",
            Self::ReadOutsideWorkspace => "read_outside_workspace",
            Self::Dotfile => "dotfile",
            Self::EnvFile => "env_file",
            Self::GitDirectory => "git_directory",
            Self::NetworkAccess => "network_access",
        }
    }
}

/// The user's decision in response to a [`SessionEvent::PermissionRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Allow this single call.
    Allow,
    /// Allow this call and remember a matching allow rule for the session.
    AllowAlways,
    /// Reject this call.
    Deny,
}

/// A decision routed back to a provider in answer to a
/// [`SessionEvent::PermissionRequest`]. Carries the originating request id so
/// the provider can match it to the paused tool call, plus an optional message
/// the user typed (e.g. why they denied) that is fed back to the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionResponse {
    pub request_id: PermissionRequestId,
    pub decision: PermissionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The kinds of persisted allow rule the harness understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllowRuleKind {
    /// Match any `bash` command starting with `value`.
    BashPrefix,
    /// Match any `bash` command against the regex `value`.
    BashRegex,
    /// Match any write whose path starts with `value`.
    PathWrite,
    /// Match any call to the tool named `value`.
    ToolName,
}

/// A single allow rule that pre-approves matching tool calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllowRule {
    pub kind: AllowRuleKind,
    pub value: String,
}

impl AllowRule {
    pub fn new(kind: AllowRuleKind, value: impl Into<String>) -> Self {
        Self {
            kind,
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    Status {
        session_id: SessionId,
        status: SessionStatus,
    },
    UserMessage {
        session_id: SessionId,
        text: String,
    },
    TextDelta {
        session_id: SessionId,
        text: String,
    },
    TextStart {
        session_id: SessionId,
        text_id: String,
    },
    TextEnd {
        session_id: SessionId,
        text_id: String,
        text: String,
    },
    ReasoningStart {
        session_id: SessionId,
        reasoning_id: String,
    },
    ReasoningDelta {
        session_id: SessionId,
        reasoning_id: String,
        text: String,
    },
    ReasoningEnd {
        session_id: SessionId,
        reasoning_id: String,
        text: String,
    },
    ContextPrepared {
        session_id: SessionId,
        token_count: u64,
        original_token_count: u64,
        compacted: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    StepStart {
        session_id: SessionId,
        index: u32,
    },
    StepFinish {
        session_id: SessionId,
        index: u32,
        stop_reason: StopReason,
    },
    ToolCallStart {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        name: String,
        input_json: serde_json::Value,
    },
    ToolInputStart {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        name: String,
    },
    ToolInputDelta {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        delta: String,
    },
    ToolInputEnd {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        input_json: serde_json::Value,
    },
    ToolCallRequested {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        name: String,
        input_json: serde_json::Value,
    },
    ToolCallProgress {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        message: String,
    },
    ToolCallResult {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        metadata: serde_json::Value,
        output: String,
        exit_code: Option<i32>,
    },
    ToolCallError {
        session_id: SessionId,
        tool_call_id: ToolCallId,
        message: String,
    },
    Patch {
        session_id: SessionId,
        files: Vec<PatchFile>,
        additions: u64,
        deletions: u64,
    },
    Diagnostics {
        session_id: SessionId,
        files: Vec<DiagnosticFile>,
    },
    PermissionRequest {
        session_id: SessionId,
        request_id: PermissionRequestId,
        reason: String,
        tool_name: String,
        /// The tool's input (path, content, command…) for a rich preview.
        #[serde(default)]
        input_json: serde_json::Value,
    },
    PermissionResolved {
        session_id: SessionId,
        request_id: PermissionRequestId,
        decision: PermissionDecision,
    },
    TerminalOutput {
        session_id: SessionId,
        chunk: String,
    },
    Result {
        session_id: SessionId,
        stop_reason: StopReason,
    },
    Error {
        session_id: SessionId,
        message: String,
    },
    /// Real token usage reported by the provider for this turn.
    Usage {
        session_id: SessionId,
        input_tokens: Option<u64>,
        output_tokens: Option<u64>,
        cache_read_tokens: Option<u64>,
        /// Provider-reported cost in USD, when available (Claude SDK reports it).
        total_cost_usd: Option<f64>,
    },
    /// Session/worktree metadata changed while the run is still active.
    MetadataUpdated {
        session_id: SessionId,
        display_name: Option<String>,
        workspace_id: Option<WorkspaceId>,
        worktree_path: Option<String>,
        branch_name: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchFile {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticFile {
    pub path: String,
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn text_delta_event_uses_tagged_json_shape() {
        let session_id = SessionId(Ulid::from_string("01KT4H9V3W2M0W4Z5X6Y7Z8A9B").unwrap());
        let event = SessionEvent::TextDelta {
            session_id,
            text: "hello".to_string(),
        };

        let value = serde_json::to_value(event).unwrap();

        assert_eq!(
            value,
            json!({
                "type": "text_delta",
                "session_id": "01KT4H9V3W2M0W4Z5X6Y7Z8A9B",
                "text": "hello"
            })
        );
    }

    #[test]
    fn user_message_event_uses_tagged_json_shape() {
        let session_id = SessionId(Ulid::from_string("01KT4H9V3W2M0W4Z5X6Y7Z8A9B").unwrap());
        let event = SessionEvent::UserMessage {
            session_id,
            text: "do all the tool calls".to_string(),
        };

        let value = serde_json::to_value(event).unwrap();

        assert_eq!(
            value,
            json!({
                "type": "user_message",
                "session_id": "01KT4H9V3W2M0W4Z5X6Y7Z8A9B",
                "text": "do all the tool calls"
            })
        );
    }

    #[test]
    fn tool_call_event_keeps_input_as_json() {
        let session_id = SessionId(Ulid::from_string("01KT4H9V3W2M0W4Z5X6Y7Z8A9B").unwrap());
        let tool_call_id = ToolCallId(Ulid::from_string("01KT4H9Z7SM6PR9J3KVDP92M6Q").unwrap());
        let event = SessionEvent::ToolCallStart {
            session_id,
            tool_call_id,
            name: "read_file".to_string(),
            input_json: json!({ "path": "README.md" }),
        };

        let value = serde_json::to_value(event).unwrap();

        assert_eq!(
            value,
            json!({
                "type": "tool_call_start",
                "session_id": "01KT4H9V3W2M0W4Z5X6Y7Z8A9B",
                "tool_call_id": "01KT4H9Z7SM6PR9J3KVDP92M6Q",
                "name": "read_file",
                "input_json": {
                    "path": "README.md"
                }
            })
        );
    }

    #[test]
    fn diagnostics_event_serializes_file_metadata() {
        let session_id = SessionId(Ulid::from_string("01KT4H9V3W2M0W4Z5X6Y7Z8A9B").unwrap());
        let event = SessionEvent::Diagnostics {
            session_id,
            files: vec![DiagnosticFile {
                path: "src/main.rs".to_string(),
                exists: true,
                bytes: Some(120),
                lines: Some(7),
            }],
        };

        let value = serde_json::to_value(event).unwrap();

        assert_eq!(
            value,
            json!({
                "type": "diagnostics",
                "session_id": "01KT4H9V3W2M0W4Z5X6Y7Z8A9B",
                "files": [{
                    "path": "src/main.rs",
                    "exists": true,
                    "bytes": 120,
                    "lines": 7
                }]
            })
        );
    }

    #[test]
    fn every_demo_event_serializes_as_json_object() {
        for event in demo_session_events() {
            let value = serde_json::to_value(event).unwrap();
            assert!(matches!(value, Value::Object(_)));
            assert!(value.get("type").is_some());
        }
    }

    fn demo_session_events() -> Vec<SessionEvent> {
        let session_id = SessionId(Ulid::from_string("01KT4H9V3W2M0W4Z5X6Y7Z8A9B").unwrap());
        let tool_call_id = ToolCallId(Ulid::from_string("01KT4H9Z7SM6PR9J3KVDP92M6Q").unwrap());
        let request_id =
            PermissionRequestId(Ulid::from_string("01KT4HA2VR4SPQZKA1JTEB5AKP").unwrap());

        vec![
            SessionEvent::Status {
                session_id,
                status: SessionStatus::Starting,
            },
            SessionEvent::TextDelta {
                session_id,
                text: "Inspecting the workspace.".to_string(),
            },
            SessionEvent::TextStart {
                session_id,
                text_id: "text-0".to_string(),
            },
            SessionEvent::TextEnd {
                session_id,
                text_id: "text-0".to_string(),
                text: "Inspecting the workspace.".to_string(),
            },
            SessionEvent::ReasoningStart {
                session_id,
                reasoning_id: "reasoning-0".to_string(),
            },
            SessionEvent::ReasoningDelta {
                session_id,
                reasoning_id: "reasoning-0".to_string(),
                text: "Need inspect file.".to_string(),
            },
            SessionEvent::ReasoningEnd {
                session_id,
                reasoning_id: "reasoning-0".to_string(),
                text: "Need inspect file.".to_string(),
            },
            SessionEvent::ContextPrepared {
                session_id,
                token_count: 42,
                original_token_count: 42,
                compacted: false,
                summary: None,
            },
            SessionEvent::StepStart {
                session_id,
                index: 0,
            },
            SessionEvent::ToolCallStart {
                session_id,
                tool_call_id,
                name: "read_file".to_string(),
                input_json: json!({ "path": "README.md" }),
            },
            SessionEvent::ToolInputStart {
                session_id,
                tool_call_id,
                name: "read_file".to_string(),
            },
            SessionEvent::ToolInputDelta {
                session_id,
                tool_call_id,
                delta: "{\"path\":".to_string(),
            },
            SessionEvent::ToolInputEnd {
                session_id,
                tool_call_id,
                input_json: json!({ "path": "README.md" }),
            },
            SessionEvent::ToolCallResult {
                session_id,
                tool_call_id,
                title: Some("Read File".to_string()),
                metadata: json!({ "path": "README.md" }),
                output: "# Inductor\n".to_string(),
                exit_code: None,
            },
            SessionEvent::Patch {
                session_id,
                additions: 1,
                deletions: 0,
                files: vec![PatchFile {
                    path: "README.md".to_string(),
                    additions: 1,
                    deletions: 0,
                    diff: None,
                }],
            },
            SessionEvent::PermissionRequest {
                session_id,
                request_id,
                reason: "write_file wants to modify README.md".to_string(),
                tool_name: "write_file".to_string(),
                input_json: serde_json::Value::Null,
            },
            SessionEvent::PermissionResolved {
                session_id,
                request_id,
                decision: PermissionDecision::Allow,
            },
            SessionEvent::StepFinish {
                session_id,
                index: 0,
                stop_reason: StopReason::EndTurn,
            },
            SessionEvent::Result {
                session_id,
                stop_reason: StopReason::EndTurn,
            },
        ]
    }
}
