use std::{
    cell::Cell,
    io::{self, BufRead, BufReader, Write as _},
    path::PathBuf,
    process::{Child, ChildStdin, Command as ProcessCommand, Stdio},
    sync::{
        OnceLock,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose};
use image::GenericImageView;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use context::{DEFAULT_CONTEXT_SOFT_TOKENS, MAX_CONTEXT_TOKENS};
use diff::{DiffLineKind, DiffRequest, FileStatus, diff_worktree};
use harness_core::SessionId;
use persistence::{WorkspaceDb, workspace_state_path};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Padding, Paragraph},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{Style as SyntectStyle, Theme, ThemeSet},
    parsing::SyntaxSet,
};

/// Semantic color palette, synthesized from the opencode / Codex / Claude Code
/// TUIs: stay transparent to the terminal background, lean on a muted gray for
/// secondary text, and use a small set of accent colors for status.
mod theme {
    use ratatui::style::Color;

    pub const FG: Color = Color::Rgb(0xE6, 0xED, 0xF3);
    pub const MUTED: Color = Color::Rgb(0x8B, 0x94, 0x9E);
    pub const FAINT: Color = Color::Rgb(0x3A, 0x3F, 0x44);
    pub const ACCENT: Color = Color::Rgb(0x58, 0xA6, 0xFF);
    pub const BRAND: Color = Color::Rgb(0xA7, 0x8B, 0xFA);
    pub const SELECTION_BG: Color = Color::Rgb(0x36, 0x4A, 0x6E);
    pub const SELECTION_FG: Color = Color::Rgb(0xE8, 0xEE, 0xFF);
    pub const SUCCESS: Color = Color::Rgb(0x3F, 0xB9, 0x50);
    pub const WARNING: Color = Color::Rgb(0xD2, 0x99, 0x22);
    pub const ERROR: Color = Color::Rgb(0xF8, 0x51, 0x49);
    pub const ADD: Color = Color::Rgb(0x6A, 0xE3, 0x69);
    pub const ADD_BG: Color = Color::Rgb(0x06, 0x2F, 0x08);
    pub const REM: Color = Color::Rgb(0xFF, 0x7B, 0x72);
    pub const REM_BG: Color = Color::Rgb(0x4A, 0x08, 0x08);
    /// Background highlight for the user's own messages (gray block).
    pub const USER_BG: Color = Color::Rgb(0x2A, 0x2D, 0x32);
    /// Neutral border for the few framed regions (prompt box, popups).
    pub const BORDER: Color = Color::Rgb(0x6E, 0x76, 0x81);
}

/// Braille spinner frames for the live "working…" indicator.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Maximum completion rows shown in the popup.
const MAX_COMPLETIONS: usize = 8;
/// Maximum bytes of a referenced file injected into the prompt.
const MAX_MENTION_BYTES: usize = 12_000;
/// Directories we never index or suggest.
const IGNORED_DIRS: [&str; 6] = [
    ".git",
    "target",
    "node_modules",
    ".inductor",
    "dist",
    "build",
];

pub struct TuiOptions {
    pub workspace: PathBuf,
    pub provider: String,
    pub model: String,
    pub state_db: Option<PathBuf>,
    pub diff_base: String,
}

pub async fn run(options: TuiOptions) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    enable_basic_mouse(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = run_app(&mut terminal, options);
    disable_raw_mode()?;
    disable_mouse_tracking(terminal.backend_mut())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen,)?;
    terminal.show_cursor()?;
    result
}

fn enable_basic_mouse(out: &mut impl io::Write) -> io::Result<()> {
    // Use normal + button-event mouse tracking with SGR encoding. This gives
    // us wheel events plus drag events for app-level selection without enabling
    // any-event tracking (?1003), the mode that captures every mouse movement.
    write!(out, "\x1b[?1003l\x1b[?1000h\x1b[?1002h\x1b[?1006h")?;
    out.flush()
}

fn enable_selection_mouse(out: &mut impl io::Write) -> io::Result<()> {
    // Temporarily ask for motion events while dragging so the in-app selection
    // tracks the cursor smoothly, then drop back to wheel/drag mode on release.
    write!(out, "\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h")?;
    out.flush()
}

fn disable_mouse_tracking(out: &mut impl io::Write) -> io::Result<()> {
    write!(
        out,
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l"
    )?;
    out.flush()
}

fn copy_to_clipboard(text: &str) -> io::Result<()> {
    if text.trim().is_empty() {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = ProcessCommand::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(text.as_bytes())?;
            }
            if child.wait().map(|status| status.success()).unwrap_or(false) {
                return Ok(());
            }
        }
    }

    let encoded = general_purpose::STANDARD.encode(text.as_bytes());
    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    options: TuiOptions,
) -> anyhow::Result<()> {
    let mut app = App::new(options);

    loop {
        terminal.draw(|frame| render(frame, &app))?;
        // Drain any streamed output from a background run.
        app.poll_run();
        app.poll_usage();
        app.tick = app.tick.wrapping_add(1);
        // Short poll so the spinner animates and streamed output appears live.
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) => {
                    if app.handle_key(key) {
                        break;
                    }
                }
                Event::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        app.ensure_mouse_mode();
                        app.scroll_wheel(-1);
                    }
                    MouseEventKind::ScrollDown => {
                        app.ensure_mouse_mode();
                        app.scroll_wheel(1);
                    }
                    MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                        if mouse.modifiers.contains(KeyModifiers::CONTROL) {
                            app.click_at(mouse.column, mouse.row);
                        } else {
                            app.start_selection(mouse.column, mouse.row);
                        }
                    }
                    MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
                        app.update_selection(mouse.column, mouse.row);
                    }
                    MouseEventKind::Moved => {
                        if app.selection.is_some() {
                            app.update_selection(mouse.column, mouse.row);
                        }
                    }
                    MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
                        app.finish_selection(mouse.column, mouse.row);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    Ok(())
}

/// One rendered turn in the single-pane conversation.
enum ChatEntry {
    User(String),
    /// Formatted agent prose + inline tool log (from the NDJSON event stream).
    Agent(String),
    /// Inline permission prompt kept in the transcript.
    Permission(PermissionEntry),
    /// A colored diff block (green add / red remove) shown after file changes.
    Diff(Vec<DiffRow>),
    Error(String),
}

#[derive(Clone)]
struct PermissionEntry {
    request_id: String,
    tool_name: String,
    reason: String,
    input_json: serde_json::Value,
    decision: Option<String>,
    message: Option<String>,
}

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    /// Startup splash; Enter starts a session.
    Welcome,
    /// The live conversation.
    Session,
}

/// Reasoning effort level, passed to `inductor run --effort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Effort {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl Effort {
    const ALL: [Effort; 5] = [
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
    ];

    fn as_arg(self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
        }
    }
}

/// A `/`-command palette: the top-level command list, or a value picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaletteKind {
    Commands,
    Models,
    Efforts,
    Sessions,
    Permissions,
    PrActions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PrFlow {
    BaseBranch,
    CommitMessage { base: String },
}

struct Palette {
    kind: PaletteKind,
    items: Vec<String>,
    index: usize,
}

const COMMANDS: [&str; 11] = [
    "/model",
    "/effort",
    "/usage",
    "/fast",
    "/sessions",
    "/resume",
    "/permissions",
    "/pr",
    "/compact",
    "/clear",
    "/help",
];

fn is_command_prompt(prompt: &str) -> bool {
    let trimmed = prompt.trim();
    prompt.starts_with('/') && !prompt.contains(char::is_whitespace) && COMMANDS.contains(&trimmed)
}

/// All `(provider, model)` pairs offered by `/model` — Claude and OpenAI
/// models together. The model string passes straight to `--model`.
fn model_catalog() -> Vec<(&'static str, &'static str)> {
    vec![
        ("claude", "opus"),
        ("claude", "sonnet"),
        ("claude", "haiku"),
        ("codex", "gpt-5.5"),
        ("codex", "gpt-5.4"),
        ("codex", "gpt-5.4-mini"),
        ("codex", "gpt-5.6-sol"),
        ("codex", "gpt-5.6-terra"),
        ("codex", "gpt-5.6-luna"),
    ]
}

fn model_display(provider: &str, model: &str) -> String {
    let label = if provider == "codex" {
        "openai"
    } else {
        provider
    };
    format!("{label} · {model}")
}

/// Approval policies offered by `/permissions` (passed to `--approval`).
const PERMISSION_MODES: [&str; 5] = ["never", "mutating", "on-request", "on-failure", "always"];

/// Whether a provider reports a limit as "% used" or "% left". We display it
/// the way the provider gives it, with no conversion. Future providers can pick
/// either via the parser that builds the `LimitWindow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Metric {
    Used,
    Left,
}

impl Metric {
    fn label(self) -> &'static str {
        match self {
            Metric::Used => "used",
            Metric::Left => "left",
        }
    }
}

/// One rolling rate-limit window scraped from a provider's TUI, kept in the
/// provider's native polarity.
struct LimitWindow {
    /// The percentage exactly as the provider reports it.
    percent: f64,
    metric: Metric,
    /// Human reset label, e.g. "resets 16:16" or "Resets Jun 14 at 10:30am".
    reset_label: Option<String>,
}

/// The 5-hour and weekly limit windows scraped from the provider's TUI.
/// Context usage is tracked separately by Inductor (see `App::context_used`).
#[derive(Default)]
struct ProviderUsage {
    five_hour: Option<LimitWindow>,
    weekly: Option<LimitWindow>,
    /// Where the data came from, or why a metric is unavailable.
    note: String,
}

/// Inductor uses one provider-independent input ceiling even when a model
/// advertises a larger native window.
fn context_window_for(_model: &str) -> u64 {
    MAX_CONTEXT_TOKENS as u64
}

/// Compact at 200k, retaining a 50k reserve below the hard ceiling.
const AUTO_COMPACT_PCT: f64 =
    DEFAULT_CONTEXT_SOFT_TOKENS as f64 / MAX_CONTEXT_TOKENS as f64;

/// Read provider limit windows by scraping the provider's TUI.
fn read_provider_usage(provider: &str) -> ProviderUsage {
    match provider {
        "codex" => read_codex_usage(),
        "claude" => read_claude_usage(),
        other => ProviderUsage {
            note: format!("no local usage source for provider '{other}'"),
            ..Default::default()
        },
    }
}

/// Codex usage: scrape the `codex` TUI's `/status` view for 5h + weekly limits.
fn read_codex_usage() -> ProviderUsage {
    let screen = scrape_tui_screen("codex", "/status", &["5h limit", "Weekly limit"]);
    let (five, weekly) = screen
        .as_deref()
        .map(parse_codex_status)
        .unwrap_or((None, None));
    let ok = five.is_some() || weekly.is_some();
    ProviderUsage {
        five_hour: five,
        weekly,
        note: if ok {
            "5h/weekly scraped from `codex` /status (best-effort)".to_string()
        } else {
            "couldn't read Codex limits (the codex /status scrape failed)".to_string()
        },
    }
}

/// Claude usage: 5h/weekly scraped from the `claude` `/usage` view.
fn read_claude_usage() -> ProviderUsage {
    let screen = scrape_tui_screen("claude", "/usage", &["Current session", "% used"]);
    let (five, weekly) = screen
        .as_deref()
        .map(parse_claude_usage)
        .unwrap_or((None, None));
    let ok = five.is_some() || weekly.is_some();
    ProviderUsage {
        five_hour: five,
        weekly,
        note: if ok {
            "5h/weekly scraped from `claude` /usage (best-effort)".to_string()
        } else {
            "couldn't read Claude limits (the claude /usage scrape failed)".to_string()
        },
    }
}

/// Best-effort scrape of an agent CLI's TUI: spawn `bin` in a PTY, type
/// `command`, and return the rendered screen once a `ready` marker appears.
/// Inherently brittle (screen-scraping an interactive TUI) and fully isolated
/// so a failure only yields `None`.
fn scrape_tui_screen(bin: &str, command: &str, ready: &[&str]) -> Option<String> {
    use std::{thread::sleep, time::Duration};
    use terminal::{PtyManager, SpawnTerminalRequest, TerminalSize};

    let resolved = resolve_bin(bin)?;
    let mut manager = PtyManager::new();
    let mut request = SpawnTerminalRequest::new(std::env::temp_dir());
    request.shell = Some(resolved);
    request.size = TerminalSize::new(50, 160);
    let id = manager.spawn(request).ok()?;

    // Let the TUI fully boot, then type the command char-by-char (a single
    // chunk sent before the input is interactive gets dropped) and submit.
    sleep(Duration::from_millis(4000));
    for ch in command.bytes() {
        let _ = manager.write(id, &[ch]);
        sleep(Duration::from_millis(40));
    }
    sleep(Duration::from_millis(500));
    let _ = manager.write(id, b"\r");

    let mut contents = None;
    for _ in 0..25 {
        sleep(Duration::from_millis(400));
        if let Ok(snap) = manager.snapshot(id) {
            let hit = ready.iter().any(|m| snap.contents.contains(m));
            contents = Some(snap.contents);
            if hit {
                break;
            }
        }
    }
    let _ = manager.kill(id);
    contents
}

/// Parse Claude's `/usage` screen: two `NN% used` lines with `Resets …` labels.
fn parse_claude_usage(contents: &str) -> (Option<LimitWindow>, Option<LimitWindow>) {
    let mut percents: Vec<f64> = Vec::new();
    let mut resets: Vec<String> = Vec::new();
    for line in contents.lines() {
        if let Some(p) = percent_before(line, "% used") {
            percents.push(p);
        }
        if let Some(idx) = line.find("Resets") {
            resets.push(line[idx..].trim().to_string());
        }
    }
    let window = |i: usize| {
        percents.get(i).map(|&percent| LimitWindow {
            percent,
            metric: Metric::Used,
            reset_label: resets.get(i).cloned(),
        })
    };
    // First "% used" = current session (5h), second = current week.
    (window(0), window(1))
}

/// Parse Codex's `/status` screen: `5h limit: […] NN% left (resets …)` and the
/// matching `Weekly limit:` line. Polarity is kept native ("% left").
fn parse_codex_status(contents: &str) -> (Option<LimitWindow>, Option<LimitWindow>) {
    let mut five = None;
    let mut weekly = None;
    for line in contents.lines() {
        if line.contains("5h limit") {
            five = codex_window(line);
        } else if line.contains("Weekly limit") {
            weekly = codex_window(line);
        }
    }
    (five, weekly)
}

fn codex_window(line: &str) -> Option<LimitWindow> {
    // Codex reports "% left" (some builds "% used"); keep the native polarity.
    let (percent, metric) = if let Some(left) = percent_before(line, "% left") {
        (left, Metric::Left)
    } else {
        (percent_before(line, "% used")?, Metric::Used)
    };
    // Reset label lives inside "(resets …)".
    let reset_label = line.find("(resets").map(|i| {
        line[i + 1..]
            .split(')')
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    });
    Some(LimitWindow {
        percent,
        metric,
        reset_label,
    })
}

/// Extract the integer percentage immediately preceding `marker` in `line`.
fn percent_before(line: &str, marker: &str) -> Option<f64> {
    let pos = line.find(marker)?;
    let digits: String = line[..pos]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

/// Rough token estimate (chars/4) for the post-compaction summary size.
fn approx_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// `5m 10s` / `42s` style duration for the run stopwatch.
fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Resolve an executable on PATH via `which`.
fn resolve_bin(name: &str) -> Option<PathBuf> {
    let out = ProcessCommand::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Messages streamed from the background harness run to the UI.
enum RunEvent {
    /// One NDJSON event line from the harness stdout.
    Line(String),
    /// The harness stdout closed (process is exiting).
    Done,
}

/// Whether a run is a normal user turn or a context-compaction summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunKind {
    Normal,
    Compaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectionPoint {
    line: usize,
    col: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveSelection {
    anchor: SelectionPoint,
    focus: SelectionPoint,
}

/// A tool-permission request from the agent, awaiting the user's decision.
struct PendingPermission {
    request_id: String,
    tool_name: String,
    reason: String,
    input_json: serde_json::Value,
    transcript_index: usize,
    /// Highlighted option (0=Allow, 1=Allow all session, 2=No / deny).
    selected: usize,
    /// When denying, the optional message typed back to the agent.
    typing_message: bool,
    message: String,
}

/// State for an in-flight harness run, owned by the UI thread.
struct RunState {
    child: Child,
    /// The run child's stdin, used to send permission decisions back down.
    stdin: Option<ChildStdin>,
    pid: u32,
    rx: Receiver<RunEvent>,
    kind: RunKind,
    /// Assistant output accumulated in arrival order — prose and tool-call
    /// lines interleaved as the agent produces them (text, tool, text, tool…).
    body: String,
    /// When the run started (drives the live stopwatch).
    started: Instant,
    /// The harness session id observed in the event stream.
    session_seen: Option<String>,
    /// Latest provider-reported context size (input + cache_read tokens).
    ctx_used: u64,
    /// Worktree diff captured before this run, used to hide unrelated
    /// pre-existing changes from the end-of-turn diff summary.
    baseline_diff: Option<diff::DiffSummary>,
}

struct App {
    workspace: PathBuf,
    provider: String,
    model: String,
    state_db_path: PathBuf,
    diff_base: String,
    screen: Screen,
    prompt: String,
    /// Byte offset of the edit cursor within `prompt` (always on a char boundary).
    cursor: usize,
    completions: Vec<String>,
    completion_index: usize,
    completion_active: bool,
    /// Active `/`-command palette, if any.
    palette: Option<Palette>,
    /// In-progress `/pr` create flow.
    pr_flow: Option<PrFlow>,
    /// Current reasoning effort.
    effort: Effort,
    /// Fast mode: forces minimal effort while on, restoring `saved_effort` off.
    fast: bool,
    saved_effort: Effort,
    /// Approval policy passed to runs (`/permissions`).
    approval: String,
    /// Session to continue (`/resume`, or auto-captured from the last run).
    session_id: Option<String>,
    /// `/usage` overlay visibility + provider-level usage (read from disk).
    show_usage: bool,
    provider_usage: Option<ProviderUsage>,
    /// Background channel delivering scraped provider usage.
    usage_rx: Option<Receiver<ProviderUsage>>,
    /// True after one Esc; a second consecutive Esc pauses/interrupts.
    esc_armed: bool,
    /// An in-flight harness run, if any.
    run: Option<RunState>,
    /// A pending tool-permission prompt awaiting the user's decision.
    pending_permission: Option<PendingPermission>,
    /// Animation tick for the spinner.
    tick: usize,
    /// Current provider context size (input + cache_read of the latest turn).
    context_used: u64,
    /// Summary to seed the next (fresh) provider session after compaction.
    pending_seed: Option<String>,
    transcript: Vec<ChatEntry>,
    status: String,
    last_activity: Instant,
    /// First visible conversation line. When `follow_tail` is true, rendering
    /// keeps this pinned to the latest output.
    scroll_top: Cell<u16>,
    follow_tail: Cell<bool>,
    /// Max top offset + visible height from the last render, used to clamp
    /// keyboard scrolling without re-measuring in the key handler.
    view_max: Cell<u16>,
    view_h: Cell<u16>,
    /// Plain text of the conversation lines from the last render plus the index
    /// of the first visible one — used to resolve mouse clicks to file paths.
    view_text: std::cell::RefCell<Vec<String>>,
    view_first: Cell<usize>,
    /// Conversation area (x, y, w, h) from the last render, for click hit-tests.
    view_rect: Cell<(u16, u16, u16, u16)>,
    selection: Option<ActiveSelection>,
    last_click: Option<(SelectionPoint, Instant, u8)>,
    click_selection: bool,
    selection_dragged: bool,
    selection_visible: bool,
    selection_full_row: bool,
    wheel_accum: i16,
    /// True while the mouse is released to the terminal for text selection.
    /// Mouse mode is required for app-level trackpad/wheel scrolling.
    select_mode: bool,
    /// True after one ctrl+c; a second consecutive ctrl+c quits.
    ctrl_c_armed: bool,
    /// Previously submitted prompts, recalled with Up/Down in the composer.
    history: Vec<String>,
    /// Current position while browsing `history` (None = editing a fresh prompt).
    history_index: Option<usize>,
}

impl App {
    fn new(options: TuiOptions) -> Self {
        let state_db_path = options
            .state_db
            .unwrap_or_else(|| workspace_state_path(&options.workspace));
        Self {
            workspace: options.workspace,
            provider: options.provider,
            model: options.model,
            state_db_path,
            diff_base: options.diff_base,
            screen: Screen::Welcome,
            prompt: String::new(),
            cursor: 0,
            completions: Vec::new(),
            completion_index: 0,
            completion_active: false,
            palette: None,
            pr_flow: None,
            effort: Effort::Medium,
            fast: false,
            saved_effort: Effort::Medium,
            approval: "never".to_string(),
            session_id: None,
            show_usage: false,
            provider_usage: None,
            usage_rx: None,
            esc_armed: false,
            run: None,
            pending_permission: None,
            tick: 0,
            context_used: 0,
            pending_seed: None,
            transcript: Vec::new(),
            status: "Ready · type @ to reference files, enter to run".to_string(),
            last_activity: Instant::now(),
            scroll_top: Cell::new(0),
            follow_tail: Cell::new(true),
            view_max: Cell::new(0),
            view_h: Cell::new(0),
            view_text: std::cell::RefCell::new(Vec::new()),
            view_first: Cell::new(0),
            view_rect: Cell::new((0, 0, 0, 0)),
            selection: None,
            last_click: None,
            click_selection: false,
            selection_dragged: false,
            selection_visible: false,
            selection_full_row: false,
            wheel_accum: 0,
            select_mode: false,
            ctrl_c_armed: false,
            history: Vec::new(),
            history_index: None,
        }
    }

    fn context_window(&self) -> u64 {
        context_window_for(&self.model)
    }

    fn is_running(&self) -> bool {
        self.run.is_some()
    }

    fn enter_select_mode(&mut self) {
        if self.select_mode {
            return;
        }
        self.select_mode = true;
        let mut out = io::stdout();
        let _ = disable_mouse_tracking(&mut out);
    }

    fn ensure_mouse_mode(&mut self) {
        if !self.select_mode {
            return;
        }
        self.select_mode = false;
        let mut out = io::stdout();
        let _ = enable_basic_mouse(&mut out);
    }

    /// Clear the prompt and reset the edit cursor.
    fn clear_prompt(&mut self) {
        self.prompt.clear();
        self.cursor = 0;
    }

    /// Insert a char at the cursor and advance past it.
    fn insert_char(&mut self, ch: char) {
        self.prompt.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
    }

    /// Delete the char immediately before the cursor.
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prompt[..self.cursor]
            .chars()
            .next_back()
            .map(char::len_utf8)
            .unwrap_or(0);
        self.cursor -= prev;
        self.prompt
            .replace_range(self.cursor..self.cursor + prev, "");
    }

    /// Move the cursor one char left, respecting UTF-8 boundaries.
    fn cursor_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.prompt[..self.cursor]
            .chars()
            .next_back()
            .map(char::len_utf8)
            .unwrap_or(0);
        self.cursor -= prev;
    }

    /// Move the cursor one char right, respecting UTF-8 boundaries.
    fn cursor_right(&mut self) {
        if self.cursor >= self.prompt.len() {
            return;
        }
        let next = self.prompt[self.cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(0);
        self.cursor += next;
    }

    /// Scroll the conversation up (toward older output) by `lines`.
    fn scroll_up(&mut self, lines: u16) {
        let next = self.scroll_top.get().saturating_sub(lines);
        self.scroll_top.set(next);
        self.follow_tail.set(false);
    }

    /// Scroll the conversation down (toward the latest output) by `lines`.
    fn scroll_down(&mut self, lines: u16) {
        let max = self.view_max.get();
        let next = self.scroll_top.get().saturating_add(lines).min(max);
        self.scroll_top.set(next);
        self.follow_tail.set(next >= max);
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll_top.set(self.view_max.get());
        self.follow_tail.set(true);
    }

    fn scroll_wheel(&mut self, direction: i16) {
        const WHEEL_EVENTS_PER_LINE: i16 = 1;
        if self.wheel_accum.signum() != 0 && self.wheel_accum.signum() != direction.signum() {
            self.wheel_accum = 0;
        }
        self.wheel_accum += direction;
        if self.wheel_accum <= -WHEEL_EVENTS_PER_LINE {
            self.scroll_up(1);
            self.wheel_accum += WHEEL_EVENTS_PER_LINE;
        } else if self.wheel_accum >= WHEEL_EVENTS_PER_LINE {
            self.scroll_down(1);
            self.wheel_accum -= WHEEL_EVENTS_PER_LINE;
        }
    }

    fn selection_point_at(&self, col: u16, row: u16) -> Option<SelectionPoint> {
        let (vx, vy, vw, vh) = self.view_rect.get();
        if col < vx || col >= vx + vw || row < vy || row >= vy + vh {
            return None;
        }
        let line = self.view_first.get() + (row - vy) as usize;
        let max_col = self
            .view_text
            .borrow()
            .get(line)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        Some(SelectionPoint {
            line,
            col: ((col - vx) as usize).min(max_col),
        })
    }

    fn start_selection(&mut self, col: u16, row: u16) {
        self.ensure_mouse_mode();
        let Some(point) = self.selection_point_at(col, row) else {
            return;
        };
        let now = Instant::now();
        let click_count = self.click_count(point, now);
        self.last_click = Some((point, now, click_count));
        if click_count >= 3 && self.select_line_at(point) {
            self.click_selection = true;
            self.selection_dragged = false;
            self.selection_visible = true;
            self.selection_full_row = true;
            return;
        }
        if click_count == 2 && self.select_word_at(point) {
            self.click_selection = true;
            self.selection_dragged = false;
            self.selection_visible = true;
            self.selection_full_row = false;
            return;
        }
        self.click_selection = false;
        self.selection_dragged = false;
        self.selection_visible = false;
        self.selection_full_row = false;
        let mut out = io::stdout();
        let _ = enable_selection_mouse(&mut out);
        self.selection = Some(ActiveSelection {
            anchor: point,
            focus: point,
        });
    }

    fn update_selection(&mut self, col: u16, row: u16) {
        if self.click_selection {
            return;
        }
        let Some(point) = self.selection_point_at(col, row) else {
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            if selection.focus != point {
                self.selection_dragged = true;
                self.selection_visible = true;
                self.selection_full_row = true;
            }
            selection.focus = point;
        }
    }

    fn finish_selection(&mut self, col: u16, row: u16) {
        if !self.click_selection {
            self.update_selection(col, row);
        }
        let mut out = io::stdout();
        let _ = enable_basic_mouse(&mut out);
        if !self.click_selection && !self.selection_dragged {
            self.selection = None;
            self.selection_visible = false;
            return;
        }
        if !self.click_selection
            && let Some(selection) = self.selection.as_mut()
        {
            selection.focus.col = selection.focus.col.saturating_add(1);
        }
        self.click_selection = false;
        self.selection_dragged = false;
        let Some(selection) = self.selection else {
            return;
        };
        let text = self.selected_text(selection);
        if text.trim().is_empty() {
            self.selection = None;
            return;
        }
        match copy_to_clipboard(&text) {
            Ok(()) => {}
            Err(err) => self.status = format!("Could not copy selection: {err}"),
        }
    }

    fn click_count(&self, point: SelectionPoint, now: Instant) -> u8 {
        let Some((last_point, last_time, last_count)) = self.last_click else {
            return 1;
        };
        if now.duration_since(last_time) <= Duration::from_millis(450)
            && last_point.line == point.line
            && last_point.col.abs_diff(point.col) <= 2
        {
            last_count.saturating_add(1).min(3)
        } else {
            1
        }
    }

    fn select_word_at(&mut self, point: SelectionPoint) -> bool {
        let Some((start, end)) = self.word_range_at(point) else {
            return false;
        };
        self.selection = Some(ActiveSelection {
            anchor: SelectionPoint {
                line: point.line,
                col: start,
            },
            focus: SelectionPoint {
                line: point.line,
                col: end,
            },
        });
        true
    }

    fn select_line_at(&mut self, point: SelectionPoint) -> bool {
        let line_width = self
            .view_text
            .borrow()
            .get(point.line)
            .map(|text| text.chars().count())
            .unwrap_or(0);
        if line_width == 0 {
            return false;
        }
        self.selection = Some(ActiveSelection {
            anchor: SelectionPoint {
                line: point.line,
                col: 0,
            },
            focus: SelectionPoint {
                line: point.line,
                col: line_width,
            },
        });
        true
    }

    fn word_range_at(&self, point: SelectionPoint) -> Option<(usize, usize)> {
        let lines = self.view_text.borrow();
        let line = lines.get(point.line)?;
        let chars = line.chars().collect::<Vec<_>>();
        if chars.is_empty() {
            return None;
        }
        let mut idx = point.col.min(chars.len().saturating_sub(1));
        if !is_word_char(chars[idx]) {
            if idx > 0 && is_word_char(chars[idx - 1]) {
                idx -= 1;
            } else {
                return None;
            }
        }
        let mut start = idx;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        let mut end = idx + 1;
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }
        (end > start).then_some((start, end))
    }

    fn selected_text(&self, selection: ActiveSelection) -> String {
        let (start, end) = ordered_selection(selection);
        let lines = self.view_text.borrow();
        let mut out = Vec::new();
        for line_idx in start.line..=end.line {
            let Some(line) = lines.get(line_idx) else {
                continue;
            };
            let chars = line.chars().collect::<Vec<_>>();
            let from = if line_idx == start.line {
                start.col.min(chars.len())
            } else {
                0
            };
            let to = if line_idx == end.line {
                end.col.min(chars.len())
            } else {
                chars.len()
            };
            if to >= from {
                out.push(chars[from..to].iter().collect::<String>());
            }
        }
        out.join("\n")
    }

    /// A left click in the conversation: if the clicked word is a file path that
    /// exists (workspace-relative or absolute), open it with the system opener.
    fn click_at(&mut self, col: u16, row: u16) {
        let (vx, vy, vw, vh) = self.view_rect.get();
        if col < vx || col >= vx + vw || row < vy || row >= vy + vh {
            return;
        }
        let line_idx = self.view_first.get() + (row - vy) as usize;
        let text = {
            let lines = self.view_text.borrow();
            match lines.get(line_idx) {
                Some(line) => line.clone(),
                None => return,
            }
        };
        let Some(path) = path_token_at(&text, (col - vx) as usize) else {
            return;
        };
        let full = if std::path::Path::new(&path).is_absolute() {
            PathBuf::from(&path)
        } else {
            self.workspace.join(&path)
        };
        if !full.exists() {
            return;
        }
        #[cfg(target_os = "macos")]
        let opener = "open";
        #[cfg(all(unix, not(target_os = "macos")))]
        let opener = "xdg-open";
        #[cfg(not(unix))]
        let opener = "start";
        let ok = ProcessCommand::new(opener).arg(&full).spawn().is_ok();
        self.status = if ok {
            format!("Opened {}", short_path(&full.display().to_string()))
        } else {
            "Could not open file".to_string()
        };
    }

    /// Recall the previous submitted prompt into the composer.
    fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_index {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.set_prompt_from_history(idx);
    }

    /// Recall the next submitted prompt, or clear back to an empty draft.
    fn history_next(&mut self) {
        let Some(i) = self.history_index else {
            return;
        };
        if i + 1 < self.history.len() {
            self.set_prompt_from_history(i + 1);
        } else {
            self.history_index = None;
            self.clear_prompt();
            self.completion_active = false;
            self.palette = None;
        }
    }

    fn set_prompt_from_history(&mut self, idx: usize) {
        self.history_index = Some(idx);
        self.prompt = self.history[idx].clone();
        self.cursor = self.prompt.len();
        // Don't trigger @/slash popups while browsing history.
        self.completion_active = false;
        self.palette = None;
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // Ctrl+C: first press warns, a second consecutive press stops any
        // running agent (killing its process tree so tokens stop) and quits.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.ctrl_c_armed {
                if self.is_running() {
                    self.interrupt_run();
                }
                return true;
            }
            self.ctrl_c_armed = true;
            self.status = if self.is_running() {
                "Press ctrl+c again to stop the agent and quit".to_string()
            } else {
                "Press ctrl+c again to quit".to_string()
            };
            return false;
        }
        self.ctrl_c_armed = false;

        // Ctrl+S toggles between native terminal selection and TUI mouse mode.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            if self.select_mode {
                self.ensure_mouse_mode();
            } else {
                self.enter_select_mode();
            }
            return false;
        }

        // Welcome splash: Enter starts a session, nothing else does anything.
        if self.screen == Screen::Welcome {
            if key.code == KeyCode::Enter {
                self.screen = Screen::Session;
                self.status = "New session · type @ to reference files, enter to run".to_string();
                self.last_activity = Instant::now();
            }
            return false;
        }

        // Conversation scrolling works at any time (PageUp/PageDown, or
        // Ctrl+Up/Ctrl+Down for line-by-line).
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::PageUp => {
                self.scroll_up(self.view_h.get().saturating_sub(2).max(1));
                return false;
            }
            KeyCode::PageDown => {
                self.scroll_down(self.view_h.get().saturating_sub(2).max(1));
                return false;
            }
            KeyCode::Up if ctrl => {
                self.scroll_up(1);
                return false;
            }
            KeyCode::Down if ctrl => {
                self.scroll_down(1);
                return false;
            }
            _ => {}
        }

        // A pending permission prompt captures all keys until it's answered.
        if self.pending_permission.is_some() {
            self.permission_key(key);
            return false;
        }

        // Ctrl+J inserts a literal newline into the prompt without submitting.
        // (Some terminals also deliver this as Enter+CONTROL.)
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('j') | KeyCode::Enter)
        {
            self.esc_armed = false;
            self.insert_char('\n');
            return false;
        }

        // While a popup (`@` files or `/` commands) is open it captures
        // navigation keys.
        if self.completion_active || self.palette.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.popup_move(-1);
                    return false;
                }
                KeyCode::Down => {
                    self.popup_move(1);
                    return false;
                }
                KeyCode::Enter | KeyCode::Tab => {
                    self.popup_accept();
                    return false;
                }
                KeyCode::Esc => {
                    self.completion_active = false;
                    self.palette = None;
                    return false;
                }
                KeyCode::Char(_) if self.palette.is_some() => {
                    if matches!(self.palette.as_ref().map(|p| p.kind), Some(PaletteKind::Models)) {
                        self.palette = None;
                    }
                }
                _ => {}
            }
        }

        match key.code {
            // First Esc warns; a second consecutive Esc interrupts the running
            // agent (Ctrl+C quits Inductor entirely).
            KeyCode::Esc => {
                if self.pr_flow.take().is_some() {
                    self.clear_prompt();
                    self.status = "PR creation cancelled".to_string();
                } else if self.show_usage {
                    self.show_usage = false;
                    self.status = "Usage hidden".to_string();
                } else if self.esc_armed {
                    self.esc_armed = false;
                    if self.is_running() {
                        self.interrupt_run();
                    } else {
                        self.status = "Idle — nothing to interrupt".to_string();
                    }
                } else {
                    self.esc_armed = true;
                    self.status = if self.is_running() {
                        "⚠ Interrupt the running agent? Press Esc again to stop.".to_string()
                    } else {
                        "⚠ Press Esc again to confirm.".to_string()
                    };
                }
            }
            // Enter runs the agent (newlines are inserted with Ctrl+J).
            KeyCode::Enter => {
                self.esc_armed = false;
                self.submit_prompt();
            }
            // Up/Down recall previously submitted prompts (shell-style history).
            KeyCode::Up => {
                self.esc_armed = false;
                self.history_prev();
            }
            KeyCode::Down => {
                self.esc_armed = false;
                self.history_next();
            }
            KeyCode::Left => {
                self.esc_armed = false;
                self.cursor_left();
            }
            KeyCode::Right => {
                self.esc_armed = false;
                self.cursor_right();
            }
            KeyCode::Home => {
                self.esc_armed = false;
                self.cursor = 0;
            }
            KeyCode::End => {
                self.esc_armed = false;
                self.cursor = self.prompt.len();
            }
            KeyCode::Backspace => {
                self.esc_armed = false;
                self.history_index = None;
                self.backspace();
                self.refresh_popups();
            }
            KeyCode::Char(ch) => {
                self.esc_armed = false;
                self.history_index = None;
                self.insert_char(ch);
                self.refresh_popups();
            }
            _ => {}
        }
        false
    }

    /// Re-evaluate which popup should be open after a prompt edit: a `/command`
    /// palette when the prompt is a single `/…` token, otherwise `@` files.
    fn refresh_popups(&mut self) {
        if self.pr_flow.is_some() {
            self.palette = None;
            self.completion_active = false;
        } else if self.prompt.starts_with('/') && !self.prompt.contains(char::is_whitespace) {
            self.completion_active = false;
            self.open_commands();
        } else {
            self.palette = None;
            self.update_completions();
        }
    }

    fn open_commands(&mut self) {
        let items: Vec<String> = COMMANDS
            .iter()
            .filter(|c| c.starts_with(self.prompt.as_str()))
            .map(|c| c.to_string())
            .collect();
        self.palette = (!items.is_empty()).then_some(Palette {
            kind: PaletteKind::Commands,
            items,
            index: 0,
        });
    }

    /// Open the session picker: all sessions previously run in this workspace
    /// (the state DB is per-workspace), newest first, with a one-line preview.
    fn open_sessions(&mut self) {
        let Ok(db) = WorkspaceDb::open(&self.state_db_path) else {
            self.status = "Could not open workspace session store".to_string();
            return;
        };
        let sessions = db.list_sessions().unwrap_or_default();
        if sessions.is_empty() {
            self.status = "No sessions yet in this workspace".to_string();
            return;
        }
        // Each item keeps the session id as its first token (the accept handler
        // parses it back out), followed by model · time · first-prompt preview.
        let items = sessions
            .iter()
            .take(15)
            .map(|s| {
                let preview = db
                    .messages(s.id)
                    .ok()
                    .and_then(|msgs| {
                        msgs.into_iter()
                            .find(|m| m.role == "user")
                            .map(|m| m.content)
                    })
                    .map(|c| truncate(&c, 48))
                    .unwrap_or_else(|| "(no messages)".to_string());
                format!(
                    "{}  {} · {}  {preview}",
                    s.id,
                    s.model,
                    short_time(&s.updated_at)
                )
            })
            .collect();
        self.palette = Some(Palette {
            kind: PaletteKind::Sessions,
            items,
            index: 0,
        });
        self.status = "Pick a session to resume · esc to keep a new one".to_string();
    }

    /// Load a session's stored messages into the visible transcript. Returns the
    /// number of messages restored, or None if it couldn't be read.
    fn load_session_transcript(&mut self, id: &str) -> Option<usize> {
        let sid = id.parse::<SessionId>().ok()?;
        let db = WorkspaceDb::open(&self.state_db_path).ok()?;
        let messages = db.messages(sid).ok()?;
        self.transcript = messages
            .into_iter()
            .map(|m| match m.role.as_str() {
                "user" => ChatEntry::User(m.content),
                _ => ChatEntry::Agent(m.content),
            })
            .collect();
        Some(self.transcript.len())
    }

    fn popup_move(&mut self, delta: i32) {
        if self.completion_active {
            self.move_completion(delta);
        } else if let Some(palette) = &mut self.palette {
            let len = palette.items.len() as i32;
            if len > 0 {
                palette.index = (palette.index as i32 + delta).rem_euclid(len) as usize;
            }
        }
    }

    fn popup_accept(&mut self) {
        if self.completion_active {
            self.accept_completion();
            return;
        }
        let Some(palette) = &self.palette else { return };
        let Some(choice) = palette.items.get(palette.index).cloned() else {
            return;
        };
        match palette.kind {
            PaletteKind::Commands => {
                self.clear_prompt();
                self.palette = None;
                match choice.as_str() {
                    "/model" => {
                        self.palette = Some(Palette {
                            kind: PaletteKind::Models,
                            items: model_catalog()
                                .iter()
                                .map(|(p, m)| model_display(p, m))
                                .collect(),
                            index: 0,
                        });
                    }
                    "/effort" => {
                        self.palette = Some(Palette {
                            kind: PaletteKind::Efforts,
                            items: Effort::ALL.iter().map(|e| e.as_arg().to_string()).collect(),
                            index: 0,
                        });
                    }
                    "/usage" => {
                        self.show_usage = !self.show_usage;
                        if self.show_usage {
                            // Limits are scraped from the provider's own TUI
                            // (codex /status, claude /usage) in the background so
                            // the UI stays responsive.
                            self.provider_usage = Some(ProviderUsage {
                                note: format!("fetching usage from `{}`…", self.provider),
                                ..Default::default()
                            });
                            self.status = "Provider usage — /usage or esc to hide".to_string();
                            let provider = self.provider.clone();
                            let (tx, rx) = mpsc::channel();
                            thread::spawn(move || {
                                let _ = tx.send(read_provider_usage(&provider));
                            });
                            self.usage_rx = Some(rx);
                        } else {
                            self.status = "Usage hidden".to_string();
                            self.usage_rx = None;
                        }
                    }
                    "/fast" => {
                        self.fast = !self.fast;
                        if self.fast {
                            self.saved_effort = self.effort;
                            self.effort = Effort::Minimal;
                            self.status = "⚡ Fast mode on — effort forced to minimal".to_string();
                        } else {
                            self.effort = self.saved_effort;
                            self.status = format!(
                                "Fast mode off — effort restored to {}",
                                self.effort.as_arg()
                            );
                        }
                    }
                    "/sessions" | "/resume" => {
                        self.open_sessions();
                    }
                    "/permissions" => {
                        self.palette = Some(Palette {
                            kind: PaletteKind::Permissions,
                            items: PERMISSION_MODES.iter().map(|m| m.to_string()).collect(),
                            index: 0,
                        });
                    }
                    "/pr" => {
                        self.palette = Some(Palette {
                            kind: PaletteKind::PrActions,
                            items: vec![
                                "Create PR against main".to_string(),
                                "Change base branch".to_string(),
                            ],
                            index: 0,
                        });
                        self.status = "Create a PR · default base is main".to_string();
                    }
                    "/compact" => {
                        self.start_compaction();
                    }
                    "/clear" => {
                        // Fresh start: drop the provider session and wipe the
                        // visible transcript and tracked context.
                        if self.is_running() {
                            self.interrupt_run();
                        }
                        self.pending_permission = None;
                        self.transcript.clear();
                        self.session_id = None;
                        self.pending_seed = None;
                        self.context_used = 0;
                        self.status = "Cleared — fresh start".to_string();
                    }
                    _ => {
                        self.status = "Shortcuts: @ files · / commands (model, effort, usage, fast, pr, resume, permissions, compact, clear) · enter run · ctrl+j newline · esc esc interrupt · ctrl+c quit"
                            .to_string();
                    }
                }
            }
            PaletteKind::Models => {
                // Display is "<label> · <model>"; map openai back to codex.
                if let Some((label, model)) = choice.split_once(" · ") {
                    self.provider = if label == "openai" { "codex" } else { label }.to_string();
                    self.model = model.to_string();
                    // A different provider/model means a fresh harness session.
                    self.session_id = None;
                }
                self.clear_prompt();
                self.palette = None;
                self.status = format!("Model set to {} ({})", self.model, self.provider);
            }
            PaletteKind::Efforts => {
                if let Some(effort) = Effort::ALL.iter().find(|e| e.as_arg() == choice) {
                    self.effort = *effort;
                    self.fast = false;
                }
                self.clear_prompt();
                self.palette = None;
                self.status = format!("Effort set to {choice}");
            }
            PaletteKind::Sessions => {
                let id = choice.split_whitespace().next().unwrap_or("").to_string();
                self.clear_prompt();
                self.palette = None;
                if id.is_empty() {
                    self.status = "Could not parse session id".to_string();
                } else {
                    self.session_id = Some(id.clone());
                    // Restore the prior conversation into the visible transcript.
                    let restored = self.load_session_transcript(&id);
                    self.scroll_to_bottom();
                    self.status = match restored {
                        Some(n) => format!("Resumed session — {n} messages restored"),
                        None => format!("Resuming session {id} — next run continues it"),
                    };
                }
            }
            PaletteKind::Permissions => {
                self.approval = choice.clone();
                self.clear_prompt();
                self.palette = None;
                self.status = format!(
                    "Approval set to {choice}{}",
                    if choice == "never" {
                        " — tools run without asking"
                    } else {
                        " — you'll be asked before risky tools run"
                    }
                );
            }
            PaletteKind::PrActions => {
                self.clear_prompt();
                self.palette = None;
                if choice.starts_with("Change base") {
                    self.pr_flow = Some(PrFlow::BaseBranch);
                    self.status = "PR base branch · type a branch name, enter to continue (empty = main)"
                        .to_string();
                } else {
                    self.pr_flow = Some(PrFlow::CommitMessage {
                        base: "main".to_string(),
                    });
                    self.status =
                        "PR commit message · type message, enter to commit/push/create PR (base: main)"
                            .to_string();
                }
            }
        }
    }

    /// The active `@mention` query: the text after the last `@` that begins a
    /// token, when the cursor (end of prompt) is still inside that token.
    fn current_mention(&self) -> Option<(usize, String)> {
        let at = self.prompt.rfind('@')?;
        // The `@` must start a token (preceded by start or whitespace).
        if at > 0 {
            let prev = self.prompt[..at].chars().next_back();
            if !matches!(prev, Some(c) if c.is_whitespace()) {
                return None;
            }
        }
        let query = &self.prompt[at + 1..];
        if query.chars().any(char::is_whitespace) {
            return None;
        }
        Some((at, query.to_string()))
    }

    fn update_completions(&mut self) {
        let Some((_, query)) = self.current_mention() else {
            self.completion_active = false;
            self.completions.clear();
            return;
        };

        // Only complete within the directory level the mention points at:
        // `src/ma` lists `src/`'s children matching `ma`; `gr` lists the root.
        let (dir_part, leaf) = split_dir_leaf(&query);
        let needle = leaf.to_lowercase();
        let rel_dir = dir_part.trim_end_matches('/');

        let mut comps: Vec<String> = list_dir_for_completion(&self.workspace, rel_dir)
            .into_iter()
            .filter(|(name, _)| needle.is_empty() || name.to_lowercase().contains(&needle))
            .map(|(name, is_dir)| {
                if is_dir {
                    format!("{dir_part}{name}/")
                } else {
                    format!("{dir_part}{name}")
                }
            })
            .collect();
        comps.truncate(MAX_COMPLETIONS);

        self.completions = comps;
        self.completion_index = 0;
        self.completion_active = !self.completions.is_empty();
    }

    fn move_completion(&mut self, delta: i32) {
        if self.completions.is_empty() {
            return;
        }
        let len = self.completions.len() as i32;
        let next = (self.completion_index as i32 + delta).rem_euclid(len);
        self.completion_index = next as usize;
    }

    fn accept_completion(&mut self) {
        let Some((at, _)) = self.current_mention() else {
            self.completion_active = false;
            return;
        };
        let Some(choice) = self.completions.get(self.completion_index).cloned() else {
            self.completion_active = false;
            return;
        };

        self.prompt.truncate(at);
        self.prompt.push('@');
        self.prompt.push_str(&choice);
        self.cursor = self.prompt.len();

        if choice.ends_with('/') {
            // Directory: drill in and show its children, keeping the popup open.
            // The user can press Space to keep `@dir/` as-is instead.
            self.update_completions();
        } else {
            // File: accept and close.
            self.prompt.push(' ');
            self.cursor = self.prompt.len();
            self.completion_active = false;
            self.completions.clear();
        }
    }

    /// File/dir paths (workspace-relative) referenced via `@` in the prompt.
    fn mentioned_paths(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for token in self.prompt.split_whitespace() {
            if let Some(path) = token.strip_prefix('@') {
                // Strip trailing punctuation and any trailing slash from a
                // drilled-in directory mention (`@src/` -> `src`).
                let path = path
                    .trim_end_matches([',', '.', ';', ':'])
                    .trim_end_matches('/');
                if !path.is_empty() && !seen.iter().any(|p| p == path) {
                    seen.push(path.to_string());
                }
            }
        }
        seen
    }

    /// Submit the composed user prompt as a normal run.
    fn submit_prompt(&mut self) {
        if self.is_running() {
            self.status = "Agent is already running — esc esc to interrupt".to_string();
            return;
        }
        if self.pr_flow.is_some() {
            self.submit_pr_flow();
            return;
        }
        let prompt = self.prompt.trim().to_string();
        if prompt.is_empty() {
            self.status = "Prompt is empty".to_string();
            return;
        }
        if prompt.starts_with('/') {
            self.status = "Pick a command from the list, or clear it before running".to_string();
            return;
        }

        self.completion_active = false;
        self.palette = None;
        // Record for Up/Down recall (skip consecutive duplicates) and jump to the
        // latest output.
        if self.history.last().map(String::as_str) != Some(prompt.as_str()) {
            self.history.push(prompt.clone());
        }
        self.history_index = None;
        self.scroll_to_bottom();
        let mut multimodal_message = self.composed_multimodal_message();
        // After a compaction the provider session was reset; seed the fresh
        // session with the summary so it keeps the earlier context.
        if let Some(seed) = self.pending_seed.take() {
            multimodal_message.text = format!(
                "[Summary of earlier conversation]\n{seed}\n\n{}",
                multimodal_message.text
            );
        }
        self.transcript.push(ChatEntry::User(prompt));
        self.start_multimodal_run(multimodal_message, RunKind::Normal);
    }

    fn submit_pr_flow(&mut self) {
        let Some(flow) = self.pr_flow.take() else { return };
        let input = self.prompt.trim().to_string();
        self.clear_prompt();
        match flow {
            PrFlow::BaseBranch => {
                let base = if input.is_empty() {
                    "main".to_string()
                } else {
                    input
                };
                self.pr_flow = Some(PrFlow::CommitMessage { base: base.clone() });
                self.status = format!(
                    "PR commit message · type message, enter to commit/push/create PR (base: {base})"
                );
            }
            PrFlow::CommitMessage { base } => {
                if input.is_empty() {
                    self.pr_flow = Some(PrFlow::CommitMessage { base });
                    self.status = "Commit message is required for /pr".to_string();
                    return;
                }
                self.create_pr(&base, &input);
            }
        }
    }

    fn create_pr(&mut self, base: &str, message: &str) {
        self.status = format!("Creating PR against {base}…");
        let result = create_pull_request(
            &self.workspace,
            base,
            message,
            &self.provider,
            &self.model,
        );
        match result {
            Ok(url) => {
                self.transcript.push(ChatEntry::Agent(format!(
                    "✅ Pull request created against `{base}`:
{url}"
                )));
                self.status = "PR created".to_string();
            }
            Err(err) => {
                self.transcript
                    .push(ChatEntry::Error(format!("PR creation failed: {err}")));
                self.status = "PR creation failed".to_string();
            }
        }
        self.scroll_to_bottom();
        self.last_activity = Instant::now();
    }

    /// Compact the provider context: summarize the conversation in a background
    /// run, then start a fresh provider session seeded with that summary. The
    /// visible transcript is kept intact — only the provider gets the summary.
    fn start_compaction(&mut self) {
        if self.is_running() {
            self.status = "Busy — can't compact while a run is active".to_string();
            return;
        }
        if self.session_id.is_none() {
            self.status = "Nothing to compact yet".to_string();
            return;
        }
        self.transcript.push(ChatEntry::Agent(
            "🧹 Compacting context — summarizing the conversation for the provider…".to_string(),
        ));
        let prompt = "Summarize our entire conversation so far into a concise handoff for a \
             fresh session: decisions made, files changed, open tasks, key facts, and current \
             state. Reply with only the summary."
            .to_string();
        self.start_run(prompt, RunKind::Compaction);
    }

    /// Start a multimodal run with text and images
    fn start_multimodal_run(&mut self, multimodal_message: MultimodalMessage, kind: RunKind) {
        // For now, fallback to text-only if no images are present
        if multimodal_message.images.is_empty() {
            self.start_run(multimodal_message.text, kind);
            return;
        }

        // Serialize the multimodal message as JSON and pass it with a special prefix
        match serde_json::to_string(&multimodal_message) {
            Ok(json_str) => {
                let multimodal_prompt = format!("__MULTIMODAL_MESSAGE__:{}", json_str);
                self.start_run(multimodal_prompt, kind);
            }
            Err(_) => {
                // Fallback to text-only with a note about the images
                let mut enhanced_text = multimodal_message.text.clone();
                enhanced_text.push_str("\n\n[Note: This prompt included ");
                enhanced_text.push_str(&multimodal_message.images.len().to_string());
                enhanced_text.push_str(" image(s), but serialization failed]");
                self.start_run(enhanced_text, kind);
            }
        }
    }

    /// Spawn a harness run (normal or compaction) in the background and stream
    /// its events. The UI keeps drawing; `poll_run` drains output.
    fn start_run(&mut self, composed: String, kind: RunKind) {
        let baseline_diff = diff_worktree(&DiffRequest::tracked_only(
            &self.workspace,
            self.diff_base.clone(),
        ))
        .ok();

        let mut command =
            ProcessCommand::new(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("agent")));
        command
            .arg("run")
            .arg("--provider")
            .arg(&self.provider)
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--model")
            .arg(&self.model)
            .arg("--prompt")
            .arg(&composed)
            .arg("--state-db")
            .arg(&self.state_db_path)
            .arg("--effort")
            .arg(self.effort.as_arg())
            .arg("--approval")
            .arg(&self.approval)
            // Keep stdin open so we can stream permission decisions to the run.
            .stdin(Stdio::piped());
        if self.approval == "never" {
            command.arg("--yes");
        }
        if let Some(session_id) = &self.session_id {
            command.arg("--session-id").arg(session_id);
        }
        command.stdout(Stdio::piped()).stderr(Stdio::null());
        // Put the child in its own process group so interrupting kills the
        // whole tree (including the Claude Node bridge grandchild).
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                self.transcript
                    .push(ChatEntry::Error(format!("failed to spawn harness: {err}")));
                self.status = "Spawn failed".to_string();
                self.clear_prompt();
                return;
            }
        };

        let pid = child.id();
        let child_stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let (tx, rx) = mpsc::channel();
        if let Some(stdout) = stdout {
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    match line {
                        Ok(line) => {
                            if tx.send(RunEvent::Line(line)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = tx.send(RunEvent::Done);
            });
        }

        self.run = Some(RunState {
            child,
            stdin: child_stdin,
            pid,
            rx,
            kind,
            body: String::new(),
            started: Instant::now(),
            session_seen: None,
            ctx_used: 0,
            baseline_diff,
        });
        self.clear_prompt();
        self.status = match kind {
            RunKind::Normal => "Running…",
            RunKind::Compaction => "Compacting…",
        }
        .to_string();
        self.last_activity = Instant::now();
    }

    /// Install background-scraped provider usage when it arrives.
    fn poll_usage(&mut self) {
        let Some(rx) = &self.usage_rx else { return };
        match rx.try_recv() {
            Ok(usage) => {
                self.provider_usage = Some(usage);
                self.usage_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.usage_rx = None,
        }
    }

    /// Drain streamed output from a background run; finalize when it completes.
    fn poll_run(&mut self) {
        if self.run.is_none() {
            return;
        }

        let mut done = false;
        let mut new_permission: Option<PendingPermission> = None;
        let workspace = self.workspace.clone();
        {
            let run = self.run.as_mut().unwrap();
            loop {
                match run.rx.try_recv() {
                    Ok(RunEvent::Line(line)) => {
                        let line = line.trim();
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
                            // A tool-permission prompt: pause and ask the user.
                            if value.get("type").and_then(serde_json::Value::as_str)
                                == Some("permission_request")
                            {
                                let s = |k: &str| {
                                    value
                                        .get(k)
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default()
                                        .to_string()
                                };
                                new_permission = Some(PendingPermission {
                                    request_id: s("request_id"),
                                    tool_name: s("tool_name"),
                                    reason: s("reason"),
                                    input_json: value
                                        .get("input_json")
                                        .cloned()
                                        .unwrap_or(serde_json::Value::Null),
                                    transcript_index: 0,
                                    selected: 0,
                                    typing_message: false,
                                    message: String::new(),
                                });
                            }
                            // Capture the harness session id so the next run
                            // continues the same persisted session.
                            if run.session_seen.is_none() {
                                if let Some(id) =
                                    value.get("session_id").and_then(serde_json::Value::as_str)
                                {
                                    run.session_seen = Some(id.to_string());
                                }
                            }
                            // Real context size = input + cache_read of this turn.
                            if value.get("type").and_then(serde_json::Value::as_str)
                                == Some("usage")
                            {
                                let n = |k: &str| {
                                    value
                                        .get(k)
                                        .and_then(serde_json::Value::as_u64)
                                        .unwrap_or(0)
                                };
                                let ctx = n("input_tokens") + n("cache_read_tokens");
                                if ctx > 0 {
                                    run.ctx_used = ctx;
                                }
                            }
                        }
                        apply_event_line(line, &mut run.body, &workspace);
                    }
                    Ok(RunEvent::Done) => {
                        done = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
        }

        if let Some(pending) = new_permission {
            dlog(&format!(
                "show permission tool={} id={}",
                pending.tool_name, pending.request_id
            ));
            self.status = format!("Permission needed — {}", pending.tool_name);
            if let Some(run) = self.run.as_mut() {
                let text = finalize_agent_text(&run.body);
                if !text.is_empty() {
                    self.transcript.push(ChatEntry::Agent(text));
                    run.body.clear();
                }
            }
            let transcript_index = self.transcript.len();
            self.transcript.push(ChatEntry::Permission(PermissionEntry {
                request_id: pending.request_id.clone(),
                tool_name: pending.tool_name.clone(),
                reason: pending.reason.clone(),
                input_json: pending.input_json.clone(),
                decision: None,
                message: None,
            }));
            self.pending_permission = Some(PendingPermission {
                transcript_index,
                ..pending
            });
        }

        if done {
            // The run ended; drop any unanswered permission prompt.
            if let Some(pending) = self.pending_permission.take() {
                self.update_permission_entry(
                    pending.transcript_index,
                    "No response; agent stopped".to_string(),
                    None,
                );
            }
            let mut run = self.run.take().unwrap();
            let success = run.child.wait().map(|s| s.success()).unwrap_or(false);
            let text = finalize_agent_text(&run.body);
            self.last_activity = Instant::now();

            match run.kind {
                RunKind::Compaction => {
                    // Seed a fresh provider session with the summary; keep the
                    // full visible transcript so the user loses nothing.
                    self.session_id = None;
                    self.context_used = approx_tokens(&text);
                    self.pending_seed = Some(text);
                    self.transcript.push(ChatEntry::Agent(
                        "✅ Context compacted — the provider now starts fresh with a summary; \
                         your full history stays here."
                            .to_string(),
                    ));
                    self.status = "Compacted".to_string();
                }
                RunKind::Normal => {
                    if let Some(seen) = run.session_seen.take() {
                        self.session_id = Some(seen);
                    }
                    if run.ctx_used > 0 {
                        self.context_used = run.ctx_used;
                    }
                    // Closing line: keep only the run stopwatch in the visible
                    // transcript. Token usage still feeds context accounting.
                    let mut text = text;
                    if !text.is_empty() {
                        text.push_str(&format!(
                            "\n\n✳ Cooked for {}",
                            fmt_duration(run.started.elapsed()),
                        ));
                    }
                    self.transcript.push(ChatEntry::Agent(text));
                    self.push_diff_entry(run.baseline_diff.as_ref());
                    self.status = if success { "Done" } else { "Harness failed" }.to_string();
                    // Auto-compact when the context crosses the threshold.
                    if self.context_used as f64 >= AUTO_COMPACT_PCT * self.context_window() as f64 {
                        self.start_compaction();
                    }
                }
            }
        }
    }

    /// Kill the running harness (and its process group) so it stops consuming
    /// tokens, then record what was produced before the interrupt.
    fn interrupt_run(&mut self) {
        let Some(mut run) = self.run.take() else {
            return;
        };

        // Kill the whole process group so the provider HTTP stream / Node
        // bridge is torn down and token generation stops.
        #[cfg(unix)]
        {
            let _ = ProcessCommand::new("kill")
                .arg("-KILL")
                .arg(format!("-{}", run.pid))
                .status();
        }
        let _ = run.child.kill();
        let _ = run.child.wait();
        self.pending_permission = None;
        if let Some(seen) = run.session_seen.take() {
            self.session_id = Some(seen);
        }

        let mut text = finalize_agent_text(&run.body);
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str("⏸ interrupted by user");
        self.transcript.push(ChatEntry::Agent(text));
        self.push_diff_entry(run.baseline_diff.as_ref());
        self.status = "Interrupted".to_string();
        self.last_activity = Instant::now();
    }

    /// Handle a key while a tool-permission prompt is open.
    fn permission_key(&mut self, key: KeyEvent) {
        if self.pending_permission.is_none() {
            return;
        }

        // Deny-with-message sub-mode: type the reason sent back to the agent.
        if self.pending_permission.as_ref().unwrap().typing_message {
            match key.code {
                KeyCode::Enter => {
                    let msg = self
                        .pending_permission
                        .as_ref()
                        .unwrap()
                        .message
                        .trim()
                        .to_string();
                    let message = (!msg.is_empty()).then_some(msg);
                    self.resolve_permission("deny", message);
                }
                KeyCode::Esc => {
                    self.pending_permission.as_mut().unwrap().typing_message = false;
                }
                KeyCode::Backspace => {
                    self.pending_permission.as_mut().unwrap().message.pop();
                }
                KeyCode::Char(c) => {
                    self.pending_permission.as_mut().unwrap().message.push(c);
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Up => {
                let p = self.pending_permission.as_mut().unwrap();
                p.selected = (p.selected + 2) % 3;
            }
            KeyCode::Down => {
                let p = self.pending_permission.as_mut().unwrap();
                p.selected = (p.selected + 1) % 3;
            }
            KeyCode::Char('1') => self.resolve_permission("allow", None),
            KeyCode::Char('2') => self.resolve_permission("allow_always", None),
            KeyCode::Char('3') => {
                self.pending_permission.as_mut().unwrap().typing_message = true;
            }
            KeyCode::Enter => match self.pending_permission.as_ref().unwrap().selected {
                0 => self.resolve_permission("allow", None),
                1 => self.resolve_permission("allow_always", None),
                _ => self.pending_permission.as_mut().unwrap().typing_message = true,
            },
            // Esc cancels = deny without a message.
            KeyCode::Esc => self.resolve_permission("deny", None),
            _ => {}
        }
    }

    fn update_permission_entry(&mut self, index: usize, decision: String, message: Option<String>) {
        if let Some(ChatEntry::Permission(entry)) = self.transcript.get_mut(index) {
            entry.decision = Some(decision);
            entry.message = message;
        }
    }

    /// Send the user's decision down to the running agent and clear the prompt.
    fn resolve_permission(&mut self, decision: &str, message: Option<String>) {
        let Some(pending) = self.pending_permission.take() else {
            return;
        };
        if let Some(run) = self.run.as_mut() {
            if let Some(stdin) = run.stdin.as_mut() {
                let payload = serde_json::json!({
                    "type": "permission_decision",
                    "request_id": pending.request_id,
                    "decision": decision,
                    "message": message,
                });
                dlog(&format!(
                    "send decision={decision} id={} (stdin present)",
                    pending.request_id
                ));
                let _ = writeln!(stdin, "{payload}");
                let _ = stdin.flush();
            } else {
                dlog("send decision FAILED: run.stdin is None");
            }
        } else {
            dlog("send decision FAILED: no active run");
        }
        let decision_label = match decision {
            "allow" => "Allowed once".to_string(),
            "allow_always" => "Allowed for this session".to_string(),
            _ => "Denied by user; agent stopped".to_string(),
        };
        self.update_permission_entry(pending.transcript_index, decision_label, message.clone());
        self.status = match decision {
            "allow" => format!("Allowed {} — continuing…", pending.tool_name),
            "allow_always" => {
                format!(
                    "Allowing {} for this session — continuing…",
                    pending.tool_name
                )
            }
            _ => format!("Denied {} — continuing…", pending.tool_name),
        };
        if !matches!(decision, "allow" | "allow_always") {
            if let Some(mut run) = self.run.take() {
                #[cfg(unix)]
                {
                    let _ = ProcessCommand::new("kill")
                        .arg("-KILL")
                        .arg(format!("-{}", run.pid))
                        .status();
                }
                let _ = run.child.kill();
                let _ = run.child.wait();
                let text = finalize_agent_text(&run.body);
                if !text.is_empty() {
                    self.transcript.push(ChatEntry::Agent(text));
                }
                self.status = format!("Denied {} — agent stopped", pending.tool_name);
            }
        }
        self.last_activity = Instant::now();
    }

    /// Compute the worktree diff against the base ref and, if there are
    /// changes, append a colored diff block to the conversation.
    fn push_diff_entry(&mut self, baseline: Option<&diff::DiffSummary>) {
        let request = DiffRequest::tracked_only(&self.workspace, self.diff_base.clone());
        if let Ok(mut summary) = diff_worktree(&request) {
            if let Some(baseline) = baseline {
                summary
                    .files
                    .retain(|file| !baseline.files.iter().any(|before| before == file));
            }
            if summary.changed_files() > 0 {
                self.transcript
                    .push(ChatEntry::Diff(build_diff_rows(summary)));
            }
        }
    }

    /// Expand the prompt: for each `@mention` that resolves to a file inside
    /// the workspace, inline its contents so the model has them as context.
    #[cfg(test)]
    fn composed_prompt(&self) -> String {
        // For backward compatibility, return just the text portion
        let multimodal = self.composed_multimodal_message();
        multimodal.text
    }

    /// Create a multimodal message with text and images from the prompt
    fn composed_multimodal_message(&self) -> MultimodalMessage {
        let task = self.prompt.trim();
        let mentions = self.mentioned_paths();
        if mentions.is_empty() {
            return MultimodalMessage {
                text: task.to_string(),
                images: Vec::new(),
            };
        }

        let mut context = String::new();
        let mut images = Vec::new();

        for rel in &mentions {
            match read_workspace_entry(&self.workspace, rel) {
                Some(MentionContent::File(body)) => {
                    context.push_str(&format!("===== @{rel} =====\n{body}\n\n"));
                }
                Some(MentionContent::Dir(listing)) => {
                    context.push_str(&format!("===== @{rel}/ (directory) =====\n{listing}\n\n"));
                }
                Some(MentionContent::Image(image_mention)) => {
                    images.push(image_mention.clone());
                    // Also add text description of the image
                    context.push_str(&format!("===== @{rel} (image) =====\n"));
                    context.push_str(&format!(
                        "Image: {} ({}×{} pixels, {} bytes, {})\n\n",
                        rel,
                        image_mention
                            .width
                            .map_or("?".to_string(), |w| w.to_string()),
                        image_mention
                            .height
                            .map_or("?".to_string(), |h| h.to_string()),
                        image_mention.file_size,
                        image_mention.mime_type
                    ));
                }
                None => {
                    context.push_str(&format!("===== @{rel} (not found in workspace) =====\n\n"));
                }
            }
        }

        let text = if context.is_empty() {
            task.to_string()
        } else {
            format!(
                "The user referenced these workspace paths; their contents are included for context.\n\n{context}Task:\n{task}"
            )
        };

        MultimodalMessage { text, images }
    }
}

#[derive(Debug, Clone)]
struct DiffRow {
    kind: DiffRowKind,
    /// Display gutter line number (new side for add/context, old side for remove).
    line_no: Option<u32>,
    syntax_extension: Option<String>,
    text: String,
}

impl DiffRow {
    fn header(text: impl Into<String>) -> Self {
        Self {
            kind: DiffRowKind::FileHeader,
            line_no: None,
            syntax_extension: None,
            text: text.into(),
        }
    }

    fn stat(text: impl Into<String>) -> Self {
        Self {
            kind: DiffRowKind::Stat,
            line_no: None,
            syntax_extension: None,
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DiffRowKind {
    /// `● Update(path)` per-file header.
    FileHeader,
    /// `└ Added N lines, removed M lines` summary under a header.
    Stat,
    Add,
    Remove,
    Context,
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(frame: &mut Frame<'_>, app: &App) {
    if app.screen == Screen::Welcome {
        render_welcome(frame, app, frame.area());
        return;
    }

    // The composer grows with the prompt so long / multi-line input wraps onto
    // additional rows instead of being clipped.
    let prompt_rows = prompt_visual_rows(
        &app.prompt,
        prompt_text_width(frame.area().width),
        max_prompt_rows(frame.area().height),
    );
    let composer_height = 2 /* borders */ + prompt_rows as u16;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(MIN_CONVERSATION_ROWS),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(frame.area());

    render_header(frame, app, chunks[0]);
    render_conversation(frame, app, chunks[1]);
    render_composer(frame, app, chunks[2]);
    render_status(frame, app, chunks[3]);

    // The `@` files / `/` command popup floats above the composer.
    if app.completion_active {
        render_completion_popup(frame, app, chunks[2]);
    } else if app.palette.is_some() {
        render_palette_popup(frame, app, chunks[2]);
    }

    // The `/usage` overlay floats over the conversation.
    if app.show_usage {
        render_usage_overlay(frame, app, chunks[1]);
    }
}

/// Max non-diff lines shown in the permission card for command/JSON previews.
const PERMISSION_NON_DIFF_MAX: usize = 24;

/// Build a preview of a pending tool call for the permission card: a colored
/// diff (green adds / red removes, line-numbered) for file writes/edits, the
/// shell command for bash, or pretty JSON otherwise.
fn permission_preview_lines(
    _tool_name: &str,
    input: &serde_json::Value,
    width: usize,
) -> Vec<Line<'static>> {
    let s = |k: &str| input.get(k).and_then(serde_json::Value::as_str);
    let mut out: Vec<Line> = Vec::new();

    // Bash / shell command.
    if let Some(cmd) = s("command") {
        for line in cmd.lines().take(PERMISSION_NON_DIFF_MAX) {
            out.push(Line::from(vec![
                Span::styled("$ ", Style::default().fg(theme::FAINT)),
                Span::styled(line.to_string(), Style::default().fg(theme::FG)),
            ]));
        }
        return out;
    }

    let path = s("path").or_else(|| s("file_path"));
    let syntax_extension = path.and_then(|path| {
        std::path::Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
    });
    if let Some(path) = path {
        out.push(Line::from(Span::styled(
            short_path(path),
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        )));
    }

    // Edit: old_string (removed) → new_string (added). Write: content (added).
    let removed = s("old_string").or_else(|| s("old"));
    let added = s("content")
        .or_else(|| s("new_string"))
        .or_else(|| s("new"));

    let push_diff = |out: &mut Vec<Line>, text: &str, kind: DiffRowKind| {
        for (i, line) in text.lines().enumerate() {
            let bg = match kind {
                DiffRowKind::Add => Some(theme::ADD_BG),
                DiffRowKind::Remove => Some(theme::REM_BG),
                _ => None,
            };
            let (sign, sign_fg) = match kind {
                DiffRowKind::Add => ("+", theme::ADD),
                DiffRowKind::Remove => ("-", theme::REM),
                _ => (" ", theme::FG),
            };
            let mut sign_style = Style::default().fg(sign_fg);
            if let Some(bg) = bg {
                sign_style = sign_style.bg(bg);
            }
            let mut spans = vec![
                Span::styled(format!("{:>4} ", i + 1), Style::default().fg(theme::FAINT)),
                Span::styled(format!("{sign} "), sign_style),
            ];
            spans.extend(highlight_code(line, bg, syntax_extension));
            pad_spans_to_width(&mut spans, width, bg);
            out.push(Line::from(spans));
        }
    };

    if let Some(removed) = removed {
        push_diff(&mut out, removed, DiffRowKind::Remove);
    }
    if let Some(added) = added {
        push_diff(&mut out, added, DiffRowKind::Add);
    }

    // Fall back to JSON when there's no recognizable file content.
    if removed.is_none() && added.is_none() {
        let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
        for line in pretty.lines().take(PERMISSION_NON_DIFF_MAX) {
            out.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(theme::MUTED),
            )));
        }
    }
    out
}

/// A `[████░░░░]  <bold value>  <dim suffix>` bar line. `fill_pct` is how much
/// of the bar to fill; `value` is the bold label (e.g. "37% left"); `suffix` is
/// dim trailing text (e.g. reset time).
fn bar_line(fill_pct: f64, width: usize, value: String, suffix: String) -> Line<'static> {
    let fill_pct = fill_pct.clamp(0.0, 100.0);
    let filled = ((fill_pct / 100.0) * width as f64).round() as usize;
    let filled = filled.min(width);
    Line::from(vec![
        Span::raw("  "),
        Span::styled("█".repeat(filled), Style::default().fg(theme::BRAND)),
        Span::styled(
            "░".repeat(width - filled),
            Style::default().fg(theme::FAINT),
        ),
        Span::styled(
            format!("  {value}"),
            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(suffix, Style::default().fg(theme::FAINT)),
    ])
}

/// A limit section: a header, then either a bar or an "unavailable" line.
fn limit_section(
    lines: &mut Vec<Line<'static>>,
    title: &'static str,
    window: &Option<LimitWindow>,
    bar_w: usize,
) {
    lines.push(Line::from(Span::styled(
        title,
        Style::default()
            .fg(theme::BRAND)
            .add_modifier(Modifier::BOLD),
    )));
    match window {
        Some(w) => {
            let suffix = w
                .reset_label
                .as_ref()
                .map(|label| format!("  ·  {label}"))
                .unwrap_or_default();
            // Fill the bar to the native percentage and label it natively
            // ("37% left" / "10% used") — no conversion between providers.
            let value = format!("{:.0}% {}", w.percent, w.metric.label());
            lines.push(bar_line(w.percent, bar_w, value, suffix));
        }
        None => lines.push(Line::styled(
            "  not exposed by this provider",
            Style::default().fg(theme::FAINT),
        )),
    }
    lines.push(Line::raw(""));
}

fn render_usage_overlay(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let usage = app.provider_usage.as_ref();
    let provider_label = if app.provider == "codex" {
        "openai"
    } else {
        &app.provider
    };
    let bar_w = (area.width.saturating_sub(28)).clamp(10, 36) as usize;

    let mut lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("provider  ", Style::default().fg(theme::MUTED)),
            Span::styled(
                provider_label.to_string(),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  {}", app.model),
                Style::default().fg(theme::MUTED),
            ),
        ]),
        Line::raw(""),
    ];

    let five = usage.and_then(|u| u.five_hour.as_ref()).cloned_window();
    let weekly = usage.and_then(|u| u.weekly.as_ref()).cloned_window();
    limit_section(&mut lines, "5-hour limit", &five, bar_w);
    limit_section(&mut lines, "Weekly limit", &weekly, bar_w);

    // Context window — calculated by us from the latest turn's real usage.
    let window = app.context_window();
    lines.push(Line::from(Span::styled(
        format!("Context window ({}k)", window / 1000),
        Style::default()
            .fg(theme::BRAND)
            .add_modifier(Modifier::BOLD),
    )));
    if app.context_used > 0 {
        let pct = (app.context_used as f64 / window as f64) * 100.0;
        lines.push(bar_line(
            pct,
            bar_w,
            format!("{pct:.0}% used"),
            String::new(),
        ));
        lines.push(Line::styled(
            format!(
                "  {} / {} tokens · auto-compacts at {:.0}%",
                app.context_used,
                window,
                AUTO_COMPACT_PCT * 100.0
            ),
            Style::default().fg(theme::FAINT),
        ));
    } else {
        lines.push(Line::styled(
            "  no turns yet — run something to measure context",
            Style::default().fg(theme::FAINT),
        ));
    }
    lines.push(Line::raw(""));

    let note = usage.map(|u| u.note.clone()).unwrap_or_default();
    if !note.is_empty() {
        lines.push(Line::styled(
            note,
            Style::default()
                .fg(theme::FAINT)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    lines.push(Line::styled(
        "esc or /usage to hide",
        Style::default().fg(theme::FAINT),
    ));

    let width = area.width.saturating_sub(4).min(72).max(46);
    let height = (lines.len() as u16 + 2).min(area.height.max(3));
    let overlay = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + 1,
        width,
        height,
    };
    frame.render_widget(Clear, overlay);
    frame.render_widget(
        Paragraph::new(lines).block(panel_styled(
            "/usage · provider limits",
            theme::BORDER,
            theme::MUTED,
        )),
        overlay,
    );
}

/// Helper to clone an `Option<&LimitWindow>` into `Option<LimitWindow>`.
trait CloneWindow {
    fn cloned_window(self) -> Option<LimitWindow>;
}
impl CloneWindow for Option<&LimitWindow> {
    fn cloned_window(self) -> Option<LimitWindow> {
        self.map(|w| LimitWindow {
            percent: w.percent,
            metric: w.metric,
            reset_label: w.reset_label.clone(),
        })
    }
}

fn render_palette_popup(frame: &mut Frame<'_>, app: &App, composer: Rect) {
    let Some(palette) = &app.palette else { return };
    let title = match palette.kind {
        PaletteKind::Commands => "/ commands · ↑↓ enter",
        PaletteKind::Models => "select model · ↑↓ enter",
        PaletteKind::Efforts => "select effort · ↑↓ enter",
        PaletteKind::Sessions => "↑↓ select · enter resume · esc new session",
        PaletteKind::Permissions => "approval policy · ↑↓ enter",
        PaletteKind::PrActions => "create PR · ↑↓ enter",
    };
    let visible_rows = palette.items.len().min(8);
    let extra_rows = usize::from(palette.index > 0)
        + usize::from(palette.index + visible_rows < palette.items.len());
    let height = (visible_rows + extra_rows + 2).min(12) as u16;
    // Session rows carry a preview, so give them more room.
    let cap = if palette.kind == PaletteKind::Sessions {
        100
    } else {
        60
    };
    let width = composer.width.saturating_sub(2).min(cap).max(24);
    let area = Rect {
        x: composer.x + 1,
        y: composer.y.saturating_sub(height),
        width,
        height,
    };

    let start = if palette.items.len() <= visible_rows {
        0
    } else {
        palette
            .index
            .saturating_sub(visible_rows / 2)
            .min(palette.items.len() - visible_rows)
    };
    let hidden_before = start;
    let hidden_after = palette.items.len().saturating_sub(start + visible_rows);

    let mut items = Vec::new();
    if hidden_before > 0 {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  ↑ {hidden_before} more"),
            Style::default().fg(theme::MUTED),
        ))));
    }
    items.extend(palette
        .items
        .iter()
        .enumerate()
        .skip(start)
        .take(visible_rows)
        .map(|(i, item)| {
            let selected = i == palette.index;
            // Mark the current model/effort/permission.
            let current = match palette.kind {
                PaletteKind::Models => *item == model_display(&app.provider, &app.model),
                PaletteKind::Efforts => item == app.effort.as_arg(),
                PaletteKind::Permissions => *item == app.approval,
                PaletteKind::Sessions => app
                    .session_id
                    .as_deref()
                    .is_some_and(|id| item.starts_with(id)),
                PaletteKind::Commands | PaletteKind::PrActions => false,
            };
            let style = if selected {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                Style::default().fg(theme::FG)
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    if current { "● " } else { "  " },
                    Style::default().fg(theme::SUCCESS),
                ),
                Span::styled(item.clone(), style),
            ]))
        }));
    if hidden_after > 0 {
        items.push(ListItem::new(Line::from(Span::styled(
            format!("  ↓ {hidden_after} more"),
            Style::default().fg(theme::MUTED),
        ))));
    }

    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(panel_styled(title, theme::BORDER, theme::MUTED)),
        area,
    );
}

/// Block-letter banner spelling INDUCTOR, built per-letter to stay aligned.
fn inductor_banner() -> Vec<String> {
    const I: [&str; 5] = ["█████", "  █  ", "  █  ", "  █  ", "█████"];
    const N: [&str; 5] = ["█   █", "██  █", "█ █ █", "█  ██", "█   █"];
    const D: [&str; 5] = ["████ ", "█   █", "█   █", "█   █", "████ "];
    const U: [&str; 5] = ["█   █", "█   █", "█   █", "█   █", "█████"];
    const C: [&str; 5] = ["█████", "█    ", "█    ", "█    ", "█████"];
    const T: [&str; 5] = ["█████", "  █  ", "  █  ", "  █  ", "  █  "];
    const O: [&str; 5] = ["█████", "█   █", "█   █", "█   █", "█████"];
    const R: [&str; 5] = ["████ ", "█   █", "████ ", "█  █ ", "█   █"];
    let letters = [I, N, D, U, C, T, O, R];
    (0..5)
        .map(|row| {
            letters
                .iter()
                .map(|letter| letter[row])
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn render_welcome(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(2),
        ])
        .split(area);

    // Welcome banner box.
    let welcome = Line::from(vec![
        Span::styled("✲ ", Style::default().fg(theme::BRAND)),
        Span::styled("Welcome to ", Style::default().fg(theme::FG)),
        Span::styled(
            "Inductor",
            Style::default()
                .fg(theme::BRAND)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " — a control plane for coding agents",
            Style::default().fg(theme::MUTED),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(welcome).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(theme::BRAND))
                .padding(Padding::horizontal(1)),
        ),
        rows[0],
    );

    // Big block-letter wordmark.
    let banner: Vec<Line> = inductor_banner()
        .into_iter()
        .map(|line| {
            Line::styled(
                line,
                Style::default()
                    .fg(theme::BRAND)
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(banner).alignment(Alignment::Center), rows[1]);

    // Footer prompt.
    let footer = Line::from(vec![
        Span::styled("✦ ", Style::default().fg(theme::SUCCESS)),
        Span::styled(
            format!("Ready on {} · {}. Press ", app.provider, app.model),
            Style::default().fg(theme::SUCCESS),
        ),
        Span::styled(
            "Enter",
            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " to start · Ctrl+C to quit",
            Style::default().fg(theme::SUCCESS),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer), rows[2]);
}

/// One faint line: the only chrome above the conversation — model, effort,
/// workspace.
fn render_header(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let effort = if app.fast {
        "fast"
    } else {
        app.effort.as_arg()
    };
    let line = Line::from(Span::styled(
        format!(
            " {} · {effort} · {}",
            app.model,
            short_path(&app.workspace.display().to_string())
        ),
        Style::default().fg(theme::MUTED),
    ));
    frame.render_widget(Paragraph::new(line), area);
}

fn render_conversation(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let width = area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::new();

    if app.transcript.is_empty() && !app.is_running() {
        lines.push(Line::from(Span::styled(
            " Ask anything · @ files · / commands",
            Style::default().fg(theme::FAINT),
        )));
    } else {
        for entry in &app.transcript {
            lines.extend(entry_lines(entry, width, app.pending_permission.as_ref()));
            lines.push(Line::raw(""));
        }
    }

    // Live, in-progress agent output while a run streams.
    if let Some(run) = &app.run {
        let body = finalize_agent_text(&run.body);
        lines.extend(agent_body_lines(&body, width));
        let frame_glyph = SPINNER[app.tick % SPINNER.len()];
        lines.push(Line::from(Span::styled(
            format!(
                "{frame_glyph} {} · esc esc to interrupt",
                fmt_duration(run.started.elapsed())
            ),
            Style::default().fg(theme::MUTED),
        )));
        lines.push(Line::raw(""));
    }

    let inner_height = area.height;
    let max_offset = (lines.len() as u16).saturating_sub(inner_height);
    app.view_max.set(max_offset);
    app.view_h.set(inner_height);
    let offset = if app.follow_tail.get() {
        max_offset
    } else {
        app.scroll_top.get().min(max_offset)
    };
    app.scroll_top.set(offset);
    app.follow_tail.set(offset >= max_offset);

    // Cache the visible plain text + geometry so clicks can resolve file paths.
    {
        let mut cache = app.view_text.borrow_mut();
        cache.clear();
        cache.extend(lines.iter().map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        }));
    }
    app.view_first.set(offset as usize);
    app.view_rect.set((area.x, area.y, area.width, area.height));

    let visible_selection = if app.selection_visible {
        app.selection
    } else {
        None
    };
    apply_selection_highlight(
        &mut lines,
        visible_selection,
        app.selection_full_row,
        area.width as usize,
    );
    frame.render_widget(Paragraph::new(lines).scroll((offset, 0)), area);
}

fn entry_lines(
    entry: &ChatEntry,
    width: usize,
    pending_permission: Option<&PendingPermission>,
) -> Vec<Line<'static>> {
    match entry {
        // The user's message: a gray block, no label (the only highlighted text).
        ChatEntry::User(text) => user_lines(text, width),
        ChatEntry::Agent(text) => agent_body_lines(text, width),
        ChatEntry::Permission(entry) => {
            let active =
                pending_permission.filter(|pending| pending.request_id == entry.request_id);
            permission_entry_lines(entry, active, width)
        }
        ChatEntry::Error(text) => text
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("✗ {l}"),
                    Style::default().fg(theme::ERROR),
                ))
            })
            .collect(),
        // Per-file `● Update(path)` headers + line-numbered colored diff.
        ChatEntry::Diff(rows) => rows.iter().map(|row| diff_row_line(row, width)).collect(),
    }
}

fn span_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.chars().count()).sum()
}

fn ordered_selection(selection: ActiveSelection) -> (SelectionPoint, SelectionPoint) {
    let a = selection.anchor;
    let b = selection.focus;
    if (a.line, a.col) <= (b.line, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '-'
}

fn apply_selection_highlight(
    lines: &mut [Line<'static>],
    selection: Option<ActiveSelection>,
    full_row: bool,
    width: usize,
) {
    let Some(selection) = selection else {
        return;
    };
    let (start, end) = ordered_selection(selection);
    for (global_line, line) in lines.iter_mut().enumerate() {
        if global_line < start.line || global_line > end.line {
            continue;
        }
        let line_width = span_width(&line.spans);
        let is_single_line = start.line == end.line;
        let from = if is_single_line || global_line == start.line {
            start.col.min(width)
        } else {
            0
        };
        let to = if is_single_line && full_row {
            width
        } else if is_single_line {
            end.col.min(width)
        } else if global_line == end.line {
            if full_row { width } else { end.col.min(width) }
        } else {
            width
        }
        .max(from.min(line_width));
        if to <= from {
            continue;
        }
        line.spans = highlight_span_range(&line.spans, from, to);
    }
}

fn highlight_span_range(spans: &[Span<'_>], from: usize, to: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        let text = span.content.as_ref();
        let len = text.chars().count();
        let span_start = pos;
        let span_end = pos + len;
        if span_end <= from || span_start >= to {
            out.push(Span::styled(text.to_string(), span.style));
            pos = span_end;
            continue;
        }

        let chars = text.chars().collect::<Vec<_>>();
        let local_from = from.saturating_sub(span_start).min(len);
        let local_to = (to.saturating_sub(span_start)).min(len);
        if local_from > 0 {
            out.push(Span::styled(
                chars[..local_from].iter().collect::<String>(),
                span.style,
            ));
        }
        if local_to > local_from {
            out.push(Span::styled(
                chars[local_from..local_to].iter().collect::<String>(),
                span.style.bg(theme::SELECTION_BG).fg(theme::SELECTION_FG),
            ));
        }
        if local_to < len {
            out.push(Span::styled(
                chars[local_to..].iter().collect::<String>(),
                span.style,
            ));
        }
        pos = span_end;
    }
    if to > pos {
        if from > pos {
            out.push(Span::raw(" ".repeat(from - pos)));
        }
        out.push(Span::styled(
            " ".repeat(to.saturating_sub(from.max(pos))),
            Style::default()
                .bg(theme::SELECTION_BG)
                .fg(theme::SELECTION_FG),
        ));
    }
    out
}

fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: usize, bg: Option<Color>) {
    let Some(bg) = bg else {
        return;
    };
    let used = span_width(spans);
    if width > used {
        spans.push(Span::styled(
            " ".repeat(width - used),
            Style::default().bg(bg),
        ));
    }
}

fn permission_entry_lines(
    entry: &PermissionEntry,
    active: Option<&PendingPermission>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let title = if entry.reason.is_empty() {
        format!("Permission needed for {}", entry.tool_name)
    } else {
        entry.reason.clone()
    };
    lines.push(Line::from(vec![
        Span::styled("● ", Style::default().fg(theme::WARNING)),
        Span::styled(
            title,
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  └ ", Style::default().fg(theme::FAINT)),
        Span::styled(
            format!("tool · {}", entry.tool_name),
            Style::default().fg(theme::MUTED),
        ),
    ]));
    lines.extend(permission_preview_lines(
        &entry.tool_name,
        &entry.input_json,
        width,
    ));
    lines.push(Line::raw(""));

    if let Some(pending) = active {
        if pending.typing_message {
            lines.push(Line::from(Span::styled(
                "  Tell the agent why you're denying (enter to send · esc to go back):",
                Style::default().fg(theme::WARNING),
            )));
            lines.push(Line::from(vec![
                Span::styled("  › ", Style::default().fg(theme::ACCENT)),
                Span::styled(pending.message.clone(), Style::default().fg(theme::FG)),
                Span::styled("▎", Style::default().fg(theme::ACCENT)),
            ]));
        } else {
            let options = [
                "1. Yes, allow once",
                "2. Yes, allow this tool for the rest of the session",
                "3. No, and tell the agent why",
            ];
            for (i, label) in options.iter().enumerate() {
                let selected = i == pending.selected;
                let marker = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme::FG)
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {marker}"), Style::default().fg(theme::ACCENT)),
                    Span::styled(*label, style),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "  ↑↓ move · enter choose · 1/2/3 quick · esc deny",
                Style::default().fg(theme::FAINT),
            )));
        }
    } else if let Some(decision) = &entry.decision {
        let color = if decision.starts_with("Denied") {
            theme::ERROR
        } else {
            theme::FG
        };
        lines.push(Line::from(vec![
            Span::styled("  ✓ ", Style::default().fg(color)),
            Span::styled(decision.clone(), Style::default().fg(color)),
        ]));
        if let Some(message) = &entry.message {
            lines.push(Line::from(Span::styled(
                format!("  reason: {message}"),
                Style::default().fg(theme::MUTED),
            )));
        }
    }

    lines
}

/// The user's prompt as a full-width gray block (Claude-Code style).
fn user_lines(text: &str, width: usize) -> Vec<Line<'static>> {
    let base_style = Style::default().fg(theme::FG).bg(theme::USER_BG);
    let mut lines = Vec::new();
    for raw in text.lines() {
        let wrapped = textwrap::wrap(raw, width.saturating_sub(4).max(20));
        let rows = if wrapped.is_empty() {
            vec![std::borrow::Cow::Borrowed("")]
        } else {
            wrapped
        };
        for row in rows {
            // Check for @mentions and style them appropriately
            let mut spans = Vec::new();
            spans.push(Span::styled("  ", base_style));

            // Split by spaces and style @mentions
            for (i, word) in row.split(' ').enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" ", base_style));
                }

                if word.starts_with('@') {
                    let path = word.trim_start_matches('@');
                    let style = if is_image_file(path) {
                        Style::default()
                            .fg(theme::SUCCESS)
                            .bg(theme::USER_BG)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                            .fg(theme::ACCENT)
                            .bg(theme::USER_BG)
                            .add_modifier(Modifier::BOLD)
                    };
                    spans.push(Span::styled(word.to_string(), style));
                } else {
                    spans.push(Span::styled(word.to_string(), base_style));
                }
            }

            // Pad to the full width so the gray block is continuous.
            let current_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
            let pad = width.saturating_sub(current_width);
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), base_style));
            }

            lines.push(Line::from(spans));
        }
    }
    lines
}

/// The agent's output: prose (markdown), tool lines, results, and embedded
/// diffs — rendered in arrival order with no label or border.
fn agent_body_lines(body: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw in body.lines() {
        // Embedded diff line: faint gutter + colored, syntax-highlighted code.
        if let Some((gutter, sign, content)) = parse_diff_body_line(raw) {
            let (bg, sign_fg) = if sign == '+' {
                (theme::ADD_BG, theme::ADD)
            } else {
                (theme::REM_BG, theme::REM)
            };
            let mut spans = vec![
                Span::styled(format!("{gutter} "), Style::default().fg(theme::FAINT)),
                Span::styled(format!("{sign} "), Style::default().fg(sign_fg).bg(bg)),
            ];
            spans.extend(highlight_code(&content, Some(bg), None));
            pad_spans_to_width(&mut spans, width, Some(bg));
            lines.push(Line::from(spans));
            continue;
        }
        // Tool call header / stat / result lines.
        if raw.starts_with('●') {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            )));
            continue;
        }
        if raw.trim_start().starts_with('└') {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme::MUTED),
            )));
            continue;
        }
        if raw.trim_start().starts_with('✓') {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme::FG),
            )));
            continue;
        }
        if raw.trim_start().starts_with('✗') {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme::ERROR),
            )));
            continue;
        }
        if raw.trim_start().starts_with('✳') {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(theme::MUTED),
            )));
            continue;
        }
        // Prose: lightweight markdown (headers, bullets, **bold**, `code`).
        lines.extend(markdown_line(raw, width.max(20)));
    }
    lines
}

fn syntax_set() -> &'static SyntaxSet {
    static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn syntax_theme() -> &'static Theme {
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    let themes = THEMES.get_or_init(ThemeSet::load_defaults);
    themes
        .themes
        .get("base16-ocean.dark")
        .or_else(|| themes.themes.values().next())
        .expect("syntect default themes are available")
}

fn syntect_color(color: syntect::highlighting::Color) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

fn syntect_span_style(style: SyntectStyle, bg: Option<Color>) -> Style {
    let mut out = Style::default().fg(syntect_color(style.foreground));
    if let Some(bg) = bg {
        out = out.bg(bg);
    }
    out
}

fn plain_code_span(line: &str, bg: Option<Color>) -> Vec<Span<'static>> {
    let mut style = Style::default().fg(theme::FG);
    if let Some(bg) = bg {
        style = style.bg(bg);
    }
    vec![Span::styled(
        if line.is_empty() {
            " ".to_string()
        } else {
            line.to_string()
        },
        style,
    )]
}

/// Syntax-highlight one code line with syntect. `syntax_extension` comes from
/// the changed file path, so Rust/Python/JS/etc. use the right grammar.
fn highlight_code(
    line: &str,
    bg: Option<Color>,
    syntax_extension: Option<&str>,
) -> Vec<Span<'static>> {
    let Some(extension) = syntax_extension.filter(|s| !s.is_empty()) else {
        return plain_code_span(line, bg);
    };
    let ps = syntax_set();
    let syntax = ps
        .find_syntax_by_extension(extension)
        .unwrap_or_else(|| ps.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, syntax_theme());
    let Ok(ranges) = highlighter.highlight_line(line, ps) else {
        return plain_code_span(line, bg);
    };
    if ranges.is_empty() {
        return plain_code_span("", bg);
    }
    ranges
        .into_iter()
        .map(|(style, text)| Span::styled(text.to_string(), syntect_span_style(style, bg)))
        .collect()
}

/// Render one source line of prose as wrapped, markdown-styled ratatui lines:
/// `#`/`##` headers, `-`/`*` bullets, inline `**bold**` and `` `code` ``.
fn markdown_line(raw: &str, width: usize) -> Vec<Line<'static>> {
    let trimmed = raw.trim_start();

    // Headers: render bold + accent, hashes stripped.
    if let Some(rest) = trimmed
        .strip_prefix("### ")
        .or_else(|| trimmed.strip_prefix("## "))
        .or_else(|| trimmed.strip_prefix("# "))
    {
        let mut out = Vec::new();
        for wrapped in textwrap::wrap(rest, width) {
            let mut spans = vec![Span::raw("  ")];
            for s in inline_md(&wrapped, theme::ACCENT) {
                spans.push(Span::styled(s.0, s.1.add_modifier(Modifier::BOLD)));
            }
            out.push(Line::from(spans));
        }
        return out;
    }

    // Bullets: normalize `- `/`* ` to `• `.
    let (prefix, content) = match trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        Some(rest) => ("• ", rest),
        None => ("", raw),
    };

    let mut out = Vec::new();
    for (i, wrapped) in textwrap::wrap(content, width.saturating_sub(prefix.len()))
        .into_iter()
        .enumerate()
    {
        let mut spans = vec![Span::raw("  ")];
        if i == 0 && !prefix.is_empty() {
            spans.push(Span::styled(
                prefix.to_string(),
                Style::default().fg(theme::ACCENT),
            ));
        } else if !prefix.is_empty() {
            spans.push(Span::raw("  "));
        }
        for (text, style) in inline_md(&wrapped, theme::FG) {
            spans.push(Span::styled(text, style));
        }
        out.push(Line::from(spans));
    }
    if out.is_empty() {
        out.push(Line::from(Span::raw("  ")));
    }
    out
}

/// Split text into styled segments, rendering `**bold**` bold and `` `code` ``
/// in the accent color. `base` is the default foreground.
fn inline_md(text: &str, base: Color) -> Vec<(String, Style)> {
    let base_style = Style::default().fg(base);
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            // Find closing **.
            if let Some(close) = find_marker(&chars, i + 2, "**") {
                if !buf.is_empty() {
                    out.push((std::mem::take(&mut buf), base_style));
                }
                let inner: String = chars[i + 2..close].iter().collect();
                out.push((inner, base_style.add_modifier(Modifier::BOLD)));
                i = close + 2;
                continue;
            }
        }
        if chars[i] == '`' {
            if let Some(close) = find_marker(&chars, i + 1, "`") {
                if !buf.is_empty() {
                    out.push((std::mem::take(&mut buf), base_style));
                }
                let inner: String = chars[i + 1..close].iter().collect();
                out.push((inner, Style::default().fg(theme::ACCENT)));
                i = close + 1;
                continue;
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    if !buf.is_empty() {
        out.push((buf, base_style));
    }
    if out.is_empty() {
        out.push((String::new(), base_style));
    }
    // Highlight file-path-looking words in plain segments (they're clickable).
    out.into_iter()
        .flat_map(|(text, style)| {
            if style == base_style {
                accent_paths(text, base_style)
            } else {
                vec![(text, style)]
            }
        })
        .collect()
}

/// Split a plain segment so file-path-looking words render in the accent color
/// (signaling they can be clicked to open).
fn accent_paths(text: String, base: Style) -> Vec<(String, Style)> {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut chunk = String::new();
    let mut chunk_is_space = None::<bool>;
    let flush = |chunk: &mut String, is_space: bool, out: &mut Vec<(String, Style)>| {
        if chunk.is_empty() {
            return;
        }
        let word = std::mem::take(chunk);
        let trimmed = word.trim_matches(|c: char| "()[]{}`'\",;:".contains(c));
        let is_path = !is_space
            && (trimmed.contains('/')
                && trimmed.len() > 1
                && trimmed.chars().any(|c| c.is_alphanumeric()));
        let style = if is_path {
            Style::default().fg(theme::ACCENT)
        } else {
            base
        };
        out.push((word, style));
    };
    for c in text.chars() {
        let is_space = c.is_whitespace();
        if chunk_is_space != Some(is_space) {
            if let Some(prev) = chunk_is_space {
                flush(&mut chunk, prev, &mut out);
            }
            chunk_is_space = Some(is_space);
        }
        chunk.push(c);
    }
    if let Some(prev) = chunk_is_space {
        flush(&mut chunk, prev, &mut out);
    }
    if out.is_empty() {
        out.push((String::new(), base));
    }
    out
}

/// Find the start index of `marker` in `chars` at or after `from`.
fn find_marker(chars: &[char], from: usize, marker: &str) -> Option<usize> {
    let m: Vec<char> = marker.chars().collect();
    let mut i = from;
    while i + m.len() <= chars.len() {
        if chars[i..i + m.len()] == m[..] {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn diff_row_line(row: &DiffRow, width: usize) -> Line<'static> {
    match row.kind {
        // `● Update(path)` — same visual language as a tool card header.
        DiffRowKind::FileHeader => Line::from(vec![
            Span::styled(
                "● ",
                Style::default()
                    .fg(theme::BRAND)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                row.text.clone(),
                Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
            ),
        ]),
        DiffRowKind::Stat => Line::from(vec![
            Span::styled("  └ ", Style::default().fg(theme::FAINT)),
            Span::styled(row.text.clone(), Style::default().fg(theme::FAINT)),
        ]),
        DiffRowKind::Add | DiffRowKind::Remove | DiffRowKind::Context => {
            let gutter = match row.line_no {
                Some(n) => format!("{n:>5} "),
                None => "      ".to_string(),
            };
            let (sign, sign_fg, bg) = match row.kind {
                DiffRowKind::Add => ("+ ", theme::ADD, Some(theme::ADD_BG)),
                DiffRowKind::Remove => ("- ", theme::REM, Some(theme::REM_BG)),
                _ => ("  ", theme::MUTED, None),
            };
            let mut spans = vec![Span::styled(gutter, Style::default().fg(theme::FAINT))];
            let sign_style = match bg {
                Some(bg) => Style::default().fg(sign_fg).bg(bg),
                None => Style::default().fg(sign_fg),
            };
            spans.push(Span::styled(sign.to_string(), sign_style));
            // Syntax highlighting on the code, tinted by the diff background.
            spans.extend(highlight_code(
                &row.text,
                bg,
                row.syntax_extension.as_deref(),
            ));
            pad_spans_to_width(&mut spans, width, bg);
            Line::from(spans)
        }
    }
}

fn render_composer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    // Minimal composer: just the input box (no files chip / no hint clutter).
    render_prompt(frame, app, area);
}

/// Max visible rows reserved for conversation while the prompt grows.
const MIN_CONVERSATION_ROWS: u16 = 3;
/// Columns reserved for the `› ` prompt prefix on each row.
const PROMPT_PREFIX_COLS: u16 = 2;
/// Horizontal columns consumed by prompt borders and padding before text starts.
const PROMPT_HORIZONTAL_CHROME: u16 = 2 /* block borders */ + 2 /* horizontal padding */;

/// Maximum prompt rows that can fit while keeping the header, conversation, and status visible.
fn max_prompt_rows(frame_height: u16) -> usize {
    frame_height
        .saturating_sub(1 /* header */ + MIN_CONVERSATION_ROWS + 1 /* status */ + 2 /* prompt borders */)
        .max(1) as usize
}

/// Usable text width inside the prompt box for a given frame width.
fn prompt_text_width(frame_width: u16) -> usize {
    frame_width
        .saturating_sub(PROMPT_HORIZONTAL_CHROME + PROMPT_PREFIX_COLS)
        .max(1) as usize
}

/// Number of visual rows the prompt occupies once wrapped (clamped to available space).
fn prompt_visual_rows(text: &str, width: usize, max_rows: usize) -> usize {
    layout_prompt(text, 0, width)
        .0
        .len()
        .clamp(1, max_rows.max(1))
}

/// Wrap `text` into visual rows of at most `width` columns, breaking on explicit
/// newlines and at the width boundary. Returns the rows plus the `(row, col)` of
/// the edit cursor at byte offset `cursor`.
fn layout_prompt(text: &str, cursor: usize, width: usize) -> (Vec<String>, (usize, usize)) {
    let width = width.max(1);
    let mut rows: Vec<String> = vec![String::new()];
    let mut col = 0usize;
    let mut cur_rc = (0usize, 0usize);
    let mut byte = 0usize;

    for ch in text.chars() {
        if byte == cursor {
            cur_rc = (rows.len() - 1, col);
        }
        if ch == '\n' {
            rows.push(String::new());
            col = 0;
        } else {
            if col >= width {
                rows.push(String::new());
                col = 0;
            }
            rows.last_mut().unwrap().push(ch);
            col += 1;
        }
        byte += ch.len_utf8();
    }
    if byte == cursor {
        cur_rc = (rows.len() - 1, col);
    }

    (rows, cur_rc)
}

fn render_prompt(frame: &mut Frame<'_>, app: &App, area: Rect) {
    // The one framed region: a plain neutral border around the input.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme::BORDER))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.prompt.is_empty() {
        let line = Line::from(vec![
            Span::styled(
                "› ",
                Style::default()
                    .fg(theme::MUTED)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("type here…", Style::default().fg(theme::FAINT)),
        ]);
        frame.render_widget(Paragraph::new(line), inner);
        frame.set_cursor_position((inner.x + PROMPT_PREFIX_COLS, inner.y));
        return;
    }

    let width = prompt_text_width(area.width);
    let (rows, (cur_row, cur_col)) = layout_prompt(&app.prompt, app.cursor, width);

    // Scroll so the cursor row stays visible when the prompt is taller than box.
    let visible = (inner.height as usize).max(1);
    let first = cur_row
        .saturating_sub(visible.saturating_sub(1))
        .min(rows.len().saturating_sub(visible));

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
        .map(|(i, row)| {
            let prefix = if i == 0 { "› " } else { "  " };
            let mut spans = vec![Span::styled(
                prefix,
                Style::default()
                    .fg(theme::MUTED)
                    .add_modifier(Modifier::BOLD),
            )];
            for (w, word) in row.split(' ').enumerate() {
                if w > 0 {
                    spans.push(Span::raw(" "));
                }
                let style = if is_command_prompt(&app.prompt) && i == 0 && word == app.prompt.trim() {
                    Style::default()
                        .fg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else if word.starts_with('@') {
                    let path = word.trim_start_matches('@');
                    if is_image_file(path) {
                        // Style image mentions differently (e.g., with a different color)
                        Style::default()
                            .fg(theme::SUCCESS) // Use green for images
                            .add_modifier(Modifier::BOLD)
                    } else {
                        // Regular file mention styling
                        Style::default()
                            .fg(theme::ACCENT)
                            .add_modifier(Modifier::BOLD)
                    }
                } else {
                    Style::default().fg(theme::FG)
                };
                spans.push(Span::styled(word.to_string(), style));
            }
            Line::from(spans)
        })
        .collect();

    frame.render_widget(Paragraph::new(lines), inner);

    // Place the real terminal cursor at the edit position.
    if cur_row >= first && cur_row < first + visible {
        let x = inner.x + PROMPT_PREFIX_COLS + cur_col as u16;
        let y = inner.y + (cur_row - first) as u16;
        frame.set_cursor_position((x.min(inner.x + inner.width.saturating_sub(1)), y));
    }
}

fn render_completion_popup(frame: &mut Frame<'_>, app: &App, composer: Rect) {
    let rows = app.completions.len() as u16;
    let height = (rows + 2).min(10);
    let width = composer.width.saturating_sub(2).min(72).max(24);
    let area = Rect {
        x: composer.x + 1,
        y: composer.y.saturating_sub(height),
        width,
        height,
    };

    let items = app
        .completions
        .iter()
        .enumerate()
        .map(|(i, path)| {
            let selected = i == app.completion_index;
            let is_dir = path.ends_with('/');
            let is_image = !is_dir && is_image_file(path);
            let glyph = if is_dir {
                "▸ "
            } else if is_image {
                "📷" // Image icon for image files
            } else {
                "  "
            };
            let style = if selected {
                Style::default()
                    .fg(theme::ACCENT)
                    .add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else if is_image {
                Style::default().fg(theme::SUCCESS) // Green for images
            } else {
                Style::default().fg(if is_dir { theme::ACCENT } else { theme::FG })
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    glyph,
                    Style::default().fg(if is_image {
                        theme::SUCCESS
                    } else {
                        theme::MUTED
                    }),
                ),
                Span::styled(path.clone(), style),
            ]))
        })
        .collect::<Vec<_>>();

    frame.render_widget(Clear, area);
    frame.render_widget(
        List::new(items).block(panel_styled(
            "@ files · ↑↓ · enter open/pick · space keep",
            theme::BORDER,
            theme::MUTED,
        )),
        area,
    );
}

/// One faint status line below the prompt: just the latest user-facing state.
fn render_status(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let left = Line::from(Span::styled(
        format!(" {}", app.status),
        Style::default().fg(theme::MUTED),
    ));
    frame.render_widget(Paragraph::new(left), area);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a file path represents an image file based on its extension
fn is_image_file(path: &str) -> bool {
    let path = path.to_lowercase();
    path.ends_with(".jpg")
        || path.ends_with(".jpeg")
        || path.ends_with(".png")
        || path.ends_with(".gif")
        || path.ends_with(".webp")
        || path.ends_with(".bmp")
        || path.ends_with(".tiff")
        || path.ends_with(".tif")
}

/// Process an image file and return ImageMention with encoded data
fn process_image_file(workspace: &std::path::Path, rel_path: &str) -> Option<ImageMention> {
    let path = workspace.join(rel_path);

    // Read the image file
    let image_bytes = std::fs::read(&path).ok()?;

    // Try to determine the image format and get dimensions
    let (mime_type, width, height) = match image::load_from_memory(&image_bytes) {
        Ok(img) => {
            let (w, h) = img.dimensions();
            let mime = guess_mime_type_from_path(rel_path).unwrap_or("image/jpeg".to_string());
            (mime, Some(w), Some(h))
        }
        Err(_) => {
            // Fallback to basic mime type detection without dimensions
            let mime = guess_mime_type_from_path(rel_path).unwrap_or("image/jpeg".to_string());
            (mime, None, None)
        }
    };

    // Encode to base64
    let base64_data = general_purpose::STANDARD.encode(&image_bytes);

    Some(ImageMention {
        path: rel_path.to_string(),
        base64_data,
        mime_type,
        width,
        height,
        file_size: image_bytes.len(),
    })
}

/// Guess MIME type from file extension
fn guess_mime_type_from_path(path: &str) -> Option<String> {
    let path = path.to_lowercase();
    if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        Some("image/jpeg".to_string())
    } else if path.ends_with(".png") {
        Some("image/png".to_string())
    } else if path.ends_with(".gif") {
        Some("image/gif".to_string())
    } else if path.ends_with(".webp") {
        Some("image/webp".to_string())
    } else if path.ends_with(".bmp") {
        Some("image/bmp".to_string())
    } else if path.ends_with(".tiff") || path.ends_with(".tif") {
        Some("image/tiff".to_string())
    } else {
        None
    }
}

fn panel_styled(title: &str, border: Color, title_color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border))
        .padding(Padding::horizontal(1))
        .title(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        ))
}

/// Extract a file-path-looking token at character column `col` of `text`:
/// the whitespace-delimited word under the cursor, stripped of wrapping
/// punctuation. Returns None when the word doesn't look like a path.
fn path_token_at(text: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if col >= chars.len() || chars[col].is_whitespace() {
        return None;
    }
    let mut start = col;
    while start > 0 && !chars[start - 1].is_whitespace() {
        start -= 1;
    }
    let mut end = col;
    while end < chars.len() && !chars[end].is_whitespace() {
        end += 1;
    }
    let word: String = chars[start..end].iter().collect();
    let mut word = word
        .trim_matches(|c: char| "()[]{}`'\",;:".contains(c))
        .to_string();
    // Tool labels look like `Write(src/x.rs` after trimming — take the inside.
    if let Some(open) = word.rfind('(') {
        word = word[open + 1..].to_string();
    }
    // Must look like a path: a slash, or a dotted filename like foo.rs.
    let looks_like_path = word.contains('/')
        || (word.contains('.')
            && !word.starts_with('.')
            && !word.ends_with('.')
            && word.chars().all(|c| !c.is_whitespace()));
    (looks_like_path && !word.is_empty()).then_some(word)
}

/// Compact an RFC3339 timestamp to `YYYY-MM-DD HH:MM` for the session list.
fn short_time(rfc3339: &str) -> String {
    let trimmed: String = rfc3339.chars().take(16).collect();
    trimmed.replacen('T', " ", 1)
}

/// Collapse a long absolute path to `…/last/two/segments` for compact headers.
fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let path = if !home.is_empty() && path.starts_with(&home) {
        path.replacen(&home, "~", 1)
    } else {
        path.to_string()
    };
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 3 {
        return path;
    }
    let tail = &parts[parts.len() - 2..];
    format!("…/{}", tail.join("/"))
}

/// Split an `@mention` query into its directory portion (with trailing slash)
/// and the leaf being typed. `"src/ma"` -> `("src/", "ma")`, `"gr"` -> `("", "gr")`.
fn split_dir_leaf(query: &str) -> (String, String) {
    match query.rfind('/') {
        Some(i) => (query[..=i].to_string(), query[i + 1..].to_string()),
        None => (String::new(), query.to_string()),
    }
}

/// List the immediate children of a workspace-relative directory for `@`
/// completion: directories first, then files, each alphabetical. Reads live so
/// freshly created files appear without restarting. Returns `(name, is_dir)`.
fn list_dir_for_completion(workspace: &std::path::Path, rel_dir: &str) -> Vec<(String, bool)> {
    let target = if rel_dir.is_empty() {
        workspace.to_path_buf()
    } else {
        if rel_dir.split('/').any(|seg| seg == "..") {
            return Vec::new();
        }
        workspace.join(rel_dir)
    };
    let (Ok(canonical), Ok(root)) = (target.canonicalize(), workspace.canonicalize()) else {
        return Vec::new();
    };
    if !canonical.starts_with(&root) {
        return Vec::new();
    }

    let Ok(entries) = std::fs::read_dir(&canonical) else {
        return Vec::new();
    };
    let mut out: Vec<(String, bool)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') && name != ".env" {
                return None;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && IGNORED_DIRS.contains(&name.as_str()) {
                return None;
            }
            Some((name, is_dir))
        })
        .collect();

    out.sort_by(|(a_name, a_dir), (b_name, b_dir)| {
        b_dir
            .cmp(a_dir)
            .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
    });
    out
}

enum MentionContent {
    File(String),
    Dir(String),
    Image(ImageMention),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct ImageMention {
    path: String,
    base64_data: String,
    mime_type: String,
    width: Option<u32>,
    height: Option<u32>,
    file_size: usize,
}

/// Represents a multimodal message that can contain text and images
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct MultimodalMessage {
    text: String,
    images: Vec<ImageMention>,
}

/// Read a workspace-relative `@mention` target, refusing anything that escapes
/// the workspace. Files return capped contents; directories return a listing.
fn read_workspace_entry(workspace: &std::path::Path, rel: &str) -> Option<MentionContent> {
    let rel = rel.trim_end_matches('/');
    if rel.is_empty() || rel.starts_with('/') || rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    let target = workspace.join(rel);
    let canonical = target.canonicalize().ok()?;
    let root = workspace.canonicalize().ok()?;
    if !canonical.starts_with(&root) {
        return None;
    }

    if canonical.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(&canonical)
            .ok()?
            .flatten()
            .map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    format!("{n}/")
                } else {
                    n
                }
            })
            .collect();
        names.sort();
        Some(MentionContent::Dir(names.join("\n")))
    } else {
        // Check if it's an image file
        if is_image_file(rel) {
            if let Some(image_mention) = process_image_file(workspace, rel) {
                return Some(MentionContent::Image(image_mention));
            }
        }

        // Handle as regular text file
        let bytes = std::fs::read(&canonical).ok()?;
        let mut text = String::from_utf8_lossy(&bytes).to_string();
        if text.len() > MAX_MENTION_BYTES {
            let cut = (0..=MAX_MENTION_BYTES)
                .rev()
                .find(|i| text.is_char_boundary(*i))
                .unwrap_or(0);
            text.truncate(cut);
            text.push_str("\n… [file truncated]");
        }
        Some(MentionContent::File(text))
    }
}

fn create_pull_request(
    workspace: &std::path::Path,
    base: &str,
    message: &str,
    provider: &str,
    model: &str,
) -> Result<String, String> {
    let branch = git_stdout(workspace, &["branch", "--show-current"])?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err("not on a named git branch".to_string());
    }
    if branch == base {
        return Err(format!(
            "current branch is `{base}`; switch to a feature branch first"
        ));
    }

    git_ok(workspace, &["add", "-A"])?;
    let status = git_stdout(workspace, &["status", "--porcelain"])?;
    if !status.trim().is_empty() {
        git_ok(workspace, &["commit", "-m", message])?;
    }

    let ahead = git_stdout(workspace, &["rev-list", "--count", &format!("origin/{base}..HEAD")])?;
    if ahead.trim() == "0" {
        return Err(format!(
            "no changes to commit; current branch `{branch}` has no commits ahead of `origin/{base}`"
        ));
    }

    git_ok(workspace, &["push", "-u", "origin", branch])?;

    let pr_body = generate_pr_body(workspace, base, message, provider, model)
        .unwrap_or_else(|_| "Created by Inductor.".to_string());

    let output = gh_command(workspace)
        .args([
            "pr",
            "create",
            "--base",
            base,
            "--head",
            branch,
            "--title",
            message,
            "--body",
            &pr_body,
        ])
        .output()
        .map_err(|err| format!("failed to run `gh pr create`: {err}"))?;
    if !output.status.success() {
        if let Ok(url) = gh_pr_url(workspace, branch) {
            return Ok(url);
        }
        return Err(format!(
            "gh pr create failed: {}{}",
            String::from_utf8_lossy(&output.stderr).trim(),
            if output.stdout.is_empty() {
                "".to_string()
            } else {
                format!("\n{}", String::from_utf8_lossy(&output.stdout).trim())
            }
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find(|line| line.trim_start().starts_with("http"))
        .map(|line| line.trim().to_string())
        .or_else(|| gh_pr_url(workspace, branch).ok())
        .ok_or_else(|| format!("gh pr create did not return a URL: {}", stdout.trim()))
}

fn generate_pr_body(
    workspace: &std::path::Path,
    base: &str,
    message: &str,
    provider: &str,
    model: &str,
) -> Result<String, String> {
    let diff = git_stdout(workspace, &["diff", "--stat", &format!("origin/{base}...HEAD")])?;
    let diff = if diff.trim().is_empty() {
        git_stdout(workspace, &["diff", "--stat", &format!("origin/{base}..HEAD")])?
    } else {
        diff
    };
    let output = ProcessCommand::new(std::env::current_exe().unwrap_or_else(|_| PathBuf::from("inductor")))
        .args([
            "pr-body",
            "--provider",
            provider,
            "--workspace",
            &workspace.display().to_string(),
            "--title",
            message,
            "--diff",
            &diff,
            "--model",
            model,
        ])
        .current_dir(workspace)
        .output()
        .map_err(|err| format!("failed to generate PR body: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "PR body generation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let body = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if body.is_empty() {
        Err("PR body generation returned an empty response".to_string())
    } else {
        Ok(body)
    }
}

fn gh_command(workspace: &std::path::Path) -> ProcessCommand {
    let mut command = ProcessCommand::new("gh");
    command.current_dir(workspace);
    // GH_REPO overrides repository detection in GitHub CLI. Some shells set it
    // to the workspace path, which makes `gh pr ...` fail with:
    // expected the "[HOST/]OWNER/REPO" format, got "/path/to/repo".
    // For /pr, the target repo should be inferred from the workspace's git remote.
    command.env_remove("GH_REPO");
    command
}

fn gh_pr_url(workspace: &std::path::Path, branch: &str) -> Result<String, String> {
    let output = gh_command(workspace)
        .args(["pr", "view", branch, "--json", "url", "--jq", ".url"])
        .output()
        .map_err(|err| format!("failed to run `gh pr view`: {err}"))?;
    if !output.status.success() {
        if !gh_supports_json(&output.stderr) {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
    } else {
        let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !url.is_empty() {
            return Ok(url);
        }
    }

    let fallback = gh_command(workspace)
        .args(["pr", "view", branch])
        .output()
        .map_err(|err| format!("failed to run `gh pr view`: {err}"))?;
    if !fallback.status.success() {
        let stderr = String::from_utf8_lossy(&fallback.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "gh pr view failed".to_string()
        } else {
            format!("gh pr view failed: {stderr}")
        });
    }
    let stdout = String::from_utf8_lossy(&fallback.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("http://") || line.starts_with("https://"))
        .map(str::to_string)
        .ok_or_else(|| "gh pr view did not return a URL".to_string())
}

fn gh_supports_json(stderr: &[u8]) -> bool {
    !String::from_utf8_lossy(stderr).contains("unknown flag: --json")
}

fn git_stdout(workspace: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let output = ProcessCommand::new("git")
        .args(args)
        .current_dir(workspace)
        .output()
        .map_err(|err| format!("failed to run git {args:?}: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_ok(workspace: &std::path::Path, args: &[&str]) -> Result<(), String> {
    git_stdout(workspace, args).map(|_| ())
}

fn build_diff_rows(summary: diff::DiffSummary) -> Vec<DiffRow> {
    let mut rows = Vec::new();

    for file in summary.files {
        let path = short_path(&file.display_path().display().to_string());
        let syntax_extension = file
            .display_path()
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_string);
        let header = match file.status {
            FileStatus::Added => format!("Create({path})"),
            FileStatus::Deleted => format!("Delete({path})"),
            FileStatus::Renamed | FileStatus::Copied => format!("Move({path})"),
            FileStatus::Modified => format!("Update({path})"),
        };
        rows.push(DiffRow::header(header));
        rows.push(DiffRow::stat(diff_stat(
            file.added_lines(),
            file.removed_lines(),
        )));

        for hunk in file.hunks {
            for line in hunk.lines {
                let (kind, line_no) = match line.kind {
                    DiffLineKind::Add => (DiffRowKind::Add, line.new_line),
                    DiffLineKind::Remove => (DiffRowKind::Remove, line.old_line),
                    DiffLineKind::Context => (DiffRowKind::Context, line.new_line),
                };
                rows.push(DiffRow {
                    kind,
                    line_no,
                    syntax_extension: syntax_extension.clone(),
                    text: line.content,
                });
            }
        }
    }
    rows
}

/// "Added N lines, removed M lines" — dropping a zero side and pluralizing.
fn diff_stat(added: usize, removed: usize) -> String {
    let plural = |n: usize| if n == 1 { "line" } else { "lines" };
    match (added, removed) {
        (0, 0) => "No line changes".to_string(),
        (a, 0) => format!("Added {a} {}", plural(a)),
        (0, r) => format!("Removed {r} {}", plural(r)),
        (a, r) => format!("Added {a} {}, removed {r} {}", plural(a), plural(r)),
    }
}

/// Start a fresh line in `body` if it doesn't already end on one.
fn ensure_newline(body: &mut String) {
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
}

/// Append a diff block for a file-mutating tool call into the body, with real
/// line numbers, so the change stays visible in the transcript permanently.
/// Lines use the `NNNNN + content` / `NNNNN - content` format that the
/// renderer colors green/red with syntax highlighting.
fn append_tool_diff(
    body: &mut String,
    name: &str,
    input: &serde_json::Value,
    workspace: &std::path::Path,
) {
    let s = |k: &str| input.get(k).and_then(serde_json::Value::as_str);
    let path = s("path").or_else(|| s("file_path"));

    // Find the 1-based line where `needle` currently starts in the file, so
    // edit diffs carry real line numbers.
    let start_line_of = |needle: &str| -> usize {
        let Some(rel) = path else { return 1 };
        let full = if std::path::Path::new(rel).is_absolute() {
            PathBuf::from(rel)
        } else {
            workspace.join(rel)
        };
        std::fs::read_to_string(full)
            .ok()
            .and_then(|content| {
                content
                    .find(needle)
                    .map(|idx| content[..idx].matches('\n').count() + 1)
            })
            .unwrap_or(1)
    };

    fn push_block(body: &mut String, text: &str, sign: char, start: usize) {
        for (i, line) in text.lines().enumerate() {
            body.push_str(&format!("{:>5} {sign} {line}\n", start + i));
        }
    }

    // Collect (old, new) pairs per tool shape.
    let mut pairs: Vec<(Option<String>, Option<String>)> = Vec::new();
    match name {
        "Write" | "write_file" => {
            if let Some(content) = s("content") {
                pairs.push((None, Some(content.to_string())));
            }
        }
        "Edit" | "edit_file" => {
            let old = s("old_string").or_else(|| s("old")).map(str::to_string);
            let new = s("new_string").or_else(|| s("new")).map(str::to_string);
            if old.is_some() || new.is_some() {
                pairs.push((old, new));
            }
        }
        "MultiEdit" | "multi_edit" => {
            if let Some(edits) = input.get("edits").and_then(serde_json::Value::as_array) {
                for edit in edits {
                    let e = |k: &str| edit.get(k).and_then(serde_json::Value::as_str);
                    pairs.push((
                        e("old_string").or_else(|| e("old")).map(str::to_string),
                        e("new_string").or_else(|| e("new")).map(str::to_string),
                    ));
                }
            }
        }
        _ => {}
    }
    if pairs.is_empty() {
        return;
    }

    let added: usize = pairs
        .iter()
        .filter_map(|(_, n)| n.as_deref().map(|t| t.lines().count()))
        .sum();
    let removed: usize = pairs
        .iter()
        .filter_map(|(o, _)| o.as_deref().map(|t| t.lines().count()))
        .sum();
    body.push_str(&format!("  └ {}\n", diff_stat(added, removed)));

    for (old, new) in pairs {
        let start = old.as_deref().map(&start_line_of).unwrap_or(1);
        if let Some(old) = old {
            push_block(body, &old, '-', start);
        }
        if let Some(new) = new {
            push_block(body, &new, '+', start);
        }
    }
}

/// Parse a body line in the embedded-diff format: 5-wide line number, space,
/// `+`/`-`, space, content. Returns (line_no_text, sign, content).
fn parse_diff_body_line(raw: &str) -> Option<(String, char, String)> {
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() < 7 {
        return None;
    }
    let gutter: String = chars[..5].iter().collect();
    if !gutter.chars().any(|c| c.is_ascii_digit())
        || !gutter.chars().all(|c| c.is_ascii_digit() || c == ' ')
    {
        return None;
    }
    if chars[5] != ' '
        || !matches!(chars[6], '+' | '-')
        || chars.get(7).map(|c| *c != ' ').unwrap_or(false)
    {
        return None;
    }
    let content: String = chars
        .get(8..)
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    Some((gutter, chars[6], content))
}

/// Apply one NDJSON event line to the ordered agent body. Prose and tool-call
/// lines are appended in arrival order so the display preserves the real
/// sequence: text, tool, its result, more text, the next tool, and so on.
fn apply_event_line(line: &str, body: &mut String, workspace: &std::path::Path) {
    use serde_json::Value;

    if line.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        body.push_str(line);
        body.push('\n');
        return;
    };

    match value.get("type").and_then(Value::as_str) {
        Some("text_delta") => {
            if let Some(text) = value.get("text").and_then(Value::as_str) {
                body.push_str(text);
            }
        }
        Some("tool_call_start") => {
            let name = value.get("name").and_then(Value::as_str).unwrap_or("tool");
            let input = value.get("input_json").cloned().unwrap_or(Value::Null);
            ensure_newline(body);
            body.push_str(&format!("● {}\n", tool_call_label(name, &input)));
            // File-mutating tools keep their diff in the transcript.
            append_tool_diff(body, name, &input, workspace);
        }
        Some("tool_call_result") => {
            let code = value.get("exit_code").and_then(Value::as_i64);
            let glyph = if code.unwrap_or(0) == 0 {
                "  ✓"
            } else {
                "  ✗"
            };
            let first = value
                .get("output")
                .and_then(Value::as_str)
                .and_then(|o| o.lines().find(|l| !l.trim().is_empty()))
                .unwrap_or("");
            ensure_newline(body);
            body.push_str(&format!("{glyph} {}\n", truncate(first, 100)));
        }
        Some("tool_call_error") => {
            let msg = value.get("message").and_then(Value::as_str).unwrap_or("");
            ensure_newline(body);
            body.push_str(&format!("  ✗ {}\n", truncate(msg, 100)));
        }
        Some("error") => {
            let msg = value.get("message").and_then(Value::as_str).unwrap_or("");
            ensure_newline(body);
            body.push_str(&format!("✗ {}\n", truncate(msg, 120)));
        }
        _ => {}
    }
}

/// Finalize the ordered agent body for display (strip tool-call envelopes the
/// model/provider emitted as text, then trim).
fn finalize_agent_text(body: &str) -> String {
    strip_tool_envelopes(body).trim().to_string()
}

/// Remove `<inductor_tool_call>…</inductor_tool_call>` envelopes from assistant
/// prose so they don't show as raw JSON. A trailing unterminated envelope (still
/// streaming) is dropped too — the tool log renders those instead.
fn strip_tool_envelopes(text: &str) -> String {
    const OPEN: &str = "<inductor_tool_call>";
    const CLOSE: &str = "</inductor_tool_call>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after = &rest[start + OPEN.len()..];
        match after.find(CLOSE) {
            Some(end) => rest = &after[end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Turn the full harness NDJSON event stream into a human-readable transcript.
/// (Batch form; the live UI streams via [`apply_event_line`]/[`finalize_agent_text`].)
#[cfg(test)]
fn format_agent_events(stdout: &str) -> String {
    let mut body = String::new();
    let workspace = std::env::temp_dir();
    for line in stdout.lines() {
        apply_event_line(line.trim(), &mut body, &workspace);
    }
    let out = finalize_agent_text(&body);
    if out.is_empty() {
        "(no output)".to_string()
    } else {
        out
    }
}

/// A human-readable header for a tool call, e.g. `Update(src/x.rs)`, `Read(a.md)`,
/// `bash ls -la`, `grep /TODO/` — mirroring how Claude Code labels tool use.
fn tool_call_label(name: &str, input: &serde_json::Value) -> String {
    use serde_json::Value;
    let pick = |key: &str| input.get(key).and_then(Value::as_str);
    let path = pick("path").or_else(|| pick("file_path"));
    // Show a short, workspace-relative path when possible.
    let short = |p: &str| short_path(p);
    match name {
        // Inductor harness tools (Codex text protocol).
        "write_file" => format!("Write({})", path.unwrap_or("?")),
        "edit_file" | "multi_edit" => format!("Update({})", path.unwrap_or("?")),
        "apply_patch_freeform" | "apply_patch_structured" => "Apply patch".to_string(),
        "read_file" => format!("Read({})", path.unwrap_or("?")),
        "grep" => format!("grep /{}/", pick("pattern").unwrap_or("")),
        "bash" => format!("bash {}", truncate(pick("command").unwrap_or(""), 80)),
        "web_search" => format!("WebSearch {}", truncate(pick("query").unwrap_or(""), 60)),
        // Claude Agent SDK native tools (capitalized).
        "Write" => format!("Write({})", path.map(short).unwrap_or_else(|| "?".into())),
        "Edit" | "MultiEdit" => {
            format!("Update({})", path.map(short).unwrap_or_else(|| "?".into()))
        }
        "Read" => format!("Read({})", path.map(short).unwrap_or_else(|| "?".into())),
        "NotebookEdit" => format!(
            "Notebook({})",
            path.map(short).unwrap_or_else(|| "?".into())
        ),
        "Bash" => format!("bash {}", truncate(pick("command").unwrap_or(""), 80)),
        "Grep" => format!("grep /{}/", pick("pattern").unwrap_or("")),
        "Glob" => format!("glob {}", pick("pattern").unwrap_or("")),
        "TodoWrite" => "Update todos".to_string(),
        "WebFetch" => format!("WebFetch {}", pick("url").unwrap_or("")),
        "WebSearch" => format!("WebSearch {}", truncate(pick("query").unwrap_or(""), 60)),
        other => {
            let arg = path
                .map(|p| format!("({p})"))
                .or_else(|| pick("command").map(|c| format!(" {}", truncate(c, 60))))
                .unwrap_or_default();
            format!("{other}{arg}")
        }
    }
}

/// Append a line to `~/.inductor-debug.log` (shared with the run subprocess).
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
        let _ = writeln!(file, "[tui] {msg}");
    }
}

fn truncate(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn workspace_with_files() -> std::path::PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("inductor-mention-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("greet.py"), "def greet(name):\n    return name\n").unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        dir
    }

    fn app_in(workspace: &std::path::Path) -> App {
        App::new(TuiOptions {
            workspace: workspace.to_path_buf(),
            provider: "codex".into(),
            model: "gpt-5.5".into(),
            state_db: Some(workspace.join("state.db")),
            diff_base: "HEAD".into(),
        })
    }

    fn git(workspace: &std::path::Path, args: &[&str]) {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(workspace)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn type_str(app: &mut App, s: &str) {
        for ch in s.chars() {
            app.insert_char(ch);
            app.refresh_popups();
        }
    }

    #[test]
    fn cursor_moves_and_inserts_midword() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "helo");
        // Move cursor left past "o", insert the missing "l": he|lo -> hello
        app.cursor_left(); // before 'o'
        app.cursor_left(); // before 'l'
        app.insert_char('l');
        assert_eq!(app.prompt, "hello");
        // Home/End jump to the extremes.
        app.cursor = 0;
        app.insert_char('>');
        assert_eq!(app.prompt, ">hello");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn backspace_deletes_before_cursor_not_at_end() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "abcd");
        app.cursor_left(); // between c and d: abc|d
        app.backspace(); // removes 'c': ab|d
        assert_eq!(app.prompt, "abd");
        assert_eq!(app.cursor, 2);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn ctrl_j_inserts_newline_without_submitting() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        type_str(&mut app, "line one");
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        type_str(&mut app, "line two");
        assert_eq!(app.prompt, "line one\nline two");
        // Nothing was submitted (no run spawned, transcript untouched).
        assert!(!app.is_running());
        assert!(app.transcript.is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn pending(selected: usize) -> PendingPermission {
        PendingPermission {
            request_id: "01ABCDEF".to_string(),
            tool_name: "Write".to_string(),
            reason: "Create file".to_string(),
            input_json: serde_json::json!({ "path": "a.rs", "content": "fn main() {}" }),
            transcript_index: 0,
            selected,
            typing_message: false,
            message: String::new(),
        }
    }

    #[test]
    fn permission_prompt_navigates_and_resolves() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        app.pending_permission = Some(pending(0));

        // Down highlights "allow all session"; Enter resolves it.
        app.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.pending_permission.as_ref().unwrap().selected, 1);
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.pending_permission.is_none());
        assert!(app.status.contains("session"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn permission_prompt_quick_allow_with_number_key() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        app.pending_permission = Some(pending(0));
        app.handle_key(KeyEvent::from(KeyCode::Char('1')));
        assert!(app.pending_permission.is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn permission_prompt_deny_with_typed_message() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        app.pending_permission = Some(pending(0));

        // Option 3 switches into deny-with-message mode.
        app.handle_key(KeyEvent::from(KeyCode::Char('3')));
        assert!(app.pending_permission.as_ref().unwrap().typing_message);
        for ch in "nope".chars() {
            app.handle_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        assert_eq!(app.pending_permission.as_ref().unwrap().message, "nope");
        // Enter sends the denial and clears the prompt.
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert!(app.pending_permission.is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn permission_preview_shows_command_and_diff() {
        let cmd = permission_preview_lines(
            "Bash",
            &serde_json::json!({ "command": "rm -rf build" }),
            80,
        );
        assert!(line_text(&cmd[0]).contains("rm -rf build"));

        let write = permission_preview_lines(
            "Write",
            &serde_json::json!({ "file_path": "src/x.rs", "content": "line1\nline2" }),
            80,
        );
        let all: String = write
            .iter()
            .map(|l| line_text(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("src/x.rs"));
        assert!(all.contains("+ line1"));
        assert!(all.contains("+ line2"));
    }

    #[test]
    fn up_down_recall_prompt_history() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        // Two submitted prompts feed history (submit spawns a run; record first).
        app.history.push("first".to_string());
        app.history.push("second".to_string());

        app.handle_key(KeyEvent::from(KeyCode::Up)); // newest
        assert_eq!(app.prompt, "second");
        app.handle_key(KeyEvent::from(KeyCode::Up)); // older
        assert_eq!(app.prompt, "first");
        app.handle_key(KeyEvent::from(KeyCode::Up)); // clamp at oldest
        assert_eq!(app.prompt, "first");
        app.handle_key(KeyEvent::from(KeyCode::Down)); // back toward newest
        assert_eq!(app.prompt, "second");
        app.handle_key(KeyEvent::from(KeyCode::Down)); // past newest -> empty draft
        assert_eq!(app.prompt, "");
        assert_eq!(app.history_index, None);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn page_keys_scroll_and_clamp() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        app.view_max.set(20);
        app.view_h.set(10);
        app.scroll_to_bottom();

        app.handle_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.scroll_top.get(), 12); // view_h - 2
        assert!(!app.follow_tail.get());
        app.handle_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.scroll_top.get(), 4);
        app.handle_key(KeyEvent::from(KeyCode::PageUp));
        assert_eq!(app.scroll_top.get(), 0); // clamped to top
        app.handle_key(KeyEvent::from(KeyCode::PageDown));
        assert_eq!(app.scroll_top.get(), 8);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn inline_markdown_splits_bold_and_code() {
        let segs = inline_md("see **Backend** and `npm`", theme::FG);
        // Bold segment present.
        assert!(
            segs.iter()
                .any(|(t, s)| t == "Backend" && s.add_modifier.contains(Modifier::BOLD))
        );
        // Code segment present, and no literal markers leak through.
        assert!(segs.iter().any(|(t, _)| t == "npm"));
        let joined: String = segs.iter().map(|(t, _)| t.as_str()).collect();
        assert!(!joined.contains('*'));
        assert!(!joined.contains('`'));
    }

    #[test]
    fn markdown_header_and_bullet_render() {
        let header = markdown_line("## Backend", 80);
        assert_eq!(header.len(), 1);
        let bullet = markdown_line("- do a thing", 80);
        let text: String = bullet[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("•"));
        assert!(text.contains("do a thing"));
    }

    #[test]
    fn sessions_command_lists_workspace_sessions_and_resumes() {
        use harness_core::{ProviderId, WorkspaceId};
        use persistence::{StoredMessage, new_session_record};

        let ws = workspace_with_files();
        let state_db = ws.join("state.db");
        let sid = SessionId::new();
        let wid = WorkspaceId::new();
        {
            let db = WorkspaceDb::open(&state_db).unwrap();
            let rec = new_session_record(
                sid,
                wid,
                ProviderId("codex".to_string()),
                "gpt-5.5".to_string(),
            )
            .unwrap();
            db.upsert_session(&rec).unwrap();
            db.replace_messages(
                sid,
                &[
                    StoredMessage::new("user", "build a todo app", 0),
                    StoredMessage::new("assistant", "on it", 1),
                ],
            )
            .unwrap();
        }

        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        app.open_sessions();

        let palette = app.palette.as_ref().unwrap();
        assert_eq!(palette.kind, PaletteKind::Sessions);
        assert!(palette.items[0].starts_with(&sid.to_string()));
        assert!(palette.items[0].contains("build a todo app"));

        // Selecting a session resumes it and restores the visible transcript.
        app.popup_accept();
        assert!(app.palette.is_none());
        assert_eq!(app.session_id.as_deref(), Some(sid.to_string().as_str()));
        assert_eq!(app.transcript.len(), 2);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn long_prompt_wraps_to_multiple_rows() {
        let text = "a".repeat(25);
        assert_eq!(prompt_text_width(14), 10);
        assert_eq!(prompt_visual_rows(&text, 10, 99), 3);
        assert_eq!(prompt_visual_rows(&text, 10, 2), 2);
        // Explicit newlines also add rows.
        assert_eq!(prompt_visual_rows("a\nb\nc", 80, 99), 3);
        // Cursor at end of a full row lands on the right coordinates.
        let (rows, (r, c)) = layout_prompt("abcde", 5, 5);
        assert_eq!(rows.len(), 1);
        assert_eq!((r, c), (0, 5));
    }

    #[test]
    fn slash_opens_command_palette() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "/");
        let palette = app.palette.as_ref().unwrap();
        assert_eq!(palette.kind, PaletteKind::Commands);
        assert!(palette.items.iter().any(|c| c == "/model"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn slash_palette_filters_while_typing() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "/mo");
        let palette = app.palette.as_ref().unwrap();
        assert_eq!(palette.kind, PaletteKind::Commands);
        assert_eq!(palette.items, vec!["/model".to_string()]);

        type_str(&mut app, "del");
        let palette = app.palette.as_ref().unwrap();
        assert_eq!(palette.items, vec!["/model".to_string()]);
        assert!(is_command_prompt(&app.prompt));

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn slash_palette_closes_when_no_commands_match_or_escape() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "/");
        assert_eq!(app.palette.as_ref().unwrap().kind, PaletteKind::Commands);

        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.palette.is_none());

        app.clear_prompt();
        type_str(&mut app, "/");
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.palette.is_none());
        assert!(!app.esc_armed);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn model_picker_closes_on_text_or_escape() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "/");
        app.popup_accept();
        assert_eq!(app.palette.as_ref().unwrap().kind, PaletteKind::Models);

        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        assert!(app.palette.is_none());

        app.clear_prompt();
        type_str(&mut app, "/");
        app.popup_accept();
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(app.palette.is_none());
        assert!(!app.esc_armed);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn model_command_lists_all_providers_and_switches() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws); // provider codex
        type_str(&mut app, "/model");
        app.popup_accept(); // pick "/model" -> opens model picker
        let palette = app.palette.as_ref().unwrap();
        assert_eq!(palette.kind, PaletteKind::Models);
        // Claude and OpenAI models are shown together.
        assert!(palette.items.iter().any(|i| i.starts_with("claude · ")));
        assert!(palette.items.iter().any(|i| i.starts_with("openai · ")));

        // Picking a Claude model switches the provider too.
        let claude_idx = palette
            .items
            .iter()
            .position(|i| i == "claude · opus")
            .unwrap();
        app.palette.as_mut().unwrap().index = claude_idx;
        app.popup_accept();
        assert!(app.palette.is_none());
        assert_eq!(app.provider, "claude");
        assert_eq!(app.model, "opus");
        assert!(app.prompt.is_empty());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn fast_command_toggles_minimal_effort() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.effort = Effort::High;

        type_str(&mut app, "/fast");
        app.popup_accept();
        assert!(app.fast);
        assert_eq!(app.effort, Effort::Minimal);

        type_str(&mut app, "/fast");
        app.popup_accept();
        assert!(!app.fast);
        assert_eq!(app.effort, Effort::High); // restored
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn usage_command_toggles_overlay_and_esc_hides_it() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;

        type_str(&mut app, "/usage");
        app.popup_accept();
        assert!(app.show_usage);

        // Overlay renders without panicking.
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();

        // Esc hides the overlay instead of arming the interrupt.
        app.handle_key(KeyEvent::from(KeyCode::Esc));
        assert!(!app.show_usage);
        assert!(!app.esc_armed);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn permissions_command_sets_approval() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "/permissions");
        app.popup_accept();
        let palette = app.palette.as_ref().unwrap();
        assert_eq!(palette.kind, PaletteKind::Permissions);

        let idx = palette
            .items
            .iter()
            .position(|i| i == "on-request")
            .unwrap();
        app.palette.as_mut().unwrap().index = idx;
        app.popup_accept();
        assert_eq!(app.approval, "on-request");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn compact_keeps_transcript_and_needs_a_session() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.transcript.push(ChatEntry::User("a".into()));
        app.transcript.push(ChatEntry::Agent("b".into()));

        // No provider session yet → compaction is a no-op note, history kept.
        type_str(&mut app, "/compact");
        app.popup_accept();
        assert_eq!(app.transcript.len(), 2);
        assert!(app.status.contains("Nothing to compact"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn clear_wipes_transcript_session_and_context() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.transcript.push(ChatEntry::User("a".into()));
        app.transcript.push(ChatEntry::Agent("b".into()));
        app.session_id = Some("01OLDSESSION".into());
        app.pending_seed = Some("summary".into());
        app.context_used = 120_000;

        type_str(&mut app, "/clear");
        app.popup_accept();

        assert!(app.transcript.is_empty());
        assert!(app.session_id.is_none());
        assert!(app.pending_seed.is_none());
        assert_eq!(app.context_used, 0);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn context_window_per_model() {
        assert_eq!(context_window_for("opus"), 250_000);
        assert_eq!(context_window_for("claude-sonnet-4-6"), 250_000);
        assert_eq!(context_window_for("haiku"), 250_000);
        assert_eq!(context_window_for("gpt-5.5"), 250_000);
    }

    #[test]
    fn pending_seed_prepends_to_next_prompt() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        // Simulate a completed compaction.
        app.pending_seed = Some("did X, edited a.rs".into());
        app.prompt = "now do Y".into();
        // Reproduce submit_prompt's seed-prepend (without spawning a process).
        let mut composed = app.composed_prompt();
        if let Some(seed) = app.pending_seed.take() {
            composed = format!("[Summary of earlier conversation]\n{seed}\n\n{composed}");
        }
        assert!(composed.contains("did X, edited a.rs"));
        assert!(composed.contains("now do Y"));
        assert!(app.pending_seed.is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn parses_codex_status_screen() {
        let screen = concat!(
            "  Account:    me@example.com (Plus)\n",
            "  5h limit:             [███████████████████░] 95% left (resets 16:16)\n",
            "  Weekly limit:         [██████████████████░░] 88% left (resets 09:24 on 11 Jun)\n",
        );
        let (five, weekly) = parse_codex_status(screen);
        let five = five.unwrap();
        let weekly = weekly.unwrap();
        // Native polarity preserved: codex reports "% left", no conversion.
        assert_eq!(five.percent, 95.0);
        assert_eq!(five.metric, Metric::Left);
        assert!(five.reset_label.unwrap().contains("16:16"));
        assert_eq!(weekly.percent, 88.0);
        assert_eq!(weekly.metric, Metric::Left);
        assert!(weekly.reset_label.unwrap().contains("11 Jun"));
    }

    #[test]
    fn parses_claude_usage_screen() {
        let screen = concat!(
            "Current session\n",
            "  [####------]  18% used\n",
            "  Resets 12:50am (America/Phoenix)\n",
            "Current week (all models)\n",
            "  [########--]  79% used\n",
            "  Resets Jun 6 at 10pm (America/Phoenix)\n",
        );
        let (five, weekly) = parse_claude_usage(screen);
        let five = five.unwrap();
        let weekly = weekly.unwrap();
        assert_eq!(five.percent, 18.0);
        assert_eq!(five.metric, Metric::Used);
        assert!(five.reset_label.unwrap().contains("12:50am"));
        assert_eq!(weekly.percent, 79.0);
        assert!(weekly.reset_label.unwrap().contains("Jun 6"));
    }

    #[test]
    fn usage_overlay_renders_for_both_providers() {
        for provider in ["codex", "claude"] {
            let ws = workspace_with_files();
            let mut app = app_in(&ws);
            app.provider = provider.to_string();
            app.screen = Screen::Session;
            app.show_usage = true;
            app.context_used = 120_000;
            app.provider_usage = Some(ProviderUsage {
                five_hour: Some(LimitWindow {
                    percent: 95.0,
                    metric: Metric::Left,
                    reset_label: Some("resets 16:16".into()),
                }),
                weekly: Some(LimitWindow {
                    percent: 10.0,
                    metric: Metric::Used,
                    reset_label: Some("resets Jun 11".into()),
                }),
                note: "test".into(),
            });
            let mut terminal = Terminal::new(TestBackend::new(90, 28)).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let _ = std::fs::remove_dir_all(&ws);
        }
    }

    #[test]
    fn effort_command_changes_effort() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        assert_eq!(app.effort, Effort::Medium);
        type_str(&mut app, "/effort");
        app.popup_accept(); // opens effort picker
        assert_eq!(app.palette.as_ref().unwrap().kind, PaletteKind::Efforts);

        let high = Effort::ALL.iter().position(|e| *e == Effort::High).unwrap();
        app.palette.as_mut().unwrap().index = high;
        app.popup_accept();
        assert_eq!(app.effort, Effort::High);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn slash_palette_renders() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        type_str(&mut app, "/");
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn renders_welcome_then_enter_starts_session() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        assert_eq!(app.screen, Screen::Welcome);

        // Welcome renders without panicking.
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|frame| render(frame, &app)).unwrap();

        // Enter starts the session.
        app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Session);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn double_esc_pauses_and_does_not_quit() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;

        assert!(!app.handle_key(KeyEvent::from(KeyCode::Esc))); // arm (warning)
        assert!(app.esc_armed);
        assert!(app.status.contains("Press Esc again"));
        assert!(!app.handle_key(KeyEvent::from(KeyCode::Esc))); // confirm, no quit
        assert!(!app.esc_armed);
        // Nothing is running in this test, so the confirm is a no-op interrupt.
        assert!(app.status.contains("nothing to interrupt"));

        // Ctrl+C warns first, then a second consecutive press quits.
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.handle_key(ctrl_c));
        assert!(app.status.contains("again to quit"));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.handle_key(ctrl_c));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn any_key_disarms_ctrl_c() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.screen = Screen::Session;
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.handle_key(ctrl_c));
        // Typing something else disarms; the next ctrl+c warns again.
        app.handle_key(KeyEvent::from(KeyCode::Char('x')));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!app.handle_key(ctrl_c));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn renders_conversation_without_panicking() {
        for (w, h) in [(60u16, 24u16), (100, 40), (200, 60)] {
            let ws = workspace_with_files();
            let mut app = app_in(&ws);
            app.screen = Screen::Session;
            app.transcript
                .push(ChatEntry::User("refactor greet".into()));
            app.transcript.push(ChatEntry::Agent(
                "Done.\n\n● read_file greet.py\n  ✓ ok".into(),
            ));
            app.transcript.push(ChatEntry::Diff(vec![
                DiffRow::header("Update(greet.py)"),
                DiffRow::stat("Added 1 line, removed 1 line"),
                DiffRow {
                    kind: DiffRowKind::Add,
                    line_no: Some(1),
                    syntax_extension: Some("txt".into()),
                    text: "new line".into(),
                },
                DiffRow {
                    kind: DiffRowKind::Remove,
                    line_no: Some(1),
                    syntax_extension: Some("txt".into()),
                    text: "old line".into(),
                },
            ]));
            app.transcript.push(ChatEntry::Error("boom".into()));
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let _ = std::fs::remove_dir_all(&ws);
        }
    }

    #[test]
    fn at_triggers_completions_and_enter_accepts() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "explain @gr");
        assert!(app.completion_active);
        assert!(app.completions.iter().any(|p| p == "greet.py"));

        app.accept_completion();
        assert!(!app.completion_active);
        assert_eq!(app.prompt, "explain @greet.py ");
        assert_eq!(app.mentioned_paths(), vec!["greet.py".to_string()]);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn enter_on_directory_drills_in_then_accepts_file() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        app.prompt.push('@');
        app.update_completions();
        let dir_idx = app.completions.iter().position(|c| c == "src/").unwrap();
        app.completion_index = dir_idx;

        app.accept_completion();
        assert_eq!(app.prompt, "@src/");
        assert!(app.completion_active);
        assert!(app.completions.iter().any(|c| c == "src/main.rs"));

        let file_idx = app
            .completions
            .iter()
            .position(|c| c == "src/main.rs")
            .unwrap();
        app.completion_index = file_idx;
        app.accept_completion();
        assert_eq!(app.prompt, "@src/main.rs ");
        assert!(!app.completion_active);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn space_keeps_directory_mention_as_is() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "@src");
        assert!(app.completion_active);

        type_str(&mut app, " ");
        assert!(!app.completion_active);
        assert_eq!(app.prompt, "@src ");
        assert_eq!(app.mentioned_paths(), vec!["src".to_string()]);
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn directory_mention_injects_listing_not_recursive_contents() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "show @src/ contents");
        let composed = app.composed_prompt();
        assert!(composed.contains("===== @src/ (directory) ====="));
        assert!(composed.contains("main.rs"));
        assert!(!composed.contains("fn main() {}"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn composed_prompt_inlines_mentioned_file() {
        let ws = workspace_with_files();
        let mut app = app_in(&ws);
        type_str(&mut app, "explain @greet.py please");
        let composed = app.composed_prompt();
        assert!(composed.contains("===== @greet.py ====="));
        assert!(composed.contains("def greet(name):"));
        assert!(composed.contains("Task:\nexplain @greet.py please"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn mention_rejects_workspace_escape() {
        let ws = workspace_with_files();
        assert!(read_workspace_entry(&ws, "../secret").is_none());
        assert!(read_workspace_entry(&ws, "/etc/hosts").is_none());
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn format_agent_events_builds_readable_transcript() {
        let stream = concat!(
            r#"{"type":"text_delta","session_id":"s","text":"Hello"}"#,
            "\n",
            r#"{"type":"tool_call_start","session_id":"s","tool_call_id":"t","name":"read_file","input_json":{"path":"a.rs"}}"#,
            "\n",
            r#"{"type":"tool_call_result","session_id":"s","tool_call_id":"t","output":"fn main(){}","exit_code":0}"#,
            "\n",
            r#"{"type":"result","session_id":"s","stop_reason":"end_turn"}"#,
        );
        let out = format_agent_events(stream);
        assert!(out.contains("Hello"));
        assert!(out.contains("● Read(a.rs)"));
        assert!(out.contains("✓ fn main(){}"));
    }

    #[test]
    fn agent_body_interleaves_tools_and_text_in_order() {
        let stream = concat!(
            r#"{"type":"text_delta","session_id":"s","text":"First I read it."}"#,
            "\n",
            r#"{"type":"tool_call_start","session_id":"s","tool_call_id":"t","name":"read_file","input_json":{"path":"a.rs"}}"#,
            "\n",
            r#"{"type":"tool_call_result","session_id":"s","tool_call_id":"t","output":"fn a(){}","exit_code":0}"#,
            "\n",
            r#"{"type":"text_delta","session_id":"s","text":"Now I write it."}"#,
            "\n",
            r#"{"type":"tool_call_start","session_id":"s","tool_call_id":"u","name":"write_file","input_json":{"path":"b.rs"}}"#,
            "\n",
        );
        let out = format_agent_events(stream);
        // Order is preserved: text, tool+result, text, next tool — not all text
        // then all tools.
        let read_at = out.find("Read(a.rs)").unwrap();
        let mid_text = out.find("Now I write it.").unwrap();
        let write_at = out.find("Write(b.rs)").unwrap();
        assert!(out.find("First I read it.").unwrap() < read_at);
        assert!(read_at < mid_text);
        assert!(mid_text < write_at);
    }

    #[test]
    fn write_tool_embeds_persistent_diff_in_body() {
        let stream = concat!(
            r#"{"type":"tool_call_start","session_id":"s","tool_call_id":"t","name":"Write","input_json":{"file_path":"a.rs","content":"fn main() {\n    println!(\"hi\");\n}"}}"#,
            "\n",
        );
        let out = format_agent_events(stream);
        assert!(out.contains("● Write(a.rs)"));
        assert!(out.contains("└ Added 3 lines"));
        // Numbered add lines stay in the transcript body.
        assert!(out.contains("    1 + fn main() {"));
        assert!(out.contains("    3 + }"));
    }

    #[test]
    fn edit_tool_diff_uses_real_file_line_numbers() {
        let ws = workspace_with_files();
        let mut body = String::new();
        // greet.py content: "def greet(name):\n    return name\n" — the old
        // text starts on line 2.
        let line = serde_json::json!({
            "type": "tool_call_start",
            "session_id": "s",
            "tool_call_id": "t",
            "name": "Edit",
            "input_json": {
                "file_path": "greet.py",
                "old_string": "    return name",
                "new_string": "    return name.upper()"
            }
        })
        .to_string();
        apply_event_line(&line, &mut body, &ws);
        assert!(body.contains("    2 -     return name"));
        assert!(body.contains("    2 +     return name.upper()"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn diff_body_line_parses_and_rejects() {
        let (gutter, sign, content) = parse_diff_body_line("    7 + let x = 1;").unwrap();
        assert_eq!(gutter, "    7");
        assert_eq!(sign, '+');
        assert_eq!(content, "let x = 1;");
        assert!(parse_diff_body_line("ordinary prose line").is_none());
        assert!(parse_diff_body_line("1. numbered list item").is_none());
    }

    #[test]
    fn path_token_under_click_is_extracted() {
        // Click in the middle of a path-looking word.
        assert_eq!(
            path_token_at("see src/main.rs for details", 6).as_deref(),
            Some("src/main.rs")
        );
        // `Tool(path)` labels resolve to the inner path.
        assert_eq!(
            path_token_at("● Read(greet.py)", 9).as_deref(),
            Some("greet.py")
        );
        // Plain words are not paths.
        assert_eq!(path_token_at("just words here", 2), None);
    }

    #[test]
    fn diff_stat_drops_zero_side() {
        assert_eq!(diff_stat(19, 0), "Added 19 lines");
        assert_eq!(diff_stat(0, 1), "Removed 1 line");
        assert_eq!(diff_stat(2, 3), "Added 2 lines, removed 3 lines");
    }

    #[test]
    fn end_of_turn_diff_hides_pre_existing_untracked_files() {
        let ws = workspace_with_files();
        git(&ws, &["init"]);
        git(&ws, &["add", "greet.py", "src/main.rs"]);
        git(
            &ws,
            &[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-m",
                "initial",
            ],
        );
        std::fs::write(ws.join("plan.md"), "existing plan\n").unwrap();

        let baseline = diff_worktree(&DiffRequest::tracked_only(&ws, "HEAD")).unwrap();
        let mut app = app_in(&ws);
        app.push_diff_entry(Some(&baseline));
        assert!(app.transcript.is_empty());

        std::fs::write(
            ws.join("greet.py"),
            "def greet(name):\n    return name.upper()\n",
        )
        .unwrap();
        app.push_diff_entry(Some(&baseline));
        let diff_text = match app.transcript.last().unwrap() {
            ChatEntry::Diff(rows) => rows
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        assert!(diff_text.contains("Update(greet.py)"));
        assert!(diff_text.contains("return name.upper()"));
        assert!(!diff_text.contains("plan.md"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn tool_envelopes_are_stripped_from_prose() {
        let prose = "Let me write it.\n\
             <inductor_tool_call>{\"name\":\"write_file\",\"input\":{\"path\":\"a.rs\"}}</inductor_tool_call>\n\
             All done.";
        let out = finalize_agent_text(prose);
        assert!(!out.contains("inductor_tool_call"));
        assert!(out.contains("Let me write it."));
        assert!(out.contains("All done."));
    }

    #[test]
    fn tool_call_labels_match_claude_style() {
        assert_eq!(
            tool_call_label("write_file", &serde_json::json!({ "path": "x.rs" })),
            "Write(x.rs)"
        );
        assert_eq!(
            tool_call_label("edit_file", &serde_json::json!({ "path": "x.rs" })),
            "Update(x.rs)"
        );
        assert_eq!(
            tool_call_label("bash", &serde_json::json!({ "command": "ls -la" })),
            "bash ls -la"
        );
    }

    #[test]
    fn short_path_collapses_deep_paths() {
        assert_eq!(short_path("/a/b/c/d/e"), "…/d/e");
    }
}
