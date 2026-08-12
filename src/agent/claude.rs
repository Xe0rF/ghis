//! Claude Code hook and settings integration.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::NamedTempFile;
use thiserror::Error;

use super::{AgentError, AgentKind, ContextProvider, launcher_spec, resolved_context};
use crate::process::CommandSpec;

pub const SETTINGS_MARKER: &str = "ghis-agent-context-v1";
pub const MAX_HOOK_INPUT_BYTES: usize = 1024 * 1024;
pub const MAX_ADDITIONAL_CONTEXT_BYTES: usize = 64 * 1024;
const HOOK_EVENTS: [&str; 3] = ["SessionStart", "UserPromptSubmit", "SubagentStart"];

#[derive(Debug, Error)]
pub enum ClaudeError {
    #[error("Claude hook input exceeds {limit} bytes")]
    InputTooLarge { limit: usize },
    #[error("rendered Claude context exceeds {limit} bytes")]
    ContextTooLarge { limit: usize },
    #[error("invalid Claude hook input: {0}")]
    InvalidInput(#[from] serde_json::Error),
    #[error("Claude settings root must be a JSON object")]
    InvalidSettings,
    #[error("could not access `{path}`: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Agent(#[from] AgentError),
}

#[derive(Debug, Deserialize)]
struct HookInput {
    #[serde(default)]
    hook_event_name: Option<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    // Remaining fields, including prompt, are intentionally ignored and never
    // copied into output or diagnostics.
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: HookSpecificOutput,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookSpecificOutput {
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,
    #[serde(rename = "additionalContext")]
    pub additional_context: String,
}

/// Handle one hook invocation without ever returning or logging the prompt.
pub fn handle_hook<P: ContextProvider, R: Read>(
    provider: &P,
    mut input: R,
) -> Result<HookOutput, ClaudeError> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|source| ClaudeError::Io {
            path: PathBuf::from("<stdin>"),
            source,
        })?;
    if bytes.len() > MAX_HOOK_INPUT_BYTES {
        return Err(ClaudeError::InputTooLarge {
            limit: MAX_HOOK_INPUT_BYTES,
        });
    }
    let hook: HookInput = serde_json::from_slice(&bytes)?;
    let event = hook
        .hook_event_name
        .unwrap_or_else(|| "SessionStart".into());
    let _cwd = hook.cwd;
    let context = resolved_context(provider, AgentKind::Claude)?;
    if context.len() > MAX_ADDITIONAL_CONTEXT_BYTES {
        return Err(ClaudeError::ContextTooLarge {
            limit: MAX_ADDITIONAL_CONTEXT_BYTES,
        });
    }
    Ok(HookOutput {
        hook_specific_output: HookSpecificOutput {
            hook_event_name: event,
            additional_context: context,
        },
    })
}

pub fn write_hook_output(output: &HookOutput, mut writer: impl Write) -> Result<(), ClaudeError> {
    serde_json::to_writer(&mut writer, output)?;
    writer.write_all(b"\n").map_err(|source| ClaudeError::Io {
        path: PathBuf::from("<stdout>"),
        source,
    })
}

/// Hook command installed in Claude settings.
pub fn hook_command(ghis: &Path, config: Option<&Path>) -> String {
    let mut parts = vec![shell_word(ghis.as_os_str())];
    if let Some(config) = config {
        parts.push("--config".into());
        parts.push(shell_word(config.as_os_str()));
    }
    parts.extend(["agent".into(), "hook".into(), "claude".into()]);
    parts.join(" ")
}

/// Merge ghis hooks while preserving unknown settings and unrelated hooks.
pub fn setup_settings(
    path: &Path,
    ghis: &Path,
    config: Option<&Path>,
) -> Result<bool, ClaudeError> {
    let mut root = read_settings(path)?;
    let object = root.as_object_mut().ok_or(ClaudeError::InvalidSettings)?;
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or(ClaudeError::InvalidSettings)?;
    let command = hook_command(ghis, config);
    let entry = managed_hook_entry(&command);
    let mut changed = false;

    for event in HOOK_EVENTS {
        let array = hooks.entry(event).or_insert_with(|| json!([]));
        let array = array.as_array_mut().ok_or(ClaudeError::InvalidSettings)?;
        let matches = array
            .iter()
            .enumerate()
            .filter_map(|(index, value)| is_managed_entry(value).then_some(index))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [] => {
                array.push(entry.clone());
                changed = true;
            }
            [index] if array[*index] == entry => {}
            [first, rest @ ..] => {
                array[*first] = entry.clone();
                for index in rest.iter().rev() {
                    array.remove(*index);
                }
                changed = true;
            }
        }
    }
    if changed {
        atomic_write_json(path, &root)?;
    }
    Ok(changed)
}

/// Remove only entries marked as ghis-owned. Repeated uninstall is a no-op.
pub fn uninstall_settings(path: &Path) -> Result<bool, ClaudeError> {
    if !path.exists() {
        return Ok(false);
    }
    let mut root = read_settings(path)?;
    let object = root.as_object_mut().ok_or(ClaudeError::InvalidSettings)?;
    let Some(hooks) = object.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(false);
    };
    let mut changed = false;
    for event in HOOK_EVENTS {
        if let Some(array) = hooks.get_mut(event).and_then(Value::as_array_mut) {
            let before = array.len();
            array.retain(|value| !is_managed_entry(value));
            changed |= before != array.len();
        }
    }
    if changed {
        atomic_write_json(path, &root)?;
    }
    Ok(changed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsStatus {
    pub session_start: bool,
    pub user_prompt_submit: bool,
    pub subagent_start: bool,
}

impl SettingsStatus {
    pub fn installed(self) -> bool {
        self.session_start && self.user_prompt_submit && self.subagent_start
    }
}

pub fn settings_status(path: &Path) -> Result<SettingsStatus, ClaudeError> {
    if !path.exists() {
        return Ok(SettingsStatus {
            session_start: false,
            user_prompt_submit: false,
            subagent_start: false,
        });
    }
    let root = read_settings(path)?;
    let present = |event: &str| {
        root.get("hooks")
            .and_then(|value| value.get(event))
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().any(is_managed_entry))
    };
    Ok(SettingsStatus {
        session_start: present("SessionStart"),
        user_prompt_submit: present("UserPromptSubmit"),
        subagent_start: present("SubagentStart"),
    })
}

pub fn run_spec<P: ContextProvider>(
    provider: &P,
    program: impl Into<std::ffi::OsString>,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
    cwd: &Path,
) -> Result<CommandSpec, AgentError> {
    let context = resolved_context(provider, AgentKind::Claude)?;
    if context.len() > MAX_ADDITIONAL_CONTEXT_BYTES {
        return Err(AgentError::Context(Box::new(
            ClaudeError::ContextTooLarge {
                limit: MAX_ADDITIONAL_CONTEXT_BYTES,
            },
        )));
    }
    let mut argv = vec!["--append-system-prompt".into(), context.into()];
    // Keep any user-provided --append-system-prompt entries intact and add our
    // context as another occurrence. Claude Code stacks repeated options,
    // while argv preservation means existing --model/-C/subcommands retain
    // their normal parsing semantics.
    argv.extend(args.into_iter().map(Into::into));
    Ok(launcher_spec(program, argv, cwd))
}

fn managed_hook_entry(command: &str) -> Value {
    json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": command,
            "statusMessage": SETTINGS_MARKER
        }]
    })
}

fn is_managed_entry(value: &Value) -> bool {
    value
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("statusMessage").and_then(Value::as_str) == Some(SETTINGS_MARKER)
            })
        })
}

fn read_settings(path: &Path) -> Result<Value, ClaudeError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(ClaudeError::InvalidInput),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(json!({})),
        Err(source) => Err(ClaudeError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn atomic_write_json(path: &Path, value: &Value) -> Result<(), ClaudeError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ClaudeError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temp = NamedTempFile::new_in(parent).map_err(|source| ClaudeError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    serde_json::to_writer_pretty(temp.as_file_mut(), value)?;
    temp.as_file_mut()
        .write_all(b"\n")
        .map_err(|source| ClaudeError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temp.as_file_mut()
        .sync_all()
        .map_err(|source| ClaudeError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temp.persist(path).map_err(|error| ClaudeError::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

fn shell_word(value: &std::ffi::OsStr) -> String {
    let value = value.to_string_lossy();
    format!("'{}'", value.replace('\'', "'\\''"))
}
