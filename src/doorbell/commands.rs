//! Doorbell command-line clients.

use std::{
    env,
    io::{self, IsTerminal, Read},
    process,
    time::{SystemTime, UNIX_EPOCH},
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
                events.push(DetailEvent::TodoUpsert { id, content, status });
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
                events.push(DetailEvent::TodoUpsert { id, content, status });
            }
        }
        _ => {}
    }
    events
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
            hook_payload_detail(br#"{"hook_event_name":"PreToolUse","model":"o4-mini"}"#.as_slice()),
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
        assert_eq!(hook_payload_model(payload.as_slice()), Some("o4-mini".to_owned()));
        assert_eq!(hook_payload_detail(payload.as_slice()).len(), 1);
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
