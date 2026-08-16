#[cfg(windows)]
#[test]
fn windows_private_shims_fail_closed_without_an_argv_transparent_native_launcher() {
    let error = ghis::agent::SessionShims::create(std::path::Path::new("ghis.exe"))
        .err()
        .expect("Windows must fail closed without a native shim");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    let message = error.to_string();
    assert!(message.contains("Windows agent launcher is currently unavailable"));
    assert!(message.contains("argv-transparent native launcher"));
    assert!(message.contains("run the target CLI directly"));
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
