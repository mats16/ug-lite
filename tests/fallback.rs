//! Commands ug-lite does not implement go to the original `ug`: `$UG_LITE_FALLBACK`, else
//! `~/.local/bin/ug`, regardless of PATH.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Where the original `ug` installs itself.
const UG: &str = ".local/bin/ug";

/// A temp HOME holding a fake original `ug` at UG that echoes its arguments.
fn fake_home(name: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!("ug-lite-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let ug = home.join(UG);
    fs::create_dir_all(ug.parent().unwrap()).unwrap();
    fs::write(
        &ug,
        "#!/bin/sh\ncase \"$*\" in --version|-V) echo 9.9.9+fake; exit 0 ;; esac\necho \"args:$*\"; exit 7\n",
    )
    .unwrap();
    fs::set_permissions(&ug, fs::Permissions::from_mode(0o755)).unwrap();
    home
}

fn run(home: &Path, fallback: Option<&Path>, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ug-lite"));
    // An empty PATH: finding the original must not depend on it.
    c.args(args)
        .env("HOME", home)
        .env("PATH", "")
        .env_remove("UG_LITE_VISITED");
    match fallback {
        Some(f) => c.env("UG_LITE_FALLBACK", f),
        None => c.env_remove("UG_LITE_FALLBACK"),
    };
    c.output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn forwards_to_home_local_bin_ug_by_default() {
    let home = fake_home("default");
    let out = run(&home, None, &["configure", "--x"]);
    assert_eq!(out.status.code(), Some(7), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "args:configure --x\n");
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn env_var_overrides_the_default() {
    let home = fake_home("override");
    let other = home.join("other-ug");
    fs::write(&other, "#!/bin/sh\necho other:$*\n").unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
    let out = run(&home, Some(&other), &["configure"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "other:configure\n");
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn version_is_the_original_untouched() {
    let home = fake_home("version");
    for flag in ["--version", "-V"] {
        let out = run(&home, None, &[flag]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        // Launchers parse it as the version, so it is exactly the original's.
        assert_eq!(text(&out.stdout), "9.9.9+fake\n");
        assert!(out.stderr.is_empty(), "{}", text(&out.stderr));
    }
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn help_lists_ug_lite_commands_above_the_original_help() {
    let home = fake_home("help");
    let out = run(&home, None, &["--help"]);
    assert_eq!(out.status.code(), Some(7), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    let ug = std::path::absolute(home.join(UG)).unwrap();
    let header = format!(
        "ug-lite {} runs these commands itself:\n",
        env!("CARGO_PKG_VERSION")
    );
    assert!(stdout.starts_with(&header), "{stdout}");
    for want in ["  mcp-proxy ", "  auth-token "] {
        assert!(stdout.contains(want), "{stdout}");
    }
    // The original's help comes last, after the path it was run from.
    assert!(
        stdout.ends_with(&format!("  {}\nargs:--help\n", ug.display())),
        "{stdout}"
    );
    // `ug --help` with a subcommand is the original's alone.
    let out = run(&home, None, &["configure", "--help"]);
    assert_eq!(text(&out.stdout), "args:configure --help\n");
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn missing_original_or_itself_exits_2() {
    let home = fake_home("missing");
    fs::remove_file(home.join(UG)).unwrap();
    let out = run(&home, None, &["--help"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stdout).contains("\n  unavailable: "),
        "{}",
        text(&out.stdout)
    );
    let err = text(&out.stderr);
    assert!(err.contains("UG_LITE_FALLBACK"), "{err}");

    // Present but not executable: still "nowhere to forward" (exit 2), not a failed exec.
    let plain = home.join("plain-ug");
    fs::write(&plain, "").unwrap();
    let out = run(&home, Some(&plain), &["configure"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("is not executable"),
        "{}",
        text(&out.stderr)
    );

    let me = Path::new(env!("CARGO_BIN_EXE_ug-lite"));
    let out = run(&home, Some(me), &["configure"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("is ug-lite, not the original"),
        "{}",
        text(&out.stderr)
    );
    fs::remove_dir_all(home).unwrap();
}

#[test]
fn a_copy_that_already_forwarded_is_never_run_again() {
    // UG_LITE_VISITED says B already forwarded here, and this binary's target is B. B is a plain
    // script, so ignoring the record would visibly run it (exit 7) instead of refusing (exit 2).
    let home = fake_home("loop");
    let b = home.join(UG);
    let out = Command::new(env!("CARGO_BIN_EXE_ug-lite"))
        .arg("configure")
        .env("HOME", &home)
        .env("PATH", "")
        .env("UG_LITE_FALLBACK", &b)
        .env("UG_LITE_VISITED", b.canonicalize().unwrap())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(
        text(&out.stderr).contains("is ug-lite, not the original"),
        "{}",
        text(&out.stderr)
    );
    fs::remove_dir_all(home).unwrap();
}
