//! Doorbell command-line clients.

use std::{
    env, process,
    time::{SystemTime, UNIX_EPOCH},
};

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
    let state = match (env::args().nth(2).as_deref(), env::args().nth(3)) {
        (Some("starting"), None) => PulseState::Starting,
        (Some("thinking"), None) => PulseState::Thinking,
        (Some("working"), None) => PulseState::Working,
        (Some("ready"), None) => PulseState::Ready,
        (Some("error"), None) => PulseState::Error,
        (Some("offline"), None) => PulseState::Offline,
        _ => {
            return Err(
                "usage: crowded pulse starting|thinking|working|ready|error|offline".into(),
            );
        }
    };
    let token = env::var("CROWDED_TOKEN").map_err(|_| "CROWDED_TOKEN is not set")?;
    let room = env::var("CROWDED_ROOM").unwrap_or_else(|_| "external".to_owned());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let response = send_request(&WireRequest::Pulse(PulseRequest {
        token,
        id: format!("{room}-pulse-{}-{now}", process::id()),
        state,
    }))?;
    response.into_result("Doorbell rejected pulse")
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
