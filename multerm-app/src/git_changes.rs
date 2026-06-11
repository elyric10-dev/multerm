//! Git change tracking and diff UI helpers for per-terminal attribution.

use crossbeam_channel::{unbounded, Receiver};
use std::time::Duration;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Tracks files touched while a terminal session is open.
#[derive(Clone, Debug, Default)]
pub struct TerminalGitSession {
    pub repo_root: Option<PathBuf>,
    pub baseline_head: Option<String>,
    pub touched_paths: HashSet<PathBuf>,
    pub shell_pid: Option<u32>,
}

impl TerminalGitSession {
    pub fn begin(working_dir: &str, shell_pid: Option<u32>) -> Self {
        let mut session = Self {
            repo_root: None,
            baseline_head: None,
            touched_paths: HashSet::new(),
            shell_pid,
        };
        session.refresh_repo(working_dir);
        session
    }

    /// Re-resolve git root from workspace folder (handles `~` and updated paths).
    pub fn refresh_repo(&mut self, working_dir: &str) {
        self.repo_root = git_repo_root(working_dir);
        if self.baseline_head.is_none() {
            self.baseline_head = self
                .repo_root
                .as_deref()
                .and_then(|root| git_rev_parse_head(root).ok());
        }
    }

    pub fn note_path(&mut self, path: PathBuf) {
        if let Some(root) = &self.repo_root {
            if let Ok(rel) = path.strip_prefix(root) {
                self.touched_paths.insert(rel.to_path_buf());
                return;
            }
            if path.is_relative() {
                self.touched_paths.insert(path);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitFileStatus {
    Modified,
    Added,
    Deleted,
    Untracked,
    Renamed,
}

impl GitFileStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Untracked => "U",
            Self::Renamed => "R",
        }
    }

    pub fn color(self) -> eframe::egui::Color32 {
        use eframe::egui::Color32;
        match self {
            Self::Modified => Color32::from_rgb(230, 190, 80),
            Self::Added => Color32::from_rgb(115, 210, 120),
            Self::Deleted => Color32::from_rgb(240, 110, 110),
            Self::Untracked => Color32::from_rgb(140, 180, 240),
            Self::Renamed => Color32::from_rgb(180, 140, 240),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitFileEntry {
    pub path: PathBuf,
    pub status: GitFileStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Add,
    Remove,
    Hunk,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GitChangesScope {
    Terminal(u64),
    AllTerminals,
}

/// Open git changes modal state.
#[derive(Clone)]
pub struct GitChangesPanelState {
    pub workspace_idx: usize,
    pub scope: GitChangesScope,
    pub selected_path: Option<PathBuf>,
    pub commit_message: String,
    pub show_commit_section: bool,
    pub status_message: Option<String>,
}

/// Cached git lookup results so the modal does not re-shell to `git` every
/// frame (subprocess-per-frame causes noticeable FPS drops while it's open).
#[derive(Clone, Default)]
pub struct GitChangesPanelCache {
    pub workspace_idx: usize,
    pub scope: Option<GitChangesScope>,
    pub repo_root: Option<PathBuf>,
    pub baseline_head: Option<String>,
    pub paths: HashSet<PathBuf>,
    pub entries: Vec<GitFileEntry>,
    pub diff_path: Option<PathBuf>,
    pub diff_lines: Vec<DiffLine>,
}

/// Expand `~` and resolve relative paths the same way workspace spawn does.
pub fn resolve_working_dir(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    let expanded = if trimmed == "~" {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("~"))
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|home| PathBuf::from(home).join(rest))
            .unwrap_or_else(|_| PathBuf::from(trimmed))
    } else {
        PathBuf::from(trimmed)
    };
    if expanded.as_os_str().is_empty() {
        return std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    }
    let mut path = expanded;
    if path.is_relative() {
        if let Ok(cwd) = std::env::current_dir() {
            path = cwd.join(path);
        }
    }
    path
}

pub fn git_repo_root(working_dir: &str) -> Option<PathBuf> {
    git_repo_root_at(&resolve_working_dir(working_dir))
}

pub fn git_repo_root_at(dir: &Path) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// All paths changed in the working tree since `baseline` (session start).
pub fn changed_paths_since_baseline(repo: &Path, baseline: Option<&str>) -> HashSet<PathBuf> {
    let mut set = HashSet::new();
    let Some(base) = baseline else {
        return set;
    };
    let diff = Command::new("git")
        .args(["diff", "--name-only", base])
        .current_dir(repo)
        .output();
    if let Ok(out) = diff {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let line = line.trim();
                if !line.is_empty() {
                    set.insert(PathBuf::from(line));
                }
            }
        }
    }
    let untracked = Command::new("git")
        .args(["ls-files", "--others", "--exclude-standard"])
        .current_dir(repo)
        .output();
    if let Ok(out) = untracked {
        if out.status.success() {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let line = line.trim();
                if !line.is_empty() {
                    set.insert(PathBuf::from(line));
                }
            }
        }
    }
    set
}

pub fn git_is_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn git_head_at(repo: &Path) -> Option<String> {
    git_rev_parse_head(repo).ok()
}

fn git_rev_parse_head(repo: &Path) -> Result<String, String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn path_in_repo(repo: &Path, path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        path.strip_prefix(repo).ok().map(|p| p.to_path_buf())
    } else {
        Some(path.to_path_buf())
    }
}

pub fn collect_entries_for_paths(
    repo: &Path,
    baseline_head: Option<&str>,
    paths: &HashSet<PathBuf>,
) -> Vec<GitFileEntry> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    for rel in paths {
        let Some(status) = file_status_in_repo(repo, baseline_head, rel) else {
            continue;
        };
        entries.push(GitFileEntry {
            path: rel.clone(),
            status,
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

fn file_status_in_repo(
    repo: &Path,
    baseline_head: Option<&str>,
    rel: &Path,
) -> Option<GitFileStatus> {
    let full = repo.join(rel);
    if !full.exists() {
        // Deleted or never existed — check if git still knows it.
        let output = Command::new("git")
            .args(["ls-files", "--error-unmatch"])
            .arg(rel)
            .current_dir(repo)
            .output()
            .ok()?;
        if output.status.success() {
            return Some(GitFileStatus::Deleted);
        }
        return None;
    }

    let porcelain = Command::new("git")
        .args(["status", "--porcelain", "--"])
        .arg(rel)
        .current_dir(repo)
        .output()
        .ok()?;
    if !porcelain.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&porcelain.stdout);
    let first = line.lines().next()?;
    let code = first.get(0..2).unwrap_or("  ");
    let status = match code.trim() {
        "" => {
            if let Some(head) = baseline_head {
                let diff = Command::new("git")
                    .args(["diff", "--quiet", head, "--"])
                    .arg(rel)
                    .current_dir(repo)
                    .status()
                    .ok()?;
                if diff.success() {
                    return None;
                }
                Some(GitFileStatus::Modified)
            } else {
                None
            }
        }
        "M" | "MM" | "AM" => Some(GitFileStatus::Modified),
        "A" | "??" => {
            if code.contains('?') {
                Some(GitFileStatus::Untracked)
            } else {
                Some(GitFileStatus::Added)
            }
        }
        "D" => Some(GitFileStatus::Deleted),
        "R" => Some(GitFileStatus::Renamed),
        _ => Some(GitFileStatus::Modified),
    };
    status
}

pub fn git_diff_for_file(
    repo: &Path,
    baseline_head: Option<&str>,
    rel: &Path,
) -> Result<String, String> {
    let mut args = vec!["diff", "--no-color", "-U3"];
    if let Some(head) = baseline_head {
        args.push(head);
    }
    args.push("--");
    let rel_s = rel.to_string_lossy();
    args.push(rel_s.as_ref());

    let output = Command::new("git")
        .args(&args)
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() && output.stdout.is_empty() {
        // Untracked: show entire file as addition via diff /dev/null
        let output = Command::new("git")
            .args([
                "diff",
                "--no-color",
                "-U3",
                "--no-index",
                "/dev/null",
            ])
            .arg(repo.join(rel))
            .current_dir(repo)
            .output()
            .map_err(|e| e.to_string())?;
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn parse_unified_diff(text: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    let mut old_line: u32 = 0;
    let mut new_line: u32 = 0;

    for raw in text.lines() {
        if raw.starts_with("@@") {
            lines.push(DiffLine {
                kind: DiffLineKind::Hunk,
                text: raw.to_string(),
                old_line: None,
                new_line: None,
            });
            if let Some((o, n)) = parse_hunk_header(raw) {
                old_line = o;
                new_line = n;
            }
            continue;
        }
        if raw.starts_with("+++") || raw.starts_with("---") || raw.starts_with("diff ") {
            lines.push(DiffLine {
                kind: DiffLineKind::Hunk,
                text: raw.to_string(),
                old_line: None,
                new_line: None,
            });
            continue;
        }
        let (kind, content) = match raw.as_bytes().first() {
            Some(b'+') => (DiffLineKind::Add, &raw[1..]),
            Some(b'-') => (DiffLineKind::Remove, &raw[1..]),
            Some(b' ') => (DiffLineKind::Context, &raw[1..]),
            _ => (DiffLineKind::Context, raw),
        };
        let (o_num, n_num) = match kind {
            DiffLineKind::Add => {
                let n = new_line;
                new_line = new_line.saturating_add(1);
                (None, Some(n))
            }
            DiffLineKind::Remove => {
                let o = old_line;
                old_line = old_line.saturating_add(1);
                (Some(o), None)
            }
            DiffLineKind::Context => {
                let o = old_line;
                let n = new_line;
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
                (Some(o), Some(n))
            }
            DiffLineKind::Hunk => (None, None),
        };
        lines.push(DiffLine {
            kind,
            text: content.to_string(),
            old_line: o_num,
            new_line: n_num,
        });
    }
    lines
}

fn parse_hunk_header(s: &str) -> Option<(u32, u32)> {
    // @@ -1,4 +1,5 @@
    let plus = s.split('+').nth(1)?;
    let new_part = plus.split_whitespace().next()?;
    let new_start = new_part.split(',').next()?.parse().ok()?;
    let minus = s.split('-').nth(1)?;
    let old_part = minus.split('+').next()?.trim();
    let old_start = old_part.split(',').next()?.parse().ok()?;
    Some((old_start, new_start))
}

pub fn git_commit_paths(repo: &Path, paths: &[PathBuf], message: &str) -> Result<(), String> {
    if paths.is_empty() {
        return Err("No files selected to commit.".into());
    }
    let msg = message.trim();
    if msg.is_empty() {
        return Err("Commit message is required.".into());
    }
    let mut add = Command::new("git");
    add.arg("add").current_dir(repo);
    for p in paths {
        add.arg(p);
    }
    let add_out = add.output().map_err(|e| e.to_string())?;
    if !add_out.status.success() {
        return Err(String::from_utf8_lossy(&add_out.stderr).trim().to_string());
    }

    let commit = Command::new("git")
        .args(["commit", "-m", msg])
        .current_dir(repo)
        .output()
        .map_err(|e| e.to_string())?;
    if !commit.status.success() {
        return Err(String::from_utf8_lossy(&commit.stderr).trim().to_string());
    }
    Ok(())
}

/// Background watcher keyed by repository root.
pub struct GitRepoWatcherHub {
    watchers: HashMap<PathBuf, Receiver<Vec<PathBuf>>>,
}

impl Default for GitRepoWatcherHub {
    fn default() -> Self {
        Self {
            watchers: HashMap::new(),
        }
    }
}

impl GitRepoWatcherHub {
    pub fn ensure_watching(&mut self, repo_root: &Path) {
        let key = repo_root.to_path_buf();
        if self.watchers.contains_key(&key) {
            return;
        }
        if let Some(receiver) = spawn_repo_watcher(repo_root) {
            self.watchers.insert(key, receiver);
        }
    }

    /// Drain pending path batches from all watchers.
    pub fn drain_events(&mut self) -> Vec<(PathBuf, Vec<PathBuf>)> {
        let keys: Vec<PathBuf> = self.watchers.keys().cloned().collect();
        let mut out = Vec::new();
        for key in keys {
            let Some(rx) = self.watchers.get(&key) else {
                continue;
            };
            let mut batch = Vec::new();
            while let Ok(paths) = rx.try_recv() {
                batch.extend(paths);
            }
            if !batch.is_empty() {
                batch.sort();
                batch.dedup();
                out.push((key, batch));
            }
        }
        out
    }
}

fn spawn_repo_watcher(repo_root: &Path) -> Option<Receiver<Vec<PathBuf>>> {
    let (tx, rx) = unbounded::<Vec<PathBuf>>();
    let root = repo_root.to_path_buf();
    let root_watch = root.clone();
    let pending: Arc<Mutex<HashSet<PathBuf>>> = Arc::new(Mutex::new(HashSet::new()));
    let pending_cb = Arc::clone(&pending);

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| {
            let Ok(event) = res else {
                return;
            };
            if !classify_fs_event_kind(&event.kind) {
                return;
            }
            if let Ok(mut set) = pending_cb.lock() {
                for p in event.paths {
                    if let Some(rel) = path_in_repo(&root, &p) {
                        set.insert(rel);
                    }
                }
            }
        },
        notify::Config::default(),
    )
    .ok()?;

    watcher
        .watch(&root_watch, RecursiveMode::Recursive)
        .ok()?;

    let pending_flush = Arc::clone(&pending);
    std::thread::spawn(move || {
        let _watcher = watcher;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let batch: Vec<PathBuf> = pending_flush
                .lock()
                .ok()
                .map(|mut set| {
                    let v: Vec<PathBuf> = set.drain().collect();
                    v
                })
                .unwrap_or_default();
            if !batch.is_empty() {
                let _ = tx.send(batch);
            }
        }
    });

    Some(rx)
}

/// Returns true if `pid` is the same as or a descendant of `ancestor_pid`.
pub fn pid_in_tree(ancestor_pid: u32, pid: u32) -> bool {
    if ancestor_pid == pid {
        return true;
    }
    let output = Command::new("ps")
        .args(["-eo", "pid,ppid"])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let mut parent_of: HashMap<u32, u32> = HashMap::new();
    for line in String::from_utf8_lossy(&output.stdout).lines().skip(1) {
        let mut parts = line.split_whitespace();
        let Some(child_s) = parts.next() else {
            continue;
        };
        let Some(parent_s) = parts.next() else {
            continue;
        };
        if let (Ok(child), Ok(parent)) = (child_s.parse::<u32>(), parent_s.parse::<u32>()) {
            parent_of.insert(child, parent);
        }
    }
    let mut current = pid;
    for _ in 0..256 {
        if current == ancestor_pid {
            return true;
        }
        let Some(parent) = parent_of.get(&current).copied() else {
            break;
        };
        if parent == current {
            break;
        }
        current = parent;
    }
    false
}

pub fn classify_fs_event_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(_)
            | EventKind::Remove(_)
            | EventKind::Any
    )
}
