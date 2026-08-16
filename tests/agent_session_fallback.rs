#[cfg(windows)]
#[test]
fn windows_private_shims_are_native_executable_copies() {
    let source = assert_cmd::cargo::cargo_bin!("ghis");

    let source_size = std::fs::metadata(&source).unwrap().len();
    assert!(source_size > 0);

    let shims = ghis::agent::SessionShims::create(&source).unwrap();
    for command in ["git.exe", "gh.exe"] {
        assert_eq!(
            std::fs::metadata(shims.directory().join(command))
                .unwrap()
                .len(),
            source_size
        );
    }
    assert!(!shims.directory().join("git.cmd").exists());
    assert!(!shims.directory().join("gh.cmd").exists());
}

#[cfg(not(any(unix, windows)))]
#[test]
fn private_shims_fail_closed_on_other_unsupported_platforms() {
    let error = ghis::agent::SessionShims::create(std::path::Path::new("ghis"))
        .err()
        .expect("unsupported platforms must fail closed without a native shim");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    assert!(
        error
            .to_string()
            .contains("argv-transparent native launcher")
    );
}
