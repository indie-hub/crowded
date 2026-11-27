//! Doorbell command-line clients.

use std::{
    env,
    io::{self, IsTerminal, Read},
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::Deserialize;

use crate::room_detail::DetailEvent;

#[cfg(unix)]
use super::client_unix::send_request;
#[cfg(windows)]
use super::client_windows::send_request;
use super::protocol::*;

pub(super) struct SendArgs {
    pub(super) target: usize,
    pub(super) task: Option<String>,
    pub(super) role: Option<String>,
    pub(super) body: String,
}

const SEND_USAGE: &str = "usage: crowded send ROOM [--task ID] [--role ROLE] [--] MESSAGE";

#[cfg(not(any(unix, windows)))]
fn send_request(_: &WireRequest) -> Result<WireResponse, Box<dyn std::error::Error>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Doorbell local transport is not available on this platform",
    )
    .into())
}

pub(super) fn parse_send_args(args: impl IntoIterator<Item = String>) -> Result<SendArgs, String> {
    let mut args = args.into_iter();
    let target = args
        .next()
        .ok_or_else(|| SEND_USAGE.to_owned())?
        .parse()
        .map_err(|_| SEND_USAGE.to_owned())?;
    let mut task = None;
    let mut role = None;
    let mut body = Vec::new();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--task" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--task requires an ID".to_owned())?;
                if task.replace(value).is_some() {
                    return Err("--task may appear only once".to_owned());
                }
            }
            "--role" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--role requires a role".to_owned())?;
                if role.replace(value).is_some() {
                    return Err("--role may appear only once".to_owned());
                }
            }
            "--" => {
                body.extend(args);
                break;
            }
            _ => {
                body.push(argument);
                body.extend(args);
                break;
            }
        }
    }

    let body = body.join(" ");
    if body.is_empty() {
        return Err(SEND_USAGE.to_owned());
    }
    validate_label("task", task.as_deref())?;
    validate_label("role", role.as_deref())?;
    Ok(SendArgs {
        target,
        task,
        role,
        body,
    })
}

pub(crate) fn send_command() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_send_args(env::args().skip(2))?;

    let token = env::var("CROWDED_TOKEN").map_err(|_| "CROWDED_TOKEN is not set")?;
    let room = env::var("CROWDED_ROOM").unwrap_or_else(|_| "external".to_owned());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let request = WireRequest::Message(MessageRequest {
        token,
        id: format!("{room}-{}-{now}", process::id()),
        to: args.target,
        body: args.body,
        task: args.task,
        role: args.role,
        hop: 0,
    });

    let response = send_request(&request)?;
    println!("{}", serde_json::to_string(&response)?);
    response.into_result("Doorbell rejected envelope")
}

const CONTROL_USAGE: &str = "usage: crowded control ROOM clear | resume | model MODEL | \
     effort LEVEL | model MODEL effort LEVEL";

pub(super) fn parse_control_args(
    args: impl IntoIterator<Item = String>,
) -> Result<(usize, ControlAction), String> {
    let mut args = args.into_iter().peekable();
    let target = args
        .next()
        .ok_or_else(|| CONTROL_USAGE.to_owned())?
        .parse()
        .map_err(|_| CONTROL_USAGE.to_owned())?;
    if args.peek().is_some_and(|next| next == "clear") {
        args.next();
        if args.next().is_none() {
            return Ok((target, ControlAction::ClearContext));
        }
        return Err(CONTROL_USAGE.to_owned());
    }
    if args.peek().is_some_and(|next| next == "resume") {
        args.next();
        if args.next().is_none() {
            return Ok((target, ControlAction::Resume));
        }
        return Err(CONTROL_USAGE.to_owned());
    }
    let mut model = None;
    let mut effort = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "clear" | "resume" => return Err(CONTROL_USAGE.to_owned()),
            "model" => {
                if model.is_some() {
                    return Err(CONTROL_USAGE.to_owned());
                }
                let value = args.next().ok_or_else(|| CONTROL_USAGE.to_owned())?;
                model = Some(value);
            }
            "effort" => {
                if effort.is_some() {
                    return Err(CONTROL_USAGE.to_owned());
                }
                let value = args.next().ok_or_else(|| CONTROL_USAGE.to_owned())?;
                let parsed = match value.as_str() {
                    "low" => Effort::Low,
                    "medium" => Effort::Medium,
                    "high" => Effort::High,
                    "xhigh" => Effort::Xhigh,
                    "max" => Effort::Max,
                    _ => return Err(CONTROL_USAGE.to_owned()),
                };
                effort = Some(parsed);
            }
            _ => return Err(CONTROL_USAGE.to_owned()),
        }
    }
    if model.is_none() && effort.is_none() {
        return Err(CONTROL_USAGE.to_owned());
    }
    Ok((target, ControlAction::Configure { model, effort }))
}

pub(crate) fn control_command() -> Result<(), Box<dyn std::error::Error>> {
    let (target, action) = parse_control_args(env::args().skip(2))?;
    let token = env::var("CROWDED_TOKEN").map_err(|_| "CROWDED_TOKEN is not set")?;
    let room = env::var("CROWDED_ROOM").unwrap_or_else(|_| "external".to_owned());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let request = WireRequest::Control(ControlRequest {
        token,
        id: format!("{room}-control-{}-{now}", process::id()),
        to: target,
        action,
    });

    let response = send_request(&request)?;
    println!("{}", serde_json::to_string(&response)?);
    response.into_result("Doorbell rejected control")
}

pub(crate) fn pulse_command() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(2);
    let state = match args.next().as_deref() {
        Some("starting") => PulseState::Starting,
        Some("thinking") => PulseState::Thinking,
        Some("working") => PulseState::Working,
        Some("ready") => PulseState::Ready,
        Some("error") => PulseState::Error,
        Some("offline") => PulseState::Offline,
        _ => {
            return Err(PULSE_USAGE.into());
        }
    };
    let mut model: Option<String> = None;
    let mut detail = None;
    let mut from_hook = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--model" => {
                let value = args.next().ok_or(PULSE_USAGE)?;
                if value.is_empty() {
                    return Err(PULSE_USAGE.into());
                }
                model = Some(value);
            }
            "--hook-stdin" => from_hook = true,
            _ => return Err(PULSE_USAGE.into()),
        }
    }
    if from_hook {
        if model.is_some() {
            return Err("--model and --hook-stdin cannot be combined".into());
        }
        let mut text = String::new();
        let captured = (!io::stdin().is_terminal())
            .then(|| io::stdin().read_to_string(&mut text).ok().map(|_| &text))
            .flatten();
        if let Some(text) = captured {
            model = hook_payload_model(text.as_bytes());
            detail = Some(hook_payload_detail(text.as_bytes()));
        }
    }
    let token = env::var("CROWDED_TOKEN").map_err(|_| "CROWDED_TOKEN is not set")?;
    let room = env::var("CROWDED_ROOM").unwrap_or_else(|_| "external".to_owned());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let response = send_request(&WireRequest::Pulse(PulseRequest {
        token,
        id: format!("{room}-pulse-{}-{now}", process::id()),
        state,
        model,
        detail,
    }))?;
    response.into_result("Doorbell rejected pulse")
}

const PULSE_USAGE: &str = "usage: crowded pulse starting|thinking|working|ready|error|offline [--model MODEL | --hook-stdin]";

/// A permissive view of a vendor hook payload: the pulse hook reads the
/// optional active `model`, plus Claude Code's sub-agent and todo events when
/// the runtime supplies them. Unknown fields are ignored so the same reader
/// works for Claude, Codex, and any future hook shape.
#[derive(Deserialize)]
struct HookPayload {
    model: Option<String>,
    hook_event_name: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    task_id: Option<String>,
    task_subject: Option<String>,
    todo: Option<String>,
    status: Option<String>,
    tool_input: Option<HookToolInput>,
}

#[derive(Deserialize)]
struct HookToolInput {
    task: Option<String>,
}

/// Reads a vendor hook JSON payload and returns its `model` field when present
/// and usable, so a generated hook command can hand the runtime-reported model
/// to `crowded pulse --hook-stdin` without shell-side JSON parsing. Returns
/// `None` when the input is not JSON, carries no model, or the model is not
/// a usable slug; the pulse then degrades to the plain no-model report.
fn hook_payload_model<R: io::Read>(mut input: R) -> Option<String> {
    let mut text = String::new();
    input.read_to_string(&mut text).ok()?;
    let payload: HookPayload = serde_json::from_str(&text).ok()?;
    payload.model.filter(|model| {
        !model.is_empty()
            && model.len() <= MAX_MODEL_BYTES
            && !model.starts_with('-')
            && !model.chars().any(char::is_control)
    })
}

/// Reads a vendor hook JSON payload and returns the sub-agent/todo events it
/// describes. Claude Code emits `SubagentStart`/`SubagentStop` and
/// `TaskCreate`/`TaskUpdate`/`TodoWrite`; every other event name or payload
/// yields an empty list.
fn hook_payload_detail<R: io::Read>(mut input: R) -> Vec<DetailEvent> {
    let mut text = String::new();
    if input.read_to_string(&mut text).is_err() {
        return Vec::new();
    }
    let Ok(payload) = serde_json::from_str::<HookPayload>(&text) else {
        return Vec::new();
    };
    hook_detail_events(&payload)
}

fn hook_detail_events(payload: &HookPayload) -> Vec<DetailEvent> {
    let mut events = Vec::new();
    match payload.hook_event_name.as_deref() {
        Some("SubagentStart") => {
            if let Some(id) = payload.agent_id.as_deref() {
                events.push(DetailEvent::SubAgentStarted {
                    id: id.to_owned(),
                    kind: payload
                        .agent_type
                        .clone()
                        .unwrap_or_else(|| "task".to_owned()),
                });
            }
        }
        Some("SubagentStop") => {
            if let Some(id) = payload.agent_id.as_deref() {
                events.push(DetailEvent::SubAgentStopped { id: id.to_owned() });
            }
        }
        Some("TaskCreate") | Some("TaskUpdate") => {
            let id = payload
                .task_id
                .clone()
                .or_else(|| payload.agent_id.clone())
                .unwrap_or_default();
            if !id.is_empty() {
                let content = payload
                    .task_subject
                    .clone()
                    .or_else(|| payload.todo.clone())
                    .or_else(|| {
                        payload
                            .tool_input
                            .as_ref()
                            .and_then(|input| input.task.clone())
                    })
                    .unwrap_or_default();
                let status = payload
                    .status
                    .clone()
                    .unwrap_or_else(|| "pending".to_owned());
                events.push(DetailEvent::TodoUpsert {
                    id,
                    content,
                    status,
                });
            }
        }
        Some("TodoWrite") => {
            let content = payload.todo.clone().unwrap_or_default();
            if !content.is_empty() {
                let status = payload
                    .status
                    .clone()
                    .unwrap_or_else(|| "pending".to_owned());
                let id = payload.task_id.clone().unwrap_or_else(|| content.clone());
                events.push(DetailEvent::TodoUpsert {
                    id,
                    content,
                    status,
                });
            }
        }
        _ => {}
    }
    events
}

const SESSION_REPORT_USAGE: &str = "usage: crowded session-report [--session ID] [--pending] [--ack] (the id may also arrive as JSON on stdin with session_id or sessionID). Without --ack the command is silent and always exits 0, so a hook never breaks its CLI. With --ack it exits 0 only when Crowded accepted the report: 2 usage or nothing to report, 3 invalid session id, 4 not configured (no token or unusable environment), 5 Crowded unreachable or silent for 4 seconds, 6 report rejected";

/// The most a report exchange (connect, write, read) may take. A hook must
/// never hang, and a server that accepts the request and never answers would
/// otherwise block the command forever. The shared clients set no deadline, so
/// `session-report` bounds the whole exchange itself: a worker thread does the
/// exchange and the command stops waiting for it, which works the same on the
/// Unix socket and the Windows pipe, where std has no connect timeout.
const SESSION_REPORT_DEADLINE: Duration = Duration::from_secs(4);

fn send_within(request: WireRequest, deadline: Duration) -> Result<WireResponse, String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = sender.send(send_request(&request).map_err(|error| error.to_string()));
    });
    receiver.recv_timeout(deadline).unwrap_or_else(|_| {
        Err(format!(
            "Crowded did not answer within {} seconds",
            deadline.as_secs()
        ))
    })
}

/// How `--ack` reports a failed report: each cause has its own exit status so a
/// caller that needs proof of delivery can tell them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReportFailure {
    Usage,
    InvalidId,
    NotConfigured,
    Unreachable,
    Rejected,
}

impl ReportFailure {
    fn exit_code(self) -> i32 {
        match self {
            ReportFailure::Usage => 2,
            ReportFailure::InvalidId => 3,
            ReportFailure::NotConfigured => 4,
            ReportFailure::Unreachable => 5,
            ReportFailure::Rejected => 6,
        }
    }
}

/// What the hook reported: a session id, a between-sessions mark, or both.
struct SessionReport {
    session: Option<String>,
    pending: bool,
}

fn parse_session_report_args(
    args: impl IntoIterator<Item = String>,
) -> Result<SessionReport, String> {
    let mut args = args.into_iter();
    let mut session = None;
    let mut pending = false;
    let mut ack = false;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--session" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--session requires an ID".to_owned())?;
                if session.replace(value).is_some() {
                    return Err("--session may appear only once".to_owned());
                }
            }
            "--pending" => {
                if pending {
                    return Err("--pending may appear only once".to_owned());
                }
                pending = true;
            }
            "--ack" => {
                if ack {
                    return Err("--ack may appear only once".to_owned());
                }
                ack = true;
            }
            _ => return Err(SESSION_REPORT_USAGE.to_owned()),
        }
    }
    Ok(SessionReport { session, pending })
}

/// What stdin said, when the caller gave no `--session`. A hook either pipes
/// a JSON payload and closes, or leaves the pipe open; the read below is
/// bounded so an open pipe can never hang the hook.
enum StdinSession {
    /// Nothing arrived, or nothing parseable: proceed without a session id.
    Absent,
    /// A session key arrived but its value is unusable: reject before
    /// contacting the server, even with `--pending`.
    Invalid,
    /// A usable session id from `session_id` (Codex) or `sessionID`.
    Valid(String),
}

/// How long a piped stdin may take to deliver its bytes before the command
/// proceeds without them. Genuine hook payloads are already waiting; only an
/// open pipe that never delivers anything waits the whole bound. This is the
/// bound for `--pending`, which can answer from the mark alone and must never
/// make a hook wait.
const STDIN_WAIT: Duration = Duration::from_millis(500);

/// How long `session-report` waits for stdin when stdin is the only place a
/// report can come from: no `--session` and no `--pending`, so a JSON payload
/// on the pipe is the whole report. A hook that creates a session during a
/// slow frame may write well after the command starts, so this bound is long
/// enough to catch such a payload and still ends the process in time.
const STDIN_REPORT_WAIT: Duration = Duration::from_secs(5);

/// Reads a hook payload from already-collected stdin bytes. Unknown fields
/// are ignored so the same reader works for Claude, Codex, and any future
/// hook shape. Only a truncated or malformed document counts as absent,
/// never a false rejection: a key that is present with any non-string
/// value, `null` included, is invalid, and only a missing key is absent.
fn parse_session_payload(bytes: &[u8]) -> StdinSession {
    if bytes.is_empty() {
        return StdinSession::Absent;
    }
    // Anything that is not one whole JSON document has not arrived yet, so
    // the caller keeps waiting rather than calling it absent.
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return StdinSession::Absent;
    };
    for key in ["session_id", "sessionID"] {
        match payload.get(key) {
            // Only a missing key is absent.
            None => {}
            Some(serde_json::Value::String(id)) => {
                return if valid_session_id(id) {
                    StdinSession::Valid(id.clone())
                } else {
                    StdinSession::Invalid
                };
            }
            // Present with a value no session id can have, null included:
            // reject it before contacting the server instead of pretending
            // the key was not there.
            Some(_) => return StdinSession::Invalid,
        }
    }
    StdinSession::Absent
}

/// Whether stdin already holds one whole JSON document. The payload is then
/// as complete as it will ever get, so waiting for EOF would add nothing.
fn payload_is_complete(bytes: &[u8]) -> bool {
    !bytes.is_empty() && serde_json::from_slice::<serde_json::Value>(bytes).is_ok()
}

/// Collects stdin until the payload is complete, EOF arrives, or `bound`
/// expires. An open pipe that never delivers anything yields whatever
/// (possibly nothing) arrived in time.
fn read_stdin_bounded(bound: Duration) -> Vec<u8> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let worker = Arc::clone(&buffer);
    let at_end = Arc::new(AtomicBool::new(false));
    let worker_at_end = Arc::clone(&at_end);
    let (arrival, arrived) = mpsc::channel::<()>();
    thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        let stdin = io::stdin();
        let mut locked = stdin.lock();
        loop {
            match locked.read(&mut chunk) {
                Ok(0) => break,
                Ok(count) => {
                    worker
                        .lock()
                        .expect("stdin buffer is never poisoned")
                        .extend_from_slice(&chunk[..count]);
                    let _ = arrival.send(());
                }
                Err(_) => break,
            }
        }
        worker_at_end.store(true, Ordering::SeqCst);
        let _ = arrival.send(());
    });
    let started = Instant::now();
    loop {
        {
            let held = buffer.lock().expect("stdin buffer is never poisoned");
            if at_end.load(Ordering::SeqCst) || payload_is_complete(&held) {
                return held.clone();
            }
        }
        let remaining = bound.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break;
        }
        let _ = arrived.recv_timeout(remaining);
    }
    buffer
        .lock()
        .expect("stdin buffer is never poisoned")
        .clone()
}

/// A hook may deliver the id as JSON on stdin, as `session_id` (Codex) or
/// `sessionID` (Claude/OpenCode). Anything else yields no id.
/// Test-only now: production reads through [`parse_session_payload`].
#[cfg(test)]
fn session_id_from_reader<R: io::Read>(mut input: R) -> Option<String> {
    let mut text = Vec::new();
    input.read_to_end(&mut text).ok()?;
    match parse_session_payload(&text) {
        StdinSession::Valid(id) => Some(id),
        StdinSession::Absent | StdinSession::Invalid => None,
    }
}

/// A session id is an opaque hook string: non-empty, bounded, and a
/// conservative charset. Anything else is rejected before contacting the
/// server, so a malformed payload can never rebind a room.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_SESSION_BYTES
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

/// Reports this room's session to Crowded from a hook. Silent on stdout in
/// every outcome: hook stdout can become model context. Failures get at most
/// one short stderr line. Without `--ack` it always exits 0, so a hook never
/// breaks its CLI; with `--ack` the exit status says whether Crowded accepted
/// the report (see `SESSION_REPORT_USAGE`).
pub(crate) fn session_report_command() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(2).collect();
    let ack = args.iter().any(|argument| argument == "--ack");
    match deliver_session_report(args) {
        Ok(()) => Ok(()),
        Err((failure, message)) => {
            eprintln!("crowded session-report: {message}");
            if ack {
                process::exit(failure.exit_code());
            }
            Ok(())
        }
    }
}

fn deliver_session_report(args: Vec<String>) -> Result<(), (ReportFailure, String)> {
    let report = parse_session_report_args(args).map_err(|error| (ReportFailure::Usage, error))?;
    let malformed = || {
        (
            ReportFailure::InvalidId,
            "ignoring a malformed session id".to_owned(),
        )
    };
    let session = match report.session {
        Some(id) => {
            if !valid_session_id(&id) {
                return Err(malformed());
            }
            Some(id)
        }
        // An explicit id needs no stdin at all, and a terminal never carries
        // a hook payload. Otherwise read whatever piped stdin holds, bounded
        // so an open pipe can never hang the hook: `--pending` may still be
        // answered by the mark alone, so it keeps the short bound, while
        // without either flag stdin is the report's only possible input and
        // gets the longer one.
        None if io::stdin().is_terminal() => None,
        None => {
            let bound = if report.pending {
                STDIN_WAIT
            } else {
                STDIN_REPORT_WAIT
            };
            match parse_session_payload(&read_stdin_bounded(bound)) {
                StdinSession::Absent => None,
                StdinSession::Valid(id) => Some(id),
                StdinSession::Invalid => return Err(malformed()),
            }
        }
    };
    if session.is_none() && !report.pending {
        return Err((
            ReportFailure::Usage,
            "nothing to report: pass --session, --pending, or JSON on stdin".to_owned(),
        ));
    }
    let token = env::var("CROWDED_TOKEN").map_err(|_| {
        (
            ReportFailure::NotConfigured,
            "CROWDED_TOKEN is not set".to_owned(),
        )
    })?;
    let room = env::var("CROWDED_ROOM").unwrap_or_else(|_| "external".to_owned());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| {
            (
                ReportFailure::NotConfigured,
                "the system clock is before the epoch".to_owned(),
            )
        })?
        .as_nanos();
    let response = send_within(
        WireRequest::SessionReport(SessionReportRequest {
            token,
            id: format!("{room}-session-report-{now}"),
            session,
            pending: report.pending,
            launch: env::var("CROWDED_LAUNCH_GENERATION")
                .ok()
                .and_then(|value| value.parse::<u64>().ok()),
        }),
        SESSION_REPORT_DEADLINE,
    )
    .map_err(|error| (ReportFailure::Unreachable, error))?;
    if response.ok {
        Ok(())
    } else {
        Err((
            ReportFailure::Rejected,
            response.error.unwrap_or_else(|| response.status.clone()),
        ))
    }
}

pub(crate) fn roster_command() -> Result<(), Box<dyn std::error::Error>> {
    match (env::args().nth(2).as_deref(), env::args().nth(3)) {
        (None, None) | (Some("--json"), None) => {}
        _ => return Err("usage: crowded roster [--json]".into()),
    }
    let token = env::var("CROWDED_TOKEN").map_err(|_| "CROWDED_TOKEN is not set")?;
    let room = env::var("CROWDED_ROOM").unwrap_or_else(|_| "external".to_owned());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let response = send_request(&WireRequest::Roster(RosterRequest {
        token,
        id: format!("{room}-roster-{}-{now}", process::id()),
    }))?;
    println!("{}", serde_json::to_string(&response)?);
    response.into_result("Doorbell rejected roster request")
}

impl WireResponse {
    fn into_result(self, fallback: &'static str) -> Result<(), Box<dyn std::error::Error>> {
        if self.ok {
            Ok(())
        } else {
            Err(self.error.unwrap_or_else(|| fallback.to_owned()).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_payload_model_extracts_the_active_model_when_present() {
        assert_eq!(
            hook_payload_model(
                br#"{"session_id":"s","hook_event_name":"PreToolUse","model":"o4-mini"}"#
                    .as_slice()
            ),
            Some("o4-mini".to_owned())
        );
        assert_eq!(
            hook_payload_model(br#"{"model":"deepseek/deepseek-v4-flash"}"#.as_slice()),
            Some("deepseek/deepseek-v4-flash".to_owned())
        );
    }

    #[test]
    fn hook_payload_model_degrades_when_the_payload_carries_no_usable_model() {
        assert_eq!(hook_payload_model(br#"{}"#.as_slice()), None);
        assert_eq!(
            hook_payload_model(br#"{"session_id":"s"}"#.as_slice()),
            None
        );
        assert_eq!(hook_payload_model(br#"{"model":""}"#.as_slice()), None);
        assert_eq!(hook_payload_model(b"not json".as_slice()), None);
        assert_eq!(hook_payload_model(b"".as_slice()), None);
    }

    #[test]
    fn hook_payload_model_rejects_a_model_that_would_fail_server_validation() {
        assert_eq!(
            hook_payload_model(br#"{"model":"-startswith-dash"}"#.as_slice()),
            None
        );
        assert_eq!(
            hook_payload_model(br#"{"model":"has\ncontrol"}"#.as_slice()),
            None
        );
        let oversized = format!(r#"{{"model":"{}"}}"#, "x".repeat(129));
        assert_eq!(hook_payload_model(oversized.as_bytes()), None);
    }

    #[test]
    fn hook_payload_detail_extracts_sub_agent_and_todo_events() {
        assert_eq!(
            hook_payload_detail(
                br#"{"hook_event_name":"SubagentStart","agent_id":"a1","agent_type":"Task"}"#
                    .as_slice()
            ),
            vec![DetailEvent::SubAgentStarted {
                id: "a1".to_owned(),
                kind: "Task".to_owned()
            }]
        );
        assert_eq!(
            hook_payload_detail(
                br#"{"hook_event_name":"SubagentStop","agent_id":"a1"}"#.as_slice()
            ),
            vec![DetailEvent::SubAgentStopped {
                id: "a1".to_owned()
            }]
        );
        assert_eq!(
            hook_payload_detail(
                br#"{"hook_event_name":"TodoWrite","todo":"Ship it","status":"in_progress"}"#
                    .as_slice()
            ),
            vec![DetailEvent::TodoUpsert {
                id: "Ship it".to_owned(),
                content: "Ship it".to_owned(),
                status: "in_progress".to_owned()
            }]
        );
        assert_eq!(
            hook_payload_detail(
                br#"{"hook_event_name":"TaskCreate","task_id":"t9","task_subject":"Design"}"#
                    .as_slice()
            ),
            vec![DetailEvent::TodoUpsert {
                id: "t9".to_owned(),
                content: "Design".to_owned(),
                status: "pending".to_owned()
            }]
        );
    }

    #[test]
    fn hook_payload_detail_ignores_unrelated_and_malformed_payloads() {
        assert_eq!(
            hook_payload_detail(
                br#"{"hook_event_name":"PreToolUse","model":"o4-mini"}"#.as_slice()
            ),
            Vec::<DetailEvent>::new()
        );
        assert_eq!(hook_payload_detail(br#"{}"#.as_slice()), Vec::new());
        assert_eq!(hook_payload_detail(b"not json".as_slice()), Vec::new());
        assert_eq!(hook_payload_detail(b"".as_slice()), Vec::new());
        // A SubagentStart without an id carries no usable event.
        assert_eq!(
            hook_payload_detail(
                br#"{"hook_event_name":"SubagentStart","agent_type":"Task"}"#.as_slice()
            ),
            Vec::<DetailEvent>::new()
        );
    }

    #[test]
    fn hook_payload_model_and_detail_read_the_same_payload() {
        let payload = br#"{"hook_event_name":"SubagentStart","agent_id":"a1","model":"o4-mini"}"#;
        assert_eq!(
            hook_payload_model(payload.as_slice()),
            Some("o4-mini".to_owned())
        );
        assert_eq!(hook_payload_detail(payload.as_slice()).len(), 1);
    }

    #[test]
    fn session_report_args_parse_each_shape_once() {
        let parsed = parse_session_report_args(["--session", "ses-x"].map(str::to_owned)).unwrap();
        assert_eq!(parsed.session.as_deref(), Some("ses-x"));
        assert!(!parsed.pending);
        let parsed = parse_session_report_args(["--pending".to_owned()]).unwrap();
        assert_eq!(parsed.session, None);
        assert!(parsed.pending);
        let parsed =
            parse_session_report_args(["--session", "ses-x", "--pending"].map(str::to_owned))
                .unwrap();
        assert_eq!(parsed.session.as_deref(), Some("ses-x"));
        assert!(parsed.pending);
        assert!(parse_session_report_args(Vec::<String>::new()).is_ok());
        assert!(parse_session_report_args(["--session".to_owned()]).is_err());
        assert!(
            parse_session_report_args(["--session", "a", "--session", "b"].map(str::to_owned))
                .is_err()
        );
        assert!(parse_session_report_args(["--pending", "--pending"].map(str::to_owned)).is_err());
        assert!(parse_session_report_args(["--model", "x"].map(str::to_owned)).is_err());
        assert!(parse_session_report_args(["ses-x".to_owned()]).is_err());
    }

    #[test]
    fn session_stdin_distinguishes_absent_from_invalid() {
        assert!(matches!(parse_session_payload(b""), StdinSession::Absent));
        assert!(matches!(
            parse_session_payload(b"not json"),
            StdinSession::Absent
        ));
        assert!(matches!(
            parse_session_payload(br#"{}"#,),
            StdinSession::Absent
        ));
        assert!(matches!(
            parse_session_payload(br#"{"other":1}"#),
            StdinSession::Absent
        ));
        assert!(matches!(
            parse_session_payload(br#"{"session_id":"ses-x"}"#),
            StdinSession::Valid(_)
        ));
        assert!(matches!(
            parse_session_payload(br#"{"sessionID":"ses-x"}"#),
            StdinSession::Valid(_)
        ));
        // A present but unusable id is Invalid, never silently dropped.
        assert!(matches!(
            parse_session_payload(br#"{"session_id":""}"#),
            StdinSession::Invalid
        ));
        assert!(matches!(
            parse_session_payload(br#"{"session_id":"has space"}"#),
            StdinSession::Invalid
        ));
        assert!(matches!(
            parse_session_payload(br#"{"sessionID":"bad/id"}"#),
            StdinSession::Invalid
        ));
        let oversized = format!(r#"{{"session_id":"{}"}}"#, "s".repeat(129));
        assert!(matches!(
            parse_session_payload(oversized.as_bytes()),
            StdinSession::Invalid
        ));
        // A truncated payload is absent, not a false rejection.
        assert!(matches!(
            parse_session_payload(br#"{"session_id":"se"#,),
            StdinSession::Absent
        ));
    }

    #[test]
    fn session_report_reads_either_session_key_from_stdin() {
        assert_eq!(
            session_id_from_reader(br#"{"session_id":"ses-codex-1"}"#.as_slice()),
            Some("ses-codex-1".to_owned())
        );
        assert_eq!(
            session_id_from_reader(br#"{"sessionID":"ses-open-2"}"#.as_slice()),
            Some("ses-open-2".to_owned())
        );
        assert_eq!(
            session_id_from_reader(br#"{"session_id":"first","sessionID":"second"}"#.as_slice()),
            Some("first".to_owned())
        );
        assert_eq!(session_id_from_reader(br#"{}"#.as_slice()), None);
        assert_eq!(session_id_from_reader(b"not json".as_slice()), None);
        assert_eq!(session_id_from_reader(b"".as_slice()), None);
        assert_eq!(
            session_id_from_reader(br#"{"session_id":""}"#.as_slice()),
            None
        );
        assert_eq!(
            session_id_from_reader(br#"{"session_id":"has space"}"#.as_slice()),
            None
        );
    }

    #[test]
    fn session_report_allows_only_conservative_session_characters() {
        for good in [
            "ses_abc",
            "ses-ABC.1",
            "11111111-2222-3333-4444-555555555555",
            "x",
        ] {
            assert!(valid_session_id(good), "{good} must pass");
        }
        for bad in ["", "has space", "tab\there", "semi;colon", "quote\"q"] {
            assert!(!valid_session_id(bad), "{bad:?} must fail");
        }
        assert!(!valid_session_id(&"s".repeat(129)));
        assert!(valid_session_id(&"s".repeat(128)));
    }

    #[test]
    fn session_report_command_never_prints_to_stdout() {
        // Hook stdout can become model context, so the whole command body is
        // held to silence: no println!, only eprintln! for the single short
        // stderr line. The production CLI returns Ok(()) on every path.
        let source = include_str!("commands.rs");
        let production = &source[..source.find("\n#[cfg(test)]\nmod tests").unwrap()];
        let command = &production[production.find("fn session_report_command(").unwrap()..];
        let command = &command[..command.find("\npub(crate) fn ").unwrap()];
        let without_stderr = command.replace("eprintln!", "");
        assert!(
            !without_stderr.contains("println!"),
            "session-report must stay silent on stdout"
        );
    }

    #[test]
    fn applying_hooked_events_builds_the_room_detail() {
        let mut detail = crate::room_detail::RoomDetail::default();
        for event in hook_payload_detail(
            br#"{"hook_event_name":"SubagentStart","agent_id":"a1","agent_type":"Task"}"#
                .as_slice(),
        ) {
            crate::room_detail::apply_detail_event(&mut detail, event);
        }
        for event in hook_payload_detail(
            br#"{"hook_event_name":"TodoWrite","todo":"Build","status":"completed"}"#.as_slice(),
        ) {
            crate::room_detail::apply_detail_event(&mut detail, event);
        }
        assert_eq!(detail.sub_agents.len(), 1);
        assert_eq!(detail.sub_agents[0].kind, "Task");
        assert_eq!(detail.todos.len(), 1);
        assert_eq!(detail.todos[0].content, "Build");
    }
}
