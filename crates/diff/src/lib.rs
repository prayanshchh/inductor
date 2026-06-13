use std::{
    ffi::OsStr,
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRequest {
    pub repo: PathBuf,
    pub base: String,
    pub context_lines: u16,
    pub include_untracked: bool,
}

impl DiffRequest {
    pub fn new(repo: impl Into<PathBuf>, base: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            base: base.into(),
            context_lines: 3,
            include_untracked: true,
        }
    }

    pub fn tracked_only(repo: impl Into<PathBuf>, base: impl Into<String>) -> Self {
        Self {
            include_untracked: false,
            ..Self::new(repo, base)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub repo_root: PathBuf,
    pub base: String,
    pub files: Vec<FileDiff>,
}

impl DiffSummary {
    pub fn changed_files(&self) -> usize {
        self.files.len()
    }

    pub fn added_lines(&self) -> usize {
        self.files.iter().map(FileDiff::added_lines).sum()
    }

    pub fn removed_lines(&self) -> usize {
        self.files.iter().map(FileDiff::removed_lines).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub status: FileStatus,
    pub hunks: Vec<DiffHunk>,
}

impl FileDiff {
    pub fn display_path(&self) -> &Path {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_else(|| Path::new(""))
    }

    pub fn added_lines(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == DiffLineKind::Add)
            .count()
    }

    pub fn removed_lines(&self) -> usize {
        self.hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .filter(|line| line.kind == DiffLineKind::Remove)
            .count()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Add,
    Remove,
}

pub fn diff_worktree(request: &DiffRequest) -> Result<DiffSummary, DiffError> {
    let repo_root = git_stdout(&request.repo, ["rev-parse", "--show-toplevel"])?;
    let repo_root = PathBuf::from(repo_root.trim());
    let context = format!("--unified={}", request.context_lines);
    let args = vec![
        OsStr::new("diff"),
        OsStr::new("--no-ext-diff"),
        OsStr::new("--find-renames"),
        OsStr::new("--no-color"),
        OsStr::new("--src-prefix=a/"),
        OsStr::new("--dst-prefix=b/"),
        OsStr::new(&context),
        OsStr::new(&request.base),
        OsStr::new("--"),
    ];
    let output = git_stdout(&repo_root, args)?;
    let mut files = parse_unified_diff(&output)?;
    if request.include_untracked {
        files.extend(untracked_file_diffs(&repo_root)?);
    }

    Ok(DiffSummary {
        repo_root,
        base: request.base.clone(),
        files,
    })
}

pub fn parse_unified_diff(input: &str) -> Result<Vec<FileDiff>, DiffError> {
    let lines = input.lines().collect::<Vec<_>>();
    let mut index = 0usize;
    let mut files = Vec::new();

    while index < lines.len() {
        if !lines[index].starts_with("diff --git ") {
            index += 1;
            continue;
        }

        let (mut old_path, mut new_path) = parse_diff_git_paths(lines[index])?;
        let mut status = FileStatus::Modified;
        index += 1;

        while index < lines.len()
            && !lines[index].starts_with("--- ")
            && !lines[index].starts_with("diff --git ")
        {
            let line = lines[index];
            if line.starts_with("new file mode ") {
                status = FileStatus::Added;
                old_path = None;
            } else if line.starts_with("deleted file mode ") {
                status = FileStatus::Deleted;
                new_path = None;
            } else if let Some(path) = line.strip_prefix("rename from ") {
                status = FileStatus::Renamed;
                old_path = Some(PathBuf::from(path));
            } else if let Some(path) = line.strip_prefix("rename to ") {
                status = FileStatus::Renamed;
                new_path = Some(PathBuf::from(path));
            } else if let Some(path) = line.strip_prefix("copy from ") {
                status = FileStatus::Copied;
                old_path = Some(PathBuf::from(path));
            } else if let Some(path) = line.strip_prefix("copy to ") {
                status = FileStatus::Copied;
                new_path = Some(PathBuf::from(path));
            }
            index += 1;
        }

        if index < lines.len() && lines[index].starts_with("--- ") {
            old_path = parse_file_header_path(lines[index], old_path, true)?;
            index += 1;
        }
        if index < lines.len() && lines[index].starts_with("+++ ") {
            new_path = parse_file_header_path(lines[index], new_path, false)?;
            index += 1;
        }

        let mut hunks = Vec::new();
        while index < lines.len() && !lines[index].starts_with("diff --git ") {
            if !lines[index].starts_with("@@ ") {
                index += 1;
                continue;
            }

            let (hunk, next_index) = parse_hunk(&lines, index)?;
            hunks.push(hunk);
            index = next_index;
        }

        files.push(FileDiff {
            old_path,
            new_path,
            status,
            hunks,
        });
    }

    Ok(files)
}

fn parse_diff_git_paths(line: &str) -> Result<(Option<PathBuf>, Option<PathBuf>), DiffError> {
    let rest = line
        .strip_prefix("diff --git ")
        .ok_or_else(|| DiffError::InvalidDiff(format!("invalid diff header: {line}")))?;
    let mut parts = rest.split_whitespace();
    let old = parts
        .next()
        .ok_or_else(|| DiffError::InvalidDiff(format!("missing old path in {line}")))?;
    let new = parts
        .next()
        .ok_or_else(|| DiffError::InvalidDiff(format!("missing new path in {line}")))?;

    Ok((strip_ab_prefix(old, "a/"), strip_ab_prefix(new, "b/")))
}

fn parse_file_header_path(
    line: &str,
    fallback: Option<PathBuf>,
    old: bool,
) -> Result<Option<PathBuf>, DiffError> {
    let prefix = if old { "--- " } else { "+++ " };
    let raw = line
        .strip_prefix(prefix)
        .ok_or_else(|| DiffError::InvalidDiff(format!("invalid file header: {line}")))?;
    let raw = raw.split_whitespace().next().unwrap_or(raw);
    if raw == "/dev/null" {
        return Ok(None);
    }
    let prefix = if old { "a/" } else { "b/" };
    Ok(strip_ab_prefix(raw, prefix).or(fallback))
}

fn strip_ab_prefix(path: &str, prefix: &str) -> Option<PathBuf> {
    Some(PathBuf::from(path.strip_prefix(prefix).unwrap_or(path)))
}

fn parse_hunk(lines: &[&str], start: usize) -> Result<(DiffHunk, usize), DiffError> {
    let header = lines[start].to_string();
    let (old_start, old_lines, new_start, new_lines) = parse_hunk_header(&header)?;
    let mut index = start + 1;
    let mut parsed = Vec::new();
    let mut old_line = old_start;
    let mut new_line = new_start;

    while index < lines.len()
        && !lines[index].starts_with("@@ ")
        && !lines[index].starts_with("diff --git ")
    {
        let raw = lines[index];
        if raw == r"\ No newline at end of file" {
            index += 1;
            continue;
        }

        let Some(marker) = raw.as_bytes().first().copied() else {
            return Err(DiffError::InvalidDiff("empty hunk line".to_string()));
        };
        let content = raw.get(1..).unwrap_or_default().to_string();
        match marker {
            b' ' => {
                parsed.push(DiffLine {
                    kind: DiffLineKind::Context,
                    old_line: Some(old_line),
                    new_line: Some(new_line),
                    content,
                });
                old_line += 1;
                new_line += 1;
            }
            b'-' => {
                parsed.push(DiffLine {
                    kind: DiffLineKind::Remove,
                    old_line: Some(old_line),
                    new_line: None,
                    content,
                });
                old_line += 1;
            }
            b'+' => {
                parsed.push(DiffLine {
                    kind: DiffLineKind::Add,
                    old_line: None,
                    new_line: Some(new_line),
                    content,
                });
                new_line += 1;
            }
            _ => {
                return Err(DiffError::InvalidDiff(format!(
                    "invalid hunk line marker in {raw:?}"
                )));
            }
        }
        index += 1;
    }

    Ok((
        DiffHunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            header,
            lines: parsed,
        },
        index,
    ))
}

fn parse_hunk_header(header: &str) -> Result<(u32, u32, u32, u32), DiffError> {
    let mut parts = header.split_whitespace();
    let Some("@@") = parts.next() else {
        return Err(DiffError::InvalidDiff(format!(
            "invalid hunk header: {header}"
        )));
    };
    let old = parts
        .next()
        .ok_or_else(|| DiffError::InvalidDiff(format!("missing old range in {header}")))?;
    let new = parts
        .next()
        .ok_or_else(|| DiffError::InvalidDiff(format!("missing new range in {header}")))?;
    let old = parse_range(old, '-')?;
    let new = parse_range(new, '+')?;
    Ok((old.0, old.1, new.0, new.1))
}

fn parse_range(range: &str, prefix: char) -> Result<(u32, u32), DiffError> {
    let range = range
        .strip_prefix(prefix)
        .ok_or_else(|| DiffError::InvalidDiff(format!("invalid range {range}")))?;
    let mut parts = range.split(',');
    let start = parts
        .next()
        .unwrap_or("0")
        .parse::<u32>()
        .map_err(|_| DiffError::InvalidDiff(format!("invalid range start {range}")))?;
    let len = parts
        .next()
        .unwrap_or("1")
        .parse::<u32>()
        .map_err(|_| DiffError::InvalidDiff(format!("invalid range length {range}")))?;
    Ok((start, len))
}

fn git_stdout<I, S>(repo: &Path, args: I) -> Result<String, DiffError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args.into_iter().collect::<Vec<_>>();
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(&args)
        .output()
        .map_err(|source| DiffError::CommandFailed {
            args: args_to_strings(&args),
            status: None,
            stderr: source.to_string(),
        })?;

    if !output.status.success() {
        return Err(DiffError::CommandFailed {
            args: args_to_strings(&args),
            status: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn untracked_file_diffs(repo_root: &Path) -> Result<Vec<FileDiff>, DiffError> {
    let output = git_stdout(repo_root, ["ls-files", "--others", "--exclude-standard"])?;
    let mut files = Vec::new();

    for line in output.lines().filter(|line| !line.trim().is_empty()) {
        let path = PathBuf::from(line);
        let full_path = repo_root.join(&path);
        if !full_path.is_file() {
            continue;
        }

        let bytes = fs::read(&full_path).map_err(|source| DiffError::Io {
            path: full_path.clone(),
            source,
        })?;
        if bytes.iter().any(|byte| *byte == 0) {
            files.push(FileDiff {
                old_path: None,
                new_path: Some(path),
                status: FileStatus::Added,
                hunks: Vec::new(),
            });
            continue;
        }

        let content = String::from_utf8_lossy(&bytes);
        let lines = content
            .lines()
            .enumerate()
            .map(|(index, line)| DiffLine {
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(index as u32 + 1),
                content: line.to_string(),
            })
            .collect::<Vec<_>>();
        let new_lines = lines.len() as u32;
        files.push(FileDiff {
            old_path: None,
            new_path: Some(path),
            status: FileStatus::Added,
            hunks: vec![DiffHunk {
                old_start: 0,
                old_lines: 0,
                new_start: 1,
                new_lines,
                header: format!("@@ -0,0 +1,{new_lines} @@"),
                lines,
            }],
        });
    }

    Ok(files)
}

fn args_to_strings<S>(args: &[S]) -> Vec<String>
where
    S: AsRef<OsStr>,
{
    args.iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect()
}

#[derive(Debug)]
pub enum DiffError {
    CommandFailed {
        args: Vec<String>,
        status: Option<i32>,
        stderr: String,
    },
    InvalidDiff(String),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandFailed {
                args,
                status,
                stderr,
            } => write!(
                f,
                "git {} failed status={:?}: {}",
                args.join(" "),
                status,
                stderr.trim()
            ),
            Self::InvalidDiff(message) => write!(f, "invalid diff: {message}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for DiffError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn parses_modified_file_hunk_with_line_numbers() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 fn main() {
-    println!(\"old\");
+    println!(\"new\");
 }
";

        let files = parse_unified_diff(diff).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[0].old_path.as_deref(), Some(Path::new("src/main.rs")));
        assert_eq!(files[0].new_path.as_deref(), Some(Path::new("src/main.rs")));
        assert_eq!(files[0].hunks[0].old_start, 1);
        assert_eq!(files[0].hunks[0].new_start, 1);
        assert_eq!(files[0].hunks[0].lines[1].kind, DiffLineKind::Remove);
        assert_eq!(files[0].hunks[0].lines[1].old_line, Some(2));
        assert_eq!(files[0].hunks[0].lines[2].kind, DiffLineKind::Add);
        assert_eq!(files[0].hunks[0].lines[2].new_line, Some(2));
    }

    #[test]
    fn parses_rename_headers() {
        let diff = "\
diff --git a/old.txt b/new.txt
similarity index 100%
rename from old.txt
rename to new.txt
";

        let files = parse_unified_diff(diff).unwrap();

        assert_eq!(files[0].status, FileStatus::Renamed);
        assert_eq!(files[0].old_path.as_deref(), Some(Path::new("old.txt")));
        assert_eq!(files[0].new_path.as_deref(), Some(Path::new("new.txt")));
        assert!(files[0].hunks.is_empty());
    }

    #[test]
    fn git_diff_detects_added_deleted_modified_and_renamed_files() {
        let repo = TempRepo::new("phase9");
        repo.git(["init"]).unwrap();
        repo.git(["config", "user.email", "inductor@example.com"])
            .unwrap();
        repo.git(["config", "user.name", "Inductor"]).unwrap();
        fs::write(repo.path().join("modified.txt"), "old\n").unwrap();
        fs::write(repo.path().join("deleted.txt"), "delete me\n").unwrap();
        fs::write(repo.path().join("renamed.txt"), "rename me\n").unwrap();
        repo.git(["add", "."]).unwrap();
        repo.git(["commit", "-m", "base"]).unwrap();

        fs::write(repo.path().join("modified.txt"), "new\n").unwrap();
        fs::remove_file(repo.path().join("deleted.txt")).unwrap();
        fs::write(repo.path().join("added.txt"), "add me\n").unwrap();
        fs::rename(
            repo.path().join("renamed.txt"),
            repo.path().join("renamed-new.txt"),
        )
        .unwrap();
        repo.git(["add", "-A"]).unwrap();

        let summary = diff_worktree(&DiffRequest::new(repo.path(), "HEAD")).unwrap();
        let statuses = summary
            .files
            .iter()
            .map(|file| (file.display_path().to_path_buf(), file.status))
            .collect::<Vec<_>>();

        assert!(statuses.contains(&(PathBuf::from("modified.txt"), FileStatus::Modified)));
        assert!(statuses.contains(&(PathBuf::from("added.txt"), FileStatus::Added)));
        assert!(statuses.contains(&(PathBuf::from("deleted.txt"), FileStatus::Deleted)));
        assert!(statuses.contains(&(PathBuf::from("renamed-new.txt"), FileStatus::Renamed)));
        assert!(summary.added_lines() >= 2);
        assert!(summary.removed_lines() >= 2);
    }

    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new(label: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("inductor-diff-{label}-{nanos}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        fn git<I, S>(&self, args: I) -> Result<(), String>
        where
            I: IntoIterator<Item = S>,
            S: AsRef<OsStr>,
        {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.path)
                .args(args)
                .output()
                .map_err(|err| err.to_string())?;
            if output.status.success() {
                Ok(())
            } else {
                Err(String::from_utf8_lossy(&output.stderr).into_owned())
            }
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
