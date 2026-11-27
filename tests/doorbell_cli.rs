#![cfg(unix)]
//! Built-binary tests for the doorbell commands' user-visible errors. The
//! production call site (`roster`/`send` -> `send_within` -> transport) must
//! print the same transport text as before the deadline existed, and the plain
//! timeout message after it, with the same exit status.

use std::{
    io::Read,
    os::unix::net::UnixListener,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// A listener that accepts one connection and never answers, so the command's
/// own deadline is the only way out. The accepted flag proves the server read
/// (accepted) the connection rather than leaving it queued.
fn silent_listener() -> (tempfile::TempDir, PathBuf, Arc<AtomicBool>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silent.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let accepted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&accepted);
    thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            flag.store(true, Ordering::SeqCst);
            thread::sleep(Duration::from_secs(20));
            drop(stream);
        }
    });
    (dir, path, accepted)
}

/// Runs the built binary with a token and the given socket, and returns the
/// exit code and stderr. A watchdog keeps a stalled command from hanging the
/// suite.
fn run(arguments: &[&str], socket: &Path) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_crowded"));
    for variable in ["CROWDED_ROOM", "CROWDED_BIN", "CROWDED_SOCKET"] {
        command.env_remove(variable);
    }
    command
        .args(arguments)
        .env("CROWDED_TOKEN", "test-token")
        .env("CROWDED_SOCKET", socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "the doorbell command must stay bounded"
        );
        thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    (status.code(), stderr)
}

#[test]
fn roster_prints_the_os_error_for_a_missing_socket() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.sock");
    let (code, stderr) = run(&["roster"], &missing);
    assert_eq!(code, Some(1));
    assert!(stderr.starts_with("Error: Os {"), "{stderr}");
    assert!(stderr.contains("No such file or directory"), "{stderr}");
    assert!(!stderr.contains("Transport("), "{stderr}");
}

#[test]
fn roster_times_out_against_an_accepting_silent_listener() {
    let (_dir, path, accepted) = silent_listener();
    let started = Instant::now();
    let (code, stderr) = run(&["roster"], &path);
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr.trim(),
        "Error: Crowded did not answer within 10 seconds"
    );
    assert!(started.elapsed() >= Duration::from_secs(10));
    assert!(started.elapsed() < Duration::from_secs(16));
    assert!(
        accepted.load(Ordering::SeqCst),
        "the silent server must have accepted the connection"
    );
}

#[test]
fn send_timeout_tells_the_caller_not_to_resend() {
    let (_dir, path, _accepted) = silent_listener();
    let (code, stderr) = run(&["send", "2", "--", "hello"], &path);
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr.trim(),
        "Error: Crowded did not answer within 10 seconds; the request may have been queued, do not resend"
    );
}
