//! Doorbell wire types and trust-boundary validation.

use std::{
    collections::VecDeque,
    sync::mpsc::SyncSender,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::config::RoomScheduling;
use crate::room_detail::DetailEvent;

pub(super) const MAX_WIRE_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_BODY_BYTES: usize = 1024 * 1024;
pub(super) const MAX_ID_BYTES: usize = 128;
pub(super) const MAX_LABEL_BYTES: usize = 64;
pub(super) const MAX_HOPS: u8 = 8;
pub(super) const MAX_MESSAGES_PER_SECOND: usize = 5;
pub(super) const EVENT_QUEUE_CAPACITY: usize = 100;
pub(super) const DEDUPE_CAPACITY: usize = 256;
pub(super) const MAX_MODEL_BYTES: usize = 128;
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
    /// Optional self-reported model the hook is actually running, so the
    /// roster can show it even when the operator never set `control model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
    /// Optional incremental sub-agent/todo events parsed from a vendor hook
    /// payload, so a room's internal detail can be tracked in real time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<Vec<DetailEvent>>,
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
    Resume,
    Configure {
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
    },
}

impl ControlAction {
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Self::ClearContext => "clear context",
            Self::Resume => "resume",
            Self::Configure {
                model: Some(_),
                effort: Some(_),
            } => "set model and effort",
            Self::Configure {
                model: Some(_),
                effort: None,
            } => "set model",
            Self::Configure {
                model: None,
                effort: Some(_),
            } => "set effort",
            Self::Configure {
                model: None,
                effort: None,
            } => "set model and effort",
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
    /// How `state` was resolved: the process is offline, a fresh hook
    /// self-report is trusted, a stale transient self-report was overridden
    /// by demonstrable readiness, or the delivery gate/screen inferred it.
    /// Backward-compatible: old wire shapes without this field deserialize to
    /// `Gate` (the resolver's most common fallback).
    #[serde(default)]
    pub(crate) state_source: PulseSource,
    pub(crate) allow_control: bool,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    #[serde(default = "unknown_cost")]
    pub(crate) cost: String,
    pub(crate) headroom: bool,
    /// Age of the last received Pulse hook sample, in milliseconds, when one
    /// has been received. Lets consumers tell a fresh self-report from a
    /// stale one that the delivery gate has since overridden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pulse_age_ms: Option<u64>,
    /// Adapter-derived capability matrix: which peer controls the adapter can
    /// apply, which effort levels it accepts, and what is known about the
    /// model catalogue. Never probed at runtime.
    #[serde(default)]
    pub(crate) capabilities: RoomCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) scheduling: Option<RoomScheduling>,
}

fn unknown_cost() -> String {
    "unknown".to_owned()
}

/// Where a roster `state` came from. Kept explicit so the TUI and the JSON
/// roster cannot silently disagree about whether a state is a live hook
/// self-report or a conductor-side inference.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PulseSource {
    /// The process is offline; no hook or screen reading applies.
    Offline,
    /// The resolved state is a trusted fresh hook self-report.
    Hook,
    /// A stale transient hook self-report (starting/thinking/working) was
    /// overridden because the delivery gate demonstrably showed readiness.
    Readiness,
    /// The delivery gate and/or screen inference produced the state, with no
    /// trusted hook self-report in play.
    #[default]
    Gate,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RoomCapabilities {
    /// Backward-compatible summary for older roster consumers. New consumers
    /// should use `supported_controls`.
    #[serde(default)]
    pub(crate) controls: bool,
    /// Controls the Conductor adapter can apply without vendor probing.
    #[serde(default)]
    pub(crate) supported_controls: Vec<SupportedControl>,
    /// Effort levels the adapter accepts. Empty when the guest has no stable
    /// effort launch option (OpenCode), so nothing is claimed there.
    #[serde(default)]
    pub(crate) effort_levels: Vec<Effort>,
    /// The model catalogue. Crowded never probes vendors, so this is always
    /// `Unknown`; the configured `model` field is the only model claim.
    #[serde(default)]
    pub(crate) model_catalogue: ModelCatalogue,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SupportedControl {
    Clear,
    Resume,
    Model,
    Effort,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ModelCatalogue {
    #[default]
    Unknown,
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
    pub(crate) model: Option<String>,
    pub(crate) detail: Option<Vec<DetailEvent>>,
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
        return Err(format!(
            "message body must contain 1..={MAX_BODY_BYTES} bytes"
        ));
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
    if let ControlAction::Configure {
        model: Some(model), ..
    } = &request.action
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
    if let Some(model) = request.model.as_deref()
        && (model.is_empty()
            || model.len() > MAX_MODEL_BYTES
            || model.starts_with('-')
            || model.chars().any(char::is_control))
    {
        return Err(
            "model must contain 1..=128 bytes, must not start with `-`, and must not contain controls"
                .to_owned(),
        );
    }
    if let Some(events) = &request.detail {
        for event in events {
            validate_detail_event(event)?;
        }
    }
    validate_identity(&request.token, &request.id, "pulse", tokens)
}

fn validate_detail_event(event: &DetailEvent) -> Result<(), String> {
    const MAX_DETAIL_BYTES: usize = 512;
    let mut fields = Vec::new();
    match event {
        DetailEvent::SubAgentStarted { id, kind } => {
            fields.push(id);
            fields.push(kind);
        }
        DetailEvent::SubAgentStopped { id } => fields.push(id),
        DetailEvent::TodoUpsert {
            id,
            content,
            status,
        } => {
            fields.push(id);
            fields.push(content);
            fields.push(status);
        }
    }
    if fields
        .iter()
        .any(|field| field.len() > MAX_DETAIL_BYTES || field.chars().any(char::is_control))
    {
        return Err(format!(
            "detail fields must contain 0..={MAX_DETAIL_BYTES} bytes without control characters"
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_large_agent_message() {
        let request = MessageRequest {
            token: "sender".to_owned(),
            id: "large-message".to_owned(),
            to: 2,
            body: "x".repeat(64 * 1024),
            task: None,
            role: None,
            hop: 0,
        };
        let mut recent = vec![VecDeque::new(), VecDeque::new()];

        assert_eq!(
            validate_request(
                &request,
                &["sender".to_owned(), "receiver".to_owned()],
                2,
                &mut recent,
            ),
            Ok(0)
        );
    }

    #[test]
    fn roster_capabilities_serialize_backward_compatible_fields() {
        let room = RosterRoom {
            room: 1,
            name: "Builder".to_owned(),
            guest: "opencode".to_owned(),
            vendor: "sparks".to_owned(),
            transport: "raw".to_owned(),
            state: PulseState::Ready,
            state_source: PulseSource::Readiness,
            allow_control: true,
            model: Some("deepseek/deepseek-v4-flash".to_owned()),
            effort: Some("high".to_owned()),
            cost: "unknown".to_owned(),
            headroom: false,
            pulse_age_ms: Some(1234),
            capabilities: RoomCapabilities {
                controls: true,
                supported_controls: vec![
                    SupportedControl::Clear,
                    SupportedControl::Resume,
                    SupportedControl::Model,
                ],
                effort_levels: Vec::new(),
                model_catalogue: ModelCatalogue::Unknown,
            },
            scheduling: None,
        };
        let value = serde_json::to_value(&room).unwrap();
        assert_eq!(value["state"], "ready");
        assert_eq!(value["state_source"], "readiness");
        assert_eq!(value["pulse_age_ms"], 1234);
        assert_eq!(value["model"], "deepseek/deepseek-v4-flash");
        assert_eq!(value["effort"], "high");
        assert_eq!(value["cost"], "unknown");
        assert_eq!(value["capabilities"]["controls"], true);
        assert_eq!(
            value["capabilities"]["supported_controls"],
            serde_json::json!(["clear", "resume", "model"])
        );
        assert_eq!(
            value["capabilities"]["effort_levels"],
            serde_json::json!([])
        );
        assert_eq!(value["capabilities"]["model_catalogue"], "unknown");
        assert!(value.get("scheduling").is_none());
    }

    #[test]
    fn roster_scheduling_serializes_separately_from_capabilities() {
        let room = RosterRoom {
            room: 1,
            name: "Builder".to_owned(),
            guest: "codex".to_owned(),
            vendor: "openai".to_owned(),
            transport: "raw".to_owned(),
            state: PulseState::Ready,
            state_source: PulseSource::Hook,
            allow_control: true,
            model: None,
            effort: None,
            cost: "unknown".to_owned(),
            headroom: false,
            pulse_age_ms: None,
            capabilities: RoomCapabilities::default(),
            scheduling: Some(RoomScheduling {
                model_tier: Some("balanced".to_owned()),
                cost_tier: Some("medium".to_owned()),
                capabilities: vec!["implement".to_owned(), "validate".to_owned()],
            }),
        };
        let value = serde_json::to_value(&room).unwrap();
        assert_eq!(value["scheduling"]["model_tier"], "balanced");
        assert_eq!(value["scheduling"]["cost_tier"], "medium");
        assert_eq!(
            value["scheduling"]["capabilities"],
            serde_json::json!(["implement", "validate"])
        );
        assert_eq!(
            value["capabilities"],
            serde_json::json!({
                "controls": false,
                "supported_controls": [],
                "effort_levels": [],
                "model_catalogue": "unknown",
            })
        );
    }

    #[test]
    fn roster_capabilities_parse_older_wire_shapes_with_defaults() {
        let old = r#"{
            "room": 1,
            "name": "Builder",
            "guest": "opencode",
            "vendor": "sparks",
            "transport": "raw",
            "state": "ready",
            "allow_control": true,
            "model": null,
            "effort": null,
            "headroom": false
        }"#;
        let parsed: RosterRoom = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.state_source, PulseSource::Gate);
        assert_eq!(parsed.pulse_age_ms, None);
        assert!(!parsed.capabilities.controls);
        assert!(parsed.capabilities.supported_controls.is_empty());
        assert!(parsed.capabilities.effort_levels.is_empty());
        assert_eq!(parsed.capabilities.model_catalogue, ModelCatalogue::Unknown);
    }

    #[test]
    fn effort_labels_cover_the_doorbell_catalogue() {
        let labels = [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ]
        .iter()
        .map(|effort| effort.label())
        .collect::<Vec<_>>();
        assert_eq!(labels, ["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn pulse_validation_accepts_hook_detail_and_rejects_oversized_fields() {
        let request = PulseRequest {
            token: "left".to_owned(),
            id: "pulse-detail".to_owned(),
            state: PulseState::Working,
            model: None,
            detail: Some(vec![DetailEvent::SubAgentStarted {
                id: "a1".to_owned(),
                kind: "Task".to_owned(),
            }]),
        };
        assert_eq!(
            validate_pulse(&request, &["left".to_owned(), "right".to_owned()]),
            Ok(0)
        );

        let oversized = PulseRequest {
            token: "left".to_owned(),
            id: "pulse-oversized".to_owned(),
            state: PulseState::Working,
            model: None,
            detail: Some(vec![DetailEvent::TodoUpsert {
                id: "t".to_owned(),
                content: "x".repeat(513),
                status: "pending".to_owned(),
            }]),
        };
        assert!(validate_pulse(&oversized, &["left".to_owned(), "right".to_owned()]).is_err());
    }
}
