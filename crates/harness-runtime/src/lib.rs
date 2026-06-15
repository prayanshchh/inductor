//! Phase 5: the harness turn loop.
//!
//! This crate connects a provider stream to the local tool runtime:
//!
//! ```text
//! user prompt -> provider stream -> tool-call detection -> ToolRuntime
//!   -> tool result -> provider continuation -> final answer
//! ```
//!
//! Tool calls are detected through a plain-text envelope so the same logic
//! works across Codex and Claude without depending on native provider
//! tool-calling APIs:
//!
//! ```text
//! <inductor_tool_call>{"name":"read_file","input":{"path":"Cargo.toml"}}</inductor_tool_call>
//! ```

use std::{
    collections::HashMap,
    fmt, fs,
    path::{Component, Path, PathBuf},
    pin::Pin,
    process::Command,
};

use async_stream::try_stream;
use context::{
    ApproxTokenCounter, BlobStore, ContextLimits, ContextMessage, ModelEffort, ProviderFamily,
    prepare_context, stub_tool_output, translate_effort,
};
use futures_core::Stream;
use futures_util::StreamExt;
use harness_core::{
    ApprovalPolicy, DiagnosticFile, ImageAttachment, MessagePart, ModelMessage, PatchFile,
    PermissionDecision, PermissionRequestId, RiskFlag, SessionEvent, SessionId, SessionStatus,
    StopReason, ToolCallId, TurnRequest,
};
use provider_core::{ProviderAuth, ProviderPlugin, ProviderToolResponse};
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use tools::{StructuredPatch, TextEdit, ToolName, ToolRuntime};

pub mod risk;

pub use risk::AllowStore;

/// Context passed to an [`Approver`] when a tool call needs a decision.
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub session_id: SessionId,
    pub request_id: PermissionRequestId,
    pub tool_name: String,
    pub input: Value,
    pub risk_flags: Vec<RiskFlag>,
    pub reason: String,
    /// The outside-workspace path if this tool tries to access outside the workspace.
    pub outside_path: Option<String>,
}

/// Decides whether a flagged tool call may run. Implementors range from a
/// non-interactive auto-approver to an interactive CLI prompt to a future
/// UI-driven responder.
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    async fn decide(&self, request: &ApprovalRequest) -> PermissionDecision;

    /// An optional message accompanying the most recent decision — e.g. the
    /// reason the user typed when denying — fed back to the model. Defaults to
    /// none.
    fn last_message(&self) -> Option<String> {
        None
    }
}

/// An approver that allows everything. Useful for non-interactive runs.
pub struct AutoApprove;

#[async_trait::async_trait]
impl Approver for AutoApprove {
    async fn decide(&self, _request: &ApprovalRequest) -> PermissionDecision {
        PermissionDecision::Allow
    }
}

/// An approver that denies everything. Useful for tests and locked-down runs.
pub struct AutoDeny;

#[async_trait::async_trait]
impl Approver for AutoDeny {
    async fn decide(&self, _request: &ApprovalRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }
}

/// Markers that wrap a tool-call envelope in assistant output.
const TOOL_CALL_OPEN: &str = "<inductor_tool_call>";
const TOOL_CALL_CLOSE: &str = "</inductor_tool_call>";

/// System preamble for providers without native tool calling. It teaches the
/// Inductor text envelope and renders tool docs from the shared tool registry.
fn generic_tools_preamble() -> String {
    format!(
        "You are an Inductor coding agent operating on the user's machine.\n\
You can run tools by emitting EXACTLY ONE tool-call envelope at the end of your reply:\n\n\
<inductor_tool_call>{{\"name\":\"<tool>\",\"input\":{{ ... }}}}</inductor_tool_call>\n\n\
Available tools and their JSON schemas:\n{}\n\n\
Rules:\n\
- Paths may be workspace-relative or absolute unless the user has enabled workspace-only mode.\n\
- Prefer edit_file or multi_edit over write_file when changing existing files.\n\
- Exact edit tools reject binary files, stale expected_hash values, and non-unique matches.\n\
- Emit at most one envelope per reply. After a tool result is returned, continue.\n\
- When you have the final answer and need no more tools, reply with prose and NO envelope.",
        tools::tool_prompt_docs()
    )
}

/// The role of a transcript message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Assistant => "Assistant",
            Self::Tool => "Tool",
        }
    }
}

impl std::str::FromStr for Role {
    type Err = RoleParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "User" | "user" => Ok(Self::User),
            "Assistant" | "assistant" => Ok(Self::Assistant),
            "Tool" | "tool" => Ok(Self::Tool),
            other => Err(RoleParseError(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleParseError(String);

impl fmt::Display for RoleParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown transcript role: {}", self.0)
    }
}

impl std::error::Error for RoleParseError {}

/// One entry in the conversation transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptMessage {
    pub role: Role,
    pub content: String,
}

impl TranscriptMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// In-memory session model carrying the running transcript.
#[derive(Debug, Clone)]
pub struct SessionState {
    pub session_id: SessionId,
    pub transcript: Vec<TranscriptMessage>,
}

impl SessionState {
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            transcript: Vec::new(),
        }
    }

    pub fn with_transcript(session_id: SessionId, transcript: Vec<TranscriptMessage>) -> Self {
        Self {
            session_id,
            transcript,
        }
    }

    pub fn push(&mut self, role: Role, content: impl Into<String>) {
        self.transcript.push(TranscriptMessage::new(role, content));
    }

    pub fn context_messages(&self) -> Vec<ContextMessage> {
        self.transcript
            .iter()
            .map(|message| ContextMessage::new(message.role.label(), message.content.clone()))
            .collect()
    }
}

/// A parsed tool-call envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedToolCall {
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone)]
struct ToolLoopGuard {
    max_repeats: usize,
    calls: HashMap<String, usize>,
}

impl ToolLoopGuard {
    fn new(max_repeats: usize) -> Self {
        Self {
            max_repeats,
            calls: HashMap::new(),
        }
    }

    fn record(&mut self, call: &ParsedToolCall) -> Result<(), String> {
        let fingerprint = format!("{}:{}", call.name, call.input);
        let count = self.calls.entry(fingerprint).or_insert(0);
        *count += 1;
        if *count > self.max_repeats {
            return Err(format!(
                "repeated identical tool call more than {} time(s): {}",
                self.max_repeats, call.name
            ));
        }
        Ok(())
    }
}

/// Failure modes when parsing a tool-call envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolCallParseError {
    Unterminated,
    InvalidJson(String),
    MissingName,
}

impl fmt::Display for ToolCallParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unterminated => {
                write!(f, "tool call envelope is missing its closing tag")
            }
            Self::InvalidJson(detail) => {
                write!(f, "tool call envelope is not valid JSON: {detail}")
            }
            Self::MissingName => write!(f, "tool call envelope is missing a string `name` field"),
        }
    }
}

impl std::error::Error for ToolCallParseError {}

/// Look for a single tool-call envelope in assistant text.
///
/// Returns `None` when no envelope is present (the assistant is done),
/// `Some(Ok(..))` for a well-formed call, and `Some(Err(..))` when an
/// envelope exists but is malformed.
pub fn parse_tool_call(text: &str) -> Option<Result<ParsedToolCall, ToolCallParseError>> {
    let open = text.find(TOOL_CALL_OPEN)?;
    let body_start = open + TOOL_CALL_OPEN.len();

    let close_rel = match text[body_start..].find(TOOL_CALL_CLOSE) {
        Some(rel) => rel,
        None => return Some(Err(ToolCallParseError::Unterminated)),
    };
    let body = text[body_start..body_start + close_rel].trim();

    let value: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(err) => return Some(Err(ToolCallParseError::InvalidJson(err.to_string()))),
    };

    let name = match value.get("name").and_then(Value::as_str) {
        Some(name) if !name.trim().is_empty() => name.to_string(),
        _ => return Some(Err(ToolCallParseError::MissingName)),
    };

    let input = value.get("input").cloned().unwrap_or_else(|| json!({}));

    Some(Ok(ParsedToolCall { name, input }))
}

/// Errors raised while dispatching a parsed tool call to the runtime.
#[derive(Debug)]
pub enum ToolExecError {
    UnknownTool(String),
    MissingField { tool: String, field: &'static str },
    Runtime(tools::ToolError),
}

impl fmt::Display for ToolExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownTool(name) => write!(f, "unknown tool: {name}"),
            Self::MissingField { tool, field } => {
                write!(f, "tool {tool} requires a string `{field}` input field")
            }
            Self::Runtime(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ToolExecError {}

/// Dispatch a parsed tool call to the [`ToolRuntime`].
pub fn execute_tool_call(
    tools: &ToolRuntime,
    call: &ParsedToolCall,
) -> Result<tools::ToolResult, ToolExecError> {
    let input = &call.input;
    let result = match call.name.as_str() {
        name if name == ToolName::ReadFile.as_str() => {
            tools.read_file(string_field(input, "path", name)?)
        }
        name if name == ToolName::ListDir.as_str() => {
            tools.list_dir(optional_string_field(input, "path"))
        }
        name if name == ToolName::WriteFile.as_str() => tools.write_file(
            string_field(input, "path", name)?,
            string_field(input, "content", name)?,
        ),
        name if name == ToolName::EditFile.as_str() => tools.edit_file(
            string_field(input, "path", name)?,
            string_field(input, "old", name)?,
            string_field(input, "new", name)?,
            optional_string_field(input, "expected_hash"),
        ),
        name if name == ToolName::MultiEdit.as_str() => {
            let path = string_field(input, "path", name)?;
            let edits_value =
                input
                    .get("edits")
                    .cloned()
                    .ok_or_else(|| ToolExecError::MissingField {
                        tool: name.to_string(),
                        field: "edits",
                    })?;
            let edits = serde_json::from_value::<Vec<TextEdit>>(edits_value).map_err(|_| {
                ToolExecError::MissingField {
                    tool: name.to_string(),
                    field: "edits",
                }
            })?;
            tools.multi_edit(path, &edits, optional_string_field(input, "expected_hash"))
        }
        name if name == ToolName::ApplyPatchFreeform.as_str() => {
            tools.apply_patch_freeform(string_field(input, "patch", name)?)
        }
        name if name == ToolName::ApplyPatchStructured.as_str() => {
            let patch = serde_json::from_value::<StructuredPatch>(input.clone()).map_err(|_| {
                ToolExecError::MissingField {
                    tool: name.to_string(),
                    field: "operations",
                }
            })?;
            tools.apply_patch_structured(&patch)
        }
        name if name == ToolName::Glob.as_str() => tools.glob(
            string_field(input, "pattern", name)?,
            optional_string_field(input, "path"),
        ),
        name if name == ToolName::Grep.as_str() => {
            tools.grep(string_field(input, "pattern", name)?)
        }
        name if name == ToolName::WebFetch.as_str() => {
            tools.web_fetch(string_field(input, "url", name)?)
        }
        name if name == ToolName::TodoWrite.as_str() => {
            let todos_value =
                input
                    .get("todos")
                    .cloned()
                    .ok_or_else(|| ToolExecError::MissingField {
                        tool: name.to_string(),
                        field: "todos",
                    })?;
            let todos =
                serde_json::from_value::<Vec<tools::TodoItem>>(todos_value).map_err(|_| {
                    ToolExecError::MissingField {
                        tool: name.to_string(),
                        field: "todos",
                    }
                })?;
            tools.todo_write(&todos)
        }
        name if name == ToolName::Bash.as_str() => {
            tools.bash(string_field(input, "command", name)?)
        }
        other => return Err(ToolExecError::UnknownTool(other.to_string())),
    };

    result.map_err(ToolExecError::Runtime)
}

pub async fn execute_tool_call_cancellable(
    tools: &ToolRuntime,
    call: &ParsedToolCall,
    cancel: CancellationToken,
) -> Result<tools::ToolResult, ToolExecError> {
    if call.name == ToolName::Bash.as_str() {
        return tools
            .bash_cancellable(string_field(&call.input, "command", &call.name)?, cancel)
            .await
            .map_err(ToolExecError::Runtime);
    }

    execute_tool_call(tools, call)
}

fn string_field<'a>(
    input: &'a Value,
    field: &'static str,
    tool: &str,
) -> Result<&'a str, ToolExecError> {
    input
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolExecError::MissingField {
            tool: tool.to_string(),
            field,
        })
}

fn optional_string_field<'a>(input: &'a Value, field: &str) -> Option<&'a str> {
    input.get(field).and_then(Value::as_str)
}

/// Configuration for a harness turn.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub model: String,
    pub max_tool_rounds: usize,
    pub approval_policy: ApprovalPolicy,
    pub context: ContextRuntimeConfig,
    pub prompt: PromptRuntimeConfig,
    pub hooks: PluginHooks,
    pub model_effort: ModelEffort,
    pub provider_family: ProviderFamily,
}

impl HarnessConfig {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_tool_rounds: 8,
            approval_policy: ApprovalPolicy::default(),
            context: ContextRuntimeConfig::default(),
            prompt: PromptRuntimeConfig::default(),
            hooks: PluginHooks::default(),
            model_effort: ModelEffort::default(),
            provider_family: ProviderFamily::Generic,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextRuntimeConfig {
    pub limits: ContextLimits,
    pub blob_root: Option<PathBuf>,
}

impl Default for ContextRuntimeConfig {
    fn default() -> Self {
        Self {
            limits: ContextLimits::default(),
            blob_root: None,
        }
    }
}

impl ContextRuntimeConfig {
    fn blob_store(&self) -> Option<BlobStore> {
        self.blob_root.clone().map(BlobStore::new)
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptRuntimeConfig {
    pub system_layers: Vec<String>,
}

impl PromptRuntimeConfig {
    pub fn with_system_layer(mut self, layer: impl Into<String>) -> Self {
        self.system_layers.push(layer.into());
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct PluginHooks {
    pub system_prompt_layers: Vec<String>,
    pub request_metadata: Map<String, Value>,
    pub advertised_tool_names: Vec<String>,
}

impl PluginHooks {
    pub fn with_system_prompt_layer(mut self, layer: impl Into<String>) -> Self {
        self.system_prompt_layers.push(layer.into());
        self
    }

    pub fn with_request_metadata(mut self, key: impl Into<String>, value: Value) -> Self {
        self.request_metadata.insert(key.into(), value);
        self
    }

    pub fn with_advertised_tool(mut self, name: impl Into<String>) -> Self {
        self.advertised_tool_names.push(name.into());
        self
    }
}

/// Whether a tool changes state (and so should be confirmed under the
/// `Mutating` policy). Read-only tools return false.
fn is_mutating_tool(name: &str) -> bool {
    risk::is_mutating_tool_name(name)
}

fn risk_reason(tool_name: &str, risk_flags: &[RiskFlag], outside_path: Option<&str>) -> String {
    let mut reason = if risk_flags.is_empty() {
        format!("approval required for {tool_name}")
    } else {
        let names = risk_flags
            .iter()
            .map(|flag| flag.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("{tool_name} flagged: {names}")
    };

    if let Some(path) = outside_path {
        reason.push_str(&format!(" (accessing outside workspace: {})", path));
    }

    reason
}

fn request_messages(
    messages: &[ContextMessage],
    first_turn_images: Vec<ImageAttachment>,
) -> Vec<ModelMessage> {
    let mut request_messages = messages
        .iter()
        .map(|message| ModelMessage::text(message.role.to_lowercase(), message.content.clone()))
        .collect::<Vec<_>>();

    if !first_turn_images.is_empty() {
        if let Some(last_user) = request_messages
            .iter_mut()
            .rev()
            .find(|message| message.role == "user")
        {
            last_user.parts.extend(
                first_turn_images
                    .into_iter()
                    .map(|image| MessagePart::Image { image }),
            );
        }
    }

    request_messages
}

fn advertised_tool_names() -> Vec<String> {
    tools::tool_names()
}

fn advertised_tool_names_for_hooks(hooks: &PluginHooks) -> Vec<String> {
    let mut names = advertised_tool_names();
    for name in &hooks.advertised_tool_names {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

fn request_metadata(config: &HarnessConfig, round: usize) -> Value {
    let mut metadata = Map::new();
    metadata.insert("provider_family".to_string(), json!(config.provider_family));
    metadata.insert("model_effort".to_string(), json!(config.model_effort));
    metadata.insert("approval_policy".to_string(), json!(config.approval_policy));
    metadata.insert("round".to_string(), json!(round));
    for (key, value) in &config.hooks.request_metadata {
        metadata.insert(key.clone(), value.clone());
    }
    Value::Object(metadata)
}

#[derive(Debug, Clone)]
struct PreparedProviderTurn {
    request: TurnRequest,
    context_event: SessionEvent,
}

struct ProviderRequestPreparer;

#[derive(Debug)]
struct ProviderRequestInput<'a> {
    session_id: SessionId,
    round: usize,
    state: &'a SessionState,
    turn_images: Vec<ImageAttachment>,
    config: &'a HarnessConfig,
    tools: &'a ToolRuntime,
}

impl ProviderRequestPreparer {
    fn prepare(input: ProviderRequestInput<'_>) -> anyhow::Result<PreparedProviderTurn> {
        let counter = ApproxTokenCounter;
        let environment =
            SystemEnvironment::capture(&input.config.model, input.tools.workspace_root());
        let system_preamble = PromptComposer::compose(
            input.config.provider_family,
            input.config.model_effort,
            &environment,
            &input.config.prompt,
            &input.config.hooks,
        );
        let prepared_context = prepare_context(
            &system_preamble,
            &input.state.context_messages(),
            &input.config.context.limits,
            &counter,
        )?;

        let context_event = SessionEvent::ContextPrepared {
            session_id: input.session_id,
            token_count: prepared_context.token_count as u64,
            original_token_count: prepared_context.original_token_count as u64,
            compacted: prepared_context.compacted,
            summary: prepared_context.summary.clone(),
        };

        let request = TurnRequest {
            session_id: input.session_id,
            model: input.config.model.clone(),
            prompt: prepared_context.prompt,
            system_prompt: Some(system_preamble),
            messages: request_messages(&prepared_context.messages, input.turn_images.clone()),
            tool_names: advertised_tool_names_for_hooks(&input.config.hooks),
            metadata: request_metadata(input.config, input.round),
            images: input.turn_images,
        };

        Ok(PreparedProviderTurn {
            request,
            context_event,
        })
    }
}

fn permission_preview_input(tools: &ToolRuntime, call: &ParsedToolCall) -> Value {
    let mut preview = call.input.clone();
    if !is_mutating_tool(&call.name) {
        return preview;
    }

    let targets = ToolTargets::capture(tools, call);
    let patch = targets.preview_patch(call);
    if patch.files.is_empty() {
        return preview;
    }

    let Value::Object(map) = &mut preview else {
        return preview;
    };
    if let Some(first) = patch.files.first() {
        map.entry("filepath".to_string())
            .or_insert_with(|| Value::String(first.path.clone()));
        if let Some(diff) = &first.diff {
            map.entry("diff".to_string())
                .or_insert_with(|| Value::String(diff.clone()));
        }
    }
    map.insert(
        "patch".to_string(),
        serde_json::to_value(&patch.files).unwrap_or_else(|_| Value::Array(Vec::new())),
    );
    preview
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalToolRunStatus {
    Executed,
    Denied,
}

#[derive(Debug, Clone)]
struct LocalToolRunResult {
    events: Vec<SessionEvent>,
    provider_response: ProviderToolResponse,
    status: LocalToolRunStatus,
}

#[allow(clippy::too_many_arguments)]
async fn run_local_tool_call(
    session_id: SessionId,
    tool_call_id: ToolCallId,
    tools: &ToolRuntime,
    approver: &dyn Approver,
    permissions: &mut risk::PermissionService<'_>,
    state: &mut SessionState,
    call: &ParsedToolCall,
    config: &HarnessConfig,
    cancel: CancellationToken,
) -> anyhow::Result<LocalToolRunResult> {
    let mut events = Vec::new();
    let risk_flags = risk::classify(call);
    let outside_path = get_outside_path(call);
    let preview_input = permission_preview_input(tools, call);
    let mut approved_unlocked_execution =
        permissions.is_allowed(call) && execution_needs_unlocked_access(call, &risk_flags);

    if permissions.is_denied_for_session(call) {
        let message = format!("tool call denied for this session: {}", call.name);
        events.push(SessionEvent::ToolCallError {
            session_id,
            tool_call_id,
            message: message.clone(),
        });
        state.push(Role::Tool, message.clone());
        return Ok(LocalToolRunResult {
            events,
            provider_response: ProviderToolResponse {
                tool_call_id,
                output: message,
                is_error: true,
            },
            status: LocalToolRunStatus::Denied,
        });
    }

    if !matches!(config.approval_policy, ApprovalPolicy::Never)
        && (outside_path.is_some()
            || permissions.should_request(config.approval_policy, &risk_flags, call))
    {
        let reason = risk_reason(&call.name, &risk_flags, outside_path.as_deref());
        let pending = permissions.begin_request(
            call,
            preview_input.clone(),
            risk_flags.clone(),
            reason.clone(),
        );
        events.push(SessionEvent::PermissionRequest {
            session_id,
            request_id: pending.request_id,
            reason: pending.reason.clone(),
            tool_name: pending.tool_name.clone(),
            input_json: pending.input.clone(),
        });
        events.push(SessionEvent::Status {
            session_id,
            status: SessionStatus::WaitingForPermission,
        });

        let decision = approver
            .decide(&ApprovalRequest {
                session_id,
                request_id: pending.request_id,
                tool_name: call.name.clone(),
                input: preview_input.clone(),
                risk_flags: risk_flags.clone(),
                reason,
                outside_path: outside_path.clone(),
            })
            .await;

        events.push(SessionEvent::PermissionResolved {
            session_id,
            request_id: pending.request_id,
            decision,
        });
        permissions.resolve(pending.request_id, decision, call);

        if matches!(decision, PermissionDecision::Deny) {
            let message = match approver.last_message() {
                Some(note) if !note.trim().is_empty() => {
                    format!("tool call denied by user: {} - {note}", call.name)
                }
                _ => format!("tool call denied by user: {}", call.name),
            };
            events.push(SessionEvent::ToolCallError {
                session_id,
                tool_call_id,
                message: message.clone(),
            });
            state.push(Role::Tool, message.clone());
            return Ok(LocalToolRunResult {
                events,
                provider_response: ProviderToolResponse {
                    tool_call_id,
                    output: message,
                    is_error: true,
                },
                status: LocalToolRunStatus::Denied,
            });
        }

        approved_unlocked_execution = execution_needs_unlocked_access(call, &risk_flags);
    }

    let targets = ToolTargets::capture(tools, call);
    events.push(SessionEvent::ToolInputStart {
        session_id,
        tool_call_id,
        name: call.name.clone(),
    });
    events.push(SessionEvent::ToolInputEnd {
        session_id,
        tool_call_id,
        input_json: preview_input.clone(),
    });
    events.push(SessionEvent::ToolCallStart {
        session_id,
        tool_call_id,
        name: call.name.clone(),
        input_json: preview_input.clone(),
    });
    events.push(SessionEvent::Status {
        session_id,
        status: SessionStatus::RunningTools,
    });

    let unlocked_tools;
    let execution_tools = if approved_unlocked_execution {
        unlocked_tools = tools.approved_outside_access();
        &unlocked_tools
    } else {
        tools
    };

    match execute_tool_call_cancellable(execution_tools, call, cancel.clone()).await {
        Ok(mut result) => {
            if sandbox_denied_result(tools, call, &result) && !approved_unlocked_execution {
                let reason = format!(
                    "{} was blocked by the workspace sandbox; approve to retry without the sandbox for this tool call",
                    call.name
                );
                let pending = permissions.begin_request(
                    call,
                    preview_input.clone(),
                    risk_flags.clone(),
                    reason.clone(),
                );
                events.push(SessionEvent::PermissionRequest {
                    session_id,
                    request_id: pending.request_id,
                    reason: pending.reason.clone(),
                    tool_name: pending.tool_name.clone(),
                    input_json: pending.input.clone(),
                });
                events.push(SessionEvent::Status {
                    session_id,
                    status: SessionStatus::WaitingForPermission,
                });
                let decision = approver
                    .decide(&ApprovalRequest {
                        session_id,
                        request_id: pending.request_id,
                        tool_name: call.name.clone(),
                        input: preview_input.clone(),
                        risk_flags: risk_flags.clone(),
                        reason,
                        outside_path: outside_path.clone(),
                    })
                    .await;
                events.push(SessionEvent::PermissionResolved {
                    session_id,
                    request_id: pending.request_id,
                    decision,
                });
                permissions.resolve(pending.request_id, decision, call);
                if !matches!(decision, PermissionDecision::Deny) {
                    let unlocked = tools.approved_outside_access();
                    result = execute_tool_call_cancellable(&unlocked, call, cancel.clone()).await?;
                }
            }
            let patch = targets.patch(tools);
            let blob_store = config.context.blob_store();
            let stubbed = stub_tool_output(
                &result.output,
                config.context.limits.tool_result_inline_bytes,
                blob_store.as_ref(),
            )?;
            events.push(SessionEvent::ToolCallResult {
                session_id,
                tool_call_id,
                title: Some(result.title.clone()),
                metadata: result.metadata.clone(),
                output: stubbed.inline_output.clone(),
                exit_code: result.exit_code,
            });
            state.push(
                Role::Tool,
                format!("{} result:\n{}", call.name, stubbed.inline_output),
            );
            if !patch.files.is_empty() {
                let diagnostics = diagnostics_for_patch(tools, &patch.files);
                events.push(SessionEvent::Patch {
                    session_id,
                    additions: patch.additions,
                    deletions: patch.deletions,
                    files: patch.files,
                });
                events.push(SessionEvent::Diagnostics {
                    session_id,
                    files: diagnostics,
                });
            }
            Ok(LocalToolRunResult {
                events,
                provider_response: ProviderToolResponse {
                    tool_call_id,
                    output: stubbed.inline_output,
                    is_error: false,
                },
                status: LocalToolRunStatus::Executed,
            })
        }
        Err(exec_error) => {
            let message = exec_error.to_string();

            if matches!(config.approval_policy, ApprovalPolicy::OnFailure)
                && !permissions.is_allowed(call)
            {
                let pending = permissions.begin_request(
                    call,
                    call.input.clone(),
                    risk_flags.clone(),
                    format!("{} failed: {message}", call.name),
                );
                events.push(SessionEvent::PermissionRequest {
                    session_id,
                    request_id: pending.request_id,
                    reason: pending.reason.clone(),
                    tool_name: pending.tool_name.clone(),
                    input_json: pending.input.clone(),
                });
                let decision = approver
                    .decide(&ApprovalRequest {
                        session_id,
                        request_id: pending.request_id,
                        tool_name: call.name.clone(),
                        input: call.input.clone(),
                        risk_flags: risk_flags.clone(),
                        reason: message.clone(),
                        outside_path: get_outside_path(call),
                    })
                    .await;
                events.push(SessionEvent::PermissionResolved {
                    session_id,
                    request_id: pending.request_id,
                    decision,
                });
                permissions.resolve(pending.request_id, decision, call);
            }

            events.push(SessionEvent::ToolCallError {
                session_id,
                tool_call_id,
                message: message.clone(),
            });
            state.push(Role::Tool, format!("{} error: {message}", call.name));
            Ok(LocalToolRunResult {
                events,
                provider_response: ProviderToolResponse {
                    tool_call_id,
                    output: message,
                    is_error: true,
                },
                status: LocalToolRunStatus::Executed,
            })
        }
    }
}

#[derive(Debug, Clone)]
struct ToolTargets {
    files: Vec<ToolTarget>,
}

#[derive(Debug, Clone)]
struct ToolTarget {
    path: String,
    absolute: PathBuf,
    before: Option<String>,
}

#[derive(Debug, Clone)]
struct ToolPatch {
    files: Vec<PatchFile>,
    additions: u64,
    deletions: u64,
}

impl ToolTargets {
    fn capture(tools: &ToolRuntime, call: &ParsedToolCall) -> Self {
        let paths = tool_target_paths(call);
        let files = paths
            .into_iter()
            .filter_map(|path| {
                let absolute = resolve_workspace_path(tools.workspace_root(), &path)?;
                let before = fs::read_to_string(&absolute).ok();
                Some(ToolTarget {
                    path,
                    absolute,
                    before,
                })
            })
            .collect();
        Self { files }
    }

    fn preview_patch(&self, call: &ParsedToolCall) -> ToolPatch {
        let files = self
            .files
            .iter()
            .filter_map(|target| {
                let after = predicted_after(call, target)?;
                patch_file(&target.path, target.before.as_deref().unwrap_or(""), &after)
            })
            .collect::<Vec<_>>();
        summarize_patch(files)
    }

    fn patch(&self, _tools: &ToolRuntime) -> ToolPatch {
        let files = self
            .files
            .iter()
            .filter_map(|target| {
                let after = fs::read_to_string(&target.absolute)
                    .ok()
                    .unwrap_or_default();
                patch_file(&target.path, target.before.as_deref().unwrap_or(""), &after)
            })
            .collect::<Vec<_>>();
        summarize_patch(files)
    }
}

fn tool_target_paths(call: &ParsedToolCall) -> Vec<String> {
    match call.name.as_str() {
        "write_file" | "edit_file" | "multi_edit" => optional_string_field(&call.input, "path")
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        "apply_patch_structured" => structured_patch_paths(&call.input),
        _ => Vec::new(),
    }
}

fn structured_patch_paths(input: &Value) -> Vec<String> {
    input
        .get("operations")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|operation| {
            operation
                .get("path")
                .or_else(|| operation.get("from"))
                .or_else(|| operation.get("to"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

fn predicted_after(call: &ParsedToolCall, target: &ToolTarget) -> Option<String> {
    let before = target.before.as_deref().unwrap_or("");
    match call.name.as_str() {
        "write_file" => call.input.get("content")?.as_str().map(str::to_string),
        "edit_file" => {
            let old = call.input.get("old")?.as_str()?;
            let new = call.input.get("new")?.as_str()?;
            Some(before.replacen(old, new, 1))
        }
        "multi_edit" => {
            let mut text = before.to_string();
            for edit in call.input.get("edits")?.as_array()? {
                let old = edit.get("old")?.as_str()?;
                let new = edit.get("new")?.as_str()?;
                text = text.replacen(old, new, 1);
            }
            Some(text)
        }
        _ => None,
    }
}

fn resolve_workspace_path(root: &Path, path: &str) -> Option<PathBuf> {
    let input = Path::new(path);
    if input.is_absolute() {
        return None;
    }
    let mut clean = PathBuf::new();
    for component in input.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(root.join(clean))
}

/// Extract the outside-workspace path from a tool call, if any.
fn get_outside_path(call: &ParsedToolCall) -> Option<String> {
    match call.name.as_str() {
        "read_file" | "glob" | "grep" | "list_dir" | "write_file" | "edit_file" | "multi_edit" => {
            call.input
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| path.starts_with('/') || path.contains(".."))
                .map(str::to_string)
        }
        "apply_patch_structured" => {
            if let Some(operations) = call.input.get("operations").and_then(Value::as_array) {
                for operation in operations {
                    for field in ["path", "from", "to"] {
                        if let Some(path) = operation
                            .get(field)
                            .and_then(Value::as_str)
                            .filter(|p| p.starts_with('/') || p.contains(".."))
                        {
                            return Some(path.to_string());
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn execution_needs_unlocked_access(call: &ParsedToolCall, risk_flags: &[RiskFlag]) -> bool {
    get_outside_path(call).is_some()
        || (call.name == ToolName::Bash.as_str()
            && risk_flags.iter().any(|flag| {
                matches!(
                    flag,
                    RiskFlag::WriteOutsideWorkspace
                        | RiskFlag::Sudo
                        | RiskFlag::NetworkAccess
                        | RiskFlag::PackagePublish
                )
            }))
}

fn sandbox_denied_result(
    tools: &ToolRuntime,
    call: &ParsedToolCall,
    result: &tools::ToolResult,
) -> bool {
    if call.name != ToolName::Bash.as_str()
        || !tools.sandbox().is_enforced()
        || result.exit_code == Some(0)
    {
        return false;
    }

    let output = result.output.to_lowercase();
    output.contains("operation not permitted")
        || output.contains("sandbox")
        || output.contains("deny")
        || output.contains("not allowed")
}

fn patch_file(path: &str, before: &str, after: &str) -> Option<PatchFile> {
    if before == after {
        return None;
    }
    let diff = unified_line_diff(path, before, after);
    Some(PatchFile {
        path: path.to_string(),
        additions: diff.additions,
        deletions: diff.deletions,
        diff: Some(diff.text),
    })
}

fn summarize_patch(files: Vec<PatchFile>) -> ToolPatch {
    let additions = files.iter().map(|file| file.additions).sum();
    let deletions = files.iter().map(|file| file.deletions).sum();
    ToolPatch {
        files,
        additions,
        deletions,
    }
}

fn diagnostics_for_patch(tools: &ToolRuntime, files: &[PatchFile]) -> Vec<DiagnosticFile> {
    files
        .iter()
        .map(|file| {
            let absolute = resolve_workspace_path(tools.workspace_root(), &file.path);
            let text = absolute
                .as_ref()
                .and_then(|path| fs::read_to_string(path).ok());
            DiagnosticFile {
                path: file.path.clone(),
                exists: absolute.as_ref().is_some_and(|path| path.exists()),
                bytes: text.as_ref().map(|text| text.len() as u64),
                lines: text.as_ref().map(|text| text.lines().count() as u64),
            }
        })
        .collect()
}

#[derive(Debug)]
struct GeneratedDiff {
    text: String,
    additions: u64,
    deletions: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeneratedDiffLineKind {
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone)]
struct GeneratedDiffLine {
    kind: GeneratedDiffLineKind,
    old_line: Option<u32>,
    new_line: Option<u32>,
    content: String,
}

fn unified_line_diff(path: &str, before: &str, after: &str) -> GeneratedDiff {
    let lines = generate_diff_lines(before, after);
    let additions = lines
        .iter()
        .filter(|line| line.kind == GeneratedDiffLineKind::Add)
        .count() as u64;
    let deletions = lines
        .iter()
        .filter(|line| line.kind == GeneratedDiffLineKind::Remove)
        .count() as u64;
    let mut text = String::new();
    text.push_str(&format!("--- a/{path}\n"));
    text.push_str(&format!("+++ b/{path}\n"));

    for (start, end) in diff_hunk_ranges(&lines, 3) {
        let hunk = &lines[start..end];
        let old_start = hunk.iter().find_map(|line| line.old_line).unwrap_or(0);
        let new_start = hunk.iter().find_map(|line| line.new_line).unwrap_or(0);
        let old_count = hunk.iter().filter(|line| line.old_line.is_some()).count();
        let new_count = hunk.iter().filter(|line| line.new_line.is_some()).count();
        text.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start, old_count, new_start, new_count
        ));
        for line in hunk {
            let prefix = match line.kind {
                GeneratedDiffLineKind::Context => ' ',
                GeneratedDiffLineKind::Add => '+',
                GeneratedDiffLineKind::Remove => '-',
            };
            text.push(prefix);
            text.push_str(&line.content);
            text.push('\n');
        }
    }

    GeneratedDiff {
        text,
        additions,
        deletions,
    }
}

fn generate_diff_lines(before: &str, after: &str) -> Vec<GeneratedDiffLine> {
    let old_lines = before.lines().map(str::to_string).collect::<Vec<_>>();
    let new_lines = after.lines().map(str::to_string).collect::<Vec<_>>();
    let mut matrix = vec![vec![0usize; new_lines.len() + 1]; old_lines.len() + 1];

    for old_index in (0..old_lines.len()).rev() {
        for new_index in (0..new_lines.len()).rev() {
            matrix[old_index][new_index] = if old_lines[old_index] == new_lines[new_index] {
                matrix[old_index + 1][new_index + 1] + 1
            } else {
                matrix[old_index + 1][new_index].max(matrix[old_index][new_index + 1])
            };
        }
    }

    let mut old_index = 0usize;
    let mut new_index = 0usize;
    let mut old_line_number = 1u32;
    let mut new_line_number = 1u32;
    let mut diff = Vec::new();

    while old_index < old_lines.len() && new_index < new_lines.len() {
        if old_lines[old_index] == new_lines[new_index] {
            diff.push(GeneratedDiffLine {
                kind: GeneratedDiffLineKind::Context,
                old_line: Some(old_line_number),
                new_line: Some(new_line_number),
                content: old_lines[old_index].clone(),
            });
            old_index += 1;
            new_index += 1;
            old_line_number += 1;
            new_line_number += 1;
        } else if matrix[old_index + 1][new_index] >= matrix[old_index][new_index + 1] {
            diff.push(GeneratedDiffLine {
                kind: GeneratedDiffLineKind::Remove,
                old_line: Some(old_line_number),
                new_line: None,
                content: old_lines[old_index].clone(),
            });
            old_index += 1;
            old_line_number += 1;
        } else {
            diff.push(GeneratedDiffLine {
                kind: GeneratedDiffLineKind::Add,
                old_line: None,
                new_line: Some(new_line_number),
                content: new_lines[new_index].clone(),
            });
            new_index += 1;
            new_line_number += 1;
        }
    }

    while old_index < old_lines.len() {
        diff.push(GeneratedDiffLine {
            kind: GeneratedDiffLineKind::Remove,
            old_line: Some(old_line_number),
            new_line: None,
            content: old_lines[old_index].clone(),
        });
        old_index += 1;
        old_line_number += 1;
    }
    while new_index < new_lines.len() {
        diff.push(GeneratedDiffLine {
            kind: GeneratedDiffLineKind::Add,
            old_line: None,
            new_line: Some(new_line_number),
            content: new_lines[new_index].clone(),
        });
        new_index += 1;
        new_line_number += 1;
    }

    diff
}

fn diff_hunk_ranges(lines: &[GeneratedDiffLine], context: usize) -> Vec<(usize, usize)> {
    let mut ranges = Vec::<(usize, usize)>::new();
    let mut index = 0usize;

    while index < lines.len() {
        while index < lines.len() && lines[index].kind == GeneratedDiffLineKind::Context {
            index += 1;
        }
        if index >= lines.len() {
            break;
        }

        let start = index.saturating_sub(context);
        let mut last_change = index;
        index += 1;
        while index < lines.len() {
            if lines[index].kind != GeneratedDiffLineKind::Context {
                last_change = index;
            } else if index > last_change + context {
                break;
            }
            index += 1;
        }
        let end = (last_change + context + 1).min(lines.len());

        if let Some((_, previous_end)) = ranges.last_mut() {
            if start <= *previous_end {
                *previous_end = (*previous_end).max(end);
                continue;
            }
        }
        ranges.push((start, end));
    }

    ranges
}

/// Run one user turn through the provider/tool loop, streaming
/// [`SessionEvent`]s. Borrows its inputs for the lifetime of the stream so
/// the caller keeps ownership of the session state and provider.
#[allow(clippy::too_many_arguments)]
pub fn run_turn<'a>(
    provider: &'a dyn ProviderPlugin,
    auth: &'a ProviderAuth,
    tools: &'a ToolRuntime,
    approver: &'a dyn Approver,
    allow: &'a mut AllowStore,
    state: &'a mut SessionState,
    user_prompt: String,
    config: HarnessConfig,
    cancel: CancellationToken,
    responses: provider_core::PermissionResponses,
) -> Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send + 'a>> {
    Box::pin(try_stream! {
        let session_id = state.session_id;
        let user_message = parse_multimodal_prompt(&user_prompt);
        state.push(Role::User, user_message.text.clone());
        // The provider-side permission channel is consumed by the first provider
        // turn (the only one that pauses for SDK tool permission). Later tool
        // rounds get a fresh, empty channel.
        let mut responses = Some(responses);
        let mut permissions = risk::PermissionService::new(allow);
        let mut loop_guard = ToolLoopGuard::new(3);

        yield SessionEvent::Status { session_id, status: SessionStatus::Starting };

        let mut round = 0usize;
        loop {
            if cancel.is_cancelled() {
                yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                return;
            }

            // --- Provider turn ---------------------------------------------
            yield SessionEvent::Status { session_id, status: SessionStatus::Streaming };
            yield SessionEvent::StepStart { session_id, index: round as u32 };

            let prepared_turn = ProviderRequestPreparer::prepare(ProviderRequestInput {
                session_id,
                round,
                state,
                turn_images: if round == 0 { user_message.images.clone() } else { Vec::new() },
                config: &config,
                tools,
            })?;
            yield prepared_turn.context_event;
            let request = prepared_turn.request;
            let turn_responses = responses
                .take()
                .unwrap_or_else(provider_core::empty_permission_responses);
            let (tool_response_tx, tool_response_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut stream = provider
                .stream_turn(auth, request, cancel.clone(), turn_responses, tool_response_rx)
                .await?;

            let mut assistant_text = String::new();
            let mut provider_stop_reason = StopReason::EndTurn;
            let text_id = format!("round-{round}-text-0");
            let mut text_started = false;
            let mut text_ended = false;
            loop {
                if cancel.is_cancelled() {
                    if text_started && !text_ended {
                        yield SessionEvent::TextEnd {
                            session_id,
                            text_id: text_id.clone(),
                            text: assistant_text.clone(),
                        };
                    }
                    yield SessionEvent::StepFinish {
                        session_id,
                        index: round as u32,
                        stop_reason: StopReason::Interrupted,
                    };
                    yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                    return;
                }

                let event = tokio::select! {
                    _ = cancel.cancelled() => {
                        if text_started && !text_ended {
                            yield SessionEvent::TextEnd {
                                session_id,
                                text_id: text_id.clone(),
                                text: assistant_text.clone(),
                            };
                        }
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: round as u32,
                            stop_reason: StopReason::Interrupted,
                        };
                        yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                        return;
                    }
                    event = stream.next() => event,
                };
                let Some(event) = event else {
                    break;
                };
                match event? {
                    SessionEvent::TextDelta { text, .. } => {
                        if !text_started {
                            text_started = true;
                            yield SessionEvent::TextStart {
                                session_id,
                                text_id: text_id.clone(),
                            };
                        }
                        assistant_text.push_str(&text);
                        yield SessionEvent::TextDelta { session_id, text };
                    }
                    SessionEvent::TextStart { session_id: event_session_id, text_id } => {
                        if !text_started {
                            text_started = true;
                        }
                        yield SessionEvent::TextStart { session_id: event_session_id, text_id };
                    }
                    SessionEvent::TextEnd { session_id: event_session_id, text_id, text } => {
                        text_ended = true;
                        yield SessionEvent::TextEnd { session_id: event_session_id, text_id, text };
                    }
                    SessionEvent::ToolCallRequested {
                        tool_call_id,
                        name,
                        input_json,
                        ..
                    } => {
                        let call = ParsedToolCall { name, input: input_json };
                        state.push(
                            Role::Assistant,
                            format!(
                                "tool call requested: {}",
                                json!({ "name": &call.name, "input": &call.input })
                            ),
                        );
                        if let Err(message) = loop_guard.record(&call) {
                            yield SessionEvent::ToolCallError {
                                session_id,
                                tool_call_id,
                                message: message.clone(),
                            };
                            yield SessionEvent::StepFinish {
                                session_id,
                                index: round as u32,
                                stop_reason: StopReason::Error,
                            };
                            yield SessionEvent::Error {
                                session_id,
                                message,
                            };
                            yield SessionEvent::Result { session_id, stop_reason: StopReason::Error };
                            return;
                        }
                        if cancel.is_cancelled() {
                            yield SessionEvent::StepFinish {
                                session_id,
                                index: round as u32,
                                stop_reason: StopReason::Interrupted,
                            };
                            yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                            return;
                        }
                        let run = run_local_tool_call(
                            session_id,
                            tool_call_id,
                            tools,
                            approver,
                            &mut permissions,
                            state,
                            &call,
                            &config,
                            cancel.clone(),
                        ).await?;
                        for event in run.events {
                            yield event;
                        }
                        let _ = tool_response_tx.send(run.provider_response);
                        if cancel.is_cancelled() {
                            yield SessionEvent::StepFinish {
                                session_id,
                                index: round as u32,
                                stop_reason: StopReason::Interrupted,
                            };
                            yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                            return;
                        }
                    }
                    // Provider-level result ends this round's stream.
                    SessionEvent::Result { stop_reason, .. } => {
                        provider_stop_reason = stop_reason;
                        break;
                    }
                    SessionEvent::Error { message, .. } => {
                        yield SessionEvent::Error { session_id, message: message.clone() };
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: round as u32,
                            stop_reason: StopReason::Error,
                        };
                        yield SessionEvent::Result { session_id, stop_reason: StopReason::Error };
                        return;
                    }
                    // Pass through any other normalized events unchanged.
                    other => yield other,
                }
            }

            if text_started && !text_ended {
                yield SessionEvent::TextEnd {
                    session_id,
                    text_id: text_id.clone(),
                    text: assistant_text.clone(),
                };
            }

            if provider_stop_reason == StopReason::Interrupted {
                yield SessionEvent::StepFinish {
                    session_id,
                    index: round as u32,
                    stop_reason: StopReason::Interrupted,
                };
                yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                return;
            }

            state.push(Role::Assistant, assistant_text.clone());

            // --- Tool-call detection ---------------------------------------
            match parse_tool_call(&assistant_text) {
                // No envelope: the assistant produced its final answer.
                None => {
                    yield SessionEvent::StepFinish {
                        session_id,
                        index: round as u32,
                        stop_reason: StopReason::EndTurn,
                    };
                    yield SessionEvent::Result { session_id, stop_reason: StopReason::EndTurn };
                    return;
                }
                // Malformed envelope: feed the error back so the model can fix it.
                Some(Err(parse_error)) => {
                    let tool_call_id = ToolCallId::new();
                    let message = parse_error.to_string();
                    yield SessionEvent::ToolCallError {
                        session_id,
                        tool_call_id,
                        message: message.clone(),
                    };
                    state.push(Role::Tool, format!("tool call error: {message}"));
                }
                // Well-formed envelope: gate it through approval, then execute.
                Some(Ok(call)) => {
                    let tool_call_id = ToolCallId::new();
                    if let Err(message) = loop_guard.record(&call) {
                        yield SessionEvent::ToolCallError {
                            session_id,
                            tool_call_id,
                            message: message.clone(),
                        };
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: round as u32,
                            stop_reason: StopReason::Error,
                        };
                        yield SessionEvent::Error {
                            session_id,
                            message,
                        };
                        yield SessionEvent::Result { session_id, stop_reason: StopReason::Error };
                        return;
                    }
                    if cancel.is_cancelled() {
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: round as u32,
                            stop_reason: StopReason::Interrupted,
                        };
                        yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                        return;
                    }
                    let run = run_local_tool_call(
                        session_id,
                        tool_call_id,
                        tools,
                        approver,
                        &mut permissions,
                        state,
                        &call,
                        &config,
                        cancel.clone(),
                    ).await?;
                    let denied = matches!(run.status, LocalToolRunStatus::Denied);
                    for event in run.events {
                        yield event;
                    }

                    if denied {
                        round += 1;
                        if round >= config.max_tool_rounds {
                            yield SessionEvent::StepFinish {
                                session_id,
                                index: (round - 1) as u32,
                                stop_reason: StopReason::Error,
                            };
                            yield SessionEvent::Error {
                                session_id,
                                message: format!(
                                    "reached max tool rounds ({})",
                                    config.max_tool_rounds
                                ),
                            };
                            yield SessionEvent::Result {
                                session_id,
                                stop_reason: StopReason::Error,
                            };
                            return;
                        }
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: (round - 1) as u32,
                            stop_reason: StopReason::EndTurn,
                        };
                        continue;
                    }

                    if cancel.is_cancelled() {
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: round as u32,
                            stop_reason: StopReason::Interrupted,
                        };
                        yield SessionEvent::Result {
                            session_id,
                            stop_reason: StopReason::Interrupted,
                        };
                        return;
                    }
                }
            }

            yield SessionEvent::StepFinish {
                session_id,
                index: round as u32,
                stop_reason: StopReason::EndTurn,
            };

            // --- Round limit -----------------------------------------------
            round += 1;
            if round >= config.max_tool_rounds {
                yield SessionEvent::Error {
                    session_id,
                    message: format!("reached max tool rounds ({})", config.max_tool_rounds),
                };
                yield SessionEvent::Result { session_id, stop_reason: StopReason::Error };
                return;
            }
        }
    })
}

#[derive(Debug, Clone)]
struct MultimodalPrompt {
    text: String,
    images: Vec<ImageAttachment>,
}

fn parse_multimodal_prompt(prompt: &str) -> MultimodalPrompt {
    const PREFIX: &str = "__MULTIMODAL_MESSAGE__:";
    let Some(payload) = prompt.strip_prefix(PREFIX) else {
        return MultimodalPrompt {
            text: prompt.to_string(),
            images: Vec::new(),
        };
    };

    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return MultimodalPrompt {
            text: prompt.to_string(),
            images: Vec::new(),
        };
    };

    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let images = value
        .get("images")
        .cloned()
        .and_then(|images| serde_json::from_value::<Vec<ImageAttachment>>(images).ok())
        .unwrap_or_default();

    MultimodalPrompt { text, images }
}

/// Providers with native tool-calling (Claude via the Agent SDK, Codex via the
/// Responses API `tools`) get a plain coding preamble — NOT the text-envelope
/// protocol, which they make unnecessary and which the model would leak.
const NATIVE_TOOLS_PREAMBLE: &str = "\
You are an Inductor coding agent working directly in the user's workspace. Use \
your tools to read, edit, and create files and run commands. Don't ask for \
permission to use tools or mention any tool-call format — just do the work. Keep \
explanations concise.

Working rules:
- Inspect relevant files before changing code; do not guess codebase structure.
- Prefer the smallest correct change that follows existing local patterns.
- Never revert or overwrite unrelated user changes.
- Continue after tool results until the task is complete, blocked, or clearly needs user input.
- Use precise edits for existing files and run focused verification when practical.
- Final responses should state the outcome and any verification performed without extra preamble.";

#[derive(Debug, Clone)]
struct SystemEnvironment {
    model: String,
    cwd: PathBuf,
    workspace_root: PathBuf,
    is_git_repo: bool,
    platform: &'static str,
    date_utc: String,
}

impl SystemEnvironment {
    fn capture(model: &str, workspace_root: &Path) -> Self {
        Self {
            model: model.to_string(),
            cwd: std::env::current_dir().unwrap_or_else(|_| workspace_root.to_path_buf()),
            workspace_root: workspace_root.to_path_buf(),
            is_git_repo: is_git_repo(workspace_root),
            platform: std::env::consts::OS,
            date_utc: OffsetDateTime::now_utc().date().to_string(),
        }
    }

    fn render(&self) -> String {
        format!(
            "Environment:\n<env>\n  Model: {}\n  Working directory: {}\n  Workspace root: {}\n  Is workspace a git repo: {}\n  Platform: {}\n  Current date (UTC): {}\n</env>",
            self.model,
            self.cwd.display(),
            self.workspace_root.display(),
            if self.is_git_repo { "yes" } else { "no" },
            self.platform,
            self.date_utc,
        )
    }
}

fn is_git_repo(workspace_root: &Path) -> bool {
    if workspace_root.join(".git").exists() {
        return true;
    }

    Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptLayer {
    name: &'static str,
    content: String,
}

struct PromptComposer;

impl PromptComposer {
    fn compose(
        provider: ProviderFamily,
        effort: ModelEffort,
        environment: &SystemEnvironment,
        prompt: &PromptRuntimeConfig,
        hooks: &PluginHooks,
    ) -> String {
        Self::layers(provider, effort, environment, prompt, hooks)
            .into_iter()
            .map(|layer| layer.content)
            .filter(|content| !content.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn layers(
        provider: ProviderFamily,
        effort: ModelEffort,
        environment: &SystemEnvironment,
        prompt: &PromptRuntimeConfig,
        hooks: &PluginHooks,
    ) -> Vec<PromptLayer> {
        let mut layers = vec![
            PromptLayer {
                name: "base",
                content: match provider {
                    ProviderFamily::Claude | ProviderFamily::Codex => {
                        String::from(NATIVE_TOOLS_PREAMBLE)
                    }
                    _ => generic_tools_preamble(),
                },
            },
            PromptLayer {
                name: "environment",
                content: environment.render(),
            },
        ];

        layers.extend(
            prompt
                .system_layers
                .iter()
                .cloned()
                .map(|content| PromptLayer {
                    name: "configured",
                    content,
                }),
        );
        layers.extend(
            hooks
                .system_prompt_layers
                .iter()
                .cloned()
                .map(|content| PromptLayer {
                    name: "plugin",
                    content,
                }),
        );

        if let Some(hint) = translate_effort(provider, effort).prompt_hint {
            layers.push(PromptLayer {
                name: "effort",
                content: hint,
            });
        }

        layers
    }
}

#[cfg(test)]
fn system_preamble_for_effort(
    provider: ProviderFamily,
    effort: ModelEffort,
    environment: &SystemEnvironment,
) -> String {
    PromptComposer::compose(
        provider,
        effort,
        environment,
        &PromptRuntimeConfig::default(),
        &PluginHooks::default(),
    )
}

#[cfg(test)]
mod tests;
