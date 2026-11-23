//! Codex native delivery: `codex queue` as an argument array, and the outbox
//! adapter that judges its acceptance. Nothing here writes to a terminal.
#![allow(dead_code)]

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use sha2::{Digest, Sha256};

use crate::{
    command::ResolvedCommand,
    native_outbox::{Adapter, AdapterResult, Binding, Message},
};

/// A queue command that has not finished by now is killed. It may already have
/// submitted, so the outcome is uncertain rather than a failure. Tests use a
/// short bound so a deliberately hung stub does not slow the suite.
#[cfg(not(test))]
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
#[cfg(test)]
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Only the start of the CLI's output is kept; the rest is drained so a chatty
/// process can never block on a full pipe.
const MAX_OUTPUT: usize = 8 * 1024;
/// A helper the CLI started can hold its pipes open after the CLI exits.
#[cfg(not(test))]
const DRAIN_WAIT: Duration = Duration::from_secs(2);
// Long enough that a test can tell a read that stopped at the first session
// line from one that waited out the drain, by order rather than by elapsed
// time: it stays above the ten-second patience of the test that makes that
// distinction, so a read waiting out the drain cannot return inside it.
#[cfg(test)]
const DRAIN_WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(20);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Accepted,
    Rejected(String),
    NotSent,
    Uncertain,
}

/// One room's exact queue target: the CLI to run, its directory and
/// environment, the captured thread it owns, and whether that room's process is
/// still alive. The flag is shared with the owner of the process, so a send can
/// be refused after the room exits.
#[derive(Clone)]
pub(crate) struct Room {
    pub(crate) program: OsString,
    pub(crate) cwd: PathBuf,
    pub(crate) thread: String,
    pub(crate) variables: Vec<(OsString, OsString)>,
    pub(crate) live: Arc<AtomicBool>,
}

/// Only an exact captured thread id is ever passed. Anything else could name
/// another room's session, so it is refused before a process is started.
pub(crate) fn valid_thread(thread: &str) -> bool {
    thread.len() <= 128
        && thread
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
        && thread
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// A session id minted by `codex exec` is a canonical UUID; anything else is
/// not trusted as a binding.
pub(crate) fn valid_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8usize, 4, 4, 4, 12])
            .all(|(group, length)| {
                group.len() == length && group.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

/// Options the interactive CLI accepts but `codex exec` does not. They stay on
/// the resumed terminal and are left out of the exec command, so a room's
/// interactive settings never make its mint fail.
fn exec_unsupported(argument: &str) -> bool {
    matches!(
        argument,
        "-a" | "--ask-for-approval"
            | "--search"
            | "--no-alt-screen"
            | "--no-daemon"
            | "--last"
            | "--all"
            | "--include-non-interactive"
            | "--remote"
            | "--remote-auth-token-env"
    ) || argument.starts_with("--ask-for-approval=")
        || argument.starts_with("--search=")
        || argument.starts_with("--no-alt-screen=")
        || argument.starts_with("--remote=")
        || argument.starts_with("--remote-auth-token-env=")
}

/// Whether a dropped option takes the next argument as its value.
fn exec_unsupported_takes_value(argument: &str) -> bool {
    matches!(
        argument,
        "-a" | "--ask-for-approval" | "--remote" | "--remote-auth-token-env"
    )
}

/// The room's launch options that `codex exec` accepts, in order. Unknown
/// options are kept: a flag exec later gains must not be silently lost, and an
/// option exec rejects fails the mint with the CLI's own reason.
pub(crate) fn exec_room_arguments(room_args: &[OsString]) -> Vec<OsString> {
    let mut kept = Vec::with_capacity(room_args.len());
    let mut index = 0;
    while index < room_args.len() {
        let argument = room_args[index].to_string_lossy();
        if exec_unsupported(&argument) {
            index += 1;
            if exec_unsupported_takes_value(&argument) && index < room_args.len() {
                index += 1;
            }
            continue;
        }
        kept.push(room_args[index].clone());
        index += 1;
    }
    kept
}

/// The command that creates a room's session: the room's exec-compatible
/// launch options followed by the exec prompt. `--json` makes the new thread
/// id machine readable before the terminal starts.
pub(crate) fn exec_arguments(room_args: &[OsString], prompt: &str) -> Vec<OsString> {
    let room_args = exec_room_arguments(room_args);
    let mut arguments = Vec::with_capacity(room_args.len() + 4);
    arguments.push("exec".into());
    arguments.extend(room_args);
    arguments.push("--json".into());
    arguments.push("--skip-git-repo-check".into());
    arguments.push(prompt.into());
    arguments
}

/// The session id from the CLI's JSONL events.
fn thread_started(output: &[u8]) -> Option<String> {
    String::from_utf8_lossy(output).lines().find_map(|line| {
        let event: serde_json::Value = serde_json::from_str(line).ok()?;
        if event["type"].as_str() != Some("thread.started") {
            return None;
        }
        let id = event["thread_id"].as_str()?;
        valid_uuid(id).then(|| id.to_owned())
    })
}

/// A short actionable reason for a failed exec, avoiding the CLI's unrelated
/// warning noise: its own `Error:` line when there is one, else the last line.
fn exec_reason(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let line = lines
        .iter()
        .rev()
        .find(|line| line.starts_with("Error:"))
        .or_else(|| lines.last())
        .copied()
        .unwrap_or("`codex exec` failed");
    let line = line.strip_prefix("Error: ").unwrap_or(line);
    let mut short: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    if line.chars().count() > 200 {
        short.push('…');
    }
    short
}

fn arguments(thread: &str, text: &str) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = vec!["queue".into(), "--thread".into(), thread.into()];
    if text.starts_with('-') {
        // The parser would read a leading dash as another option; the equals
        // form takes the text literally.
        arguments.push(format!("--message={text}").into());
    } else {
        arguments.push("--message".into());
        arguments.push(text.into());
    }
    arguments
}

fn reason(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("the queue command failed");
    let line = line.strip_prefix("Error: ").unwrap_or(line);
    let mut short: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    if line.chars().count() > 200 {
        short.push('…');
    }
    short
}

/// Whether the CLI printed its exact acceptance line for this thread.
/// The line is `Queued message {id} for thread {thread}.`; a prefix, a
/// substring, or another thread's id is not acceptance.
fn accepted(output: &[u8], thread: &str) -> bool {
    String::from_utf8_lossy(output).lines().any(|line| {
        let Some(rest) = line.trim().strip_prefix("Queued message ") else {
            return false;
        };
        let Some((id, tail)) = rest.split_once(" for thread ") else {
            return false;
        };
        !id.is_empty() && tail == format!("{thread}.")
    })
}

fn missing_queue(output: &[u8]) -> bool {
    let text = String::from_utf8_lossy(output).to_ascii_lowercase();
    let unknown = text.contains("unrecognized subcommand") || text.contains("unknown subcommand");
    let unsupported = text.contains("unexpected argument")
        && (text.contains("--thread") || text.contains("--message"));
    (unknown && text.contains("queue")) || unsupported
}

/// Reads the end of the stream, up to [`MAX_OUTPUT`] bytes. The acceptance line
/// is the last thing a queue command prints, so the tail is what matters; all
/// output is still drained so the child never blocks on a full pipe.
fn drain(mut reader: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut kept: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    kept.extend_from_slice(&buffer[..count]);
                    if kept.len() > MAX_OUTPUT {
                        let excess = kept.len() - MAX_OUTPUT;
                        kept.drain(..excess);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = sender.send(kept);
    });
    receiver
}

/// Reads the start of the stream and keeps [`MAX_OUTPUT`] bytes, draining the
/// rest so the child never blocks. The exec session id is the first event.
/// The first output of a child, readable while the pipe is still open. A
/// process the CLI left behind (or one that inherited the pipe through a
/// concurrent spawn) can keep the pipe open long after the CLI exited, so the
/// session line must be found without waiting for end of file.
struct Head {
    kept: Arc<std::sync::Mutex<Vec<u8>>>,
    finished: Arc<AtomicBool>,
}

impl Head {
    /// Returns the output as soon as `enough` accepts it, the pipe closes, or
    /// `DRAIN_WAIT` passes.
    fn wait_until(&self, enough: impl Fn(&[u8]) -> bool) -> Vec<u8> {
        let deadline = Instant::now() + DRAIN_WAIT;
        loop {
            // Read the flag first: the reader sets it after its last append.
            let finished = self.finished.load(Ordering::Acquire);
            let out = self
                .kept
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            if finished || enough(&out) || Instant::now() >= deadline {
                return out;
            }
            thread::sleep(POLL);
        }
    }
}

fn head(mut reader: impl Read + Send + 'static) -> Head {
    let kept = Arc::new(std::sync::Mutex::new(Vec::new()));
    let finished = Arc::new(AtomicBool::new(false));
    let (shared, flag) = (Arc::clone(&kept), Arc::clone(&finished));
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let mut kept = shared.lock().unwrap_or_else(|poison| poison.into_inner());
                    if kept.len() < MAX_OUTPUT {
                        let room = MAX_OUTPUT - kept.len();
                        kept.extend_from_slice(&buffer[..count.min(room)]);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        flag.store(true, Ordering::Release);
    });
    Head { kept, finished }
}

/// The command for one CLI invocation: the room's own PATH when it has one,
/// its directory, and its environment. Both the terminal launch and the exec
/// phase resolve and configure the process here.
fn command_for(
    program: &OsStr,
    arguments: &[OsString],
    cwd: &Path,
    variables: &[(OsString, OsString)],
) -> io::Result<std::process::Command> {
    let variable = |name: &str| {
        variables
            .iter()
            .find(|(key, _)| key == OsStr::new(name))
            .map(|(_, value)| value.clone())
    };
    let resolved = match (variable("PATH"), variable("PATHEXT")) {
        (None, None) => ResolvedCommand::resolve(program, arguments, cwd),
        (path, path_ext) => {
            ResolvedCommand::resolve_with_environment(program, arguments, cwd, path, path_ext)
        }
    };
    let mut process = resolved?.standard();
    process
        .current_dir(cwd)
        .env("PWD", cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in variables {
        process.env(key, value);
    }
    Ok(process)
}

fn wait(child: &mut Child, deadline: Instant) -> io::Result<Option<ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

/// Runs one queue command and classifies the outcome from its own evidence.
pub(crate) fn deliver(room: &Room, text: &str, timeout: Duration) -> Delivery {
    if !valid_thread(&room.thread) {
        return Delivery::NotSent;
    }
    let arguments = arguments(&room.thread, text);
    let mut process = match command_for(&room.program, &arguments, &room.cwd, &room.variables) {
        Ok(process) => process,
        Err(_) => return Delivery::NotSent,
    };
    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(_) => return Delivery::NotSent,
    };
    let (stdout, stderr) = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => (drain(stdout), drain(stderr)),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Delivery::NotSent;
        }
    };
    let deadline = Instant::now() + timeout;
    let status = match wait(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            // The command may have submitted before it was stopped.
            return Delivery::Uncertain;
        }
    };
    let out = stdout.recv_timeout(DRAIN_WAIT).unwrap_or_default();
    let err = stderr.recv_timeout(DRAIN_WAIT).unwrap_or_default();
    // The CLI's own line is the evidence that it queued something; the exit
    // status decides whether that is a clean acceptance. A matching line with
    // a failed exit may still have submitted, so it is not a rejection either.
    let printed = accepted(&out, &room.thread) || accepted(&err, &room.thread);
    if printed {
        return if status.success() {
            Delivery::Accepted
        } else {
            Delivery::Uncertain
        };
    }
    if missing_queue(&out) || missing_queue(&err) {
        return Delivery::Rejected(
            "the installed codex has no usable `queue` command; update codex".into(),
        );
    }
    if status.success() {
        // A success without the acceptance line is not proof that anything was
        // submitted, so it stays uncertain and is never resent.
        return Delivery::Uncertain;
    }
    let detail = if err.iter().any(|byte| !byte.is_ascii_whitespace()) {
        reason(&err)
    } else {
        reason(&out)
    };
    Delivery::Rejected(detail)
}

/// Creates a room's Codex session with `codex exec` and returns the UUID the
/// CLI reported. The exec phase is wrapped and configured exactly like the
/// terminal phase, so the room's Headroom and environment apply to both.
pub(crate) fn start_session(
    program: &OsStr,
    arguments: &[OsString],
    cwd: &Path,
    variables: &[(OsString, OsString)],
    timeout: Duration,
) -> Result<String, String> {
    let mut process = command_for(program, arguments, cwd, variables)
        .map_err(|error| format!("could not start `codex exec`: {error}"))?;
    let mut child = process
        .spawn()
        .map_err(|error| format!("could not start `codex exec`: {error}"))?;
    let (stdout, stderr) = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => (head(stdout), drain(stderr)),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("could not read `codex exec` output".to_owned());
        }
    };
    let deadline = Instant::now() + timeout;
    let status = match wait(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "`codex exec` did not finish within {} seconds",
                timeout.as_secs()
            ));
        }
    };
    let out = stdout.wait_until(|out| thread_started(out).is_some());
    if !status.success() {
        let err = stderr.recv_timeout(DRAIN_WAIT).unwrap_or_default();
        let detail = if err.iter().any(|byte| !byte.is_ascii_whitespace()) {
            exec_reason(&err)
        } else {
            exec_reason(&out)
        };
        return Err(detail);
    }
    thread_started(&out).ok_or_else(|| "`codex exec` did not report a session id".to_owned())
}

pub(crate) struct CodexAdapter {
    rooms: HashMap<String, (Binding, Room)>,
    wait: Duration,
}

impl CodexAdapter {
    pub(crate) fn new(rooms: Vec<(Binding, Room)>) -> io::Result<Self> {
        let mut by_room = HashMap::new();
        for (binding, room) in rooms {
            if by_room
                .insert(binding.room.clone(), (binding, room))
                .is_some()
            {
                return Err(io::Error::other("duplicate native room"));
            }
        }
        Ok(Self {
            rooms: by_room,
            wait: COMMAND_TIMEOUT,
        })
    }

    /// Whether this adapter owns the room. An unknown room is held by the
    /// caller instead of being sent to another one.
    pub(crate) fn has_room(&self, room: &str) -> bool {
        self.rooms.contains_key(room)
    }

    fn send(&self, message: Message) -> AdapterResult {
        if format!("{:x}", Sha256::digest(message.text.as_bytes())) != message.hash {
            return AdapterResult::Failed("message content hash mismatch".into());
        }
        let Some((binding, room)) = self.rooms.get(&message.destination) else {
            return AdapterResult::Dropped("Codex room is not bound".into());
        };
        if binding != &message.binding {
            return AdapterResult::Dropped(
                "Codex room binding changed; inspect before retry".into(),
            );
        }
        if !room.live.load(Ordering::SeqCst) {
            return AdapterResult::Dropped(
                "the Codex room process is no longer running; mail was not sent".into(),
            );
        }
        match deliver(room, &message.text, self.wait) {
            Delivery::Accepted => AdapterResult::Accepted(None),
            Delivery::Rejected(reason) => AdapterResult::Failed(reason),
            Delivery::NotSent => AdapterResult::Dropped(
                "the codex queue command was not started; inspect the room before retry".into(),
            ),
            Delivery::Uncertain => AdapterResult::Uncertain,
        }
    }
}

impl Adapter for CodexAdapter {
    fn send(&self, message: Message) -> AdapterResult {
        Self::send(self, message)
    }
}

/// A thread id captured for a room is only the room's own when the capture was
/// anchored to that room's spawn and keyed by its title; this builds the
/// binding from that evidence and the room's number.
pub(crate) fn binding(room: &str, thread: &str, generation: u64) -> Binding {
    Binding {
        room: room.to_owned(),
        session: thread.to_owned(),
        endpoint: "codex".to_owned(),
        generation,
    }
}

/// The queue target for a room whose process is running.
pub(crate) fn room(
    program: OsString,
    cwd: &Path,
    thread: String,
    variables: Vec<(OsString, OsString)>,
) -> Room {
    Room {
        program,
        cwd: cwd.to_path_buf(),
        thread,
        variables,
        live: Arc::new(AtomicBool::new(true)),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, sync::atomic::Ordering};

    fn stub(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        let tmp = root.join(format!(".{name}.tmp"));
        fs::write(&tmp, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(&tmp, &path).unwrap();
        path
    }

    fn room_for(program: &Path, dir: &Path) -> Room {
        room(
            program.as_os_str().to_owned(),
            dir,
            "11111111-2222-3333-4444-555555555555".into(),
            Vec::new(),
        )
    }

    const THREAD: &str = "11111111-2222-3333-4444-555555555555";

    /// Writes each argument on its own line, so the test can prove the text
    /// arrived as one exact argument.
    #[test]
    fn a_session_is_minted_from_the_json_event_stream() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(
            dir,
            "codex",
            &format!(
                "printf '%s\\n' \"$@\" > '{}/args'\nprintf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' '{THREAD}'\nprintf '{{\"type\":\"turn.completed\"}}\\n'",
                dir.display()
            ),
        );
        let arguments = exec_arguments(
            &[OsString::from("--model"), OsString::from("gpt-5")],
            "House rules",
        );
        assert_eq!(
            arguments,
            vec![
                OsString::from("exec"),
                OsString::from("--model"),
                OsString::from("gpt-5"),
                OsString::from("--json"),
                OsString::from("--skip-git-repo-check"),
                OsString::from("House rules"),
            ]
        );
        let uuid = start_session(
            program.as_os_str(),
            &arguments,
            dir,
            &[],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(uuid, THREAD);
        assert_eq!(
            fs::read_to_string(dir.join("args")).unwrap(),
            "exec\n--model\ngpt-5\n--json\n--skip-git-repo-check\nHouse rules\n"
        );
    }

    #[test]
    fn a_session_line_is_found_while_a_leftover_process_holds_the_pipe_open() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().to_path_buf();
        let gate = dir.join("release");
        // The CLI exits after reporting, but a process it started keeps the
        // output pipe open until the test opens the gate (or thirty seconds
        // pass). The drain wait is far longer than the test's own patience, so
        // the call can only return before the gate if it stopped reading at the
        // session line.
        let program = stub(
            &dir,
            "leaky",
            &format!(
                "printf '{{\"type\":\"thread.started\",\"thread_id\":\"{THREAD}\"}}\\n'\n(n=0; while [ ! -e '{gate}' ] && [ $n -lt 600 ]; do sleep 0.05; n=$((n+1)); done) &\nexit 0",
                gate = gate.display()
            ),
        );
        let returned = Arc::new(AtomicBool::new(false));
        let worker = {
            let (returned, dir) = (Arc::clone(&returned), dir.clone());
            thread::spawn(move || {
                let result = start_session(
                    program.as_os_str(),
                    &exec_arguments(&[], "prompt"),
                    &dir,
                    &[],
                    Duration::from_secs(20),
                );
                returned.store(true, Ordering::SeqCst);
                result
            })
        };
        let patience = Instant::now() + Duration::from_secs(10);
        while !returned.load(Ordering::SeqCst) && Instant::now() < patience {
            thread::sleep(Duration::from_millis(20));
        }
        let returned_before_the_gate = returned.load(Ordering::SeqCst);
        fs::write(&gate, b"").unwrap();
        let result = worker.join().unwrap();
        assert!(
            returned_before_the_gate,
            "the mint waited for the pipe to close instead of stopping at the session line"
        );
        assert_eq!(result.unwrap(), THREAD);
    }

    #[test]
    fn a_failed_exec_reports_its_own_error_and_never_a_session() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        // Generous: every stub exits at once, so this only has to outlast a
        // loaded runner. The bound itself has its own test below.
        let bound = Duration::from_secs(30);
        for (name, body, expected) in [
            (
                "failed",
                "printf 'warning: noise\\nError: model unavailable\\n' >&2\nexit 1",
                "model unavailable",
            ),
            (
                "eventless",
                "printf '{\"type\":\"turn.completed\"}\\n'\nexit 0",
                "did not report a session id",
            ),
            (
                "invalid",
                "printf '{\"type\":\"thread.started\",\"thread_id\":\"not-a-uuid\"}\\n'\nexit 0",
                "did not report a session id",
            ),
        ] {
            let program = stub(dir, name, body);
            let error = start_session(
                program.as_os_str(),
                &exec_arguments(&[], "prompt"),
                dir,
                &[],
                bound,
            )
            .unwrap_err();
            assert!(error.contains(expected), "{name}: {error}");
        }
        // A line that was printed before a failed exit is not a session either.
        let program = stub(
            dir,
            "line-then-fail",
            &format!(
                "printf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' '{THREAD}'\nprintf 'Error: turn failed\\n' >&2\nexit 1"
            ),
        );
        let error = start_session(
            program.as_os_str(),
            &exec_arguments(&[], "prompt"),
            dir,
            &[],
            bound,
        )
        .unwrap_err();
        assert!(error.contains("turn failed"), "{error}");
    }

    #[test]
    fn an_exec_that_does_not_finish_is_stopped_at_its_bound() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(dir, "slow", "exec sleep 30");
        let started = Instant::now();
        let error = start_session(
            program.as_os_str(),
            &exec_arguments(&[], "prompt"),
            dir,
            &[],
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.contains("did not finish within"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn exec_options_are_split_between_the_mint_and_the_resumed_terminal() {
        let room: Vec<OsString> = [
            "--search",
            "--ask-for-approval",
            "never",
            "-a",
            "on-request",
            "--no-alt-screen",
            "--no-daemon",
            "--remote=ws://127.0.0.1:1",
            "-c",
            "model_reasoning_effort=\"high\"",
            "-m",
            "gpt-5",
            "-s",
            "workspace-write",
            "--unknown-flag",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(
            exec_arguments(&room, "prompt"),
            vec![
                OsString::from("exec"),
                OsString::from("-c"),
                OsString::from("model_reasoning_effort=\"high\""),
                OsString::from("-m"),
                OsString::from("gpt-5"),
                OsString::from("-s"),
                OsString::from("workspace-write"),
                // An option this build does not classify is kept rather than
                // silently dropped.
                OsString::from("--unknown-flag"),
                OsString::from("--json"),
                OsString::from("--skip-git-repo-check"),
                OsString::from("prompt"),
            ]
        );
        // The full set stays available to the terminal phase.
        assert_eq!(room.len(), 15);
    }

    #[test]
    fn only_a_canonical_uuid_is_a_session() {
        assert!(valid_uuid(THREAD));
        for text in [
            "",
            "not-a-uuid",
            "11111111-2222-3333-4444",
            "11111111-2222-3333-4444-5555555555555",
            "1111111g-2222-3333-4444-555555555555",
            " 11111111-2222-3333-4444-555555555555",
        ] {
            assert!(!valid_uuid(text), "{text}");
        }
    }

    #[test]
    fn text_is_passed_as_one_exact_argument_and_acceptance_is_read_from_output() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(
            dir,
            "codex",
            &format!(
                "printf '%s\\n' \"$@\" > '{}/args'\nprintf 'Queued message sub_1 for thread %s.\\n' '{THREAD}'\n",
                dir.display()
            ),
        );
        let text = "multi\nline \u{2713} \u{fc}\u{f1}\u{ed} — ok";
        assert_eq!(
            deliver(&room_for(&program, dir), text, Duration::from_secs(5)),
            Delivery::Accepted
        );
        let args = fs::read_to_string(dir.join("args")).unwrap();
        assert_eq!(
            args,
            format!("queue\n--thread\n{THREAD}\n--message\n{text}\n")
        );
    }

    #[test]
    fn a_message_starting_with_a_dash_is_still_one_exact_argument() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(
            dir,
            "codex",
            &format!(
                "printf '%s\\n' \"$@\" > '{}/args'\nprintf 'Queued message sub_1 for thread %s.\\n' '{THREAD}'",
                dir.display()
            ),
        );
        let text = "-dash \u{2713}";
        assert_eq!(
            deliver(&room_for(&program, dir), text, Duration::from_secs(5)),
            Delivery::Accepted
        );
        assert_eq!(
            fs::read_to_string(dir.join("args")).unwrap(),
            format!("queue\n--thread\n{THREAD}\n--message={text}\n")
        );
    }

    #[test]
    fn a_definitive_rejection_names_its_reason() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(
            dir,
            "codex",
            "printf 'Error: failed to queue session message: no rollout found\\n' >&2\nexit 1",
        );
        assert_eq!(
            deliver(&room_for(&program, dir), "hello", Duration::from_secs(5)),
            Delivery::Rejected("failed to queue session message: no rollout found".into())
        );
    }

    #[test]
    fn a_command_that_hangs_after_submitting_is_uncertain_and_killed() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(
            dir,
            "codex",
            &format!("printf 'Queued message sub_1 for thread %s.\\n' '{THREAD}'\nsleep 30"),
        );
        let started = Instant::now();
        assert_eq!(
            deliver(
                &room_for(&program, dir),
                "hello",
                Duration::from_millis(300)
            ),
            Delivery::Uncertain
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_success_without_acceptance_evidence_is_uncertain() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(dir, "codex", "exit 0");
        assert_eq!(
            deliver(&room_for(&program, dir), "hello", Duration::from_secs(5)),
            Delivery::Uncertain
        );
        // Accepted evidence for another thread proves nothing about this one.
        let program = stub(
            dir,
            "other",
            "printf 'Queued message sub_1 for thread %s.\\n' '99999999-0000-0000-0000-000000000000'",
        );
        assert_eq!(
            deliver(&room_for(&program, dir), "hello", Duration::from_secs(5)),
            Delivery::Uncertain
        );
    }

    #[test]
    fn acceptance_needs_the_exact_line_not_a_prefix_or_a_substring() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        for line in [
            // No message id.
            format!("Queued message for thread {THREAD}."),
            // Something follows the thread and the full stop.
            format!("Queued message sub_1 for thread {THREAD}. trailing"),
            // The thread appears, but not as the line's own target.
            format!(
                "Queued message sub_1 for thread 99999999-0000-0000-0000-000000000000. ({THREAD})"
            ),
            // A prefix of the line only.
            "Queued message".to_owned(),
        ] {
            let program = stub(dir, "codex", &format!("printf '%s\\n' \"{line}\""));
            assert_eq!(
                deliver(&room_for(&program, dir), "hello", Duration::from_secs(5)),
                Delivery::Uncertain,
                "{line}"
            );
        }
    }

    #[test]
    fn a_matching_line_on_a_failed_exit_is_uncertain_and_on_stderr_is_accepted() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        // The CLI printed acceptance but exited nonzero: it may have submitted.
        let program = stub(
            dir,
            "failed-exit",
            &format!("printf 'Queued message sub_1 for thread %s.\\n' '{THREAD}'\nexit 1"),
        );
        assert_eq!(
            deliver(&room_for(&program, dir), "hello", Duration::from_secs(5)),
            Delivery::Uncertain
        );
        // The same line on stderr with a clean exit is still acceptance.
        let program = stub(
            dir,
            "stderr-line",
            &format!("printf 'Queued message sub_1 for thread %s.\\n' '{THREAD}' >&2\nexit 0"),
        );
        assert_eq!(
            deliver(&room_for(&program, dir), "hello", Duration::from_secs(5)),
            Delivery::Accepted
        );
    }

    #[test]
    fn a_missing_queue_subcommand_is_visible_and_actionable() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        for output in [
            "error: unrecognized subcommand 'queue'",
            "error: unexpected argument '--thread' found",
        ] {
            let program = stub(dir, "codex", &format!("printf '{output}\\n' >&2\nexit 2"));
            assert_eq!(
                deliver(&room_for(&program, dir), "hello", Duration::from_secs(5)),
                Delivery::Rejected(
                    "the installed codex has no usable `queue` command; update codex".into()
                )
            );
        }
    }

    #[test]
    fn output_is_bounded_and_a_chatty_command_still_finishes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(
            dir,
            "codex",
            &format!(
                "yes 'filler line' | head -n 20000\nprintf 'Queued message sub_1 for thread %s.\\n' '{THREAD}'"
            ),
        );
        let started = Instant::now();
        assert_eq!(
            deliver(&room_for(&program, dir), "hello", Duration::from_secs(10)),
            Delivery::Accepted
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn an_invalid_thread_or_missing_program_never_starts_a_command() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let marker = dir.join("ran");
        let program = stub(dir, "codex", &format!("touch '{}'", marker.display()));
        for thread in ["", "-rf", "a b", "a/b", "x".repeat(129).as_str()] {
            let mut room = room_for(&program, dir);
            room.thread = thread.into();
            assert_eq!(
                deliver(&room, "hello", Duration::from_secs(5)),
                Delivery::NotSent,
                "{thread}"
            );
        }
        let mut room = room_for(&program, dir);
        room.program = dir.join("missing").into_os_string();
        assert_eq!(
            deliver(&room, "hello", Duration::from_secs(5)),
            Delivery::NotSent
        );
        assert!(!marker.exists());
    }

    fn message(adapter_room: &Binding, text: &str) -> Message {
        Message {
            id: "a".repeat(64),
            source: "source".into(),
            destination: adapter_room.room.clone(),
            binding: adapter_room.clone(),
            text: text.into(),
            hash: format!("{:x}", Sha256::digest(text.as_bytes())),
            created_ms: 0,
            sequence: 1,
            attempts: Vec::new(),
            receipt: None,
            status: crate::native_outbox::Status::Sending,
        }
    }

    #[test]
    fn the_adapter_gates_on_binding_and_content_and_runs_once_per_send() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let program = stub(
            dir,
            "codex",
            &format!(
                "n=$(cat '{}/count' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{}/count'\nprintf 'Queued message sub_%s for thread %s.\\n' \"$n\" '{THREAD}'",
                dir.display(),
                dir.display()
            ),
        );
        let adapter_room = binding("3", THREAD, 1);
        let codex_room = room_for(&program, dir);
        let live = Arc::clone(&codex_room.live);
        let adapter = CodexAdapter::new(vec![(adapter_room.clone(), codex_room)]).unwrap();
        assert!(adapter.has_room("3") && !adapter.has_room("4"));

        let mut stale = adapter_room.clone();
        stale.session = "99999999-0000-0000-0000-000000000000".into();
        assert!(matches!(
            adapter.send(message(&stale, "hello")),
            AdapterResult::Dropped(_)
        ));
        // The same session under another launch generation is not this binding.
        let mut other_generation = adapter_room.clone();
        other_generation.generation += 1;
        assert!(matches!(
            adapter.send(message(&other_generation, "hello")),
            AdapterResult::Dropped(_)
        ));
        let mut wrong_hash = message(&adapter_room, "hello");
        wrong_hash.hash = "wrong".into();
        assert!(matches!(adapter.send(wrong_hash), AdapterResult::Failed(_)));
        let mut unknown = message(&adapter_room, "hello");
        unknown.destination = "4".into();
        assert!(matches!(adapter.send(unknown), AdapterResult::Dropped(_)));
        assert!(matches!(
            adapter.send(message(&adapter_room, "hello")),
            AdapterResult::Accepted(None)
        ));
        assert_eq!(fs::read_to_string(dir.join("count")).unwrap().trim(), "1");
        // Once the room's process is gone, nothing is started for it again.
        live.store(false, Ordering::SeqCst);
        assert!(matches!(
            adapter.send(message(&adapter_room, "hello")),
            AdapterResult::Dropped(reason) if reason.contains("no longer running")
        ));
        assert_eq!(fs::read_to_string(dir.join("count")).unwrap().trim(), "1");
    }
}
