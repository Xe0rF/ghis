use assert_cmd::cargo::cargo_bin;
use sha2::{Digest, Sha256};
use std::process::Command;

fn zsh_cli_output(command: &str) -> Vec<u8> {
    let output = Command::new(cargo_bin!("ghis"))
        .args([command, "zsh"])
        .output()
        .expect("run ghis zsh renderer");
    assert!(
        output.status.success(),
        "{command} zsh failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn zsh_cli_output_bytes_match_golden() {
    assert_eq!(
        sha256_hex(&zsh_cli_output("init")),
        "89effa45f19ac819ab3f9c9f86440ba18bb2250bce043451fcf4e23109c8ce0f",
        "init zsh bytes changed"
    );
    assert_eq!(
        sha256_hex(&zsh_cli_output("completion")),
        "1352a06f3b6502fe8edead05804358e7b3037e8ab2017cc8fc3a80877477e5c8",
        "completion zsh bytes changed"
    );
}
