use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
};

use auth::{AuthDetector, ProviderKind, RuntimeCredentialLoader};
use base64::{Engine as _, engine::general_purpose};
use clap::{Parser, Subcommand, ValueEnum};
use context::{
    ApproxTokenCounter, ContextLimits, ContextMessage, ModelEffort, ProviderFamily, TokenCounter,
    compact_messages, prepare_context, translate_effort,
};
use diff::{DiffRequest, diff_worktree};
use futures_util::StreamExt;
use git::{CreateWorktreeRequest, WorktreeManager};
use harness_core::{
    ApprovalPolicy, ImageAttachment, ModelRole, PermissionDecision, PermissionRequestId,
    PermissionResponse, ProviderId, QuestionAnswer, QuestionResponse, SessionEvent, SessionId,
    SessionStatus, StopReason, ToolCallId, TurnRequest, WorkspaceId,
};
use harness_runtime::{
    AllowStore, ApprovalRequest, Approver, AutoApprove, HarnessConfig, Role, SessionState,
    TranscriptMessage, run_turn,
};
use image::GenericImageView;
use persistence::{
    AppDb, StoredMessage, ToolCallRecord, ToolResultRecord, WorkspaceDb, WorktreeRecord,
    WorktreeStatus, new_session_record, now_rfc3339, workspace_state_path,
};
use provider_claude::ClaudeProvider;
use provider_codex::CodexProvider;
use provider_copilot::CopilotProvider;
use provider_core::{ProviderAuth, ProviderAuthKind, ProviderPlugin};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use session_naming::{
    SessionNamingConfig, generate_context_name, generate_pull_request_description,
    generate_session_name,
};
use std::time::{Duration, Instant};
use terminal::{PtyManager, SpawnTerminalRequest, TerminalSize};
use tokio_util::sync::CancellationToken;
use tools::{StructuredPatch, TextEdit, ToolRuntime};

const MAX_PROMPT_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_REPO_MEMORY_BYTES: usize = 32 * 1024;
const ORPHANED_SESSION_RECOVERY_MESSAGE: &str = "Recovered after Inductor restarted: the previous agent run was interrupted before it could finish. Inductor will automatically resume the latest interrupted prompt when this session is loaded.";

#[derive(Debug, Parser)]
#[command(name = "agent")]
#[command(about = "Rust harness sidecar for agent sessions")]
struct Cli {
    #[arg(long)]
    version_info: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    Diff {
        #[command(subcommand)]
        command: DiffCommand,
    },
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Run the experimental OpenTUI/Solid presentation layer.
    OpenTui {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        #[arg(long, value_enum, default_value_t = ProviderArg::Claude)]
        provider: ProviderArg,

        #[arg(long)]
        model: Option<String>,

        /// When to pause tool calls for approval. Defaults to yolo mode:
        /// never ask before running commands, edits, reads, or writes.
        #[arg(long, value_enum, default_value_t = ApprovalArg::Never)]
        approval: ApprovalArg,

        /// Restrict file tools and bash to the workspace instead of yolo mode.
        #[arg(long)]
        workspace_only: bool,
    },
    /// Run a full harness turn loop: prompt -> provider -> tools -> answer.
    Run {
        #[arg(long)]
        provider: ProviderArg,

        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        prompt: String,

        #[arg(long)]
        model: Option<String>,

        /// Development mode: edit the workspace in place, or run inside an
        /// isolated git worktree for parallel sessions.
        #[arg(long, value_enum, default_value_t = ModeArg::InPlace)]
        mode: ModeArg,

        /// Branch slug used when creating a worktree (worktree mode only).
        #[arg(long)]
        slug: Option<String>,

        #[arg(long, default_value_t = 8)]
        max_tool_rounds: usize,

        /// When to pause tool calls for approval. Defaults to yolo mode:
        /// never ask before running commands, edits, reads, or writes.
        #[arg(long, value_enum, default_value_t = ApprovalArg::Never)]
        approval: ApprovalArg,

        /// Auto-approve every prompt instead of asking on the terminal.
        #[arg(long)]
        yes: bool,

        /// Restrict file tools and bash to the workspace instead of yolo mode.
        #[arg(long)]
        workspace_only: bool,

        /// Deprecated no-op: yolo mode is now the default.
        #[arg(long, hide = true)]
        no_sandbox: bool,

        #[arg(long, default_value_t = 16_000)]
        soft_tokens: usize,

        #[arg(long, default_value_t = 24_000)]
        hard_tokens: usize,

        #[arg(long, default_value_t = 4 * 1024)]
        tool_result_inline_bytes: usize,

        #[arg(long)]
        blob_root: Option<PathBuf>,

        #[arg(long, value_enum, default_value_t = EffortArg::Medium)]
        effort: EffortArg,

        #[arg(long, value_enum, default_value_t = ModelRoleArg::Reasoning)]
        model_role: ModelRoleArg,

        /// Skill names or paths to load into the system prompt for this turn.
        #[arg(long = "skill")]
        skills: Vec<String>,

        /// App-level database path. When set, workspace/session metadata is also recorded there.
        #[arg(long)]
        app_db: Option<PathBuf>,

        /// Workspace/session database path. Defaults to <workspace>/.inductor/state.db.
        #[arg(long)]
        state_db: Option<PathBuf>,

        /// Continue an existing persisted session.
        #[arg(long)]
        session_id: Option<SessionId>,

        /// Stable workspace id to record with new sessions.
        #[arg(long)]
        workspace_id: Option<WorkspaceId>,
    },
    PrBody {
        #[arg(long)]
        provider: ProviderArg,

        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        title: String,

        #[arg(long)]
        diff: String,

        #[arg(long)]
        model: Option<String>,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Tool {
        #[command(subcommand)]
        command: ToolCommand,
    },
    Terminal {
        #[command(subcommand)]
        command: TerminalCommand,
    },
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    Detect,
    CopilotLogin,
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    InspectAuth {
        #[arg(long)]
        provider: ProviderArg,
    },
    Models {
        #[arg(long)]
        provider: ProviderArg,
    },
    Turn {
        #[arg(long)]
        provider: ProviderArg,

        #[arg(long)]
        prompt: String,

        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum DiffCommand {
    /// Show a renderable diff model for a workspace/repo against a base ref.
    Show {
        #[arg(long)]
        repo: PathBuf,

        #[arg(long, default_value = "HEAD")]
        base: String,

        #[arg(long, default_value_t = 3)]
        context_lines: u16,

        #[arg(long)]
        summary: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ContextCommand {
    Count {
        #[arg(long)]
        text: String,
    },
    Compact {
        #[arg(long)]
        text: String,

        #[arg(long, default_value_t = 100)]
        soft_tokens: usize,

        #[arg(long, default_value_t = 200)]
        hard_tokens: usize,
    },
    Effort {
        #[arg(long, value_enum)]
        provider: EffortProviderArg,

        #[arg(long, value_enum)]
        effort: EffortArg,
    },
}

#[derive(Debug, Subcommand)]
enum DbCommand {
    InitApp {
        #[arg(long)]
        path: PathBuf,
    },
    InitWorkspace {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        state_db: Option<PathBuf>,
    },
    Sessions {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        state_db: Option<PathBuf>,

        #[arg(long)]
        json: bool,
    },
    ShowSession {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        state_db: Option<PathBuf>,

        #[arg(long)]
        session_id: SessionId,

        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EffortProviderArg {
    Claude,
    Codex,
    Copilot,
    Generic,
}

impl From<EffortProviderArg> for ProviderFamily {
    fn from(value: EffortProviderArg) -> Self {
        match value {
            EffortProviderArg::Claude => Self::Claude,
            EffortProviderArg::Codex => Self::Codex,
            EffortProviderArg::Copilot => Self::Copilot,
            EffortProviderArg::Generic => Self::Generic,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// List skills found in the workspace, repo, and global skill directories.
    List {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        #[arg(long)]
        json: bool,
    },
    /// Create a new standard skill under <workspace>/.agents/skills/<name>/SKILL.md.
    Create {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        #[arg(long)]
        name: String,

        #[arg(long)]
        description: String,

        #[arg(long)]
        body: Option<String>,

        #[arg(long)]
        force: bool,

        #[arg(long)]
        json: bool,
    },
    /// Print the composed prompt layer, optionally preloading one or more tagged skills.
    Use {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        #[arg(long = "skill")]
        skills: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
struct SkillInfo {
    name: String,
    description: String,
    path: PathBuf,
    source: String,
}

#[derive(Debug, Deserialize)]
struct SkillFrontMatter {
    name: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum EffortArg {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl From<EffortArg> for ModelEffort {
    fn from(value: EffortArg) -> Self {
        match value {
            EffortArg::None => Self::None,
            EffortArg::Minimal => Self::Minimal,
            EffortArg::Low => Self::Low,
            EffortArg::Medium => Self::Medium,
            EffortArg::High => Self::High,
            EffortArg::Xhigh => Self::XHigh,
            EffortArg::Max => Self::Max,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ModelRoleArg {
    Reasoning,
    Executor,
    Reviewer,
}

impl From<ModelRoleArg> for ModelRole {
    fn from(value: ModelRoleArg) -> Self {
        match value {
            ModelRoleArg::Reasoning => Self::Reasoning,
            ModelRoleArg::Executor => Self::Executor,
            ModelRoleArg::Reviewer => Self::Reviewer,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderArg {
    Claude,
    Codex,
    Copilot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Edit the given workspace directory directly (default).
    InPlace,
    /// Run the agent inside an isolated git worktree so multiple sessions can
    /// work on the same repo in parallel.
    Worktree,
}

impl std::fmt::Display for ModeArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModeArg::InPlace => write!(f, "in-place"),
            ModeArg::Worktree => write!(f, "worktree"),
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ApprovalArg {
    Never,
    OnRequest,
    Mutating,
    OnFailure,
    Always,
}

impl From<ApprovalArg> for ApprovalPolicy {
    fn from(value: ApprovalArg) -> Self {
        match value {
            ApprovalArg::Never => Self::Never,
            ApprovalArg::OnRequest => Self::OnRequest,
            ApprovalArg::Mutating => Self::Mutating,
            ApprovalArg::OnFailure => Self::OnFailure,
            ApprovalArg::Always => Self::Always,
        }
    }
}

impl std::fmt::Display for ApprovalArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApprovalArg::Never => write!(f, "never"),
            ApprovalArg::OnRequest => write!(f, "on-request"),
            ApprovalArg::Mutating => write!(f, "mutating"),
            ApprovalArg::OnFailure => write!(f, "on-failure"),
            ApprovalArg::Always => write!(f, "always"),
        }
    }
}

impl From<ProviderArg> for ProviderKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::Claude => Self::Claude,
            ProviderArg::Codex => Self::Codex,
            ProviderArg::Copilot => Self::Copilot,
        }
    }
}

impl std::fmt::Display for ProviderArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderArg::Claude => write!(f, "claude"),
            ProviderArg::Codex => write!(f, "codex"),
            ProviderArg::Copilot => write!(f, "copilot"),
        }
    }
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    DemoEvents,
}

#[derive(Debug, Subcommand)]
enum ToolCommand {
    ReadFile {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        path: PathBuf,
    },
    WriteFile {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        path: PathBuf,

        #[arg(long)]
        content: String,
    },
    EditFile {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        path: PathBuf,

        #[arg(long, allow_hyphen_values = true)]
        old: String,

        #[arg(long, allow_hyphen_values = true)]
        new: String,

        #[arg(long)]
        expected_hash: Option<String>,
    },
    MultiEdit {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        path: PathBuf,

        /// JSON array of {"old": "...", "new": "..."} edits.
        #[arg(long)]
        edits_json: String,

        #[arg(long)]
        expected_hash: Option<String>,
    },
    ApplyPatchFreeform {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long, allow_hyphen_values = true)]
        patch: String,
    },
    ApplyPatchStructured {
        #[arg(long)]
        workspace: PathBuf,

        /// JSON object matching StructuredPatch: {"operations":[...]}.
        #[arg(long)]
        patch_json: String,
    },
    Grep {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        pattern: String,
    },
    Bash {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        command: String,
    },
}

#[derive(Debug, Subcommand)]
enum TerminalCommand {
    /// Spawn a PTY, run a command in the workspace, and print a JSON snapshot.
    Run {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long)]
        command: String,

        #[arg(long, default_value_t = 24)]
        rows: u16,

        #[arg(long, default_value_t = 80)]
        cols: u16,

        #[arg(long, default_value_t = 2_000)]
        timeout_ms: u64,
    },
    /// Minimal Phase 8 PTY smoke: command input, live output, resize, kill.
    Smoke {
        #[arg(long)]
        workspace: PathBuf,
    },
    /// Spawn a persistent interactive PTY and stream JSON snapshots over stdio.
    ///
    /// Reads newline-delimited control messages on stdin
    /// (`{"type":"input","data":"..."}` and `{"type":"resize","rows":N,"cols":M}`)
    /// and writes `{"type":"snapshot",...}` lines on stdout whenever the screen
    /// changes. Exits when the shell exits.
    Serve {
        #[arg(long)]
        workspace: PathBuf,

        #[arg(long, default_value_t = 30)]
        rows: u16,

        #[arg(long, default_value_t = 80)]
        cols: u16,
    },
}

#[derive(Debug, Subcommand)]
enum WorktreeCommand {
    Inspect {
        #[arg(long)]
        repo: PathBuf,
    },
    Create {
        #[arg(long)]
        repo: PathBuf,

        #[arg(long)]
        slug: String,

        #[arg(long)]
        managed_root: Option<PathBuf>,

        #[arg(long)]
        allow_dirty: bool,

        /// Record the worktree in this app DB so it can be listed and archived.
        #[arg(long)]
        app_db: Option<PathBuf>,

        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        repo: PathBuf,
    },
    /// List the worktrees Inductor manages in the app DB registry, joined with
    /// their session name/status. Used by the TUI multi-agent dashboard.
    Registry {
        #[arg(long)]
        app_db: Option<PathBuf>,

        /// Only list worktrees created from this repo. The path is resolved to
        /// its git toplevel, so a subdirectory of the repo works too. Omit to
        /// list every managed worktree across all repos.
        #[arg(long)]
        source_repo: Option<PathBuf>,

        #[arg(long)]
        json: bool,
    },
    Remove {
        #[arg(long)]
        repo: PathBuf,

        #[arg(long)]
        path: PathBuf,

        #[arg(long)]
        force: bool,
    },
    /// Report how far the target branch has moved since a worktree was created.
    Drift {
        #[arg(long)]
        workspace_id: WorkspaceId,

        #[arg(long)]
        app_db: Option<PathBuf>,

        /// Branch to compare against. Defaults to the worktree's base branch.
        #[arg(long)]
        target: Option<String>,
    },
    /// Archive a worktree: remove its working directory but keep the registry
    /// record and the session's chats/messages.
    Archive {
        #[arg(long)]
        workspace_id: WorkspaceId,

        #[arg(long)]
        app_db: Option<PathBuf>,

        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if cli.version_info {
        println!("inductor {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let result = match cli.command {
        Some(Command::Auth { command }) => run_auth_command(command).await,
        Some(Command::Provider { command }) => run_provider_command(command).await,
        Some(Command::Diff { command }) => run_diff_command(command).await,
        Some(Command::Context { command }) => run_context_command(command).await,
        Some(Command::Skill { command }) => run_skill_command(command).await,
        Some(Command::Db { command }) => run_db_command(command).await,
        Some(Command::OpenTui {
            workspace,
            provider,
            model,
            approval,
            workspace_only,
        }) => run_opentui_command(workspace, provider, model, approval, workspace_only).await,
        Some(Command::Run {
            provider,
            workspace,
            prompt,
            model,
            mode,
            slug,
            max_tool_rounds,
            approval,
            yes,
            workspace_only,
            no_sandbox: _,
            soft_tokens,
            hard_tokens,
            tool_result_inline_bytes,
            blob_root,
            effort,
            model_role,
            skills,
            app_db,
            state_db,
            session_id,
            workspace_id,
        }) => {
            run_harness_command(
                provider,
                workspace,
                prompt,
                model,
                mode,
                slug,
                max_tool_rounds,
                approval,
                yes,
                workspace_only,
                soft_tokens,
                hard_tokens,
                tool_result_inline_bytes,
                blob_root,
                effort,
                model_role,
                skills,
                app_db,
                state_db,
                session_id,
                workspace_id,
            )
            .await
        }
        Some(Command::PrBody {
            provider,
            workspace,
            title,
            diff,
            model,
        }) => run_pr_body_command(provider, workspace, title, diff, model).await,
        Some(Command::Session { command }) => run_session_command(command).await,
        Some(Command::Tool { command }) => run_tool_command(command).await,
        Some(Command::Terminal { command }) => run_terminal_command(command).await,
        Some(Command::Worktree { command }) => run_worktree_command(command).await,
        None => {
            run_opentui_command(
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                ProviderArg::Claude,
                None,
                ApprovalArg::Never,
                false,
            )
            .await
        }
    };

    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run_provider_command(command: ProviderCommand) -> Result<(), String> {
    match command {
        ProviderCommand::InspectAuth { provider } => {
            let provider = ProviderKind::from(provider);
            let detector = AuthDetector::from_env().map_err(|err| err.to_string())?;
            let credentials = detector.detect_all();
            let reference = credentials
                .iter()
                .find(|credential| credential.provider == provider)
                .ok_or_else(|| format!("no detected credential for {provider}"))?;

            println!("provider: {provider}");
            println!(
                "source: {}",
                reference.source.display_safe(detector.home_dir())
            );
            match provider {
                ProviderKind::Claude => {
                    let provider = ClaudeProvider::new().map_err(|err| err.to_string())?;
                    match provider.check_auth().await {
                        Ok(check) => {
                            println!("auth_loaded: {}", check.ok);
                            if let Some(error) = check.error {
                                println!("auth_error: {error}");
                            }
                        }
                        Err(error) => {
                            println!("auth_loaded: false");
                            println!("auth_error: {error}");
                        }
                    }
                    println!("auth_runtime: handled_by_claude_agent_sdk");
                }
                ProviderKind::Codex => {
                    let runtime =
                        RuntimeCredentialLoader::load(reference).map_err(|err| err.to_string())?;
                    let runtime_debug = format!("{runtime:?}");
                    let provider_auth = runtime.into_provider_auth();
                    let provider_auth_debug = format!("{provider_auth:?}");

                    println!("auth_loaded: true");
                    println!("runtime_credential: {runtime_debug}");
                    println!("provider_auth: {provider_auth_debug}");
                    println!("provider_auth_kind: {:?}", provider_auth.kind());
                }
                ProviderKind::Copilot => {
                    let runtime =
                        RuntimeCredentialLoader::load(reference).map_err(|err| err.to_string())?;
                    let runtime_debug = format!("{runtime:?}");
                    let provider_auth = runtime.into_provider_auth();
                    let provider_auth_debug = format!("{provider_auth:?}");
                    let provider = CopilotProvider::new().map_err(|err| err.to_string())?;

                    println!("auth_loaded: true");
                    println!("runtime_credential: {runtime_debug}");
                    println!("provider_auth: {provider_auth_debug}");
                    println!("provider_auth_kind: {:?}", provider_auth.kind());
                    match provider.list_models(&provider_auth).await {
                        Ok(models) => println!("model_count: {}", models.len()),
                        Err(error) => println!("auth_error: {error}"),
                    }
                }
            }
        }
        ProviderCommand::Models { provider } => {
            let provider = ProviderKind::from(provider);
            let detector = AuthDetector::from_env().map_err(|err| err.to_string())?;
            let credentials = detector.detect_all();
            let reference = credentials
                .iter()
                .find(|credential| credential.provider == provider)
                .ok_or_else(|| format!("no detected credential for {provider}"))?;
            let auth = provider_auth_for_kind(provider, reference)?;
            let models = provider_plugin_for_kind(
                provider,
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            )?
            .list_models(&auth)
            .await
            .map_err(|err| err.to_string())?;
            println!(
                "{}",
                serde_json::to_string(&models).map_err(|err| err.to_string())?
            );
        }
        ProviderCommand::Turn {
            provider,
            prompt,
            model,
        } => {
            let provider = ProviderKind::from(provider);
            let detector = AuthDetector::from_env().map_err(|err| err.to_string())?;
            let credentials = detector.detect_all();
            let reference = credentials
                .iter()
                .find(|credential| credential.provider == provider)
                .ok_or_else(|| format!("no detected credential for {provider}"))?;

            let provider_auth = provider_auth_for_kind(provider, reference)?;
            let session_id = SessionId::new();
            let request = TurnRequest {
                session_id,
                model: model.unwrap_or_else(|| default_provider_model(provider).to_string()),
                prompt,
                system_prompt: None,
                messages: Vec::new(),
                tool_names: Vec::new(),
                metadata: serde_json::Value::Null,
                images: Vec::new(),
            };
            let cancel = CancellationToken::new();
            // The smoke-test `provider turn` command auto-approves: it never
            // surfaces an interactive prompt, so its permission channel stays empty.
            let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel::<PermissionResponse>();
            let tool_rx = provider_core::empty_tool_responses();
            let plugin = provider_plugin_for_kind(
                provider,
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            )?;
            let mut stream = plugin
                .stream_turn(
                    &provider_auth,
                    request,
                    cancel,
                    perm_rx,
                    tool_rx,
                    provider_core::empty_question_responses(),
                    provider_core::empty_question_requests(),
                )
                .await
                .map_err(|err| err.to_string())?;

            while let Some(event) = stream.next().await {
                let event = event.map_err(|err| err.to_string())?;
                let line = serde_json::to_string(&event).map_err(|err| err.to_string())?;
                println!("{line}");
            }
        }
    }

    Ok(())
}

fn provider_auth_for_kind(
    provider: ProviderKind,
    reference: &auth::DetectedCredential,
) -> Result<ProviderAuth, String> {
    match provider {
        ProviderKind::Claude => Ok(ProviderAuth::new(
            ProviderAuthKind::SessionToken,
            SecretString::from(String::new()),
        )),
        ProviderKind::Codex | ProviderKind::Copilot => Ok(RuntimeCredentialLoader::load(reference)
            .map_err(|err| err.to_string())?
            .into_provider_auth()),
    }
}

fn provider_plugin_for_kind(
    provider: ProviderKind,
    cwd: PathBuf,
) -> Result<Box<dyn ProviderPlugin>, String> {
    match provider {
        ProviderKind::Claude => Ok(Box::new(ClaudeProvider::with_cwd(cwd))),
        ProviderKind::Codex => Ok(Box::new(
            CodexProvider::new().map_err(|err| err.to_string())?,
        )),
        ProviderKind::Copilot => Ok(Box::new(
            CopilotProvider::new().map_err(|err| err.to_string())?,
        )),
    }
}

async fn run_diff_command(command: DiffCommand) -> Result<(), String> {
    match command {
        DiffCommand::Show {
            repo,
            base,
            context_lines,
            summary,
        } => {
            let mut request = DiffRequest::new(repo, base);
            request.context_lines = context_lines;
            let diff = diff_worktree(&request).map_err(|err| err.to_string())?;

            if summary {
                println!("repo_root: {}", diff.repo_root.display());
                println!("base: {}", diff.base);
                println!("changed_files: {}", diff.changed_files());
                println!("added_lines: {}", diff.added_lines());
                println!("removed_lines: {}", diff.removed_lines());
                for file in &diff.files {
                    println!(
                        "{} {}",
                        format!("{:?}", file.status).to_lowercase(),
                        file.display_path().display()
                    );
                }
            } else {
                let line = serde_json::to_string(&diff).map_err(|err| err.to_string())?;
                println!("{line}");
            }
        }
    }

    Ok(())
}

async fn run_opentui_command(
    workspace: PathBuf,
    provider: ProviderArg,
    model: Option<String>,
    approval: ApprovalArg,
    workspace_only: bool,
) -> Result<(), String> {
    let repo_root = resolve_repo_root()?;
    let tui_dir = repo_root.join("packages").join("tui");
    if !tui_dir.join("src").join("index.tsx").exists() {
        return Err(format!(
            "OpenTUI frontend not found at {}",
            tui_dir.display()
        ));
    }
    ensure_opentui_dependencies(&repo_root, &tui_dir)?;

    let backend_bin = std::env::current_exe()
        .map_err(|err| format!("could not resolve current executable: {err}"))?;
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or(workspace));

    // The worktree registry lives in the app DB; share its path with the TUI so
    // the dashboard can list/archive worktrees the backend creates.
    let app_db = default_app_db_path()?;
    let registry = AppDb::open(&app_db).map_err(|err| err.to_string())?;
    let recovered = recover_orphaned_sessions(&registry)?;
    if recovered > 0 {
        eprintln!("Recovered {recovered} interrupted Inductor session(s) from a previous process.");
    }

    let mut command = std::process::Command::new("bun");
    command
        .arg("run")
        .arg("./src/index.tsx")
        .arg("--backend-bin")
        .arg(backend_bin)
        .arg("--workspace")
        .arg(workspace)
        .arg("--provider")
        .arg(provider.to_string())
        .arg("--approval")
        .arg(approval.to_string())
        .arg("--repo-root")
        .arg(repo_root)
        .arg("--app-db")
        .arg(app_db);
    if workspace_only {
        command.arg("--workspace-only");
    }
    command.current_dir(&tui_dir);

    if let Some(model) = model {
        command.arg("--model").arg(model);
    }

    // GH_REPO overrides GitHub CLI repository detection for all descendants.
    // If a user's shell exports it as a local path, `/pr` eventually fails with:
    // expected the "[HOST/]OWNER/REPO" format, got "/path/to/repo".
    // The PR helpers also scrub it before invoking `gh`, but clearing it at the
    // frontend boundary keeps any future/indirect `gh` calls from inheriting a
    // bad repository override.
    command.env_remove("GH_REPO");

    let status = command
        .status()
        .map_err(|err| format!("failed to launch OpenTUI frontend with bun: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("OpenTUI frontend exited with {status}"))
    }
}

fn ensure_opentui_dependencies(repo_root: &Path, tui_dir: &Path) -> Result<(), String> {
    if opentui_preload_exists(repo_root, tui_dir) {
        return Ok(());
    }

    eprintln!("OpenTUI dependencies are missing; running `bun install --frozen-lockfile`...");
    let status = std::process::Command::new("bun")
        .arg("install")
        .arg("--frozen-lockfile")
        .current_dir(repo_root)
        .status()
        .map_err(|err| format!("failed to run bun install for OpenTUI dependencies: {err}"))?;

    if !status.success() {
        return Err(format!(
            "OpenTUI dependency install failed with {status}; run `bun install` from {}",
            repo_root.display()
        ));
    }

    if opentui_preload_exists(repo_root, tui_dir) {
        Ok(())
    } else {
        Err(format!(
            "OpenTUI dependency install completed but @opentui/solid/preload is still missing; run `bun install` from {}",
            repo_root.display()
        ))
    }
}

fn opentui_preload_exists(repo_root: &Path, tui_dir: &Path) -> bool {
    let package_roots = [
        repo_root
            .join("node_modules")
            .join("@opentui")
            .join("solid"),
        repo_root
            .join("node_modules")
            .join(".bun")
            .join("node_modules")
            .join("@opentui")
            .join("solid"),
        tui_dir.join("node_modules").join("@opentui").join("solid"),
    ];

    package_roots.iter().any(|package_root| {
        [
            package_root.join("scripts").join("preload.ts"),
            package_root.join("scripts").join("preload.js"),
            package_root.join("preload.ts"),
            package_root.join("preload.js"),
        ]
        .iter()
        .any(|path| path.exists())
    })
}

/// Locate the inductor repository root that ships the OpenTUI frontend.
///
/// The binary is typically `cargo install`ed globally, so the compile-time
/// `CARGO_MANIFEST_DIR` points at whatever workspace happened to build it and
/// goes stale the moment you run from a different checkout. Instead we resolve
/// the root dynamically, preferring the workspace the user is actually running
/// in:
///   1. an explicit `INDUCTOR_REPO_ROOT` override,
///   2. the nearest ancestor of the current directory that contains
///      `packages/tui/src/index.tsx`,
///   3. the compile-time manifest dir as a last-resort fallback.
fn resolve_repo_root() -> Result<PathBuf, String> {
    fn has_tui(root: &Path) -> bool {
        root.join("packages")
            .join("tui")
            .join("src")
            .join("index.tsx")
            .exists()
    }

    if let Some(root) = std::env::var_os("INDUCTOR_REPO_ROOT") {
        return Ok(PathBuf::from(root));
    }

    if let Ok(cwd) = std::env::current_dir() {
        for ancestor in cwd.ancestors() {
            if has_tui(ancestor) {
                return Ok(ancestor.to_path_buf());
            }
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .map(Path::to_path_buf)
        .ok_or_else(|| "could not resolve repository root from Cargo manifest path".to_string())
}

async fn run_context_command(command: ContextCommand) -> Result<(), String> {
    match command {
        ContextCommand::Count { text } => {
            let counter = ApproxTokenCounter;
            println!("tokens: {}", counter.count_tokens(&text));
        }
        ContextCommand::Compact {
            text,
            soft_tokens,
            hard_tokens,
        } => {
            let messages = text
                .split("\n---\n")
                .enumerate()
                .map(|(index, part)| ContextMessage::new(format!("Message{index}"), part))
                .collect::<Vec<_>>();
            let counter = ApproxTokenCounter;
            let prepared = prepare_context(
                "system",
                &messages,
                &ContextLimits::new(
                    soft_tokens,
                    hard_tokens,
                    ContextLimits::default().tool_result_inline_bytes,
                ),
                &counter,
            )
            .map_err(|err| err.to_string())?;
            println!("tokens: {}", prepared.token_count);
            println!("compacted: {}", prepared.compacted);
            println!("{}", prepared.prompt);
            let compacted_count = compact_messages(&messages).len();
            println!("messages_after_compaction: {compacted_count}");
        }
        ContextCommand::Effort { provider, effort } => {
            let mapping =
                translate_effort(ProviderFamily::from(provider), ModelEffort::from(effort));
            let line = serde_json::to_string(&mapping).map_err(|err| err.to_string())?;
            println!("{line}");
        }
    }

    Ok(())
}

async fn run_skill_command(command: SkillCommand) -> Result<(), String> {
    match command {
        SkillCommand::List { workspace, json } => {
            let skills = discover_skills(&workspace)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&skills).map_err(|err| err.to_string())?
                );
            } else if skills.is_empty() {
                println!("no skills found");
            } else {
                for skill in skills {
                    println!(
                        "{}\t{}\t{}\t{}",
                        skill.name,
                        skill.source,
                        skill.path.display(),
                        skill.description
                    );
                }
            }
        }
        SkillCommand::Create {
            workspace,
            name,
            description,
            body,
            force,
            json,
        } => {
            let path = create_skill(&workspace, &name, &description, body.as_deref(), force)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&json!({ "name": name, "path": path }))
                        .map_err(|err| err.to_string())?
                );
            } else {
                println!("created skill: {}", path.display());
            }
        }
        SkillCommand::Use { workspace, skills } => {
            if let Some(layer) = compose_skill_prompt_layer(&workspace, &workspace, &skills)? {
                println!("{layer}");
            }
        }
    }

    Ok(())
}

fn create_skill(
    workspace: &Path,
    name: &str,
    description: &str,
    body: Option<&str>,
    force: bool,
) -> Result<PathBuf, String> {
    let slug = sanitize_skill_name(name)?;
    let dir = workspace.join(".agents").join("skills").join(&slug);
    let path = dir.join("SKILL.md");
    if path.exists() && !force {
        return Err(format!(
            "skill already exists: {} (pass --force to overwrite)",
            path.display()
        ));
    }
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    let body = body
        .unwrap_or("Describe when to use this skill and the exact steps the agent should follow.");
    let content = format!(
        "---\nname: {slug}\ndescription: {}\n---\n\n# {slug}\n\n{}\n",
        yaml_scalar(description),
        body.trim()
    );
    std::fs::write(&path, content).map_err(|err| err.to_string())?;
    Ok(path)
}

fn discover_skills(workspace: &Path) -> Result<Vec<SkillInfo>, String> {
    let mut roots = Vec::new();
    let repo_root = resolve_repo_root().ok();

    for root in workspace_skill_roots(workspace, repo_root.as_deref()) {
        roots.push((root.join(".agents").join("skills"), "repo".to_string()));
        roots.push((root.join(".claude").join("skills"), "claude-project".to_string()));
        roots.push((root.join(".github").join("skills"), "copilot-project".to_string()));
    }

    // Back-compatibility for skills created by the first Inductor skill MVP.
    roots.push((
        workspace.join(".inductor").join("skills"),
        "legacy-inductor".to_string(),
    ));
    roots.push((workspace.join("skills"), "legacy-workspace".to_string()));
    if let Some(repo_root) = &repo_root {
        roots.push((repo_root.join("skills"), "legacy-repo".to_string()));
    }

    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        roots.push((home.join(".agents").join("skills"), "user".to_string()));
        roots.push((home.join(".claude").join("skills"), "claude-user".to_string()));
        roots.push((home.join(".copilot").join("skills"), "copilot-user".to_string()));
        roots.push((home.join(".codex").join("skills"), "codex-user-legacy".to_string()));
        roots.push((
            home.join(".inductor").join("skills"),
            "legacy-inductor-user".to_string(),
        ));
    }

    roots.push((PathBuf::from("/etc/codex/skills"), "codex-admin".to_string()));

    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (root, source) in roots {
        read_skills_from_root(&root, &source, &mut seen, &mut skills)?;
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    Ok(skills)
}

fn workspace_skill_roots(workspace: &Path, repo_root: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut current = workspace.to_path_buf();
    loop {
        roots.push(current.clone());
        if repo_root.is_some_and(|root| current == root) {
            break;
        }
        if !current.pop() {
            break;
        }
    }
    roots
}

fn read_skills_from_root(
    root: &Path,
    source: &str,
    seen: &mut std::collections::HashSet<PathBuf>,
    skills: &mut Vec<SkillInfo>,
) -> Result<(), String> {
    if !root.is_dir() {
        return Ok(());
    }
    let entries = std::fs::read_dir(root).map_err(|err| err.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let skill_path = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
            path
        } else {
            continue;
        };
        let seen_key = skill_path
            .canonicalize()
            .unwrap_or_else(|_| skill_path.clone());
        if !skill_path.is_file() || !seen.insert(seen_key) {
            continue;
        }
        if let Some(skill) = read_skill_info(&skill_path, source) {
            skills.push(skill);
        }
    }
    Ok(())
}

fn compose_skill_prompt_layer(
    workspace: &Path,
    source_workspace: &Path,
    requested: &[String],
) -> Result<Option<String>, String> {
    let catalog = discover_skills(source_workspace)?;
    if catalog.is_empty() {
        return Ok(None);
    }

    let catalog_rows = catalog
        .iter()
        .map(|skill| {
            format!(
                "- ${}: {} (source: {}; path: {})",
                skill.name,
                skill.description,
                skill.source,
                skill.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut sections = Vec::new();
    for request in requested {
        let path = resolve_skill_request(workspace, source_workspace, &catalog, request)?;
        let content = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
        let name = skill_name_from_path(&path, &content);
        sections.push(format!(
            "## Preloaded skill: {name}\nSource: {}\n\n{}",
            path.display(),
            content.trim()
        ));
    }

    let preloaded = if sections.is_empty() {
        "No skill has been explicitly preloaded for this turn. Use the catalog above to choose skills yourself.".to_string()
    } else {
        sections.join("\n\n---\n\n")
    };

    Ok(Some(format!(
        "# Skills\n\nSkills are provider-standard SKILL.md capability packages from Codex, Claude, Copilot, and compatible locations. All discovered skill names, descriptions, and paths are listed below so you can choose the right skill without the user tagging it.\n\nBe proactive: whenever a task matches a skill description, invoke that skill by reading its SKILL.md at the listed path, then follow the skill instructions. Do not wait for the user to tag the skill. If the user explicitly mentions a $skill, prefer that skill. If a skill conflicts with higher-priority system or developer instructions, follow the higher-priority instruction.\n\n## Available skills\n\n{}\n\n## Explicitly tagged/preloaded skills\n\n{}",
        catalog_rows,
        preloaded
    )))
}

fn resolve_skill_request(
    workspace: &Path,
    source_workspace: &Path,
    catalog: &[SkillInfo],
    request: &str,
) -> Result<PathBuf, String> {
    let request = request.strip_prefix('$').unwrap_or(request);
    let requested_path = PathBuf::from(request);
    if requested_path.components().count() > 1 || request.ends_with(".md") {
        let path = if requested_path.is_absolute() {
            requested_path
        } else {
            workspace.join(&requested_path)
        };
        let path = if path.is_dir() {
            path.join("SKILL.md")
        } else {
            path
        };
        if path.is_file() {
            return Ok(path);
        }
        let source_path = if PathBuf::from(request).is_absolute() {
            PathBuf::from(request)
        } else {
            source_workspace.join(request)
        };
        let source_path = if source_path.is_dir() {
            source_path.join("SKILL.md")
        } else {
            source_path
        };
        if source_path.is_file() {
            return Ok(source_path);
        }
    }
    catalog
        .iter()
        .find(|skill| skill.name == request)
        .map(|skill| skill.path.clone())
        .ok_or_else(|| format!("skill not found: {request}"))
}

fn read_skill_info(path: &Path, source: &str) -> Option<SkillInfo> {
    let content = std::fs::read_to_string(path).ok()?;
    let (frontmatter, _) = split_frontmatter(&content);
    let parsed = frontmatter.and_then(|raw| serde_yaml::from_str::<SkillFrontMatter>(raw).ok());
    let fallback_name = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("skill")
        .to_string();
    Some(SkillInfo {
        name: parsed
            .as_ref()
            .and_then(|meta| meta.name.clone())
            .unwrap_or(fallback_name),
        description: parsed.and_then(|meta| meta.description).unwrap_or_default(),
        path: path.to_path_buf(),
        source: source.to_string(),
    })
}

fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---\n") else {
        return (None, content);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, content);
    };
    (Some(&rest[..end]), &rest[end + 4..])
}

fn skill_name_from_path(path: &Path, content: &str) -> String {
    let (frontmatter, _) = split_frontmatter(content);
    frontmatter
        .and_then(|raw| serde_yaml::from_str::<SkillFrontMatter>(raw).ok())
        .and_then(|meta| meta.name)
        .or_else(|| {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "skill".to_string())
}

fn sanitize_skill_name(name: &str) -> Result<String, String> {
    let slug = name
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if slug.is_empty() || slug == "." || slug == ".." || slug.contains("..") {
        return Err(format!("invalid skill name: {name}"));
    }
    Ok(slug)
}

fn yaml_scalar(value: &str) -> String {
    serde_yaml::to_string(value)
        .unwrap_or_else(|_| format!("\"{}\"", value.replace('"', "\\\"")))
        .trim()
        .trim_start_matches("---")
        .trim()
        .to_string()
}

async fn run_db_command(command: DbCommand) -> Result<(), String> {
    match command {
        DbCommand::InitApp { path } => {
            let db = AppDb::open(&path).map_err(|err| err.to_string())?;
            println!("app_db: {}", path.display());
            println!(
                "schema_version: {}",
                db.schema_version().map_err(|err| err.to_string())?
            );
        }
        DbCommand::InitWorkspace {
            workspace,
            state_db,
        } => {
            let path = state_db.unwrap_or_else(|| workspace_state_path(&workspace));
            let db = WorkspaceDb::open(&path).map_err(|err| err.to_string())?;
            println!("workspace_db: {}", path.display());
            println!(
                "schema_version: {}",
                db.schema_version().map_err(|err| err.to_string())?
            );
        }
        DbCommand::Sessions {
            workspace,
            state_db,
            json: json_output,
        } => {
            let path = state_db.unwrap_or_else(|| workspace_state_path(&workspace));
            let db = WorkspaceDb::open(&path).map_err(|err| err.to_string())?;
            let sessions = db.list_sessions().map_err(|err| err.to_string())?;
            if json_output {
                let rows = sessions
                    .iter()
                    .map(|session| {
                        let preview = db
                            .messages(session.id)
                            .ok()
                            .and_then(|messages| {
                                messages
                                    .into_iter()
                                    .find(|message| message.role.eq_ignore_ascii_case("user"))
                                    .map(|message| message.content)
                            })
                            .unwrap_or_default();
                        json!({
                            "id": session.id,
                            "provider": session.provider_id.0,
                            "model": session.model,
                            "status": session.status,
                            "display_name": session.display_name,
                            "created_at": session.created_at,
                            "updated_at": session.updated_at,
                            "preview": preview,
                        })
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string(&rows).map_err(|err| err.to_string())?
                );
                return Ok(());
            }
            for session in sessions {
                println!(
                    "{} provider={} model={} status={:?} updated_at={}",
                    session.id,
                    session.provider_id.0,
                    session.model,
                    session.status,
                    session.updated_at
                );
            }
        }
        DbCommand::ShowSession {
            workspace,
            state_db,
            session_id,
            json: json_output,
        } => {
            let path = state_db.unwrap_or_else(|| workspace_state_path(&workspace));
            let db = WorkspaceDb::open(&path).map_err(|err| err.to_string())?;
            let session = db
                .get_session(session_id)
                .map_err(|err| err.to_string())?
                .ok_or_else(|| format!("session not found: {session_id}"))?;
            let messages = db.messages(session_id).map_err(|err| err.to_string())?;
            let events = db.events(session_id).map_err(|err| err.to_string())?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "session": session,
                        "messages": messages,
                        "events": events,
                    }))
                    .map_err(|err| err.to_string())?
                );
                return Ok(());
            }
            println!(
                "session: {} provider={} model={} status={:?}",
                session.id, session.provider_id.0, session.model, session.status
            );
            for message in messages {
                println!(
                    "[{} #{}]\n{}",
                    message.role, message.ordinal, message.content
                );
            }
        }
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_harness_command(
    provider: ProviderArg,
    workspace: PathBuf,
    prompt: String,
    model: Option<String>,
    mode: ModeArg,
    slug: Option<String>,
    max_tool_rounds: usize,
    approval: ApprovalArg,
    yes: bool,
    workspace_only: bool,
    soft_tokens: usize,
    hard_tokens: usize,
    tool_result_inline_bytes: usize,
    blob_root: Option<PathBuf>,
    effort: EffortArg,
    model_role: ModelRoleArg,
    skills: Vec<String>,
    app_db: Option<PathBuf>,
    state_db: Option<PathBuf>,
    requested_session_id: Option<SessionId>,
    requested_workspace_id: Option<WorkspaceId>,
) -> Result<(), String> {
    let provider = ProviderKind::from(provider);
    let provider_id = ProviderId(provider.to_string());

    // Resolve development mode. In worktree mode the agent runs inside an
    // isolated git worktree (created off `workspace`, or reused when resuming
    // a session), so parallel sessions never touch each other's files. The
    // worktree registry lives in the app DB, so worktree mode needs one — fall
    // back to a default path when the caller didn't supply `--app-db`.
    let mut workspace = workspace;
    // Remember the workspace Inductor was opened in before it is swapped for a
    // worktree path below. Pasted/dropped images are written by the TUI into
    // this source workspace's `.inductor/attachments/`, which is untracked and
    // therefore absent from a freshly created worktree — keep it so prompt
    // image mentions can be sourced from it.
    let source_workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());
    let mut memory_source_workspace = source_workspace.clone();
    let mut app_db = app_db;
    let mut forced_workspace_id = requested_workspace_id;
    let mut forced_state_db: Option<PathBuf> = None;
    let mut worktree_rename_candidate: Option<git::ManagedWorktree> = None;
    if mode == ModeArg::Worktree {
        if app_db.is_none() {
            app_db = Some(default_app_db_path()?);
        }
        let registry = AppDb::open(app_db.as_ref().unwrap()).map_err(|err| err.to_string())?;

        // A worktree may already exist for this turn: either the resumed
        // session is bound to one, or the TUI pre-created one the moment the
        // user opened the session (passed via --workspace-id). Otherwise this
        // is a brand-new session that needs a fresh worktree.
        let bound_worktree = requested_session_id
            .and_then(|sid| registry.get_session(sid).ok().flatten())
            .and_then(|session| registry.get_worktree(session.workspace_id).ok().flatten())
            .or_else(|| {
                requested_workspace_id.and_then(|wid| registry.get_worktree(wid).ok().flatten())
            });

        let binding = if let Some(worktree) = bound_worktree {
            // Resuming a session that already owns a worktree — reuse it as-is.
            WorktreeBinding {
                workspace_id: worktree.id,
                source_repo: worktree.source_repo,
                worktree_path: worktree.worktree_path,
                created_worktree: None,
            }
        } else {
            // Fresh session: create a placeholder worktree immediately. After
            // the first user prompt is persisted, a silent provider call renames
            // the session, branch, and directory in-place and emits a metadata
            // event so the TUI updates without restart.
            create_worktree_binding(&registry, &workspace, slug.as_deref())?
        };
        workspace = binding.worktree_path.clone();
        memory_source_workspace = binding.source_repo.clone();
        forced_workspace_id = Some(binding.workspace_id);
        worktree_rename_candidate = binding.created_worktree;
        // Keep the session's state.db outside the worktree so archiving
        // (which deletes the worktree dir) preserves the chats.
        forced_state_db = Some(worktree_state_db_path(binding.workspace_id)?);
    }

    // Resolve auth the same way `provider turn` does. Claude auth is handled
    // by the Claude Agent SDK, so we pass an empty placeholder secret.
    let detector = AuthDetector::from_env().map_err(|err| err.to_string())?;
    let credentials = detector.detect_all();
    let reference = credentials
        .iter()
        .find(|credential| credential.provider == provider)
        .ok_or_else(|| format!("no detected credential for {provider}"))?;
    let provider_auth = match provider {
        ProviderKind::Claude => ProviderAuth::new(
            ProviderAuthKind::SessionToken,
            SecretString::from(String::new()),
        ),
        ProviderKind::Codex => RuntimeCredentialLoader::load(reference)
            .map_err(|err| err.to_string())?
            .into_provider_auth(),
        ProviderKind::Copilot => RuntimeCredentialLoader::load(reference)
            .map_err(|err| err.to_string())?
            .into_provider_auth(),
    };

    // Yolo mode is the default: file tools may read/write outside the
    // workspace and bash runs without the macOS workspace sandbox. Users can
    // opt back into workspace-only execution with `--workspace-only`.
    //
    // For a fresh worktree-mode session this is the *placeholder* worktree
    // (slug `session`). The silent naming pass below may `git worktree move`
    // it to a descriptive path, so `workspace_path`/`tools`/`provider_plugin`
    // are rebound afterwards to the renamed directory.
    let mut workspace_path = workspace.clone();

    // Anchor the process working directory to the resolved workspace (the
    // worktree in worktree mode). The TUI launches this backend with its cwd
    // at the repo root, but every tool, the environment block we show the
    // model, and any provider that falls back to `current_dir()` should treat
    // the worktree as home — otherwise agents drift into editing the original
    // checkout. This is the *preferred* cwd, not a restriction: bash and file
    // tools can still reach outside in the default unrestricted mode. Resolve
    // to an absolute path first so changing cwd never disturbs the relative
    // path math that follows.
    workspace_path = workspace_path
        .canonicalize()
        .unwrap_or_else(|_| workspace_path.clone());
    set_process_cwd(&workspace_path);

    // Build the provider as a trait object so the harness loop can drive
    // either backend through `&dyn ProviderPlugin`. Claude's SDK also needs
    // its cwd set to the resolved workspace/worktree, otherwise its built-in
    // environment context and tools point at the source checkout.
    let mut provider_plugin: Box<dyn ProviderPlugin> = match provider {
        ProviderKind::Claude => Box::new(ClaudeProvider::with_cwd(workspace_path.clone())),
        ProviderKind::Codex => Box::new(CodexProvider::new().map_err(|err| err.to_string())?),
        ProviderKind::Copilot => Box::new(CopilotProvider::new().map_err(|err| err.to_string())?),
    };

    let memory_file = repo_memory_file(&memory_source_workspace, &workspace_path)
        .unwrap_or_else(|| workspace_path.join(".inductor").join("memory.md"));
    ensure_repo_memory_file(&memory_file).map_err(|err| err.to_string())?;

    let mut tools = (if workspace_only {
        ToolRuntime::sandboxed(workspace_path.clone())
    } else {
        ToolRuntime::unrestricted(workspace_path.clone())
    })
    .map_err(|err| err.to_string())?
    .with_memory_file(memory_file.clone());

    let state_db_path = state_db
        .or(forced_state_db)
        .unwrap_or_else(|| workspace_state_path(&workspace_path));
    let blob_root = blob_root.or_else(|| default_blob_root(&state_db_path));
    let workspace_db = WorkspaceDb::open(&state_db_path).map_err(|err| err.to_string())?;
    let model = model.unwrap_or_else(|| default_provider_model(provider).to_string());
    let session_id = requested_session_id.unwrap_or_else(SessionId::new);
    let existing_session = workspace_db
        .get_session(session_id)
        .map_err(|err| err.to_string())?;
    let workspace_id = existing_session
        .as_ref()
        .map(|session| session.workspace_id)
        .or(forced_workspace_id)
        .unwrap_or_else(WorkspaceId::new);
    let mut session_record = existing_session.unwrap_or_else(|| {
        new_session_record(session_id, workspace_id, provider_id.clone(), model.clone())
            .expect("current UTC time should format as RFC3339")
    });
    session_record.provider_id = provider_id.clone();
    session_record.model = model.clone();
    session_record.status = SessionStatus::Starting;
    session_record.updated_at = now_rfc3339().map_err(|err| err.to_string())?;
    workspace_db
        .upsert_session(&session_record)
        .map_err(|err| err.to_string())?;

    // Held open across the run so live status transitions (below) can be written
    // back to the dashboard database without reopening the connection each time.
    let live_app_db = match app_db {
        Some(ref app_db_path) => {
            let app_db = AppDb::open(app_db_path).map_err(|err| err.to_string())?;
            app_db
                .upsert_workspace(
                    workspace_id,
                    &workspace_path,
                    workspace_path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("workspace"),
                )
                .map_err(|err| err.to_string())?;
            app_db
                .upsert_session(&session_record)
                .map_err(|err| err.to_string())?;
            Some(app_db)
        }
        None => None,
    };

    let loaded_messages = workspace_db
        .messages(session_id)
        .map_err(|err| err.to_string())?
        .into_iter()
        .map(stored_message_to_transcript)
        .collect::<Result<Vec<_>, _>>()?;
    let mut state = if loaded_messages.is_empty() {
        SessionState::new(session_id)
    } else {
        SessionState::with_transcript(session_id, loaded_messages)
    };
    let should_silently_name = state.transcript.is_empty() && session_record.display_name.is_none();

    let mut config = HarnessConfig::new(model.clone());
    config.max_tool_rounds = max_tool_rounds;
    config.approval_policy = ApprovalPolicy::from(approval);
    config.context.limits = ContextLimits::new(soft_tokens, hard_tokens, tool_result_inline_bytes);
    config.context.blob_root = blob_root;
    config.model_effort = ModelEffort::from(effort);
    config.model_role = ModelRole::from(model_role);
    config.provider_family = match provider {
        ProviderKind::Claude => ProviderFamily::Claude,
        ProviderKind::Codex => ProviderFamily::Codex,
        ProviderKind::Copilot => ProviderFamily::Copilot,
    };
    if let Some(layer) = compose_skill_prompt_layer(&workspace_path, &source_workspace, &skills)? {
        config.prompt.system_layers.push(layer);
    }
    if let Some(layer) = repo_memory_prompt_layer(&memory_file)? {
        config.prompt.system_layers.push(layer);
    }
    let cancel = CancellationToken::new();
    let cancel_on_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel_on_signal.cancel();
        }
    });

    let mut allow = AllowStore::new();
    // Two tool paths need decisions: Claude executes tools in its SDK and gates
    // them via the provider permission channel (`perm_tx`/`perm_rx`); the harness
    // text-tool path (Codex) gates via the `Approver`. `--yes` auto-approves both.
    // A single stdin reader fans each TUI decision out to both — only the active
    // path has a pending request, so the other simply ignores it.
    let auto = AutoApprove;
    let channel_approver = ChannelApprover::new();
    let (perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel::<PermissionResponse>();
    let (question_tx, question_rx) = tokio::sync::mpsc::unbounded_channel::<QuestionResponse>();
    let (question_request_tx, mut question_request_rx) =
        tokio::sync::mpsc::unbounded_channel::<provider_core::ProviderQuestionRequest>();
    if !yes {
        let approver_tx = channel_approver.sender();
        let provider_tx = perm_tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(resp) = parse_permission_decision(line) {
                    let _ = provider_tx.send(resp.clone());
                    if approver_tx.send(resp).is_err() {
                        break;
                    }
                    continue;
                }
                if let Some(resp) = parse_question_response(line) {
                    let _ = question_tx.send(resp);
                }
            }
        });
    }
    let approver: &dyn Approver = if yes { &auto } else { &channel_approver };

    let approval_policy_dbg = config.approval_policy;
    let prompt = attach_prompt_image_mentions(&workspace_path, &source_workspace, &prompt);
    persist_submitted_user_message(&workspace_db, session_id, &state.transcript, &prompt)
        .map_err(|err| err.to_string())?;

    if should_silently_name {
        if let Some(event) = silently_name_session_and_worktree(
            provider,
            &model,
            &workspace_path,
            &prompt,
            &mut session_record,
            &workspace_db,
            live_app_db.as_ref(),
            worktree_rename_candidate.as_ref(),
        )
        .await
        {
            // The silent rename may have `git worktree move`d the placeholder
            // directory out from under us. The tool runtime and Claude's cwd
            // cached the old path, so rebind them to the new one — otherwise
            // every bash/read/list/grep runs in a directory that no longer
            // exists and fails with ENOENT.
            if let SessionEvent::MetadataUpdated {
                worktree_path: Some(renamed),
                ..
            } = &event
            {
                let renamed = PathBuf::from(renamed);
                if renamed != workspace_path {
                    workspace_path = renamed;
                    // The directory the process sits in was just moved on disk;
                    // follow it so cwd-relative work keeps landing in the
                    // worktree rather than the repo root.
                    set_process_cwd(&workspace_path);
                    tools = (if workspace_only {
                        ToolRuntime::sandboxed(workspace_path.clone())
                    } else {
                        ToolRuntime::unrestricted(workspace_path.clone())
                    })
                    .map_err(|err| err.to_string())?
                    .with_memory_file(memory_file.clone());
                    if matches!(provider, ProviderKind::Claude) {
                        provider_plugin =
                            Box::new(ClaudeProvider::with_cwd(workspace_path.clone()));
                    }
                }
            }
            persist_event(&workspace_db, &event).map_err(|err| err.to_string())?;
            println!(
                "{}",
                serde_json::to_string(&event).map_err(|err| err.to_string())?
            );
        }
    }

    let mut stream = run_turn(
        provider_plugin.as_ref(),
        &provider_auth,
        &tools,
        approver,
        &mut allow,
        &mut state,
        prompt,
        config,
        cancel,
        perm_rx,
        question_rx,
        question_request_tx,
    );

    dlog(&format!(
        "run start: provider={} approval={:?} yes={yes}",
        provider_id.0, approval_policy_dbg
    ));

    let mut final_status = SessionStatus::Completed;
    let mut saw_terminal_result = false;
    loop {
        let next_event = tokio::select! {
            request = question_request_rx.recv() => {
                if let Some(request) = request {
                    let event = SessionEvent::QuestionsRequested {
                        session_id,
                        tool_call_id: request.tool_call_id,
                        questions: request.questions,
                    };
                    persist_event(&workspace_db, &event).map_err(|err| err.to_string())?;
                    println!(
                        "{}",
                        serde_json::to_string(&event).map_err(|err| err.to_string())?
                    );
                }
                continue;
            }
            event = stream.next() => event,
        };
        let Some(event) = next_event else { break };
        let event = match event {
            Ok(event) => event,
            Err(err) => {
                let message = format!("agent stream failed: {err}");
                dlog(&message);
                let error_event = SessionEvent::Error {
                    session_id,
                    message,
                };
                persist_event(&workspace_db, &error_event).map_err(|err| err.to_string())?;
                println!(
                    "{}",
                    serde_json::to_string(&error_event).map_err(|err| err.to_string())?
                );
                let result_event = SessionEvent::Result {
                    session_id,
                    stop_reason: StopReason::Error,
                };
                persist_event(&workspace_db, &result_event).map_err(|err| err.to_string())?;
                println!(
                    "{}",
                    serde_json::to_string(&result_event).map_err(|err| err.to_string())?
                );
                final_status = SessionStatus::Failed;
                saw_terminal_result = true;
                break;
            }
        };
        match &event {
            SessionEvent::PermissionRequest {
                tool_name,
                request_id,
                ..
            } => {
                dlog(&format!(
                    "emit PermissionRequest tool={tool_name} id={request_id}"
                ));
                // Auto-approve mode: immediately allow the provider's request.
                if yes {
                    let _ = perm_tx.send(PermissionResponse {
                        request_id: *request_id,
                        decision: PermissionDecision::Allow,
                        message: None,
                    });
                }
            }
            SessionEvent::ToolCallStart { name, .. } => dlog(&format!("tool start: {name}")),
            SessionEvent::ToolCallResult { exit_code, .. } => {
                dlog(&format!("tool result: exit={exit_code:?}"))
            }
            SessionEvent::ToolCallError { message, .. } => dlog(&format!("tool error: {message}")),
            _ => {}
        }
        if matches!(
            event,
            SessionEvent::Result {
                stop_reason: StopReason::Error,
                ..
            } | SessionEvent::Error { .. }
        ) {
            final_status = SessionStatus::Failed;
        }
        if matches!(event, SessionEvent::Result { .. }) {
            saw_terminal_result = true;
        }
        if matches!(
            event,
            SessionEvent::Result {
                stop_reason: StopReason::Interrupted,
                ..
            }
        ) {
            final_status = SessionStatus::Idle;
        }
        // Mirror live status transitions onto the session record. The dashboard
        // reads `sessions.status`, which is otherwise only written at start
        // (`starting`) and after the stream fully ends. Without this, an active
        // run shows `starting` the whole time, and a run whose process is killed
        // mid-stream is stranded at `starting` forever, looking permanently hung.
        if let SessionEvent::Status { status, .. } = &event {
            if let Err(err) = workspace_db.set_session_status(session_id, *status) {
                dlog(&format!("set_session_status (workspace) failed: {err}"));
            }
            if let Some(ref db) = live_app_db {
                if let Err(err) = db.set_session_status(session_id, *status) {
                    dlog(&format!("set_session_status (app) failed: {err}"));
                }
            }
        }
        persist_event(&workspace_db, &event).map_err(|err| err.to_string())?;
        let line = serde_json::to_string(&event).map_err(|err| err.to_string())?;
        println!("{line}");
    }
    drop(stream);

    if !saw_terminal_result {
        let message = "agent stream ended without a terminal result".to_string();
        dlog(&message);
        let error_event = SessionEvent::Error {
            session_id,
            message,
        };
        persist_event(&workspace_db, &error_event).map_err(|err| err.to_string())?;
        println!(
            "{}",
            serde_json::to_string(&error_event).map_err(|err| err.to_string())?
        );
        let result_event = SessionEvent::Result {
            session_id,
            stop_reason: StopReason::Error,
        };
        persist_event(&workspace_db, &result_event).map_err(|err| err.to_string())?;
        println!(
            "{}",
            serde_json::to_string(&result_event).map_err(|err| err.to_string())?
        );
        final_status = SessionStatus::Failed;
    }

    let stored_messages = state
        .transcript
        .iter()
        .enumerate()
        .map(|(index, message)| {
            StoredMessage::new(message.role.label(), message.content.clone(), index as i64)
        })
        .collect::<Vec<_>>();
    workspace_db
        .replace_messages(session_id, &stored_messages)
        .map_err(|err| err.to_string())?;

    // Generate session name if not already set and we have user messages
    if session_record.display_name.is_none() && final_status == SessionStatus::Completed {
        let user_prompts: Vec<String> = state
            .transcript
            .iter()
            .filter(|msg| msg.role == harness_runtime::Role::User)
            .take(2) // Take first 2 user prompts for naming
            .map(|msg| msg.content.clone())
            .collect();

        if !user_prompts.is_empty() {
            match generate_session_name(&user_prompts, Some(SessionNamingConfig::default())).await {
                Ok(name) => {
                    session_record.display_name = Some(name);
                }
                Err(err) => {
                    eprintln!("Failed to generate session name: {err}");
                    // Continue without naming - not a critical failure
                }
            }
        }
    }

    session_record.status = final_status;
    session_record.updated_at = now_rfc3339().map_err(|err| err.to_string())?;
    workspace_db
        .upsert_session(&session_record)
        .map_err(|err| err.to_string())?;

    // Update app database if it exists, reusing the connection held open across
    // the run for live status updates.
    if let Some(ref app_db_conn) = live_app_db {
        app_db_conn
            .upsert_session(&session_record)
            .map_err(|err| err.to_string())?;
    }

    Ok(())
}

async fn silently_name_session_and_worktree(
    provider: ProviderKind,
    model: &str,
    workspace_path: &Path,
    prompt: &str,
    session_record: &mut persistence::SessionRecord,
    workspace_db: &WorkspaceDb,
    app_db: Option<&AppDb>,
    worktree: Option<&git::ManagedWorktree>,
) -> Option<SessionEvent> {
    let context = naming_context(prompt, workspace_path, worktree);
    let config = SessionNamingConfig {
        provider,
        model: model.to_string(),
        enabled: true,
        cwd: Some(workspace_path.to_path_buf()),
    };
    let mut name = match generate_context_name(&context, Some(config)).await {
        Ok(name) if name != "New Session" && !name.trim().is_empty() => name,
        _ => fallback_worktree_name(prompt)?,
    };
    name = name.trim().to_string();

    session_record.display_name = Some(name.clone());
    session_record.updated_at = now_rfc3339().ok()?;
    if let Err(err) = workspace_db.upsert_session(session_record) {
        dlog(&format!("silent session name update failed: {err}"));
        return None;
    }
    if let Some(db) = app_db {
        if let Err(err) = db.upsert_session(session_record) {
            dlog(&format!("silent app session name update failed: {err}"));
        }
    }

    if let Some(db) = app_db {
        if let Err(err) = db.upsert_workspace(session_record.workspace_id, workspace_path, &name) {
            dlog(&format!("silent workspace name update failed: {err}"));
        }
    }

    let mut workspace_id = None;
    let mut worktree_path = None;
    let mut branch_name = None;
    if let (Some(db), Some(worktree)) = (app_db, worktree) {
        let manager = WorktreeManager::new(default_managed_root().ok()?);
        match manager.rename_managed_worktree(worktree, &name) {
            Ok(renamed) => {
                if let Err(err) = register_worktree(db, &renamed) {
                    dlog(&format!("silent worktree registry update failed: {err}"));
                }
                workspace_id = Some(renamed.workspace_id);
                worktree_path = Some(renamed.worktree_path.display().to_string());
                branch_name = Some(renamed.branch_name.clone());
                session_record.workspace_id = renamed.workspace_id;
                if let Err(err) =
                    db.upsert_workspace(renamed.workspace_id, &renamed.worktree_path, &name)
                {
                    dlog(&format!("silent workspace registry update failed: {err}"));
                }
                if let Err(err) = db.upsert_session(session_record) {
                    dlog(&format!("silent app session rebind failed: {err}"));
                }
            }
            Err(err) => dlog(&format!("silent worktree rename failed: {err}")),
        }
    }

    Some(SessionEvent::MetadataUpdated {
        session_id: session_record.id,
        display_name: Some(name),
        workspace_id,
        worktree_path,
        branch_name,
    })
}

fn naming_context(
    prompt: &str,
    workspace_path: &Path,
    worktree: Option<&git::ManagedWorktree>,
) -> String {
    let mut context = String::new();
    context.push_str("User first prompt:\n");
    context.push_str(&prompt_transcript_text(prompt));
    context.push_str("\n\nWorkspace path:\n");
    context.push_str(&workspace_path.display().to_string());
    if let Some(worktree) = worktree {
        context.push_str("\n\nCurrent placeholder branch:\n");
        context.push_str(&worktree.branch_name);
        context.push_str("\nCurrent placeholder worktree path:\n");
        context.push_str(&worktree.worktree_path.display().to_string());
        context.push_str("\nBase branch:\n");
        context.push_str(&worktree.base_branch);
    }
    context
}

fn stored_message_to_transcript(message: StoredMessage) -> Result<TranscriptMessage, String> {
    let role = message
        .role
        .parse::<Role>()
        .map_err(|err| err.to_string())?;
    Ok(TranscriptMessage::new(role, message.content))
}

fn persist_submitted_user_message(
    db: &WorkspaceDb,
    session_id: SessionId,
    transcript: &[TranscriptMessage],
    prompt: &str,
) -> persistence::Result<()> {
    let text = prompt_transcript_text(prompt);
    if text.trim().is_empty() {
        return Ok(());
    }

    let event = SessionEvent::UserMessage {
        session_id,
        text: text.clone(),
    };
    db.append_event(session_id, &event)?;

    let mut messages = transcript
        .iter()
        .enumerate()
        .map(|(index, message)| {
            StoredMessage::new(message.role.label(), message.content.clone(), index as i64)
        })
        .collect::<Vec<_>>();
    messages.push(StoredMessage::new(
        Role::User.label(),
        text,
        messages.len() as i64,
    ));
    db.replace_messages(session_id, &messages)?;
    Ok(())
}

fn prompt_transcript_text(prompt: &str) -> String {
    const PREFIX: &str = "__MULTIMODAL_MESSAGE__:";
    prompt
        .strip_prefix(PREFIX)
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .and_then(|value| value["text"].as_str().map(str::to_string))
        .unwrap_or_else(|| prompt.to_string())
}

fn persist_event(db: &WorkspaceDb, event: &SessionEvent) -> persistence::Result<()> {
    match event {
        SessionEvent::Status { session_id, .. }
        | SessionEvent::UserMessage { session_id, .. }
        | SessionEvent::TextDelta { session_id, .. }
        | SessionEvent::TextStart { session_id, .. }
        | SessionEvent::TextEnd { session_id, .. }
        | SessionEvent::ReasoningStart { session_id, .. }
        | SessionEvent::ReasoningDelta { session_id, .. }
        | SessionEvent::ReasoningEnd { session_id, .. }
        | SessionEvent::ContextPrepared { session_id, .. }
        | SessionEvent::ModelRoleChanged { session_id, .. }
        | SessionEvent::StepStart { session_id, .. }
        | SessionEvent::StepFinish { session_id, .. }
        | SessionEvent::ToolCallStart { session_id, .. }
        | SessionEvent::ToolInputStart { session_id, .. }
        | SessionEvent::ToolInputDelta { session_id, .. }
        | SessionEvent::ToolInputEnd { session_id, .. }
        | SessionEvent::ToolCallRequested { session_id, .. }
        | SessionEvent::ToolCallProgress { session_id, .. }
        | SessionEvent::ToolCallResult { session_id, .. }
        | SessionEvent::ToolCallError { session_id, .. }
        | SessionEvent::Patch { session_id, .. }
        | SessionEvent::Diagnostics { session_id, .. }
        | SessionEvent::QuestionsRequested { session_id, .. }
        | SessionEvent::QuestionsAnswered { session_id, .. }
        | SessionEvent::PermissionRequest { session_id, .. }
        | SessionEvent::PermissionResolved { session_id, .. }
        | SessionEvent::TerminalOutput { session_id, .. }
        | SessionEvent::SkillUsed { session_id, .. }
        | SessionEvent::Result { session_id, .. }
        | SessionEvent::Usage { session_id, .. }
        | SessionEvent::MetadataUpdated { session_id, .. }
        | SessionEvent::Error { session_id, .. } => {
            db.append_event(*session_id, event)?;
        }
        SessionEvent::Unknown => {}
    }

    match event {
        SessionEvent::ToolCallStart {
            session_id,
            tool_call_id,
            name,
            input_json,
        } => {
            db.upsert_tool_call(&ToolCallRecord {
                id: tool_call_id.to_string(),
                session_id: *session_id,
                name: name.clone(),
                input_json: input_json.clone(),
                status: "started".to_string(),
            })?;
        }
        SessionEvent::ToolCallResult {
            tool_call_id,
            output,
            exit_code,
            ..
        } => {
            db.add_tool_result(&ToolResultRecord {
                tool_call_id: tool_call_id.to_string(),
                output: output.clone(),
                exit_code: *exit_code,
                blob_id: None,
            })?;
        }
        SessionEvent::ToolCallError { tool_call_id, .. } => {
            db.mark_tool_failed(&tool_call_id.to_string())?;
        }
        _ => {}
    }

    Ok(())
}

/// Channel-driven approver: each tool call awaits the next decision delivered on
/// a channel (fed by a stdin reader that parses the TUI's JSON decisions). Tool
/// calls are gated serially, so a single ordered channel matches them in order.
struct ChannelApprover {
    tx: tokio::sync::mpsc::UnboundedSender<PermissionResponse>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<PermissionResponse>>,
    last_message: std::sync::Mutex<Option<String>>,
}

impl ChannelApprover {
    fn new() -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tx,
            rx: tokio::sync::Mutex::new(rx),
            last_message: std::sync::Mutex::new(None),
        }
    }

    fn sender(&self) -> tokio::sync::mpsc::UnboundedSender<PermissionResponse> {
        self.tx.clone()
    }
}

#[async_trait::async_trait]
impl Approver for ChannelApprover {
    async fn decide(&self, request: &ApprovalRequest) -> PermissionDecision {
        emit_live_permission_request(request);
        dlog(&format!(
            "approver waiting for decision on {}",
            request.tool_name
        ));
        let mut rx = self.rx.lock().await;
        match rx.recv().await {
            Some(resp) => {
                dlog(&format!("approver received {:?}", resp.decision));
                *self.last_message.lock().unwrap() = resp.message.clone();
                resp.decision
            }
            // Channel closed (stdin ended): fail safe by denying.
            None => {
                dlog("approver channel closed -> deny");
                PermissionDecision::Deny
            }
        }
    }

    fn last_message(&self) -> Option<String> {
        self.last_message.lock().unwrap().clone()
    }
}

fn emit_live_permission_request(request: &ApprovalRequest) {
    use std::io::Write;

    let permission = SessionEvent::PermissionRequest {
        session_id: request.session_id,
        request_id: request.request_id,
        reason: request.reason.clone(),
        tool_name: request.tool_name.clone(),
        input_json: request.input.clone(),
    };
    let status = SessionEvent::Status {
        session_id: request.session_id,
        status: SessionStatus::WaitingForPermission,
    };

    let mut stdout = std::io::stdout().lock();
    if let Ok(line) = serde_json::to_string(&permission) {
        let _ = writeln!(stdout, "{line}");
    }
    if let Ok(line) = serde_json::to_string(&status) {
        let _ = writeln!(stdout, "{line}");
    }
    let _ = stdout.flush();
}

/// Append a line to `~/.inductor-debug.log` for diagnosing the permission and
/// tool-execution flow. Best-effort; failures are ignored.
fn dlog(msg: &str) {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = PathBuf::from(home).join(".inductor-debug.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "[run] {msg}");
    }
}

/// Parse a `{"type":"permission_decision","request_id":..,"decision":..,"message":..}`
/// line (written by the TUI to our stdin) into a [`PermissionResponse`].
fn parse_permission_decision(line: &str) -> Option<PermissionResponse> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("type").and_then(|t| t.as_str()) != Some("permission_decision") {
        return None;
    }
    let request_id = value
        .get("request_id")
        .and_then(|x| x.as_str())?
        .parse::<PermissionRequestId>()
        .ok()?;
    let decision = match value.get("decision").and_then(|x| x.as_str())? {
        "allow" => PermissionDecision::Allow,
        "allow_always" => PermissionDecision::AllowAlways,
        _ => PermissionDecision::Deny,
    };
    let message = value
        .get("message")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    Some(PermissionResponse {
        request_id,
        decision,
        message,
    })
}

fn parse_question_response(line: &str) -> Option<QuestionResponse> {
    let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
    if value.get("type").and_then(|t| t.as_str()) != Some("question_response") {
        return None;
    }
    let tool_call_id = value
        .get("tool_call_id")
        .and_then(|x| x.as_str())?
        .parse::<ToolCallId>()
        .ok()?;
    let answers_value = value.get("answers")?.clone();
    let answers = serde_json::from_value::<Vec<QuestionAnswer>>(answers_value).ok()?;
    Some(QuestionResponse {
        tool_call_id,
        answers,
    })
}

fn default_provider_model(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "sonnet",
        ProviderKind::Codex => "gpt-5.5",
        ProviderKind::Copilot => "gpt-4.1",
    }
}

fn attach_prompt_image_mentions(workspace: &Path, source_workspace: &Path, prompt: &str) -> String {
    const PREFIX: &str = "__MULTIMODAL_MESSAGE__:";
    if prompt.starts_with(PREFIX) {
        return prompt.to_string();
    }

    let mut images = Vec::new();
    for rel in image_mentions(prompt) {
        // The TUI writes pasted/dropped images into the source workspace. When
        // the agent runs in a worktree, that untracked file is missing here, so
        // mirror it into the worktree at the same relative path. This makes both
        // the multimodal payload below and any later `read_file` tool call on
        // the mentioned path resolve.
        if workspace != source_workspace {
            ensure_attachment_in_workspace(workspace, source_workspace, &rel);
        }
        if let Some(image) = read_prompt_image(workspace, &rel) {
            images.push(image);
        }
    }

    if images.is_empty() {
        return prompt.to_string();
    }

    let mut text = prompt.to_string();
    text.push_str("\n\nAttached image(s):\n");
    for image in &images {
        let path = image.path.as_deref().unwrap_or("image");
        let width = image
            .width
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        let height = image
            .height
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string());
        text.push_str(&format!(
            "- @{path} ({width}x{height}, {} bytes, {})\n",
            image.file_size, image.mime_type
        ));
    }

    match serde_json::to_string(&json!({ "text": text, "images": images })) {
        Ok(payload) => format!("{PREFIX}{payload}"),
        Err(_) => prompt.to_string(),
    }
}

fn image_mentions(prompt: &str) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut result = Vec::new();

    for raw in prompt.split_whitespace() {
        let token = raw
            .trim_matches(|ch: char| {
                matches!(ch, ',' | '.' | ';' | ':' | ')' | ']' | '}' | '"' | '\'')
            })
            .trim_start_matches(|ch: char| matches!(ch, '(' | '[' | '{' | '"' | '\''));
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        if !is_image_path(path) || !seen.insert(path.to_string()) {
            continue;
        }
        result.push(path.to_string());
    }

    result
}

fn read_prompt_image(workspace: &Path, rel: &str) -> Option<ImageAttachment> {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }

    let path = workspace.join(rel_path);
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() > MAX_PROMPT_IMAGE_BYTES {
        return None;
    }
    let dimensions = image::load_from_memory(&bytes)
        .ok()
        .map(|image| image.dimensions());
    Some(ImageAttachment {
        path: Some(rel.to_string()),
        mime_type: image_mime_type(rel).to_string(),
        base64_data: general_purpose::STANDARD.encode(bytes.as_slice()),
        width: dimensions.map(|(width, _)| width),
        height: dimensions.map(|(_, height)| height),
        file_size: bytes.len(),
    })
}

/// Copy a relative attachment from the source workspace into `workspace` when
/// it is missing there. Used so worktree runs can see images the TUI wrote into
/// the source checkout. Rejects absolute paths and `..` traversal, and never
/// overwrites an existing file.
fn ensure_attachment_in_workspace(workspace: &Path, source_workspace: &Path, rel: &str) {
    let rel_path = Path::new(rel);
    if rel_path.is_absolute()
        || rel_path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return;
    }

    let dest = workspace.join(rel_path);
    if dest.exists() {
        return;
    }
    let src = source_workspace.join(rel_path);
    if !src.exists() {
        return;
    }
    if let Some(parent) = dest.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = std::fs::copy(&src, &dest);
}

fn is_image_path(path: &str) -> bool {
    image_mime_type(path) != "application/octet-stream"
}

fn image_mime_type(path: &str) -> &'static str {
    match Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("tif" | "tiff") => "image/tiff",
        _ => "application/octet-stream",
    }
}

async fn run_auth_command(command: AuthCommand) -> Result<(), String> {
    match command {
        AuthCommand::Detect => {
            let detector = AuthDetector::from_env().map_err(|err| err.to_string())?;
            let credentials = detector.detect_all();

            if credentials.is_empty() {
                println!("status: none");
                return Ok(());
            }

            for credential in credentials {
                println!("provider: {}", credential.provider);
                println!("provider_id: {}", credential.provider_id.0);
                println!(
                    "source: {}",
                    credential.source.display_safe(detector.home_dir())
                );
                if let Some(identity_hint) = credential.identity_hint {
                    println!("identity_hint: {identity_hint}");
                }
                println!("status: found");
                println!();
            }
        }
        AuthCommand::CopilotLogin => copilot_device_login().await?,
    }

    Ok(())
}

const GITHUB_COPILOT_OAUTH_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    interval: u64,
}

#[derive(Debug, Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn copilot_device_login() -> Result<(), String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|err| err.to_string())?;
    let device = client
        .post("https://github.com/login/device/code")
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({
            "client_id": GITHUB_COPILOT_OAUTH_CLIENT_ID,
            "scope": "read:user",
        }))
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = device.status();
    let body = device.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!(
            "copilot device login failed with HTTP {status}: {}",
            redact_auth_body(&body)
        ));
    }
    let device: DeviceCodeResponse = serde_json::from_str(&body).map_err(|err| err.to_string())?;
    emit_auth_status(json!({
        "type": "auth_status",
        "provider": "copilot",
        "status": "device_code",
        "verification_uri": device.verification_uri,
        "user_code": device.user_code,
        "expires_in": device.expires_in,
    }));

    let mut interval = Duration::from_secs(device.interval.max(5));
    let expires_at = Instant::now() + Duration::from_secs(device.expires_in.max(900));
    loop {
        if Instant::now() >= expires_at {
            emit_auth_status(json!({
                "type": "auth_status",
                "provider": "copilot",
                "status": "expired",
            }));
            return Err("copilot device login expired before approval".to_string());
        }
        tokio::time::sleep(interval).await;
        let response = client
            .post("https://github.com/login/oauth/access_token")
            .header(reqwest::header::ACCEPT, "application/json")
            .json(&json!({
                "client_id": GITHUB_COPILOT_OAUTH_CLIENT_ID,
                "device_code": device.device_code.as_str(),
                "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            }))
            .send()
            .await
            .map_err(|err| err.to_string())?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "copilot device token poll failed with HTTP {status}: {}",
                redact_auth_body(&body)
            ));
        }
        let token: AccessTokenResponse =
            serde_json::from_str(&body).map_err(|err| err.to_string())?;
        if let Some(access_token) = token.access_token {
            write_copilot_apps_cache(&access_token).map_err(|err| err.to_string())?;
            emit_auth_status(json!({
                "type": "auth_status",
                "provider": "copilot",
                "status": "connected",
            }));
            return Ok(());
        }
        match token.error.as_deref() {
            Some("authorization_pending") => {
                emit_auth_status(json!({
                    "type": "auth_status",
                    "provider": "copilot",
                    "status": "waiting",
                }));
            }
            Some("slow_down") => {
                interval += Duration::from_secs(5);
                emit_auth_status(json!({
                    "type": "auth_status",
                    "provider": "copilot",
                    "status": "waiting",
                }));
            }
            Some(error) => {
                let message = token.error_description.unwrap_or_else(|| error.to_string());
                emit_auth_status(json!({
                    "type": "auth_status",
                    "provider": "copilot",
                    "status": "failed",
                    "message": message,
                }));
                return Err(format!("copilot device login failed: {error}"));
            }
            None => {
                return Err("copilot device login returned no token or error".to_string());
            }
        }
    }
}

fn emit_auth_status(value: Value) {
    use std::io::Write;
    if let Ok(line) = serde_json::to_string(&value) {
        println!("{line}");
        let _ = std::io::stdout().flush();
    }
}

fn write_copilot_apps_cache(access_token: &str) -> std::io::Result<()> {
    let detector = AuthDetector::from_env()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::NotFound, err.to_string()))?;
    let path = detector.copilot_auth_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cache = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let key = format!("github.com:{GITHUB_COPILOT_OAUTH_CLIENT_ID}");
    cache[&key] = json!({
        "oauth_token": access_token,
        "user": ""
    });
    let bytes = serde_json::to_vec_pretty(&cache)?;
    std::fs::write(path, bytes)
}

fn redact_auth_body(body: &str) -> String {
    body.chars()
        .take(2_000)
        .collect::<String>()
        .replace("access_token", "access_token_redacted")
        .replace("oauth_token", "oauth_token_redacted")
}

async fn run_pr_body_command(
    provider: ProviderArg,
    workspace: PathBuf,
    title: String,
    diff: String,
    model: Option<String>,
) -> Result<(), String> {
    let provider = ProviderKind::from(provider);
    let model = model.unwrap_or_else(|| default_provider_model(provider).to_string());
    let body = generate_pull_request_description(
        &title,
        &diff,
        Some(SessionNamingConfig {
            provider,
            model,
            enabled: true,
            cwd: Some(workspace),
        }),
    )
    .await
    .map_err(|err| err.to_string())?;
    println!("{body}");
    Ok(())
}

async fn run_session_command(command: SessionCommand) -> Result<(), String> {
    match command {
        SessionCommand::DemoEvents => {
            for event in demo_session_events() {
                let line = serde_json::to_string(&event).map_err(|err| err.to_string())?;
                println!("{line}");
            }
        }
    }

    Ok(())
}

async fn run_tool_command(command: ToolCommand) -> Result<(), String> {
    let result = match command {
        ToolCommand::ReadFile { workspace, path } => ToolRuntime::new(workspace)
            .map_err(|err| err.to_string())?
            .read_file(path)
            .map_err(|err| err.to_string())?,
        ToolCommand::WriteFile {
            workspace,
            path,
            content,
        } => ToolRuntime::new(workspace)
            .map_err(|err| err.to_string())?
            .write_file(path, content)
            .map_err(|err| err.to_string())?,
        ToolCommand::EditFile {
            workspace,
            path,
            old,
            new,
            expected_hash,
        } => ToolRuntime::new(workspace)
            .map_err(|err| err.to_string())?
            .edit_file(path, old, new, expected_hash.as_deref())
            .map_err(|err| err.to_string())?,
        ToolCommand::MultiEdit {
            workspace,
            path,
            edits_json,
            expected_hash,
        } => {
            let edits = serde_json::from_str::<Vec<TextEdit>>(&edits_json)
                .map_err(|err| format!("invalid edits_json: {err}"))?;
            ToolRuntime::new(workspace)
                .map_err(|err| err.to_string())?
                .multi_edit(path, &edits, expected_hash.as_deref())
                .map_err(|err| err.to_string())?
        }
        ToolCommand::ApplyPatchFreeform { workspace, patch } => ToolRuntime::new(workspace)
            .map_err(|err| err.to_string())?
            .apply_patch_freeform(patch)
            .map_err(|err| err.to_string())?,
        ToolCommand::ApplyPatchStructured {
            workspace,
            patch_json,
        } => {
            let patch = serde_json::from_str::<StructuredPatch>(&patch_json)
                .map_err(|err| format!("invalid patch_json: {err}"))?;
            ToolRuntime::new(workspace)
                .map_err(|err| err.to_string())?
                .apply_patch_structured(&patch)
                .map_err(|err| err.to_string())?
        }
        ToolCommand::Grep { workspace, pattern } => ToolRuntime::new(workspace)
            .map_err(|err| err.to_string())?
            .grep(pattern)
            .map_err(|err| err.to_string())?,
        ToolCommand::Bash { workspace, command } => ToolRuntime::new(workspace)
            .map_err(|err| err.to_string())?
            .bash(command)
            .map_err(|err| err.to_string())?,
    };

    let line = serde_json::to_string(&result).map_err(|err| err.to_string())?;
    println!("{line}");

    Ok(())
}

async fn run_terminal_command(command: TerminalCommand) -> Result<(), String> {
    match command {
        TerminalCommand::Run {
            workspace,
            command,
            rows,
            cols,
            timeout_ms,
        } => {
            let mut manager = PtyManager::new();
            let mut request = SpawnTerminalRequest::new(workspace);
            request.size = TerminalSize::new(rows, cols);
            let id = manager.spawn(request).map_err(|err| err.to_string())?;
            manager
                .write(id, format!("{command}\nexit\n"))
                .map_err(|err| err.to_string())?;

            let snapshot = wait_for_terminal_exit(&mut manager, id, timeout_ms)?;
            let line = serde_json::to_string(&snapshot).map_err(|err| err.to_string())?;
            println!("{line}");
            let _ = manager.kill(id);
        }
        TerminalCommand::Smoke { workspace } => {
            let mut manager = PtyManager::new();
            let id = manager
                .spawn(SpawnTerminalRequest::new(workspace))
                .map_err(|err| err.to_string())?;
            manager
                .write(id, "printf phase8-ready\n")
                .map_err(|err| err.to_string())?;
            wait_for_terminal_output(&manager, id, "phase8-ready", 2_000)?;
            manager
                .resize(id, TerminalSize::new(40, 120))
                .map_err(|err| err.to_string())?;
            let snapshot = manager.snapshot(id).map_err(|err| err.to_string())?;
            println!(
                "{}",
                serde_json::to_string(&snapshot).map_err(|err| err.to_string())?
            );
            let exit_code = manager.kill(id).map_err(|err| err.to_string())?;
            println!("killed: {exit_code:?}");
        }
        TerminalCommand::Serve {
            workspace,
            rows,
            cols,
        } => {
            run_terminal_serve(workspace, rows, cols)?;
        }
    }

    Ok(())
}

/// Drive a long-lived interactive PTY, forwarding stdin control messages into
/// the shell and emitting screen snapshots on stdout. Used by the TUI's
/// embedded terminal panel.
fn run_terminal_serve(workspace: PathBuf, rows: u16, cols: u16) -> Result<(), String> {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    let manager = Arc::new(Mutex::new(PtyManager::new()));
    let id = {
        let mut guard = manager
            .lock()
            .map_err(|_| "terminal manager lock poisoned".to_string())?;
        let mut request = SpawnTerminalRequest::new(workspace);
        request.size = TerminalSize::new(rows, cols);
        guard.spawn(request).map_err(|err| err.to_string())?
    };

    // Stdin reader thread: forward input/resize control messages to the PTY.
    let reader_manager = manager.clone();
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                continue;
            };
            match value.get("type").and_then(|v| v.as_str()) {
                Some("input") => {
                    if let Some(data) = value.get("data").and_then(|v| v.as_str()) {
                        if let Ok(mut guard) = reader_manager.lock() {
                            let _ = guard.write(id, data);
                        }
                    }
                }
                Some("resize") => {
                    let new_rows = value.get("rows").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    let new_cols = value.get("cols").and_then(|v| v.as_u64()).unwrap_or(0) as u16;
                    if new_rows > 0 && new_cols > 0 {
                        if let Ok(mut guard) = reader_manager.lock() {
                            let _ = guard.resize(id, TerminalSize::new(new_rows, new_cols));
                        }
                    }
                }
                _ => {}
            }
        }
    });

    // Main loop: emit a snapshot whenever the screen contents or cursor change.
    let mut last_contents = String::new();
    let mut last_cursor = (u16::MAX, u16::MAX);
    let stdout = std::io::stdout();
    loop {
        let snapshot = {
            let mut guard = manager
                .lock()
                .map_err(|_| "terminal manager lock poisoned".to_string())?;
            let _ = guard.try_wait(id);
            guard.snapshot(id).map_err(|err| err.to_string())?
        };
        let cursor = (snapshot.cursor_row, snapshot.cursor_col);
        if snapshot.contents != last_contents || cursor != last_cursor || !snapshot.is_running {
            last_contents = snapshot.contents.clone();
            last_cursor = cursor;
            let payload = json!({
                "type": "snapshot",
                "contents": snapshot.contents,
                "screen_rows": snapshot.screen_rows,
                "cursor_row": snapshot.cursor_row,
                "cursor_col": snapshot.cursor_col,
                "rows": snapshot.size.rows,
                "cols": snapshot.size.cols,
                "is_running": snapshot.is_running,
            });
            let line = serde_json::to_string(&payload).map_err(|err| err.to_string())?;
            let mut handle = stdout.lock();
            let _ = writeln!(handle, "{line}");
            let _ = handle.flush();
        }
        if !snapshot.is_running {
            break;
        }
        std::thread::sleep(Duration::from_millis(40));
    }

    if let Ok(mut guard) = manager.lock() {
        let _ = guard.kill(id);
    }
    Ok(())
}

fn wait_for_terminal_exit(
    manager: &mut PtyManager,
    id: terminal::TerminalId,
    timeout_ms: u64,
) -> Result<terminal::TerminalSnapshot, String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let _ = manager.try_wait(id).map_err(|err| err.to_string())?;
        let snapshot = manager.snapshot(id).map_err(|err| err.to_string())?;
        if snapshot.is_running && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            continue;
        }

        return Ok(snapshot);
    }
}

fn wait_for_terminal_output(
    manager: &PtyManager,
    id: terminal::TerminalId,
    needle: &str,
    timeout_ms: u64,
) -> Result<terminal::TerminalSnapshot, String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let snapshot = manager.snapshot(id).map_err(|err| err.to_string())?;
        if snapshot.raw_output.contains(needle) {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for terminal output {needle:?}"));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

async fn run_worktree_command(command: WorktreeCommand) -> Result<(), String> {
    match command {
        WorktreeCommand::Inspect { repo } => {
            let manager = WorktreeManager::new(default_managed_root()?);
            let info = manager.inspect_repo(&repo).map_err(|err| err.to_string())?;

            println!("root: {}", info.root.display());
            println!("current_branch: {}", info.current_branch);
            println!("head_commit: {}", info.head_commit);
            println!("is_dirty: {}", info.is_dirty);
        }
        WorktreeCommand::Create {
            repo,
            slug,
            managed_root,
            allow_dirty,
            app_db,
            json,
        } => {
            let managed_root = match managed_root {
                Some(root) => root,
                None => default_managed_root()?,
            };
            let manager = WorktreeManager::new(managed_root);
            let worktree = manager
                .create_worktree(CreateWorktreeRequest {
                    source_repo: repo,
                    slug,
                    allow_dirty,
                })
                .map_err(|err| err.to_string())?;

            if let Some(app_db) = app_db {
                let registry = AppDb::open(&app_db).map_err(|err| err.to_string())?;
                register_worktree(&registry, &worktree)?;
            }

            if json {
                let state_db = worktree_state_db_path(worktree.workspace_id)?;
                let payload = json!({
                    "workspace_id": worktree.workspace_id,
                    "source_repo": worktree.source_repo.display().to_string(),
                    "worktree_path": worktree.worktree_path.display().to_string(),
                    "state_db": state_db.display().to_string(),
                    "branch_name": worktree.branch_name,
                    "base_branch": worktree.base_branch,
                    "base_commit": worktree.base_commit,
                });
                println!(
                    "{}",
                    serde_json::to_string(&payload).map_err(|err| err.to_string())?
                );
            } else {
                println!("workspace_id: {}", worktree.workspace_id);
                println!("source_repo: {}", worktree.source_repo.display());
                println!("worktree_path: {}", worktree.worktree_path.display());
                println!("branch_name: {}", worktree.branch_name);
                println!("base_branch: {}", worktree.base_branch);
                println!("base_commit: {}", worktree.base_commit);
            }
        }
        WorktreeCommand::List { repo } => {
            let manager = WorktreeManager::new(default_managed_root()?);
            let worktrees = manager
                .list_worktrees(&repo)
                .map_err(|err| err.to_string())?;

            for worktree in worktrees {
                println!("path: {}", worktree.path.display());
                println!("head: {}", worktree.head);
                if let Some(branch) = worktree.branch {
                    println!("branch: {branch}");
                }
                println!();
            }
        }
        WorktreeCommand::Registry {
            app_db,
            source_repo,
            json: json_output,
        } => {
            let registry = open_worktree_registry(app_db)?;
            let mut worktrees = registry.list_worktrees().map_err(|err| err.to_string())?;
            // Scope the listing to the repo Inductor was opened in: resolve the
            // git toplevel of `source_repo` (worktree records store the
            // canonical toplevel) and keep only worktrees created from it. If
            // the path isn't a git repo, keep the unfiltered list rather than
            // hiding everything.
            if let Some(source_repo) = source_repo {
                let manager = WorktreeManager::new(default_managed_root()?);
                if let Ok(repo) = manager.inspect_repo(&source_repo) {
                    worktrees.retain(|worktree| worktree.source_repo == repo.root);
                }
            }
            refresh_merged_worktrees(&registry, &worktrees);
            if json_output {
                let rows = worktrees
                    .iter()
                    .map(|worktree| {
                        // Newest session in this workspace gives the display name
                        // and live status for the dashboard.
                        let session = registry
                            .list_sessions(worktree.id)
                            .ok()
                            .and_then(|sessions| sessions.into_iter().next());
                        let status = refreshed_worktree_status(&registry, worktree);
                        json!({
                            "workspace_id": worktree.id,
                            "source_repo": worktree.source_repo.display().to_string(),
                            "worktree_path": worktree.worktree_path.display().to_string(),
                            "state_db": worktree_state_db_path(worktree.id)
                                .ok()
                                .map(|path| path.display().to_string()),
                            "branch_name": worktree.branch_name,
                            "base_branch": worktree.base_branch,
                            "status": status.as_str(),
                            "exists": worktree.worktree_path.exists(),
                            "display_name": session.as_ref().and_then(|s| s.display_name.clone()),
                            "session_id": session.as_ref().map(|s| s.id.to_string()),
                            "session_status": session.as_ref().map(|s| format!("{:?}", s.status).to_lowercase()),
                            "provider": session.as_ref().map(|s| s.provider_id.0.clone()),
                            "model": session.as_ref().map(|s| s.model.clone()),
                            "updated_at": worktree.updated_at,
                        })
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string(&rows).map_err(|err| err.to_string())?
                );
            } else {
                for worktree in worktrees {
                    println!("workspace_id: {}", worktree.id);
                    println!("branch: {}", worktree.branch_name);
                    println!("status: {}", worktree.status.as_str());
                    println!("path: {}", worktree.worktree_path.display());
                    println!();
                }
            }
        }
        WorktreeCommand::Remove { repo, path, force } => {
            let manager = WorktreeManager::new(default_managed_root()?);
            manager
                .remove_worktree(&repo, &path, force)
                .map_err(|err| err.to_string())?;

            println!("removed: {}", path.display());
        }
        WorktreeCommand::Drift {
            workspace_id,
            app_db,
            target,
        } => {
            let registry = open_worktree_registry(app_db)?;
            let worktree = lookup_worktree(&registry, workspace_id)?;
            let target_branch = target.unwrap_or(worktree.base_branch);

            let manager = WorktreeManager::new(default_managed_root()?);
            let drift = manager
                .check_drift(&worktree.source_repo, &target_branch, &worktree.base_commit)
                .map_err(|err| err.to_string())?;

            println!("target_branch: {target_branch}");
            println!("base_commit: {}", drift.base_commit);
            println!("target_head: {}", drift.target_head);
            println!("drifted: {}", drift.drifted);
        }
        WorktreeCommand::Archive {
            workspace_id,
            app_db,
            json: json_output,
        } => {
            let registry = open_worktree_registry(app_db)?;
            let worktree = lookup_worktree(&registry, workspace_id)?;

            // Remove the working directory but keep the registry record and the
            // session's chats (the state.db lives outside the worktree dir).
            cleanup_worktree_dir(&worktree)?;
            registry
                .set_worktree_status(workspace_id, WorktreeStatus::Archived)
                .map_err(|err| err.to_string())?;

            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&json!({
                        "result": "archived",
                        "workspace_id": workspace_id.to_string(),
                    }))
                    .map_err(|err| err.to_string())?
                );
            } else {
                println!("archived worktree {workspace_id} (chats kept)");
            }
        }
    }

    Ok(())
}

/// Remove a managed worktree's working directory (and prune git's metadata),
/// leaving the registry record and the session's chats intact. No-op if the
/// directory is already gone.
fn cleanup_worktree_dir(worktree: &WorktreeRecord) -> Result<(), String> {
    if !worktree.worktree_path.exists() {
        return Ok(());
    }
    let manager = WorktreeManager::new(default_managed_root()?);
    manager
        .remove_worktree(&worktree.source_repo, &worktree.worktree_path, true)
        .map_err(|err| err.to_string())
}

fn open_worktree_registry(app_db: Option<PathBuf>) -> Result<AppDb, String> {
    let path = match app_db {
        Some(path) => path,
        None => default_app_db_path()?,
    };
    AppDb::open(&path).map_err(|err| err.to_string())
}

fn refreshed_worktree_status(registry: &AppDb, worktree: &WorktreeRecord) -> WorktreeStatus {
    if matches!(
        worktree.status,
        WorktreeStatus::Archived | WorktreeStatus::Abandoned
    ) {
        return worktree.status;
    }
    let detected = detect_pr_status(worktree).unwrap_or(WorktreeStatus::Active);
    if detected != worktree.status {
        let _ = registry.set_worktree_status(worktree.id, detected);
    }
    detected
}

fn refresh_merged_worktrees(registry: &AppDb, worktrees: &[WorktreeRecord]) {
    for worktree in worktrees {
        let _ = refreshed_worktree_status(registry, worktree);
    }
}

fn detect_pr_status(worktree: &WorktreeRecord) -> Option<WorktreeStatus> {
    let branch = worktree.branch_name.as_str();
    let pr_state = gh_command(&worktree.worktree_path)
        .args(["pr", "view", branch, "--json", "state", "--jq", ".state"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    match pr_state.as_deref() {
        Some("MERGED") => Some(WorktreeStatus::Merged),
        Some("OPEN") => Some(WorktreeStatus::PrOpen),
        _ => Some(WorktreeStatus::Active),
    }
}

fn gh_command(workspace: &Path) -> std::process::Command {
    let mut command = std::process::Command::new("gh");
    command.current_dir(workspace);
    // GH_REPO overrides repository detection in GitHub CLI. Some shells set it
    // to the workspace path, which makes `gh pr ...` fail with:
    // expected the "[HOST/]OWNER/REPO" format, got "/path/to/repo".
    // For Inductor PR/status operations, infer the target repo from git remote.
    command.env_remove("GH_REPO");
    command
}

fn lookup_worktree(registry: &AppDb, workspace_id: WorkspaceId) -> Result<WorktreeRecord, String> {
    registry
        .get_worktree(workspace_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("no managed worktree for workspace {workspace_id}"))
}

fn recover_orphaned_sessions(registry: &AppDb) -> Result<usize, String> {
    let incomplete = registry
        .list_incomplete_sessions()
        .map_err(|err| err.to_string())?;
    if incomplete.is_empty() {
        return Ok(0);
    }

    let mut state_paths = HashMap::new();
    for workspace in registry.list_workspaces().map_err(|err| err.to_string())? {
        state_paths.insert(workspace.id, workspace_state_path(workspace.path));
    }
    for worktree in registry.list_worktrees().map_err(|err| err.to_string())? {
        state_paths.insert(worktree.id, worktree_state_db_path(worktree.id)?);
    }

    let mut recovered = 0;
    for mut session in incomplete {
        let Some(state_path) = state_paths.get(&session.workspace_id) else {
            continue;
        };
        let workspace_db = WorkspaceDb::open(state_path).map_err(|err| err.to_string())?;

        session.status = SessionStatus::Idle;
        session.updated_at = now_rfc3339().map_err(|err| err.to_string())?;
        workspace_db
            .upsert_session(&session)
            .map_err(|err| err.to_string())?;
        registry
            .upsert_session(&session)
            .map_err(|err| err.to_string())?;

        let error_event = SessionEvent::Error {
            session_id: session.id,
            message: ORPHANED_SESSION_RECOVERY_MESSAGE.to_string(),
        };
        workspace_db
            .append_event(session.id, &error_event)
            .map_err(|err| err.to_string())?;
        let idle_event = SessionEvent::Status {
            session_id: session.id,
            status: SessionStatus::Idle,
        };
        workspace_db
            .append_event(session.id, &idle_event)
            .map_err(|err| err.to_string())?;

        recovered += 1;
    }

    Ok(recovered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(name: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("inductor-agent-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn image_mentions_detect_workspace_image_paths() {
        assert_eq!(
            image_mentions("compare @screens/a.png and @photo.jpeg."),
            vec!["screens/a.png", "photo.jpeg"]
        );
    }

    #[test]
    fn prompt_image_mentions_are_wrapped_as_multimodal_payload() {
        let workspace = temp_workspace("image-wrap");
        std::fs::write(workspace.join("screen.png"), b"fake image bytes").unwrap();

        let prompt = attach_prompt_image_mentions(&workspace, &workspace, "describe @screen.png");
        let payload = prompt
            .strip_prefix("__MULTIMODAL_MESSAGE__:")
            .expect("prompt should be wrapped");
        let value: serde_json::Value = serde_json::from_str(payload).unwrap();

        assert!(
            value["text"]
                .as_str()
                .unwrap()
                .contains("describe @screen.png")
        );
        assert_eq!(value["images"][0]["path"], "screen.png");
        assert_eq!(value["images"][0]["mime_type"], "image/png");

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn submitted_user_message_is_durable_before_run_completion() {
        let workspace = temp_workspace("submitted-user-message");
        let db = WorkspaceDb::open(workspace.join("state.db")).unwrap();
        let session_id = SessionId::new();
        let session = new_session_record(
            session_id,
            WorkspaceId::new(),
            ProviderId("codex".to_string()),
            "gpt-5.5".to_string(),
        )
        .unwrap();
        db.upsert_session(&session).unwrap();
        let transcript = vec![TranscriptMessage::new(Role::User, "hi")];

        persist_submitted_user_message(
            &db,
            session_id,
            &transcript,
            "do all the tools calls availble to u just to check their reliability",
        )
        .unwrap();

        let messages = db.messages(session_id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hi");
        assert_eq!(
            messages[1].content,
            "do all the tools calls availble to u just to check their reliability"
        );

        let events = db.events(session_id).unwrap();
        assert_eq!(
            events,
            vec![SessionEvent::UserMessage {
                session_id,
                text: "do all the tools calls availble to u just to check their reliability"
                    .to_string()
            }]
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn submitted_multimodal_user_message_persists_visible_text() {
        let workspace = temp_workspace("submitted-multimodal-user-message");
        let db = WorkspaceDb::open(workspace.join("state.db")).unwrap();
        let session_id = SessionId::new();
        let session = new_session_record(
            session_id,
            WorkspaceId::new(),
            ProviderId("codex".to_string()),
            "gpt-5.5".to_string(),
        )
        .unwrap();
        db.upsert_session(&session).unwrap();
        let payload = serde_json::json!({
            "text": "look at this screenshot",
            "images": [{"path": "screen.png"}]
        });

        persist_submitted_user_message(
            &db,
            session_id,
            &[],
            &format!("__MULTIMODAL_MESSAGE__:{payload}"),
        )
        .unwrap();

        let messages = db.messages(session_id).unwrap();
        assert_eq!(messages[0].content, "look at this screenshot");

        let events = db.events(session_id).unwrap();
        assert_eq!(
            events,
            vec![SessionEvent::UserMessage {
                session_id,
                text: "look at this screenshot".to_string()
            }]
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn startup_recovery_marks_incomplete_sessions_idle() {
        let workspace = temp_workspace("startup-recovery");
        let app_path = workspace.join("app.db");
        let state_path = workspace_state_path(&workspace);
        let app_db = AppDb::open(&app_path).unwrap();
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        app_db
            .upsert_workspace(workspace_id, &workspace, "startup-recovery")
            .unwrap();

        let mut session = new_session_record(
            session_id,
            workspace_id,
            ProviderId("codex".to_string()),
            "gpt-5.5",
        )
        .unwrap();
        session.status = SessionStatus::Streaming;
        app_db.upsert_session(&session).unwrap();

        let workspace_db = WorkspaceDb::open(&state_path).unwrap();
        workspace_db.upsert_session(&session).unwrap();
        workspace_db
            .append_event(
                session_id,
                &SessionEvent::Status {
                    session_id,
                    status: SessionStatus::Streaming,
                },
            )
            .unwrap();

        assert_eq!(recover_orphaned_sessions(&app_db).unwrap(), 1);

        let app_session = app_db.get_session(session_id).unwrap().unwrap();
        assert_eq!(app_session.status, SessionStatus::Idle);
        let workspace_session = workspace_db.get_session(session_id).unwrap().unwrap();
        assert_eq!(workspace_session.status, SessionStatus::Idle);

        let events = workspace_db.events(session_id).unwrap();
        assert!(matches!(
            events.last(),
            Some(SessionEvent::Status {
                status: SessionStatus::Idle,
                ..
            })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::Error { message, .. }
                if message.contains("Recovered after Inductor restarted")
        )));

        assert_eq!(recover_orphaned_sessions(&app_db).unwrap(), 0);

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn opentui_preload_detection_checks_workspace_and_package_node_modules() {
        let repo = temp_workspace("opentui-preload");
        let tui = repo.join("packages").join("tui");
        std::fs::create_dir_all(&tui).unwrap();

        assert!(!opentui_preload_exists(&repo, &tui));

        let root_preload = repo
            .join("node_modules")
            .join("@opentui")
            .join("solid")
            .join("scripts")
            .join("preload.ts");
        std::fs::create_dir_all(root_preload.parent().unwrap()).unwrap();
        std::fs::write(&root_preload, "").unwrap();
        assert!(opentui_preload_exists(&repo, &tui));

        std::fs::remove_file(&root_preload).unwrap();
        let bun_preload = repo
            .join("node_modules")
            .join(".bun")
            .join("node_modules")
            .join("@opentui")
            .join("solid")
            .join("scripts")
            .join("preload.ts");
        std::fs::create_dir_all(bun_preload.parent().unwrap()).unwrap();
        std::fs::write(&bun_preload, "").unwrap();
        assert!(opentui_preload_exists(&repo, &tui));

        std::fs::remove_file(&bun_preload).unwrap();
        let package_preload = tui
            .join("node_modules")
            .join("@opentui")
            .join("solid")
            .join("scripts")
            .join("preload.ts");
        std::fs::create_dir_all(package_preload.parent().unwrap()).unwrap();
        std::fs::write(&package_preload, "").unwrap();
        assert!(opentui_preload_exists(&repo, &tui));

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn prompt_image_from_source_workspace_is_mirrored_into_worktree() {
        let source = temp_workspace("image-source");
        let worktree = temp_workspace("image-worktree");
        // The TUI wrote the pasted image into the source workspace only; the
        // freshly created worktree does not contain the untracked attachment.
        std::fs::create_dir_all(source.join(".inductor/attachments")).unwrap();
        std::fs::write(
            source.join(".inductor/attachments/pasted-image-1.png"),
            b"fake image bytes",
        )
        .unwrap();

        let prompt = attach_prompt_image_mentions(
            &worktree,
            &source,
            "look @.inductor/attachments/pasted-image-1.png",
        );
        let payload = prompt
            .strip_prefix("__MULTIMODAL_MESSAGE__:")
            .expect("prompt should be wrapped with the image payload");
        let value: serde_json::Value = serde_json::from_str(payload).unwrap();
        assert_eq!(
            value["images"][0]["path"],
            ".inductor/attachments/pasted-image-1.png"
        );
        // The attachment must now exist in the worktree too so a later
        // `read_file` on the mentioned path resolves there.
        assert!(
            worktree
                .join(".inductor/attachments/pasted-image-1.png")
                .exists()
        );

        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(worktree);
    }

    #[test]
    fn fallback_worktree_name_uses_prompt_keywords() {
        assert_eq!(
            fallback_worktree_name(
                "please implement backend cache invalidation and update the worker API"
            ),
            Some("Backend Cache Invalidation".to_string())
        );
    }

    #[test]
    fn fallback_worktree_name_reads_multimodal_prompt_text() {
        let payload = serde_json::json!({
            "text": "fix topbar overflow and remove merge controls",
            "images": [{"path": "screen.png"}]
        });
        assert_eq!(
            fallback_worktree_name(&format!("__MULTIMODAL_MESSAGE__:{payload}")),
            Some("Topbar Overflow Merge".to_string())
        );
    }

    #[test]
    fn repo_memory_file_prefers_source_workspace_git_root() {
        let source = temp_workspace("memory-source");
        let worktree = temp_workspace("memory-worktree");
        std::process::Command::new("git")
            .arg("init")
            .arg(&source)
            .output()
            .unwrap();

        let memory = repo_memory_file(&source, &worktree).unwrap();

        assert_eq!(
            memory,
            std::fs::canonicalize(&source)
                .unwrap()
                .join(".inductor")
                .join("memory.md")
        );

        let _ = std::fs::remove_dir_all(source);
        let _ = std::fs::remove_dir_all(worktree);
    }

    #[test]
    fn repo_memory_file_falls_back_to_workspace_folder_for_non_git_projects() {
        let workspace = temp_workspace("memory-nongit");

        let memory = repo_memory_file(&workspace, &workspace).unwrap();

        assert_eq!(
            memory,
            std::fs::canonicalize(&workspace)
                .unwrap()
                .join(".inductor")
                .join("memory.md")
        );

        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn ensure_repo_memory_file_creates_default_memory() {
        let workspace = temp_workspace("memory-create");
        let memory = workspace.join(".inductor").join("memory.md");

        ensure_repo_memory_file(&memory).unwrap();

        let content = std::fs::read_to_string(memory).unwrap();
        assert!(content.contains("# Inductor Repo Memory"));
        assert!(content.contains("Do not store secrets"));

        let _ = std::fs::remove_dir_all(workspace);
    }
}

/// Point the process working directory at `path`, the preferred home for a
/// session. In worktree mode this is the managed worktree; anchoring cwd here
/// keeps the environment block we show the model, cwd-relative tool fallbacks,
/// and providers that inherit `current_dir()` pointed at the worktree instead
/// of the original checkout. Best-effort: a failure (e.g. the directory was
/// moved mid-flight) is non-fatal because tools already carry an absolute
/// workspace root, so we only log to stderr and continue. This never restricts
/// where tools may go — it just sets the default.
fn set_process_cwd(path: &Path) {
    if let Err(err) = std::env::set_current_dir(path) {
        eprintln!(
            "warning: could not set working directory to {}: {err}",
            path.display()
        );
    }
}

fn default_managed_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;

    // Worktrees live under `~/inductor/workspaces/<repo>/<branch>`. The repo and
    // branch segments are appended by the worktree manager when each worktree is
    // created; this is just the shared root.
    Ok(PathBuf::from(home).join("inductor").join("workspaces"))
}

fn default_app_db_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Inductor")
        .join("app.db"))
}

/// Stable, app-managed location for a worktree-mode session's `state.db`,
/// keyed by workspace id. Kept OUTSIDE the worktree directory so archiving
/// (which deletes the worktree dir) never destroys the chats/messages.
fn worktree_state_db_path(workspace_id: WorkspaceId) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Inductor")
        .join("state")
        .join(format!("{workspace_id}.db")))
}

fn default_blob_root(state_db_path: &Path) -> Option<PathBuf> {
    state_db_path
        .parent()
        .map(|parent| parent.join("tool-output-blobs"))
}

/// Repo-scoped memory is stored in the source checkout's Inductor state dir,
/// not inside a per-session worktree. In worktree mode every session passes the
/// same `source_workspace`, so each worktree reads/writes the same file.
fn repo_memory_file(source_workspace: &Path, workspace_path: &Path) -> Option<PathBuf> {
    let source_repo = canonical_git_root(source_workspace)
        .or_else(|| canonical_git_root(workspace_path))
        .or_else(|| canonical_dir(source_workspace))?;
    Some(source_repo.join(".inductor").join("memory.md"))
}

fn ensure_repo_memory_file(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, initial_repo_memory_content())
}

fn initial_repo_memory_content() -> &'static str {
    "# Inductor Repo Memory\n\n\
Shared memory for all Inductor sessions and worktrees for this repository.\n\n\
Guidelines:\n\
- Keep this concise and durable: project conventions, recurring workflows, stable architecture notes, and known pitfalls.\n\
- Do not store secrets, credentials, tokens, or one-off scratch notes.\n\
- Required team guidance belongs in AGENTS.md or checked-in docs; this file is a local recall layer.\n\n"
}

fn repo_memory_prompt_layer(path: &Path) -> Result<Option<String>, String> {
    let content = fs::read_to_string(path).map_err(|err| err.to_string())?;
    let content = truncate_memory_content(&content, MAX_REPO_MEMORY_BYTES);
    Ok(Some(format!(
        "Repo memory is enabled for this workspace. It is shared by all Inductor sessions and worktrees for the same source repo.\n\
Memory file: {}\n\n\
Current repo memory:\n<repo_memory>\n{}\n</repo_memory>\n\n\
Use read_memory if you need to inspect the latest memory file, and use write_memory to update it with concise, durable context learned during the task. Do not store secrets in memory.",
        path.display(),
        content
    )))
}

fn truncate_memory_content(content: &str, limit: usize) -> String {
    if content.len() <= limit {
        return content.to_string();
    }
    let mut end = limit;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[Inductor truncated repo memory from {} bytes to {} bytes for this prompt. Use read_memory for the full file.]",
        &content[..end],
        content.len(),
        end
    )
}

fn canonical_git_root(path: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    canonical_dir(Path::new(root.trim()))
}

fn canonical_dir(path: &Path) -> Option<PathBuf> {
    let canonical = fs::canonicalize(path).ok()?;
    canonical.is_dir().then_some(canonical)
}

/// Generate a short (<=3 word) fallback name for a fresh worktree-mode session
/// from its first prompt when the silent provider naming call is unavailable.
fn fallback_worktree_name(prompt: &str) -> Option<String> {
    let text = prompt
        .strip_prefix("__MULTIMODAL_MESSAGE__:")
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .and_then(|value| value["text"].as_str().map(str::to_string))
        .unwrap_or_else(|| prompt.to_string());
    let stopwords = [
        "a",
        "add",
        "an",
        "and",
        "are",
        "as",
        "be",
        "can",
        "change",
        "create",
        "do",
        "fix",
        "for",
        "from",
        "have",
        "i",
        "implement",
        "in",
        "into",
        "is",
        "it",
        "make",
        "me",
        "need",
        "now",
        "of",
        "on",
        "or",
        "please",
        "remove",
        "should",
        "that",
        "the",
        "this",
        "to",
        "update",
        "want",
        "we",
        "with",
        "you",
    ];
    let mut words = Vec::new();
    for raw in text.split(|c: char| !c.is_ascii_alphanumeric()) {
        let word = raw.trim().to_ascii_lowercase();
        if word.len() < 3 || stopwords.contains(&word.as_str()) {
            continue;
        }
        if !words.contains(&word) {
            words.push(word);
        }
        if words.len() == 3 {
            break;
        }
    }
    if words.is_empty() {
        None
    } else {
        Some(
            words
                .into_iter()
                .map(|word| {
                    let mut chars = word.chars();
                    match chars.next() {
                        Some(first) => {
                            format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
                        }
                        None => word,
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

/// Where a worktree-mode session should run and the workspace id that ties it
/// to the worktree registry.
struct WorktreeBinding {
    workspace_id: WorkspaceId,
    source_repo: PathBuf,
    worktree_path: PathBuf,
    created_worktree: Option<git::ManagedWorktree>,
}

/// Resolve the worktree a worktree-mode `run` should execute in: reuse the one
/// bound to the resumed session, otherwise create a fresh isolated worktree off
/// `source_repo` and record it (and its workspace) in the app DB.
fn create_worktree_binding(
    registry: &AppDb,
    source_repo: &Path,
    slug: Option<&str>,
) -> Result<WorktreeBinding, String> {
    // Fresh worktree off the source repo's current branch. Allow a dirty repo:
    // a new worktree checks out HEAD and never touches the source checkout, so
    // the user's uncommitted changes stay put rather than blocking session
    // creation (worktree mode is the default for new sessions).
    let manager = WorktreeManager::new(default_managed_root()?);
    let created = manager
        .create_worktree(CreateWorktreeRequest {
            source_repo: source_repo.to_path_buf(),
            slug: slug.unwrap_or("session").to_string(),
            allow_dirty: true,
        })
        .map_err(|err| err.to_string())?;

    register_worktree(registry, &created)?;

    Ok(WorktreeBinding {
        workspace_id: created.workspace_id,
        source_repo: created.source_repo.clone(),
        worktree_path: created.worktree_path.clone(),
        created_worktree: Some(created),
    })
}

/// Record a freshly created worktree (and its workspace) in the app DB so it
/// shows up in the registry and can be reopened or archived later.
fn register_worktree(registry: &AppDb, created: &git::ManagedWorktree) -> Result<(), String> {
    let display_name = created
        .worktree_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("worktree")
        .to_string();
    registry
        .upsert_workspace(created.workspace_id, &created.worktree_path, &display_name)
        .map_err(|err| err.to_string())?;

    let now = now_rfc3339().map_err(|err| err.to_string())?;
    registry
        .upsert_worktree(&WorktreeRecord {
            id: created.workspace_id,
            source_repo: created.source_repo.clone(),
            worktree_path: created.worktree_path.clone(),
            branch_name: created.branch_name.clone(),
            base_branch: created.base_branch.clone(),
            base_commit: created.base_commit.clone(),
            status: WorktreeStatus::Active,
            created_at: now.clone(),
            updated_at: now,
        })
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn demo_session_events() -> Vec<SessionEvent> {
    let session_id = SessionId::new();
    let tool_call_id = ToolCallId::new();
    let request_id = PermissionRequestId::new();

    vec![
        SessionEvent::Status {
            session_id,
            status: SessionStatus::Starting,
        },
        SessionEvent::TextDelta {
            session_id,
            text: "Inspecting the workspace.".to_string(),
        },
        SessionEvent::ToolCallStart {
            session_id,
            tool_call_id,
            name: "read_file".to_string(),
            input_json: json!({ "path": "README.md" }),
        },
        SessionEvent::ToolCallProgress {
            session_id,
            tool_call_id,
            message: "Reading README.md".to_string(),
        },
        SessionEvent::ToolCallResult {
            session_id,
            tool_call_id,
            title: Some("Read File".to_string()),
            metadata: json!({ "path": "README.md" }),
            output: "# Inductor\n".to_string(),
            exit_code: None,
        },
        SessionEvent::PermissionRequest {
            session_id,
            request_id,
            reason: "write_file wants to modify README.md".to_string(),
            tool_name: "write_file".to_string(),
            input_json: json!({ "path": "README.md" }),
        },
        SessionEvent::TerminalOutput {
            session_id,
            chunk: "cargo test\n".to_string(),
        },
        SessionEvent::Result {
            session_id,
            stop_reason: StopReason::EndTurn,
        },
    ]
}
