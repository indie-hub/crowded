#![cfg(unix)]
//! Built-binary tests for `crowded session-report`: hook-facing
//! silence, no-hang stdin handling, and no-contact on invalid input.
//! Unix-only: mock socket transport and open-pipe stdin.

/// A mock doorbell socket that records whether the CLI contacted it and
/// answers one report. Unix-only: it drives the real client transport.
struct MockSocket {
    path: std::path::PathBuf,
    contacted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _dir: tempfile::TempDir,
}

impl MockSocket {
    fn bind() -> Self {
        Self::bind_answering(serde_json::json!({"ok": true, "status": "recorded"}))
    }

    fn bind_answering(answer: serde_json::Value) -> Self {
        Self::bind_after(answer, std::time::Duration::ZERO)
    }

    /// Accepts, reads the request, and answers only after `delay`; a server
    /// that never answers uses a delay far longer than the command's deadline.
    fn bind_after(answer: serde_json::Value, delay: std::time::Duration) -> Self {
        use std::os::unix::net::UnixListener;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crowded-report.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let contacted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&contacted);
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader, Write};
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while std::time::Instant::now() < deadline
                && !flag.load(std::sync::atomic::Ordering::SeqCst)
            {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        flag.store(true, std::sync::atomic::Ordering::SeqCst);
                        let mut line = String::new();
                        let _ = BufReader::new(&mut stream).read_line(&mut line);
                        std::thread::sleep(delay);
                        let _ = serde_json::to_writer(&mut stream, &answer);
                        let _ = stream.write_all(b"\n");
                        return;
                    }
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
                }
            }
        });
        Self {
            path,
            contacted,
            _dir: dir,
        }
    }

    fn contacted(&self) -> bool {
        self.contacted.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn session_report_binary() -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_crowded"));
    command.arg("session-report");
    for variable in [
        "CROWDED_TOKEN",
        "CROWDED_ROOM",
        "CROWDED_BIN",
        "CROWDED_SOCKET",
    ] {
        command.env_remove(variable);
    }
    command
}

fn run_report(
    configure: impl FnOnce(&mut std::process::Command),
    stdin: std::process::Stdio,
) -> (bool, Vec<u8>, Vec<u8>) {
    let (code, stdout, stderr) = run_report_code(configure, stdin);
    (code == Some(0), stdout, stderr)
}

fn run_report_code(
    configure: impl FnOnce(&mut std::process::Command),
    stdin: std::process::Stdio,
) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    use std::io::Read;
    let mut command = session_report_binary();
    configure(&mut command);
    command
        .stdin(stdin)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "session-report must never hang"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .unwrap();
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .unwrap();
    (status.code(), stdout, stderr)
}

/// --pending with an open stdin pipe that never closes and never delivers
/// anything must finish quickly: the command never waits on stdin for it.
#[test]
fn session_report_pending_does_not_wait_for_an_open_stdin_pipe() {
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    let socket = MockSocket::bind();
    let start = std::time::Instant::now();
    let (reader, writer) = UnixStream::pair().unwrap();
    // Hold the write end open with nothing written: stdin never EOFs.
    let _held = writer;
    let (success, stdout, _stderr) = run_report(
        |command| {
            command.arg("--pending");
            command.env("CROWDED_TOKEN", "test-token");
            command.env("CROWDED_SOCKET", &socket.path);
        },
        unsafe { Stdio::from_raw_fd(reader.into_raw_fd()) },
    );
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    assert!(success);
    assert!(stdout.is_empty(), "hook stdout must stay silent");
    assert!(socket.contacted(), "the pending report is still delivered");
}

/// A present but invalid stdin id rejects before any contact, even with
/// --pending: the pipe stays open so only a bounded read finds it.
#[test]
fn session_report_with_an_invalid_stdin_id_never_contacts_the_server() {
    use std::io::Write;
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    let socket = MockSocket::bind();
    let (reader, mut writer) = UnixStream::pair().unwrap();
    writer.write_all(br#"{"session_id":"bad/id"}"#).unwrap();
    writer.flush().unwrap();
    // The write end stays open: no EOF follows the payload.
    let (success, stdout, stderr) = run_report(
        |command| {
            command.arg("--pending");
            command.env("CROWDED_TOKEN", "test-token");
            command.env("CROWDED_SOCKET", &socket.path);
        },
        unsafe { Stdio::from_raw_fd(reader.into_raw_fd()) },
    );
    std::mem::forget(writer);
    assert!(success);
    assert!(stdout.is_empty(), "hook stdout must stay silent");
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("malformed"), "{stderr}");
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        !socket.contacted(),
        "an invalid session id must never reach the server"
    );
}

/// A session id key that is present with any non-string value, null
/// included, is rejected before anything is contacted: with and without
/// --pending, for both spellings. Only a missing key counts as absent.
/// The pipe stays open throughout, so only a bounded read can find the
/// payload.
#[test]
fn session_report_rejects_a_present_non_string_id_before_contact() {
    use std::io::Write;
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    let run = |args: &[&str], payload: &str| {
        let socket = MockSocket::bind();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(payload.as_bytes()).unwrap();
        writer.flush().unwrap();
        let (success, stdout, stderr) = run_report(
            |command| {
                command.args(args);
                command.env("CROWDED_TOKEN", "test-token");
                command.env("CROWDED_SOCKET", &socket.path);
            },
            unsafe { Stdio::from_raw_fd(reader.into_raw_fd()) },
        );
        std::mem::forget(writer);
        (success, stdout, stderr, socket)
    };
    let present_invalid = [
        r#"{"session_id":null}"#,
        r#"{"sessionID":null}"#,
        r#"{"session_id":42}"#,
        r#"{"sessionID":true}"#,
        r#"{"session_id":{"nested":1}}"#,
        r#"{"sessionID":["ses-a"]}"#,
        r#"{"session_id":"bad/id"}"#,
    ];
    let modes: [&[&str]; 2] = [&[], &["--pending"]];
    for args in modes {
        for payload in present_invalid {
            let (success, stdout, stderr, socket) = run(args, payload);
            let label = format!("{payload} (args {args:?})");
            assert!(success, "{label}");
            assert!(stdout.is_empty(), "{label}: hook stdout must stay silent");
            assert!(
                String::from_utf8_lossy(&stderr).contains("malformed"),
                "{label}: {stderr:?}"
            );
            assert!(
                !socket.contacted(),
                "{label}: a present invalid id must never reach the server"
            );
        }
        // A usable id does reach the server in the very same modes.
        let (success, stdout, _stderr, socket) = run(args, r#"{"session_id":"ses-good-1"}"#);
        assert!(success);
        assert!(stdout.is_empty(), "hook stdout must stay silent");
        assert!(socket.contacted(), "a valid id must reach the server");
    }
    // Only a missing key is absent, so --pending still reports on its own.
    let (success, _stdout, _stderr, socket) = run(&["--pending"], r#"{"other":1}"#);
    assert!(success);
    assert!(
        socket.contacted(),
        "a missing key leaves --pending to report"
    );
}

/// With neither --session nor --pending, stdin is the whole report: a valid
/// payload written after a delay still reaches the server, and the pipe is
/// never closed, so the completed payload alone ends the wait.
#[test]
fn session_report_waits_for_a_session_payload_that_arrives_late() {
    use std::io::Write;
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    for payload in [
        r#"{"session_id":"ses-late-1"}"#,
        r#"{"sessionID":"ses-late-2"}"#,
    ] {
        let socket = MockSocket::bind();
        let (reader, mut writer) = UnixStream::pair().unwrap();
        let mut command = session_report_binary();
        command.env("CROWDED_TOKEN", "test-token");
        command.env("CROWDED_SOCKET", &socket.path);
        command
            .stdin(unsafe { Stdio::from_raw_fd(reader.into_raw_fd()) })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        writer.write_all(payload.as_bytes()).unwrap();
        writer.flush().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{payload}: session-report must never hang"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        // The write end is still open: only a complete payload ends the wait.
        std::mem::forget(writer);
        assert!(status.success(), "{payload}");
        assert!(
            socket.contacted(),
            "{payload}: a late payload must reach the server"
        );
    }
}

/// An open pipe that never delivers anything costs the command its whole
/// documented stdin bound and nothing more: it exits 0, silent, without
/// contacting the server, so hook liveness is intact.
#[test]
fn session_report_waits_only_its_bound_for_a_pipe_that_never_delivers() {
    use std::os::unix::io::{FromRawFd, IntoRawFd};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;
    let socket = MockSocket::bind();
    let (reader, writer) = UnixStream::pair().unwrap();
    // Hold the write end open with nothing written: stdin never EOFs.
    let _held = writer;
    let start = std::time::Instant::now();
    let (success, stdout, _stderr) = run_report(
        |command| {
            command.env("CROWDED_TOKEN", "test-token");
            command.env("CROWDED_SOCKET", &socket.path);
        },
        unsafe { Stdio::from_raw_fd(reader.into_raw_fd()) },
    );
    let elapsed = start.elapsed();
    assert!(success);
    assert!(stdout.is_empty(), "hook stdout must stay silent");
    assert!(
        elapsed >= std::time::Duration::from_secs(4),
        "a stdin-only report waits its documented bound, waited {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(8),
        "the bound plus a margin must end the command, waited {elapsed:?}"
    );
    assert!(!socket.contacted(), "nothing to report, so nothing is sent");
}

/// No environment at all: no token, no contact, exit 0, silent stdout.
#[test]
fn session_report_without_any_environment_stays_silent() {
    use std::process::Stdio;
    let (success, stdout, _stderr) = run_report(
        |command| {
            command.arg("--session").arg("ses-ok");
        },
        Stdio::null(),
    );
    assert!(success);
    assert!(stdout.is_empty(), "hook stdout must stay silent");
}

/// An invalid --session id is rejected without contacting the server,
/// even when --pending is also set.
#[test]
fn session_report_with_an_invalid_argument_never_contacts_the_server() {
    use std::process::Stdio;
    let socket = MockSocket::bind();
    let (success, stdout, stderr) = run_report(
        |command| {
            command.args(["--session", "bad/id", "--pending"]);
            command.env("CROWDED_TOKEN", "test-token");
            command.env("CROWDED_SOCKET", &socket.path);
        },
        Stdio::null(),
    );
    assert!(success);
    assert!(stdout.is_empty(), "hook stdout must stay silent");
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(stderr.contains("malformed"), "{stderr}");
    std::thread::sleep(std::time::Duration::from_secs(1));
    assert!(
        !socket.contacted(),
        "an invalid session id must never reach the server"
    );
}

/// Runs `session-report` with the given arguments and environment pieces and
/// returns its exit status, stdout and stderr. The socket path is whatever the
/// test gives; an empty stdin never delivers anything.
fn run_with(
    args: &[&str],
    token: bool,
    socket: Option<&std::path::Path>,
) -> (Option<i32>, Vec<u8>, Vec<u8>) {
    run_report_code(
        |command| {
            command.args(args);
            if token {
                command.env("CROWDED_TOKEN", "test-token");
            }
            if let Some(path) = socket {
                command.env("CROWDED_SOCKET", path);
            }
        },
        std::process::Stdio::null(),
    )
}

fn assert_quiet_failure(stdout: &[u8], stderr: &[u8], label: &str) {
    assert!(stdout.is_empty(), "{label}: stdout must stay silent");
    let stderr = String::from_utf8_lossy(stderr);
    assert!(
        stderr.lines().count() <= 1,
        "{label}: at most one stderr line: {stderr}"
    );
}

/// `--ack` exits 0 only when Crowded accepted the report.
#[test]
fn session_report_ack_exits_zero_only_when_the_server_accepted() {
    let socket = MockSocket::bind();
    let (code, stdout, stderr) = run_with(&["--pending", "--ack"], true, Some(&socket.path));
    assert_eq!(code, Some(0));
    assert!(stdout.is_empty());
    assert!(
        stderr.is_empty(),
        "an accepted report prints nothing: {stderr:?}"
    );
    assert!(socket.contacted());
}

#[test]
fn session_report_ack_exits_4_without_a_token() {
    let socket = MockSocket::bind();
    let (code, stdout, stderr) = run_with(&["--pending", "--ack"], false, Some(&socket.path));
    assert_eq!(code, Some(4));
    assert_quiet_failure(&stdout, &stderr, "no token");
    assert!(String::from_utf8_lossy(&stderr).contains("CROWDED_TOKEN"));
    assert!(!socket.contacted());
}

#[test]
fn session_report_ack_exits_5_when_the_server_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nobody-listens.sock");
    let (code, stdout, stderr) = run_with(&["--session", "ses-a", "--ack"], true, Some(&missing));
    assert_eq!(code, Some(5));
    assert_quiet_failure(&stdout, &stderr, "unreachable");
}

#[test]
fn session_report_ack_exits_6_when_the_report_is_rejected() {
    let socket = MockSocket::bind_answering(
        serde_json::json!({"ok": false, "status": "rejected", "error": "unknown token"}),
    );
    let (code, stdout, stderr) =
        run_with(&["--session", "ses-a", "--ack"], true, Some(&socket.path));
    assert_eq!(code, Some(6));
    assert_quiet_failure(&stdout, &stderr, "rejected");
    assert!(String::from_utf8_lossy(&stderr).contains("unknown token"));
    assert!(socket.contacted());
}

#[test]
fn session_report_ack_exits_3_for_an_invalid_id_without_contacting_the_server() {
    let socket = MockSocket::bind();
    let (code, stdout, stderr) =
        run_with(&["--session", "bad/id", "--ack"], true, Some(&socket.path));
    assert_eq!(code, Some(3));
    assert_quiet_failure(&stdout, &stderr, "invalid id");
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !socket.contacted(),
        "an invalid id never reaches the server"
    );
}

#[test]
fn session_report_ack_exits_2_on_a_usage_error() {
    let (code, stdout, stderr) = run_with(&["--ack", "--bogus"], true, None);
    assert_eq!(code, Some(2));
    assert_quiet_failure(&stdout, &stderr, "usage");
    let (code, _, _) = run_with(&["--ack", "--ack", "--pending"], true, None);
    assert_eq!(code, Some(2), "--ack may appear only once");
    let (code, stdout, stderr) = run_with(&["--ack"], true, None);
    assert_eq!(code, Some(2), "nothing to report");
    assert_quiet_failure(&stdout, &stderr, "nothing to report");
}

/// Without `--ack` every failure above still exits 0, silently, exactly as the
/// hook contract requires.
#[test]
fn session_report_without_ack_exits_zero_in_every_failure() {
    let rejecting = MockSocket::bind_answering(
        serde_json::json!({"ok": false, "status": "rejected", "error": "unknown token"}),
    );
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nobody-listens.sock");
    let cases: Vec<(&str, Vec<&str>, bool, Option<&std::path::Path>)> = vec![
        ("no token", vec!["--pending"], false, None),
        (
            "unreachable",
            vec!["--session", "ses-a"],
            true,
            Some(missing.as_path()),
        ),
        (
            "rejected",
            vec!["--session", "ses-a"],
            true,
            Some(rejecting.path.as_path()),
        ),
        ("invalid id", vec!["--session", "bad/id"], true, None),
        ("usage", vec!["--bogus"], true, None),
        ("nothing to report", vec![], true, None),
    ];
    for (label, args, token, socket) in cases {
        let (code, stdout, stderr) = run_with(&args, token, socket);
        assert_eq!(code, Some(0), "{label}");
        assert_quiet_failure(&stdout, &stderr, label);
    }
}

/// The deadline the command documents (`SESSION_REPORT_DEADLINE`, 4 seconds).
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(4);
const MARGIN: std::time::Duration = std::time::Duration::from_secs(4);

/// A server that accepts the request and never answers: the command gives up at
/// its deadline instead of hanging, and `--ack` reports it as unreachable.
#[test]
fn session_report_ack_exits_5_within_the_deadline_when_the_server_never_answers() {
    let socket = MockSocket::bind_after(
        serde_json::json!({"ok": true, "status": "recorded"}),
        std::time::Duration::from_secs(60),
    );
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_with(&["--pending", "--ack"], true, Some(&socket.path));
    let elapsed = started.elapsed();
    assert_eq!(
        code,
        Some(5),
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(elapsed < DEADLINE + MARGIN, "waited {elapsed:?}");
    assert_quiet_failure(&stdout, &stderr, "silent server");
    assert!(socket.contacted());
}

#[test]
fn session_report_without_ack_exits_zero_within_the_deadline_when_the_server_never_answers() {
    let socket = MockSocket::bind_after(
        serde_json::json!({"ok": true, "status": "recorded"}),
        std::time::Duration::from_secs(60),
    );
    let started = std::time::Instant::now();
    let (code, stdout, stderr) = run_with(&["--pending"], true, Some(&socket.path));
    let elapsed = started.elapsed();
    assert_eq!(code, Some(0));
    assert!(elapsed < DEADLINE + MARGIN, "waited {elapsed:?}");
    assert_quiet_failure(&stdout, &stderr, "silent server, default");
}

/// A reply that arrives before the deadline still counts as accepted.
#[test]
fn session_report_ack_accepts_a_reply_that_arrives_just_before_the_deadline() {
    let socket = MockSocket::bind_after(
        serde_json::json!({"ok": true, "status": "recorded"}),
        std::time::Duration::from_millis(2500),
    );
    let (code, stdout, stderr) = run_with(&["--pending", "--ack"], true, Some(&socket.path));
    assert_eq!(
        code,
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(stdout.is_empty());
}
