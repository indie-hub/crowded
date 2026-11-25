//! Pure, IO-free rules for native session identity and mail holds: one owner
//! for the bug class behind ab39d84, 83c281b, e069509 and 7245fc1. No stored
//! per-room state lives here; the router and outbox maps own that. Callers
//! pass values in and get decisions back.

use crate::native_outbox::{Binding, Message, Status};

/// The hold reason of mail whose binding is no longer the room's current one.
pub(crate) const STALE_HOLD: &str = "sent for an earlier launch of this room; not delivered";
/// The hold reason a Claude room's mail carries until the room-side mod exists.
pub(crate) const CLAUDE_HOLD: &str = "no Claude mod connected";
/// The hold an older build wrote on mail behind a message that may already
/// have arrived. Nothing is sent past an uncertain message, so that mail was
/// never sent; this build releases it instead of stalling the room.
pub(crate) const UNCERTAIN_HOLD: &str =
    "an earlier message to this room is uncertain; nothing new is sent";
/// The hold on a message saved while its room reported itself between
/// sessions. It is never sent with its old binding; a later report for the
/// same room either adopts it onto the new session or, for the same session
/// id, releases it.
pub(crate) const PENDING_HOLD: &str = "session pending after clear";
/// The hold on a message whose enqueue reported an error. It is never sent, and
/// shows as held after a restart, so a caller that retries cannot cause a duplicate.
pub(crate) const UNCONFIRMED_HOLD: &str = "saving this message was not confirmed; it was not sent";
/// The hold for mail no mod collected in time. The mail never reached the
/// room, so a later offer is a first send, not a resend.
pub(crate) const UNCOLLECTED_HOLD: &str = "Claude mod did not collect the message";
/// The hold for mail the mod could not receive inside its response limit.
pub(crate) const TOO_LARGE_HOLD: &str = "the message is too large for the Claude mod";
/// The hold for a room whose registered mod stopped polling. The mail was
/// never queued for the mod, so a later offer is a first send, not a resend.
pub(crate) const DISCONNECTED_HOLD: &str = "Claude mod disconnected";
/// Prefixes a hold reason the Claude mod itself supplied, so it can never look
/// like one of this crate's own hold strings and be retried.
pub(crate) const MOD_HOLD_PREFIX: &str = "Claude mod: ";
/// The terminal state of an unconfirmed delivery. It does not claim the mail
/// was lost, and a later authenticated receipt may still correct it.
pub(crate) const UNCONFIRMED_FAILED: &str =
    "delivery was not confirmed; it may have arrived; not resent";
/// How long an unconfirmed row gets after its last attempt. Provisional: a
/// per-guest bound needs a measured late-receipt distribution.
pub(crate) const UNCERTAIN_BOUND_MS: u128 = 600_000;

/// Whether a row is the terminal unconfirmed state that evidence may correct.
pub(crate) fn is_unconfirmed_failed(status: &Status) -> bool {
    matches!(status, Status::Failed { reason } if reason == UNCONFIRMED_FAILED)
}

/// The only statuses a late authenticated receipt may settle.
pub(crate) fn is_correctable_unconfirmed(status: &Status) -> bool {
    matches!(status, Status::Uncertain) || is_unconfirmed_failed(status)
}

/// Whether an unconfirmed row is past its bound. The clock comes from the
/// persisted attempt, so it survives a restart; a backwards clock never
/// expires a row.
pub(crate) fn uncertain_expired(message: &Message, now_ms: u128) -> bool {
    if !matches!(message.status, Status::Uncertain) {
        return false;
    }
    let since = message
        .attempts
        .last()
        .map(|attempt| attempt.started_ms)
        .unwrap_or(message.created_ms);
    now_ms >= since && now_ms - since >= UNCERTAIN_BOUND_MS
}

/// The session name a Claude room is bound to before its own hook reports the
/// real session. It names one launch, never a session, and is never persisted.
pub(crate) fn unreported_session(room: &str, generation: u64) -> String {
    format!("unreported-{room}-{generation}")
}

/// Whether a binding is the unreported placeholder of its own launch.
pub(crate) fn is_unreported_placeholder(binding: &Binding) -> bool {
    binding.session == unreported_session(&binding.room, binding.generation)
}

/// Whether a held message still belongs to the room's current launch: it was
/// bound to this launch's unreported placeholder, for the same generation.
/// Mail bound to a real session or an older generation belongs to another
/// launch and is never retargeted.
pub(crate) fn same_launch_placeholder(held: &Binding, current: &Binding) -> bool {
    held.room == current.room
        && held.generation == current.generation
        && is_unreported_placeholder(held)
}

/// What a session report does to the launch generation: [`ReportedGeneration`]
/// keeps the caller's allocation and error handling.
pub(crate) enum ReportedGeneration {
    /// The launch keeps its generation: the report turns the launch's
    /// unreported placeholder into the real session, so mail saved against
    /// the placeholder still belongs to this launch.
    Keep(u64),
    /// A session switch or relaunch takes a new generation, which holds older
    /// mail as stale. The caller allocates it.
    Allocate,
}

/// Decides the generation for a session report without allocating anything:
/// a launch placeholder keeps its generation on its first report.
pub(crate) fn generation_for_report(current: Option<&Binding>) -> ReportedGeneration {
    match current {
        Some(binding) if is_unreported_placeholder(binding) => {
            ReportedGeneration::Keep(binding.generation)
        }
        _ => ReportedGeneration::Allocate,
    }
}

/// Whether a queued message is held only because its launch is gone: it can
/// never be adopted onto another binding.
pub(crate) fn is_stale_hold(message: &Message) -> bool {
    matches!(&message.status, Status::Queued { hold: Some(reason) } if reason == STALE_HOLD)
}

/// Holds retried once the room can receive again: the room could not take the
/// mail, and the mail never reached it.
pub(crate) fn is_retryable_hold(reason: &str) -> bool {
    reason == CLAUDE_HOLD
        || reason == UNCERTAIN_HOLD
        || reason == UNCOLLECTED_HOLD
        || reason == DISCONNECTED_HOLD
}

/// Holds that never stand in front of later mail: the mod refused the prompt,
/// the message can never be taken, or its save was never confirmed. Nothing
/// clears them, so blocking on one would strand the room. Every other
/// non-retryable hold - OpenCode's and Codex's "inspect before retry" reasons,
/// routing failures, unknown older reasons - keeps blocking its lane.
pub(crate) fn is_final_hold(reason: &str) -> bool {
    reason.starts_with(MOD_HOLD_PREFIX) || reason == TOO_LARGE_HOLD || reason == UNCONFIRMED_HOLD
}
