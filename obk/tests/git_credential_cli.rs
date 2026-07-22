//! End-to-end tests of the `obk git-credential` helper contract against the
//! real binary: a recognized github.com request must emit `quit=true` (exit
//! 0) on every failure — never fall through to another credential helper —
//! while non-GitHub hosts decline quietly (exit 1, no output).

use std::io::Write;
use std::process::{Command, Stdio};

/// Run `obk git-credential get` with the given stdin and optional
/// OCTOBROKER_KEY. OCTOBROKER_URL points at a closed port so any fetch attempt
/// fails without touching the network.
fn run_get(input: &str, key: Option<&str>) -> (String, i32) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_obk"));
    cmd.args(["git-credential", "get"])
        .env_remove("OCTOBROKER_KEY")
        .env("OCTOBROKER_URL", "http://127.0.0.1:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(k) = key {
        cmd.env("OCTOBROKER_KEY", k);
    }
    let mut child = cmd.spawn().expect("spawn obk");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().expect("wait obk");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        out.status.code().expect("exit code"),
    )
}

#[test]
fn recognized_github_missing_key_quits() {
    let (stdout, code) = run_get("protocol=https\nhost=github.com\npath=o/r.git\n\n", None);
    assert_eq!(stdout, "quit=true\n");
    assert_eq!(code, 0);
}

#[test]
fn recognized_github_host_variants_quit_on_failure() {
    // case-insensitive host + explicit :443 port are still recognized;
    // with the server unreachable the helper must quit, not fall through
    for host in ["GitHub.com", "github.com:443"] {
        let input = format!("protocol=https\nhost={}\npath=o/r.git\n\n", host);
        let (stdout, code) = run_get(&input, Some("some-key"));
        assert_eq!(stdout, "quit=true\n", "host: {}", host);
        assert_eq!(code, 0, "host: {}", host);
    }
}

#[test]
fn recognized_github_missing_path_quits() {
    let (stdout, code) = run_get("protocol=https\nhost=github.com\n\n", Some("some-key"));
    assert_eq!(stdout, "quit=true\n");
    assert_eq!(code, 0);
}

#[test]
fn non_github_host_declines_quietly() {
    let (stdout, code) = run_get(
        "protocol=https\nhost=gitlab.com\npath=o/r.git\n\n",
        Some("some-key"),
    );
    assert_eq!(stdout, "");
    assert_eq!(code, 1);
}

#[test]
fn store_and_erase_are_noops() {
    for op in ["store", "erase"] {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_obk"));
        let out = cmd
            .args(["git-credential", op])
            .env_remove("OCTOBROKER_KEY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "op: {}", op);
        assert!(out.stdout.is_empty(), "op: {}", op);
    }
}
