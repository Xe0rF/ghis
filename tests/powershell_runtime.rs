#![cfg(windows)]

use ghis::shell::powershell::powershell_init_script;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type TestResult = Result<Output, String>;

fn run_pwsh(driver: &Path, config: &Path) -> TestResult {
    Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-File"])
        .arg(driver)
        .env("GHIS_CONFIG", config)
        .env_remove("GHIS_BYPASS")
        .env_remove("GHIS_WRAPPER_ACTIVE")
        .output()
        .map_err(|error| format!("pwsh is required for the explicit Windows runtime test: {error}"))
}

fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n")
}

#[test]
#[ignore = "runtime-deferred: requires pwsh; Windows CI runs this explicitly"]
fn pwsh_real_ghis_dispatch_preserves_runtime_contract() {
    let directory = tempfile::tempdir().expect("tempdir");
    let init = directory.path().join("ghis.ps1");
    let driver = directory.path().join("driver.ps1");
    let config = directory.path().join("config.toml");
    let ghis = std::env::var_os("CARGO_BIN_EXE_ghis").expect("Cargo must provide ghis binary");
    let ghis = ghis.to_str().expect("ghis path is Unicode");

    fs::write(&init, powershell_init_script(ghis)).expect("init script");
    fs::write(&config, "").expect("isolated config");
    fs::write(
        &driver,
        format!(
            ". '{}'\n$version = git --version\nif ($LASTEXITCODE -ne 0) {{ exit 10 }}\nWrite-Output \"VERSION=$version\"\n$argv = git rev-parse --sq-quote 'alpha beta' -- 'gamma delta'\nif ($LASTEXITCODE -ne 0) {{ exit 11 }}\nWrite-Output \"ARGV=$argv\"\n$env:GHIS_WRAPPER_ACTIVE = '1'\n$guarded = git --version\nif ($LASTEXITCODE -ne 0) {{ exit 12 }}\nWrite-Output \"GUARDED=$guarded\"\nRemove-Item Env:GHIS_WRAPPER_ACTIVE -ErrorAction SilentlyContinue\n",
            ghis::shell::powershell::powershell_quote(init.to_str().expect("Unicode init path")),
        ),
    )
    .expect("driver script");

    let output = run_pwsh(&driver, &config).expect("run PowerShell wrapper");
    assert!(output.status.success(), "{}", output_text(&output.stderr));
    let stdout = output_text(&output.stdout);
    assert!(stdout.contains("VERSION=git version "), "{stdout:?}");
    assert!(
        stdout.contains("ARGV= 'alpha beta' '--' 'gamma delta'"),
        "argv was not preserved: {stdout:?}"
    );
    assert!(stdout.contains("GUARDED=git version "), "{stdout:?}");

    let failing_driver = directory.path().join("failing-driver.ps1");
    fs::write(
        &failing_driver,
        format!(
            ". '{}'\ngit definitely-not-a-ghis-test-command\nexit $LASTEXITCODE\n",
            ghis::shell::powershell::powershell_quote(init.to_str().expect("Unicode init path")),
        ),
    )
    .expect("failing driver");
    let expected = Command::new("git")
        .args(["definitely-not-a-ghis-test-command"])
        .output()
        .expect("git is required for the runtime test");
    let failure = run_pwsh(&failing_driver, &config).expect("run failing PowerShell wrapper");
    assert_eq!(failure.status.code(), expected.status.code());
    assert!(!failure.status.success());
}
