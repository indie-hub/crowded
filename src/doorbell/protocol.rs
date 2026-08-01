//! Doorbell wire types and trust-boundary validation.

use std::{
    collections::VecDeque,
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

pub(super) const MAX_WIRE_BYTES: usize = 8 * 1024;
pub(super) const MAX_BODY_BYTES: usize = 4 * 1024;
pub(super) const MAX_ID_BYTES: usize = 128;
pub(super) const MAX_LABEL_BYTES: usize = 64;
pub(super) const MAX_HOPS: u8 = 8;
pub(super) const MAX_MESSAGES_PER_SECOND: usize = 5;
pub(super) const EVENT_QUEUE_CAPACITY: usize = 100;
pub(super) const DEDUPE_CAPACITY: usize = 256;
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum WireRequest {
    Message(MessageRequest),
    Control(ControlRequest),
    Pulse(PulseRequest),
    Roster(RosterRequest),
}

#[derive(Deserialize, Serialize)]
pub(super) struct MessageRequest {
    pub(super) token: String,
    pub(super) id: String,
    pub(super) to: usize,
    pub(super) body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) hop: u8,
}

#[derive(Deserialize, Serialize)]
pub(super) struct PulseRequest {
    pub(super) token: String,
    pub(super) id: String,
    pub(super) state: PulseState,
}

#[derive(Deserialize, Serialize)]
pub(super) struct RosterRequest {
    pub(super) token: String,
    pub(super) id: String,
}

#[derive(Deserialize, Serialize)]
pub(super) struct ControlRequest {
    pub(super) token: String,
    pub(super) id: String,
    pub(super) to: usize,
    #[serde(flatten)]
    pub(super) action: ControlAction,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "action", content = "value", rename_all = "snake_case")]
pub(crate) enum ControlAction {
    ClearContext,
    SetModel(String),
    SetEffort(Effort),
}

impl ControlAction {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::ClearContext => "clear context",
            Self::SetModel(_) => "set model",
            Self::SetEffort(_) => "set effort",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PulseState {
    Starting,
    Thinking,
    Working,
    Ready,
    Error,
    Offline,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RosterRoom {
    pub(crate) room: usize,
    pub(crate) name: String,
    pub(crate) guest: String,
    pub(crate) vendor: String,
    pub(crate) transport: String,
    pub(crate) state: PulseState,
    pub(crate) allow_control: bool,
}

impl PulseState {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Thinking => "thinking",
            Self::Working => "working",
            Self::Ready => "ready",
            Self::Error => "error",
            Self::Offline => "offline",
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct WireResponse {
    pub(super) ok: bool,
    pub(super) status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) envelope_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) rooms: Option<Vec<RosterRoom>>,
}

impl WireResponse {
    pub(super) fn accepted(status: &str, envelope_id: Option<u64>) -> Self {
        Self {
            ok: true,
            status: status.to_owned(),
            envelope_id,
            error: None,
            rooms: None,
        }
    }

    pub(super) fn roster(rooms: Vec<RosterRoom>) -> Self {
        Self {
            ok: true,
            status: "listed".to_owned(),
            envelope_id: None,
            error: None,
            rooms: Some(rooms),
        }
    }

    pub(super) fn rejected(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            status: "rejected".to_owned(),
            envelope_id: None,
            error: Some(error.into()),
            rooms: None,
        }
    }
}

pub(crate) struct DoorbellEnvelope {
    pub(crate) from: usize,
    pub(crate) to: usize,
    pub(crate) body: String,
    pub(crate) task: Option<String>,
    pub(crate) role: Option<String>,
    pub(super) reply: SyncSender<WireResponse>,
}

pub(crate) struct DoorbellPulse {
    pub(crate) from: usize,
    pub(crate) state: PulseState,
}

pub(crate) struct DoorbellControl {
    pub(crate) from: usize,
    pub(crate) to: usize,
    pub(crate) action: ControlAction,
    pub(super) reply: SyncSender<WireResponse>,
}

pub(crate) struct DoorbellRoster {
    pub(super) reply: SyncSender<WireResponse>,
}

pub(crate) enum DoorbellEvent {
    Message(DoorbellEnvelope),
    Control(DoorbellControl),
    Pulse(DoorbellPulse),
    Roster(DoorbellRoster),
}

impl DoorbellEnvelope {
    pub(crate) fn reply_injected(&self, envelope_id: u64) {
        let _ = self
            .reply
            .send(WireResponse::accepted("injected", Some(envelope_id)));
    }

    pub(crate) fn reply_queued(&self, envelope_id: u64) {
        let _ = self
            .reply
            .send(WireResponse::accepted("queued", Some(envelope_id)));
    }

    pub(crate) fn reply_failed(&self, error: impl Into<String>) {
        let _ = self.reply.send(WireResponse::rejected(error));
    }
}

impl DoorbellControl {
    pub(crate) fn reply_applied(&self) {
        let _ = self.reply.send(WireResponse::accepted("applied", None));
    }

    pub(crate) fn reply_failed(&self, error: impl Into<String>) {
        let _ = self.reply.send(WireResponse::rejected(error));
    }
}

impl DoorbellRoster {
    pub(crate) fn reply(&self, rooms: Vec<RosterRoom>) {
        let _ = self.reply.send(WireResponse::roster(rooms));
    }
}

pub(super) fn validate_request(
    request: &MessageRequest,
    tokens: &[String],
    room_count: usize,
    recent_by_room: &mut [VecDeque<Instant>],
) -> Result<usize, String> {
    let from = validate_route(&request.token, &request.id, request.to, tokens, room_count)?;
    if request.body.is_empty() || request.body.len() > MAX_BODY_BYTES {
        return Err("message body must contain 1..=4096 bytes".to_owned());
    }
    validate_label("task", request.task.as_deref())?;
    validate_label("role", request.role.as_deref())?;
    if request.hop > MAX_HOPS {
        return Err("message exceeded hop limit".to_owned());
    }
    record_rate(from, recent_by_room)?;
    Ok(from)
}

pub(super) fn validate_control(
    request: &ControlRequest,
    tokens: &[String],
    room_count: usize,
    recent_by_room: &mut [VecDeque<Instant>],
) -> Result<usize, String> {
    let from = validate_route(&request.token, &request.id, request.to, tokens, room_count)?;
    if let ControlAction::SetModel(model) = &request.action
        && (model.is_empty()
            || model.len() > 128
            || model.starts_with('-')
            || model.chars().any(char::is_control))
    {
        return Err(
            "model must contain 1..=128 bytes, must not start with `-`, and must not contain controls"
                .to_owned(),
        );
    }
    record_rate(from, recent_by_room)?;
    Ok(from)
}

fn validate_route(
    token: &str,
    id: &str,
    to: usize,
    tokens: &[String],
    room_count: usize,
) -> Result<usize, String> {
    let from = tokens
        .iter()
        .position(|candidate| candidate == token)
        .ok_or_else(|| "invalid capability".to_owned())?;
    if !(1..=room_count).contains(&to) {
        return Err("target room does not exist".to_owned());
    }
    if to - 1 == from {
        return Err("source and target rooms must differ".to_owned());
    }
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err("request id must contain 1..=128 bytes".to_owned());
    }
    Ok(from)
}

fn record_rate(from: usize, recent_by_room: &mut [VecDeque<Instant>]) -> Result<(), String> {
    let now = Instant::now();
    let recent = &mut recent_by_room[from];
    while recent
        .front()
        .is_some_and(|sent| now.duration_since(*sent) >= Duration::from_secs(1))
    {
        recent.pop_front();
    }
    if recent.len() >= MAX_MESSAGES_PER_SECOND {
        return Err("source exceeded 5 messages per second".to_owned());
    }
    recent.push_back(now);
    Ok(())
}

fn validate_identity(
    token: &str,
    id: &str,
    kind: &str,
    tokens: &[String],
) -> Result<usize, String> {
    let from = tokens
        .iter()
        .position(|candidate| candidate == token)
        .ok_or_else(|| "invalid capability".to_owned())?;
    if id.is_empty() || id.len() > MAX_ID_BYTES {
        return Err(format!("{kind} id must contain 1..=128 bytes"));
    }
    Ok(from)
}

pub(super) fn validate_pulse(request: &PulseRequest, tokens: &[String]) -> Result<usize, String> {
    validate_identity(&request.token, &request.id, "pulse", tokens)
}

pub(super) fn validate_roster(
    request: &RosterRequest,
    tokens: &[String],
    recent_by_room: &mut [VecDeque<Instant>],
) -> Result<usize, String> {
    let from = validate_identity(&request.token, &request.id, "roster", tokens)?;
    record_rate(from, recent_by_room)?;
    Ok(from)
}

pub(super) fn validate_label(name: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value
        && (value.is_empty()
            || value.len() > MAX_LABEL_BYTES
            || value.chars().any(|character| character.is_control()))
    {
        return Err(format!(
            "{name} must contain 1..={MAX_LABEL_BYTES} bytes without control characters"
        ));
    }
    Ok(())
}
