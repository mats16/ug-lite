//! install.sh puts ug-lite at ~/.local/bin/ug-lite and leaves the original ~/.local/bin/ug alone.
//! The release download is served by a `curl` shim from a tarball of the binary under test.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::process::Command;

fn exe(p: &std::path::Path, body: &str) {
    fs::write(p, body).unwrap();
    fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn installs_next_to_the_original_and_reinstalls_in_place() {
    let root = std::env::temp_dir().join(format!("ug-lite-install-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (home, shim, rel) = (root.join("home"), root.join("shim"), root.join("rel"));
    let bin = home.join(".local/bin");
    for d in [&bin, &shim, &rel] {
        fs::create_dir_all(d).unwrap();
    }
    // uv's layout: ~/.local/bin/ug is a link to the original in uv's tool dir.
    let real = root.join("real-ug");
    exe(&real, "#!/bin/sh\necho original\n");
    symlink(&real, bin.join("ug")).unwrap();

    fs::copy(env!("CARGO_BIN_EXE_ug-lite"), rel.join("ug-lite")).unwrap();
    let ok = Command::new("tar")
        .args(["-czf", "ug-lite.tar.gz", "ug-lite"])
        .current_dir(&rel)
        .status()
        .unwrap();
    assert!(ok.success());
    // `curl -fsSL -o OUT URL`: any asset name gets the same tarball and a matching checksum.
    exe(
        &shim.join("curl"),
        &format!(
            "#!/bin/sh\nout=$3; url=$4; tb={tb}\n\
             case \"$url\" in\n\
             *.sha256) if command -v sha256sum >/dev/null; then h=$(sha256sum \"$tb\"); else h=$(shasum -a 256 \"$tb\"); fi\n\
             echo \"${{h%% *}}  $(basename \"$url\" .sha256)\" > \"$out\" ;;\n\
             *) cp \"$tb\" \"$out\" ;;\nesac\n",
            tb = rel.join("ug-lite.tar.gz").display()
        ),
    );

    let install = || {
        Command::new("sh")
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
            .env("HOME", &home)
            .env("PATH", format!("{}:/usr/bin:/bin", shim.display()))
            .env_remove("UG_LITE_INSTALL_DIR")
            .output()
            .unwrap()
    };
    // The second run replaces the first install in place.
    for _ in 0..2 {
        let out = install();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            fs::read(bin.join("ug-lite")).unwrap(),
            fs::read(env!("CARGO_BIN_EXE_ug-lite")).unwrap()
        );
    }
    assert_eq!(fs::read_link(bin.join("ug")).unwrap(), real);
    assert_eq!(
        fs::read_to_string(&real).unwrap(),
        "#!/bin/sh\necho original\n"
    );
    fs::remove_dir_all(root).unwrap();
}
