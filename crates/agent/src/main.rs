use std::path::{Path, PathBuf};

use auth::{AuthDetector, ProviderKind, RuntimeCredentialLoader};
use base64::{Engine as _, engine::general_purpose};
use clap::{Parser, Subcommand, ValueEnum};
use context::{
    ApproxTokenCounter, ContextLimits, ContextMessage, ModelEffort, ProviderFamily, TokenCounter,
    compact_messages, prepare_context, translate_effort,
};
use diff::{DiffRequest, diff_worktree};
use futures_util::StreamExt;
use git::{CreateWorktreeRequest, MergeOutcome, MergeRequest, WorktreeManager};
use harness_core::{
    ApprovalPolicy, ImageAttachment, PermissionDecision, PermissionRequestId, PermissionResponse,
    ProviderId, SessionEvent, SessionId, SessionStatus, StopReason, ToolCallId, TurnRequest,
    WorkspaceId,
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
use provider_core::{ProviderAuth, ProviderAuthKind, ProviderPlugin};
use secrecy::SecretString;
use serde_json::json;
use session_naming::{SessionNamingConfig, generate_session_name};
use std::time::{Duration, Instant};
use terminal::{PtyManager, SpawnTerminalRequest, TerminalSize};
use tokio_util::sync::CancellationToken;
use tools::{StructuredPatch, TextEdit, ToolRuntime};

mod tui;

const MAX_PROMPT_IMAGE_BYTES: usize = 5 * 1024 * 1024;

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
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    /// Open the terminal UI vertical slice.
    Tui {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        #[arg(long, value_enum, default_value_t = ProviderArg::Claude)]
        provider: ProviderArg,

        #[arg(long)]
        model: Option<String>,

        #[arg(long)]
        state_db: Option<PathBuf>,

        #[arg(long, default_value = "HEAD")]
        diff_base: String,
    },
    /// Run the experimental OpenTUI/Solid presentation layer.
    OpenTui {
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        #[arg(long, value_enum, default_value_t = ProviderArg::Claude)]
        provider: ProviderArg,

        #[arg(long)]
        model: Option<String>,

        /// When to pause tool calls for approval.
        #[arg(long, value_enum, default_value_t = ApprovalArg::Mutating)]
        approval: ApprovalArg,
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

        /// When to pause tool calls for approval.
        #[arg(long, value_enum, default_value_t = ApprovalArg::OnRequest)]
        approval: ApprovalArg,

        /// Auto-approve every prompt instead of asking on the terminal.
        #[arg(long)]
        yes: bool,

        /// Disable the macOS bash sandbox (writes outside the workspace).
        #[arg(long)]
        no_sandbox: bool,

        #[arg(long, default_value_t = 16_000)]
        soft_tokens: usize,

        #[arg(long, default_value_t = 24_000)]
        hard_tokens: usize,

        #[arg(long, default_value_t = 16 * 1024)]
        tool_result_inline_bytes: usize,

        #[arg(long)]
        blob_root: Option<PathBuf>,

        #[arg(long, value_enum, default_value_t = EffortArg::Medium)]
        effort: EffortArg,

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
}

#[derive(Debug, Subcommand)]
enum ProviderCommand {
    InspectAuth {
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
    Generic,
}

impl From<EffortProviderArg> for ProviderFamily {
    fn from(value: EffortProviderArg) -> Self {
        match value {
            EffortProviderArg::Claude => Self::Claude,
            EffortProviderArg::Codex => Self::Codex,
            EffortProviderArg::Generic => Self::Generic,
        }
    }
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
enum ProviderArg {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ModeArg {
    /// Edit the given workspace directory directly (default).
    InPlace,
    /// Run the agent inside an isolated git worktree so multiple sessions can
    /// work on the same repo in parallel and merge back later.
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
        }
    }
}

impl std::fmt::Display for ProviderArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderArg::Claude => write!(f, "claude"),
            ProviderArg::Codex => write!(f, "codex"),
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

        /// Record the worktree in this app DB so it can be merged back later.
        #[arg(long)]
        app_db: Option<PathBuf>,
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
    /// Merge a worktree branch back into its base branch in the source repo.
    Merge {
        #[arg(long)]
        workspace_id: WorkspaceId,

        #[arg(long)]
        app_db: Option<PathBuf>,

        /// Branch to merge into. Defaults to the worktree's base branch.
        #[arg(long)]
        target: Option<String>,

        /// Always create a merge commit, even when a fast-forward is possible.
        #[arg(long)]
        no_ff: bool,

        #[arg(long)]
        json: bool,
    },
    /// Abort an in-progress (conflicted) merge in a worktree's source repo.
    AbortMerge {
        #[arg(long)]
        workspace_id: WorkspaceId,

        #[arg(long)]
        app_db: Option<PathBuf>,
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
        Some(Command::Db { command }) => run_db_command(command).await,
        Some(Command::Tui {
            workspace,
            provider,
            model,
            state_db,
            diff_base,
        }) => {
            let provider_kind = ProviderKind::from(provider);
            tui::run(tui::TuiOptions {
                workspace,
                provider: provider.to_string(),
                model: model.unwrap_or_else(|| default_provider_model(provider_kind).to_string()),
                state_db,
                diff_base,
            })
            .await
            .map_err(|err| err.to_string())
        }
        Some(Command::OpenTui {
            workspace,
            provider,
            model,
            approval,
        }) => run_opentui_command(workspace, provider, model, approval).await,
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
            no_sandbox,
            soft_tokens,
            hard_tokens,
            tool_result_inline_bytes,
            blob_root,
            effort,
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
                no_sandbox,
                soft_tokens,
                hard_tokens,
                tool_result_inline_bytes,
                blob_root,
                effort,
                app_db,
                state_db,
                session_id,
                workspace_id,
            )
            .await
        }
        Some(Command::Session { command }) => run_session_command(command).await,
        Some(Command::Tool { command }) => run_tool_command(command).await,
        Some(Command::Terminal { command }) => run_terminal_command(command).await,
        Some(Command::Worktree { command }) => run_worktree_command(command).await,
        None => {
            run_opentui_command(
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                ProviderArg::Claude,
                None,
                ApprovalArg::Mutating,
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
            }
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

            let provider_auth = match provider {
                ProviderKind::Claude => ProviderAuth::new(
                    ProviderAuthKind::SessionToken,
                    SecretString::from(String::new()),
                ),
                ProviderKind::Codex => RuntimeCredentialLoader::load(reference)
                    .map_err(|err| err.to_string())?
                    .into_provider_auth(),
            };
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
            let mut stream = match provider {
                ProviderKind::Claude => ClaudeProvider::new()
                    .map_err(|err| err.to_string())?
                    .stream_turn(&provider_auth, request, cancel, perm_rx, tool_rx)
                    .await
                    .map_err(|err| err.to_string())?,
                ProviderKind::Codex => CodexProvider::new()
                    .map_err(|err| err.to_string())?
                    .stream_turn(&provider_auth, request, cancel, perm_rx, tool_rx)
                    .await
                    .map_err(|err| err.to_string())?,
            };

            while let Some(event) = stream.next().await {
                let event = event.map_err(|err| err.to_string())?;
                let line = serde_json::to_string(&event).map_err(|err| err.to_string())?;
                println!("{line}");
            }
        }
    }

    Ok(())
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
) -> Result<(), String> {
    let repo_root = resolve_repo_root()?;
    let tui_dir = repo_root.join("packages").join("tui");
    if !tui_dir.join("src").join("index.tsx").exists() {
        return Err(format!(
            "OpenTUI frontend not found at {}",
            tui_dir.display()
        ));
    }

    let backend_bin = std::env::current_exe()
        .map_err(|err| format!("could not resolve current executable: {err}"))?;
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or(workspace));

    // The worktree registry lives in the app DB; share its path with the TUI so
    // the dashboard can list/merge/archive worktrees the backend creates.
    let app_db = default_app_db_path()?;

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
    command.current_dir(&tui_dir);

    if let Some(model) = model {
        command.arg("--model").arg(model);
    }

    let status = command
        .status()
        .map_err(|err| format!("failed to launch OpenTUI frontend with bun: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("OpenTUI frontend exited with {status}"))
    }
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
                &ContextLimits::new(soft_tokens, hard_tokens, 16 * 1024),
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
    no_sandbox: bool,
    soft_tokens: usize,
    hard_tokens: usize,
    tool_result_inline_bytes: usize,
    blob_root: Option<PathBuf>,
    effort: EffortArg,
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
    let mut app_db = app_db;
    let mut forced_workspace_id = requested_workspace_id;
    let mut forced_state_db: Option<PathBuf> = None;
    let mut generated_display_name: Option<String> = None;
    if mode == ModeArg::Worktree {
        if app_db.is_none() {
            app_db = Some(default_app_db_path()?);
        }
        let registry = AppDb::open(app_db.as_ref().unwrap()).map_err(|err| err.to_string())?;

        // Resuming an existing worktree-bound session reuses its worktree and
        // name; only a brand-new session needs a fresh worktree + a name.
        let resuming = requested_session_id
            .and_then(|sid| registry.get_session(sid).ok().flatten())
            .and_then(|session| registry.get_worktree(session.workspace_id).ok().flatten())
            .is_some();

        // For a fresh session, name the worktree after the chat (<=3 words) and
        // use that as both the branch slug and the session's display name.
        let mut effective_slug = slug.clone();
        if !resuming {
            if let Some(name) = derive_worktree_name(&prompt).await {
                effective_slug = Some(name.clone());
                generated_display_name = Some(name);
            }
        }

        let binding =
            resolve_worktree(&registry, &workspace, requested_session_id, effective_slug.as_deref())?;
        eprintln!(
            "worktree: {} (workspace {})",
            binding.worktree_path.display(),
            binding.workspace_id
        );
        workspace = binding.worktree_path;
        forced_workspace_id = Some(binding.workspace_id);
        // Keep the session's state.db outside the worktree so merging/archiving
        // (which delete the worktree dir) preserve the chats.
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
    };

    // Build the provider as a trait object so the harness loop can drive
    // either backend through `&dyn ProviderPlugin`.
    let provider_plugin: Box<dyn ProviderPlugin> = match provider {
        ProviderKind::Claude => Box::new(ClaudeProvider::new().map_err(|err| err.to_string())?),
        ProviderKind::Codex => Box::new(CodexProvider::new().map_err(|err| err.to_string())?),
    };

    // Sandbox bash by default (writes confined to the workspace + tempdir,
    // network denied) unless the user opts out.
    let workspace_path = workspace.clone();
    let tools = if no_sandbox {
        ToolRuntime::new(workspace_path.clone())
    } else {
        ToolRuntime::sandboxed(workspace_path.clone())
    }
    .map_err(|err| err.to_string())?;

    let state_db_path = state_db
        .or(forced_state_db)
        .unwrap_or_else(|| workspace_state_path(&workspace_path));
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
    // Apply the chat-derived worktree name as the session display name so the
    // dashboard shows it immediately (the end-of-turn namer then leaves it be).
    if session_record.display_name.is_none() {
        if let Some(name) = generated_display_name.take() {
            session_record.display_name = Some(name);
        }
    }
    session_record.updated_at = now_rfc3339().map_err(|err| err.to_string())?;
    workspace_db
        .upsert_session(&session_record)
        .map_err(|err| err.to_string())?;

    if let Some(ref app_db_path) = app_db {
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
    }

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

    let mut config = HarnessConfig::new(model);
    config.max_tool_rounds = max_tool_rounds;
    config.approval_policy = ApprovalPolicy::from(approval);
    config.context.limits = ContextLimits::new(soft_tokens, hard_tokens, tool_result_inline_bytes);
    config.context.blob_root = blob_root;
    config.model_effort = ModelEffort::from(effort);
    config.provider_family = match provider {
        ProviderKind::Claude => ProviderFamily::Claude,
        ProviderKind::Codex => ProviderFamily::Codex,
    };
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
                }
            }
        });
    }
    let approver: &dyn Approver = if yes { &auto } else { &channel_approver };

    let approval_policy_dbg = config.approval_policy;
    let prompt = attach_prompt_image_mentions(&workspace_path, &prompt);

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
    );

    dlog(&format!(
        "run start: provider={} approval={:?} yes={yes}",
        provider_id.0, approval_policy_dbg
    ));

    let mut final_status = SessionStatus::Completed;
    while let Some(event) = stream.next().await {
        let event = event.map_err(|err| err.to_string())?;
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
        if matches!(
            event,
            SessionEvent::Result {
                stop_reason: StopReason::Interrupted,
                ..
            }
        ) {
            final_status = SessionStatus::Idle;
        }
        persist_event(&workspace_db, &event).map_err(|err| err.to_string())?;
        let line = serde_json::to_string(&event).map_err(|err| err.to_string())?;
        println!("{line}");
    }
    drop(stream);

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

    // Update app database if it exists
    if let Some(ref app_db_path) = app_db {
        let app_db_conn = AppDb::open(app_db_path).map_err(|err| err.to_string())?;
        app_db_conn
            .upsert_session(&session_record)
            .map_err(|err| err.to_string())?;
    }

    Ok(())
}

fn stored_message_to_transcript(message: StoredMessage) -> Result<TranscriptMessage, String> {
    let role = message
        .role
        .parse::<Role>()
        .map_err(|err| err.to_string())?;
    Ok(TranscriptMessage::new(role, message.content))
}

fn persist_event(db: &WorkspaceDb, event: &SessionEvent) -> persistence::Result<()> {
    match event {
        SessionEvent::Status { session_id, .. }
        | SessionEvent::TextDelta { session_id, .. }
        | SessionEvent::TextStart { session_id, .. }
        | SessionEvent::TextEnd { session_id, .. }
        | SessionEvent::ReasoningStart { session_id, .. }
        | SessionEvent::ReasoningDelta { session_id, .. }
        | SessionEvent::ReasoningEnd { session_id, .. }
        | SessionEvent::ContextPrepared { session_id, .. }
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
        | SessionEvent::PermissionRequest { session_id, .. }
        | SessionEvent::PermissionResolved { session_id, .. }
        | SessionEvent::TerminalOutput { session_id, .. }
        | SessionEvent::Result { session_id, .. }
        | SessionEvent::Usage { session_id, .. }
        | SessionEvent::Error { session_id, .. } => {
            db.append_event(*session_id, event)?;
        }
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

fn default_provider_model(provider: ProviderKind) -> &'static str {
    match provider {
        ProviderKind::Claude => "sonnet",
        ProviderKind::Codex => "gpt-5.5",
    }
}

fn attach_prompt_image_mentions(workspace: &Path, prompt: &str) -> String {
    const PREFIX: &str = "__MULTIMODAL_MESSAGE__:";
    if prompt.starts_with(PREFIX) {
        return prompt.to_string();
    }

    let mut images = Vec::new();
    for rel in image_mentions(prompt) {
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
    }

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
        } => {
            let manager = WorktreeManager::new(managed_root.unwrap_or(default_managed_root()?));
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

            println!("workspace_id: {}", worktree.workspace_id);
            println!("source_repo: {}", worktree.source_repo.display());
            println!("worktree_path: {}", worktree.worktree_path.display());
            println!("branch_name: {}", worktree.branch_name);
            println!("base_branch: {}", worktree.base_branch);
            println!("base_commit: {}", worktree.base_commit);
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
            json: json_output,
        } => {
            let registry = open_worktree_registry(app_db)?;
            let worktrees = registry.list_worktrees().map_err(|err| err.to_string())?;
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
                        json!({
                            "workspace_id": worktree.id,
                            "source_repo": worktree.source_repo.display().to_string(),
                            "worktree_path": worktree.worktree_path.display().to_string(),
                            "state_db": worktree_state_db_path(worktree.id)
                                .ok()
                                .map(|path| path.display().to_string()),
                            "branch_name": worktree.branch_name,
                            "base_branch": worktree.base_branch,
                            "status": worktree.status.as_str(),
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
        WorktreeCommand::Merge {
            workspace_id,
            app_db,
            target,
            no_ff,
            json: json_output,
        } => {
            let registry = open_worktree_registry(app_db)?;
            let worktree = lookup_worktree(&registry, workspace_id)?;
            let target_branch = target.unwrap_or_else(|| worktree.base_branch.clone());

            let manager = WorktreeManager::new(default_managed_root()?);
            let outcome = manager
                .merge_branch(MergeRequest {
                    source_repo: worktree.source_repo.clone(),
                    branch_name: worktree.branch_name.clone(),
                    target_branch: target_branch.clone(),
                    base_commit: worktree.base_commit.clone(),
                    no_ff,
                })
                .map_err(|err| err.to_string())?;

            // Report the result. On a clean merge the working directory is no
            // longer needed, so remove it; the registry record and the
            // session's chats (state.db lives outside the worktree) are kept.
            let result = match outcome {
                MergeOutcome::UpToDate => {
                    registry
                        .set_worktree_status(workspace_id, WorktreeStatus::Merged)
                        .map_err(|err| err.to_string())?;
                    cleanup_worktree_dir(&worktree)?;
                    json!({ "result": "up_to_date", "target": target_branch })
                }
                MergeOutcome::Merged {
                    merged_commit,
                    fast_forward,
                } => {
                    registry
                        .set_worktree_status(workspace_id, WorktreeStatus::Merged)
                        .map_err(|err| err.to_string())?;
                    cleanup_worktree_dir(&worktree)?;
                    json!({
                        "result": "merged",
                        "commit": merged_commit,
                        "fast_forward": fast_forward,
                        "target": target_branch,
                    })
                }
                MergeOutcome::Conflict { files } => {
                    json!({
                        "result": "conflict",
                        "target": target_branch,
                        "source_repo": worktree.source_repo.display().to_string(),
                        "files": files
                            .iter()
                            .map(|file| file.display().to_string())
                            .collect::<Vec<_>>(),
                    })
                }
            };

            if json_output {
                println!("{}", serde_json::to_string(&result).map_err(|err| err.to_string())?);
            } else {
                match result["result"].as_str() {
                    Some("up_to_date") => {
                        println!("merge: up-to-date ({target_branch} already contains the branch)")
                    }
                    Some("merged") => println!(
                        "merge: ok commit={} fast_forward={} target={target_branch}",
                        result["commit"].as_str().unwrap_or_default(),
                        result["fast_forward"].as_bool().unwrap_or_default()
                    ),
                    _ => {
                        let files = result["files"].as_array().map(Vec::len).unwrap_or_default();
                        println!("merge: conflict in {files} file(s):");
                        if let Some(items) = result["files"].as_array() {
                            for file in items {
                                println!("  {}", file.as_str().unwrap_or_default());
                            }
                        }
                        println!(
                            "resolve in {} then commit, or run `worktree abort-merge --workspace-id {workspace_id}`",
                            worktree.source_repo.display()
                        );
                    }
                }
            }
        }
        WorktreeCommand::AbortMerge {
            workspace_id,
            app_db,
        } => {
            let registry = open_worktree_registry(app_db)?;
            let worktree = lookup_worktree(&registry, workspace_id)?;

            let manager = WorktreeManager::new(default_managed_root()?);
            manager
                .abort_merge(&worktree.source_repo)
                .map_err(|err| err.to_string())?;

            println!("merge aborted in {}", worktree.source_repo.display());
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

fn lookup_worktree(registry: &AppDb, workspace_id: WorkspaceId) -> Result<WorktreeRecord, String> {
    registry
        .get_worktree(workspace_id)
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("no managed worktree for workspace {workspace_id}"))
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

        let prompt = attach_prompt_image_mentions(&workspace, "describe @screen.png");
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
}

fn default_managed_root() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Inductor")
        .join("worktrees"))
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
/// keyed by workspace id. Kept OUTSIDE the worktree directory so merging or
/// archiving (which delete the worktree dir) never destroy the chats/messages.
fn worktree_state_db_path(workspace_id: WorkspaceId) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;

    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("Inductor")
        .join("state")
        .join(format!("{workspace_id}.db")))
}

/// Generate a short (<=3 word) name for a fresh worktree-mode session from its
/// first prompt, reusing the session-naming model. Returns `None` on any
/// failure (missing creds, model error) so worktree creation falls back to a
/// generic slug rather than blocking.
async fn derive_worktree_name(prompt: &str) -> Option<String> {
    // The prompt may be wrapped for multimodal payloads; pull the text back out
    // so the namer sees the user's words, not a JSON blob.
    let text = prompt
        .strip_prefix("__MULTIMODAL_MESSAGE__:")
        .and_then(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
        .and_then(|value| value["text"].as_str().map(str::to_string))
        .unwrap_or_else(|| prompt.to_string());

    match generate_session_name(&[text], Some(SessionNamingConfig::default())).await {
        Ok(name) if name != "New Session" && !name.trim().is_empty() => Some(name),
        _ => None,
    }
}

/// Where a worktree-mode session should run and the workspace id that ties it
/// to the worktree registry.
struct WorktreeBinding {
    workspace_id: WorkspaceId,
    worktree_path: PathBuf,
}

/// Resolve the worktree a worktree-mode `run` should execute in: reuse the one
/// bound to the resumed session, otherwise create a fresh isolated worktree off
/// `source_repo` and record it (and its workspace) in the app DB.
fn resolve_worktree(
    registry: &AppDb,
    source_repo: &Path,
    requested_session_id: Option<SessionId>,
    slug: Option<&str>,
) -> Result<WorktreeBinding, String> {
    // Resume: if the session already lives in a managed worktree, reuse it.
    if let Some(session_id) = requested_session_id {
        if let Some(session) = registry.get_session(session_id).map_err(|err| err.to_string())? {
            if let Some(worktree) = registry
                .get_worktree(session.workspace_id)
                .map_err(|err| err.to_string())?
            {
                return Ok(WorktreeBinding {
                    workspace_id: worktree.id,
                    worktree_path: worktree.worktree_path,
                });
            }
        }
    }

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
        worktree_path: created.worktree_path,
    })
}

/// Record a freshly created worktree (and its workspace) in the app DB so it
/// shows up in the registry and can be merged back later.
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
