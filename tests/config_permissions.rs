#![cfg(unix)]

use ghis::config::ConfigPaths;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[test]
fn config_directories_are_private_even_when_preexisting() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let paths = ConfigPaths::from_bases(
        temporary.path().join("config"),
        temporary.path().join("cache"),
        temporary.path().join("state"),
    );

    for directory in [
        &paths.config_dir,
        &paths.fragments_dir,
        &paths.cache_dir,
        &paths.state_dir,
    ] {
        fs::create_dir_all(directory).expect("pre-create directory");
        fs::set_permissions(directory, fs::Permissions::from_mode(0o777))
            .expect("make directory deliberately broad");
    }

    paths.create_dirs().expect("tighten directories");

    for directory in [
        &paths.config_dir,
        &paths.fragments_dir,
        &paths.cache_dir,
        &paths.state_dir,
    ] {
        assert_eq!(
            fs::metadata(directory)
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "{} must not retain broad permissions",
            directory.display()
        );
    }
}
