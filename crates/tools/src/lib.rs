use std::{
    collections::HashMap,
    fmt, fs, io,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use harness_core::{AgentQuestion, QuestionOption};
use sandbox::SandboxPolicy;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{io::AsyncReadExt, process::Command as TokioCommand, sync::Notify};
use tokio_util::sync::CancellationToken;

const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 16 * 1024;
const DEFAULT_GREP_MATCH_LIMIT: usize = 100;
const DEFAULT_GLOB_MATCH_LIMIT: usize = 100;
const WEB_FETCH_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolName {
    ReadFile,
    ReadBlob,
    ListDir,
    ReadMemory,
    WriteMemory,
    WriteFile,
    EditFile,
    MultiEdit,
    ApplyPatch,
    ApplyPatchFreeform,
    ApplyPatchStructured,
    Glob,
    Grep,
    WebFetch,
    TodoWrite,
    AskQuestions,
    Bash,
    BashWait,
    BashKill,
}

impl ToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::ReadBlob => "read_blob",
            Self::ListDir => "list_dir",
            Self::ReadMemory => "read_memory",
            Self::WriteMemory => "write_memory",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
            Self::MultiEdit => "multi_edit",
            Self::ApplyPatch => "apply_patch",
            Self::ApplyPatchFreeform => "apply_patch_freeform",
            Self::ApplyPatchStructured => "apply_patch_structured",
            Self::Glob => "glob",
            Self::Grep => "grep",
            Self::WebFetch => "web_fetch",
            Self::TodoWrite => "todo_write",
            Self::AskQuestions => "ask_questions",
            Self::Bash => "bash",
            Self::BashWait => "bash_wait",
            Self::BashKill => "bash_kill",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::ReadFile => "Read File",
            Self::ReadBlob => "Read Stored Tool Output",
            Self::ListDir => "List Directory",
            Self::ReadMemory => "Read Repo Memory",
            Self::WriteMemory => "Write Repo Memory",
            Self::WriteFile => "Write File",
            Self::EditFile => "Edit File",
            Self::MultiEdit => "Multi Edit",
            Self::ApplyPatch => "Apply Patch",
            Self::ApplyPatchFreeform => "Apply Patch",
            Self::ApplyPatchStructured => "Apply Patch",
            Self::Glob => "Find Files",
            Self::Grep => "Search Text",
            Self::WebFetch => "Fetch Webpage",
            Self::TodoWrite => "Update Todo List",
            Self::AskQuestions => "Ask User Questions",
            Self::Bash => "Run Command",
            Self::BashWait => "Wait For Command",
            Self::BashKill => "Stop Command",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: ToolName,
    pub description: &'static str,
    pub input_schema: serde_json::Value,
    pub read_only: bool,
}

pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: ToolName::ReadFile,
            description: "Read a UTF-8 text file. Paths may be workspace-relative or absolute when unrestricted access is enabled. Prefer start_line/end_line for large files.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "start_line": { "type": "integer", "description": "Optional 1-based first line to return" },
                    "end_line": { "type": "integer", "description": "Optional 1-based last line to return, inclusive" }
                },
                "required": ["path"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::ReadBlob,
            description: "Read previously stored full output for a truncated tool result without rerunning the original tool. Use the blob_id shown in a truncated tool result.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "blob_id": { "type": "string", "description": "Blob id from a truncated tool result" },
                    "start_byte": { "type": "integer", "description": "Optional 0-based byte offset to start reading" },
                    "limit_bytes": { "type": "integer", "description": "Optional max bytes to return; defaults to the normal tool output cap" }
                },
                "required": ["blob_id"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::ListDir,
            description: "List entries in a directory, with directories suffixed by slash.",
            input_schema: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Optional directory path" } }
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::ReadMemory,
            description: "Read this repository's shared memory markdown file. The memory is shared by all Inductor sessions and worktrees for the same source repo.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::WriteMemory,
            description: "Replace this repository's shared memory markdown file. Keep it concise and durable: stable project conventions, recurring workflows, known pitfalls, and useful context. Do not store secrets.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Full markdown content to write to the repo memory file." }
                },
                "required": ["content"]
            }),
            read_only: false,
        },
        ToolDefinition {
            name: ToolName::ApplyPatch,
            description: "Apply exact line-aware file changes in the workspace. Prefer this for all file writes. Updates must include the exact 1-based inclusive start_line/end_line range plus the old text expected in that range; the tool will not search for ambiguous anchors. Multiple updates to the same file are applied against one original snapshot and written once. Use add_file for new files and delete_file for deletes.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "operations": {
                        "type": "array",
                        "description": "Line-aware patch operations. For edits, first call read_file with start_line/end_line to get exact line numbers and old text.",
                        "items": {
                            "oneOf": [
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "type": "string", "enum": ["update"] },
                                        "path": { "type": "string", "description": "Workspace-relative or allowed absolute path" },
                                        "start_line": { "type": "integer", "description": "1-based first line of the old range; use total_lines + 1 with old = \"\" to append." },
                                        "end_line": { "type": "integer", "description": "1-based last line of the old range, inclusive. For insertions where old is empty, set end_line equal to start_line." },
                                        "old": { "type": "string", "description": "Exact text expected in start_line..end_line inclusive. Use empty string only for insertion." },
                                        "new": { "type": "string", "description": "Replacement text for the inclusive line range." }
                                    },
                                    "required": ["op", "path", "start_line", "end_line", "old", "new"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "type": "string", "enum": ["add_file"] },
                                        "path": { "type": "string", "description": "Workspace-relative or allowed absolute path" },
                                        "content": { "type": "string", "description": "Full file contents." }
                                    },
                                    "required": ["op", "path", "content"]
                                },
                                {
                                    "type": "object",
                                    "properties": {
                                        "op": { "type": "string", "enum": ["delete_file"] },
                                        "path": { "type": "string", "description": "Workspace-relative or allowed absolute path" }
                                    },
                                    "required": ["op", "path"]
                                }
                            ]
                        }
                    }
                },
                "required": ["operations"]
            }),
            read_only: false,
        },
        ToolDefinition {
            name: ToolName::Glob,
            description: "Find files by glob pattern.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern" },
                    "path": { "type": "string", "description": "Optional directory path" }
                },
                "required": ["pattern"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::Grep,
            description: "Search text files for a substring or regular expression.",
            input_schema: json!({
                "type": "object",
                "properties": { "pattern": { "type": "string", "description": "Search pattern" } },
                "required": ["pattern"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::WebFetch,
            description: "Fetch a webpage and return its text content.",
            input_schema: json!({
                "type": "object",
                "properties": { "url": { "type": "string", "description": "HTTP or HTTPS URL" } },
                "required": ["url"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::TodoWrite,
            description: "Update the session todo list shown in the UI. Use for every task; call at the start with planned steps, when starting a step, and when completing steps. Pass an empty list to clear stale todos.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string" },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::AskQuestions,
            description: "Pause for user input on important choices. Provide one or more questions, each with options that include label, description, pros, cons, and an optional recommended label.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "question": { "type": "string" },
                                "recommended": { "type": "string", "description": "The recommended option label" },
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "type": "string" },
                                            "description": { "type": "string" },
                                            "pros": { "type": "string" },
                                            "cons": { "type": "string" }
                                        },
                                        "required": ["label", "description", "pros", "cons"]
                                    }
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::Bash,
            description: "Run a shell command in the workspace. Long-running commands may return a checkpoint with a command_id; use bash_wait to keep waiting for final output or bash_kill to stop it.",
            input_schema: json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "Shell command" } },
                "required": ["command"]
            }),
            read_only: false,
        },
        ToolDefinition {
            name: ToolName::BashWait,
            description: "Continue waiting for a bash command that returned a checkpoint. Returns final output when the command finishes, or another checkpoint after timeout_secs while it keeps running.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command_id": { "type": "string", "description": "command_id returned by a bash checkpoint" },
                    "timeout_secs": { "type": "integer", "description": "Seconds to wait before returning another checkpoint; defaults to 30" }
                },
                "required": ["command_id"]
            }),
            read_only: true,
        },
        ToolDefinition {
            name: ToolName::BashKill,
            description: "Stop a bash command that is still running after a checkpoint and return the output captured so far.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command_id": { "type": "string", "description": "command_id returned by a bash checkpoint" }
                },
                "required": ["command_id"]
            }),
            read_only: false,
        },
    ]
}

pub fn tool_names() -> Vec<String> {
    tool_definitions()
        .into_iter()
        .map(|definition| definition.name.as_str().to_string())
        .collect()
}

pub fn tool_prompt_docs() -> String {
    tool_definitions()
        .into_iter()
        .map(|definition| {
            format!(
                "- {} {}",
                definition.name.as_str(),
                compact_schema(&definition.input_schema)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn compact_schema(schema: &serde_json::Value) -> String {
    serde_json::to_string(schema).unwrap_or_else(|_| "{}".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskQuestionsInput {
    pub questions: Vec<AgentQuestion>,
}

pub fn normalize_questions(questions: &[AgentQuestion]) -> Vec<AgentQuestion> {
    questions
        .iter()
        .filter(|question| !question.question.trim().is_empty())
        .map(|question| AgentQuestion {
            id: question.id.clone(),
            question: question.question.clone(),
            recommended: question.recommended.clone(),
            options: question
                .options
                .iter()
                .filter(|option| !option.label.trim().is_empty())
                .map(|option| QuestionOption {
                    label: option.label.clone(),
                    description: option.description.clone(),
                    pros: option.pros.clone(),
                    cons: option.cons.clone(),
                })
                .collect(),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEdit {
    pub old: String,
    pub new: String,
}

impl TextEdit {
    pub fn new(old: impl Into<String>, new: impl Into<String>) -> Self {
        Self {
            old: old.into(),
            new: new.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StructuredPatchOperation {
    Edit {
        path: PathBuf,
        old: String,
        new: String,
        expected_hash: Option<String>,
    },
    MultiEdit {
        path: PathBuf,
        edits: Vec<TextEdit>,
        expected_hash: Option<String>,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
        expected_hash: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredPatch {
    pub operations: Vec<StructuredPatchOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LinePatchOperation {
    Update {
        path: PathBuf,
        start_line: usize,
        end_line: usize,
        old: String,
        new: String,
        expected_hash: Option<String>,
    },
    AddFile {
        path: PathBuf,
        content: String,
    },
    DeleteFile {
        path: PathBuf,
        expected_hash: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinePatch {
    pub operations: Vec<LinePatchOperation>,
}

#[derive(Debug)]
struct PendingLineUpdate {
    start_line: usize,
    end_line: usize,
    old: String,
    new: String,
}

#[derive(Debug)]
struct PendingFileLineUpdates {
    display_path: PathBuf,
    expected_hash: Option<String>,
    updates: Vec<PendingLineUpdate>,
}

#[derive(Debug)]
struct NormalizedLineUpdate {
    start_index: usize,
    end_index: usize,
    new_lines: Vec<String>,
    insertion: bool,
}

#[derive(Debug, Clone)]
pub struct ToolRuntime {
    workspace_root: PathBuf,
    memory_file: Option<PathBuf>,
    output_limit_bytes: usize,
    grep_match_limit: usize,
    sandbox: SandboxPolicy,
    allow_outside_paths: bool,
    background_commands: Arc<Mutex<HashMap<String, BackgroundCommand>>>,
    background_command_counter: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
struct BackgroundCommand {
    id: String,
    command: String,
    started_at: Instant,
    stdout_buffer: Arc<Mutex<Vec<u8>>>,
    stderr_buffer: Arc<Mutex<Vec<u8>>>,
    status: Arc<Mutex<BackgroundCommandStatus>>,
    kill: CancellationToken,
    notify: Arc<Notify>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundCommandStatus {
    Running,
    Completed { exit_code: Option<i32> },
    Killed { exit_code: Option<i32> },
}

impl BackgroundCommand {
    fn status(&self) -> BackgroundCommandStatus {
        self.status
            .lock()
            .map(|status| *status)
            .unwrap_or(BackgroundCommandStatus::Running)
    }

    fn combined_output(&self) -> String {
        let stdout = self
            .stdout_buffer
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let stderr = self
            .stderr_buffer
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&stdout));
        combined.push_str(&String::from_utf8_lossy(&stderr));
        combined
    }
}

impl ToolRuntime {
    /// Build a runtime with no shell sandboxing. Path validation still applies
    /// to `read_file`/`write_file`; only `bash` runs unconfined.
    pub fn new(workspace_root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let workspace_root = Self::canonical_workspace(workspace_root.as_ref())?;
        Ok(Self {
            workspace_root,
            memory_file: None,
            output_limit_bytes: DEFAULT_OUTPUT_LIMIT_BYTES,
            grep_match_limit: DEFAULT_GREP_MATCH_LIMIT,
            sandbox: SandboxPolicy::Disabled,
            allow_outside_paths: false,
            background_commands: Arc::new(Mutex::new(HashMap::new())),
            background_command_counter: Arc::new(AtomicU64::new(1)),
        })
    }

    /// Build a runtime with unrestricted local access. File tools may resolve
    /// absolute paths and `..` paths outside the workspace, and bash runs
    /// without the macOS workspace sandbox.
    pub fn unrestricted(workspace_root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let mut runtime = Self::new(workspace_root)?;
        runtime.allow_outside_paths = true;
        Ok(runtime)
    }

    /// Build a per-call runtime for a tool execution the user explicitly approved.
    /// File paths may resolve outside the workspace and bash runs without the
    /// workspace write sandbox; callers should only use this for the approved call.
    pub fn approved_outside_access(&self) -> Self {
        Self {
            workspace_root: self.workspace_root.clone(),
            memory_file: self.memory_file.clone(),
            output_limit_bytes: self.output_limit_bytes,
            grep_match_limit: self.grep_match_limit,
            sandbox: SandboxPolicy::Disabled,
            allow_outside_paths: true,
            background_commands: self.background_commands.clone(),
            background_command_counter: self.background_command_counter.clone(),
        }
    }

    /// Build a runtime whose `bash` tool is confined by `sandbox`.
    pub fn with_sandbox(
        workspace_root: impl AsRef<Path>,
        sandbox: SandboxPolicy,
    ) -> Result<Self, ToolError> {
        let mut runtime = Self::new(workspace_root)?;
        runtime.sandbox = sandbox;
        Ok(runtime)
    }

    /// Build a runtime whose `bash` tool is confined to the (canonicalized)
    /// workspace and tempdir, with network denied. This is the default policy
    /// the harness uses so commands cannot write outside the workspace.
    pub fn sandboxed(workspace_root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let mut runtime = Self::new(workspace_root)?;
        // Build the policy from the already-canonicalized root so Seatbelt's
        // path matching lines up with the real filesystem path.
        runtime.sandbox = SandboxPolicy::workspace_default(&runtime.workspace_root);
        Ok(runtime)
    }

    fn canonical_workspace(workspace_root: &Path) -> Result<PathBuf, ToolError> {
        let metadata = fs::metadata(workspace_root)
            .map_err(|err| ToolError::workspace_io(workspace_root, err))?;
        if !metadata.is_dir() {
            return Err(ToolError::WorkspaceNotDirectory {
                path: workspace_root.to_path_buf(),
            });
        }

        workspace_root
            .canonicalize()
            .map_err(|err| ToolError::workspace_io(workspace_root, err))
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn with_memory_file(mut self, memory_file: impl Into<PathBuf>) -> Self {
        self.memory_file = Some(memory_file.into());
        self
    }

    pub fn memory_file(&self) -> Option<&Path> {
        self.memory_file.as_deref()
    }

    fn configured_memory_file(&self) -> Result<PathBuf, ToolError> {
        self.memory_file
            .clone()
            .ok_or_else(ToolError::memory_unavailable)
    }

    pub fn sandbox(&self) -> &SandboxPolicy {
        &self.sandbox
    }

    fn relative_path(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    }

    pub fn read_file(&self, path: impl AsRef<Path>) -> Result<ToolResult, ToolError> {
        self.read_file_range(path, None, None)
    }

    pub fn read_file_range(
        &self,
        path: impl AsRef<Path>,
        start_line: Option<usize>,
        end_line: Option<usize>,
    ) -> Result<ToolResult, ToolError> {
        let path = self.resolve_existing_path(path.as_ref())?;
        let output = fs::read_to_string(&path).map_err(|err| ToolError::io(&path, err))?;
        let sha256 = sha256_hex(output.as_bytes());
        let bytes = output.len();
        let total_lines = output.lines().count();
        let requested_start = start_line.unwrap_or(1).max(1);
        let requested_end = end_line.unwrap_or(total_lines).max(requested_start);
        let ranged = start_line.is_some() || end_line.is_some();
        let output = if ranged {
            output
                .lines()
                .enumerate()
                .filter_map(|(index, line)| {
                    let line_number = index + 1;
                    (line_number >= requested_start && line_number <= requested_end)
                        .then(|| format!("{line_number}: {line}"))
                })
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            output
        };
        let returned_lines = if ranged {
            output.lines().count()
        } else {
            total_lines
        };

        Ok(ToolResult::success(
            ToolName::ReadFile,
            cap_output(output, self.output_limit_bytes),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "path": self.relative_path(&path),
                "bytes": bytes,
                "sha256": sha256,
                "total_lines": total_lines,
                "start_line": if ranged { Some(requested_start) } else { None },
                "end_line": if ranged { Some(requested_end.min(total_lines)) } else { None },
                "returned_lines": returned_lines,
            }))
        })
    }

    pub fn list_dir(&self, path: Option<impl AsRef<Path>>) -> Result<ToolResult, ToolError> {
        let path = match path {
            Some(path) => self.resolve_existing_path(path.as_ref())?,
            None => self.workspace_root.clone(),
        };
        let metadata = fs::metadata(&path).map_err(|err| ToolError::io(&path, err))?;
        if !metadata.is_dir() {
            return Err(ToolError::NotDirectory { path });
        }

        let mut entries = fs::read_dir(&path)
            .map_err(|err| ToolError::io(&path, err))?
            .map(|entry| {
                let entry = entry.map_err(|err| ToolError::io(&path, err))?;
                let metadata = entry
                    .metadata()
                    .map_err(|err| ToolError::io(&entry.path(), err))?;
                let mut name = entry.file_name().to_string_lossy().to_string();
                if metadata.is_dir() {
                    name.push('/');
                }
                Ok(name)
            })
            .collect::<Result<Vec<_>, ToolError>>()?;
        entries.sort();

        let entry_count = entries.len();
        Ok(ToolResult::success(
            ToolName::ListDir,
            CappedOutput::complete(if entries.is_empty() {
                "No entries found".to_string()
            } else {
                entries.join("\n")
            }),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "path": self.relative_path(&path),
                "entries": entry_count,
            }))
        })
    }

    pub fn read_memory(&self) -> Result<ToolResult, ToolError> {
        let path = self.configured_memory_file()?;
        match fs::read_to_string(&path) {
            Ok(output) => {
                let sha256 = sha256_hex(output.as_bytes());
                let bytes = output.len();
                Ok(ToolResult::success(
                    ToolName::ReadMemory,
                    cap_output(output, self.output_limit_bytes),
                ))
                .map(|result| {
                    result.with_metadata(json!({
                        "path": path.display().to_string(),
                        "exists": true,
                        "bytes": bytes,
                        "sha256": sha256,
                    }))
                })
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(ToolResult::success(
                ToolName::ReadMemory,
                CappedOutput::complete(format!(
                    "No repo memory file exists yet at {}. Use write_memory to create it when you learn durable project context.",
                    path.display()
                )),
            ))
            .map(|result| {
                result.with_metadata(json!({
                    "path": path.display().to_string(),
                    "exists": false,
                    "bytes": 0,
                }))
            }),
            Err(err) => Err(ToolError::io(&path, err)),
        }
    }

    pub fn write_memory(&self, content: impl AsRef<str>) -> Result<ToolResult, ToolError> {
        let path = self.configured_memory_file()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| ToolError::io(parent, err))?;
        }
        fs::write(&path, content.as_ref()).map_err(|err| ToolError::io(&path, err))?;

        Ok(ToolResult::success(
            ToolName::WriteMemory,
            CappedOutput::complete(format!("wrote repo memory to {}", path.display())),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "path": path.display().to_string(),
                "bytes": content.as_ref().len(),
            }))
        })
    }

    pub fn write_file(
        &self,
        path: impl AsRef<Path>,
        content: impl AsRef<str>,
    ) -> Result<ToolResult, ToolError> {
        let path = self.resolve_write_path(path.as_ref())?;
        fs::write(&path, content.as_ref()).map_err(|err| ToolError::io(&path, err))?;

        Ok(ToolResult::success(
            ToolName::WriteFile,
            CappedOutput::complete(format!("wrote {}", path.display())),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "path": self.relative_path(&path),
                "bytes": content.as_ref().len(),
            }))
        })
    }

    pub fn edit_file(
        &self,
        path: impl AsRef<Path>,
        old: impl AsRef<str>,
        new: impl AsRef<str>,
        expected_hash: Option<&str>,
    ) -> Result<ToolResult, ToolError> {
        let edit = TextEdit::new(old.as_ref(), new.as_ref());
        let mut result = self.multi_edit(path, &[edit], expected_hash)?;
        result.name = ToolName::EditFile;
        result.title = ToolName::EditFile.title().to_string();
        Ok(result)
    }

    pub fn multi_edit(
        &self,
        path: impl AsRef<Path>,
        edits: &[TextEdit],
        expected_hash: Option<&str>,
    ) -> Result<ToolResult, ToolError> {
        if edits.is_empty() {
            return Err(ToolError::EmptyEdit);
        }

        let path = self.resolve_existing_path(path.as_ref())?;
        let text_file = self.read_text_file(&path, expected_hash)?;
        let mut content = text_file.text;
        let newline = text_file.newline;

        for edit in edits {
            let old = normalize_edit_text(&edit.old, newline);
            let new = normalize_edit_text(&edit.new, newline);
            if old.is_empty() {
                return Err(ToolError::EmptyEdit);
            }
            let count = content.matches(&old).count();
            if count == 0 {
                if !new.is_empty() && content.matches(&new).count() == 1 {
                    // The requested replacement is already present and the target is
                    // absent. Treat this as an idempotent success: callers can retry
                    // after transport/UI interruptions without reporting a false
                    // patch failure for an edit that did land.
                    continue;
                }
                return Err(ToolError::EditTargetNotFound {
                    path: path.clone(),
                    old: edit.old.clone(),
                    hint: edit_target_hint(&content, &old),
                });
            }
            if count > 1 {
                return Err(ToolError::EditTargetNotUnique {
                    path: path.clone(),
                    old: edit.old.clone(),
                    count,
                    hint: edit_target_hint(&content, &old),
                });
            }
            content = content.replacen(&old, &new, 1);
        }

        fs::write(&path, content).map_err(|err| ToolError::io(&path, err))?;

        Ok(ToolResult::success(
            ToolName::MultiEdit,
            CappedOutput::complete(format!(
                "applied {} edit(s) to {}",
                edits.len(),
                path.display()
            )),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "path": self.relative_path(&path),
                "edits": edits.len(),
            }))
        })
    }

    pub fn apply_patch_structured(&self, patch: &StructuredPatch) -> Result<ToolResult, ToolError> {
        if patch.operations.is_empty() {
            return Err(ToolError::EmptyPatch);
        }

        let mut changed = 0usize;
        for operation in &patch.operations {
            match operation {
                StructuredPatchOperation::Edit {
                    path,
                    old,
                    new,
                    expected_hash,
                } => {
                    self.edit_file(path, old, new, expected_hash.as_deref())?;
                    changed += 1;
                }
                StructuredPatchOperation::MultiEdit {
                    path,
                    edits,
                    expected_hash,
                } => {
                    self.multi_edit(path, edits, expected_hash.as_deref())?;
                    changed += 1;
                }
                StructuredPatchOperation::Rename {
                    from,
                    to,
                    expected_hash,
                } => {
                    self.rename_file(from, to, expected_hash.as_deref())?;
                    changed += 1;
                }
            }
        }

        Ok(ToolResult::success(
            ToolName::ApplyPatchStructured,
            CappedOutput::complete(format!("applied {changed} structured patch operation(s)")),
        ))
        .map(|result| result.with_metadata(json!({ "operations": changed })))
    }

    pub fn apply_line_patch(&self, patch: &LinePatch) -> Result<ToolResult, ToolError> {
        if patch.operations.is_empty() {
            return Err(ToolError::EmptyPatch);
        }

        let mut changed = 0usize;
        let mut pending_updates: HashMap<PathBuf, PendingFileLineUpdates> = HashMap::new();
        let mut immediate_operations: Vec<&LinePatchOperation> = Vec::new();

        for operation in &patch.operations {
            match operation {
                LinePatchOperation::Update {
                    path,
                    start_line,
                    end_line,
                    old,
                    new,
                    expected_hash,
                } => {
                    let resolved_path = self.resolve_existing_path(path)?;
                    let entry = pending_updates.entry(resolved_path).or_insert_with(|| {
                        PendingFileLineUpdates {
                            display_path: path.clone(),
                            expected_hash: expected_hash.clone(),
                            updates: Vec::new(),
                        }
                    });
                    if entry.expected_hash.is_none() {
                        entry.expected_hash = expected_hash.clone();
                    } else if expected_hash.is_some() && entry.expected_hash != *expected_hash {
                        return Err(ToolError::InvalidPatch(format!(
                            "conflicting expected_hash values for {}",
                            path.display()
                        )));
                    }
                    entry.updates.push(PendingLineUpdate {
                        start_line: *start_line,
                        end_line: *end_line,
                        old: old.clone(),
                        new: new.clone(),
                    });
                }
                LinePatchOperation::AddFile { .. } | LinePatchOperation::DeleteFile { .. } => {
                    immediate_operations.push(operation);
                }
            }
        }

        for operation in immediate_operations {
            match operation {
                LinePatchOperation::AddFile { path, content } => {
                    let resolved_path = self.resolve_write_path(path)?;
                    if pending_updates.contains_key(&resolved_path) {
                        return Err(ToolError::InvalidPatch(format!(
                            "cannot add and update the same file in one line-aware patch: {}",
                            path.display()
                        )));
                    }
                    self.write_file(path, content)?;
                    changed += 1;
                }
                LinePatchOperation::DeleteFile {
                    path,
                    expected_hash,
                } => {
                    let resolved_path = self.resolve_existing_path(path)?;
                    if pending_updates.contains_key(&resolved_path) {
                        return Err(ToolError::InvalidPatch(format!(
                            "cannot delete and update the same file in one line-aware patch: {}",
                            path.display()
                        )));
                    }
                    self.read_text_file(&resolved_path, expected_hash.as_deref())?;
                    fs::remove_file(&resolved_path)
                        .map_err(|err| ToolError::io(&resolved_path, err))?;
                    changed += 1;
                }
                LinePatchOperation::Update { .. } => {}
            }
        }

        for (resolved_path, pending) in pending_updates {
            let update_count = pending.updates.len();
            self.apply_line_updates(
                &resolved_path,
                &pending.display_path,
                &pending.updates,
                pending.expected_hash.as_deref(),
            )?;
            changed += update_count;
        }

        Ok(ToolResult::success(
            ToolName::ApplyPatch,
            CappedOutput::complete(format!("applied {changed} line-aware patch operation(s)")),
        ))
        .map(|result| result.with_metadata(json!({ "operations": changed })))
    }

    pub fn apply_patch_freeform(&self, patch: impl AsRef<str>) -> Result<ToolResult, ToolError> {
        let patch = patch.as_ref();
        if patch.trim_start().starts_with("*** Begin Patch") {
            return self.apply_begin_patch(patch);
        }

        let files = parse_unified_patch(patch)?;
        if files.is_empty() {
            return Err(ToolError::EmptyPatch);
        }

        for file in &files {
            self.apply_unified_file_patch(file)?;
        }

        Ok(ToolResult::success(
            ToolName::ApplyPatchFreeform,
            CappedOutput::complete(format!("applied unified patch to {} file(s)", files.len())),
        ))
        .map(|result| result.with_metadata(json!({ "files": files.len() })))
    }

    pub fn apply_patch(&self, patch: impl AsRef<str>) -> Result<ToolResult, ToolError> {
        let mut result = self.apply_patch_freeform(patch)?;
        result.name = ToolName::ApplyPatch;
        result.title = ToolName::ApplyPatch.title().to_string();
        Ok(result)
    }

    fn apply_begin_patch(&self, patch: &str) -> Result<ToolResult, ToolError> {
        let operations = parse_begin_patch(patch)?;
        if operations.is_empty() {
            return Err(ToolError::EmptyPatch);
        }

        let mut changed = 0usize;
        for operation in operations {
            match operation {
                BeginPatchOperation::Update { path, edits } => {
                    self.multi_edit(path, &edits, None)?;
                    changed += 1;
                }
                BeginPatchOperation::Add { path, content } => {
                    self.write_file(path, content)?;
                    changed += 1;
                }
                BeginPatchOperation::Delete { path } => {
                    let path = self.resolve_existing_path(&path)?;
                    fs::remove_file(&path).map_err(|err| ToolError::io(&path, err))?;
                    changed += 1;
                }
            }
        }

        Ok(ToolResult::success(
            ToolName::ApplyPatchFreeform,
            CappedOutput::complete(format!("applied begin patch to {changed} file(s)")),
        ))
        .map(|result| result.with_metadata(json!({ "files": changed })))
    }

    fn apply_line_updates(
        &self,
        path: &Path,
        display_path: &Path,
        updates: &[PendingLineUpdate],
        expected_hash: Option<&str>,
    ) -> Result<(), ToolError> {
        if updates.is_empty() {
            return Ok(());
        }

        let text_file = self.read_text_file(path, expected_hash)?;
        let newline = text_file.newline;
        let mut lines = split_lines_lossless(&text_file.text);
        let original_len = lines.len();
        let mut normalized = Vec::with_capacity(updates.len());

        for update in updates {
            let start_line = update.start_line;
            let end_line = update.end_line;
            if start_line == 0 {
                return Err(ToolError::InvalidPatch(format!(
                    "line-aware update start_line must be 1-based for {}",
                    display_path.display()
                )));
            }
            if end_line < start_line {
                return Err(ToolError::InvalidPatch(format!(
                    "line-aware update end_line must be greater than or equal to start_line for {}",
                    display_path.display()
                )));
            }

            let index = start_line - 1;
            let old = normalize_edit_text(&update.old, newline);
            let new = normalize_edit_text(&update.new, newline);
            let old_lines = split_lines_lossless(&old);
            let new_lines = split_lines_lossless(&new);

            if old.is_empty() {
                if end_line != start_line {
                    return Err(ToolError::InvalidPatch(format!(
                        "line-aware insert must set end_line equal to start_line for {}",
                        display_path.display()
                    )));
                }
                if index > original_len {
                    return Err(ToolError::PatchApplyFailed {
                        path: path.to_path_buf(),
                        message: format!(
                            "line-aware insert start_line {start_line} is beyond end of file; file has {original_len} line(s)"
                        ),
                    });
                }
                normalized.push(NormalizedLineUpdate {
                    start_index: index,
                    end_index: index,
                    new_lines,
                    insertion: true,
                });
                continue;
            }

            let expected_line_count = end_line - start_line + 1;
            if old_lines.len() != expected_line_count {
                return Err(ToolError::InvalidPatch(format!(
                    "line-aware update for {} has start_line {start_line} and end_line {end_line}, but old text spans {} line(s)",
                    display_path.display(),
                    old_lines.len()
                )));
            }

            let end_index = end_line;
            if index >= original_len
                || end_index > original_len
                || lines[index..end_index] != old_lines
            {
                return Err(ToolError::PatchApplyFailed {
                    path: path.to_path_buf(),
                    message: format_line_patch_mismatch(&lines, start_line, end_line, &old),
                });
            }

            normalized.push(NormalizedLineUpdate {
                start_index: index,
                end_index,
                new_lines,
                insertion: false,
            });
        }

        normalized.sort_by(|left, right| {
            right
                .start_index
                .cmp(&left.start_index)
                .then_with(|| right.end_index.cmp(&left.end_index))
        });

        for pair in normalized.windows(2) {
            let later = &pair[0];
            let earlier = &pair[1];
            if earlier.end_index > later.start_index {
                return Err(ToolError::InvalidPatch(format!(
                    "overlapping line-aware updates for {} around lines {} and {}",
                    display_path.display(),
                    earlier.start_index + 1,
                    later.start_index + 1
                )));
            }
            if earlier.insertion && later.insertion && earlier.start_index == later.start_index {
                return Err(ToolError::InvalidPatch(format!(
                    "multiple insertions at line {} in {} are ambiguous; combine them into one operation",
                    earlier.start_index + 1,
                    display_path.display()
                )));
            }
        }

        for update in normalized {
            lines.splice(update.start_index..update.end_index, update.new_lines);
        }

        fs::write(path, lines.concat()).map_err(|err| ToolError::io(path, err))
    }

    pub fn grep(&self, pattern: impl AsRef<str>) -> Result<ToolResult, ToolError> {
        let pattern = pattern.as_ref();
        let mut output = String::new();
        let mut matches = 0;

        self.grep_dir(&self.workspace_root, pattern, &mut output, &mut matches)?;

        Ok(ToolResult::success(
            ToolName::Grep,
            cap_output(output, self.output_limit_bytes),
        ))
        .map(|result| result.with_metadata(json!({ "pattern": pattern, "matches": matches })))
    }

    pub fn glob(
        &self,
        pattern: impl AsRef<str>,
        path: Option<impl AsRef<Path>>,
    ) -> Result<ToolResult, ToolError> {
        let pattern = pattern.as_ref();
        if pattern.trim().is_empty() {
            return Err(ToolError::InvalidPattern(pattern.to_string()));
        }
        let root = match path {
            Some(path) => self.resolve_existing_path(path.as_ref())?,
            None => self.workspace_root.clone(),
        };
        let metadata = fs::metadata(&root).map_err(|err| ToolError::io(&root, err))?;
        let search_root = if metadata.is_dir() {
            root
        } else {
            root.parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.workspace_root.clone())
        };

        let mut output = Vec::new();
        self.glob_dir(&search_root, pattern, &mut output)?;
        output.sort();
        let matches = output.len();
        if output.len() >= DEFAULT_GLOB_MATCH_LIMIT {
            output.push(format!(
                "(Results truncated: showing first {DEFAULT_GLOB_MATCH_LIMIT} matches.)"
            ));
        }

        Ok(ToolResult::success(
            ToolName::Glob,
            CappedOutput::complete(if output.is_empty() {
                "No files found".to_string()
            } else {
                output.join("\n")
            }),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "pattern": pattern,
                "path": self.relative_path(&search_root),
                "matches": matches,
            }))
        })
    }

    pub fn web_fetch(&self, url: impl AsRef<str>) -> Result<ToolResult, ToolError> {
        let url = url.as_ref().trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ToolError::InvalidUrl(url.to_string()));
        }
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(WEB_FETCH_TIMEOUT_SECS))
            .user_agent("inductor/0.1")
            .build()
            .map_err(|err| ToolError::WebFetchFailed {
                url: url.to_string(),
                message: err.to_string(),
            })?;
        let response = client
            .get(url)
            .send()
            .map_err(|err| ToolError::WebFetchFailed {
                url: url.to_string(),
                message: err.to_string(),
            })?;
        let status = response.status();
        let text = response.text().map_err(|err| ToolError::WebFetchFailed {
            url: url.to_string(),
            message: err.to_string(),
        })?;

        Ok(ToolResult::success(
            ToolName::WebFetch,
            cap_output(
                format!("status: {status}\nurl: {url}\n\n{text}"),
                self.output_limit_bytes,
            ),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "url": url,
                "status": status.as_u16(),
            }))
        })
    }

    pub fn todo_write(&self, todos: &[TodoItem]) -> Result<ToolResult, ToolError> {
        let output = if todos.is_empty() {
            "Todo list cleared".to_string()
        } else {
            todos
                .iter()
                .enumerate()
                .map(|(index, todo)| format!("{}. [{}] {}", index + 1, todo.status, todo.content))
                .collect::<Vec<_>>()
                .join("\n")
        };

        Ok(ToolResult::success(
            ToolName::TodoWrite,
            CappedOutput::complete(output),
        ))
        .map(|result| {
            result.with_metadata(json!({
                "items": todos.len(),
                "pending": todos.iter().filter(|todo| todo.status == "pending").count(),
                "in_progress": todos.iter().filter(|todo| todo.status == "in_progress").count(),
                "completed": todos.iter().filter(|todo| todo.status == "completed").count(),
            }))
        })
    }

    pub fn bash(&self, command: impl AsRef<str>) -> Result<ToolResult, ToolError> {
        let command = command.as_ref();
        let (program, args) = self.sandbox.wrap_shell_command(command);
        let output = Command::new(&program)
            .args(&args)
            .current_dir(&self.workspace_root)
            .output()
            .map_err(|err| ToolError::CommandSpawnFailed {
                command: command.to_string(),
                source: err,
            })?;

        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        combined.push_str(&String::from_utf8_lossy(&output.stderr));

        Ok(ToolResult {
            name: ToolName::Bash,
            title: ToolName::Bash.title().to_string(),
            metadata: json!({ "command": command }),
            output: cap_output(combined, self.output_limit_bytes).text,
            exit_code: output.status.code(),
            truncated: false,
        }
        .with_output_cap(self.output_limit_bytes))
    }

    pub async fn bash_cancellable(
        &self,
        command: impl AsRef<str>,
        cancel: CancellationToken,
    ) -> Result<ToolResult, ToolError> {
        self.bash_cancellable_until(command, cancel, None).await
    }

    pub async fn bash_cancellable_until(
        &self,
        command: impl AsRef<str>,
        cancel: CancellationToken,
        checkpoint_after: Option<Duration>,
    ) -> Result<ToolResult, ToolError> {
        let command = command.as_ref();
        let (program, args) = self.sandbox.wrap_shell_command(command);
        let mut command_builder = TokioCommand::new(&program);
        command_builder
            .args(&args)
            .current_dir(&self.workspace_root)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command_builder.process_group(0);
        let mut child = command_builder
            .spawn()
            .map_err(|err| ToolError::CommandSpawnFailed {
                command: command.to_string(),
                source: err,
            })?;
        let child_id = child.id();

        let stdout_buffer = Arc::new(Mutex::new(Vec::new()));
        let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
        let stdout_task = {
            let buffer = stdout_buffer.clone();
            let stdout = child.stdout.take();
            tokio::spawn(async move {
                if let Some(mut stdout) = stdout {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match stdout.read(&mut chunk).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if let Ok(mut guard) = buffer.lock() {
                                    guard.extend_from_slice(&chunk[..n]);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            })
        };
        let stderr_task = {
            let buffer = stderr_buffer.clone();
            let stderr = child.stderr.take();
            tokio::spawn(async move {
                if let Some(mut stderr) = stderr {
                    let mut chunk = [0u8; 8192];
                    loop {
                        match stderr.read(&mut chunk).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if let Ok(mut guard) = buffer.lock() {
                                    guard.extend_from_slice(&chunk[..n]);
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
            })
        };

        enum BashOutcome {
            Completed(std::process::ExitStatus),
            Cancelled,
            Checkpoint,
        }

        let checkpoint = async {
            if let Some(duration) = checkpoint_after {
                tokio::time::sleep(duration).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(checkpoint);
        let outcome = tokio::select! {
            output = child.wait() => BashOutcome::Completed(output.map_err(|err| ToolError::CommandSpawnFailed {
                command: command.to_string(),
                source: err,
            })?),
            _ = cancel.cancelled() => {
                kill_background_process(child_id);
                let _ = child.start_kill();
                let _ = child.wait().await;
                BashOutcome::Cancelled
            }
            _ = &mut checkpoint => {
                BashOutcome::Checkpoint
            }
        };

        let status = match outcome {
            BashOutcome::Completed(status) => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                status
            }
            BashOutcome::Cancelled => {
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Err(ToolError::CommandCancelled {
                    command: command.to_string(),
                });
            }
            BashOutcome::Checkpoint => {
                let command_id = self.next_background_command_id();
                let kill = CancellationToken::new();
                let notify = Arc::new(Notify::new());
                let status_state = Arc::new(Mutex::new(BackgroundCommandStatus::Running));
                let handle = BackgroundCommand {
                    id: command_id.clone(),
                    command: command.to_string(),
                    started_at: Instant::now()
                        .checked_sub(checkpoint_after.unwrap_or_default())
                        .unwrap_or_else(Instant::now),
                    stdout_buffer: stdout_buffer.clone(),
                    stderr_buffer: stderr_buffer.clone(),
                    status: status_state.clone(),
                    kill: kill.clone(),
                    notify: notify.clone(),
                };
                self.store_background_command(handle)?;
                tokio::spawn(async move {
                    let outcome = tokio::select! {
                        output = child.wait() => BackgroundCommandStatus::Completed {
                            exit_code: output.ok().and_then(|status| status.code()),
                        },
                        _ = kill.cancelled() => {
                            kill_background_process(child_id);
                            let _ = child.start_kill();
                            BackgroundCommandStatus::Killed {
                                exit_code: child.wait().await.ok().and_then(|status| status.code()),
                            }
                        }
                    };
                    let _ = stdout_task.await;
                    let _ = stderr_task.await;
                    if let Ok(mut status) = status_state.lock() {
                        *status = outcome;
                    }
                    notify.notify_waiters();
                });
                let stdout = stdout_buffer
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default();
                let stderr = stderr_buffer
                    .lock()
                    .map(|guard| guard.clone())
                    .unwrap_or_default();
                let mut combined = String::new();
                combined.push_str(&String::from_utf8_lossy(&stdout));
                combined.push_str(&String::from_utf8_lossy(&stderr));
                let elapsed = checkpoint_after.unwrap_or_default();
                return Err(ToolError::CommandCheckpoint {
                    command_id,
                    command: command.to_string(),
                    elapsed,
                    output: cap_output(combined, self.output_limit_bytes).text,
                });
            }
        };

        let stdout = stdout_buffer
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let stderr = stderr_buffer
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&stdout));
        combined.push_str(&String::from_utf8_lossy(&stderr));

        Ok(ToolResult {
            name: ToolName::Bash,
            title: ToolName::Bash.title().to_string(),
            metadata: json!({ "command": command }),
            output: cap_output(combined, self.output_limit_bytes).text,
            exit_code: status.code(),
            truncated: false,
        }
        .with_output_cap(self.output_limit_bytes))
    }

    pub async fn bash_wait(
        &self,
        command_id: impl AsRef<str>,
        timeout: Duration,
    ) -> Result<ToolResult, ToolError> {
        let command_id = command_id.as_ref();
        let handle = self.background_command(command_id)?;

        if matches!(handle.status(), BackgroundCommandStatus::Running) {
            let notified = handle.notify.notified();
            if matches!(handle.status(), BackgroundCommandStatus::Running) {
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(timeout) => {}
                }
            }
        }

        match handle.status() {
            BackgroundCommandStatus::Running => {
                let output = cap_output(handle.combined_output(), self.output_limit_bytes);
                Ok(ToolResult {
                    name: ToolName::BashWait,
                    title: ToolName::BashWait.title().to_string(),
                    metadata: json!({
                        "command_id": handle.id,
                        "command": handle.command,
                        "running": true,
                        "elapsed_secs": handle.started_at.elapsed().as_secs(),
                    }),
                    output: format!(
                        "command_id {} is still running after waiting {}. The command is NOT killed. Call bash_wait again to keep waiting for final output, or bash_kill to stop it.\n{}",
                        command_id,
                        format_duration(timeout),
                        format_partial_output(&output.text),
                    ),
                    exit_code: None,
                    truncated: output.truncated,
                }
                .with_output_cap(self.output_limit_bytes))
            }
            BackgroundCommandStatus::Completed { exit_code } => {
                self.remove_background_command(command_id);
                self.finished_background_result(&handle, "completed", exit_code, ToolName::BashWait)
            }
            BackgroundCommandStatus::Killed { exit_code } => {
                self.remove_background_command(command_id);
                self.finished_background_result(&handle, "killed", exit_code, ToolName::BashWait)
            }
        }
    }

    pub async fn bash_kill(&self, command_id: impl AsRef<str>) -> Result<ToolResult, ToolError> {
        let command_id = command_id.as_ref();
        let handle = self.background_command(command_id)?;

        if matches!(handle.status(), BackgroundCommandStatus::Running) {
            handle.kill.cancel();
            let notified = handle.notify.notified();
            if matches!(handle.status(), BackgroundCommandStatus::Running) {
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {
                        return Err(ToolError::CommandKillTimedOut {
                            command_id: command_id.to_string(),
                        });
                    }
                }
            }
        }

        let status = handle.status();
        self.remove_background_command(command_id);
        match status {
            BackgroundCommandStatus::Running => Err(ToolError::CommandKillTimedOut {
                command_id: command_id.to_string(),
            }),
            BackgroundCommandStatus::Completed { exit_code } => self.finished_background_result(
                &handle,
                "completed before kill",
                exit_code,
                ToolName::BashKill,
            ),
            BackgroundCommandStatus::Killed { exit_code } => {
                self.finished_background_result(&handle, "killed", exit_code, ToolName::BashKill)
            }
        }
    }

    fn next_background_command_id(&self) -> String {
        let id = self
            .background_command_counter
            .fetch_add(1, Ordering::Relaxed);
        format!("bash-{id}")
    }

    fn store_background_command(&self, command: BackgroundCommand) -> Result<(), ToolError> {
        let mut commands = self
            .background_commands
            .lock()
            .map_err(|_| ToolError::BackgroundCommandRegistryPoisoned)?;
        commands.insert(command.id.clone(), command);
        Ok(())
    }

    fn background_command(&self, command_id: &str) -> Result<BackgroundCommand, ToolError> {
        let commands = self
            .background_commands
            .lock()
            .map_err(|_| ToolError::BackgroundCommandRegistryPoisoned)?;
        commands
            .get(command_id)
            .cloned()
            .ok_or_else(|| ToolError::UnknownBackgroundCommand {
                command_id: command_id.to_string(),
            })
    }

    fn remove_background_command(&self, command_id: &str) {
        if let Ok(mut commands) = self.background_commands.lock() {
            commands.remove(command_id);
        }
    }

    fn finished_background_result(
        &self,
        handle: &BackgroundCommand,
        status: &str,
        exit_code: Option<i32>,
        name: ToolName,
    ) -> Result<ToolResult, ToolError> {
        let output = cap_output(handle.combined_output(), self.output_limit_bytes);
        Ok(ToolResult {
            name,
            title: name.title().to_string(),
            metadata: json!({
                "command_id": handle.id,
                "command": handle.command,
                "status": status,
                "elapsed_secs": handle.started_at.elapsed().as_secs(),
            }),
            output: format!(
                "command_id {} {status}. Final output:\n{}",
                handle.id,
                if output.text.is_empty() {
                    "<no output>"
                } else {
                    &output.text
                },
            ),
            exit_code,
            truncated: output.truncated,
        }
        .with_output_cap(self.output_limit_bytes))
    }

    fn grep_dir(
        &self,
        dir: &Path,
        pattern: &str,
        output: &mut String,
        matches: &mut usize,
    ) -> Result<(), ToolError> {
        if *matches >= self.grep_match_limit {
            return Ok(());
        }

        let entries = fs::read_dir(dir).map_err(|err| ToolError::io(dir, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| ToolError::io(dir, err))?;
            let path = entry.path();
            let metadata = entry
                .metadata()
                .map_err(|err| ToolError::io(&entry.path(), err))?;

            if metadata.is_dir() {
                self.grep_dir(&path, pattern, output, matches)?;
            } else if metadata.is_file() {
                self.grep_file(&path, pattern, output, matches)?;
            }

            if *matches >= self.grep_match_limit {
                break;
            }
        }

        Ok(())
    }

    fn glob_dir(
        &self,
        dir: &Path,
        pattern: &str,
        output: &mut Vec<String>,
    ) -> Result<(), ToolError> {
        if output.len() >= DEFAULT_GLOB_MATCH_LIMIT {
            return Ok(());
        }

        let entries = fs::read_dir(dir).map_err(|err| ToolError::io(dir, err))?;
        for entry in entries {
            let entry = entry.map_err(|err| ToolError::io(dir, err))?;
            let path = entry.path();
            let metadata = entry.metadata().map_err(|err| ToolError::io(&path, err))?;
            if metadata.is_dir() {
                self.glob_dir(&path, pattern, output)?;
            } else if metadata.is_file() {
                let relative = path.strip_prefix(&self.workspace_root).unwrap_or(&path);
                let relative = relative.to_string_lossy().replace('\\', "/");
                if wildcard_match(pattern, &relative) {
                    output.push(relative);
                }
            }
            if output.len() >= DEFAULT_GLOB_MATCH_LIMIT {
                break;
            }
        }

        Ok(())
    }

    fn grep_file(
        &self,
        path: &Path,
        pattern: &str,
        output: &mut String,
        matches: &mut usize,
    ) -> Result<(), ToolError> {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => return Ok(()),
            Err(error) => return Err(ToolError::io(path, error)),
        };

        for (line_index, line) in content.lines().enumerate() {
            if !line.contains(pattern) {
                continue;
            }

            let relative = path.strip_prefix(&self.workspace_root).unwrap_or(path);
            output.push_str(&format!(
                "{}:{}:{line}\n",
                relative.display(),
                line_index + 1
            ));
            *matches += 1;

            if *matches >= self.grep_match_limit {
                break;
            }
        }

        Ok(())
    }

    fn resolve_existing_path(&self, input: &Path) -> Result<PathBuf, ToolError> {
        if self.allow_outside_paths {
            let path = self.resolve_any_path(input);
            return path.canonicalize().map_err(|err| ToolError::io(&path, err));
        }

        let path = self.resolve_workspace_relative_path(input)?;
        let canonical = path
            .canonicalize()
            .map_err(|err| ToolError::io(&path, err))?;

        if !canonical.starts_with(&self.workspace_root) {
            return Err(ToolError::PathEscapesWorkspace {
                path: input.to_path_buf(),
                workspace_root: self.workspace_root.clone(),
            });
        }

        Ok(canonical)
    }

    fn resolve_write_path(&self, input: &Path) -> Result<PathBuf, ToolError> {
        if self.allow_outside_paths {
            let path = self.resolve_any_path(input);
            if path.exists() {
                return path.canonicalize().map_err(|err| ToolError::io(&path, err));
            }
            let parent = path.parent().ok_or_else(|| ToolError::InvalidPath {
                path: input.to_path_buf(),
            })?;
            fs::create_dir_all(parent).map_err(|err| ToolError::io(parent, err))?;
            let canonical_parent = parent
                .canonicalize()
                .map_err(|err| ToolError::io(parent, err))?;
            let file_name = path.file_name().ok_or_else(|| ToolError::InvalidPath {
                path: input.to_path_buf(),
            })?;
            return Ok(canonical_parent.join(file_name));
        }

        let path = self.resolve_workspace_relative_path(input)?;

        if path.exists() {
            return self.resolve_existing_path(input);
        }

        let parent = path.parent().ok_or_else(|| ToolError::InvalidPath {
            path: input.to_path_buf(),
        })?;

        // The normalized path is already confirmed `..`-free and workspace-
        // relative, so the parent is inside the workspace. Create it (mkdir -p
        // style) so writes to new nested paths like `a/b/c.txt` succeed instead
        // of failing with "No such file or directory".
        fs::create_dir_all(parent).map_err(|err| ToolError::io(parent, err))?;

        let canonical_parent = parent
            .canonicalize()
            .map_err(|err| ToolError::io(parent, err))?;

        if !canonical_parent.starts_with(&self.workspace_root) {
            return Err(ToolError::PathEscapesWorkspace {
                path: input.to_path_buf(),
                workspace_root: self.workspace_root.clone(),
            });
        }

        Ok(path)
    }

    fn resolve_any_path(&self, input: &Path) -> PathBuf {
        if input.is_absolute() {
            input.to_path_buf()
        } else {
            self.workspace_root.join(input)
        }
    }

    fn resolve_workspace_relative_path(&self, input: &Path) -> Result<PathBuf, ToolError> {
        if input.is_absolute() {
            let anchor = if input.exists() {
                input.to_path_buf()
            } else {
                input
                    .parent()
                    .ok_or_else(|| ToolError::InvalidPath {
                        path: input.to_path_buf(),
                    })?
                    .to_path_buf()
            };
            let canonical_anchor = anchor
                .canonicalize()
                .map_err(|err| ToolError::io(&anchor, err))?;
            if !canonical_anchor.starts_with(&self.workspace_root) {
                return Err(ToolError::AbsolutePath {
                    path: input.to_path_buf(),
                });
            }
            return Ok(input.to_path_buf());
        }

        let mut normalized = PathBuf::new();
        for component in input.components() {
            match component {
                Component::Normal(part) => normalized.push(part),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(ToolError::PathEscapesWorkspace {
                            path: input.to_path_buf(),
                            workspace_root: self.workspace_root.clone(),
                        });
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(ToolError::InvalidPath {
                        path: input.to_path_buf(),
                    });
                }
            }
        }

        Ok(self.workspace_root.join(normalized))
    }

    fn read_text_file(
        &self,
        path: &Path,
        expected_hash: Option<&str>,
    ) -> Result<TextFile, ToolError> {
        let bytes = fs::read(path).map_err(|err| ToolError::io(path, err))?;
        if bytes.iter().any(|byte| *byte == 0) {
            return Err(ToolError::BinaryFile {
                path: path.to_path_buf(),
            });
        }

        let actual_hash = sha256_hex(&bytes);
        if let Some(expected_hash) = expected_hash.filter(|hash| !hash.trim().is_empty()) {
            if !hashes_equal(expected_hash, &actual_hash) {
                return Err(ToolError::StaleFile {
                    path: path.to_path_buf(),
                    expected_hash: expected_hash.to_string(),
                    actual_hash,
                });
            }
        }

        let text = String::from_utf8(bytes).map_err(|_| ToolError::BinaryFile {
            path: path.to_path_buf(),
        })?;
        let newline = if text.contains("\r\n") {
            NewlineStyle::Crlf
        } else {
            NewlineStyle::Lf
        };

        Ok(TextFile { text, newline })
    }

    fn rename_file(
        &self,
        from: impl AsRef<Path>,
        to: impl AsRef<Path>,
        expected_hash: Option<&str>,
    ) -> Result<(), ToolError> {
        let from_input = from.as_ref();
        let to_input = to.as_ref();
        let from = self.resolve_existing_path(from_input)?;
        self.read_text_file(&from, expected_hash)?;

        let to = self.resolve_write_path(to_input)?;
        if to.exists() {
            return Err(ToolError::RenameTargetExists { path: to });
        }

        fs::rename(&from, &to).map_err(|err| ToolError::io(&from, err))
    }

    fn apply_unified_file_patch(&self, patch: &UnifiedFilePatch) -> Result<(), ToolError> {
        let path = self.resolve_existing_path(&patch.path)?;
        let text_file = self.read_text_file(&path, None)?;
        let newline = text_file.newline;
        let mut lines = split_lines_lossless(&text_file.text);
        let mut offset: isize = 0;

        for hunk in &patch.hunks {
            let start = (hunk.old_start as isize - 1 + offset) as usize;
            if start > lines.len() {
                return Err(ToolError::PatchApplyFailed {
                    path: path.clone(),
                    message: "hunk starts beyond end of file".to_string(),
                });
            }

            let old_lines = hunk
                .lines
                .iter()
                .filter_map(|line| match line.kind {
                    HunkLineKind::Context | HunkLineKind::Remove => {
                        Some(normalize_edit_text(&line.text, newline))
                    }
                    HunkLineKind::Add => None,
                })
                .collect::<Vec<_>>();
            let new_lines = hunk
                .lines
                .iter()
                .filter_map(|line| match line.kind {
                    HunkLineKind::Context | HunkLineKind::Add => {
                        Some(normalize_edit_text(&line.text, newline))
                    }
                    HunkLineKind::Remove => None,
                })
                .collect::<Vec<_>>();

            let end = start + old_lines.len();
            if end > lines.len() || lines[start..end] != old_lines {
                if already_applied_hunk(&lines, &new_lines, start) {
                    offset += new_lines.len() as isize - old_lines.len() as isize;
                    continue;
                }
                return Err(ToolError::PatchApplyFailed {
                    path: path.clone(),
                    message: format!("hunk did not match at line {}", hunk.old_start),
                });
            }

            lines.splice(start..end, new_lines.clone());
            offset += new_lines.len() as isize - old_lines.len() as isize;
        }

        fs::write(&path, lines.concat()).map_err(|err| ToolError::io(&path, err))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub name: ToolName,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub output: String,
    pub exit_code: Option<i32>,
    pub truncated: bool,
}

impl ToolResult {
    fn success(name: ToolName, output: CappedOutput) -> Self {
        Self {
            name,
            title: name.title().to_string(),
            metadata: serde_json::Value::Null,
            output: output.text,
            exit_code: Some(0),
            truncated: output.truncated,
        }
    }

    fn with_output_cap(mut self, limit: usize) -> Self {
        let capped = cap_output(self.output, limit);
        self.output = capped.text;
        self.truncated = capped.truncated;
        self
    }

    fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CappedOutput {
    text: String,
    truncated: bool,
}

impl CappedOutput {
    fn complete(text: String) -> Self {
        Self {
            text,
            truncated: false,
        }
    }
}

#[derive(Debug)]
pub enum ToolError {
    WorkspaceIo {
        path: PathBuf,
        source: io::Error,
    },
    WorkspaceNotDirectory {
        path: PathBuf,
    },
    AbsolutePath {
        path: PathBuf,
    },
    InvalidPath {
        path: PathBuf,
    },
    MemoryUnavailable,
    PathEscapesWorkspace {
        path: PathBuf,
        workspace_root: PathBuf,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    CommandSpawnFailed {
        command: String,
        source: io::Error,
    },
    CommandCancelled {
        command: String,
    },
    CommandCheckpoint {
        command_id: String,
        command: String,
        elapsed: Duration,
        output: String,
    },
    UnknownBackgroundCommand {
        command_id: String,
    },
    BackgroundCommandRegistryPoisoned,
    CommandKillTimedOut {
        command_id: String,
    },
    BinaryFile {
        path: PathBuf,
    },
    StaleFile {
        path: PathBuf,
        expected_hash: String,
        actual_hash: String,
    },
    EditTargetNotFound {
        path: PathBuf,
        old: String,
        hint: Option<String>,
    },
    EditTargetNotUnique {
        path: PathBuf,
        old: String,
        count: usize,
        hint: Option<String>,
    },
    EmptyEdit,
    EmptyPatch,
    // Keep the variant for compatibility with older callers; todo_write now
    // accepts an empty list to clear stale session todos.
    EmptyTodoList,
    InvalidPattern(String),
    InvalidUrl(String),
    NotDirectory {
        path: PathBuf,
    },
    WebFetchFailed {
        url: String,
        message: String,
    },
    RenameTargetExists {
        path: PathBuf,
    },
    InvalidPatch(String),
    PatchApplyFailed {
        path: PathBuf,
        message: String,
    },
}

impl ToolError {
    fn workspace_io(path: &Path, source: io::Error) -> Self {
        Self::WorkspaceIo {
            path: path.to_path_buf(),
            source,
        }
    }

    fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }

    fn memory_unavailable() -> Self {
        Self::MemoryUnavailable
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceIo { path, source } => {
                write!(
                    f,
                    "failed to inspect workspace {}: {source}",
                    path.display()
                )
            }
            Self::WorkspaceNotDirectory { path } => {
                write!(f, "workspace is not a directory: {}", path.display())
            }
            Self::AbsolutePath { path } => {
                write!(
                    f,
                    "tool paths must be workspace-relative: {}",
                    path.display()
                )
            }
            Self::InvalidPath { path } => write!(f, "invalid tool path: {}", path.display()),
            Self::MemoryUnavailable => write!(f, "repo memory is not configured for this run"),
            Self::PathEscapesWorkspace {
                path,
                workspace_root,
            } => write!(
                f,
                "tool path {} escapes workspace {}",
                path.display(),
                workspace_root.display()
            ),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::CommandSpawnFailed { command, source } => {
                write!(f, "failed to run command {command:?}: {source}")
            }
            Self::CommandCancelled { command } => write!(f, "command cancelled: {command:?}"),
            Self::CommandCheckpoint {
                command_id,
                command,
                elapsed,
                output,
            } => {
                write!(
                    f,
                    "command {command:?} has been running for {} and reached the tool checkpoint. command_id: {command_id}. The command is still running in the background. Call bash_wait with this command_id to keep waiting and receive the final output when it finishes, or call bash_kill with this command_id to stop it. {}",
                    format_duration(*elapsed),
                    format_partial_output(output)
                )
            }
            Self::UnknownBackgroundCommand { command_id } => {
                write!(f, "unknown background command_id: {command_id}")
            }
            Self::BackgroundCommandRegistryPoisoned => {
                write!(f, "background command registry is unavailable")
            }
            Self::CommandKillTimedOut { command_id } => {
                write!(
                    f,
                    "timed out while stopping background command_id: {command_id}"
                )
            }
            Self::BinaryFile { path } => {
                write!(f, "refusing to edit binary file: {}", path.display())
            }
            Self::StaleFile {
                path,
                expected_hash,
                actual_hash,
            } => write!(
                f,
                "refusing stale edit for {}: expected hash {expected_hash}, current hash {actual_hash}",
                path.display()
            ),
            Self::EditTargetNotFound { path, old, hint } => write!(
                f,
                "edit target was not found in {}. Target preview: {}{}",
                path.display(),
                edit_preview(old),
                format_edit_hint(hint)
            ),
            Self::EditTargetNotUnique {
                path,
                old,
                count,
                hint,
            } => write!(
                f,
                "edit target is not unique in {}: matched {count} times. Use a larger unique old string. Target preview: {}{}",
                path.display(),
                edit_preview(old),
                format_edit_hint(hint)
            ),
            Self::EmptyEdit => f.write_str("edit list is empty"),
            Self::EmptyPatch => f.write_str("patch contains no operations"),
            Self::EmptyTodoList => f.write_str("todo list is empty"),
            Self::InvalidPattern(pattern) => write!(f, "invalid glob pattern: {pattern}"),
            Self::InvalidUrl(url) => {
                write!(
                    f,
                    "web_fetch url must start with http:// or https://: {url}"
                )
            }
            Self::NotDirectory { path } => {
                write!(f, "path is not a directory: {}", path.display())
            }
            Self::WebFetchFailed { url, message } => write!(f, "failed to fetch {url}: {message}"),
            Self::RenameTargetExists { path } => {
                write!(f, "rename target already exists: {}", path.display())
            }
            Self::InvalidPatch(message) => write!(f, "invalid patch: {message}"),
            Self::PatchApplyFailed { path, message } => {
                write!(f, "failed to apply patch to {}: {message}", path.display())
            }
        }
    }
}

impl std::error::Error for ToolError {}

fn cap_output(text: String, limit: usize) -> CappedOutput {
    if text.len() <= limit {
        return CappedOutput::complete(text);
    }

    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }

    CappedOutput {
        text: text[..end].to_string(),
        truncated: true,
    }
}

fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let minutes = total / 60;
    let seconds = total % 60;
    if minutes == 0 {
        format!("{seconds}s")
    } else {
        format!("{minutes}m {seconds}s")
    }
}

fn format_partial_output(output: &str) -> String {
    if output.trim().is_empty() {
        "No output was captured before the checkpoint.".to_string()
    } else {
        format!("Partial output captured before the checkpoint:\n{output}")
    }
}

fn kill_background_process(child_id: Option<u32>) {
    #[cfg(unix)]
    {
        if let Some(pid) = child_id {
            // Commands are spawned in their own process group, so a negative pid
            // terminates the shell and any children it started.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child_id;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewlineStyle {
    Lf,
    Crlf,
}

#[derive(Debug)]
struct TextFile {
    text: String,
    newline: NewlineStyle,
}

fn normalize_edit_text(text: &str, newline: NewlineStyle) -> String {
    match newline {
        NewlineStyle::Lf => text.replace("\r\n", "\n"),
        NewlineStyle::Crlf => text.replace("\r\n", "\n").replace('\n', "\r\n"),
    }
}

fn edit_preview(text: &str) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 120 {
        return format!("{compact:?}");
    }
    let preview = compact.chars().take(117).collect::<String>();
    format!("{preview}...").escape_debug().to_string()
}

fn edit_target_hint(content: &str, old: &str) -> Option<String> {
    let probe = old
        .lines()
        .map(str::trim)
        .find(|line| line.len() >= 12)
        .or_else(|| old.lines().map(str::trim).find(|line| !line.is_empty()))?;
    let probe = probe.chars().take(80).collect::<String>();

    content.lines().enumerate().find_map(|(index, line)| {
        if !line.contains(&probe) {
            return None;
        }
        Some(format!(
            " Nearby candidate at line {}: {}",
            index + 1,
            edit_preview(line)
        ))
    })
}

fn format_edit_hint(hint: &Option<String>) -> &str {
    hint.as_deref().unwrap_or("")
}

fn format_line_patch_mismatch(
    lines: &[String],
    start_line: usize,
    end_line: usize,
    expected: &str,
) -> String {
    let index = start_line.saturating_sub(1);
    let nearby_start = index.saturating_sub(2);
    let nearby_end = (index + 3).min(lines.len());
    let nearby = if nearby_start < nearby_end {
        lines[nearby_start..nearby_end]
            .iter()
            .enumerate()
            .map(|(offset, line)| {
                let number = nearby_start + offset + 1;
                format!("{number}: {}", line.trim_end_matches(['\r', '\n']))
            })
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        "<no nearby lines; start_line is beyond end of file>".to_string()
    };

    format!(
        "line-aware update did not match at lines {start_line}-{end_line}. The tool does not search for anchors. Expected preview: {}. Nearby actual lines:\n{}",
        edit_preview(expected),
        nearby
    )
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn hashes_equal(left: &str, right: &str) -> bool {
    left.trim().eq_ignore_ascii_case(right.trim())
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn inner(pattern: &[u8], text: &[u8]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        if pattern[0] == b'*' {
            return inner(&pattern[1..], text) || (!text.is_empty() && inner(pattern, &text[1..]));
        }
        if !text.is_empty() && (pattern[0] == b'?' || pattern[0] == text[0]) {
            return inner(&pattern[1..], &text[1..]);
        }
        false
    }

    inner(pattern.as_bytes(), text.as_bytes())
}

fn split_lines_lossless(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut lines = Vec::new();
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            lines.push(text[start..=index].to_string());
            start = index + 1;
        }
    }
    if start < text.len() {
        lines.push(text[start..].to_string());
    }
    lines
}

#[derive(Debug)]
struct UnifiedFilePatch {
    path: PathBuf,
    hunks: Vec<UnifiedHunk>,
}

#[derive(Debug)]
struct UnifiedHunk {
    old_start: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug)]
struct HunkLine {
    kind: HunkLineKind,
    text: String,
}

#[derive(Debug, Clone, Copy)]
enum HunkLineKind {
    Context,
    Remove,
    Add,
}

#[derive(Debug)]
enum BeginPatchOperation {
    Update { path: PathBuf, edits: Vec<TextEdit> },
    Add { path: PathBuf, content: String },
    Delete { path: PathBuf },
}

fn parse_begin_patch(patch: &str) -> Result<Vec<BeginPatchOperation>, ToolError> {
    let lines = patch.lines().collect::<Vec<_>>();
    if !matches!(
        lines.first().map(|line| line.trim()),
        Some("*** Begin Patch")
    ) {
        return Err(ToolError::InvalidPatch(
            "begin patch must start with *** Begin Patch".to_string(),
        ));
    }

    let mut operations = Vec::new();
    let mut index = 1usize;
    while index < lines.len() {
        let line = lines[index];
        if line.trim() == "*** End Patch" {
            return Ok(operations);
        }

        if let Some(path) = line.strip_prefix("*** Update File: ") {
            index += 1;
            let mut edits = Vec::new();
            let mut old = String::new();
            let mut new = String::new();

            while index < lines.len() && !lines[index].starts_with("*** ") {
                let raw = lines[index];
                if raw.starts_with("@@") {
                    push_begin_patch_edit(&mut edits, &mut old, &mut new)?;
                    index += 1;
                    continue;
                }
                if raw == "*** End of File" || raw == r"\ No newline at end of file" {
                    index += 1;
                    continue;
                }

                let Some(prefix) = raw.as_bytes().first().copied() else {
                    return Err(ToolError::InvalidPatch(
                        "empty line in begin patch hunk must be prefixed with a space, +, or -"
                            .to_string(),
                    ));
                };
                let text = format!("{}\n", &raw[1..]);
                match prefix {
                    b' ' => {
                        old.push_str(&text);
                        new.push_str(&text);
                    }
                    b'-' => old.push_str(&text),
                    b'+' => new.push_str(&text),
                    _ => {
                        return Err(ToolError::InvalidPatch(format!(
                            "invalid begin patch hunk line: {raw}"
                        )));
                    }
                }
                index += 1;
            }
            push_begin_patch_edit(&mut edits, &mut old, &mut new)?;
            if edits.is_empty() {
                return Err(ToolError::InvalidPatch(format!(
                    "update patch for {path} has no edits"
                )));
            }
            operations.push(BeginPatchOperation::Update {
                path: PathBuf::from(path.trim()),
                edits,
            });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut content = String::new();
            while index < lines.len() && !lines[index].starts_with("*** ") {
                let raw = lines[index];
                if !raw.starts_with('+') {
                    return Err(ToolError::InvalidPatch(format!(
                        "add file patch line must start with +: {raw}"
                    )));
                }
                content.push_str(&raw[1..]);
                content.push('\n');
                index += 1;
            }
            operations.push(BeginPatchOperation::Add {
                path: PathBuf::from(path.trim()),
                content,
            });
            continue;
        }

        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            operations.push(BeginPatchOperation::Delete {
                path: PathBuf::from(path.trim()),
            });
            index += 1;
            continue;
        }

        return Err(ToolError::InvalidPatch(format!(
            "unsupported begin patch directive: {line}"
        )));
    }

    Err(ToolError::InvalidPatch(
        "begin patch is missing *** End Patch".to_string(),
    ))
}

fn push_begin_patch_edit(
    edits: &mut Vec<TextEdit>,
    old: &mut String,
    new: &mut String,
) -> Result<(), ToolError> {
    if old == new {
        old.clear();
        new.clear();
        return Ok(());
    }
    if old.is_empty() {
        return Err(ToolError::InvalidPatch(
            "update hunks must include context or removed lines".to_string(),
        ));
    }
    edits.push(TextEdit::new(std::mem::take(old), std::mem::take(new)));
    Ok(())
}

fn parse_unified_patch(patch: &str) -> Result<Vec<UnifiedFilePatch>, ToolError> {
    let lines = patch.lines().collect::<Vec<_>>();
    let mut index = 0usize;
    let mut files = Vec::new();

    while index < lines.len() {
        if !lines[index].starts_with("--- ") {
            index += 1;
            continue;
        }
        index += 1;

        if index >= lines.len() || !lines[index].starts_with("+++ ") {
            return Err(ToolError::InvalidPatch(
                "missing +++ file header".to_string(),
            ));
        }
        let path = parse_patch_path(lines[index].trim_start_matches("+++ ").trim())?;
        index += 1;

        let mut hunks = Vec::new();
        while index < lines.len() {
            let line = lines[index];
            if line.starts_with("--- ") {
                break;
            }
            if !line.starts_with("@@ ") {
                index += 1;
                continue;
            }

            let old_start = parse_hunk_old_start(line)?;
            index += 1;
            let mut hunk_lines = Vec::new();
            while index < lines.len()
                && !lines[index].starts_with("@@ ")
                && !lines[index].starts_with("--- ")
            {
                let raw = lines[index];
                if raw == r"\ No newline at end of file" {
                    index += 1;
                    continue;
                }
                let (kind, text) = match raw.as_bytes().first().copied() {
                    Some(b' ') => (HunkLineKind::Context, &raw[1..]),
                    Some(b'-') => (HunkLineKind::Remove, &raw[1..]),
                    Some(b'+') => (HunkLineKind::Add, &raw[1..]),
                    _ => {
                        return Err(ToolError::InvalidPatch(format!("invalid hunk line: {raw}")));
                    }
                };
                hunk_lines.push(HunkLine {
                    kind,
                    text: format!("{text}\n"),
                });
                index += 1;
            }
            hunks.push(UnifiedHunk {
                old_start,
                lines: hunk_lines,
            });
        }

        if hunks.is_empty() {
            return Err(ToolError::InvalidPatch(format!(
                "file patch for {} has no hunks",
                path.display()
            )));
        }
        files.push(UnifiedFilePatch { path, hunks });
    }

    Ok(files)
}

fn parse_patch_path(raw: &str) -> Result<PathBuf, ToolError> {
    let path = raw.split_whitespace().next().unwrap_or(raw);
    let path = path
        .strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path);
    if path == "/dev/null" || path.is_empty() {
        return Err(ToolError::InvalidPatch(
            "freeform patches currently require an existing file path".to_string(),
        ));
    }
    Ok(PathBuf::from(path))
}

fn parse_hunk_old_start(header: &str) -> Result<usize, ToolError> {
    let old_part = header
        .split_whitespace()
        .find(|part| part.starts_with('-'))
        .ok_or_else(|| ToolError::InvalidPatch(format!("missing old range in hunk {header}")))?;
    let old_part = old_part.trim_start_matches('-');
    let start = old_part.split(',').next().unwrap_or(old_part);
    start
        .parse::<usize>()
        .map_err(|_| ToolError::InvalidPatch(format!("invalid old range in hunk {header}")))
}

fn already_applied_hunk(lines: &[String], new_lines: &[String], expected_start: usize) -> bool {
    if new_lines.is_empty() {
        return true;
    }
    if expected_start <= lines.len() && lines[expected_start..].starts_with(new_lines) {
        return true;
    }
    lines
        .windows(new_lines.len())
        .any(|window| window == new_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn read_file_reads_workspace_relative_file() {
        let temp = TempDir::new("read");
        fs::write(temp.path().join("hello.txt"), "hello").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.read_file("hello.txt").unwrap();

        assert_eq!(result.name, ToolName::ReadFile);
        assert_eq!(result.output, "hello");
        assert_eq!(result.metadata["path"], "hello.txt");
        assert_eq!(result.metadata["bytes"], 5);
        assert_eq!(result.metadata["sha256"], sha256_hex(b"hello"));
    }

    #[test]
    fn read_file_range_returns_numbered_subset_with_full_file_hash() {
        let temp = TempDir::new("read-range");
        let body = "one\ntwo\nthree\nfour\n";
        fs::write(temp.path().join("hello.txt"), body).unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime
            .read_file_range("hello.txt", Some(2), Some(3))
            .unwrap();

        assert_eq!(result.name, ToolName::ReadFile);
        assert_eq!(result.output, "2: two\n3: three");
        assert_eq!(result.metadata["path"], "hello.txt");
        assert_eq!(result.metadata["bytes"], body.len());
        assert_eq!(result.metadata["sha256"], sha256_hex(body.as_bytes()));
        assert_eq!(result.metadata["total_lines"], 4);
        assert_eq!(result.metadata["start_line"], 2);
        assert_eq!(result.metadata["end_line"], 3);
        assert_eq!(result.metadata["returned_lines"], 2);
    }

    #[test]
    fn advertised_tools_use_apply_patch_as_canonical_write_tool() {
        let names = tool_names();

        assert!(names.contains(&"apply_patch".to_string()));
        assert!(names.contains(&"read_blob".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"edit_file".to_string()));
        assert!(!names.contains(&"multi_edit".to_string()));
        assert!(!names.contains(&"apply_patch_freeform".to_string()));
        assert!(!names.contains(&"apply_patch_structured".to_string()));
    }

    #[test]
    fn list_dir_returns_sorted_entries_and_marks_directories() {
        let temp = TempDir::new("list-dir");
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("Cargo.toml"), "[package]").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.list_dir(Option::<&str>::None).unwrap();

        assert_eq!(result.name, ToolName::ListDir);
        assert_eq!(result.output, "Cargo.toml\nsrc/");
        assert_eq!(result.metadata["entries"], 2);
    }

    #[test]
    fn glob_matches_workspace_relative_paths() {
        let temp = TempDir::new("glob");
        fs::create_dir_all(temp.path().join("src/bin")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "").unwrap();
        fs::write(temp.path().join("src/bin/main.rs"), "").unwrap();
        fs::write(temp.path().join("README.md"), "").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.glob("src/*.rs", Option::<&str>::None).unwrap();

        assert_eq!(result.name, ToolName::Glob);
        assert!(result.output.contains("src/lib.rs"));
        assert!(result.output.contains("src/bin/main.rs"));
        assert!(!result.output.contains("README.md"));
        assert_eq!(result.metadata["pattern"], "src/*.rs");
        assert_eq!(result.metadata["matches"], 2);
    }

    #[test]
    fn todo_write_formats_task_list() {
        let temp = TempDir::new("todo-write");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime
            .todo_write(&[TodoItem {
                content: "Add glob tool".to_string(),
                status: "completed".to_string(),
            }])
            .unwrap();

        assert_eq!(result.name, ToolName::TodoWrite);
        assert_eq!(result.output, "1. [completed] Add glob tool");
        assert_eq!(result.metadata["items"], 1);
        assert_eq!(result.metadata["completed"], 1);
    }

    #[test]
    fn web_fetch_rejects_non_http_urls() {
        let temp = TempDir::new("web-fetch-url");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let error = runtime.web_fetch("file:///tmp/nope").unwrap_err();

        assert!(matches!(error, ToolError::InvalidUrl(_)));
    }

    #[test]
    fn write_file_refuses_parent_escape() {
        let temp = TempDir::new("write-escape");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let error = runtime.write_file("../outside.txt", "nope").unwrap_err();

        assert!(matches!(error, ToolError::PathEscapesWorkspace { .. }));
    }

    #[test]
    fn write_file_creates_missing_parent_dirs() {
        let temp = TempDir::new("write-nested");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        // Nested path whose parent dirs don't exist yet (the todo-app case).
        runtime
            .write_file("todo-app/backend/package.json", "{}")
            .unwrap();

        let written =
            std::fs::read_to_string(temp.path().join("todo-app/backend/package.json")).unwrap();
        assert_eq!(written, "{}");
    }

    #[test]
    fn write_file_refuses_absolute_paths() {
        let temp = TempDir::new("write-absolute");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let error = runtime.write_file("/tmp/outside.txt", "nope").unwrap_err();

        assert!(matches!(error, ToolError::AbsolutePath { .. }));
    }

    #[test]
    fn approved_runtime_allows_absolute_path_outside_workspace() {
        let workspace = TempDir::new("approved-outside-workspace");
        let outside = TempDir::new("approved-outside-target");
        let outside_file = outside.path().join("memory.md");
        fs::write(&outside_file, "remember this").unwrap();
        let runtime = ToolRuntime::new(workspace.path()).unwrap();

        let locked = runtime.read_file(&outside_file).unwrap_err();
        assert!(matches!(locked, ToolError::AbsolutePath { .. }));

        let approved = runtime.approved_outside_access();
        let result = approved.read_file(&outside_file).unwrap();

        assert_eq!(result.output, "remember this");
        assert_eq!(
            result.metadata["path"],
            outside_file.canonicalize().unwrap().display().to_string()
        );
    }

    #[test]
    fn unrestricted_runtime_allows_reads_and_writes_outside_workspace() {
        let workspace = TempDir::new("unrestricted-workspace");
        let outside = TempDir::new("unrestricted-target");
        let outside_file = outside.path().join("memory.md");
        fs::write(&outside_file, "remember this").unwrap();

        let runtime = ToolRuntime::unrestricted(workspace.path()).unwrap();
        let read = runtime.read_file(&outside_file).unwrap();
        assert_eq!(read.output, "remember this");

        let written = outside.path().join("notes").join("new.md");
        runtime.write_file(&written, "new memory").unwrap();
        assert_eq!(fs::read_to_string(written).unwrap(), "new memory");
    }

    #[test]
    fn memory_tools_read_and_write_configured_file_outside_workspace() {
        let workspace = TempDir::new("memory-tool-workspace");
        let source = TempDir::new("memory-tool-source");
        let memory_file = source.path().join(".inductor").join("memory.md");
        let runtime = ToolRuntime::new(workspace.path())
            .unwrap()
            .with_memory_file(memory_file.clone());

        let initial = runtime.read_memory().unwrap();
        assert_eq!(initial.metadata["exists"], false);

        runtime
            .write_memory("# Memory\n\n- Use cargo test.")
            .unwrap();
        assert_eq!(
            fs::read_to_string(&memory_file).unwrap(),
            "# Memory\n\n- Use cargo test."
        );

        let read = runtime.read_memory().unwrap();
        assert_eq!(read.output, "# Memory\n\n- Use cargo test.");
        assert_eq!(read.metadata["exists"], true);
        assert_eq!(read.metadata["path"], memory_file.display().to_string());
    }

    #[test]
    fn memory_tools_error_when_unconfigured() {
        let workspace = TempDir::new("memory-tool-unconfigured");
        let runtime = ToolRuntime::new(workspace.path()).unwrap();

        let error = runtime.read_memory().unwrap_err();

        assert!(matches!(error, ToolError::MemoryUnavailable));
    }

    #[test]
    fn read_file_accepts_absolute_path_inside_workspace() {
        let temp = TempDir::new("read-absolute-inside");
        let file = temp.path().join("CONTEXT.md");
        fs::write(&file, "context").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.read_file(&file).unwrap();

        assert_eq!(result.output, "context");
        assert_eq!(result.metadata["path"], "CONTEXT.md");
    }

    #[test]
    fn write_file_accepts_new_absolute_path_inside_workspace() {
        let temp = TempDir::new("write-absolute-inside");
        let file = temp.path().join("new.md");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        runtime.write_file(&file, "new").unwrap();

        assert_eq!(fs::read_to_string(file).unwrap(), "new");
    }

    #[test]
    fn write_file_creates_file_inside_workspace() {
        let temp = TempDir::new("write");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.write_file("nested.txt", "hello").unwrap();

        assert_eq!(result.name, ToolName::WriteFile);
        assert_eq!(
            fs::read_to_string(temp.path().join("nested.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn grep_returns_matching_lines_with_relative_paths() {
        let temp = TempDir::new("grep");
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "one\nneedle\nthree").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.grep("needle").unwrap();

        assert_eq!(result.name, ToolName::Grep);
        assert_eq!(result.output, "src/lib.rs:2:needle\n");
    }

    #[test]
    fn bash_runs_inside_workspace() {
        let temp = TempDir::new("bash");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.bash("pwd && printf done").unwrap();

        assert_eq!(result.name, ToolName::Bash);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains(temp.path().to_str().unwrap()));
        assert!(result.output.contains("done"));
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "sandbox enforced only on macOS")]
    fn sandboxed_bash_allows_writes_inside_workspace() {
        let temp = TempDir::new("sandbox-inside");
        let runtime = ToolRuntime::sandboxed(temp.path()).unwrap();

        let result = runtime
            .bash("echo hi > inside.txt && cat inside.txt")
            .unwrap();

        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("hi"));
    }

    #[test]
    #[cfg_attr(not(target_os = "macos"), ignore = "sandbox enforced only on macOS")]
    fn sandboxed_bash_blocks_writes_outside_workspace() {
        let temp = TempDir::new("sandbox-outside");
        let runtime = ToolRuntime::sandboxed(temp.path()).unwrap();
        // Target HOME, which is outside both write roots (workspace + tempdir).
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let escape_target = home.join("inductor-sandbox-escape.txt");
        let _ = fs::remove_file(&escape_target);

        let result = runtime
            .bash(&format!("echo nope > {}", escape_target.display()))
            .unwrap();

        assert_ne!(result.exit_code, Some(0));
        assert!(
            !escape_target.exists(),
            "sandbox must block writes outside the workspace"
        );
    }

    #[test]
    fn bash_reports_nonzero_exit_code() {
        let temp = TempDir::new("bash-exit");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime.bash("exit 7").unwrap();

        assert_eq!(result.exit_code, Some(7));
    }

    #[tokio::test]
    async fn bash_checkpoint_does_not_interrupt_command() {
        let temp = TempDir::new("bash-checkpoint");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let err = runtime
            .bash_cancellable_until(
                "printf partial; sleep 1; printf done > marker.txt",
                CancellationToken::new(),
                Some(Duration::from_millis(100)),
            )
            .await
            .unwrap_err();

        let command_id = match err {
            ToolError::CommandCheckpoint {
                command_id, output, ..
            } => {
                assert!(output.contains("partial"));
                command_id
            }
            other => panic!("expected checkpoint, got {other:?}"),
        };

        tokio::time::sleep(Duration::from_millis(1_300)).await;
        assert_eq!(
            fs::read_to_string(temp.path().join("marker.txt")).unwrap(),
            "done"
        );

        let result = runtime
            .bash_wait(command_id, Duration::from_millis(10))
            .await
            .unwrap();
        assert_eq!(result.name, ToolName::BashWait);
        assert!(result.output.contains("Final output"));
        assert!(result.output.contains("partial"));
    }

    #[tokio::test]
    async fn bash_wait_returns_final_output_after_checkpoint() {
        let temp = TempDir::new("bash-wait-final");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let err = runtime
            .bash_cancellable_until(
                "printf partial; sleep 1; printf done",
                CancellationToken::new(),
                Some(Duration::from_millis(100)),
            )
            .await
            .unwrap_err();

        let command_id = match err {
            ToolError::CommandCheckpoint { command_id, .. } => command_id,
            other => panic!("expected checkpoint, got {other:?}"),
        };

        let result = runtime
            .bash_wait(command_id, Duration::from_secs(2))
            .await
            .unwrap();

        assert_eq!(result.name, ToolName::BashWait);
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.contains("Final output"));
        assert!(result.output.contains("partialdone"));
    }

    #[tokio::test]
    async fn bash_kill_stops_checkpointed_command_tree() {
        let temp = TempDir::new("bash-kill");
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let err = runtime
            .bash_cancellable_until(
                "printf start; sleep 2; printf done > marker.txt",
                CancellationToken::new(),
                Some(Duration::from_millis(100)),
            )
            .await
            .unwrap_err();

        let command_id = match err {
            ToolError::CommandCheckpoint { command_id, .. } => command_id,
            other => panic!("expected checkpoint, got {other:?}"),
        };

        let result = runtime.bash_kill(command_id).await.unwrap();

        assert_eq!(result.name, ToolName::BashKill);
        assert!(result.output.contains("killed"));
        assert!(result.output.contains("start"));
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!temp.path().join("marker.txt").exists());
    }

    #[test]
    fn output_cap_preserves_utf8_boundaries() {
        let capped = cap_output("aaébb".to_string(), 4);

        assert_eq!(capped.text, "aaé");
        assert!(capped.truncated);
    }

    #[test]
    fn edit_file_replaces_one_exact_match() {
        let temp = TempDir::new("edit");
        fs::write(
            temp.path().join("main.rs"),
            "fn main() {\n    todo!();\n}\n",
        )
        .unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let result = runtime
            .edit_file("main.rs", "todo!();", "println!(\"hi\");", None)
            .unwrap();

        assert_eq!(result.name, ToolName::EditFile);
        assert_eq!(
            fs::read_to_string(temp.path().join("main.rs")).unwrap(),
            "fn main() {\n    println!(\"hi\");\n}\n"
        );
    }

    #[test]
    fn edit_file_preserves_crlf_line_endings() {
        let temp = TempDir::new("edit-crlf");
        fs::write(temp.path().join("file.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        runtime
            .edit_file("file.txt", "two\n", "TWO\n", None)
            .unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "one\r\nTWO\r\nthree\r\n"
        );
    }

    #[test]
    fn edit_file_preserves_missing_trailing_newline() {
        let temp = TempDir::new("edit-no-final-newline");
        fs::write(temp.path().join("file.txt"), "one\ntwo").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        runtime.edit_file("file.txt", "two", "TWO", None).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "one\nTWO"
        );
    }

    #[test]
    fn edit_file_rejects_non_unique_match() {
        let temp = TempDir::new("edit-duplicate");
        fs::write(temp.path().join("file.txt"), "x\nx\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let error = runtime.edit_file("file.txt", "x", "y", None).unwrap_err();

        assert!(matches!(
            error,
            ToolError::EditTargetNotUnique { count: 2, .. }
        ));
    }

    #[test]
    fn edit_file_error_includes_recovery_context() {
        let temp = TempDir::new("edit-context");
        fs::write(
            temp.path().join("file.txt"),
            "alpha\nlet value = current_call(foo);\nomega\n",
        )
        .unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let error = runtime
            .edit_file(
                "file.txt",
                "let value = previous_call(foo);",
                "let value = next_call(foo);",
                None,
            )
            .unwrap_err()
            .to_string();

        assert!(error.contains("Target preview"));
        assert!(error.contains("previous_call"));
    }

    #[test]
    fn edit_file_is_idempotent_when_replacement_already_exists() {
        let temp = TempDir::new("edit-idempotent");
        fs::write(temp.path().join("file.txt"), "new\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        runtime.edit_file("file.txt", "old", "new", None).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "new\n"
        );
    }

    #[test]
    fn edit_file_rejects_binary_file() {
        let temp = TempDir::new("edit-binary");
        fs::write(temp.path().join("file.bin"), b"abc\0def").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let error = runtime
            .edit_file("file.bin", "abc", "xyz", None)
            .unwrap_err();

        assert!(matches!(error, ToolError::BinaryFile { .. }));
    }

    #[test]
    fn edit_file_rejects_stale_expected_hash() {
        let temp = TempDir::new("edit-stale");
        fs::write(temp.path().join("file.txt"), "current").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        let error = runtime
            .edit_file("file.txt", "current", "new", Some(&sha256_hex(b"old")))
            .unwrap_err();

        assert!(matches!(error, ToolError::StaleFile { .. }));
    }

    #[test]
    fn edit_file_ignores_empty_expected_hash() {
        let temp = TempDir::new("edit-empty-hash");
        fs::write(temp.path().join("file.txt"), "current").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();

        runtime
            .edit_file("file.txt", "current", "new", Some(""))
            .unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "new"
        );
    }

    #[test]
    fn multi_edit_applies_edits_in_order() {
        let temp = TempDir::new("multi-edit");
        fs::write(temp.path().join("file.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let edits = vec![
            TextEdit::new("alpha", "ALPHA"),
            TextEdit::new("gamma", "GAMMA"),
        ];

        runtime.multi_edit("file.txt", &edits, None).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );
    }

    #[test]
    fn structured_patch_renames_and_rejects_existing_target() {
        let temp = TempDir::new("structured-rename");
        fs::write(temp.path().join("from.txt"), "body").unwrap();
        fs::write(temp.path().join("to.txt"), "collision").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = StructuredPatch {
            operations: vec![StructuredPatchOperation::Rename {
                from: PathBuf::from("from.txt"),
                to: PathBuf::from("to.txt"),
                expected_hash: None,
            }],
        };

        let error = runtime.apply_patch_structured(&patch).unwrap_err();

        assert!(matches!(error, ToolError::RenameTargetExists { .. }));
        assert!(temp.path().join("from.txt").exists());
    }

    #[test]
    fn structured_patch_applies_edit_and_rename() {
        let temp = TempDir::new("structured");
        fs::write(temp.path().join("file.txt"), "hello\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let expected_hash = sha256_hex(b"hello\n");
        let patch = StructuredPatch {
            operations: vec![
                StructuredPatchOperation::Edit {
                    path: PathBuf::from("file.txt"),
                    old: "hello".to_string(),
                    new: "hi".to_string(),
                    expected_hash: Some(expected_hash),
                },
                StructuredPatchOperation::Rename {
                    from: PathBuf::from("file.txt"),
                    to: PathBuf::from("renamed.txt"),
                    expected_hash: None,
                },
            ],
        };

        runtime.apply_patch_structured(&patch).unwrap();

        assert!(!temp.path().join("file.txt").exists());
        assert_eq!(
            fs::read_to_string(temp.path().join("renamed.txt")).unwrap(),
            "hi\n"
        );
    }

    #[test]
    fn freeform_patch_applies_unified_diff() {
        let temp = TempDir::new("freeform");
        fs::write(temp.path().join("file.txt"), "one\ntwo\nthree\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = "\
--- a/file.txt
+++ b/file.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
";

        runtime.apply_patch_freeform(patch).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[test]
    fn freeform_patch_is_idempotent_when_hunk_already_applied() {
        let temp = TempDir::new("freeform-idempotent");
        fs::write(temp.path().join("file.txt"), "one\nTWO\nthree\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = "\
--- a/file.txt
+++ b/file.txt
@@ -1,3 +1,3 @@
 one
-two
+TWO
 three
";

        runtime.apply_patch_freeform(patch).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[test]
    fn freeform_patch_accepts_begin_patch_update() {
        let temp = TempDir::new("begin-patch-update");
        fs::write(temp.path().join("file.txt"), "one\ntwo\nthree\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = "\
*** Begin Patch
*** Update File: file.txt
@@
 one
-two
+TWO
 three
*** End Patch
";

        runtime.apply_patch_freeform(patch).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[test]
    fn freeform_patch_accepts_begin_patch_add_and_delete() {
        let temp = TempDir::new("begin-patch-add-delete");
        fs::write(temp.path().join("old.txt"), "old\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = "\
*** Begin Patch
*** Add File: new.txt
+new
*** Delete File: old.txt
*** End Patch
";

        runtime.apply_patch_freeform(patch).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("new.txt")).unwrap(),
            "new\n"
        );
        assert!(!temp.path().join("old.txt").exists());
    }

    #[test]
    fn apply_patch_creates_new_file() {
        let temp = TempDir::new("canonical-apply-patch-add");
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = "\
*** Begin Patch
*** Add File: src/new_file.rs
+fn main() {
+    println!(\"hello\");
+}
*** End Patch
";

        let result = runtime.apply_patch(patch).unwrap();

        assert_eq!(result.name, ToolName::ApplyPatch);
        assert_eq!(
            fs::read_to_string(temp.path().join("src/new_file.rs")).unwrap(),
            "fn main() {\n    println!(\"hello\");\n}\n"
        );
    }

    #[test]
    fn line_patch_updates_exact_line_without_anchor_search() {
        let temp = TempDir::new("line-patch-update");
        fs::write(temp.path().join("file.txt"), "same\nsame\nsame\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = LinePatch {
            operations: vec![LinePatchOperation::Update {
                path: PathBuf::from("file.txt"),
                start_line: 2,
                end_line: 2,
                old: "same\n".to_string(),
                new: "changed\n".to_string(),
                expected_hash: None,
            }],
        };

        runtime.apply_line_patch(&patch).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "same\nchanged\nsame\n"
        );
    }

    #[test]
    fn line_patch_rejects_stale_line_without_searching_for_anchor() {
        let temp = TempDir::new("line-patch-stale");
        fs::write(temp.path().join("file.txt"), "same\nsame\nsame\n").unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let patch = LinePatch {
            operations: vec![LinePatchOperation::Update {
                path: PathBuf::from("file.txt"),
                start_line: 3,
                end_line: 3,
                old: "missing\n".to_string(),
                new: "changed\n".to_string(),
                expected_hash: None,
            }],
        };

        let error = runtime.apply_line_patch(&patch).unwrap_err();

        assert!(matches!(error, ToolError::PatchApplyFailed { .. }));
        assert!(
            error
                .to_string()
                .contains("line-aware update did not match")
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "same\nsame\nsame\n"
        );
    }

    #[test]
    fn line_patch_applies_multiple_updates_to_one_file_against_original_snapshot() {
        let temp = TempDir::new("line-patch-multiple-same-file");
        fs::write(
            temp.path().join("file.txt"),
            "one\ntwo\nthree\nfour\nfive\n",
        )
        .unwrap();
        let runtime = ToolRuntime::new(temp.path()).unwrap();
        let hash = runtime
            .read_file_range("file.txt", None, None)
            .unwrap()
            .metadata["sha256"]
            .as_str()
            .unwrap()
            .to_string();
        let patch = LinePatch {
            operations: vec![
                LinePatchOperation::Update {
                    path: PathBuf::from("file.txt"),
                    start_line: 2,
                    end_line: 2,
                    old: "two\n".to_string(),
                    new: "TWO\nTWO-B\n".to_string(),
                    expected_hash: Some(hash.clone()),
                },
                LinePatchOperation::Update {
                    path: PathBuf::from("file.txt"),
                    start_line: 4,
                    end_line: 4,
                    old: "four\n".to_string(),
                    new: "FOUR\n".to_string(),
                    expected_hash: Some(hash),
                },
            ],
        };

        runtime.apply_line_patch(&patch).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).unwrap(),
            "one\nTWO\nTWO-B\nthree\nFOUR\nfive\n"
        );
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("inductor-tools-{label}-{nanos}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
