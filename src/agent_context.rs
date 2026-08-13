//! Stable, redacted repository identity context for coding agents.
//!
//! This module deliberately projects an [`AppContext`] through an explicit
//! allowlist. Adding fields to `AppContext` therefore cannot silently expose
//! configuration, credentials, diagnostics, remote URLs, or SSH material.

use crate::agent::{AgentKind, ContextProvider};
use crate::app::AppContext;
use crate::config::ResolutionSource;
use serde::Serialize;
use std::fmt::Write as _;

pub const AGENT_CONTEXT_SCHEMA_VERSION: u32 = 1;
pub const AGENT_CONTEXT_CONTRACT: [&str; 3] = [
    "Use ordinary git and gh from PATH.",
    "Do not run gh auth switch.",
    "Do not output or persist tokens, keys, or other credentials.",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentContext {
    pub schema_version: u32,
    pub repository: AgentRepository,
    pub selection: AgentSelection,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity: Option<AgentIdentity>,
    pub contract: [&'static str; 3],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentRepository {
    pub state: RepositoryState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<RepositoryTransport>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryState {
    Git,
    Bare,
    NotRepository,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryTransport {
    Https,
    Ssh,
    Git,
    File,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentSelection {
    pub state: SelectionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub source: SelectionSource,
    pub candidates: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionState {
    Resolved,
    Unresolved,
    Ambiguous,
    InvalidExplicit,
    InvalidRepositoryBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SelectionSource {
    pub kind: SelectionSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionSourceKind {
    Explicit,
    RepositoryBinding,
    Rule,
    GithubLogin,
    Default,
    Unresolved,
    Ambiguous,
    InvalidExplicit,
    InvalidRepositoryBinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentIdentity {
    pub github_host: String,
    pub github_login: String,
    pub git_name: String,
    pub git_email: String,
    pub signing_enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentContextFormat {
    Json,
    Claude,
    Codex,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentContextError {
    #[error("agent context requires a resolved profile (selection state: {state})")]
    Unresolved { state: &'static str },
    #[error("could not render agent context: {0}")]
    Json(#[from] serde_json::Error),
}

impl AgentContext {
    pub fn from_app(context: &AppContext) -> Self {
        let repository = match context.repository.as_ref() {
            None => AgentRepository {
                state: RepositoryState::NotRepository,
                root: None,
                owner: None,
                name: None,
                transport: None,
            },
            Some(repository) => AgentRepository {
                state: if repository.bare {
                    RepositoryState::Bare
                } else {
                    RepositoryState::Git
                },
                root: Some(repository.command_dir().display().to_string()),
                owner: context
                    .remote
                    .as_ref()
                    .and_then(|remote| remote.owner.clone()),
                name: context
                    .remote
                    .as_ref()
                    .and_then(|remote| remote.repo.clone()),
                transport: context.remote.as_ref().map(|remote| {
                    use crate::repo::Transport;
                    match remote.transport {
                        Transport::Https => RepositoryTransport::Https,
                        Transport::Ssh => RepositoryTransport::Ssh,
                        Transport::Git => RepositoryTransport::Git,
                        Transport::File => RepositoryTransport::File,
                        Transport::Other(_) => RepositoryTransport::Other,
                    }
                }),
            },
        };
        let source = SelectionSource::from(&context.resolution.source);
        let state = SelectionState::from(&context.resolution.source);
        let identity = context.profile.as_ref().map(|profile| AgentIdentity {
            github_host: profile.host.clone(),
            github_login: profile.login.clone(),
            git_name: profile.git_name.clone(),
            git_email: profile.git_email.clone(),
            signing_enabled: profile.signing.enabled,
        });
        Self {
            schema_version: AGENT_CONTEXT_SCHEMA_VERSION,
            repository,
            selection: AgentSelection {
                state,
                profile: context.resolution.profile.clone(),
                source,
                candidates: context.resolution.candidates.clone(),
            },
            identity,
            contract: AGENT_CONTEXT_CONTRACT,
        }
    }

    pub fn require_resolved(&self) -> Result<(), AgentContextError> {
        if self.selection.state == SelectionState::Resolved {
            Ok(())
        } else {
            Err(AgentContextError::Unresolved {
                state: self.selection.state.name(),
            })
        }
    }

    pub fn render(&self, format: AgentContextFormat) -> Result<String, AgentContextError> {
        match format {
            AgentContextFormat::Json => Ok(serde_json::to_string_pretty(self)?),
            AgentContextFormat::Claude => Ok(self.render_text()),
            AgentContextFormat::Codex => Ok(self.render_text()),
        }
    }

    pub fn render_json(&self) -> Result<String, AgentContextError> {
        self.render(AgentContextFormat::Json)
    }

    pub fn render_claude(&self) -> String {
        self.render_text()
    }

    pub fn render_codex(&self) -> String {
        self.render_text()
    }

    fn render_text(&self) -> String {
        let mut output = String::from("ghis session context (schema v1)\n");
        writeln!(
            output,
            "repository: state={} root={} owner={} name={} transport={}",
            self.repository.state.name(),
            self.repository.root.as_deref().unwrap_or("none"),
            self.repository.owner.as_deref().unwrap_or("none"),
            self.repository.name.as_deref().unwrap_or("none"),
            self.repository
                .transport
                .map_or("none", RepositoryTransport::name),
        )
        .expect("write to string");
        writeln!(
            output,
            "selection: state={} profile={} source={} candidates={}",
            self.selection.state.name(),
            self.selection.profile.as_deref().unwrap_or("none"),
            self.selection.source.display(),
            display_candidates(&self.selection.candidates),
        )
        .expect("write to string");
        if self.selection.state == SelectionState::Resolved
            && self.selection.source.kind == SelectionSourceKind::Default
        {
            writeln!(
                output,
                "context note: the selected profile comes from ghis default_profile, not a repository binding; after entering a repository, use the repository-specific ghis context."
            )
            .expect("write to string");
        }
        if let Some(identity) = &self.identity {
            writeln!(
                output,
                "identity: {}/{}; {} <{}>; signing={}",
                identity.github_host,
                identity.github_login,
                identity.git_name,
                identity.git_email,
                if identity.signing_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
            )
            .expect("write to string");
        } else {
            writeln!(output, "identity: none").expect("write to string");
        }
        for item in self.contract {
            writeln!(output, "contract: {item}").expect("write to string");
        }
        output.trim_end().to_owned()
    }
}

impl ContextProvider for AgentContext {
    type Error = AgentContextError;

    fn require_resolved(&self) -> Result<(), Self::Error> {
        AgentContext::require_resolved(self)
    }

    fn render_for(&self, kind: AgentKind) -> Result<String, Self::Error> {
        Ok(match kind {
            AgentKind::Claude => self.render_claude(),
            AgentKind::Codex => self.render_codex(),
        })
    }
}

impl From<&ResolutionSource> for SelectionSource {
    fn from(source: &ResolutionSource) -> Self {
        match source {
            ResolutionSource::Explicit => Self::new(SelectionSourceKind::Explicit),
            ResolutionSource::InvalidExplicit => Self::new(SelectionSourceKind::InvalidExplicit),
            ResolutionSource::RepositoryBinding => {
                Self::new(SelectionSourceKind::RepositoryBinding)
            }
            ResolutionSource::InvalidRepositoryBinding => {
                Self::new(SelectionSourceKind::InvalidRepositoryBinding)
            }
            ResolutionSource::Rule { id, priority } => Self {
                kind: SelectionSourceKind::Rule,
                rule_id: Some(id.clone()),
                priority: Some(*priority),
            },
            ResolutionSource::GithubLogin => Self::new(SelectionSourceKind::GithubLogin),
            ResolutionSource::Default => Self::new(SelectionSourceKind::Default),
            ResolutionSource::Unresolved => Self::new(SelectionSourceKind::Unresolved),
            ResolutionSource::Ambiguous => Self::new(SelectionSourceKind::Ambiguous),
        }
    }
}

impl SelectionSource {
    fn new(kind: SelectionSourceKind) -> Self {
        Self {
            kind,
            rule_id: None,
            priority: None,
        }
    }

    fn display(&self) -> String {
        match (&self.rule_id, self.priority) {
            (Some(id), Some(priority)) => format!("rule:{id}@{priority}"),
            _ => self.kind.name().to_owned(),
        }
    }
}

impl From<&ResolutionSource> for SelectionState {
    fn from(source: &ResolutionSource) -> Self {
        match source {
            ResolutionSource::InvalidExplicit => Self::InvalidExplicit,
            ResolutionSource::InvalidRepositoryBinding => Self::InvalidRepositoryBinding,
            ResolutionSource::Ambiguous => Self::Ambiguous,
            ResolutionSource::Unresolved => Self::Unresolved,
            _ => Self::Resolved,
        }
    }
}

impl SelectionState {
    fn name(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Unresolved => "unresolved",
            Self::Ambiguous => "ambiguous",
            Self::InvalidExplicit => "invalid_explicit",
            Self::InvalidRepositoryBinding => "invalid_repository_binding",
        }
    }
}

impl SelectionSourceKind {
    fn name(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::RepositoryBinding => "repository_binding",
            Self::Rule => "rule",
            Self::GithubLogin => "github_login",
            Self::Default => "default",
            Self::Unresolved => "unresolved",
            Self::Ambiguous => "ambiguous",
            Self::InvalidExplicit => "invalid_explicit",
            Self::InvalidRepositoryBinding => "invalid_repository_binding",
        }
    }
}

impl RepositoryState {
    fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Bare => "bare",
            Self::NotRepository => "not_repository",
        }
    }
}

impl RepositoryTransport {
    fn name(self) -> &'static str {
        match self {
            Self::Https => "https",
            Self::Ssh => "ssh",
            Self::Git => "git",
            Self::File => "file",
            Self::Other => "other",
        }
    }
}

fn display_candidates(candidates: &[String]) -> String {
    if candidates.is_empty() {
        "none".into()
    } else {
        candidates.join(",")
    }
}
