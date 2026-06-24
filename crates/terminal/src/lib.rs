use std::{
    collections::HashMap,
    fmt,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TerminalId(u64);

impl TerminalId {
    pub fn value(self) -> u64 {
        self.0
    }
}

impl fmt::Display for TerminalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl TerminalSize {
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            rows: rows.max(1),
            cols: cols.max(1),
        }
    }
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self { rows: 24, cols: 80 }
    }
}

impl From<TerminalSize> for PtySize {
    fn from(value: TerminalSize) -> Self {
        Self {
            rows: value.rows,
            cols: value.cols,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSnapshot {
    pub id: TerminalId,
    pub size: TerminalSize,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub screen_rows: Vec<String>,
    pub contents: String,
    pub raw_output: String,
    pub is_running: bool,
    pub exit_code: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct SpawnTerminalRequest {
    pub workspace: PathBuf,
    pub shell: Option<PathBuf>,
    pub size: TerminalSize,
    pub scrollback: usize,
}

impl SpawnTerminalRequest {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            shell: None,
            size: TerminalSize::default(),
            scrollback: 1_000,
        }
    }
}

#[derive(Debug, Default)]
pub struct PtyManager {
    next_id: u64,
    sessions: HashMap<TerminalId, PtySession>,
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            sessions: HashMap::new(),
        }
    }

    pub fn spawn(&mut self, request: SpawnTerminalRequest) -> Result<TerminalId, TerminalError> {
        let id = TerminalId(self.next_id);
        self.next_id += 1;
        let session = PtySession::spawn(id, request)?;
        self.sessions.insert(id, session);
        Ok(id)
    }

    pub fn write(&mut self, id: TerminalId, input: impl AsRef<[u8]>) -> Result<(), TerminalError> {
        self.session_mut(id)?.write(input)
    }

    pub fn resize(&mut self, id: TerminalId, size: TerminalSize) -> Result<(), TerminalError> {
        self.session_mut(id)?.resize(size)
    }

    pub fn snapshot(&self, id: TerminalId) -> Result<TerminalSnapshot, TerminalError> {
        self.session(id)?.snapshot()
    }

    pub fn try_wait(&mut self, id: TerminalId) -> Result<Option<u32>, TerminalError> {
        self.session_mut(id)?.try_wait()
    }

    pub fn kill(&mut self, id: TerminalId) -> Result<Option<u32>, TerminalError> {
        let mut session = self
            .sessions
            .remove(&id)
            .ok_or(TerminalError::UnknownTerminal { id })?;
        session.kill()
    }

    pub fn ids(&self) -> Vec<TerminalId> {
        self.sessions.keys().copied().collect()
    }

    fn session(&self, id: TerminalId) -> Result<&PtySession, TerminalError> {
        self.sessions
            .get(&id)
            .ok_or(TerminalError::UnknownTerminal { id })
    }

    fn session_mut(&mut self, id: TerminalId) -> Result<&mut PtySession, TerminalError> {
        self.sessions
            .get_mut(&id)
            .ok_or(TerminalError::UnknownTerminal { id })
    }
}

pub struct PtySession {
    id: TerminalId,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    state: Arc<Mutex<TerminalState>>,
    reader_done: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl fmt::Debug for PtySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtySession")
            .field("id", &self.id)
            .field("reader_done", &self.reader_done.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl PtySession {
    pub fn spawn(id: TerminalId, request: SpawnTerminalRequest) -> Result<Self, TerminalError> {
        let workspace = canonical_workspace(&request.workspace)?;
        let shell = request.shell.unwrap_or_else(default_shell);
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(request.size.into())
            .map_err(|source| TerminalError::OpenPty { source })?;

        let mut command = CommandBuilder::new(shell);
        command.cwd(&workspace);
        command.env("TERM", "xterm-256color");

        let child =
            pair.slave
                .spawn_command(command)
                .map_err(|source| TerminalError::SpawnShell {
                    workspace: workspace.clone(),
                    source,
                })?;
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|source| TerminalError::PtyIo { source })?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|source| TerminalError::PtyIo { source })?;

        let state = Arc::new(Mutex::new(TerminalState::new(
            request.size,
            request.scrollback,
        )));
        let reader_done = Arc::new(AtomicBool::new(false));
        let reader_handle = spawn_reader_thread(reader, state.clone(), reader_done.clone());

        Ok(Self {
            id,
            master: pair.master,
            child,
            writer,
            state,
            reader_done,
            reader: Some(reader_handle),
        })
    }

    pub fn write(&mut self, input: impl AsRef<[u8]>) -> Result<(), TerminalError> {
        self.writer
            .write_all(input.as_ref())
            .map_err(|source| TerminalError::WriteFailed { source })?;
        self.writer
            .flush()
            .map_err(|source| TerminalError::WriteFailed { source })
    }

    pub fn resize(&mut self, size: TerminalSize) -> Result<(), TerminalError> {
        self.master
            .resize(size.into())
            .map_err(|source| TerminalError::ResizeFailed { source })?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        state.resize(size);
        Ok(())
    }

    pub fn snapshot(&self) -> Result<TerminalSnapshot, TerminalError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| TerminalError::StatePoisoned)?;
        if state.is_running && self.reader_done.load(Ordering::SeqCst) {
            state.is_running = false;
        }
        Ok(state.snapshot(self.id))
    }

    pub fn try_wait(&mut self) -> Result<Option<u32>, TerminalError> {
        let status = self
            .child
            .try_wait()
            .map_err(|source| TerminalError::WaitFailed { source })?;
        if let Some(status) = status {
            let code = status.exit_code();
            let mut state = self
                .state
                .lock()
                .map_err(|_| TerminalError::StatePoisoned)?;
            state.is_running = false;
            state.exit_code = Some(code);
            Ok(Some(code))
        } else {
            Ok(None)
        }
    }

    pub fn kill(&mut self) -> Result<Option<u32>, TerminalError> {
        let _ = self.child.kill();
        let code = match self.child.wait() {
            Ok(status) => Some(status.exit_code()),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => None,
            Err(source) => return Err(TerminalError::WaitFailed { source }),
        };

        if let Ok(mut state) = self.state.lock() {
            state.is_running = false;
            state.exit_code = code;
        }

        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }

        Ok(code)
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

struct TerminalState {
    parser: vt100::Parser,
    size: TerminalSize,
    raw_output: Vec<u8>,
    is_running: bool,
    exit_code: Option<u32>,
}

impl TerminalState {
    fn new(size: TerminalSize, scrollback: usize) -> Self {
        Self {
            parser: vt100::Parser::new(size.rows, size.cols, scrollback),
            size,
            raw_output: Vec::new(),
            is_running: true,
            exit_code: None,
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.raw_output.extend_from_slice(bytes);
        self.parser.process(bytes);
    }

    fn resize(&mut self, size: TerminalSize) {
        self.size = size;
        self.parser.screen_mut().set_size(size.rows, size.cols);
    }

    fn snapshot(&self, id: TerminalId) -> TerminalSnapshot {
        let screen = self.parser.screen();
        let (cursor_row, cursor_col) = screen.cursor_position();
        TerminalSnapshot {
            id,
            size: self.size,
            cursor_row,
            cursor_col,
            screen_rows: screen.rows(0, self.size.cols).collect(),
            contents: screen.contents(),
            raw_output: String::from_utf8_lossy(&self.raw_output).to_string(),
            is_running: self.is_running,
            exit_code: self.exit_code,
        }
    }
}

fn spawn_reader_thread(
    mut reader: Box<dyn Read + Send>,
    state: Arc<Mutex<TerminalState>>,
    done: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if let Ok(mut state) = state.lock() {
                        state.process(&buffer[..n]);
                    } else {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }

        if let Ok(mut state) = state.lock() {
            state.is_running = false;
        }
        done.store(true, Ordering::SeqCst);
    })
}

fn canonical_workspace(path: &Path) -> Result<PathBuf, TerminalError> {
    let metadata = std::fs::metadata(path).map_err(|source| TerminalError::WorkspaceIo {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(TerminalError::WorkspaceNotDirectory {
            path: path.to_path_buf(),
        });
    }
    path.canonicalize()
        .map_err(|source| TerminalError::WorkspaceIo {
            path: path.to_path_buf(),
            source,
        })
}

fn default_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

#[derive(Debug)]
pub enum TerminalError {
    WorkspaceIo {
        path: PathBuf,
        source: io::Error,
    },
    WorkspaceNotDirectory {
        path: PathBuf,
    },
    OpenPty {
        source: anyhow::Error,
    },
    SpawnShell {
        workspace: PathBuf,
        source: anyhow::Error,
    },
    PtyIo {
        source: anyhow::Error,
    },
    WriteFailed {
        source: io::Error,
    },
    ResizeFailed {
        source: anyhow::Error,
    },
    WaitFailed {
        source: io::Error,
    },
    UnknownTerminal {
        id: TerminalId,
    },
    StatePoisoned,
}

impl fmt::Display for TerminalError {
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
            Self::OpenPty { source } => write!(f, "failed to open PTY: {source}"),
            Self::SpawnShell { workspace, source } => {
                write!(
                    f,
                    "failed to spawn shell in {}: {source}",
                    workspace.display()
                )
            }
            Self::PtyIo { source } => write!(f, "PTY I/O setup failed: {source}"),
            Self::WriteFailed { source } => write!(f, "failed to write to PTY: {source}"),
            Self::ResizeFailed { source } => write!(f, "failed to resize PTY: {source}"),
            Self::WaitFailed { source } => write!(f, "failed to wait for PTY child: {source}"),
            Self::UnknownTerminal { id } => write!(f, "unknown terminal id: {id}"),
            Self::StatePoisoned => f.write_str("terminal state lock was poisoned"),
        }
    }
}

impl std::error::Error for TerminalError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn terminal_size_clamps_to_nonzero() {
        assert_eq!(TerminalSize::new(0, 0), TerminalSize { rows: 1, cols: 1 });
    }

    #[test]
    fn manager_reports_unknown_terminal() {
        let manager = PtyManager::new();

        assert!(matches!(
            manager.snapshot(TerminalId(42)),
            Err(TerminalError::UnknownTerminal { .. })
        ));
    }

    #[test]
    fn rejects_non_directory_workspace() {
        let temp = TempDir::new("not-dir");
        let file = temp.path().join("file.txt");
        fs::write(&file, "x").unwrap();
        let mut manager = PtyManager::new();

        let error = manager
            .spawn(SpawnTerminalRequest::new(file))
            .expect_err("file path must not be accepted as workspace");

        assert!(matches!(error, TerminalError::WorkspaceNotDirectory { .. }));
    }

    #[test]
    fn vt100_snapshot_tracks_processed_output() {
        let mut state = TerminalState::new(TerminalSize::new(5, 20), 10);
        state.process(b"hello\r\nworld");

        let snapshot = state.snapshot(TerminalId(1));

        assert!(snapshot.contents.contains("hello"));
        assert!(snapshot.contents.contains("world"));
        assert!(snapshot.raw_output.contains("hello"));
    }

    #[test]
    fn vt100_snapshot_exposes_physical_rows_for_cursor_rendering() {
        let mut state = TerminalState::new(TerminalSize::new(4, 5), 10);
        state.process(b"abcdef");

        let snapshot = state.snapshot(TerminalId(1));

        assert_eq!((snapshot.cursor_row, snapshot.cursor_col), (1, 1));
        assert_eq!(snapshot.screen_rows[0], "abcde");
        assert_eq!(snapshot.screen_rows[1], "f");
    }

    #[test]
    fn pty_smoke_runs_shell_command_and_captures_output() {
        let temp = TempDir::new("pty-smoke");
        let mut manager = PtyManager::new();
        let mut request = SpawnTerminalRequest::new(temp.path());
        request.shell = Some(PathBuf::from("/bin/sh"));
        request.size = TerminalSize::new(10, 80);
        let id = manager.spawn(request).unwrap();

        manager.write(id, "printf phase8-ready\nexit\n").unwrap();

        let snapshot = wait_for_output(&manager, id, "phase8-ready");
        assert!(snapshot.raw_output.contains("phase8-ready"));
        let _ = manager.kill(id);
    }

    #[test]
    fn resize_updates_snapshot_size() {
        let temp = TempDir::new("pty-resize");
        let mut manager = PtyManager::new();
        let mut request = SpawnTerminalRequest::new(temp.path());
        request.shell = Some(PathBuf::from("/bin/sh"));
        let id = manager.spawn(request).unwrap();

        manager.resize(id, TerminalSize::new(40, 120)).unwrap();
        let snapshot = manager.snapshot(id).unwrap();

        assert_eq!(snapshot.size, TerminalSize::new(40, 120));
        let _ = manager.kill(id);
    }

    fn wait_for_output(manager: &PtyManager, id: TerminalId, needle: &str) -> TerminalSnapshot {
        for _ in 0..100 {
            let snapshot = manager.snapshot(id).unwrap();
            if snapshot.raw_output.contains(needle) {
                return snapshot;
            }
            thread::sleep(Duration::from_millis(20));
        }
        manager.snapshot(id).unwrap()
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
            let path = std::env::temp_dir().join(format!("inductor-terminal-{label}-{nanos}"));
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
