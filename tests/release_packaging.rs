use std::process::Command;

#[test]
fn release_packaging_smoke_passes() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let status = Command::new("sh")
        .arg("scripts/test-package-release.sh")
        .current_dir(manifest_dir)
        .status()
        .expect("run release packaging smoke test");

    assert!(status.success(), "release packaging smoke test failed");
}
