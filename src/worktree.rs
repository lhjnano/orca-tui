//! # WorktreeManager — per-agent isolated git worktrees (Feature 6)
//!
//! Each coding agent runs in its **own** git worktree so that N agents can
//! hack on the same repository simultaneously without clobbering each other's
//! working tree or index — mirroring Orca GUI's parallel-worktree model.
//!
//! This module is the mechanism layer only: it shells out to the `git` CLI
//! (no `git2`/`gix` dependency) to `git worktree add` a fresh branch + checkout
//! for an agent, and to tear it down again. A higher-level task wires this into
//! the app (one worktree per [`crate::agent::AgentSpec`]); here we expose only
//! [`WorktreeManager`], [`Worktree`] and the [`OwnedWorktrees`] session guard.
//!
//! ## Layout
//!
//! Worktrees live under `<repo-root>/.orca-worktrees/<slug>-<short-id>/` with
//! matching branch `orca/<slug>-<short-id>`. The `.orca-worktrees/` directory
//! is created *inside* the repo, so it shows up as untracked in `git status`
//! until the integration task adds `.orca-worktrees/` to `.gitignore` (TODO
//! there — intentionally out of scope for this module).
//!
//! ## short-id
//!
//! The 6-hex suffix is derived dependency-free from `SystemTime` nanos xor a
//! process-local atomic counter (no `rand` crate). Collision probability is
//! negligible for an orchestrator creating a handful of worktrees per session;
//! if `git worktree add` ever collides on an existing branch the git stderr is
//! surfaced verbatim.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Directory under the repo root that holds all agent worktrees.
///
/// NOTE: this appears as untracked in `git status` until the integration task
/// appends `.orca-worktrees/` to `.gitignore`. Deliberate, out-of-scope here.
const WORKTREE_DIR: &str = ".orca-worktrees";

/// Manages per-agent git worktrees under a single repository.
///
/// Construct with [`WorktreeManager::open`]; the path must be inside a git work
/// tree (a subdirectory is fine — the true top-level is resolved and stored).
/// Every git invocation goes through the `git` CLI binary, so `git` must be
/// installed and reachable on `$PATH`.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    /// Canonical top-level of the repository (`git rev-parse --show-toplevel`).
    repo_root: PathBuf,
}

impl WorktreeManager {
    /// Open the repository at `repo_root`.
    ///
    /// Verifies the path is accessible, that the `git` binary is available,
    /// and that the path is inside a git work tree, then resolves and stores
    /// the repository's true top-level so that `.orca-worktrees` is always
    /// created at the repo root even when a subdirectory was passed in.
    pub fn open(repo_root: impl AsRef<Path>) -> Result<Self> {
        let input = repo_root.as_ref();
        let canon = input
            .canonicalize()
            .with_context(|| format!("repo root {} is not accessible", input.display()))?;

        // (1) `git` must be installed and runnable.
        let version = Command::new("git")
            .arg("--version")
            .output()
            .context("failed to spawn `git --version` — is git on PATH?")?;
        if !version.status.success() {
            bail!("`git --version` exited non-zero — git does not appear to be installed");
        }

        // (2) the path must be inside a git work tree.
        let inside = Command::new("git")
            .arg("-C")
            .arg(&canon)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .context("failed to spawn `git rev-parse`")?;
        if !inside.status.success()
            || !String::from_utf8_lossy(&inside.stdout)
                .trim()
                .eq_ignore_ascii_case("true")
        {
            bail!(
                "{} is not inside a git work tree{}",
                canon.display(),
                if inside.stderr.is_empty() {
                    String::new()
                } else {
                    format!(": {}", String::from_utf8_lossy(&inside.stderr).trim())
                }
            );
        }

        // (3) resolve the true top-level (a passed subdirectory is fine).
        let toplevel = Command::new("git")
            .arg("-C")
            .arg(&canon)
            .args(["rev-parse", "--show-toplevel"])
            .output()?;
        let repo_root = if toplevel.status.success() {
            PathBuf::from(String::from_utf8_lossy(&toplevel.stdout).trim())
        } else {
            // Unreachable given (2), but fall back to the canonical input.
            canon
        };

        Ok(Self { repo_root })
    }

    /// The resolved repository top-level.
    #[must_use]
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Create a fresh worktree + branch for `agent_name`.
    ///
    /// `agent_name` is sanitized to a filesystem/branch-safe slug; the worktree
    /// is checked out at `<repo>/.orca-worktrees/<slug>-<short-id>/` on a new
    /// branch `orca/<slug>-<short-id>` rooted at `HEAD`. On failure the raw
    /// `git worktree add` stderr is surfaced in the error.
    pub fn create_for(&self, agent_name: &str) -> Result<Worktree> {
        let slug = slugify(agent_name);
        let id = short_id();
        let suffix = format!("{slug}-{id}");
        let branch = format!("orca/{suffix}");
        let path = self.repo_root.join(WORKTREE_DIR).join(&suffix);

        // Ensure the parent exists so the layout is deterministic across git
        // versions (newer `git worktree add` creates leading dirs; older ones
        // may not). Idempotent.
        std::fs::create_dir_all(self.repo_root.join(WORKTREE_DIR))
            .with_context(|| format!("failed to create {WORKTREE_DIR}"))?;

        let out = self
            .git()
            .args(["worktree", "add", "-b", &branch])
            .arg(&path)
            .arg("HEAD")
            .output()
            .context("failed to spawn `git worktree add`")?;
        if !out.status.success() {
            bail!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        Ok(Worktree { path, branch })
    }

    /// Remove a worktree (best-effort).
    ///
    /// Runs `git worktree remove --force <path>`. If git reports failure
    /// *because the directory is already gone*, that is treated as success —
    /// the caller's goal (path no longer registered) is already satisfied.
    pub fn remove(&self, path: &Path) -> Result<()> {
        let out = self
            .git()
            .args(["worktree", "remove", "--force"])
            .arg(path)
            .output()?;
        if out.status.success() {
            return Ok(());
        }
        // Best-effort: directory already gone → nothing left to do.
        if !path.exists() {
            return Ok(());
        }
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    /// Force-delete a branch (`git branch -D`).
    ///
    /// Removing the orphaned branch after [`WorktreeManager::remove`] is the
    /// caller's concern — this just exposes the primitive. Fails if the branch
    /// is still checked out somewhere.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        let out = self.git().args(["branch", "-D", branch]).output()?;
        if out.status.success() {
            Ok(())
        } else {
            bail!(
                "git branch -D failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        }
    }

    /// Prune stale worktree metadata (`git worktree prune`).
    pub fn prune(&self) -> Result<()> {
        let out = self.git().args(["worktree", "prune"]).output()?;
        if out.status.success() {
            Ok(())
        } else {
            bail!(
                "git worktree prune failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
        }
    }

    /// Start a `git -C <repo_root> ...` command (repo root pre-bound).
    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.repo_root);
        cmd
    }
}

/// One created worktree: its on-disk path and the branch it lives on.
#[derive(Debug, Clone)]
pub struct Worktree {
    /// Absolute path to the worktree checkout.
    pub path: PathBuf,
    /// Branch name (`orca/<slug>-<short-id>`).
    pub branch: String,
}

/// Session-scoped guard over a set of worktrees created in one app run.
///
/// Tracks every [`Worktree`] produced via [`OwnedWorktrees::create_for`] and
/// removes them (plus their branches) on [`Drop`] — best-effort, logging
/// failures to stderr and never panicking. Use [`OwnedWorktrees::drain_into`]
/// to transfer ownership out and *disarm* the cleanup (keep-on-exit mode).
pub struct OwnedWorktrees {
    manager: WorktreeManager,
    entries: Vec<Worktree>,
}

impl OwnedWorktrees {
    /// Wrap a manager with an empty worktree set.
    #[must_use]
    pub fn new(manager: WorktreeManager) -> Self {
        Self {
            manager,
            entries: Vec::new(),
        }
    }

    /// Create a worktree for `agent_name` via the inner manager and remember it
    /// for Drop cleanup. Returns a reference to the stored [`Worktree`].
    pub fn create_for(&mut self, agent_name: &str) -> Result<&Worktree> {
        let wt = self.manager.create_for(agent_name)?;
        self.entries.push(wt);
        Ok(self.entries.last().expect("just pushed"))
    }

    /// A shared view of the inner manager.
    #[must_use]
    pub fn manager(&self) -> &WorktreeManager {
        &self.manager
    }

    /// A view of the tracked worktrees.
    #[must_use]
    pub fn entries(&self) -> &[Worktree] {
        &self.entries
    }

    /// Transfer ownership of all tracked worktrees out, disarming Drop cleanup.
    ///
    /// Consumes `self`. The inner `entries` are moved into the returned
    /// `Vec<Worktree>` and replaced with an empty vec, so the subsequent
    /// [`Drop`] iterates nothing — the worktrees persist on disk (keep-on-exit
    /// mode). The inner manager is dropped normally.
    pub fn drain_into(mut self) -> Vec<Worktree> {
        std::mem::take(&mut self.entries)
    }
}

impl std::fmt::Debug for OwnedWorktrees {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedWorktrees")
            .field("manager", &self.manager)
            .field("entries_len", &self.entries.len())
            .finish()
    }
}

impl Drop for OwnedWorktrees {
    fn drop(&mut self) {
        for wt in &self.entries {
            if let Err(e) = self.manager.remove(&wt.path) {
                eprintln!(
                    "orca-tui: worktree cleanup: failed to remove {}: {e:#}",
                    wt.path.display()
                );
            }
            if let Err(e) = self.manager.delete_branch(&wt.branch) {
                eprintln!(
                    "orca-tui: worktree cleanup: failed to delete branch {}: {e:#}",
                    wt.branch
                );
            }
        }
    }
}

/// Sanitize an arbitrary agent name into a filesystem/branch-safe slug:
/// lowercase ASCII alphanumerics; runs of any other character collapse to a
/// single `-`; leading/trailing dashes are trimmed. An all-empty result falls
/// back to `"agent"`.
fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = true; // suppress a leading dash
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "agent".to_owned()
    } else {
        out
    }
}

/// 6-hex-char unique suffix, dependency-free.
///
/// Mixes `SystemTime` nanos with a process-local atomic counter via a splitmix
/// step so the low 24 bits vary even when two ids are produced within the same
/// nanosecond. No `rand` crate.
fn short_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);

    let mut x = nanos ^ seq.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 33;
    x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
    x ^= x >> 33;

    format!("{:06x}", x & 0x00FF_FFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- always-on pure unit tests ----------------------------------------

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Claude Code"), "claude-code");
        assert_eq!(slugify("codex"), "codex");
        assert_eq!(slugify("my_agent!!"), "my-agent");
        assert_eq!(slugify("Claude Code!"), "claude-code");
        assert_eq!(slugify("  spaced  out "), "spaced-out");
        assert_eq!(slugify("MiXeD123"), "mixed123");
    }

    #[test]
    fn slugify_empty_or_all_unsafe_falls_back() {
        assert_eq!(slugify(""), "agent");
        assert_eq!(slugify("---"), "agent");
        assert_eq!(slugify("!!!"), "agent");
        assert_eq!(slugify("   "), "agent");
    }

    #[test]
    fn slugify_no_leading_or_trailing_dashes() {
        assert_eq!(slugify("__foo__"), "foo");
        assert_eq!(slugify("/usr/bin/claude"), "usr-bin-claude");
    }

    #[test]
    fn short_id_is_six_hex_chars_and_varies() {
        let a = short_id();
        let b = short_id();
        let c = short_id();
        for id in [a.clone(), b.clone(), c.clone()] {
            assert_eq!(id.len(), 6, "expected 6 chars, got {id}");
            assert!(
                id.chars().all(|ch| ch.is_ascii_hexdigit()),
                "non-hex char in {id}"
            );
        }
        // Three rapid ids should not all be identical (counter mixing works).
        assert!(
            !(a == b && b == c),
            "short_id produced three identical values ({a}); counter mixing broken"
        );
    }

    // ---- git-backed integration tests -------------------------------------
    //
    // These spin up a throwaway git repo under std::env::temp_dir(), run
    // `git init` + an initial commit, and exercise the real WorktreeManager
    // against it. Guarded to unix + git (git is verified present on the dev
    // box); deliberately NOT #[ignore]d so an environment regression surfaces
    // loudly instead of silently skipping.

    /// A throwaway git repo rooted at a unique temp dir; removed on Drop.
    struct TempRepo {
        path: PathBuf,
    }

    impl TempRepo {
        fn new() -> Result<Self> {
            let mut dir = std::env::temp_dir();
            dir.push(format!(
                "orca-tui-test-{}-{}",
                std::process::id(),
                short_id()
            ));
            std::fs::create_dir_all(&dir)?;

            let init = Command::new("git").arg("init").arg(&dir).output()?;
            if !init.status.success() {
                bail!(
                    "test setup: git init failed: {}",
                    String::from_utf8_lossy(&init.stderr).trim()
                );
            }

            // Local identity so `git commit` works in CI without any global
            // git config (the spec's `git -c user.name=...` intent).
            for (k, v) in [("user.name", "orca-tui test"), ("user.email", "test@orca-tui.local")]
            {
                let cfg = Command::new("git")
                    .arg("-C")
                    .arg(&dir)
                    .args(["config", k, v])
                    .output()?;
                if !cfg.status.success() {
                    bail!("test setup: git config {k} failed");
                }
            }

            // An initial commit so HEAD exists (worktree add -b ... HEAD needs
            // a born branch — on an unborn repo it errors with "invalid
            // reference: HEAD").
            std::fs::write(dir.join("README.md"), "# test\n")?;
            let add = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["add", "."])
                .output()?;
            if !add.status.success() {
                bail!("test setup: git add failed");
            }
            let commit = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["commit", "-m", "init"])
                .output()?;
            if !commit.status.success() {
                bail!(
                    "test setup: git commit failed: {}",
                    String::from_utf8_lossy(&commit.stderr).trim()
                );
            }

            Ok(Self { path: dir })
        }

        /// Run `git -C <repo> <args>`; return trimmed stdout, asserting success.
        fn git_stdout(&self, args: &[&str]) -> String {
            let out = Command::new("git")
                .arg("-C")
                .arg(&self.path)
                .args(args)
                .output()
                .expect("spawn git");
            assert!(
                out.status.success(),
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
            String::from_utf8_lossy(&out.stdout).trim().to_owned()
        }

        /// True if `path` appears as a line prefix in `git worktree list`.
        fn worktree_listed(&self, path: &Path) -> bool {
            let list = self.git_stdout(&["worktree", "list"]);
            let target = path.to_string_lossy().into_owned();
            list.lines().any(|line| line.starts_with(&target))
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            // Blow away the whole temp repo on disk; git metadata inside it is
            // irrelevant once the directory is gone. Best-effort.
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_errors_on_non_git_directory() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "orca-tui-nongit-{}-{}",
            std::process::id(),
            short_id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let err = WorktreeManager::open(&dir).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not inside a git work tree"),
            "expected a git-work-tree error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn create_for_produces_existing_worktree_on_expected_branch() {
        let repo = TempRepo::new().expect("temp git repo");
        let mgr = WorktreeManager::open(&repo.path).expect("open");

        let wt = mgr.create_for("Claude Code").expect("create");
        assert!(wt.path.is_dir(), "worktree path should exist");
        assert!(wt.path.starts_with(repo.path.join(WORKTREE_DIR)));
        assert!(
            wt.branch.starts_with("orca/claude-code-"),
            "unexpected branch: {}",
            wt.branch
        );

        // It is a real git work tree.
        let inside = Command::new("git")
            .arg("-C")
            .arg(&wt.path)
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .expect("spawn git");
        assert!(inside.status.success());
        assert_eq!(String::from_utf8_lossy(&inside.stdout).trim(), "true");

        // And HEAD sits on our branch.
        let branch = Command::new("git")
            .arg("-C")
            .arg(&wt.path)
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .expect("spawn git");
        assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), wt.branch);

        // Registered in `git worktree list`.
        assert!(repo.worktree_listed(&wt.path));
    }

    #[cfg(unix)]
    #[test]
    fn remove_unregisters_worktree_then_deletes_branch() {
        let repo = TempRepo::new().expect("temp git repo");
        let mgr = WorktreeManager::open(&repo.path).expect("open");
        let wt = mgr.create_for("codex").expect("create");
        assert!(repo.worktree_listed(&wt.path));

        mgr.remove(&wt.path).expect("remove");
        assert!(
            !repo.worktree_listed(&wt.path),
            "worktree should no longer be registered"
        );

        // Branch can now be deleted (no longer checked out anywhere).
        mgr.delete_branch(&wt.branch).expect("delete branch");

        // And it is actually gone.
        let branches = repo.git_stdout(&["branch", "--list"]);
        assert!(
            !branches.contains(&wt.branch),
            "branch {} should be deleted, got: {branches}",
            wt.branch
        );
    }

    #[cfg(unix)]
    #[test]
    fn remove_when_already_gone_is_success() {
        let repo = TempRepo::new().expect("temp git repo");
        let mgr = WorktreeManager::open(&repo.path).expect("open");
        let wt = mgr.create_for("gemini").expect("create");
        mgr.remove(&wt.path).expect("first remove");
        // Second remove: path is already gone — must not error.
        mgr.remove(&wt.path).expect("remove of already-gone worktree");
    }

    #[cfg(unix)]
    #[test]
    fn owned_worktrees_drop_removes_every_worktree() {
        let repo = TempRepo::new().expect("temp git repo");
        let mgr = WorktreeManager::open(&repo.path).expect("open");
        let mut owned = OwnedWorktrees::new(mgr);

        let w0 = owned.create_for("Claude Code").expect("c0").path.clone();
        let w1 = owned.create_for("codex").expect("c1").path.clone();
        assert!(repo.worktree_listed(&w0));
        assert!(repo.worktree_listed(&w1));

        drop(owned); // triggers Drop cleanup

        assert!(!repo.worktree_listed(&w0), "w0 should be cleaned up");
        assert!(!repo.worktree_listed(&w1), "w1 should be cleaned up");
    }

    #[cfg(unix)]
    #[test]
    fn drain_into_disarms_drop_cleanup() {
        let repo = TempRepo::new().expect("temp git repo");
        let mgr = WorktreeManager::open(&repo.path).expect("open");
        let mut owned = OwnedWorktrees::new(mgr);
        owned.create_for("amp").expect("create");

        let path = {
            let kept = owned.drain_into();
            assert_eq!(kept.len(), 1, "drain_into should yield the tracked worktree");
            kept[0].path.clone()
        }; // `kept` dropped here, but Worktree has no Drop -> stays on disk

        assert!(
            repo.worktree_listed(&path),
            "drain_into must leave the worktree registered (cleanup disarmed)"
        );
        // TempRepo::drop removes the whole temp repo on disk.
    }
}
