//! SQLite persistence for Inductor.
//!
//! Phase 11 keeps the persistence layer intentionally small and typed:
//! app-level state lives in one database, while each workspace can carry its
//! own `.inductor/state.db` for resumable session transcripts.

use std::{
    fs,
    path::{Path, PathBuf},
};

use harness_core::{
    AllowRule, AllowRuleKind, ProviderId, SessionEvent, SessionId, SessionStatus, WorkspaceId,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const APP_SCHEMA_VERSION: i64 = 3;
const WORKSPACE_SCHEMA_VERSION: i64 = 2;

const APP_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r#"
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(workspace_id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE TABLE settings (
    key TEXT PRIMARY KEY,
    value_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE provider_configs (
    provider_id TEXT PRIMARY KEY,
    config_json TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE credential_sources (
    provider_id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    identity_hint TEXT,
    updated_at TEXT NOT NULL
);

CREATE TABLE allow_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT,
    session_id TEXT,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(workspace_id, session_id, kind, value)
);

CREATE INDEX idx_sessions_workspace ON sessions(workspace_id);
CREATE INDEX idx_allow_rules_scope ON allow_rules(workspace_id, session_id);
"#,
    },
    Migration {
        version: 2,
        sql: r#"
ALTER TABLE sessions ADD COLUMN display_name TEXT;
"#,
    },
    Migration {
        version: 3,
        sql: r#"
CREATE TABLE worktrees (
    id TEXT PRIMARY KEY,
    source_repo TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    branch_name TEXT NOT NULL,
    base_branch TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(id) REFERENCES workspaces(id) ON DELETE CASCADE
);

CREATE INDEX idx_worktrees_source ON worktrees(source_repo);
"#,
    },
];

const WORKSPACE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: r#"
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    model TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(session_id, ordinal),
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE session_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    event_json TEXT NOT NULL,
    ordinal INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(session_id, ordinal),
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE tool_calls (
    id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    name TEXT NOT NULL,
    input_json TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
);

CREATE TABLE tool_results (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tool_call_id TEXT NOT NULL,
    output TEXT NOT NULL,
    exit_code INTEGER,
    blob_id TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY(tool_call_id) REFERENCES tool_calls(id) ON DELETE CASCADE
);

CREATE TABLE blobs (
    id TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE allow_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE(session_id, kind, value)
);

CREATE INDEX idx_messages_session ON messages(session_id, ordinal);
CREATE INDEX idx_events_session ON session_events(session_id, ordinal);
CREATE INDEX idx_tool_calls_session ON tool_calls(session_id);
"#,
    },
    Migration {
        version: 2,
        sql: r#"
ALTER TABLE sessions ADD COLUMN display_name TEXT;
"#,
    },
];

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("time format error: {0}")]
    TimeFormat(#[from] time::error::Format),
    #[error("database schema version {found} is newer than supported version {supported}")]
    FutureSchema { found: i64, supported: i64 },
}

pub type Result<T> = std::result::Result<T, PersistenceError>;

#[derive(Debug, Clone, Copy)]
struct Migration {
    version: i64,
    sql: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub path: PathBuf,
    pub display_name: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: SessionId,
    pub workspace_id: WorkspaceId,
    pub provider_id: ProviderId,
    pub model: String,
    pub status: SessionStatus,
    pub display_name: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Lifecycle state of a managed worktree, tracked so a multi-agent view can
/// tell which parallel sessions are active or archived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorktreeStatus {
    /// Worktree exists and an agent may still be working in it.
    Active,
    /// A pull request exists for this worktree branch and has not been merged yet.
    PrOpen,
    /// Legacy status retained for older databases that recorded local merges.
    Merged,
    /// Worktree was discarded.
    Abandoned,
    /// Worktree was archived by the user: the working directory is removed but
    /// the session's chats/messages are preserved.
    Archived,
}

impl WorktreeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            WorktreeStatus::Active => "active",
            WorktreeStatus::PrOpen => "pr_open",
            WorktreeStatus::Merged => "merged",
            WorktreeStatus::Abandoned => "abandoned",
            WorktreeStatus::Archived => "archived",
        }
    }

    pub fn from_db_value(value: &str) -> Self {
        match value {
            "pr_open" => WorktreeStatus::PrOpen,
            "merged" => WorktreeStatus::Merged,
            "abandoned" => WorktreeStatus::Abandoned,
            "archived" => WorktreeStatus::Archived,
            _ => WorktreeStatus::Active,
        }
    }
}

/// A git worktree Inductor manages on behalf of a worktree-mode session. The
/// `id` is the [`WorkspaceId`] of the worktree's workspace, so sessions link
/// back to it through their `workspace_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub id: WorkspaceId,
    pub source_repo: PathBuf,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub base_branch: String,
    pub base_commit: String,
    pub status: WorktreeStatus,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub role: String,
    pub content: String,
    pub ordinal: i64,
}

impl StoredMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>, ordinal: i64) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            ordinal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSourceRecord {
    pub provider_id: ProviderId,
    pub source: String,
    pub identity_hint: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRecord {
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub session_id: SessionId,
    pub name: String,
    pub input_json: serde_json::Value,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultRecord {
    pub tool_call_id: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub blob_id: Option<String>,
}

pub struct AppDb {
    conn: Connection,
}

impl AppDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let existed_before_open = path.exists();
        let conn = Connection::open(path)?;
        configure_connection(&conn)?;
        run_migrations(
            &conn,
            APP_SCHEMA_VERSION,
            APP_MIGRATIONS,
            file_backup(path, existed_before_open),
        )?;
        Ok(Self { conn })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure_connection(&conn)?;
        run_migrations(&conn, APP_SCHEMA_VERSION, APP_MIGRATIONS, None)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<i64> {
        schema_version(&self.conn).map_err(Into::into)
    }

    pub fn upsert_workspace(
        &self,
        id: WorkspaceId,
        path: impl AsRef<Path>,
        display_name: impl AsRef<str>,
    ) -> Result<()> {
        let now = now_rfc3339()?;
        self.conn.execute(
            r#"
INSERT INTO workspaces (id, path, display_name, created_at, updated_at)
VALUES (?1, ?2, ?3, ?4, ?4)
ON CONFLICT(id) DO UPDATE SET
    path = excluded.path,
    display_name = excluded.display_name,
    updated_at = excluded.updated_at
"#,
            params![
                id.to_string(),
                path.as_ref().display().to_string(),
                display_name.as_ref(),
                now
            ],
        )?;
        Ok(())
    }

    pub fn list_workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, display_name, created_at, updated_at FROM workspaces ORDER BY updated_at DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(WorkspaceRecord {
                    id: parse_workspace_id(row.get::<_, String>(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                    display_name: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn upsert_worktree(&self, worktree: &WorktreeRecord) -> Result<()> {
        self.conn.execute(
            r#"
INSERT INTO worktrees (id, source_repo, worktree_path, branch_name, base_branch, base_commit, status, created_at, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
ON CONFLICT(id) DO UPDATE SET
    source_repo = excluded.source_repo,
    worktree_path = excluded.worktree_path,
    branch_name = excluded.branch_name,
    base_branch = excluded.base_branch,
    base_commit = excluded.base_commit,
    status = excluded.status,
    updated_at = excluded.updated_at
"#,
            params![
                worktree.id.to_string(),
                worktree.source_repo.display().to_string(),
                worktree.worktree_path.display().to_string(),
                worktree.branch_name,
                worktree.base_branch,
                worktree.base_commit,
                worktree.status.as_str(),
                worktree.created_at,
                worktree.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_worktree(&self, id: WorkspaceId) -> Result<Option<WorktreeRecord>> {
        let record = self
            .conn
            .query_row(
                r#"
SELECT id, source_repo, worktree_path, branch_name, base_branch, base_commit, status, created_at, updated_at
FROM worktrees
WHERE id = ?1
"#,
                [id.to_string()],
                map_worktree_row,
            )
            .optional()?;
        Ok(record)
    }

    pub fn list_worktrees(&self) -> Result<Vec<WorktreeRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT id, source_repo, worktree_path, branch_name, base_branch, base_commit, status, created_at, updated_at
FROM worktrees
ORDER BY updated_at DESC
"#,
        )?;
        let rows = stmt
            .query_map([], map_worktree_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_worktree_status(&self, id: WorkspaceId, status: WorktreeStatus) -> Result<()> {
        let now = now_rfc3339()?;
        self.conn.execute(
            "UPDATE worktrees SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.to_string(), status.as_str(), now],
        )?;
        Ok(())
    }

    pub fn upsert_session(&self, session: &SessionRecord) -> Result<()> {
        self.conn.execute(
            r#"
INSERT INTO sessions (id, workspace_id, provider_id, model, status, display_name, created_at, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT(id) DO UPDATE SET
    workspace_id = excluded.workspace_id,
    provider_id = excluded.provider_id,
    model = excluded.model,
    status = excluded.status,
    display_name = excluded.display_name,
    updated_at = excluded.updated_at
"#,
            params![
                session.id.to_string(),
                session.workspace_id.to_string(),
                session.provider_id.0,
                session.model,
                session_status_to_str(session.status),
                session.display_name,
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Update only the lifecycle status of an existing session. Used to reflect
    /// live status transitions (streaming, running tools, …) in the dashboard
    /// without rewriting the whole record, so a session is never stranded at
    /// `starting` while it is actually working.
    pub fn set_session_status(&self, id: SessionId, status: SessionStatus) -> Result<()> {
        let now = now_rfc3339()?;
        self.conn.execute(
            "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.to_string(), session_status_to_str(status), now],
        )?;
        Ok(())
    }

    pub fn get_session(&self, session_id: SessionId) -> Result<Option<SessionRecord>> {
        let row = self
            .conn
            .query_row(
                r#"
SELECT id, workspace_id, provider_id, model, status, display_name, created_at, updated_at
FROM sessions
WHERE id = ?1
"#,
                [session_id.to_string()],
                map_session_row,
            )
            .optional()?;
        Ok(row)
    }

    pub fn list_sessions(&self, workspace_id: WorkspaceId) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT id, workspace_id, provider_id, model, status, display_name, created_at, updated_at
FROM sessions
WHERE workspace_id = ?1
ORDER BY updated_at DESC
"#,
        )?;
        let rows = stmt
            .query_map([workspace_id.to_string()], map_session_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn list_incomplete_sessions(&self) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT id, workspace_id, provider_id, model, status, display_name, created_at, updated_at
FROM sessions
WHERE status IN ('starting', 'streaming', 'running_tools', 'waiting_for_permission')
ORDER BY updated_at DESC
"#,
        )?;
        let rows = stmt
            .query_map([], map_session_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn put_setting(&self, key: &str, value: &serde_json::Value) -> Result<()> {
        let now = now_rfc3339()?;
        let value_json = serde_json::to_string(value)?;
        self.conn.execute(
            r#"
INSERT INTO settings (key, value_json, updated_at)
VALUES (?1, ?2, ?3)
ON CONFLICT(key) DO UPDATE SET
    value_json = excluded.value_json,
    updated_at = excluded.updated_at
"#,
            params![key, value_json, now],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let value = self
            .conn
            .query_row(
                "SELECT value_json FROM settings WHERE key = ?1",
                [key],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        value
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(Into::into)
    }

    pub fn put_provider_config(
        &self,
        provider_id: &ProviderId,
        value: &serde_json::Value,
    ) -> Result<()> {
        let now = now_rfc3339()?;
        let config_json = serde_json::to_string(value)?;
        self.conn.execute(
            r#"
INSERT INTO provider_configs (provider_id, config_json, updated_at)
VALUES (?1, ?2, ?3)
ON CONFLICT(provider_id) DO UPDATE SET
    config_json = excluded.config_json,
    updated_at = excluded.updated_at
"#,
            params![provider_id.0, config_json, now],
        )?;
        Ok(())
    }

    pub fn provider_config(&self, provider_id: &ProviderId) -> Result<Option<serde_json::Value>> {
        let value = self
            .conn
            .query_row(
                "SELECT config_json FROM provider_configs WHERE provider_id = ?1",
                [&provider_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        value
            .map(|raw| serde_json::from_str(&raw))
            .transpose()
            .map_err(Into::into)
    }

    pub fn put_credential_source(
        &self,
        provider_id: &ProviderId,
        source: &str,
        identity_hint: Option<&str>,
    ) -> Result<()> {
        let now = now_rfc3339()?;
        self.conn.execute(
            r#"
INSERT INTO credential_sources (provider_id, source, identity_hint, updated_at)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(provider_id) DO UPDATE SET
    source = excluded.source,
    identity_hint = excluded.identity_hint,
    updated_at = excluded.updated_at
"#,
            params![provider_id.0, source, identity_hint, now],
        )?;
        Ok(())
    }

    pub fn credential_source(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Option<CredentialSourceRecord>> {
        let record = self
            .conn
            .query_row(
                r#"
SELECT provider_id, source, identity_hint, updated_at
FROM credential_sources
WHERE provider_id = ?1
"#,
                [&provider_id.0],
                |row| {
                    Ok(CredentialSourceRecord {
                        provider_id: ProviderId(row.get(0)?),
                        source: row.get(1)?,
                        identity_hint: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(record)
    }

    pub fn add_allow_rule(
        &self,
        workspace_id: Option<WorkspaceId>,
        session_id: Option<SessionId>,
        rule: &AllowRule,
    ) -> Result<()> {
        let workspace_key = workspace_id.map(|id| id.to_string());
        let session_key = session_id.map(|id| id.to_string());
        let kind = allow_rule_kind_to_str(rule.kind);
        let existing: i64 = self.conn.query_row(
            r#"
SELECT COUNT(*)
FROM allow_rules
WHERE (workspace_id IS ?1 OR workspace_id = ?1)
  AND (session_id IS ?2 OR session_id = ?2)
  AND kind = ?3
  AND value = ?4
"#,
            params![
                workspace_key.as_deref(),
                session_key.as_deref(),
                kind,
                rule.value
            ],
            |row| row.get(0),
        )?;
        if existing > 0 {
            return Ok(());
        }

        let now = now_rfc3339()?;
        self.conn.execute(
            r#"
INSERT OR IGNORE INTO allow_rules (workspace_id, session_id, kind, value, created_at)
VALUES (?1, ?2, ?3, ?4, ?5)
"#,
            params![
                workspace_key.as_deref(),
                session_key.as_deref(),
                kind,
                rule.value,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn allow_rules(
        &self,
        workspace_id: Option<WorkspaceId>,
        session_id: Option<SessionId>,
    ) -> Result<Vec<AllowRule>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT kind, value
FROM allow_rules
WHERE (workspace_id IS ?1 OR workspace_id = ?1)
  AND (session_id IS ?2 OR session_id = ?2)
ORDER BY id
"#,
        )?;
        let rows = stmt
            .query_map(
                params![
                    workspace_id.map(|id| id.to_string()),
                    session_id.map(|id| id.to_string())
                ],
                |row| {
                    Ok(AllowRule::new(
                        parse_allow_rule_kind(row.get::<_, String>(0)?),
                        row.get::<_, String>(1)?,
                    ))
                },
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

pub struct WorkspaceDb {
    conn: Connection,
}

impl WorkspaceDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let existed_before_open = path.exists();
        let conn = Connection::open(path)?;
        configure_connection(&conn)?;
        run_migrations(
            &conn,
            WORKSPACE_SCHEMA_VERSION,
            WORKSPACE_MIGRATIONS,
            file_backup(path, existed_before_open),
        )?;
        Ok(Self { conn })
    }

    pub fn open_default(workspace_dir: impl AsRef<Path>) -> Result<Self> {
        Self::open(workspace_state_path(workspace_dir))
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        configure_connection(&conn)?;
        run_migrations(&conn, WORKSPACE_SCHEMA_VERSION, WORKSPACE_MIGRATIONS, None)?;
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<i64> {
        schema_version(&self.conn).map_err(Into::into)
    }

    pub fn upsert_session(&self, session: &SessionRecord) -> Result<()> {
        self.conn.execute(
            r#"
INSERT INTO sessions (id, workspace_id, provider_id, model, status, display_name, created_at, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
ON CONFLICT(id) DO UPDATE SET
    provider_id = excluded.provider_id,
    model = excluded.model,
    status = excluded.status,
    display_name = excluded.display_name,
    updated_at = excluded.updated_at
"#,
            params![
                session.id.to_string(),
                session.workspace_id.to_string(),
                session.provider_id.0,
                session.model,
                session_status_to_str(session.status),
                session.display_name,
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, session_id: SessionId) -> Result<Option<SessionRecord>> {
        let row = self
            .conn
            .query_row(
                r#"
SELECT id, workspace_id, provider_id, model, status, display_name, created_at, updated_at
FROM sessions
WHERE id = ?1
"#,
                [session_id.to_string()],
                map_session_row,
            )
            .optional()?;
        Ok(row)
    }

    /// Update only the lifecycle status of an existing session. Mirrors
    /// [`AppDb::set_session_status`] so live status transitions are recorded in
    /// the workspace transcript database as well.
    pub fn set_session_status(&self, id: SessionId, status: SessionStatus) -> Result<()> {
        let now = now_rfc3339()?;
        self.conn.execute(
            "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id.to_string(), session_status_to_str(status), now],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT id, workspace_id, provider_id, model, status, display_name, created_at, updated_at
FROM sessions
ORDER BY updated_at DESC
"#,
        )?;
        let rows = stmt
            .query_map([], map_session_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn replace_messages(
        &self,
        session_id: SessionId,
        messages: &[StoredMessage],
    ) -> Result<()> {
        let mut stmt = self
            .conn
            .prepare("DELETE FROM messages WHERE session_id = ?1")?;
        stmt.execute([session_id.to_string()])?;
        drop(stmt);

        let mut stmt = self.conn.prepare(
            r#"
INSERT INTO messages (session_id, role, content, ordinal, created_at)
VALUES (?1, ?2, ?3, ?4, ?5)
"#,
        )?;
        for (index, message) in messages.iter().enumerate() {
            stmt.execute(params![
                session_id.to_string(),
                message.role,
                message.content,
                if message.ordinal < 0 {
                    index as i64
                } else {
                    message.ordinal
                },
                now_rfc3339()?,
            ])?;
        }
        Ok(())
    }

    pub fn messages(&self, session_id: SessionId) -> Result<Vec<StoredMessage>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT role, content, ordinal
FROM messages
WHERE session_id = ?1
ORDER BY ordinal ASC
"#,
        )?;
        let rows = stmt
            .query_map([session_id.to_string()], |row| {
                Ok(StoredMessage {
                    role: row.get(0)?,
                    content: row.get(1)?,
                    ordinal: row.get(2)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn append_event(&self, session_id: SessionId, event: &SessionEvent) -> Result<i64> {
        let next = next_ordinal(&self.conn, "session_events", session_id)?;
        let event_json = serde_json::to_string(event)?;
        self.conn.execute(
            r#"
INSERT INTO session_events (session_id, event_json, ordinal, created_at)
VALUES (?1, ?2, ?3, ?4)
"#,
            params![session_id.to_string(), event_json, next, now_rfc3339()?],
        )?;
        Ok(next)
    }

    pub fn events(&self, session_id: SessionId) -> Result<Vec<SessionEvent>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT event_json
FROM session_events
WHERE session_id = ?1
ORDER BY ordinal ASC
"#,
        )?;
        let rows = stmt
            .query_map([session_id.to_string()], |row| row.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(|raw| serde_json::from_str(&raw).map_err(Into::into))
            .collect()
    }

    pub fn upsert_tool_call(&self, call: &ToolCallRecord) -> Result<()> {
        let input_json = serde_json::to_string(&call.input_json)?;
        let now = now_rfc3339()?;
        self.conn.execute(
            r#"
INSERT INTO tool_calls (id, session_id, name, input_json, status, created_at, updated_at)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
ON CONFLICT(id) DO UPDATE SET
    input_json = excluded.input_json,
    status = excluded.status,
    updated_at = excluded.updated_at
"#,
            params![
                call.id,
                call.session_id.to_string(),
                call.name,
                input_json,
                call.status,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn add_tool_result(&self, result: &ToolResultRecord) -> Result<()> {
        self.conn.execute(
            r#"
INSERT INTO tool_results (tool_call_id, output, exit_code, blob_id, created_at)
VALUES (?1, ?2, ?3, ?4, ?5)
"#,
            params![
                result.tool_call_id,
                result.output,
                result.exit_code,
                result.blob_id,
                now_rfc3339()?,
            ],
        )?;
        self.conn.execute(
            "UPDATE tool_calls SET status = 'completed', updated_at = ?1 WHERE id = ?2",
            params![now_rfc3339()?, result.tool_call_id],
        )?;
        Ok(())
    }

    pub fn mark_tool_failed(&self, tool_call_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE tool_calls SET status = 'failed', updated_at = ?1 WHERE id = ?2",
            params![now_rfc3339()?, tool_call_id],
        )?;
        Ok(())
    }

    pub fn put_blob(&self, blob: &BlobRecord) -> Result<()> {
        self.conn.execute(
            r#"
INSERT INTO blobs (id, path, bytes, created_at)
VALUES (?1, ?2, ?3, ?4)
ON CONFLICT(id) DO UPDATE SET
    path = excluded.path,
    bytes = excluded.bytes
"#,
            params![
                blob.id,
                blob.path.display().to_string(),
                blob.bytes as i64,
                blob.created_at,
            ],
        )?;
        Ok(())
    }

    pub fn blob(&self, id: &str) -> Result<Option<BlobRecord>> {
        let blob = self
            .conn
            .query_row(
                "SELECT id, path, bytes, created_at FROM blobs WHERE id = ?1",
                [id],
                |row| {
                    Ok(BlobRecord {
                        id: row.get(0)?,
                        path: PathBuf::from(row.get::<_, String>(1)?),
                        bytes: row.get::<_, i64>(2)? as u64,
                        created_at: row.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(blob)
    }

    pub fn add_allow_rule(&self, session_id: Option<SessionId>, rule: &AllowRule) -> Result<()> {
        let session_key = session_id.map(|id| id.to_string());
        let kind = allow_rule_kind_to_str(rule.kind);
        let existing: i64 = self.conn.query_row(
            r#"
SELECT COUNT(*)
FROM allow_rules
WHERE (session_id IS ?1 OR session_id = ?1)
  AND kind = ?2
  AND value = ?3
"#,
            params![session_key.as_deref(), kind, rule.value],
            |row| row.get(0),
        )?;
        if existing > 0 {
            return Ok(());
        }

        self.conn.execute(
            r#"
INSERT OR IGNORE INTO allow_rules (session_id, kind, value, created_at)
VALUES (?1, ?2, ?3, ?4)
"#,
            params![session_key.as_deref(), kind, rule.value, now_rfc3339()?,],
        )?;
        Ok(())
    }

    pub fn allow_rules(&self, session_id: Option<SessionId>) -> Result<Vec<AllowRule>> {
        let mut stmt = self.conn.prepare(
            r#"
SELECT kind, value
FROM allow_rules
WHERE session_id IS ?1 OR session_id = ?1
ORDER BY id
"#,
        )?;
        let rows = stmt
            .query_map([session_id.map(|id| id.to_string())], |row| {
                Ok(AllowRule::new(
                    parse_allow_rule_kind(row.get::<_, String>(0)?),
                    row.get::<_, String>(1)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

pub fn workspace_state_path(workspace_dir: impl AsRef<Path>) -> PathBuf {
    workspace_dir.as_ref().join(".inductor").join("state.db")
}

pub fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

pub fn new_session_record(
    session_id: SessionId,
    workspace_id: WorkspaceId,
    provider_id: ProviderId,
    model: impl Into<String>,
) -> Result<SessionRecord> {
    let now = now_rfc3339()?;
    Ok(SessionRecord {
        id: session_id,
        workspace_id,
        provider_id,
        model: model.into(),
        status: SessionStatus::Starting,
        display_name: None,
        created_at: now.clone(),
        updated_at: now,
    })
}

fn configure_connection(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;
    Ok(())
}

fn run_migrations(
    conn: &Connection,
    supported: i64,
    migrations: &[Migration],
    backup: Option<PathBuf>,
) -> Result<()> {
    conn.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL
);
"#,
    )?;

    let current = schema_version(conn)?;
    if current > supported {
        return Err(PersistenceError::FutureSchema {
            found: current,
            supported,
        });
    }

    let needs_migration = migrations
        .iter()
        .any(|migration| migration.version > current);
    if needs_migration && let Some(backup) = backup {
        fs::copy(backup_source(&backup), &backup)?;
    }

    for migration in migrations {
        if migration.version <= current {
            continue;
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
            params![migration.version, now_rfc3339()?],
        )?;
        tx.commit()?;
    }

    Ok(())
}

fn file_backup(path: &Path, existed_before_open: bool) -> Option<PathBuf> {
    if !existed_before_open {
        return None;
    }

    let timestamp = OffsetDateTime::now_utc().unix_timestamp();
    Some(PathBuf::from(format!("{}.bak.{timestamp}", path.display())))
}

fn backup_source(backup_path: &Path) -> PathBuf {
    let raw = backup_path.display().to_string();
    let Some((source, _)) = raw.rsplit_once(".bak.") else {
        return backup_path.to_path_buf();
    };
    PathBuf::from(source)
}

fn schema_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
        [],
        |row| row.get(0),
    )
}

fn next_ordinal(conn: &Connection, table: &str, session_id: SessionId) -> rusqlite::Result<i64> {
    let sql = format!("SELECT COALESCE(MAX(ordinal), -1) + 1 FROM {table} WHERE session_id = ?1");
    conn.query_row(&sql, [session_id.to_string()], |row| row.get(0))
}

fn map_session_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    Ok(SessionRecord {
        id: parse_session_id(row.get::<_, String>(0)?),
        workspace_id: parse_workspace_id(row.get::<_, String>(1)?),
        provider_id: ProviderId(row.get(2)?),
        model: row.get(3)?,
        status: parse_session_status(row.get::<_, String>(4)?),
        display_name: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn map_worktree_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeRecord> {
    Ok(WorktreeRecord {
        id: parse_workspace_id(row.get::<_, String>(0)?),
        source_repo: PathBuf::from(row.get::<_, String>(1)?),
        worktree_path: PathBuf::from(row.get::<_, String>(2)?),
        branch_name: row.get(3)?,
        base_branch: row.get(4)?,
        base_commit: row.get(5)?,
        status: WorktreeStatus::from_db_value(&row.get::<_, String>(6)?),
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn parse_workspace_id(value: String) -> WorkspaceId {
    value
        .parse()
        .unwrap_or_else(|err| panic!("invalid persisted workspace id {value}: {err}"))
}

fn parse_session_id(value: String) -> SessionId {
    value
        .parse()
        .unwrap_or_else(|err| panic!("invalid persisted session id {value}: {err}"))
}

fn session_status_to_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Idle => "idle",
        SessionStatus::Streaming => "streaming",
        SessionStatus::RunningTools => "running_tools",
        SessionStatus::WaitingForPermission => "waiting_for_permission",
        SessionStatus::Completed => "completed",
        SessionStatus::Failed => "failed",
    }
}

fn parse_session_status(value: String) -> SessionStatus {
    match value.as_str() {
        "starting" => SessionStatus::Starting,
        "idle" => SessionStatus::Idle,
        "streaming" => SessionStatus::Streaming,
        "running_tools" => SessionStatus::RunningTools,
        "waiting_for_permission" => SessionStatus::WaitingForPermission,
        "completed" => SessionStatus::Completed,
        "failed" => SessionStatus::Failed,
        other => panic!("invalid persisted session status {other}"),
    }
}

fn allow_rule_kind_to_str(kind: AllowRuleKind) -> &'static str {
    match kind {
        AllowRuleKind::BashPrefix => "bash_prefix",
        AllowRuleKind::BashRegex => "bash_regex",
        AllowRuleKind::PathWrite => "path_write",
        AllowRuleKind::ToolName => "tool_name",
    }
}

fn parse_allow_rule_kind(value: String) -> AllowRuleKind {
    match value.as_str() {
        "bash_prefix" => AllowRuleKind::BashPrefix,
        "bash_regex" => AllowRuleKind::BashRegex,
        "path_write" => AllowRuleKind::PathWrite,
        "tool_name" => AllowRuleKind::ToolName,
        other => panic!("invalid persisted allow rule kind {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{AllowRuleKind, StopReason, ToolCallId};
    use serde_json::json;

    #[test]
    fn app_db_migrates_and_stores_workspace_and_session() {
        let db = AppDb::in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), 3);

        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        db.upsert_workspace(workspace_id, "/tmp/project", "project")
            .unwrap();
        let record = new_session_record(
            session_id,
            workspace_id,
            ProviderId("codex".to_string()),
            "gpt-5.5",
        )
        .unwrap();
        db.upsert_session(&record).unwrap();

        let workspaces = db.list_workspaces().unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].id, workspace_id);

        let sessions = db.list_sessions(workspace_id).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session_id);
    }

    #[test]
    fn set_session_status_updates_only_the_status_column() {
        let db = AppDb::in_memory().unwrap();
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        db.upsert_workspace(workspace_id, "/tmp/project", "project")
            .unwrap();
        let record = new_session_record(
            session_id,
            workspace_id,
            ProviderId("claude".to_string()),
            "sonnet",
        )
        .unwrap();
        db.upsert_session(&record).unwrap();
        assert_eq!(
            db.get_session(session_id).unwrap().unwrap().status,
            SessionStatus::Starting
        );

        // A live transition mid-run must move the persisted status off `Starting`
        // so the dashboard never shows a working session as hung.
        db.set_session_status(session_id, SessionStatus::RunningTools)
            .unwrap();
        let loaded = db.get_session(session_id).unwrap().unwrap();
        assert_eq!(loaded.status, SessionStatus::RunningTools);
        assert_eq!(loaded.model, "sonnet");
        assert_eq!(loaded.display_name, record.display_name);
    }

    #[test]
    fn app_db_lists_only_incomplete_sessions() {
        let db = AppDb::in_memory().unwrap();
        let workspace_id = WorkspaceId::new();
        db.upsert_workspace(workspace_id, "/tmp/project", "project")
            .unwrap();

        let starting_id = SessionId::new();
        let mut starting = new_session_record(
            starting_id,
            workspace_id,
            ProviderId("codex".to_string()),
            "gpt-5.5",
        )
        .unwrap();
        starting.status = SessionStatus::Starting;
        db.upsert_session(&starting).unwrap();

        let completed_id = SessionId::new();
        let mut completed = new_session_record(
            completed_id,
            workspace_id,
            ProviderId("claude".to_string()),
            "sonnet",
        )
        .unwrap();
        completed.status = SessionStatus::Completed;
        db.upsert_session(&completed).unwrap();

        let incomplete = db.list_incomplete_sessions().unwrap();
        assert_eq!(incomplete.len(), 1);
        assert_eq!(incomplete[0].id, starting_id);
    }

    #[test]
    fn app_db_tracks_managed_worktrees() {
        let db = AppDb::in_memory().unwrap();
        let workspace_id = WorkspaceId::new();
        db.upsert_workspace(workspace_id, "/tmp/wt", "wt").unwrap();

        let now = now_rfc3339().unwrap();
        let record = WorktreeRecord {
            id: workspace_id,
            source_repo: PathBuf::from("/tmp/repo"),
            worktree_path: PathBuf::from("/tmp/wt"),
            branch_name: "fix-login-abcd1234".to_string(),
            base_branch: "main".to_string(),
            base_commit: "deadbeef".to_string(),
            status: WorktreeStatus::Active,
            created_at: now.clone(),
            updated_at: now,
        };
        db.upsert_worktree(&record).unwrap();

        let loaded = db.get_worktree(workspace_id).unwrap().unwrap();
        assert_eq!(loaded.branch_name, record.branch_name);
        assert_eq!(loaded.status, WorktreeStatus::Active);
        assert_eq!(db.list_worktrees().unwrap().len(), 1);

        db.set_worktree_status(workspace_id, WorktreeStatus::Merged)
            .unwrap();
        assert_eq!(
            db.get_worktree(workspace_id).unwrap().unwrap().status,
            WorktreeStatus::Merged
        );
    }

    #[test]
    fn app_db_stores_settings_provider_config_and_credential_source() {
        let db = AppDb::in_memory().unwrap();
        let provider = ProviderId("claude".to_string());

        db.put_setting("theme", &json!({"mode": "dark"})).unwrap();
        assert_eq!(
            db.get_setting("theme").unwrap(),
            Some(json!({"mode": "dark"}))
        );

        db.put_provider_config(&provider, &json!({"model": "sonnet"}))
            .unwrap();
        assert_eq!(
            db.provider_config(&provider).unwrap(),
            Some(json!({"model": "sonnet"}))
        );

        db.put_credential_source(&provider, "claude_agent_sdk", Some("signed-in"))
            .unwrap();
        let source = db.credential_source(&provider).unwrap().unwrap();
        assert_eq!(source.source, "claude_agent_sdk");
        assert_eq!(source.identity_hint.as_deref(), Some("signed-in"));
    }

    #[test]
    fn allow_rules_deduplicate() {
        let db = AppDb::in_memory().unwrap();
        let workspace_id = WorkspaceId::new();
        let rule = AllowRule::new(AllowRuleKind::ToolName, "read_file");

        db.add_allow_rule(Some(workspace_id), None, &rule).unwrap();
        db.add_allow_rule(Some(workspace_id), None, &rule).unwrap();

        let rules = db.allow_rules(Some(workspace_id), None).unwrap();
        assert_eq!(rules, vec![rule]);
    }

    #[test]
    fn workspace_db_persists_messages_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();

        {
            let db = WorkspaceDb::open(&path).unwrap();
            let session = new_session_record(
                session_id,
                workspace_id,
                ProviderId("codex".to_string()),
                "gpt-5.5",
            )
            .unwrap();
            db.upsert_session(&session).unwrap();
            db.replace_messages(
                session_id,
                &[
                    StoredMessage::new("User", "hello", 0),
                    StoredMessage::new("Assistant", "hi", 1),
                ],
            )
            .unwrap();
        }

        let reopened = WorkspaceDb::open(&path).unwrap();
        let messages = reopened.messages(session_id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hello");
        assert_eq!(messages[1].role, "Assistant");
    }

    #[test]
    fn workspace_db_stores_events_and_tool_records() {
        let db = WorkspaceDb::in_memory().unwrap();
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let tool_call_id = ToolCallId::new().to_string();
        let session = new_session_record(
            session_id,
            workspace_id,
            ProviderId("codex".to_string()),
            "gpt-5.5",
        )
        .unwrap();
        db.upsert_session(&session).unwrap();

        let event = SessionEvent::Result {
            session_id,
            stop_reason: StopReason::EndTurn,
        };
        db.append_event(session_id, &event).unwrap();
        assert_eq!(db.events(session_id).unwrap(), vec![event]);

        db.upsert_tool_call(&ToolCallRecord {
            id: tool_call_id.clone(),
            session_id,
            name: "read_file".to_string(),
            input_json: json!({"path": "Cargo.toml"}),
            status: "started".to_string(),
        })
        .unwrap();
        db.add_tool_result(&ToolResultRecord {
            tool_call_id: tool_call_id.clone(),
            output: "ok".to_string(),
            exit_code: Some(0),
            blob_id: None,
        })
        .unwrap();
        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM tool_calls WHERE id = ?1",
                [tool_call_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "completed");
    }

    #[test]
    fn workspace_db_marks_tool_errors_failed() {
        let db = WorkspaceDb::in_memory().unwrap();
        let workspace_id = WorkspaceId::new();
        let session_id = SessionId::new();
        let tool_call_id = ToolCallId::new().to_string();
        let session = new_session_record(
            session_id,
            workspace_id,
            ProviderId("codex".to_string()),
            "gpt-5.5",
        )
        .unwrap();
        db.upsert_session(&session).unwrap();

        db.upsert_tool_call(&ToolCallRecord {
            id: tool_call_id.clone(),
            session_id,
            name: "edit_file".to_string(),
            input_json: json!({"path": "src/lib.rs"}),
            status: "started".to_string(),
        })
        .unwrap();
        db.mark_tool_failed(&tool_call_id).unwrap();

        let status: String = db
            .conn
            .query_row(
                "SELECT status FROM tool_calls WHERE id = ?1",
                [tool_call_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "failed");
    }

    #[test]
    fn workspace_db_stores_blobs_and_allow_rules() {
        let db = WorkspaceDb::in_memory().unwrap();
        let session_id = SessionId::new();
        let blob = BlobRecord {
            id: "abc".to_string(),
            path: PathBuf::from("/tmp/blob"),
            bytes: 12,
            created_at: now_rfc3339().unwrap(),
        };
        let rule = AllowRule::new(AllowRuleKind::BashPrefix, "cargo");

        db.put_blob(&blob).unwrap();
        assert_eq!(db.blob("abc").unwrap(), Some(blob));

        db.add_allow_rule(Some(session_id), &rule).unwrap();
        db.add_allow_rule(Some(session_id), &rule).unwrap();
        assert_eq!(db.allow_rules(Some(session_id)).unwrap(), vec![rule]);
    }

    #[test]
    fn default_workspace_path_uses_inductor_dir() {
        assert_eq!(
            workspace_state_path("/tmp/project"),
            PathBuf::from("/tmp/project/.inductor/state.db")
        );
    }

    #[test]
    fn existing_file_gets_backup_before_migration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inductor.db");
        fs::write(&path, []).unwrap();

        let db = AppDb::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), 3);

        let backups = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("inductor.db.bak."))
                    .unwrap_or(false)
            })
            .collect::<Vec<_>>();
        assert_eq!(backups.len(), 1);
    }
}
