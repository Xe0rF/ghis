//! Disposable, non-secret XDG state.
//!
//! Repository records make `ghis sync` useful across previously bound
//! worktrees. They contain paths and profile ids only. Tokens, remote URLs,
//! command arguments, names, and email addresses are deliberately absent.

use fd_lock::RwLock;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tempfile::NamedTempFile;
use thiserror::Error;

const STATE_VERSION: u32 = 1;
const MAX_AUDIT_LOG_BYTES: u64 = 256 * 1024;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("状态文件 I/O 错误（{path}）：{source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("状态文件 JSON 无效（{path}）：{source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("不支持的状态文件版本 {0}")]
    Version(u32),
    #[error("无法锁定状态文件：{0}")]
    Lock(#[source] io::Error),
}

pub type Result<T> = std::result::Result<T, StateError>;

/// One worktree known to have a repository-local ghis binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryBindingRecord {
    pub config_file: PathBuf,
    pub repository_path: PathBuf,
    pub git_dir: PathBuf,
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct RepositoryRegistry {
    version: u32,
    repositories: Vec<RepositoryBindingRecord>,
}

impl Default for RepositoryRegistry {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            repositories: Vec::new(),
        }
    }
}

/// Return records owned by one config file.
pub fn repositories_for_config(
    path: &Path,
    config_file: &Path,
) -> Result<Vec<RepositoryBindingRecord>> {
    let config_file = absolute_path(config_file);
    Ok(load_registry(path)?
        .repositories
        .into_iter()
        .filter(|record| absolute_path(&record.config_file) == config_file)
        .collect())
}

/// Insert or update one record without disturbing records for other configs.
pub fn register(path: &Path, mut record: RepositoryBindingRecord) -> Result<()> {
    record.config_file = absolute_path(&record.config_file);
    record.repository_path = absolute_path(&record.repository_path);
    record.git_dir = absolute_path(&record.git_dir);
    update_registry(path, |registry| {
        if let Some(existing) = registry.repositories.iter_mut().find(|existing| {
            existing.config_file == record.config_file && existing.git_dir == record.git_dir
        }) {
            if *existing == record {
                return false;
            }
            *existing = record;
        } else {
            registry.repositories.push(record);
        }
        sort_records(&mut registry.repositories);
        true
    })
}

/// Forget one worktree. The Git config remains authoritative.
pub fn unregister(path: &Path, config_file: &Path, git_dir: &Path) -> Result<bool> {
    let config_file = absolute_path(config_file);
    let git_dir = absolute_path(git_dir);
    let mut removed = false;
    update_registry(path, |registry| {
        let before = registry.repositories.len();
        registry.repositories.retain(|record| {
            !(absolute_path(&record.config_file) == config_file
                && absolute_path(&record.git_dir) == git_dir)
        });
        removed = registry.repositories.len() != before;
        removed
    })?;
    Ok(removed)
}

#[derive(Debug, Clone, Copy)]
pub enum AuditAction {
    Bind,
    Unbind,
    SyncCheck,
    SyncRepair,
}

impl AuditAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::Unbind => "unbind",
            Self::SyncCheck => "sync-check",
            Self::SyncRepair => "sync-repair",
        }
    }
}

#[derive(Serialize)]
struct AuditEvent<'a> {
    version: u32,
    unix_seconds: u64,
    action: &'static str,
    repository: &'a Path,
    profile: Option<&'a str>,
}

/// Append a bounded JSON-lines event containing no credential-bearing fields.
pub fn append_audit_event(
    path: &Path,
    action: AuditAction,
    repository: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(parent)?;
    with_lock(path, || {
        append_audit_event_unlocked(path, action, repository, profile)
    })
}

fn append_audit_event_unlocked(
    path: &Path,
    action: AuditAction,
    repository: &Path,
    profile: Option<&str>,
) -> Result<()> {
    let truncate = fs::metadata(path)
        .map(|metadata| metadata.len() >= MAX_AUDIT_LOG_BYTES)
        .unwrap_or(false);
    let mut options = OpenOptions::new();
    options
        .create(true)
        .write(true)
        .append(!truncate)
        .truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    set_private_file(&file, path)?;
    let event = AuditEvent {
        version: STATE_VERSION,
        unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        action: action.as_str(),
        repository,
        profile,
    };
    serde_json::to_writer(&mut file, &event).map_err(|source| StateError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(b"\n").map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_data().map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn update_registry(
    path: &Path,
    update: impl FnOnce(&mut RepositoryRegistry) -> bool,
) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    create_private_dir(parent)?;
    with_lock(path, || {
        let mut registry = load_registry(path)?;
        if !update(&mut registry) {
            return Ok(());
        }
        registry.version = STATE_VERSION;

        let mut temporary = NamedTempFile::new_in(parent).map_err(|source| StateError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        serde_json::to_writer_pretty(&mut temporary, &registry).map_err(|source| {
            StateError::Json {
                path: path.to_path_buf(),
                source,
            }
        })?;
        temporary
            .write_all(b"\n")
            .map_err(|source| StateError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|source| StateError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
        set_private_file(temporary.as_file(), temporary.path())?;
        temporary.persist(path).map_err(|error| StateError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
        Ok(())
    })
}

fn load_registry(path: &Path) -> Result<RepositoryRegistry> {
    let mut input = Vec::new();
    match File::open(path) {
        Ok(mut file) => {
            file.read_to_end(&mut input)
                .map_err(|source| StateError::Io {
                    path: path.to_path_buf(),
                    source,
                })?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RepositoryRegistry::default());
        }
        Err(source) => {
            return Err(StateError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    let registry = serde_json::from_slice::<RepositoryRegistry>(&input).map_err(|source| {
        StateError::Json {
            path: path.to_path_buf(),
            source,
        }
    })?;
    if registry.version != STATE_VERSION {
        return Err(StateError::Version(registry.version));
    }
    Ok(registry)
}

fn with_lock<T>(path: &Path, operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_path = path.with_extension(format!(
        "{}lock",
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| format!("{extension}."))
            .unwrap_or_default()
    ));
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&lock_path).map_err(StateError::Lock)?;
    set_private_file(&file, &lock_path)?;
    let mut lock = RwLock::new(file);
    let _guard = lock.write().map_err(StateError::Lock)?;
    operation()
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|source| StateError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            StateError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn set_private_file(file: &File, path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| StateError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

fn absolute_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn sort_records(records: &mut [RepositoryBindingRecord]) {
    records.sort_by(|left, right| {
        (&left.config_file, &left.git_dir).cmp(&(&right.config_file, &right.git_dir))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(root: &Path, config: &str, git_dir: &str, profile: &str) -> RepositoryBindingRecord {
        RepositoryBindingRecord {
            config_file: root.join(config),
            repository_path: root.join(git_dir.trim_end_matches("/.git")),
            git_dir: root.join(git_dir),
            profile: profile.into(),
        }
    }

    #[test]
    fn registry_is_idempotent_and_isolated_by_config() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state/repositories.json");
        let first = record(directory.path(), "a.toml", "one/.git", "personal");
        let second = record(directory.path(), "b.toml", "two/.git", "work");
        register(&path, first.clone()).unwrap();
        register(&path, second.clone()).unwrap();
        register(&path, first.clone()).unwrap();

        assert_eq!(
            repositories_for_config(&path, &first.config_file).unwrap(),
            vec![first.clone()]
        );
        assert_eq!(
            repositories_for_config(&path, &second.config_file).unwrap(),
            vec![second.clone()]
        );
        assert!(unregister(&path, &first.config_file, &first.git_dir).unwrap());
        assert!(!unregister(&path, &first.config_file, &first.git_dir).unwrap());
        assert!(
            repositories_for_config(&path, &first.config_file)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn concurrent_registry_updates_do_not_lose_records() {
        use std::sync::{Arc, Barrier};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state/repositories.json");
        let root = directory.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(8));
        let mut threads = Vec::new();
        for index in 0..8 {
            let path = path.clone();
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                register(
                    &path,
                    record(
                        &root,
                        "config.toml",
                        &format!("repo-{index}/.git"),
                        &format!("profile-{index}"),
                    ),
                )
                .unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(
            repositories_for_config(&path, &root.join("config.toml"))
                .unwrap()
                .len(),
            8
        );
    }

    #[cfg(unix)]
    #[test]
    fn state_and_audit_files_are_private_and_contain_no_token_field() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let registry = directory.path().join("state/repositories.json");
        let log = directory.path().join("state/ghis.log");
        let item = record(directory.path(), "config.toml", "repo/.git", "work");
        register(&registry, item).unwrap();
        append_audit_event(
            &log,
            AuditAction::Bind,
            &directory.path().join("repo"),
            Some("work"),
        )
        .unwrap();

        for path in [&registry, &log] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(directory.path().join("state"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let combined = format!(
            "{}{}",
            fs::read_to_string(registry).unwrap(),
            fs::read_to_string(log).unwrap()
        );
        assert!(!combined.to_ascii_lowercase().contains("token"));
        assert!(!combined.to_ascii_lowercase().contains("password"));
    }
}
