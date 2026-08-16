//! Platform-owned filesystem conventions.
//!
//! This module keeps operating-system policy at the edge of the application:
//! user data roots, private directories/files, and same-directory temporary
//! replacement.  Callers still decide which ghis-specific namespace and file
//! names to use.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;
use thiserror::Error;

/// Errors while resolving the platform's user data roots.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PlatformError {
    #[error("{variable} is not set")]
    MissingEnvironment { variable: &'static str },
    #[error("{variable} must contain a non-empty absolute path")]
    InvalidEnvironment { variable: &'static str },
}

/// Resolve the current user's home directory without consulting the current
/// directory. Unix retains the historical `HOME` behavior. Windows prefers
/// `USERPROFILE` and falls back only to the paired `HOMEDRIVE`/`HOMEPATH`
/// values that PowerShell and the Windows profile conventions use.
pub fn user_home() -> Result<PathBuf, PlatformError> {
    user_home_from_environment(|name| env::var_os(name))
}

fn user_home_from_environment(
    get: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, PlatformError> {
    #[cfg(windows)]
    {
        if let Some(profile) = get("USERPROFILE") {
            return required_windows_directory("USERPROFILE", Some(profile));
        }

        let drive = get("HOMEDRIVE");
        let path = get("HOMEPATH");
        let home = match (drive, path) {
            (Some(drive), Some(path)) if !drive.is_empty() && !path.is_empty() => {
                PathBuf::from(drive).join(path)
            }
            _ => {
                return Err(PlatformError::MissingEnvironment {
                    variable: "USERPROFILE",
                });
            }
        };
        if home.is_absolute() {
            Ok(home)
        } else {
            Err(PlatformError::InvalidEnvironment {
                variable: "HOMEDRIVE+HOMEPATH",
            })
        }
    }

    #[cfg(not(windows))]
    {
        required_path("HOME", get("HOME"))
    }
}

/// Base directories for user configuration, cache, and state.
///
/// The application owns the namespace below these roots.  Keeping the bases
/// here means a custom configuration file can retain its existing namespace
/// rules without becoming coupled to platform environment variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserDirectories {
    pub config_base: PathBuf,
    pub cache_base: PathBuf,
    pub state_base: PathBuf,
}

impl UserDirectories {
    /// Resolve the conventional user directories from the process environment.
    pub fn discover() -> Result<Self, PlatformError> {
        Self::from_environment(|name| env::var_os(name))
    }

    /// Resolve directories from an environment lookup function.
    ///
    /// Keeping lookup injectable makes platform policy testable without
    /// changing the process environment.  On Unix, empty and relative XDG
    /// values intentionally behave exactly like an unset value.
    fn from_environment(get: impl Fn(&str) -> Option<OsString>) -> Result<Self, PlatformError> {
        #[cfg(windows)]
        {
            let config_base = required_windows_directory("APPDATA", get("APPDATA"))?;
            let local_base = required_windows_directory("LOCALAPPDATA", get("LOCALAPPDATA"))?;
            Ok(Self {
                config_base,
                cache_base: local_base.clone(),
                state_base: local_base,
            })
        }

        #[cfg(not(windows))]
        {
            let home = required_path("HOME", get("HOME"))?;
            Ok(Self {
                config_base: xdg_dir(get("XDG_CONFIG_HOME"), home.join(".config")),
                cache_base: xdg_dir(get("XDG_CACHE_HOME"), home.join(".cache")),
                state_base: xdg_dir(get("XDG_STATE_HOME"), home.join(".local").join("state")),
            })
        }
    }
}

#[cfg(not(windows))]
fn xdg_dir(value: Option<OsString>, fallback: PathBuf) -> PathBuf {
    value
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or(fallback)
}

fn required_path(
    variable: &'static str,
    value: Option<OsString>,
) -> Result<PathBuf, PlatformError> {
    value
        .map(PathBuf::from)
        .ok_or(PlatformError::MissingEnvironment { variable })
}

#[cfg(windows)]
fn required_windows_directory(
    variable: &'static str,
    value: Option<OsString>,
) -> Result<PathBuf, PlatformError> {
    let path = required_path(variable, value)?;
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return Err(PlatformError::InvalidEnvironment { variable });
    }
    Ok(path)
}

/// Set private-file permissions where the platform exposes them.
///
/// Windows ACLs are inherited from the containing user directory; no portable
/// mode-bit operation is available here, so this is intentionally a no-op.
fn set_private_file_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, permissions(0o600))?;
    let _ = path;
    Ok(())
}

/// Atomically replace a file using a temporary file in the target directory.
///
/// Existing permissions are retained. New files receive private permissions
/// on Unix, while Windows relies on its containing directory ACL.
pub fn atomic_write(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    if let Ok(metadata) = fs::metadata(path) {
        temporary
            .as_file()
            .set_permissions(metadata.permissions())?;
    } else {
        set_private_file_permissions(temporary.path())?;
    }
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(unix)]
fn permissions(mode: u32) -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt;
    fs::Permissions::from_mode(mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_directories_keep_xdg_precedence_and_ignore_invalid_values() {
        let dirs = UserDirectories::from_environment(|name| match name {
            "HOME" => Some("/home/alice".into()),
            "XDG_CONFIG_HOME" => Some("/xdg/config".into()),
            "XDG_CACHE_HOME" => Some("relative-cache".into()),
            "XDG_STATE_HOME" => Some(OsString::new()),
            _ => None,
        })
        .expect("home is sufficient when XDG overrides are invalid");

        assert_eq!(dirs.config_base, PathBuf::from("/xdg/config"));
        assert_eq!(dirs.cache_base, PathBuf::from("/home/alice/.cache"));
        assert_eq!(dirs.state_base, PathBuf::from("/home/alice/.local/state"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_requires_home() {
        let error = UserDirectories::from_environment(|_| None).expect_err("HOME is required");
        assert_eq!(
            error,
            PlatformError::MissingEnvironment { variable: "HOME" }
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_requires_home_even_when_all_xdg_roots_are_set() {
        let error = UserDirectories::from_environment(|name| match name {
            "XDG_CONFIG_HOME" | "XDG_CACHE_HOME" | "XDG_STATE_HOME" => Some("/xdg".into()),
            _ => None,
        })
        .expect_err("HOME remains required for compatibility");
        assert_eq!(
            error,
            PlatformError::MissingEnvironment { variable: "HOME" }
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_home_fallback_keeps_relative_home_behavior() {
        let dirs = UserDirectories::from_environment(|name| {
            (name == "HOME").then(|| OsString::from("relative-home"))
        })
        .expect("HOME was set");
        assert_eq!(dirs.config_base, PathBuf::from("relative-home/.config"));
        assert_eq!(dirs.cache_base, PathBuf::from("relative-home/.cache"));
        assert_eq!(dirs.state_base, PathBuf::from("relative-home/.local/state"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_user_home_keeps_the_home_environment_behavior() {
        assert_eq!(
            user_home_from_environment(|name| (name == "HOME").then(|| "relative-home".into())),
            Ok(PathBuf::from("relative-home"))
        );
        assert_eq!(
            user_home_from_environment(|_| None),
            Err(PlatformError::MissingEnvironment { variable: "HOME" })
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_creates_owner_only_files() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().expect("temporary directory");
        let file = temporary.path().join("cache.json");
        atomic_write(&file, b"{}\n").expect("atomically write private file");
        assert_eq!(fs::read(&file).unwrap(), b"{}\n");
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(windows)]
    fn windows_directories(
        appdata: OsString,
        localappdata: OsString,
    ) -> Result<UserDirectories, PlatformError> {
        UserDirectories::from_environment(|name| match name {
            "APPDATA" => Some(appdata.clone()),
            "LOCALAPPDATA" => Some(localappdata.clone()),
            _ => None,
        })
    }

    #[cfg(windows)]
    #[test]
    fn windows_user_home_prefers_userprofile_and_falls_back_to_drive_and_path() {
        let userprofile = user_home_from_environment(|name| match name {
            "USERPROFILE" => Some(r"C:\Users\alice".into()),
            "HOMEDRIVE" => Some("D:".into()),
            "HOMEPATH" => Some(r"\ignored".into()),
            _ => None,
        })
        .expect("USERPROFILE wins");
        assert_eq!(userprofile, PathBuf::from(r"C:\Users\alice"));

        let fallback = user_home_from_environment(|name| match name {
            "HOMEDRIVE" => Some("C:".into()),
            "HOMEPATH" => Some(r"\Users\alice".into()),
            _ => None,
        })
        .expect("paired fallback");
        assert_eq!(fallback, PathBuf::from(r"C:\Users\alice"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_user_home_rejects_missing_and_invalid_values_without_cwd_fallback() {
        assert_eq!(
            user_home_from_environment(|_| None),
            Err(PlatformError::MissingEnvironment {
                variable: "USERPROFILE"
            })
        );
        assert_eq!(
            user_home_from_environment(|name| {
                (name == "USERPROFILE").then(|| "relative-home".into())
            }),
            Err(PlatformError::InvalidEnvironment {
                variable: "USERPROFILE"
            })
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_maps_config_to_appdata_and_cache_state_to_localappdata() {
        let dirs = UserDirectories::from_environment(|name| match name {
            "APPDATA" => Some(r"C:\Users\alice\AppData\Roaming".into()),
            "LOCALAPPDATA" => Some(r"C:\Users\alice\AppData\Local".into()),
            _ => None,
        })
        .expect("Windows roots are set");

        assert_eq!(
            dirs.config_base,
            PathBuf::from(r"C:\Users\alice\AppData\Roaming")
        );
        assert_eq!(
            dirs.cache_base,
            PathBuf::from(r"C:\Users\alice\AppData\Local")
        );
        assert_eq!(dirs.state_base, dirs.cache_base);
    }

    #[cfg(windows)]
    #[test]
    fn windows_reports_each_missing_root() {
        let error = UserDirectories::from_environment(|_| None).expect_err("APPDATA is required");
        assert_eq!(
            error,
            PlatformError::MissingEnvironment {
                variable: "APPDATA"
            }
        );

        let error = UserDirectories::from_environment(|name| {
            (name == "APPDATA").then(|| r"C:\Users\alice\AppData\Roaming".into())
        })
        .expect_err("LOCALAPPDATA is required");
        assert_eq!(
            error,
            PlatformError::MissingEnvironment {
                variable: "LOCALAPPDATA"
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_empty_data_roots() {
        let error = windows_directories(OsString::new(), r"C:\Users\alice\AppData\Local".into())
            .expect_err("empty APPDATA must fail");
        assert_eq!(
            error,
            PlatformError::InvalidEnvironment {
                variable: "APPDATA"
            }
        );

        let error = windows_directories(r"C:\Users\alice\AppData\Roaming".into(), OsString::new())
            .expect_err("empty LOCALAPPDATA must fail");
        assert_eq!(
            error,
            PlatformError::InvalidEnvironment {
                variable: "LOCALAPPDATA"
            }
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_rejects_relative_data_roots() {
        let error = windows_directories(
            "relative-appdata".into(),
            r"C:\Users\alice\AppData\Local".into(),
        )
        .expect_err("relative APPDATA must fail");
        assert_eq!(
            error,
            PlatformError::InvalidEnvironment {
                variable: "APPDATA"
            }
        );

        let error = windows_directories(
            r"C:\Users\alice\AppData\Roaming".into(),
            "relative-localappdata".into(),
        )
        .expect_err("relative LOCALAPPDATA must fail");
        assert_eq!(
            error,
            PlatformError::InvalidEnvironment {
                variable: "LOCALAPPDATA"
            }
        );
    }
}
