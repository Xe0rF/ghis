#[cfg(not(unix))]
#[test]
fn private_shims_fail_closed_without_a_platform_safe_implementation() {
    let error = ghis::agent::SessionShims::create(std::path::Path::new("ghis"))
        .err()
        .expect("unsupported platforms must not launch without managed shims");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
}
