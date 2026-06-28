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
    time::Duration,
};

use ::time::OffsetDateTime;
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
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;
use tools::{StructuredPatch, TextEdit, ToolName, ToolRuntime};

pub mod risk;

pub use risk::AllowStore;

const TOOL_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
const TOOL_MODEL_CHECKPOINT_AFTER: Duration = Duration::from_secs(30);

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
Create and maintain a todo list for every user task with todo_write: create todos before substantive work, keep exactly one in_progress item while working, mark items completed as soon as they are done, and update/replace the list when the user's prompt changes the plan.\n\
When a feature, architecture, product, UX, data-loss, security, or other choice is ambiguous or important, ask the user instead of assuming. Use ask_questions with concrete options; every option needs a short description plus pros and cons, and mark your recommended option.\n\
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
    execute_tool_call_cancellable_until(tools, call, cancel, None).await
}

pub async fn execute_tool_call_cancellable_until(
    tools: &ToolRuntime,
    call: &ParsedToolCall,
    cancel: CancellationToken,
    checkpoint_after: Option<Duration>,
) -> Result<tools::ToolResult, ToolExecError> {
    match call.name.as_str() {
        name if name == ToolName::Bash.as_str() => tools
            .bash_cancellable_until(
                string_field(&call.input, "command", &call.name)?,
                cancel,
                checkpoint_after,
            )
            .await
            .map_err(ToolExecError::Runtime),
        name if name == ToolName::BashWait.as_str() => {
            let timeout_secs = optional_u64_field(&call.input, "timeout_secs").unwrap_or(30);
            tools
                .bash_wait(
                    string_field(&call.input, "command_id", &call.name)?,
                    Duration::from_secs(timeout_secs),
                )
                .await
                .map_err(ToolExecError::Runtime)
        }
        name if name == ToolName::BashKill.as_str() => tools
            .bash_kill(string_field(&call.input, "command_id", &call.name)?)
            .await
            .map_err(ToolExecError::Runtime),
        _ => execute_tool_call(tools, call),
    }
}

fn parse_agent_questions(input: &Value) -> Vec<harness_core::AgentQuestion> {
    let value = input.get("questions").cloned().unwrap_or_else(|| Value::Array(Vec::new()));
    serde_json::from_value::<Vec<harness_core::AgentQuestion>>(value)
        .map(|questions| tools::normalize_questions(&questions))
        .unwrap_or_default()
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

fn optional_u64_field(input: &Value, field: &str) -> Option<u64> {
    input.get(field).and_then(Value::as_u64)
}

/// Configuration for a harness turn.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    pub model: String,
    pub max_tool_rounds: usize,
    pub tool_model_checkpoint_after: Duration,
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
            tool_model_checkpoint_after: TOOL_MODEL_CHECKPOINT_AFTER,
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
    provider_response: ProviderToolResponse,
    status: LocalToolRunStatus,
}

#[derive(Debug, Default)]
struct ToolHashCache {
    hashes_by_path: HashMap<String, String>,
}

impl ToolHashCache {
    fn model_visible_call(&self, call: &ParsedToolCall) -> ParsedToolCall {
        let mut call = call.clone();
        strip_expected_hashes(&mut call.input);
        call
    }

    fn execution_call(&self, tools: &ToolRuntime, call: &ParsedToolCall) -> ParsedToolCall {
        let mut call = self.model_visible_call(call);
        attach_cached_hashes(tools, &mut call.input, &call.name, &self.hashes_by_path);
        call
    }

    fn record_success(
        &mut self,
        tools: &ToolRuntime,
        call: &ParsedToolCall,
        result: &tools::ToolResult,
    ) {
        if call.name == ToolName::ReadFile.as_str() {
            if let (Some(path), Some(hash)) = (
                result.metadata.get("path").and_then(Value::as_str),
                result.metadata.get("sha256").and_then(Value::as_str),
            ) {
                self.hashes_by_path
                    .insert(path.to_string(), hash.to_string());
            }
            return;
        }

        for path in tool_target_paths(call) {
            refresh_hash_for_path(tools, &path, &mut self.hashes_by_path);
        }
    }
}

#[derive(Debug)]
enum LocalToolRunUpdate {
    Event(SessionEvent),
    Done(LocalToolRunResult),
}

#[derive(Debug)]
enum ToolExecutionUpdate {
    Event(SessionEvent),
    Done(Result<tools::ToolResult, ToolExecError>),
}

#[allow(clippy::too_many_arguments)]
fn run_local_tool_call<'a>(
    session_id: SessionId,
    tool_call_id: ToolCallId,
    tools: &'a ToolRuntime,
    approver: &'a dyn Approver,
    permissions: &'a mut risk::PermissionService<'_>,
    state: &'a mut SessionState,
    call: &'a ParsedToolCall,
    hash_cache: &'a mut ToolHashCache,
    config: &'a HarnessConfig,
    cancel: CancellationToken,
) -> Pin<Box<dyn Stream<Item = anyhow::Result<LocalToolRunUpdate>> + Send + 'a>> {
    Box::pin(try_stream! {
    let display_call = hash_cache.model_visible_call(call);
    let execution_call = hash_cache.execution_call(tools, call);
    let risk_flags = risk::classify(&display_call);
    let outside_path = get_outside_path(&display_call);
    let preview_input = permission_preview_input(tools, &display_call);
    let mut approved_unlocked_execution =
        permissions.is_allowed(&display_call)
            && execution_needs_unlocked_access(&display_call, &risk_flags);

    if permissions.is_denied_for_session(&display_call) {
        let message = format!("tool call denied for this session: {}", display_call.name);
        yield LocalToolRunUpdate::Event(SessionEvent::ToolCallError {
            session_id,
            tool_call_id,
            message: message.clone(),
        });
        state.push(Role::Tool, message.clone());
        yield LocalToolRunUpdate::Done(LocalToolRunResult {
            provider_response: ProviderToolResponse {
                tool_call_id,
                output: message,
                is_error: true,
            },
            status: LocalToolRunStatus::Denied,
        });
        return;
    }

    if !matches!(config.approval_policy, ApprovalPolicy::Never)
        && (outside_path.is_some()
            || permissions.should_request(config.approval_policy, &risk_flags, &display_call))
    {
        let reason = risk_reason(&display_call.name, &risk_flags, outside_path.as_deref());
        let pending = permissions.begin_request(
            &display_call,
            preview_input.clone(),
            risk_flags.clone(),
            reason.clone(),
        );
        yield LocalToolRunUpdate::Event(SessionEvent::PermissionRequest {
            session_id,
            request_id: pending.request_id,
            reason: pending.reason.clone(),
            tool_name: pending.tool_name.clone(),
            input_json: pending.input.clone(),
        });
        yield LocalToolRunUpdate::Event(SessionEvent::Status {
            session_id,
            status: SessionStatus::WaitingForPermission,
        });

        let decision = approver
            .decide(&ApprovalRequest {
                session_id,
                request_id: pending.request_id,
                tool_name: display_call.name.clone(),
                input: preview_input.clone(),
                risk_flags: risk_flags.clone(),
                reason,
                outside_path: outside_path.clone(),
            })
            .await;

        yield LocalToolRunUpdate::Event(SessionEvent::PermissionResolved {
            session_id,
            request_id: pending.request_id,
            decision,
        });
        permissions.resolve(pending.request_id, decision, &display_call);

        if matches!(decision, PermissionDecision::Deny) {
            let message = match approver.last_message() {
                Some(note) if !note.trim().is_empty() => {
                    format!("tool call denied by user: {} - {note}", display_call.name)
                }
                _ => format!("tool call denied by user: {}", display_call.name),
            };
            yield LocalToolRunUpdate::Event(SessionEvent::ToolCallError {
                session_id,
                tool_call_id,
                message: message.clone(),
            });
            state.push(Role::Tool, message.clone());
            yield LocalToolRunUpdate::Done(LocalToolRunResult {
                provider_response: ProviderToolResponse {
                    tool_call_id,
                    output: message,
                    is_error: true,
                },
                status: LocalToolRunStatus::Denied,
            });
            return;
        }

        approved_unlocked_execution = execution_needs_unlocked_access(&display_call, &risk_flags);
    }

    let targets = ToolTargets::capture(tools, &display_call);
    yield LocalToolRunUpdate::Event(SessionEvent::ToolInputStart {
        session_id,
        tool_call_id,
        name: display_call.name.clone(),
    });
    yield LocalToolRunUpdate::Event(SessionEvent::ToolInputEnd {
        session_id,
        tool_call_id,
        input_json: preview_input.clone(),
    });
    yield LocalToolRunUpdate::Event(SessionEvent::ToolCallStart {
        session_id,
        tool_call_id,
        name: display_call.name.clone(),
        input_json: preview_input.clone(),
    });
    yield LocalToolRunUpdate::Event(SessionEvent::Status {
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

    let mut execution = execute_tool_call_with_progress(
        session_id,
        tool_call_id,
        execution_tools,
        &execution_call,
        cancel.clone(),
        config.tool_model_checkpoint_after,
    );
    let result = loop {
        let Some(update) = execution.next().await else {
            break Err(ToolExecError::Runtime(tools::ToolError::CommandCancelled {
                command: display_call.name.clone(),
            }));
        };
        match update? {
            ToolExecutionUpdate::Event(event) => yield LocalToolRunUpdate::Event(event),
            ToolExecutionUpdate::Done(result) => break result,
        }
    };
    drop(execution);

    match result {
        Ok(mut result) => {
            if sandbox_denied_result(tools, &display_call, &result) && !approved_unlocked_execution {
                let reason = format!(
                    "{} was blocked by the workspace sandbox; approve to retry without the sandbox for this tool call",
                    display_call.name
                );
                let pending = permissions.begin_request(
                    &display_call,
                    preview_input.clone(),
                    risk_flags.clone(),
                    reason.clone(),
                );
                yield LocalToolRunUpdate::Event(SessionEvent::PermissionRequest {
                    session_id,
                    request_id: pending.request_id,
                    reason: pending.reason.clone(),
                    tool_name: pending.tool_name.clone(),
                    input_json: pending.input.clone(),
                });
                yield LocalToolRunUpdate::Event(SessionEvent::Status {
                    session_id,
                    status: SessionStatus::WaitingForPermission,
                });
                let decision = approver
                    .decide(&ApprovalRequest {
                        session_id,
                        request_id: pending.request_id,
                        tool_name: display_call.name.clone(),
                        input: preview_input.clone(),
                        risk_flags: risk_flags.clone(),
                        reason,
                        outside_path: outside_path.clone(),
                    })
                    .await;
                yield LocalToolRunUpdate::Event(SessionEvent::PermissionResolved {
                    session_id,
                    request_id: pending.request_id,
                    decision,
                });
                permissions.resolve(pending.request_id, decision, &display_call);
                if !matches!(decision, PermissionDecision::Deny) {
                    let unlocked = tools.approved_outside_access();
                    let mut retry = execute_tool_call_with_progress(
                        session_id,
                        tool_call_id,
                        &unlocked,
                        &execution_call,
                        cancel.clone(),
                        config.tool_model_checkpoint_after,
                    );
                    result = loop {
                        let Some(update) = retry.next().await else {
                            break Err(ToolExecError::Runtime(tools::ToolError::CommandCancelled {
                                command: display_call.name.clone(),
                            }))?;
                        };
                        match update? {
                            ToolExecutionUpdate::Event(event) => {
                                yield LocalToolRunUpdate::Event(event)
                            }
                            ToolExecutionUpdate::Done(result) => break result?,
                        }
                    };
                }
            }
            hash_cache.record_success(tools, &execution_call, &result);
            let patch = targets.patch(tools);
            let blob_store = config.context.blob_store();
            let stubbed = stub_tool_output(
                &result.output,
                config.context.limits.tool_result_inline_bytes,
                blob_store.as_ref(),
            )?;
            yield LocalToolRunUpdate::Event(SessionEvent::ToolCallResult {
                session_id,
                tool_call_id,
                title: Some(result.title.clone()),
                metadata: result.metadata.clone(),
                output: stubbed.inline_output.clone(),
                exit_code: result.exit_code,
            });
            state.push(
                Role::Tool,
                format!("{} result:\n{}", display_call.name, stubbed.inline_output),
            );
            if !patch.files.is_empty() {
                let diagnostics = diagnostics_for_patch(tools, &patch.files);
                yield LocalToolRunUpdate::Event(SessionEvent::Patch {
                    session_id,
                    additions: patch.additions,
                    deletions: patch.deletions,
                    files: patch.files,
                });
                yield LocalToolRunUpdate::Event(SessionEvent::Diagnostics {
                    session_id,
                    files: diagnostics,
                });
            }
            yield LocalToolRunUpdate::Done(LocalToolRunResult {
                provider_response: ProviderToolResponse {
                    tool_call_id,
                    output: stubbed.inline_output,
                    is_error: false,
                },
                status: LocalToolRunStatus::Executed,
            });
        }
        Err(exec_error) => {
            let message = exec_error.to_string();

            if matches!(config.approval_policy, ApprovalPolicy::OnFailure)
                && !permissions.is_allowed(&display_call)
            {
                let pending = permissions.begin_request(
                    &display_call,
                    display_call.input.clone(),
                    risk_flags.clone(),
                    format!("{} failed: {message}", display_call.name),
                );
                yield LocalToolRunUpdate::Event(SessionEvent::PermissionRequest {
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
                        tool_name: display_call.name.clone(),
                        input: display_call.input.clone(),
                        risk_flags: risk_flags.clone(),
                        reason: message.clone(),
                        outside_path: get_outside_path(&display_call),
                    })
                    .await;
                yield LocalToolRunUpdate::Event(SessionEvent::PermissionResolved {
                    session_id,
                    request_id: pending.request_id,
                    decision,
                });
                permissions.resolve(pending.request_id, decision, &display_call);
            }

            yield LocalToolRunUpdate::Event(SessionEvent::ToolCallError {
                session_id,
                tool_call_id,
                message: message.clone(),
            });
            state.push(Role::Tool, format!("{} error: {message}", display_call.name));
            yield LocalToolRunUpdate::Done(LocalToolRunResult {
                provider_response: ProviderToolResponse {
                    tool_call_id,
                    output: message,
                    is_error: true,
                },
                status: LocalToolRunStatus::Executed,
            });
        }
    }
    })
}

fn execute_tool_call_with_progress<'a>(
    session_id: SessionId,
    tool_call_id: ToolCallId,
    tools: &'a ToolRuntime,
    call: &'a ParsedToolCall,
    cancel: CancellationToken,
    checkpoint_after: Duration,
) -> Pin<Box<dyn Stream<Item = anyhow::Result<ToolExecutionUpdate>> + Send + 'a>> {
    Box::pin(try_stream! {
        let execution = execute_tool_call_cancellable_until(tools, call, cancel, Some(checkpoint_after));
        tokio::pin!(execution);
        let started = Instant::now();
        let next_progress = time::sleep(TOOL_PROGRESS_INTERVAL);
        tokio::pin!(next_progress);

        loop {
            tokio::select! {
                result = &mut execution => {
                    yield ToolExecutionUpdate::Done(result);
                    break;
                }
                _ = &mut next_progress => {
                    yield ToolExecutionUpdate::Event(SessionEvent::ToolCallProgress {
                        session_id,
                        tool_call_id,
                        message: format!("still running for {}", format_elapsed(started.elapsed())),
                    });
                    next_progress.as_mut().reset(Instant::now() + TOOL_PROGRESS_INTERVAL);
                }
            }
        }
    })
}

fn format_elapsed(duration: Duration) -> String {
    let total = duration.as_secs();
    let minutes = total / 60;
    let seconds = total % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
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

fn strip_expected_hashes(input: &mut Value) {
    if let Some(object) = input.as_object_mut() {
        object.remove("expected_hash");
        if let Some(operations) = object.get_mut("operations").and_then(Value::as_array_mut) {
            for operation in operations {
                strip_expected_hashes(operation);
            }
        }
    }
}

fn attach_cached_hashes(
    tools: &ToolRuntime,
    input: &mut Value,
    tool_name: &str,
    hashes_by_path: &HashMap<String, String>,
) {
    match tool_name {
        "edit_file" | "multi_edit" => {
            let Some(path) = input.get("path").and_then(Value::as_str) else {
                return;
            };
            let Some(hash) = cached_hash_for_path(tools, path, hashes_by_path) else {
                return;
            };
            if let Some(object) = input.as_object_mut() {
                object.insert("expected_hash".to_string(), Value::String(hash));
            }
        }
        "apply_patch_structured" => {
            if let Some(operations) = input.get_mut("operations").and_then(Value::as_array_mut) {
                for operation in operations {
                    let Some(object) = operation.as_object_mut() else {
                        continue;
                    };
                    let path = object
                        .get("path")
                        .or_else(|| object.get("from"))
                        .and_then(Value::as_str);
                    let Some(path) = path else {
                        continue;
                    };
                    let Some(hash) = cached_hash_for_path(tools, path, hashes_by_path) else {
                        continue;
                    };
                    object.insert("expected_hash".to_string(), Value::String(hash));
                }
            }
        }
        _ => {}
    }
}

fn cached_hash_for_path(
    tools: &ToolRuntime,
    path: &str,
    hashes_by_path: &HashMap<String, String>,
) -> Option<String> {
    workspace_relative_key(tools, path).and_then(|key| hashes_by_path.get(&key).cloned())
}

fn refresh_hash_for_path(
    tools: &ToolRuntime,
    path: &str,
    hashes_by_path: &mut HashMap<String, String>,
) {
    let Some(key) = workspace_relative_key(tools, path) else {
        return;
    };
    match tools.read_file(&key) {
        Ok(result) => {
            if let Some(hash) = result.metadata.get("sha256").and_then(Value::as_str) {
                hashes_by_path.insert(key, hash.to_string());
            }
        }
        Err(_) => {
            hashes_by_path.remove(&key);
        }
    }
}

fn workspace_relative_key(tools: &ToolRuntime, path: &str) -> Option<String> {
    let input = Path::new(path);
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        tools.workspace_root().join(input)
    };
    let canonical = absolute.canonicalize().ok()?;
    let relative = canonical.strip_prefix(tools.workspace_root()).ok()?;
    Some(relative.to_string_lossy().replace('\\', "/"))
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
    question_responses: provider_core::QuestionResponses,
    question_requests: provider_core::QuestionRequests,
) -> Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send + 'a>> {
    Box::pin(try_stream! {
        let session_id = state.session_id;
        let user_message = parse_multimodal_prompt(&user_prompt);
        state.push(Role::User, user_message.text.clone());
        // The provider-side permission channel is consumed by the first provider
        // turn (the only one that pauses for SDK tool permission). Later tool
        // rounds get a fresh, empty channel.
        let mut responses = Some(responses);
        let mut question_responses = question_responses;
        let mut permissions = risk::PermissionService::new(allow);
        let mut hash_cache = ToolHashCache::default();

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
            // Native providers emit ask_questions as a regular tool request and
            // wait for the local tool response. The harness owns the UI answer
            // receiver so it can service those requests here.
            let turn_questions = provider_core::empty_question_responses();
            let turn_question_requests = provider_core::empty_question_requests();
            let mut stream = match provider
                .stream_turn(auth, request, cancel.clone(), turn_responses, tool_response_rx, turn_questions, turn_question_requests)
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    yield SessionEvent::Error {
                        session_id,
                        message: format!("provider failed to start turn: {error}"),
                    };
                    yield SessionEvent::StepFinish {
                        session_id,
                        index: round as u32,
                        stop_reason: StopReason::Error,
                    };
                    yield SessionEvent::Result {
                        session_id,
                        stop_reason: StopReason::Error,
                    };
                    return;
                }
            };

            let mut assistant_text = String::new();
            let mut provider_stop_reason = None;
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
                let event = match event {
                    Ok(event) => event,
                    Err(error) => {
                        if text_started && !text_ended {
                            yield SessionEvent::TextEnd {
                                session_id,
                                text_id: text_id.clone(),
                                text: assistant_text.clone(),
                            };
                        }
                        yield SessionEvent::Error {
                            session_id,
                            message: format!("provider stream failed: {error}"),
                        };
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: round as u32,
                            stop_reason: StopReason::Error,
                        };
                        yield SessionEvent::Result {
                            session_id,
                            stop_reason: StopReason::Error,
                        };
                        return;
                    }
                };
                match event {
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
                        if name == ToolName::AskQuestions.as_str() {
                            let questions = parse_agent_questions(&input_json);
                            let result = provider_core::ask_questions(
                                &question_requests,
                                &mut question_responses,
                                tool_call_id,
                                questions.clone(),
                            ).await;
                            yield SessionEvent::QuestionsAnswered {
                                session_id,
                                tool_call_id,
                                answers: result.answers.clone(),
                            };
                            let _ = tool_response_tx.send(ProviderToolResponse {
                                tool_call_id,
                                output: result.output,
                                is_error: false,
                            });
                            continue;
                        }
                        let call = ParsedToolCall { name, input: input_json };
                        state.push(
                            Role::Assistant,
                            format!(
                                "tool call requested: {}",
                                json!({ "name": &call.name, "input": &call.input })
                            ),
                        );
                        if cancel.is_cancelled() {
                            yield SessionEvent::StepFinish {
                                session_id,
                                index: round as u32,
                                stop_reason: StopReason::Interrupted,
                            };
                            yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                            return;
                        }
                        let mut updates = run_local_tool_call(
                            session_id,
                            tool_call_id,
                            tools,
                            approver,
                            &mut permissions,
                            state,
                            &call,
                            &mut hash_cache,
                            &config,
                            cancel.clone(),
                        );
                        let mut run = None;
                        while let Some(update) = updates.next().await {
                            match update? {
                                LocalToolRunUpdate::Event(event) => yield event,
                                LocalToolRunUpdate::Done(result) => run = Some(result),
                            }
                        }
                        drop(updates);
                        let run = run.ok_or_else(|| anyhow::anyhow!("tool run ended without a result"))?;
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
                        provider_stop_reason = Some(stop_reason);
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

            let Some(provider_stop_reason) = provider_stop_reason else {
                if text_started && !text_ended {
                    yield SessionEvent::TextEnd {
                        session_id,
                        text_id: text_id.clone(),
                        text: assistant_text.clone(),
                    };
                }
                yield SessionEvent::Error {
                    session_id,
                    message: "provider stream ended without a terminal result".to_string(),
                };
                yield SessionEvent::StepFinish {
                    session_id,
                    index: round as u32,
                    stop_reason: StopReason::Error,
                };
                yield SessionEvent::Result {
                    session_id,
                    stop_reason: StopReason::Error,
                };
                return;
            };

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
                    if cancel.is_cancelled() {
                        yield SessionEvent::StepFinish {
                            session_id,
                            index: round as u32,
                            stop_reason: StopReason::Interrupted,
                        };
                        yield SessionEvent::Result { session_id, stop_reason: StopReason::Interrupted };
                        return;
                    }
                    if call.name == ToolName::AskQuestions.as_str() {
                        let questions = parse_agent_questions(&call.input);
                        let result = provider_core::ask_questions(
                            &question_requests,
                            &mut question_responses,
                            tool_call_id,
                            questions,
                        ).await;
                        yield SessionEvent::QuestionsAnswered {
                            session_id,
                            tool_call_id,
                            answers: result.answers.clone(),
                        };
                        state.push(Role::Tool, result.output);
                        round += 1;
                        continue;
                    }
                    let mut updates = run_local_tool_call(
                        session_id,
                        tool_call_id,
                        tools,
                        approver,
                        &mut permissions,
                        state,
                        &call,
                        &mut hash_cache,
                        &config,
                        cancel.clone(),
                    );
                    let mut run = None;
                    while let Some(update) = updates.next().await {
                        match update? {
                            LocalToolRunUpdate::Event(event) => yield event,
                            LocalToolRunUpdate::Done(result) => run = Some(result),
                        }
                    }
                    drop(updates);
                    let run = run.ok_or_else(|| anyhow::anyhow!("tool run ended without a result"))?;
                    let denied = matches!(run.status, LocalToolRunStatus::Denied);

                    if denied {
                        round += 1;
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

            round += 1;
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
- Final responses should state the outcome and any verification performed without extra preamble.

Todo and question rules:
- `todo_write` and `ask_questions` are real tools available to you. Use those tools directly; do not simulate them in prose.
- Create and maintain a todo list for every user task with the `todo_write` tool before substantive work.
- Keep exactly one todo in_progress while actively working, and mark todos completed as soon as each is done.
- Update or replace todos when the user's next prompt changes the plan; clear stale todos if there is no remaining work.
- If the user asks you to ask questions, prompt for input, collect preferences, or choose among options, use the `ask_questions` tool instead of writing the questions in normal text.
- Ask the user instead of guessing on important or ambiguous feature, architecture, product, UX, data-loss, security, or other choice points.
- Use the `ask_questions` tool for such choices. Include options with one-line descriptions, pros, cons, and a recommended option; the user can still choose a custom answer.";

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
                    ProviderFamily::Claude | ProviderFamily::Codex | ProviderFamily::Copilot => {
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
