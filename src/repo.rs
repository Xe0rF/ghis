//! Repository discovery and remote/config helpers.
//!
//! This module intentionally uses the git executable rather than libgit2.  Git
//! itself knows about worktrees, alternates and unusual repository layouts, so
//! asking it for the paths also keeps this code correct for those layouts.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::tempdir;

#[derive(Debug)]
pub enum RepoError {
    Io(std::io::Error),
    NotRepository {
        path: PathBuf,
        stderr: String,
    },
    Command {
        operation: String,
        code: Option<i32>,
        stderr: String,
    },
    InvalidOutput {
        operation: String,
        output: String,
    },
}

impl fmt::Display for RepoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "git: {err}"),
            Self::NotRepository { path, stderr } => {
                if stderr.is_empty() {
                    write!(f, "{} is not a git repository", path.display())
                } else {
                    write!(f, "{} is not a git repository: {stderr}", path.display())
                }
            }
            Self::Command {
                operation,
                code,
                stderr,
            } => {
                write!(f, "git {operation} failed")?;
                if let Some(code) = code {
                    write!(f, " (exit {code})")?;
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            Self::InvalidOutput { operation, output } => {
                write!(f, "invalid git output for {operation}: {output}")
            }
        }
    }
}

impl std::error::Error for RepoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RepoError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, RepoError>;

/// The paths git reports for one working tree/repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    /// The path supplied to [`discover`], made absolute when possible.
    pub path: PathBuf,
    /// The per-worktree git directory (or the repository directory for a bare
    /// repository).
    pub git_dir: PathBuf,
    /// The common git directory.  For ordinary repositories this equals
    /// `git_dir`; for linked worktrees it is the main repository's `.git`.
    pub common_dir: PathBuf,
    /// `None` for a bare repository, otherwise the worktree root.
    pub root: Option<PathBuf>,
    pub bare: bool,
}

impl Repository {
    pub fn is_worktree(&self) -> bool {
        self.root.is_some() && self.git_dir != self.common_dir
    }

    /// Path used as the working directory for `git -C`.
    pub fn command_dir(&self) -> &Path {
        self.root.as_deref().unwrap_or(&self.path)
    }
}

/// Locate the repository containing `path` (or inspect `path` itself when it
/// is a repository).  Git resolves the path, including worktrees and bare
/// repositories, so no filesystem heuristics are required here.
pub fn discover(path: impl AsRef<Path>) -> Result<Repository> {
    let supplied = path.as_ref();
    let path = absolute_path(supplied);
    let out = run_git(
        &path,
        [
            "rev-parse",
            "--git-dir",
            "--git-common-dir",
            "--is-bare-repository",
        ],
    )?;
    if !out.status.success() {
        return Err(RepoError::NotRepository {
            path,
            stderr: clean_stderr(&out),
        });
    }

    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<_> = stdout.lines().map(str::trim).collect();
    if lines.len() < 3 {
        return Err(RepoError::InvalidOutput {
            operation: "rev-parse".into(),
            output: String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        });
    }

    let git_dir = resolve_git_path(&path, lines[0]);
    let common_dir = resolve_git_path(&path, lines[1]);
    let bare = match lines[2] {
        "true" => true,
        "false" => false,
        other => {
            return Err(RepoError::InvalidOutput {
                operation: "rev-parse --is-bare-repository".into(),
                output: other.to_owned(),
            });
        }
    };
    let root = if bare {
        None
    } else {
        let top = run_git(&path, ["rev-parse", "--show-toplevel"])?;
        if !top.status.success() {
            return Err(command_error("rev-parse --show-toplevel", &top));
        }
        let reported = String::from_utf8_lossy(&top.stdout).trim().to_owned();
        if reported.is_empty() {
            return Err(RepoError::InvalidOutput {
                operation: "rev-parse --show-toplevel".into(),
                output: reported,
            });
        }
        let reported = PathBuf::from(reported);
        Some(if reported.is_absolute() {
            reported
        } else {
            path.join(reported)
        })
    };

    Ok(Repository {
        path,
        git_dir,
        common_dir,
        root,
        bare,
    })
}

/// A parsed remote URL.  `owner` and `repo` are populated for the normal
/// GitHub `owner/repository` shape and left `None` for other providers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub url: String,
    pub transport: Transport,
    pub host: Option<String>,
    pub owner: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Https,
    Ssh,
    Git,
    File,
    Other(String),
}

impl Transport {
    pub fn is_ssh(&self) -> bool {
        matches!(self, Self::Ssh)
    }

    pub fn is_https(&self) -> bool {
        matches!(self, Self::Https)
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Https => "https",
            Self::Ssh => "ssh",
            Self::Git => "git",
            Self::File => "file",
            Self::Other(value) => value.as_str(),
        }
    }
}

/// A remote URL selected for an operation. Fetch and push URLs deliberately
/// remain distinct: `remote.<name>.pushurl` only applies to pushes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationRemote {
    /// One known remote will be contacted.
    Resolved(Remote),
    /// The operation will contact every listed remote (for example, `remote
    /// update` without a name). Callers must apply policy to every target.
    Multiple(Vec<Remote>),
    /// Git will choose a target that ghis cannot establish safely from the
    /// command line and local configuration.
    Uncertain,
}

impl OperationRemote {
    pub fn remotes(&self) -> &[Remote] {
        match self {
            Self::Resolved(remote) => std::slice::from_ref(remote),
            Self::Multiple(remotes) => remotes,
            Self::Uncertain => &[],
        }
    }

    pub fn is_uncertain(&self) -> bool {
        matches!(self, Self::Uncertain)
    }
}

/// Resolve the remote Git will contact for a network operation without
/// changing the existing [`primary_remote`] or [`gh_remote_context`] behavior.
///
/// `pushurl` is used only for pushes. Fetch, pull and `remote update` use the
/// ordinary fetch URL. When Git's target cannot be determined unambiguously,
/// callers receive [`OperationRemote::Uncertain`] rather than an arbitrary
/// primary remote.
pub fn operation_remote(
    repository: &Repository,
    operation: &str,
    arguments: &[String],
) -> Result<OperationRemote> {
    let direction = match operation {
        "push" => RemoteDirection::Push,
        "fetch" | "pull" | "ls-remote" | "remote" => RemoteDirection::Fetch,
        _ => return Ok(OperationRemote::Uncertain),
    };
    let configured = operation_remotes(repository, direction)?;
    if operation == "remote" {
        return operation_remote_update(arguments, &configured);
    }
    if operation == "fetch" && operation_has_flag(arguments, operation, "--all") {
        return Ok(if configured.is_empty() {
            OperationRemote::Uncertain
        } else {
            OperationRemote::Multiple(configured)
        });
    }

    let positionals = operation_positionals(arguments, operation);
    let Some(positionals) = positionals else {
        return Ok(OperationRemote::Uncertain);
    };
    if operation == "fetch" && operation_has_flag(arguments, operation, "--multiple") {
        return Ok(select_operation_remote_targets(&positionals, &configured));
    }
    if let Some(target) = positionals.first() {
        return select_explicit_operation_remote(repository, direction, target, &configured);
    }
    Ok(default_operation_remote(repository, operation, &configured))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteDirection {
    Fetch,
    Push,
}

/// Obtain Git's effective operation URLs. Asking Git through `remote get-url`
/// applies `insteadOf`/`pushInsteadOf` rewrites and preserves every configured
/// value, including multiple push URLs.
fn operation_remotes(repository: &Repository, direction: RemoteDirection) -> Result<Vec<Remote>> {
    let names = run_git(repository.command_dir(), ["remote"])?;
    if !names.status.success() {
        return Err(command_error("remote", &names));
    }
    let mut remotes = Vec::new();
    for name in String::from_utf8_lossy(&names.stdout)
        .lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let mut args = vec!["remote".to_owned(), "get-url".to_owned()];
        if matches!(direction, RemoteDirection::Push) {
            args.push("--push".to_owned());
        }
        args.extend(["--all".to_owned(), name.to_owned()]);
        let output = run_git(repository.command_dir(), args)?;
        if !output.status.success() {
            return Err(command_error("remote get-url", &output));
        }
        for url in String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        {
            remotes.push(parse_remote(name.to_owned(), url.to_owned()));
        }
    }
    Ok(remotes)
}

fn select_named_operation_remotes(name: &str, remotes: &[Remote]) -> OperationRemote {
    let selected = remotes
        .iter()
        .filter(|remote| remote.name == name)
        .cloned()
        .collect::<Vec<_>>();
    match selected.as_slice() {
        [] => OperationRemote::Uncertain,
        [remote] => OperationRemote::Resolved(remote.clone()),
        _ => OperationRemote::Multiple(selected),
    }
}

fn operation_remote_update(arguments: &[String], remotes: &[Remote]) -> Result<OperationRemote> {
    let Some(positionals) = operation_positionals(arguments, "remote") else {
        return Ok(OperationRemote::Uncertain);
    };
    let Some(action) = positionals.first().map(String::as_str) else {
        return Ok(OperationRemote::Uncertain);
    };
    if !matches!(action, "update" | "prune") {
        return Ok(OperationRemote::Uncertain);
    }
    let names = &positionals[1..];
    if names.is_empty() {
        return Ok(if remotes.is_empty() {
            OperationRemote::Uncertain
        } else {
            OperationRemote::Multiple(remotes.to_vec())
        });
    }
    let selected = names
        .iter()
        .map(|name| match select_named_operation_remotes(name, remotes) {
            OperationRemote::Resolved(remote) => vec![remote],
            OperationRemote::Multiple(remotes) => remotes,
            OperationRemote::Uncertain => Vec::new(),
        })
        .collect::<Vec<_>>();
    if selected.iter().any(Vec::is_empty) {
        return Ok(OperationRemote::Uncertain);
    }
    let selected = selected.into_iter().flatten().collect::<Vec<_>>();
    Ok(match selected.as_slice() {
        [] => OperationRemote::Uncertain,
        [remote] => OperationRemote::Resolved(remote.clone()),
        _ => OperationRemote::Multiple(selected),
    })
}

fn select_explicit_operation_remote(
    repository: &Repository,
    direction: RemoteDirection,
    target: &str,
    remotes: &[Remote],
) -> Result<OperationRemote> {
    let selected = select_named_operation_remotes(target, remotes);
    if !selected.is_uncertain() {
        return Ok(selected);
    }
    // Git also accepts a URL in place of a remote name. Resolve rewrite rules
    // in an isolated repository so the result matches Git's actual target.
    let effective = effective_explicit_url(repository, direction, target)?;
    let remote = parse_remote("<explicit>", effective);
    if remote.host.is_some() && !matches!(remote.transport, Transport::File | Transport::Other(_)) {
        Ok(OperationRemote::Resolved(remote))
    } else {
        Ok(OperationRemote::Uncertain)
    }
}

fn effective_explicit_url(
    repository: &Repository,
    direction: RemoteDirection,
    target: &str,
) -> Result<String> {
    let rules = run_git(
        repository.command_dir(),
        [
            "config",
            "--includes",
            "--null",
            "--get-regexp",
            r"^url\..*\.(insteadof|pushinsteadof)$",
        ],
    )?;
    if !rules.status.success() && rules.status.code() != Some(1) {
        return Err(command_error("config URL rewrites", &rules));
    }
    let scratch = tempdir()?;
    let init = Command::new("git")
        .args(["init", "--bare", "-q"])
        .current_dir(scratch.path())
        .output()?;
    if !init.status.success() {
        return Err(command_error("init rewrite scratch repository", &init));
    }
    let empty_global = scratch.path().join("empty-global");
    std::fs::File::create(&empty_global)?;
    for record in String::from_utf8_lossy(&rules.stdout).split('\0') {
        let Some((key, value)) = record.split_once('\n') else {
            continue;
        };
        let output = Command::new("git")
            .args(["config", "--local", "--add", key, value])
            .current_dir(scratch.path())
            .output()?;
        if !output.status.success() {
            return Err(command_error("configure URL rewrite", &output));
        }
    }
    let add = Command::new("git")
        .args(["remote", "add", "synthetic", target])
        .current_dir(scratch.path())
        .output()?;
    if !add.status.success() {
        return Err(command_error("configure explicit remote", &add));
    }
    let mut get_args = vec!["remote", "get-url"];
    if matches!(direction, RemoteDirection::Push) {
        get_args.push("--push");
    }
    get_args.push("synthetic");
    let output = Command::new("git")
        .args(get_args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &empty_global)
        .current_dir(scratch.path())
        .output()?;
    if !output.status.success() {
        return Err(command_error("resolve explicit remote URL", &output));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn default_operation_remote(
    repository: &Repository,
    operation: &str,
    remotes: &[Remote],
) -> OperationRemote {
    let branch = current_branch(repository);
    let branch_push_default = (operation == "push")
        .then(|| {
            branch.as_deref().and_then(|branch| {
                git_config_value(repository, &format!("branch.{branch}.pushRemote"))
            })
        })
        .flatten();
    let configured_default = if operation == "push" {
        git_config_value(repository, "remote.pushDefault")
    } else {
        None
    };
    let branch_default = branch
        .as_deref()
        .and_then(|branch| git_config_value(repository, &format!("branch.{branch}.remote")));
    let origin = String::from("origin");
    for name in branch_push_default
        .iter()
        .chain(configured_default.iter())
        .chain(branch_default.iter())
        .chain(std::iter::once(&origin))
    {
        let selected = select_named_operation_remotes(name, remotes);
        if !selected.is_uncertain() {
            return selected;
        }
    }
    match remotes {
        [remote] => OperationRemote::Resolved(remote.clone()),
        _ => OperationRemote::Uncertain,
    }
}

fn git_config_value(repository: &Repository, key: &str) -> Option<String> {
    let output = run_git(repository.command_dir(), ["config", "--get", key]).ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn current_branch(repository: &Repository) -> Option<String> {
    let output = run_git(
        repository.command_dir(),
        ["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn select_operation_remote_targets(targets: &[String], remotes: &[Remote]) -> OperationRemote {
    let selected = targets
        .iter()
        .map(
            |target| match select_named_operation_remotes(target, remotes) {
                OperationRemote::Resolved(remote) => vec![remote],
                OperationRemote::Multiple(remotes) => remotes,
                OperationRemote::Uncertain => Vec::new(),
            },
        )
        .collect::<Vec<_>>();
    if selected.iter().any(Vec::is_empty) {
        return OperationRemote::Uncertain;
    }
    let selected = selected.into_iter().flatten().collect::<Vec<_>>();
    match selected.as_slice() {
        [] => OperationRemote::Uncertain,
        [remote] => OperationRemote::Resolved(remote.clone()),
        _ => OperationRemote::Multiple(selected),
    }
}

fn operation_has_flag(arguments: &[String], operation: &str, flag: &str) -> bool {
    let Some(start) = arguments.iter().position(|argument| argument == operation) else {
        return false;
    };
    arguments[start + 1..]
        .iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| argument == flag)
}

/// Extract positionals after a network subcommand. The parser is intentionally
/// conservative: known options that consume values are skipped, while an
/// unfamiliar option makes the remaining target uncertain instead of guessing.
fn operation_positionals(arguments: &[String], operation: &str) -> Option<Vec<String>> {
    let start = arguments
        .iter()
        .position(|argument| argument == operation)?;
    let mut result = Vec::new();
    let mut index = start + 1;
    let mut positional_only = false;
    while let Some(argument) = arguments.get(index) {
        if positional_only {
            result.push(argument.clone());
            index += 1;
            continue;
        }
        if argument == "--" {
            positional_only = true;
            index += 1;
            continue;
        }
        if argument.starts_with('-') && argument != "-" {
            if !operation_option_is_known(operation, argument) {
                return None;
            }
            if operation_option_takes_value(operation, argument) && !argument.contains('=') {
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }
        result.push(argument.clone());
        index += 1;
    }
    Some(result)
}

fn operation_option_is_known(operation: &str, option: &str) -> bool {
    let option = option.split('=').next().unwrap_or(option);
    matches!(
        option,
        "-q" | "--quiet"
            | "-v"
            | "--verbose"
            | "--all"
            | "--prune"
            | "--prune-tags"
            | "--tags"
            | "--no-tags"
            | "--dry-run"
            | "--force"
            | "-f"
            | "--set-upstream"
            | "-u"
            | "--mirror"
            | "--porcelain"
            | "--atomic"
            | "--follow-tags"
            | "--no-verify"
            | "--ipv4"
            | "--ipv6"
            | "--recurse-submodules"
            | "--no-recurse-submodules"
            | "--multiple"
            | "--append"
            | "--no-write-fetch-head"
            | "--write-commit-graph"
            | "--update-head-ok"
            | "--negotiate-only"
            | "--prefetch"
            | "--show-forced-updates"
            | "--no-show-forced-updates"
            | "--keep"
            | "--progress"
            | "--no-progress"
    ) || matches!(
        option,
        "--receive-pack"
            | "--exec"
            | "--push-option"
            | "-o"
            | "--upload-pack"
            | "--depth"
            | "--deepen"
            | "--shallow-since"
            | "--shallow-exclude"
            | "--server-option"
            | "--jobs"
            | "-j"
            | "--negotiation-tip"
            | "--refmap"
            | "--filter"
    ) || (operation == "pull"
        && matches!(
            option,
            "--rebase" | "--strategy" | "-s" | "--strategy-option" | "-X"
        ))
}

fn operation_option_takes_value(operation: &str, option: &str) -> bool {
    let option = option.split('=').next().unwrap_or(option);
    matches!(
        option,
        "--receive-pack"
            | "--exec"
            | "--push-option"
            | "-o"
            | "--upload-pack"
            | "--depth"
            | "--deepen"
            | "--shallow-since"
            | "--shallow-exclude"
            | "--server-option"
            | "--jobs"
            | "-j"
            | "--negotiation-tip"
            | "--refmap"
            | "--filter"
    ) || (operation == "pull" && matches!(option, "--strategy" | "-s" | "--strategy-option" | "-X"))
}

/// Read all configured remotes.  A remote's push URL wins over its fetch URL
/// because identity and credential selection are used for push operations.
pub fn remotes(repository: &Repository) -> Result<Vec<Remote>> {
    let out = run_git(
        repository.command_dir(),
        ["config", "--get-regexp", r"^remote\..*\.(url|pushurl)$"],
    )?;
    if !out.status.success() {
        // `--get-regexp` exits 1 when there are no remotes.
        if out.status.code() == Some(1) {
            return Ok(Vec::new());
        }
        return Err(command_error("config --get-regexp remote", &out));
    }

    let mut values: BTreeMap<String, (Option<String>, Option<String>)> = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((key, value)) = line.split_once('\t').or_else(|| line.split_once(' ')) else {
            continue;
        };
        let mut parts = key.split('.');
        if parts.next() != Some("remote") {
            continue;
        }
        let Some(name) = parts.next() else { continue };
        let Some(kind) = parts.next() else { continue };
        let entry = values.entry(name.to_owned()).or_default();
        match kind {
            "url" if entry.0.is_none() => entry.0 = Some(value.trim().to_owned()),
            "pushurl" if entry.1.is_none() => entry.1 = Some(value.trim().to_owned()),
            _ => {}
        }
    }

    Ok(values
        .into_iter()
        .filter_map(|(name, (fetch, push))| {
            let url = push.or(fetch)?;
            Some(parse_remote(name, url))
        })
        .collect())
}

pub fn primary_remote(repository: &Repository) -> Result<Option<Remote>> {
    let remotes = remotes(repository)?;
    Ok(remotes
        .iter()
        .find(|remote| remote.name == "origin")
        .cloned()
        .or_else(|| remotes.into_iter().next()))
}

/// Result of selecting the fetch remote that `gh` should use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhRemoteContext {
    /// The preferred remote on the requested Profile/`GH_HOST`.
    Matched(Remote),
    /// Fetch remotes exist, but none target the requested host. Callers use
    /// this value to reject implicit cross-host repository operations.
    HostMismatch(Remote),
    /// The repository has no fetch remote with a network host.
    NoRemote,
}

impl GhRemoteContext {
    pub fn remote(&self) -> Option<&Remote> {
        match self {
            Self::Matched(remote) | Self::HostMismatch(remote) => Some(remote),
            Self::NoRemote => None,
        }
    }
}

/// Select the fetch remote context that `gh` should use for `target_host`.
///
/// This deliberately does not reuse [`remotes`]: Git operations care about a
/// remote's push URL, while `gh` 2.97 discovers repository context from the
/// fetch entries printed by `git remote -v`. Candidate remotes are restricted
/// to the host selected through the Profile/`GH_HOST`, then ranked using gh's
/// conventional remote names before falling back to a deterministic name
/// order.
pub fn gh_remote_context(repository: &Repository, target_host: &str) -> Result<GhRemoteContext> {
    let out = run_git(repository.command_dir(), ["remote", "-v"])?;
    if !out.status.success() {
        return Err(command_error("remote -v", &out));
    }

    let target_host = crate::github::normalize_host(target_host);
    let mut candidates = BTreeMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((name, value)) = line.split_once('\t') else {
            continue;
        };
        let Some(url) = value.strip_suffix(" (fetch)") else {
            continue;
        };
        let remote = parse_remote(name, url);
        candidates.entry(remote.name.clone()).or_insert(remote);
    }

    let mut candidates = candidates.into_values().collect::<Vec<_>>();
    candidates.sort_by_key(|remote| {
        let priority = match remote.name.as_str() {
            "upstream" => 0,
            "github" => 1,
            "origin" => 2,
            _ => 3,
        };
        (priority, remote.name.clone())
    });
    if let Some(remote) = candidates.iter().find(|remote| {
        remote
            .host
            .as_deref()
            .is_some_and(|host| crate::github::normalize_host(host) == target_host)
    }) {
        return Ok(GhRemoteContext::Matched(remote.clone()));
    }
    Ok(candidates
        .into_iter()
        .find(|remote| remote.host.is_some())
        .map_or(GhRemoteContext::NoRemote, GhRemoteContext::HostMismatch))
}

const LOCAL_CONFIG_SCOPE: &str = "--local";
const WORKTREE_CONFIG_SCOPE: &str = "--worktree";

/// Read a repository-local setting. A linked worktree uses its own
/// `config.worktree` once `extensions.worktreeConfig` is enabled. It never
/// falls back to the common config for a linked worktree because such a value
/// may belong to another worktree. A missing key is represented by `None`.
pub fn local_config(repository: &Repository, key: &str) -> Result<Option<String>> {
    let Some(scope) = config_read_scope(repository)? else {
        return Ok(None);
    };
    let out = run_git(repository.command_dir(), ["config", scope, "--get", key])?;
    if out.status.success() {
        return Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim_end().to_owned(),
        ));
    }
    if out.status.code() == Some(1) {
        return Ok(None);
    }
    Err(command_error(format!("config {scope} --get {key}"), &out))
}

/// Read every value for one repository-local key in Git's stored order.
/// Empty values are preserved because an empty credential helper resets the
/// inherited helper chain and is therefore security-relevant.
pub fn local_config_values(repository: &Repository, key: &str) -> Result<Vec<String>> {
    let Some(scope) = config_read_scope(repository)? else {
        return Ok(Vec::new());
    };
    let out = run_git(
        repository.command_dir(),
        ["config", scope, "--null", "--get-all", key],
    )?;
    if out.status.success() {
        let mut values = out
            .stdout
            .split(|byte| *byte == 0)
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect::<Vec<_>>();
        if values.last().is_some_and(String::is_empty) {
            values.pop();
        }
        return Ok(values);
    }
    if out.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    Err(command_error(
        format!("config {scope} --get-all {key}"),
        &out,
    ))
}

/// Return repository-local config keys matching a Git config regular
/// expression. Values are deliberately not returned so callers cannot
/// accidentally include credentials in diagnostics.
pub fn local_config_keys_matching(repository: &Repository, pattern: &str) -> Result<Vec<String>> {
    let Some(scope) = config_read_scope(repository)? else {
        return Ok(Vec::new());
    };
    let out = run_git(
        repository.command_dir(),
        ["config", scope, "--name-only", "--get-regexp", pattern],
    )?;
    if out.status.success() {
        let mut keys = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned())
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        return Ok(keys);
    }
    if out.status.code() == Some(1) {
        return Ok(Vec::new());
    }
    Err(command_error(
        format!("config {scope} --name-only --get-regexp"),
        &out,
    ))
}

/// Write a repository-local setting. Linked worktrees are deliberately
/// isolated from the common repository config.
pub fn set_local_config(repository: &Repository, key: &str, value: &str) -> Result<()> {
    let scope = config_write_scope(repository)?;
    let out = run_git(repository.command_dir(), ["config", scope, key, value])?;
    if out.status.success() {
        Ok(())
    } else {
        Err(command_error(format!("config {scope} {key}"), &out))
    }
}

/// Append a repository-local setting using the same linked-worktree isolation
/// as [`set_local_config`].
pub fn add_local_config(repository: &Repository, key: &str, value: &str) -> Result<()> {
    let scope = config_write_scope(repository)?;
    let out = run_git(
        repository.command_dir(),
        ["config", scope, "--add", key, value],
    )?;
    if out.status.success() {
        Ok(())
    } else {
        Err(command_error(format!("config {scope} --add {key}"), &out))
    }
}

pub fn unset_local_config(repository: &Repository, key: &str) -> Result<()> {
    let Some(scope) = config_cleanup_scope(repository)? else {
        return Ok(());
    };
    let out = run_git(
        repository.command_dir(),
        ["config", scope, "--unset-all", key],
    )?;
    if out.status.success() || out.status.code() == Some(5) || out.status.code() == Some(1) {
        Ok(())
    } else {
        Err(command_error(
            format!("config {scope} --unset-all {key}"),
            &out,
        ))
    }
}

/// Remove every repository-local multi-value entry exactly equal to `value`
/// while retaining different values under the same key.
pub fn unset_local_config_value(repository: &Repository, key: &str, value: &str) -> Result<()> {
    let Some(scope) = config_cleanup_scope(repository)? else {
        return Ok(());
    };
    let out = run_git(
        repository.command_dir(),
        ["config", scope, "--fixed-value", "--unset-all", key, value],
    )?;
    if out.status.success() || matches!(out.status.code(), Some(1 | 5)) {
        Ok(())
    } else {
        Err(command_error(
            format!("config {scope} --fixed-value --unset-all {key}"),
            &out,
        ))
    }
}

/// Replace every repository-local value for one key while preserving the
/// caller-provided order. This is primarily used when removing one owned item
/// from an ordered multi-value setting such as `credential.*.helper`.
pub fn replace_local_config_values(
    repository: &Repository,
    key: &str,
    values: &[String],
) -> Result<()> {
    unset_local_config(repository, key)?;
    for value in values {
        add_local_config(repository, key, value)?;
    }
    Ok(())
}

/// Remove a value from the shared repository config. This exists only for
/// cleaning up ghis settings written before linked-worktree isolation was
/// introduced; new linked-worktree values must use the functions above.
pub fn unset_common_config(repository: &Repository, key: &str) -> Result<()> {
    let out = run_git(
        repository.command_dir(),
        ["config", LOCAL_CONFIG_SCOPE, "--unset-all", key],
    )?;
    if out.status.success() || matches!(out.status.code(), Some(1 | 5)) {
        Ok(())
    } else {
        Err(command_error(
            format!("config {LOCAL_CONFIG_SCOPE} --unset-all {key}"),
            &out,
        ))
    }
}

fn config_read_scope(repository: &Repository) -> Result<Option<&'static str>> {
    if repository.bare {
        return Ok(Some(LOCAL_CONFIG_SCOPE));
    }
    if worktree_config_enabled(repository)? {
        return Ok(Some(WORKTREE_CONFIG_SCOPE));
    }
    // Before the extension is enabled, only the main worktree can safely own
    // values from the shared config. A linked worktree must not inherit a
    // marker written for another checkout.
    Ok((!repository.is_worktree()).then_some(LOCAL_CONFIG_SCOPE))
}

fn config_cleanup_scope(repository: &Repository) -> Result<Option<&'static str>> {
    if repository.bare {
        return Ok(Some(LOCAL_CONFIG_SCOPE));
    }
    if worktree_config_enabled(repository)? {
        return Ok(Some(WORKTREE_CONFIG_SCOPE));
    }
    // Do not delete a shared legacy value while operating from a linked
    // worktree; it may belong to the main or another linked checkout.
    Ok((!repository.is_worktree()).then_some(LOCAL_CONFIG_SCOPE))
}

fn config_write_scope(repository: &Repository) -> Result<&'static str> {
    if !repository.bare {
        enable_worktree_config(repository)?;
        Ok(WORKTREE_CONFIG_SCOPE)
    } else {
        Ok(LOCAL_CONFIG_SCOPE)
    }
}

fn worktree_config_enabled(repository: &Repository) -> Result<bool> {
    let out = run_git(
        repository.command_dir(),
        [
            "config",
            LOCAL_CONFIG_SCOPE,
            "--bool",
            "--get",
            "extensions.worktreeConfig",
        ],
    )?;
    if out.status.code() == Some(1) {
        return Ok(false);
    }
    if !out.status.success() {
        return Err(command_error(
            "config --local --bool --get extensions.worktreeConfig",
            &out,
        ));
    }
    match String::from_utf8_lossy(&out.stdout).trim() {
        "true" => Ok(true),
        "false" => Ok(false),
        value => Err(RepoError::InvalidOutput {
            operation: "config --local --bool --get extensions.worktreeConfig".into(),
            output: value.to_owned(),
        }),
    }
}

/// Whether Git has enabled per-worktree configuration for this repository.
/// Bare repositories deliberately remain in the ordinary local scope.
pub fn uses_worktree_config(repository: &Repository) -> Result<bool> {
    if repository.bare {
        Ok(false)
    } else {
        worktree_config_enabled(repository)
    }
}

fn enable_worktree_config(repository: &Repository) -> Result<()> {
    let out = run_git(
        repository.command_dir(),
        [
            "config",
            LOCAL_CONFIG_SCOPE,
            "extensions.worktreeConfig",
            "true",
        ],
    )?;
    if !out.status.success() {
        return Err(command_error(
            "config --local extensions.worktreeConfig",
            &out,
        ));
    }
    if worktree_config_enabled(repository)? {
        Ok(())
    } else {
        Err(RepoError::InvalidOutput {
            operation: "enable extensions.worktreeConfig".into(),
            output: "Git did not report the extension as enabled".into(),
        })
    }
}

/// Parse a remote URL without invoking git.  This is public to make rule
/// matching and tests independent of a real repository.
pub fn parse_remote(name: impl Into<String>, url: impl Into<String>) -> Remote {
    let name = name.into();
    let url = url.into();
    let (transport, host, path) = parse_url_parts(&url);
    let (owner, repo) = github_path_parts(path.as_deref());
    Remote {
        name,
        url,
        transport,
        host,
        owner,
        repo,
    }
}

fn parse_url_parts(url: &str) -> (Transport, Option<String>, Option<String>) {
    if let Some((scheme, rest)) = url.split_once("://") {
        match scheme.to_ascii_lowercase().as_str() {
            "https" => return split_authority_path(rest, Transport::Https),
            "http" => {
                return split_authority_path(rest, Transport::Other("http".into()));
            }
            "ssh" => return split_authority_path(rest, Transport::Ssh),
            "git" => return split_authority_path(rest, Transport::Git),
            "file" => {
                return (
                    Transport::File,
                    None,
                    Some(rest.trim_start_matches('/').to_owned()),
                );
            }
            _ => {}
        }
    }
    // SCP-like syntax: user@host:owner/repo.git.  A Windows drive path does
    // not match because it has no `@` and is therefore treated as `Other`.
    if let Some((left, right)) = split_scp_remote(url)
        && {
            let windows_drive = left.len() == 1 && left.as_bytes()[0].is_ascii_alphabetic();
            !windows_drive && (left.contains('@') || (!left.contains('/') && !left.contains('\\')))
        }
    {
        let host = left.rsplit_once('@').map_or(left, |(_, h)| h);
        return (
            Transport::Ssh,
            Some(crate::github::normalize_host(host)),
            Some(right.to_owned()),
        );
    }
    (
        Transport::Other("unknown".into()),
        None,
        Some(url.to_owned()),
    )
}

fn split_authority_path(
    rest: &str,
    transport: Transport,
) -> (Transport, Option<String>, Option<String>) {
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let path = rest
        .get(authority_end..)
        .unwrap_or_default()
        .strip_prefix('/')
        .unwrap_or_default()
        .split(['?', '#'])
        .next()
        .unwrap_or_default();
    (
        transport,
        (!host.is_empty()).then(|| crate::github::normalize_host(host)),
        Some(path.to_owned()),
    )
}

fn split_scp_remote(url: &str) -> Option<(&str, &str)> {
    if let Some(closing_bracket) = url.find("]:") {
        return Some((&url[..=closing_bracket], &url[closing_bracket + 2..]));
    }
    url.split_once(':')
}

fn github_path_parts(path: Option<&str>) -> (Option<String>, Option<String>) {
    let path = path.unwrap_or_default().trim_matches('/');
    let mut it = path.split('/').filter(|part| !part.is_empty());
    let owner = it.next().map(str::to_owned);
    let repo = it
        .next()
        .map(|value| value.strip_suffix(".git").unwrap_or(value).to_owned());
    (owner, repo)
}

fn resolve_git_path(base: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_owned(), |cwd| cwd.join(path))
    }
}

fn run_git<I, S>(dir: &Path, args: I) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new("git").args(args).current_dir(dir).output()
}

fn clean_stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).trim().to_owned()
}

fn command_error(operation: impl Into<String>, output: &Output) -> RepoError {
    RepoError::Command {
        operation: operation.into(),
        code: output.status.code(),
        stderr: clean_stderr(output),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn parses_common_remote_urls() {
        let https = parse_remote("origin", "https://GitHub.com/acme/tool.git");
        assert_eq!(https.transport, Transport::Https);
        assert_eq!(https.host.as_deref(), Some("github.com"));
        assert_eq!(https.owner.as_deref(), Some("acme"));
        assert_eq!(https.repo.as_deref(), Some("tool"));

        let ssh = parse_remote("origin", "git@github.com:acme/tool.git");
        assert!(ssh.transport.is_ssh());
        assert_eq!(ssh.owner.as_deref(), Some("acme"));
        assert_eq!(ssh.repo.as_deref(), Some("tool"));

        let ssh_url = parse_remote("origin", "ssh://git@github.com/acme/tool");
        assert!(ssh_url.transport.is_ssh());
        assert_eq!(ssh_url.repo.as_deref(), Some("tool"));
    }

    #[test]
    fn unknown_url_does_not_panic() {
        let remote = parse_remote("x", "https://example.test/one");
        assert_eq!(remote.owner.as_deref(), Some("one"));
        assert_eq!(remote.repo, None);
    }

    #[test]
    fn handles_https_userinfo_and_windows_paths_without_guessing_ssh() {
        let remote = parse_remote("origin", "https://token@example.test/acme/tool.git");
        assert_eq!(remote.host.as_deref(), Some("example.test"));
        assert_eq!(remote.owner.as_deref(), Some("acme"));
        let local = parse_remote("origin", "C:\\work\\tool");
        assert!(!local.transport.is_ssh());
    }

    #[test]
    fn preserves_enterprise_ports_and_normalizes_https_443() {
        let default_port = parse_remote(
            "origin",
            "HTTPS://token@Git.Example.Test.:443/acme/tool.git",
        );
        assert_eq!(default_port.transport, Transport::Https);
        assert_eq!(default_port.host.as_deref(), Some("git.example.test"));
        assert_eq!(default_port.owner.as_deref(), Some("acme"));

        let enterprise = parse_remote(
            "origin",
            "https://git.example.test:8443/acme/tool.git?view=1",
        );
        assert_eq!(enterprise.host.as_deref(), Some("git.example.test:8443"));
        assert_eq!(enterprise.repo.as_deref(), Some("tool"));

        let ssh = parse_remote("origin", "ssh://git@git.example.test:2222/acme/tool.git");
        assert_eq!(ssh.host.as_deref(), Some("git.example.test:2222"));
        assert_eq!(ssh.owner.as_deref(), Some("acme"));
    }

    #[test]
    fn parses_bracketed_ipv6_remote_authorities() {
        let https = parse_remote("origin", "https://[2001:DB8::1]:443/acme/tool.git");
        assert_eq!(https.host.as_deref(), Some("[2001:db8::1]"));

        let ssh = parse_remote("origin", "ssh://git@[2001:DB8::1]:2222/acme/tool.git");
        assert_eq!(ssh.host.as_deref(), Some("[2001:db8::1]:2222"));

        let scp = parse_remote("origin", "git@[2001:DB8::1]:acme/tool.git");
        assert!(scp.transport.is_ssh());
        assert_eq!(scp.host.as_deref(), Some("[2001:db8::1]"));
        assert_eq!(scp.owner.as_deref(), Some("acme"));
    }

    #[test]
    fn discovers_repository_from_a_subdirectory() {
        let directory = tempfile::tempdir().unwrap();
        let repository_path = directory.path().join("repository");
        fs::create_dir_all(repository_path.join("nested")).unwrap();
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repository_path)
            .output()
            .unwrap();
        assert!(init.status.success());
        let repository = discover(repository_path.join("nested")).unwrap();
        assert_eq!(repository.root.as_deref(), Some(repository_path.as_path()));
        assert_eq!(repository.git_dir, repository_path.join(".git"));
    }

    #[test]
    fn discovers_bare_repository() {
        let directory = tempfile::tempdir().unwrap();
        let bare = directory.path().join("remote.git");
        let init = Command::new("git")
            .args(["init", "-q", "--bare"])
            .arg(&bare)
            .output()
            .unwrap();
        assert!(init.status.success());
        let repository = discover(&bare).unwrap();
        assert!(repository.bare);
        assert_eq!(repository.root, None);
        assert_eq!(repository.git_dir, bare);
    }

    #[test]
    fn ordered_local_config_values_preserve_empty_and_multiline_entries() {
        let directory = tempfile::tempdir().unwrap();
        let repository_path = directory.path().join("repository");
        fs::create_dir_all(&repository_path).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repository_path)
                .status()
                .unwrap()
                .success()
        );
        let repository = discover(&repository_path).unwrap();
        let key = "credential.https://github.com.helper";
        let initial = vec![String::new(), "!line one\nline two".into(), String::new()];
        for value in &initial {
            add_local_config(&repository, key, value).unwrap();
        }
        assert_eq!(local_config_values(&repository, key).unwrap(), initial);

        let replacement = vec!["!first".into(), String::new(), "!last\nline".into()];
        replace_local_config_values(&repository, key, &replacement).unwrap();
        assert_eq!(local_config_values(&repository, key).unwrap(), replacement);
    }

    #[test]
    fn gh_remote_uses_fetch_url_host_and_gh_name_priority() {
        let directory = tempfile::tempdir().unwrap();
        let repository_path = directory.path().join("repository");
        fs::create_dir_all(&repository_path).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repository_path)
                .status()
                .unwrap()
                .success()
        );
        let configure = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&repository_path)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        configure(&[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/origin.git",
        ]);
        configure(&[
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://enterprise.example/acme/push.git",
        ]);
        configure(&[
            "remote",
            "add",
            "github",
            "https://github.com/acme/github.git",
        ]);
        configure(&[
            "remote",
            "add",
            "upstream",
            "https://enterprise.example/acme/upstream.git",
        ]);
        configure(&["remote", "add", "zeta", "https://github.com/acme/zeta.git"]);
        configure(&[
            "remote",
            "add",
            "alpha",
            "https://github.com/acme/alpha.git",
        ]);

        let repository = discover(&repository_path).unwrap();
        let push_remote = primary_remote(&repository).unwrap().unwrap();
        assert_eq!(push_remote.host.as_deref(), Some("enterprise.example"));
        assert_eq!(push_remote.repo.as_deref(), Some("push"));

        let GhRemoteContext::Matched(github) =
            gh_remote_context(&repository, "GitHub.com.:443").unwrap()
        else {
            panic!("expected a matching GitHub remote");
        };
        assert_eq!(github.name, "github");
        assert_eq!(github.repo.as_deref(), Some("github"));

        let GhRemoteContext::Matched(enterprise) =
            gh_remote_context(&repository, "enterprise.example").unwrap()
        else {
            panic!("expected a matching Enterprise remote");
        };
        assert_eq!(enterprise.name, "upstream");
        assert_eq!(enterprise.repo.as_deref(), Some("upstream"));

        configure(&["remote", "remove", "github"]);
        let GhRemoteContext::Matched(origin) =
            gh_remote_context(&repository, "github.com").unwrap()
        else {
            panic!("expected origin to match");
        };
        assert_eq!(origin.name, "origin");

        configure(&["remote", "remove", "origin"]);
        let GhRemoteContext::Matched(fallback) =
            gh_remote_context(&repository, "github.com").unwrap()
        else {
            panic!("expected a deterministic fallback");
        };
        assert_eq!(fallback.name, "alpha");

        let GhRemoteContext::HostMismatch(mismatch) =
            gh_remote_context(&repository, "missing.example").unwrap()
        else {
            panic!("expected the preferred mismatched remote");
        };
        assert_eq!(mismatch.name, "upstream");
    }

    #[test]
    fn operation_remote_separates_pushurl_fetch_and_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repository");
        fs::create_dir_all(&path).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&path)
                .status()
                .unwrap()
                .success()
        );
        let configure = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&path)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        configure(&[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/project.git",
        ]);
        configure(&[
            "remote",
            "set-url",
            "--push",
            "origin",
            "git@github.com:acme/project.git",
        ]);
        configure(&[
            "remote",
            "add",
            "backup",
            "ssh://git@example.test/acme/backup.git",
        ]);
        configure(&["config", "remote.pushDefault", "backup"]);
        let repository = discover(&path).unwrap();

        let OperationRemote::Resolved(push) =
            operation_remote(&repository, "push", &["push".into()]).unwrap()
        else {
            panic!("expected a default push remote");
        };
        assert_eq!(push.name, "backup");
        assert!(push.transport.is_ssh());

        let OperationRemote::Resolved(fetch) =
            operation_remote(&repository, "fetch", &["fetch".into(), "origin".into()]).unwrap()
        else {
            panic!("expected origin fetch remote");
        };
        assert_eq!(fetch.transport, Transport::Https);
        assert_eq!(fetch.url, "https://github.com/acme/project.git");

        let OperationRemote::Resolved(push_origin) =
            operation_remote(&repository, "push", &["push".into(), "origin".into()]).unwrap()
        else {
            panic!("expected origin push remote");
        };
        assert!(push_origin.transport.is_ssh());
        assert_eq!(push_origin.url, "git@github.com:acme/project.git");
    }

    #[test]
    fn operation_remote_handles_remote_update_and_unknown_targets_conservatively() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repository");
        fs::create_dir_all(&path).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&path)
                .status()
                .unwrap()
                .success()
        );
        for (name, url) in [
            ("origin", "https://github.com/acme/project.git"),
            ("backup", "git@github.com:acme/backup.git"),
        ] {
            assert!(
                Command::new("git")
                    .args(["remote", "add", name, url])
                    .current_dir(&path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let repository = discover(&path).unwrap();
        let OperationRemote::Multiple(remotes) =
            operation_remote(&repository, "remote", &["remote".into(), "update".into()]).unwrap()
        else {
            panic!("expected every remote for remote update");
        };
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().any(|remote| remote.transport.is_ssh()));
        assert!(
            operation_remote(
                &repository,
                "fetch",
                &["fetch".into(), "not-a-configured-remote".into()],
            )
            .unwrap()
            .is_uncertain()
        );
        let OperationRemote::Multiple(remotes) =
            operation_remote(&repository, "fetch", &["fetch".into(), "--all".into()]).unwrap()
        else {
            panic!("expected every remote for fetch --all");
        };
        assert_eq!(remotes.len(), 2);
        assert!(remotes.iter().any(|remote| remote.transport.is_ssh()));
        assert!(
            operation_remote(
                &repository,
                "fetch",
                &["fetch".into(), "--unrecognized-option".into()],
            )
            .unwrap()
            .is_uncertain()
        );
    }

    #[test]
    fn operation_remote_uses_effective_rewrites_and_all_push_urls() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("repository");
        fs::create_dir_all(&path).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q"])
                .current_dir(&path)
                .status()
                .unwrap()
                .success()
        );
        let configure = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&path)
                    .status()
                    .unwrap()
                    .success(),
                "git {args:?} failed"
            );
        };
        configure(&[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/project.git",
        ]);
        configure(&[
            "config",
            "url.git@github.com:.insteadOf",
            "https://github.com/",
        ]);
        let repository = discover(&path).unwrap();
        let OperationRemote::Resolved(fetch) =
            operation_remote(&repository, "fetch", &["fetch".into(), "origin".into()]).unwrap()
        else {
            panic!("expected rewritten fetch remote");
        };
        assert!(fetch.transport.is_ssh());
        assert_eq!(fetch.url, "git@github.com:acme/project.git");

        configure(&[
            "remote",
            "set-url",
            "--push",
            "origin",
            "https://github.com/acme/project.git",
        ]);
        configure(&[
            "remote",
            "set-url",
            "--add",
            "--push",
            "origin",
            "git@github.com:acme/project.git",
        ]);
        let OperationRemote::Multiple(pushes) =
            operation_remote(&repository, "push", &["push".into(), "origin".into()]).unwrap()
        else {
            panic!("expected every push URL");
        };
        assert_eq!(pushes.len(), 2);
        assert!(pushes.iter().any(|remote| remote.transport.is_ssh()));
    }
}
