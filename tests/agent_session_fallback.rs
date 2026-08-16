#[cfg(not(any(unix, windows)))]
#[test]
fn private_shims_fail_closed_without_a_platform_safe_implementation() {
    let error = ghis::agent::SessionShims::create(std::path::Path::new("ghis"))
        .err()
        .expect("unsupported platforms must not launch without managed shims");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}

#[cfg(windows)]
#[test]
fn private_shims_create_windows_command_launchers() {
    let shims = ghis::agent::SessionShims::create(std::path::Path::new("ghis.exe"))
        .expect("Windows must create private command launchers");
    assert!(shims.directory().join("git.cmd").is_file());
    assert!(shims.directory().join("gh.cmd").is_file());
}
