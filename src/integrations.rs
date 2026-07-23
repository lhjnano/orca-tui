//! # Integrations — external issue/PR sources (Feature 9)
//!
//! Feature 9 wires GitHub (and, later, Linear) into orca-tui so you can browse
//! open PRs/issues from inside the TUI and turn an issue into a dispatched
//! agent task. This module is the **source layer**: it knows how to fetch
//! [`PullRequest`] / [`Issue`] lists and how to render an [`Issue`] into a
//! prompt string.
//!
//! ## GitHub via `gh`, not HTTP
//!
//! GitHub access shells out to the [`gh`](https://cli.github.com) CLI
//! (`gh pr list`, `gh issue list`) and parses its JSON output with
//! [`serde_json`]. This deliberately avoids pulling in an HTTP crate: `gh`
//! already handles auth (tokens via `GH_TOKEN`/`gh auth login`), pagination
//! and rate limits for us, and it is a natural dependency for a developer
//! tool that lives in the terminal. `gh` must be installed and reachable on
//! `$PATH` for the `gh`-backed functions to work.
//!
//! ## Linear (stub)
//!
//! Linear needs a real HTTP client + API token, which is out of scope for the
//! initial GitHub-only cut. It is modelled here only as the [`IssueSource`]
//! trait so a future [`LinearSource`](#) (TODO) can drop in behind the same
//! abstraction without touching call sites.
//!
//! ## Testability
//!
//! The parsing logic (serde deserialize + [`RepoRef::parse`] + [`issue_to_prompt`])
//! is pure and covered by always-on unit tests using embedded sample JSON. The
//! `gh`-backed functions ([`list_pull_requests`] / [`list_issues`] / [`run_gh`])
//! require a live `gh` binary + network + auth, so they are **not** exercised by
//! the offline test suite — they are marked with a comment at the call site.

use std::fmt;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// A GitHub pull request.
///
/// Fields map onto `gh pr list --json number,title,headRefName`. `branch` is
/// sourced from the `headRefName` field (the branch the PR's head commit lives
/// on) — it is `None` when `gh` omits `headRefName` (e.g. a draft from a
/// deleted fork).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PullRequest {
    /// PR number (e.g. `42`).
    pub number: u64,
    /// PR title.
    pub title: String,
    /// Head branch name, from the `headRefName` JSON field.
    #[serde(rename = "headRefName")]
    pub branch: Option<String>,
}

/// A GitHub (or, later, Linear) issue.
///
/// The list endpoint (`gh issue list --json number,title`) does not return a
/// body, so `body` deserializes to `None` there; a single-issue fetch that
/// includes `body` populates it. Kept on the struct so the same model serves
/// both list and detail views.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Issue {
    /// Issue number (e.g. `7`).
    pub number: u64,
    /// Issue title.
    pub title: String,
    /// Issue body, when present (trimmed downstream by [`issue_to_prompt`]).
    #[serde(default)]
    pub body: Option<String>,
}

/// An `owner/name` GitHub repository reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepoRef {
    /// Repository owner (user or org), e.g. `stablyai`.
    pub owner: String,
    /// Repository name, e.g. `orca`.
    pub name: String,
}

impl RepoRef {
    /// Parse an `owner/name` string into a [`RepoRef`].
    ///
    /// Accepts exactly one `/`; rejects empty owner/name, missing slash, and
    /// any stray second slash. Round-trips with [`RepoRef`]'s [`Display`]
    /// impl (`format!("{repo}")` → `owner/name`).
    pub fn parse(s: &str) -> Result<Self> {
        let (owner, name) = s.split_once('/').ok_or_else(|| {
            anyhow::anyhow!("invalid repo reference `{s}`: expected `owner/name`")
        })?;
        if owner.is_empty() || name.is_empty() {
            bail!("invalid repo reference `{s}`: owner and name must be non-empty");
        }
        if name.contains('/') {
            bail!("invalid repo reference `{s}`: expected exactly one `/`");
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

// ---- GitHub via gh CLI ----------------------------------------------------

/// Run `gh <args..>`, returning stdout as a UTF-8 string.
///
/// Requires the `gh` CLI installed and on `$PATH`. A non-zero exit status is
/// converted into an error that includes the trimmed stderr so callers see
/// auth/quota/not-found failures verbatim.
///
/// *Not covered by the offline test suite* — needs a live `gh` binary.
fn run_gh(args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .context("failed to spawn `gh` — is the GitHub CLI installed and on PATH?")?;
    if !out.status.success() {
        bail!(
            "gh {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// List open pull requests for `repo` via `gh pr list`.
///
/// Runs `gh pr list --repo OWNER/NAME --json number,title,headRefName --limit 30`
/// and parses the JSON array into [`Vec<PullRequest>`]. `gh` must be installed
/// and authenticated.
///
/// *Not covered by the offline test suite* — needs `gh` + network + auth.
pub fn list_pull_requests(repo: &RepoRef) -> Result<Vec<PullRequest>> {
    let repo_arg = repo.to_string();
    let json = run_gh(&[
        "pr",
        "list",
        "--repo",
        &repo_arg,
        "--json",
        "number,title,headRefName",
        "--limit",
        "30",
    ])?;
    let prs: Vec<PullRequest> = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse `gh pr list` JSON: {json}"))?;
    Ok(prs)
}

/// List open issues for `repo` via `gh issue list`.
///
/// Runs `gh issue list --repo OWNER/NAME --json number,title --limit 30` and
/// parses the JSON array into [`Vec<Issue>`]. The list endpoint does not return
/// `body`, so each [`Issue::body`] is `None`. `gh` must be installed and
/// authenticated.
///
/// *Not covered by the offline test suite* — needs `gh` + network + auth.
pub fn list_issues(repo: &RepoRef) -> Result<Vec<Issue>> {
    let repo_arg = repo.to_string();
    let json = run_gh(&[
        "issue",
        "list",
        "--repo",
        &repo_arg,
        "--json",
        "number,title",
        "--limit",
        "30",
    ])?;
    let issues: Vec<Issue> = serde_json::from_str(&json)
        .with_context(|| format!("failed to parse `gh issue list` JSON: {json}"))?;
    Ok(issues)
}

/// Render an [`Issue`] into a short prompt string to hand to a coding agent.
///
/// Always includes the issue number and title; appends the trimmed body when
/// it is present and non-empty (whitespace-only bodies are treated as empty).
pub fn issue_to_prompt(issue: &Issue) -> String {
    let mut out = format!("#{} {}", issue.number, issue.title);
    let body = issue
        .body
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    if let Some(body) = body {
        out.push_str("\n\n");
        out.push_str(body);
    }
    out
}

// ---- IssueSource abstraction (Linear stub) --------------------------------

/// A source of open issues to browse / turn into agent tasks.
///
/// [`GitHubSource`] implements this against the `gh` CLI; a future
/// `LinearSource` (TODO: needs an API token and HTTP client) can implement it
/// against the Linear REST API and be swapped in behind the same call sites.
pub trait IssueSource {
    /// List the open issues currently visible to this source.
    fn list_open(&self) -> Result<Vec<Issue>>;
}

/// GitHub as an [`IssueSource`], backed by the `gh` CLI ([`list_issues`]).
#[derive(Debug, Clone)]
pub struct GitHubSource {
    repo: RepoRef,
}

impl GitHubSource {
    /// Wrap a repository reference. All [`IssueSource`] calls target this repo.
    #[must_use]
    pub fn new(repo: RepoRef) -> Self {
        Self { repo }
    }
}

 impl IssueSource for GitHubSource {
     fn list_open(&self) -> Result<Vec<Issue>> {
         list_issues(&self.repo)
     }
 }

/// Best-effort Linear issue source (Feature 9). Linear's API needs an auth
/// token and an HTTP client; to avoid pulling a network crate into this build,
/// the actual GraphQL call is deferred. This impl is **wired but inert**:
///
/// - If `LINEAR_API_KEY` is unset, `list_open` returns an empty list (Linear
///   simply contributes no tasks) — the orchestration pipeline keeps working
///   with GitHub alone.
/// - If `LINEAR_API_KEY` IS set, `list_open` returns a clear "not yet wired"
///   error so a user who configures a token is told precisely what is missing
///   rather than silently getting nothing.
///
/// Enabling real Linear means adding an HTTP client (e.g. `reqwest`) and
/// implementing the GraphQL query here — no other plumbing changes.
pub struct LinearSource;

impl LinearSource {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl IssueSource for LinearSource {
    fn list_open(&self) -> Result<Vec<Issue>> {
        if std::env::var_os("LINEAR_API_KEY").is_some() {
            anyhow::bail!(
                "Linear token is set but the HTTP client is not yet wired — \
                 implement the GraphQL query in integrations::LinearSource \
                 (add an HTTP crate) to enable it"
            );
        }
        // Token absent: Linear is disabled, contribute no issues.
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parsing: PullRequest (headRefName -> branch) ----------------------

    #[test]
    fn parse_pull_request_json_maps_headrefname_to_branch() {
        let json = r#"[
            {"number": 42, "title": "Fix login bug", "headRefName": "fix/login"},
            {"number": 100, "title": "Add dark mode", "headRefName": "feat/dark-mode"}
        ]"#;
        let prs: Vec<PullRequest> = serde_json::from_str(json).expect("parse PR array");

        assert_eq!(prs.len(), 2);
        assert_eq!(
            prs[0],
            PullRequest {
                number: 42,
                title: "Fix login bug".to_owned(),
                branch: Some("fix/login".to_owned()),
            }
        );
        assert_eq!(
            prs[1],
            PullRequest {
                number: 100,
                title: "Add dark mode".to_owned(),
                branch: Some("feat/dark-mode".to_owned()),
            }
        );
    }

    #[test]
    fn parse_pull_request_json_allows_missing_headrefname() {
        // gh always emits headRefName when requested, but the model tolerates
        // an absent field -> branch None (Option defaults to None).
        let json = r#"[{"number": 1, "title": "No branch here"}]"#;
        let prs: Vec<PullRequest> = serde_json::from_str(json).expect("parse");
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 1);
        assert_eq!(prs[0].title, "No branch here");
        assert!(prs[0].branch.is_none(), "missing headRefName -> None");
    }

    // ---- parsing: Issue (body present / absent) ----------------------------

    #[test]
    fn parse_issue_json_handles_body_present_and_absent() {
        let json = r#"[
            {"number": 7, "title": "Crash on startup", "body": "App crashes immediately when launched."},
            {"number": 12, "title": "Add README"}
        ]"#;
        let issues: Vec<Issue> = serde_json::from_str(json).expect("parse issue array");

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 7);
        assert_eq!(issues[0].title, "Crash on startup");
        assert_eq!(
            issues[0].body.as_deref(),
            Some("App crashes immediately when launched.")
        );
        assert_eq!(issues[1].number, 12);
        assert_eq!(issues[1].title, "Add README");
        // `gh issue list --json number,title` omits body -> deserializes None.
        assert!(issues[1].body.is_none(), "missing body -> None");
    }

    // ---- issue_to_prompt ---------------------------------------------------

    #[test]
    fn issue_to_prompt_includes_number_title_and_trimmed_body() {
        let issue = Issue {
            number: 1337,
            title: "Refactor the scheduler".to_owned(),
            body: Some("  please make it lock-free\n\n".to_owned()),
        };
        let prompt = issue_to_prompt(&issue);

        // Number + title are always present.
        assert!(
            prompt.contains("1337"),
            "prompt must include the number: {prompt}"
        );
        assert!(
            prompt.contains("Refactor the scheduler"),
            "prompt must include the title: {prompt}"
        );
        // Body is trimmed of surrounding whitespace.
        assert!(
            prompt.contains("please make it lock-free"),
            "prompt must include the trimmed body: {prompt}"
        );
        assert!(
            !prompt.contains("\n\n\n"),
            "body should be trimmed, not pasted verbatim with its padding"
        );
    }

    #[test]
    fn issue_to_prompt_omits_body_section_when_empty_or_missing() {
        let none_body = Issue {
            number: 1,
            title: "Just a title".to_owned(),
            body: None,
        };
        let prompt = issue_to_prompt(&none_body);
        assert_eq!(prompt, "#1 Just a title");

        let blank_body = Issue {
            number: 2,
            title: "Whitespace body".to_owned(),
            body: Some("   \n\t ".to_owned()),
        };
        let prompt = issue_to_prompt(&blank_body);
        assert_eq!(
            prompt, "#2 Whitespace body",
            "whitespace-only body should be treated as empty"
        );
    }

    // ---- RepoRef -----------------------------------------------------------

    #[test]
    fn reporef_parse_round_trips_through_display() {
        let repo = RepoRef::parse("stablyai/orca").expect("valid");
        assert_eq!(repo.owner, "stablyai");
        assert_eq!(repo.name, "orca");
        // Round-trip: Display re-joins with a single slash.
        assert_eq!(repo.to_string(), "stablyai/orca");
        // And re-parsing the Display form yields the same struct.
        let again = RepoRef::parse(&repo.to_string()).expect("re-parse");
        assert_eq!(again, repo);
    }

    #[test]
    fn reporef_parse_rejects_invalid_inputs() {
        // Missing slash.
        let err = RepoRef::parse("just-a-name").unwrap_err();
        assert!(
            format!("{err:#}").contains("owner/name"),
            "missing slash should be rejected: {err:#}"
        );
        // Empty owner.
        assert!(RepoRef::parse("/name").is_err());
        // Empty name.
        assert!(RepoRef::parse("owner/").is_err());
        // Too many slashes.
        assert!(RepoRef::parse("a/b/c").is_err());
    }

    #[test]
    fn reporef_display_format_matches_owner_slash_name() {
        let repo = RepoRef {
            owner: "lhjnano".to_owned(),
            name: "orca-tui".to_owned(),
        };
        assert_eq!(format!("{repo}"), "lhjnano/orca-tui");
    }

    // ---- GitHubSource (constructor + trait wiring, no network) -------------

    #[test]
    fn github_source_round_trip_repos_through_trait() {
        // We can't call list_open() without gh+network, but we can confirm the
        // constructor stores the repo and the type satisfies IssueSource.
        let repo = RepoRef::parse("lhjnano/orca-tui").unwrap();
        let src = GitHubSource::new(repo.clone());
        // The trait is implemented (compile-time check via a type annotation).
        let _boxed: &dyn IssueSource = &src;
        // Round-trip the stored repo via the (pub) fields to be sure nothing
        // was mangled during construction.
        assert_eq!(src.repo.owner, "lhjnano");
        assert_eq!(src.repo.name, "orca-tui");
    }
}
