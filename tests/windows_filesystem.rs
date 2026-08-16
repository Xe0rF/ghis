#![cfg(windows)]

use ghis::shell::powershell::{END_MARKER, HEALTHY_MARKER, START_MARKER};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const GHIS_CONTROL_ENV: [&str; 12] = [
    "GHIS_CONFIG",
    "GHIS_PROFILE",
    "GHIS_BANNER_SHOWN",
    "GHIS_WRAPPER_ACTIVE",
    "GHIS_BYPASS",
    "GHIS_DISABLE_CHPWD",
    "GHIS_CHPWD_ENABLED",
    "GHIS_REPO_PROFILE",
    "GHIS_REPO_PROFILE_DISPLAY",
    "GHIS_REPO_ROOT",
    "GHIS_SHELL_INTEGRATION",
    "GHIS_SHELL_INTEGRATION_HEALTH",
];

#[derive(Debug)]
struct WindowsLayout {
    root: PathBuf,
    user_profile: PathBuf,
    appdata: PathBuf,
    local_appdata: PathBuf,
}

impl WindowsLayout {
    fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            user_profile: root.join("User Profile With Spaces"),
            appdata: root.join("Roaming App Data"),
            local_appdata: root.join("Local App Data"),
            root,
        }
    }

    fn profile(&self) -> PathBuf {
        self.user_profile
            .join("Documents")
            .join("PowerShell")
            .join("Microsoft.PowerShell_profile.ps1")
    }

    fn init(&self) -> PathBuf {
        self.appdata.join("ghis").join("ghis.ps1")
    }

    fn backup(&self) -> PathBuf {
        PathBuf::from(format!("{}.ghis.bak", self.profile().display()))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ghis"));
        command
            .current_dir(&self.root)
            .env("USERPROFILE", &self.user_profile)
            .env("APPDATA", &self.appdata)
            .env("LOCALAPPDATA", &self.local_appdata)
            // These deliberately point elsewhere: native Windows path discovery
            // must not fall back to HOME or any XDG variable.
            .env("HOME", self.root.join("Wrong Home"))
            .env("XDG_CONFIG_HOME", self.root.join("Wrong XDG Config"))
            .env("XDG_CACHE_HOME", self.root.join("Wrong XDG Cache"))
            .env("XDG_STATE_HOME", self.root.join("Wrong XDG State"));
        for key in GHIS_CONTROL_ENV {
            command.env_remove(key);
        }
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command()
            .args(args)
            .output()
            .unwrap_or_else(|error| panic!("run ghis {args:?}: {error}"))
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

fn assert_success(output: &Output, operation: &str) {
    assert!(
        output.status.success(),
        "{operation} failed with {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn prepend_path(directory: &Path) -> OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    std::env::join_paths(
        std::iter::once(directory.to_path_buf()).chain(std::env::split_paths(&inherited)),
    )
    .expect("PATH should accept the temporary executable directory")
}

#[test]
fn powershell_setup_and_uninstall_use_native_roots_and_preserve_crlf_user_content() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let layout = WindowsLayout::new(temporary.path());
    let profile = layout.profile();
    fs::create_dir_all(profile.parent().expect("profile parent")).expect("profile directory");
    let original = b"$env:USER_SETTING = 'keep me'\r\n# user-owned CRLF content\r\n";
    fs::write(&profile, original).expect("initial PowerShell profile");

    let first = layout.run(&["setup", "powershell", "--yes"]);
    assert_success(&first, "explicit PowerShell setup");
    assert!(text(&first.stdout).contains("powershell 集成已安装"));
    assert_eq!(
        fs::read(layout.backup()).expect("one-time backup"),
        original
    );

    let installed = fs::read_to_string(&profile).expect("installed profile");
    assert!(installed.starts_with("$env:USER_SETTING = 'keep me'\r\n"));
    assert_eq!(installed.matches(START_MARKER).count(), 1);
    assert_eq!(installed.matches(END_MARKER).count(), 1);
    assert!(layout.init().is_file(), "APPDATA must own ghis.ps1");
    let init = fs::read_to_string(layout.init()).expect("PowerShell init");
    assert!(init.contains("Get-Command -Name 'ghis' -CommandType Application"));

    // Exercise APPDATA and LOCALAPPDATA in one real CLI process. Profile writes
    // config/fragments below roaming data and creates cache/state below local data.
    let add = layout.run(&[
        "profile",
        "add",
        "windows-e2e",
        "--login",
        "windows-e2e",
        "--name",
        "Windows E2E",
        "--email",
        "windows-e2e@example.test",
    ]);
    assert_success(&add, "profile add with native Windows roots");
    assert!(layout.appdata.join("ghis/config.toml").is_file());
    assert!(
        layout
            .appdata
            .join("ghis/fragments/windows-e2e.gitconfig")
            .is_file()
    );
    assert!(layout.local_appdata.join("ghis").is_dir());
    assert!(!layout.root.join("Wrong Home").exists());
    assert!(!layout.root.join("Wrong XDG Config").exists());
    assert!(!layout.root.join("Wrong XDG Cache").exists());
    assert!(!layout.root.join("Wrong XDG State").exists());

    let repeated = layout.run(&["setup", "pwsh", "-y"]);
    assert_success(&repeated, "repeated pwsh alias setup");
    assert!(text(&repeated.stdout).contains("powershell 集成无需更新"));
    let repeated_profile = fs::read_to_string(&profile).expect("repeated profile");
    assert_eq!(repeated_profile.matches(START_MARKER).count(), 1);
    assert_eq!(fs::read(layout.backup()).expect("stable backup"), original);

    // On Windows the omitted shell is PowerShell. Uninstall owns only its block;
    // the generated init and backup intentionally remain available for recovery.
    let uninstall = layout.run(&["uninstall"]);
    assert_success(&uninstall, "implicit PowerShell uninstall");
    assert!(text(&uninstall.stdout).contains("移除 ghis 管理的 powershell 集成"));
    assert_eq!(fs::read(&profile).expect("uninstalled profile"), original);
    assert!(layout.init().is_file());
    assert_eq!(
        fs::read(layout.backup()).expect("retained backup"),
        original
    );

    let repeated_uninstall = layout.run(&["uninstall", "powershell"]);
    assert_success(&repeated_uninstall, "repeated explicit uninstall");
    assert!(text(&repeated_uninstall.stdout).contains("未在"));
    assert_eq!(fs::read(profile).expect("user profile remains"), original);
}

#[test]
fn pwsh_loads_the_cli_installed_profile_and_finds_ghis_exe_through_pathext() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let layout = WindowsLayout::new(temporary.path());
    fs::create_dir_all(&layout.root).expect("test root");

    let setup = layout.run(&["setup", "powershell", "--yes"]);
    assert_success(&setup, "PowerShell setup before runtime probe");

    let executable_directory = layout.root.join("Ghis Program Files");
    fs::create_dir_all(&executable_directory).expect("spaced executable directory");
    let copied_ghis = executable_directory.join("ghis.exe");
    fs::copy(env!("CARGO_BIN_EXE_ghis"), &copied_ghis).expect("copy ghis.exe runtime probe");

    let driver = layout.root.join("PowerShell Driver With Spaces.ps1");
    fs::write(
        &driver,
        format!(
            r#". $env:GHIS_TEST_PROFILE
if ($PSVersionTable.PSVersion.Major -lt 7) {{ exit 19 }}
if ($env:GHIS_SHELL_INTEGRATION -ne '1') {{ exit 20 }}
if ($env:GHIS_SHELL_INTEGRATION_HEALTH -ne '{HEALTHY_MARKER}') {{ exit 21 }}
$application = Get-Command -Name ghis -CommandType Application -ErrorAction Stop | Select-Object -First 1
if ([IO.Path]::GetExtension($application.Path) -ine '.exe') {{ exit 22 }}
Write-Output "APP=$($application.Path)"
$ghisVersion = & ghis --version | Select-Object -First 1
if ($LASTEXITCODE -ne 0 -or -not $ghisVersion.StartsWith('ghis ')) {{ exit 23 }}
$gitVersion = git --version
if ($LASTEXITCODE -ne 0 -or -not $gitVersion.StartsWith('git version ')) {{ exit 24 }}
Write-Output "GHIS=$ghisVersion"
Write-Output "GIT=$gitVersion"
"#
        ),
    )
    .expect("PowerShell driver");

    let mut command = Command::new("pwsh");
    command
        .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-File"])
        .arg(&driver)
        .current_dir(&layout.root)
        .env("USERPROFILE", &layout.user_profile)
        .env("APPDATA", &layout.appdata)
        .env("LOCALAPPDATA", &layout.local_appdata)
        .env("GHIS_TEST_PROFILE", layout.profile())
        .env("PATH", prepend_path(&executable_directory))
        // The command is intentionally extensionless. It must resolve the copied
        // ghis.exe using native Windows PATHEXT application lookup.
        .env("PATHEXT", ".EXE;.COM;.BAT;.CMD");
    for key in GHIS_CONTROL_ENV {
        command.env_remove(key);
    }
    let output = command
        .output()
        .expect("PowerShell 7 (pwsh) is required by the Windows integration suite");
    assert_success(&output, "PowerShell 7 profile/runtime probe");
    let stdout = text(&output.stdout);
    let application = stdout
        .lines()
        .find_map(|line| line.strip_prefix("APP="))
        .expect("driver must report the PATHEXT-resolved application");
    assert_eq!(
        Path::new(application).file_name(),
        Some(std::ffi::OsStr::new("ghis.exe"))
    );
    assert_eq!(
        Path::new(application).parent().and_then(Path::file_name),
        Some(std::ffi::OsStr::new("Ghis Program Files")),
        "PowerShell resolved a different ghis application: {application}"
    );
    assert!(stdout.contains("GHIS=ghis "), "{stdout}");
    assert!(stdout.contains("GIT=git version "), "{stdout}");

    let uninstall = layout.run(&["uninstall", "pwsh"]);
    assert_success(&uninstall, "PowerShell uninstall after runtime probe");
}

#[test]
fn setup_fails_on_a_readonly_profile_without_leaving_partial_files() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let layout = WindowsLayout::new(temporary.path());
    let profile = layout.profile();
    fs::create_dir_all(profile.parent().expect("profile parent")).expect("profile directory");
    let original = b"# readonly user profile\r\n";
    fs::write(&profile, original).expect("readonly profile");
    let mut permissions = fs::metadata(&profile)
        .expect("profile metadata")
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&profile, permissions).expect("set readonly attribute");

    let setup = layout.run(&["setup", "powershell", "--yes"]);
    assert!(
        !setup.status.success(),
        "setup unexpectedly replaced a readonly Windows profile\nstdout:\n{}\nstderr:\n{}",
        text(&setup.stdout),
        text(&setup.stderr)
    );
    assert!(
        !setup.stderr.is_empty(),
        "readonly failure must be reported"
    );
    assert_eq!(
        fs::read(&profile).expect("readonly profile remains"),
        original
    );
    assert!(
        !layout.init().exists(),
        "failed setup must roll back ghis.ps1"
    );
    assert!(
        !layout.backup().exists(),
        "failed setup must roll back its backup"
    );

    let mut permissions = fs::metadata(&profile)
        .expect("profile metadata")
        .permissions();
    // This Windows-only test clears FILE_ATTRIBUTE_READONLY; the Unix mode-bit
    // concern behind this lint does not apply.
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    fs::set_permissions(profile, permissions).expect("clear readonly attribute for cleanup");
}

#[test]
#[ignore = "capability-required: set GHIS_TEST_UNC_ROOT to a provisioned writable UNC share and run this test explicitly"]
fn powershell_setup_supports_a_provisioned_unc_share() {
    let root = PathBuf::from(
        std::env::var_os("GHIS_TEST_UNC_ROOT")
            .expect("GHIS_TEST_UNC_ROOT must name a provisioned writable UNC share"),
    );
    assert!(
        root.to_string_lossy().starts_with(r"\\"),
        "GHIS_TEST_UNC_ROOT must be a UNC path, got {}",
        root.display()
    );
    let layout = WindowsLayout::new(root.join(format!("ghis-unc-e2e-{}", std::process::id())));
    fs::create_dir_all(&layout.root).expect("create isolated UNC test root");
    let setup = layout.run(&["setup", "powershell", "--yes"]);
    assert_success(&setup, "UNC PowerShell setup");
    assert!(layout.profile().is_file());
    assert!(layout.init().is_file());
    let uninstall = layout.run(&["uninstall", "powershell"]);
    assert_success(&uninstall, "UNC PowerShell uninstall");
    fs::remove_dir_all(&layout.root).expect("remove isolated UNC test root");
}

#[test]
#[ignore = "capability-required: set GHIS_TEST_LONG_PATH_ROOT to a writable >260-character path on a long-path-enabled runner"]
fn powershell_setup_supports_a_provisioned_long_path() {
    let root = PathBuf::from(
        std::env::var_os("GHIS_TEST_LONG_PATH_ROOT")
            .expect("GHIS_TEST_LONG_PATH_ROOT must name a provisioned writable long path"),
    );
    assert!(root.is_absolute(), "long-path root must be absolute");
    assert!(
        root.to_string_lossy().encode_utf16().count() > 260,
        "GHIS_TEST_LONG_PATH_ROOT must already exceed 260 UTF-16 code units"
    );
    let layout = WindowsLayout::new(root);
    fs::create_dir_all(&layout.root).expect("create long-path test root");
    let setup = layout.run(&["setup", "powershell", "--yes"]);
    assert_success(&setup, "long-path PowerShell setup");
    assert!(layout.profile().is_file());
    assert!(layout.init().is_file());
    let uninstall = layout.run(&["uninstall", "powershell"]);
    assert_success(&uninstall, "long-path PowerShell uninstall");
}
