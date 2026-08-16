//! Internal managed-text and file-transaction primitives.
//!
//! This module keeps setup's durable file operations separate from the public
//! shell API.  It deliberately retains malformed marker sections: only a
//! complete, non-nested block is owned by ghis.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Clone)]
pub(crate) struct ManagedBlock {
    start_marker: &'static str,
    end_marker: &'static str,
    contents: String,
}

impl ManagedBlock {
    pub(crate) fn new(
        start_marker: &'static str,
        end_marker: &'static str,
        contents: String,
    ) -> Self {
        Self {
            start_marker,
            end_marker,
            contents,
        }
    }

    /// Remove every complete, non-nested managed block from `contents`.
    pub(crate) fn remove_from(&self, contents: &str) -> (String, bool) {
        let mut output = String::with_capacity(contents.len());
        let mut cursor = 0usize;
        let mut changed = false;

        while let Some(start_rel) = contents[cursor..].find(self.start_marker) {
            let start = cursor + start_rel;
            let after_start = start + self.start_marker.len();
            let Some(end_rel) = contents[after_start..].find(self.end_marker) else {
                break;
            };
            let end_marker = after_start + end_rel;
            if let Some(next_start_rel) = contents[after_start..].find(self.start_marker) {
                let next_start = after_start + next_start_rel;
                if next_start < end_marker {
                    // A later start marker proves the older one is malformed.
                    // Keep that user-visible section and continue from the newer
                    // marker, which may itself delimit a complete ghis block.
                    output.push_str(&contents[cursor..next_start]);
                    cursor = next_start;
                    continue;
                }
            }
            let mut end = end_marker + self.end_marker.len();
            // Consume one line ending while preserving all surrounding bytes.
            if contents[end..].starts_with("\r\n") {
                end += 2;
            } else if contents[end..].starts_with('\n') || contents[end..].starts_with('\r') {
                end += 1;
            }
            // `replace_in` owns a separator line before a managed block. Remove
            // it during uninstall so normal setup/uninstall is byte-for-byte.
            let owned_start = if contents[cursor..start].ends_with("\r\n\r\n") {
                start - 2
            } else if contents[cursor..start].ends_with("\n\n")
                || contents[cursor..start].ends_with("\r\r")
            {
                start - 1
            } else {
                start
            };
            output.push_str(&contents[cursor..owned_start]);
            cursor = end;
            changed = true;
        }

        output.push_str(&contents[cursor..]);
        (output, changed)
    }

    /// Replace all complete owned blocks while preserving unrelated text.
    pub(crate) fn replace_in(&self, contents: &str) -> String {
        let (mut cleaned, _) = self.remove_from(contents);
        if !cleaned.is_empty() && !cleaned.ends_with('\n') {
            cleaned.push('\n');
        }
        if !cleaned.is_empty() && !cleaned.ends_with("\n\n") {
            cleaned.push('\n');
        }
        cleaned.push_str(self.contents.trim_end_matches(['\n', '\r']));
        cleaned.push('\n');
        cleaned
    }
}

/// A requested drop-in path and the regular file it is safe to edit.
///
/// A symlink remains in place: writes target its resolved destination instead.
#[derive(Debug, Clone)]
pub(crate) struct DropInFile {
    requested: PathBuf,
    target: PathBuf,
}

impl DropInFile {
    pub(crate) fn open(requested: &Path) -> io::Result<Self> {
        Ok(Self {
            requested: requested.to_path_buf(),
            target: editable_target(requested)?,
        })
    }

    pub(crate) fn requested(&self) -> &Path {
        &self.requested
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    pub(crate) fn read_optional(&self) -> io::Result<Option<String>> {
        read_optional(&self.target)
    }

    pub(crate) fn write(&self, contents: &[u8]) -> io::Result<()> {
        if let Some(parent) = self.target.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&self.target, contents, metadata_mode(&self.target))
    }

    pub(crate) fn snapshot(&self) -> io::Result<FileSnapshot> {
        FileSnapshot::capture(&self.target)
    }
}

#[derive(Debug)]
pub(crate) struct FileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    mode: Option<u32>,
}

impl FileSnapshot {
    pub(crate) fn capture(path: &Path) -> io::Result<Self> {
        let contents = match fs::read(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        Ok(Self {
            path: path.to_path_buf(),
            contents,
            mode: metadata_mode(path),
        })
    }

    pub(crate) fn restore(&self) -> io::Result<()> {
        match self.contents.as_deref() {
            Some(contents) => {
                if let Some(parent) = self.path.parent() {
                    fs::create_dir_all(parent)?;
                }
                atomic_write(&self.path, contents, self.mode)
            }
            None => match fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        }
    }
}

pub(crate) fn backup_path(requested: &Path) -> PathBuf {
    PathBuf::from(format!("{}.ghis.bak", requested.display()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SetupResult {
    pub(crate) zshrc: PathBuf,
    pub(crate) init_file: PathBuf,
    pub(crate) backup: Option<PathBuf>,
    pub(crate) changed: bool,
}

/// Apply the init and startup-file changes as one rollback-capable transaction.
pub(crate) fn setup(
    zshrc: &Path,
    init_file: &Path,
    init_content: &str,
    block: &ManagedBlock,
) -> io::Result<SetupResult> {
    let zshrc = DropInFile::open(zshrc)?;
    let init_file = DropInFile::open(init_file)?;
    let backup_path = backup_path(zshrc.requested());
    // Preserve this rollback order and its error rendering: it is observable to
    // callers recovering from a partially completed setup.
    let snapshots = [
        init_file.snapshot()?,
        zshrc.snapshot()?,
        FileSnapshot::capture(&backup_path)?,
    ];

    let result = (|| {
        let init_changed = init_file.read_optional()?.as_deref() != Some(init_content);
        if init_changed {
            init_file.write(init_content.as_bytes())?;
        }

        let existing = zshrc.read_optional()?.unwrap_or_default();
        let updated = block.replace_in(&existing);
        let zshrc_changed = updated != existing;
        let mut backup = None;
        if zshrc_changed {
            if !backup_path.exists() && zshrc.target().exists() {
                // A backup is made only once so repeated setup cannot overwrite
                // the user's original file with a later generated version.
                fs::copy(zshrc.target(), &backup_path)?;
                backup = Some(backup_path.clone());
            }
            zshrc.write(updated.as_bytes())?;
        }

        Ok(SetupResult {
            zshrc: zshrc.requested().to_path_buf(),
            init_file: init_file.requested().to_path_buf(),
            backup,
            changed: init_changed || zshrc_changed,
        })
    })();

    match result {
        Ok(report) => Ok(report),
        Err(error) => {
            let failures = snapshots
                .iter()
                .filter_map(|snapshot| snapshot.restore().err())
                .map(|error| error.to_string())
                .collect::<Vec<_>>();
            if failures.is_empty() {
                Err(error)
            } else {
                Err(io::Error::other(format!(
                    "{error}; shell setup rollback failed: {}",
                    failures.join("; ")
                )))
            }
        }
    }
}

/// Remove the owned block without deleting the init file or backup.
pub(crate) fn uninstall(zshrc: &Path, block: &ManagedBlock) -> io::Result<bool> {
    let zshrc = DropInFile::open(zshrc)?;
    let Some(existing) = zshrc.read_optional()? else {
        return Ok(false);
    };
    let (updated, changed) = block.remove_from(&existing);
    if changed {
        zshrc.write(updated.as_bytes())?;
    }
    Ok(changed)
}

pub(crate) fn read_optional(path: &Path) -> io::Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut content = String::new();
    fs::File::open(path)?.read_to_string(&mut content)?;
    Ok(Some(content))
}

fn editable_target(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::read_link(path)?;
            let target = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or_else(|| Path::new(".")).join(target)
            };
            // Do not replace a dangling link with a regular generated file.
            fs::canonicalize(target)
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => Err(error),
    }
}

fn metadata_mode(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        path.metadata()
            .ok()
            .map(|metadata| metadata.permissions().mode())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

fn atomic_write(path: &Path, contents: &[u8], mode: Option<u32>) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        fs::set_permissions(temporary.path(), fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;

    temporary
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_persist_cleans_up_the_randomized_temporary_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let destination = directory.path().join("destination");
        fs::create_dir(&destination).expect("destination directory");
        let before = fs::read_dir(directory.path()).unwrap().count();

        assert!(atomic_write(&destination, b"content", None).is_err());

        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), before);
    }
}
