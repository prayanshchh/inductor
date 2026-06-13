use std::{
    error::Error,
    ffi::OsStr,
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

use harness_core::WorkspaceId;

#[derive(Debug, Clone)]
pub struct ManagedWorktree {
    pub workspace_id: WorkspaceId,
    pub source_repo: PathBuf,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub base_branch: String,
    pub base_commit: String,
}

#[derive(Debug, Clone)]
pub struct RepoInfo {
    pub root: PathBuf,
    pub current_branch: String,
    pub head_commit: String,
    pub is_dirty: bool,
}

#[derive(Debug, Clone)]
pub struct CreateWorktreeRequest {
    pub source_repo: PathBuf,
    pub slug: String,
    pub allow_dirty: bool,
}

#[derive(Debug, Clone)]
pub struct GitWorktree {
    pub path: PathBuf,
    pub head: String,
    pub branch: Option<String>,
}

/// Describes the drift between the commit a worktree branch was based on and
/// the current tip of its target branch. Used to decide whether a merge is a
/// trivial fast-forward, needs a real merge, or risks conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftStatus {
    /// Commit the worktree branch was originally created from.
    pub base_commit: String,
    /// Current tip of the target branch.
    pub target_head: String,
    /// True when the target branch has advanced past `base_commit`.
    pub drifted: bool,
}

/// Request to merge a worktree branch into a target branch in the source repo.
#[derive(Debug, Clone)]
pub struct MergeRequest {
    pub source_repo: PathBuf,
    pub branch_name: String,
    pub target_branch: String,
    /// Commit the branch was based on, used for drift reporting.
    pub base_commit: String,
    /// Force a merge commit even when a fast-forward is possible.
    pub no_ff: bool,
}

/// Result of attempting a local merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Target already contains the branch; nothing to do.
    UpToDate,
    /// Merge succeeded, producing (or fast-forwarding to) `merged_commit`.
    Merged {
        merged_commit: String,
        fast_forward: bool,
    },
    /// Merge left the target checkout with conflicts that must be resolved or
    /// aborted before the merge lock is released.
    Conflict { files: Vec<PathBuf> },
}

#[derive(Debug, Clone)]
pub struct WorktreeManager {
    managed_root: PathBuf,
}

impl WorktreeManager {
    pub fn new(managed_root: PathBuf) -> Self {
        Self { managed_root }
    }

    pub fn inspect_repo(&self, source_repo: &Path) -> Result<RepoInfo, GitError> {
        let root = git_stdout(source_repo, ["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(root);

        let current_branch = git_stdout(&root, ["symbolic-ref", "--short", "HEAD"])
            .map_err(|_| GitError::DetachedHead(root.clone()))?;
        let head_commit = git_stdout(&root, ["rev-parse", "HEAD"])?;
        let status = git_stdout(&root, ["status", "--porcelain"])?;

        Ok(RepoInfo {
            root,
            current_branch,
            head_commit,
            is_dirty: !status.trim().is_empty(),
        })
    }

    pub fn create_worktree(
        &self,
        request: CreateWorktreeRequest,
    ) -> Result<ManagedWorktree, GitError> {
        let repo = self.inspect_repo(&request.source_repo)?;

        if repo.is_dirty && !request.allow_dirty {
            return Err(GitError::DirtyRepository(repo.root));
        }

        fs::create_dir_all(&self.managed_root).map_err(|source| GitError::Io {
            path: self.managed_root.clone(),
            source: source.to_string(),
        })?;

        let workspace_id = WorkspaceId::new();
        let worktree_path = self.managed_root.join(workspace_id.to_string());

        if worktree_path.exists() {
            return Err(GitError::NonEmptyTarget(worktree_path));
        }

        let branch_name = format!(
            "inductor/{}-{}",
            sanitize_slug(&request.slug),
            short_workspace_id(workspace_id)
        );

        git_stdout(
            &repo.root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("-b"),
                OsStr::new(&branch_name),
                worktree_path.as_os_str(),
                OsStr::new(&repo.current_branch),
            ],
        )?;

        Ok(ManagedWorktree {
            workspace_id,
            source_repo: repo.root,
            worktree_path,
            branch_name,
            base_branch: repo.current_branch,
            base_commit: repo.head_commit,
        })
    }

    pub fn list_worktrees(&self, source_repo: &Path) -> Result<Vec<GitWorktree>, GitError> {
        let repo = self.inspect_repo(source_repo)?;
        let output = git_stdout(&repo.root, ["worktree", "list", "--porcelain"])?;

        parse_worktree_list(&output)
    }

    pub fn remove_worktree(
        &self,
        source_repo: &Path,
        worktree_path: &Path,
        force: bool,
    ) -> Result<(), GitError> {
        let repo = self.inspect_repo(source_repo)?;

        let mut args = vec![OsStr::new("worktree"), OsStr::new("remove")];
        if force {
            args.push(OsStr::new("--force"));
        }
        args.push(worktree_path.as_os_str());

        git_stdout(&repo.root, args)?;
        Ok(())
    }

    /// Resolve the current tip commit of `target_branch` in the source repo.
    pub fn target_head(&self, source_repo: &Path, target_branch: &str) -> Result<String, GitError> {
        let repo = self.inspect_repo(source_repo)?;
        git_stdout(&repo.root, ["rev-parse", target_branch])
    }

    /// Compare the commit a branch was based on against the current target tip.
    ///
    /// Callers use this to detect that the target branch moved while an agent
    /// worked (the serialized-merge case): a drifted target may merge cleanly,
    /// fast-forward, or conflict, so the result is re-checked at merge time.
    pub fn check_drift(
        &self,
        source_repo: &Path,
        target_branch: &str,
        base_commit: &str,
    ) -> Result<DriftStatus, GitError> {
        let target_head = self.target_head(source_repo, target_branch)?;
        Ok(DriftStatus {
            drifted: target_head != base_commit,
            base_commit: base_commit.to_string(),
            target_head,
        })
    }

    /// Merge a worktree branch into its target branch in the source repo's
    /// checkout.
    ///
    /// The merge runs in the source repo root, which must be clean. If the
    /// root is not already on `target_branch` it is switched onto it first
    /// (this fails cleanly if the branch is checked out in another worktree).
    /// Conflicts are surfaced as [`MergeOutcome::Conflict`] without
    /// auto-resolving; the caller decides whether to resolve manually or call
    /// [`WorktreeManager::abort_merge`].
    pub fn merge_branch(&self, request: MergeRequest) -> Result<MergeOutcome, GitError> {
        let repo = self.inspect_repo(&request.source_repo)?;

        if repo.is_dirty {
            return Err(GitError::DirtyRepository(repo.root));
        }

        if repo.current_branch != request.target_branch {
            git_stdout(&repo.root, ["switch", &request.target_branch])?;
        }

        let mut args = vec!["merge".to_string()];
        if request.no_ff {
            args.push("--no-ff".to_string());
        }
        args.push(request.branch_name.clone());

        let result = git_run(&repo.root, &args)?;

        if result.success {
            if result.stdout.contains("Already up to date") {
                return Ok(MergeOutcome::UpToDate);
            }
            let fast_forward = result.stdout.contains("Fast-forward");
            let merged_commit = git_stdout(&repo.root, ["rev-parse", "HEAD"])?;
            return Ok(MergeOutcome::Merged {
                merged_commit,
                fast_forward,
            });
        }

        let conflicts = git_stdout(&repo.root, ["diff", "--name-only", "--diff-filter=U"])?;
        if !conflicts.trim().is_empty() {
            let files = conflicts
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(PathBuf::from)
                .collect();
            return Ok(MergeOutcome::Conflict { files });
        }

        Err(GitError::CommandFailed {
            program: "git".to_string(),
            args,
            status: result.status,
            stderr: result.stderr,
        })
    }

    /// Abort an in-progress merge in the source repo, restoring the target
    /// branch to its pre-merge state.
    pub fn abort_merge(&self, source_repo: &Path) -> Result<(), GitError> {
        let repo = self.inspect_repo(source_repo)?;
        git_stdout(&repo.root, ["merge", "--abort"])?;
        Ok(())
    }
}

/// Captured output of a git invocation that may legitimately fail (e.g. a
/// merge that hits conflicts), where the caller inspects the result rather
/// than treating non-zero as an error.
struct GitRun {
    success: bool,
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

fn git_run(repo: &Path, args: &[String]) -> Result<GitRun, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|source| GitError::CommandFailed {
            program: "git".to_string(),
            args: args.to_vec(),
            status: None,
            stderr: source.to_string(),
        })?;

    Ok(GitRun {
        success: output.status.success(),
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

#[derive(Debug, Clone)]
pub enum GitError {
    DetachedHead(PathBuf),
    DirtyRepository(PathBuf),
    NonEmptyTarget(PathBuf),
    CommandFailed {
        program: String,
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    Io {
        path: PathBuf,
        source: String,
    },
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DetachedHead(path) => {
                write!(
                    f,
                    "repository is in detached HEAD state: {}",
                    path.display()
                )
            }
            Self::DirtyRepository(path) => {
                write!(f, "repository has uncommitted changes: {}", path.display())
            }
            Self::NonEmptyTarget(path) => {
                write!(f, "worktree target path already exists: {}", path.display())
            }
            Self::CommandFailed {
                program,
                args,
                status,
                stderr,
            } => write!(
                f,
                "command failed: {} {} status={:?}: {}",
                program,
                args.join(" "),
                status,
                stderr.trim()
            ),
            Self::Io { path, source } => {
                write!(f, "io error at {}: {}", path.display(), source)
            }
        }
    }
}

impl Error for GitError {}

fn git_stdout<I, S>(repo: &Path, args: I) -> Result<String, GitError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<_> = args.into_iter().collect();

    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .output()
        .map_err(|source| GitError::CommandFailed {
            program: "git".to_string(),
            args: args_to_strings(&args),
            status: None,
            stderr: source.to_string(),
        })?;

    if !output.status.success() {
        return Err(GitError::CommandFailed {
            program: "git".to_string(),
            args: args_to_strings(&args),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn args_to_strings<S>(args: &[S]) -> Vec<String>
where
    S: AsRef<OsStr>,
{
    args.iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect()
}

fn sanitize_slug(slug: &str) -> String {
    let sanitized: String = slug
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    let slug = sanitized
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    if slug.is_empty() {
        "workspace".to_string()
    } else {
        slug
    }
}

fn short_workspace_id(workspace_id: WorkspaceId) -> String {
    workspace_id.to_string()[..8].to_ascii_lowercase()
}

fn parse_worktree_list(output: &str) -> Result<Vec<GitWorktree>, GitError> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_head: Option<String> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let (Some(path), Some(head)) = (current_path.take(), current_head.take()) {
                worktrees.push(GitWorktree {
                    path,
                    head,
                    branch: current_branch.take(),
                });
            }
            continue;
        }

        if let Some(path) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path));
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            current_head = Some(head.to_string());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            current_branch = branch.strip_prefix("refs/heads/").map(str::to_string);
        }
    }

    Ok(worktrees)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn inspect_repo_reads_branch_head_and_dirty_state() {
        let temp = TempDir::new("inspect-repo");
        let repo = temp.path().join("repo");
        init_repo(&repo);

        let manager = WorktreeManager::new(temp.path().join("managed"));
        let info = manager.inspect_repo(&repo).unwrap();

        assert_eq!(info.root, fs::canonicalize(&repo).unwrap());
        assert_eq!(info.current_branch, current_test_branch());
        assert_eq!(info.head_commit.len(), 40);
        assert!(!info.is_dirty);

        fs::write(repo.join("dirty.txt"), "dirty\n").unwrap();

        let info = manager.inspect_repo(&repo).unwrap();
        assert!(info.is_dirty);
    }

    #[test]
    fn create_worktree_creates_branch_and_managed_path() {
        let temp = TempDir::new("create-worktree");
        let repo = temp.path().join("repo");
        let managed = temp.path().join("managed");
        init_repo(&repo);

        let manager = WorktreeManager::new(managed.clone());
        let worktree = manager
            .create_worktree(CreateWorktreeRequest {
                source_repo: repo.clone(),
                slug: "Fix Login!".to_string(),
                allow_dirty: false,
            })
            .unwrap();

        assert_eq!(worktree.source_repo, fs::canonicalize(&repo).unwrap());
        assert!(worktree.worktree_path.starts_with(&managed));
        assert!(worktree.worktree_path.exists());
        assert!(worktree.branch_name.starts_with("inductor/fix-login-"));

        let list = manager.list_worktrees(&worktree.source_repo).unwrap();
        let created_path = fs::canonicalize(&worktree.worktree_path).unwrap();
        assert!(
            list.iter()
                .any(|item| fs::canonicalize(&item.path).unwrap() == created_path)
        );

        manager
            .remove_worktree(&worktree.source_repo, &worktree.worktree_path, true)
            .unwrap();

        assert!(!worktree.worktree_path.exists());
    }

    #[test]
    fn create_worktree_rejects_dirty_repo_by_default() {
        let temp = TempDir::new("reject-dirty");
        let repo = temp.path().join("repo");
        init_repo(&repo);
        fs::write(repo.join("dirty.txt"), "dirty\n").unwrap();

        let manager = WorktreeManager::new(temp.path().join("managed"));
        let err = manager
            .create_worktree(CreateWorktreeRequest {
                source_repo: repo,
                slug: "dirty".to_string(),
                allow_dirty: false,
            })
            .unwrap_err();

        assert!(matches!(err, GitError::DirtyRepository(_)));
    }

    #[test]
    fn merge_branch_fast_forwards_clean_branch() {
        let temp = TempDir::new("merge-ff");
        let repo = temp.path().join("repo");
        init_repo(&repo);

        let manager = WorktreeManager::new(temp.path().join("managed"));
        let worktree = manager
            .create_worktree(CreateWorktreeRequest {
                source_repo: repo.clone(),
                slug: "feature".to_string(),
                allow_dirty: false,
            })
            .unwrap();

        fs::write(worktree.worktree_path.join("feature.txt"), "feature\n").unwrap();
        run(&worktree.worktree_path, ["add", "feature.txt"]);
        run(&worktree.worktree_path, ["commit", "-m", "add feature"]);

        let outcome = manager
            .merge_branch(MergeRequest {
                source_repo: worktree.source_repo.clone(),
                branch_name: worktree.branch_name.clone(),
                target_branch: worktree.base_branch.clone(),
                base_commit: worktree.base_commit.clone(),
                no_ff: false,
            })
            .unwrap();

        assert!(matches!(
            outcome,
            MergeOutcome::Merged {
                fast_forward: true,
                ..
            }
        ));
        assert!(repo.join("feature.txt").exists());
    }

    #[test]
    fn merge_branch_reports_conflict_without_resolving() {
        let temp = TempDir::new("merge-conflict");
        let repo = temp.path().join("repo");
        init_repo(&repo);

        let manager = WorktreeManager::new(temp.path().join("managed"));
        let worktree = manager
            .create_worktree(CreateWorktreeRequest {
                source_repo: repo.clone(),
                slug: "feature".to_string(),
                allow_dirty: false,
            })
            .unwrap();

        // Diverge both sides on the same file to force a conflict.
        fs::write(worktree.worktree_path.join("README.md"), "# branch\n").unwrap();
        run(&worktree.worktree_path, ["add", "README.md"]);
        run(&worktree.worktree_path, ["commit", "-m", "branch edit"]);

        fs::write(repo.join("README.md"), "# main\n").unwrap();
        run(&repo, ["add", "README.md"]);
        run(&repo, ["commit", "-m", "main edit"]);

        let outcome = manager
            .merge_branch(MergeRequest {
                source_repo: worktree.source_repo.clone(),
                branch_name: worktree.branch_name.clone(),
                target_branch: worktree.base_branch.clone(),
                base_commit: worktree.base_commit.clone(),
                no_ff: false,
            })
            .unwrap();

        match outcome {
            MergeOutcome::Conflict { files } => {
                assert_eq!(files, vec![PathBuf::from("README.md")]);
            }
            other => panic!("expected conflict, got {other:?}"),
        }

        // The merge must still be abortable, leaving a clean tree.
        manager.abort_merge(&worktree.source_repo).unwrap();
        assert!(!manager.inspect_repo(&repo).unwrap().is_dirty);
    }

    #[test]
    fn check_drift_detects_advanced_target() {
        let temp = TempDir::new("drift");
        let repo = temp.path().join("repo");
        init_repo(&repo);

        let manager = WorktreeManager::new(temp.path().join("managed"));
        let worktree = manager
            .create_worktree(CreateWorktreeRequest {
                source_repo: repo.clone(),
                slug: "feature".to_string(),
                allow_dirty: false,
            })
            .unwrap();

        let before = manager
            .check_drift(
                &worktree.source_repo,
                &worktree.base_branch,
                &worktree.base_commit,
            )
            .unwrap();
        assert!(!before.drifted);

        fs::write(repo.join("more.txt"), "more\n").unwrap();
        run(&repo, ["add", "more.txt"]);
        run(&repo, ["commit", "-m", "advance target"]);

        let after = manager
            .check_drift(
                &worktree.source_repo,
                &worktree.base_branch,
                &worktree.base_commit,
            )
            .unwrap();
        assert!(after.drifted);
        assert_ne!(after.target_head, after.base_commit);
    }

    #[test]
    fn sanitize_slug_removes_unsafe_branch_chars() {
        assert_eq!(sanitize_slug("Fix Login!"), "fix-login");
        assert_eq!(sanitize_slug("  A/B C  "), "a-b-c");
        assert_eq!(sanitize_slug("!!!"), "workspace");
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
            let path = std::env::temp_dir().join(format!("inductor-{label}-{nanos}"));
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

    fn init_repo(repo: &Path) {
        fs::create_dir_all(repo).unwrap();

        run(repo, ["init"]);
        run(repo, ["config", "user.email", "test@example.com"]);
        run(repo, ["config", "user.name", "Test User"]);

        fs::write(repo.join("README.md"), "# test\n").unwrap();
        run(repo, ["add", "README.md"]);
        run(repo, ["commit", "-m", "initial"]);
    }

    fn run<I, S>(repo: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn current_test_branch() -> String {
        let output = Command::new("git")
            .args(["config", "init.defaultBranch"])
            .output()
            .unwrap();

        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() {
            "master".to_string()
        } else {
            branch
        }
    }
}
