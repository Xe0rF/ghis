//! Shell integration model, renderer registry, and public compatibility facade.
//!
//! Shell-specific source and persistent startup-file behavior lives in sibling
//! modules. The zsh facade remains available at `ghis::shell::*` for existing
//! callers.

use clap::Command;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

/// A shell family supported by the ghis integration interface.
///
/// Recognition is intentionally broader than rendering support. Callers must
/// request a renderer before changing any startup files, so a recognised but
/// unimplemented shell fails closed instead of receiving zsh content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShellKind {
    Zsh,
    Bash,
    Fish,
    PowerShell,
}

impl ShellKind {
    /// Static integration details for this shell family.
    pub const fn spec(self) -> ShellSpec {
        match self {
            Self::Zsh => ShellSpec {
                kind: Self::Zsh,
                init_file_name: "init.zsh",
                install_strategy: InstallStrategy::ManagedBlock,
                startup_file: StartupFileTarget::ZshRc,
                completion: CompletionKind::Zsh,
                health: HealthCapability::EnvironmentMarker,
            },
            Self::Bash => ShellSpec {
                kind: Self::Bash,
                init_file_name: "init.bash",
                install_strategy: InstallStrategy::ManagedBlock,
                startup_file: StartupFileTarget::BashRc,
                completion: CompletionKind::Bash,
                health: HealthCapability::EnvironmentMarker,
            },
            Self::Fish => ShellSpec {
                kind: Self::Fish,
                init_file_name: "ghis.fish",
                install_strategy: InstallStrategy::DropInFile,
                startup_file: StartupFileTarget::FishConfD,
                completion: CompletionKind::Fish,
                health: HealthCapability::EnvironmentMarker,
            },
            Self::PowerShell => ShellSpec {
                kind: Self::PowerShell,
                init_file_name: "ghis.ps1",
                install_strategy: InstallStrategy::DropInFile,
                startup_file: StartupFileTarget::PowerShellProfile,
                completion: CompletionKind::PowerShell,
                health: HealthCapability::EnvironmentMarker,
            },
        }
    }
}

impl std::fmt::Display for ShellKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Fish => "fish",
            Self::PowerShell => "powershell",
        })
    }
}

impl std::str::FromStr for ShellKind {
    type Err = ShellError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "zsh" => Ok(Self::Zsh),
            "bash" => Ok(Self::Bash),
            "fish" => Ok(Self::Fish),
            "powershell" | "power-shell" | "pwsh" => Ok(Self::PowerShell),
            _ => Err(ShellError::Unknown(value.to_owned())),
        }
    }
}

/// Persistent-installation strategy used by a shell integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InstallStrategy {
    ManagedBlock,
    DropInFile,
}

/// Shell-owned location that an installer targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StartupFileTarget {
    ZshRc,
    BashRc,
    FishConfD,
    PowerShellProfile,
}

/// Completion syntax that a future renderer must provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompletionKind {
    Zsh,
    Bash,
    Fish,
    PowerShell,
}

/// How an integration can report that its parent shell loaded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HealthCapability {
    EnvironmentMarker,
}

/// Shell-neutral integration contract. Values describe policy only; they never
/// contain shell source code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ShellSpec {
    kind: ShellKind,
    init_file_name: &'static str,
    install_strategy: InstallStrategy,
    startup_file: StartupFileTarget,
    completion: CompletionKind,
    health: HealthCapability,
}

impl ShellSpec {
    pub const fn kind(self) -> ShellKind {
        self.kind
    }

    pub const fn init_file_name(self) -> &'static str {
        self.init_file_name
    }

    pub const fn install_strategy(self) -> InstallStrategy {
        self.install_strategy
    }

    pub const fn startup_file(self) -> StartupFileTarget {
        self.startup_file
    }

    pub const fn completion(self) -> CompletionKind {
        self.completion
    }

    pub const fn health(self) -> HealthCapability {
        self.health
    }
}

/// Shell-specific renderer implementations.
pub mod fish;
pub mod powershell;
pub mod zsh;

/// Compatibility facade for all established zsh integration names.
pub use zsh::{
    END_MARKER, HEALTH_ENV, HEALTHY_MARKER, IntegrationHealth, LOADED_ENV, START_MARKER,
    SetupReport, generate_init, integration_health, integration_is_installed,
    integration_is_loaded, managed_block, remove_managed_blocks, replace_managed_block, setup,
    setup_rendered, setup_zsh, shell_quote, uninstall, uninstall_zsh, zsh_completion_script,
    zsh_init_script, zsh_wrapper_script, zshrc_path,
};

mod private {
    pub trait Sealed {}
}

/// Stable dispatch boundary for shell-specific integrations.
///
/// The trait is sealed: consumers may select an implementation, but only ghis
/// can add one. This keeps installation and health semantics coherent with the
/// corresponding renderer.
pub trait ShellRenderer: private::Sealed + Sync {
    fn spec(&self) -> ShellSpec;
    fn render_init(&self, binary: &str) -> String;
    fn render_completion(&self, command: &mut Command, binary: &str) -> Vec<u8>;
    fn startup_file(&self, home: &Path) -> PathBuf;
    fn install(
        &self,
        startup_file: &Path,
        init_file: &Path,
        init_script: &str,
    ) -> io::Result<SetupReport>;
    fn uninstall(&self, startup_file: &Path) -> io::Result<bool>;
    fn integration_is_loaded(&self) -> bool;
    fn integration_health(&self) -> IntegrationHealth;
    /// The complete generated integration marker used for active-renderer selection.
    fn healthy_marker(&self) -> &'static str;
    fn inherited_health_marker(&self) -> Option<OsString>;
    fn integration_is_installed(&self, startup_file: &Path, init_file: &Path, binary: &str)
    -> bool;
    fn setup_command(&self) -> &'static str;
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum ShellError {
    #[error("未知 shell `{0}`；可用值：zsh、bash、fish、powershell")]
    Unknown(String),
    #[error("无法检测当前 shell；请显式指定 zsh、bash、fish 或 powershell")]
    Undetected,
    #[error("shell `{0}` 尚未支持；当前仅支持 zsh")]
    Unsupported(ShellKind),
}

/// Resolve a requested shell. An explicit argument always wins; otherwise use
/// shell-provided environment hints and fail rather than guessing.
pub fn detect_shell(explicit: Option<ShellKind>) -> Result<ShellKind, ShellError> {
    detect_shell_from(
        explicit,
        std::env::var_os("SHELL").as_deref(),
        std::env::var_os("ZDOTDIR").as_deref(),
        std::env::var_os("PSModulePath").as_deref(),
    )
}

/// Testable shell detection using the same precedence as [`detect_shell`].
/// Explicit selection wins. Shell-specific current-process indicators (`ZDOTDIR`
/// and `PSModulePath`) take precedence over `$SHELL`, which describes the login
/// shell and can differ from the shell running ghis.
pub fn detect_shell_from(
    explicit: Option<ShellKind>,
    shell: Option<&OsStr>,
    zdotdir: Option<&OsStr>,
    ps_module_path: Option<&OsStr>,
) -> Result<ShellKind, ShellError> {
    if let Some(shell) = explicit {
        return Ok(shell);
    }
    let login_shell = shell
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            Path::new(value)
                .file_name()
                .and_then(OsStr::to_str)
                .unwrap_or(value)
                .trim_end_matches(".exe")
                .parse()
        });
    let has_zsh_hint = zdotdir.is_some_and(|value| !value.is_empty());
    let has_powershell_hint = ps_module_path.is_some_and(|value| !value.is_empty());

    if matches!(login_shell, Some(Err(_))) && (has_zsh_hint || has_powershell_hint) {
        return Err(ShellError::Undetected);
    }
    match (has_zsh_hint, has_powershell_hint) {
        (true, false) => Ok(ShellKind::Zsh),
        (false, true) => Ok(ShellKind::PowerShell),
        (true, true) => Err(ShellError::Undetected),
        (false, false) => login_shell.unwrap_or(Err(ShellError::Undetected)),
    }
}

/// Concrete renderers registered by ghis.
///
/// Adding a shell requires its sibling module plus one entry here; unsupported
/// recognised shells fail closed without duplicating another renderer match.
static RENDERERS: [&dyn ShellRenderer; 3] =
    [&zsh::RENDERER, &fish::RENDERER, &powershell::RENDERER];

/// Renderer selection for diagnostics that inspect the inherited health marker.
///
/// A recognised marker selects exactly one renderer. Unknown or absent markers
/// retain the historical zsh fallback. A marker shared by multiple renderers is
/// deliberately not attributed to any shell, so diagnostics cannot mistakenly
/// report a healthy wrapper.
pub enum ActiveRenderer {
    Matched(&'static dyn ShellRenderer),
    Fallback(&'static dyn ShellRenderer),
    Ambiguous,
}

/// Select the renderer that emitted the inherited versioned health marker.
pub fn active_renderer() -> ActiveRenderer {
    let fallback = RENDERERS[0];
    let Some(marker) = fallback.inherited_health_marker() else {
        return ActiveRenderer::Fallback(fallback);
    };
    let mut matches = RENDERERS
        .iter()
        .copied()
        .filter(|renderer| marker.as_os_str() == OsStr::new(renderer.healthy_marker()));
    let Some(renderer) = matches.next() else {
        return ActiveRenderer::Fallback(fallback);
    };
    if matches.next().is_some() {
        ActiveRenderer::Ambiguous
    } else {
        ActiveRenderer::Matched(renderer)
    }
}

/// Return the renderer for a shell, refusing known but unimplemented shells.
pub fn renderer(kind: ShellKind) -> Result<&'static dyn ShellRenderer, ShellError> {
    RENDERERS
        .iter()
        .copied()
        .find(|renderer| renderer.spec().kind() == kind)
        .ok_or(ShellError::Unsupported(kind))
}

/// Render initialization source only after confirming implementation support.
pub fn render_init(kind: ShellKind, binary: &str) -> Result<String, ShellError> {
    Ok(renderer(kind)?.render_init(binary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn shell_kind_parses_and_displays_canonical_names() {
        for (input, expected, display) in [
            ("zsh", ShellKind::Zsh, "zsh"),
            ("bash", ShellKind::Bash, "bash"),
            ("fish", ShellKind::Fish, "fish"),
            ("powershell", ShellKind::PowerShell, "powershell"),
            ("pwsh", ShellKind::PowerShell, "powershell"),
        ] {
            assert_eq!(input.parse(), Ok(expected));
            assert_eq!(expected.to_string(), display);
        }
        assert_eq!(ShellKind::Zsh.to_string(), "zsh");
        assert_eq!(ShellKind::Bash.to_string(), "bash");
        assert_eq!(ShellKind::Fish.to_string(), "fish");
        assert_eq!(ShellKind::PowerShell.to_string(), "powershell");
    }

    #[test]
    fn explicit_and_current_shell_hints_precede_login_shell() {
        assert_eq!(
            detect_shell_from(
                Some(ShellKind::Fish),
                Some(OsStr::new("/usr/bin/zsh")),
                Some(OsStr::new("/tmp/zsh")),
                Some(OsStr::new("PowerShell")),
            ),
            Ok(ShellKind::Fish)
        );
        assert_eq!(
            detect_shell_from(
                None,
                Some(OsStr::new("/usr/bin/bash")),
                Some(OsStr::new("/tmp/zsh")),
                None,
            ),
            Ok(ShellKind::Zsh)
        );
        assert_eq!(
            detect_shell_from(
                None,
                Some(OsStr::new("/usr/bin/zsh")),
                None,
                Some(OsStr::new("PowerShell")),
            ),
            Ok(ShellKind::PowerShell)
        );
        assert_eq!(
            detect_shell_from(
                None,
                Some(OsStr::new("/usr/bin/nu")),
                Some(OsStr::new("/tmp/zsh")),
                None,
            ),
            Err(ShellError::Undetected)
        );
        assert_eq!(
            detect_shell_from(
                None,
                Some(OsStr::new("/usr/bin/nu")),
                None,
                Some(OsStr::new("PowerShell")),
            ),
            Err(ShellError::Undetected)
        );
    }

    #[test]
    fn exact_renderers_are_registered_and_other_shells_fail_closed() {
        assert!(matches!(
            "nu".parse::<ShellKind>(),
            Err(ShellError::Unknown(_))
        ));
        assert_eq!(
            renderer(ShellKind::Fish).expect("fish renderer").spec(),
            ShellKind::Fish.spec()
        );
        assert!(matches!(
            renderer(ShellKind::Bash),
            Err(ShellError::Unsupported(ShellKind::Bash))
        ));
        assert_eq!(
            render_init(ShellKind::PowerShell, "ghis"),
            Ok(powershell::powershell_init_script("ghis"))
        );
    }

    #[test]
    fn facade_delegates_to_the_zsh_module_and_renderer() {
        let renderer = renderer(ShellKind::Zsh).expect("zsh renderer");
        assert_eq!(renderer.spec(), ShellKind::Zsh.spec());
        assert_eq!(renderer.render_init("ghis"), zsh::zsh_init_script("ghis"));
        assert_eq!(zsh_init_script("ghis"), zsh::zsh_init_script("ghis"));
        assert_eq!(
            managed_block(Path::new("/tmp/init.zsh")),
            zsh::managed_block(Path::new("/tmp/init.zsh"))
        );
        assert_eq!(
            ShellKind::Fish.spec().install_strategy(),
            InstallStrategy::DropInFile
        );
    }
    #[test]
    fn renderer_installer_writes_the_renderer_output() {
        let directory = tempfile::tempdir().expect("tempdir");
        let startup_file = directory.path().join(".zshrc");
        let init_file = directory.path().join("init.zsh");
        let renderer = renderer(ShellKind::Zsh).expect("zsh renderer");
        let script = renderer.render_init("custom-ghis");

        renderer
            .install(&startup_file, &init_file, &script)
            .expect("install rendered script");
        assert_eq!(fs::read_to_string(init_file).unwrap(), script);
        assert!(renderer.uninstall(&startup_file).expect("uninstall"));
    }
}
