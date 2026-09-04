//! Ratatui rendering, focus, note composition, and the main event loop.

use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::mpsc,
    time::{Duration, Instant},
};

use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use portable_pty::PtySize;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use tui_term::widget::PseudoTerminal;

use crate::{
    config::{
        RoomFieldUpdate, RoomScheduling, RoomSpec, TokenPricing, crowded_toml_path,
        parse_allow_control_input, parse_capabilities_input, parse_cost_tier_input,
        parse_fuse_size_input, parse_model_tier_input, persist_fuse_size, persist_room_fields,
        room_specs, room_specs_resumed,
    },
    doorbell::{
        ControlAction, Doorbell, DoorbellEvent, PulseSource, PulseState, RosterRoom,
        SupportedControl,
    },
    mailroom::Mailroom,
    pane::{self, Pane},
    room_detail::{DetailEvent, RoomDetail, apply_detail_event, collect_detail},
};

enum InputMode {
    Normal,
    Composing(String),
    MailLog,
    Config(Box<ConfigOverlayState>),
}

#[derive(Clone, Copy, PartialEq)]
enum ConfigField {
    FuseSize,
    AllowControl,
    ModelTier,
    CostTier,
    Capabilities,
}

impl ConfigField {
    fn next(self) -> Self {
        match self {
            ConfigField::FuseSize => ConfigField::AllowControl,
            ConfigField::AllowControl => ConfigField::ModelTier,
            ConfigField::ModelTier => ConfigField::CostTier,
            ConfigField::CostTier => ConfigField::Capabilities,
            ConfigField::Capabilities => ConfigField::FuseSize,
        }
    }

    fn prev(self) -> Self {
        match self {
            ConfigField::FuseSize => ConfigField::Capabilities,
            ConfigField::AllowControl => ConfigField::FuseSize,
            ConfigField::ModelTier => ConfigField::AllowControl,
            ConfigField::CostTier => ConfigField::ModelTier,
            ConfigField::Capabilities => ConfigField::CostTier,
        }
    }
}

struct ConfigOverlayState {
    fuse_input: String,
    fuse_original: usize,
    selected: usize,
    field: ConfigField,
    allow_input: String,
    allow_original: String,
    tier_input: String,
    tier_original: String,
    cost_input: String,
    cost_original: String,
    caps_input: String,
    caps_original: String,
    error: Option<String>,
}

impl ConfigOverlayState {
    fn new(panes: &[Pane], selected: usize, fuse_size: usize) -> Self {
        let mut state = Self {
            fuse_input: fuse_size.to_string(),
            fuse_original: fuse_size,
            selected,
            field: ConfigField::FuseSize,
            allow_input: String::new(),
            allow_original: String::new(),
            tier_input: String::new(),
            tier_original: String::new(),
            cost_input: String::new(),
            cost_original: String::new(),
            caps_input: String::new(),
            caps_original: String::new(),
            error: None,
        };
        state.resync(panes);
        state
    }

    fn resync(&mut self, panes: &[Pane]) {
        let pane = &panes[self.selected];
        let scheduling = pane.scheduling();
        let tier = scheduling
            .as_ref()
            .and_then(|entry| entry.model_tier.as_deref())
            .unwrap_or_default();
        let cost = scheduling
            .as_ref()
            .and_then(|entry| entry.cost_tier.as_deref())
            .unwrap_or_default();
        let caps = scheduling
            .as_ref()
            .map(|entry| entry.capabilities.join(", "))
            .unwrap_or_default();
        self.allow_input = pane.allows_control().to_string();
        self.allow_original = self.allow_input.clone();
        self.tier_input = tier.to_owned();
        self.tier_original = self.tier_input.clone();
        self.cost_input = cost.to_owned();
        self.cost_original = self.cost_input.clone();
        self.caps_input = caps;
        self.caps_original = self.caps_input.clone();
    }

    fn edit(&mut self, mutate: impl FnOnce(&mut String)) {
        match self.field {
            ConfigField::FuseSize => mutate(&mut self.fuse_input),
            ConfigField::AllowControl => mutate(&mut self.allow_input),
            ConfigField::ModelTier => mutate(&mut self.tier_input),
            ConfigField::CostTier => mutate(&mut self.cost_input),
            ConfigField::Capabilities => mutate(&mut self.caps_input),
        }
        self.error = None;
    }
}

#[cfg(not(windows))]
const HOUSE_RULES_QUIET: Duration = Duration::from_secs(2);
#[cfg(windows)]
const HOUSE_RULES_QUIET: Duration = Duration::from_secs(5);
// ponytail: `headroom wrap` adds a spawn-then-exec hop with its own quiet
// startup gap before the real guest CLI ever draws a prompt; a flat time-based
// grace absorbs it. Upgrade path if it still misfires: per-guest content
// detection like `opencode_input_ready`, keyed off the actual spawned program.
const HEADROOM_STARTUP_GRACE: Duration = Duration::from_secs(3);
// ponytail: some guests never satisfy their own readiness heuristic — a
// persistently-redrawing TUI (a Codex spinner/status line) never goes quiet,
// and a narrow-pane prompt marker can miss a live layout the heuristic
// wasn't tuned against — so without a bound the intro would never send.
// Upgrade path if this still misfires for a guest: per-guest readiness
// detection like `opencode_input_ready`, keyed off the actual program.
const INTRO_READINESS_CEILING: Duration = Duration::from_secs(15);
// How far one wheel notch scrolls the focused pane's retained history. Page
// Up / Page Down use the full visible height instead; only the wheel uses a
// small fixed step.
const WHEEL_SCROLL_STEP: usize = 3;
const PULSE_COST_REFRESH: Duration = Duration::from_secs(3);
const DETAIL_REFRESH: Duration = Duration::from_secs(2);

pub(crate) struct RoomDetailIdentity {
    pub index: usize,
    pub guest: String,
    pub cwd: std::path::PathBuf,
    pub title: String,
}

pub(crate) struct DetailScheduleCtx<'a> {
    pub now: Instant,
    pub last_refresh: &'a mut Option<Instant>,
    pub pending: &'a mut bool,
    pub tx: std::sync::mpsc::Sender<(usize, RoomDetail)>,
}

pub(crate) fn schedule_detail_collection(
    identity: RoomDetailIdentity,
    ctx: DetailScheduleCtx<'_>,
    collect: impl FnOnce(String, std::path::PathBuf, String) -> RoomDetail + Send + 'static,
) -> bool {
    if identity.guest == "claude" {
        return false;
    }
    let should_refresh = ctx
        .last_refresh
        .as_ref()
        .is_none_or(|last| ctx.now.duration_since(*last) >= DETAIL_REFRESH);
    if !should_refresh || *ctx.pending {
        return false;
    }
    *ctx.last_refresh = Some(ctx.now);
    *ctx.pending = true;
    let tx_clone = ctx.tx.clone();
    let idx = identity.index;
    std::thread::spawn(move || {
        let detail = collect(identity.guest, identity.cwd, identity.title);
        let _ = tx_clone.send((idx, detail));
    });
    true
}

pub(crate) struct CostIdentity {
    pub index: usize,
    pub guest: String,
    pub cwd: std::path::PathBuf,
    pub title: String,
    pub pricing: Vec<TokenPricing>,
}

pub(crate) struct CostScheduleCtx<'a> {
    pub now: Instant,
    pub cache: &'a mut Option<(Instant, String)>,
    pub pending: &'a mut bool,
    pub tx: std::sync::mpsc::Sender<(usize, (Instant, String))>,
}

pub(crate) fn schedule_cost_collection(
    identity: CostIdentity,
    ctx: CostScheduleCtx<'_>,
    collect: impl FnOnce(String, std::path::PathBuf, String, Vec<TokenPricing>) -> String
    + Send
    + 'static,
) -> bool {
    let should_refresh = ctx
        .cache
        .as_ref()
        .is_none_or(|(last, _)| ctx.now.saturating_duration_since(*last) >= PULSE_COST_REFRESH);
    if !should_refresh || *ctx.pending {
        return false;
    }
    *ctx.pending = true;
    let tx_clone = ctx.tx.clone();
    let idx = identity.index;
    let now = ctx.now;
    std::thread::spawn(move || {
        let cost = collect(
            identity.guest,
            identity.cwd,
            identity.title,
            identity.pricing,
        );
        let _ = tx_clone.send((idx, (now, cost)));
    });
    true
}
// Button-event reporting (`?1000h`) plus SGR encoding (`?1006h`), and
// deliberately not `?1002h`/`?1003h`. Crossterm's `EnableMouseCapture` turns
// drag and any-motion reporting on as well; Crowded reads neither, but the
// parent terminal still delivers a motion report for every pointer movement,
// which competes with the wheel reports we do care about.
const ENABLE_WHEEL_REPORTING: &[u8] = b"\x1b[?1000h\x1b[?1006h";
const DISABLE_WHEEL_REPORTING: &[u8] = b"\x1b[?1006l\x1b[?1000l";

// Upper bound on reports discarded per gesture, so a terminal that never stops
// reporting cannot starve rendering.
const WHEEL_DRAIN_CEILING: usize = 1024;

fn is_wheel(event: &Event) -> Option<MouseEventKind> {
    match event {
        Event::Mouse(mouse)
            if matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) =>
        {
            Some(mouse.kind)
        }
        _ => None,
    }
}

/// Collapse an already-queued burst of identical wheel reports into the single
/// report the caller is about to act on.
///
/// A physical wheel notch is not one report. Warp resends the same SGR event
/// well over a hundred times per notch, and terminals that repeat less still
/// repeat. Acting on each one individually both overshoots the scroll target by
/// two orders of magnitude and lets the queue outgrow the rate the render loop
/// can drain it, at which point the wheel stops responding entirely.
///
/// Only reports already waiting are consumed, so a terminal that sends exactly
/// one report per notch is unaffected. Draining stops at the first event that is
/// not the same wheel direction; that event is handed back through `stashed` so
/// the next iteration sees it rather than dropping it.
fn drain_wheel_burst(kind: MouseEventKind, stashed: &mut VecDeque<Event>) -> io::Result<usize> {
    let mut drained = 0;
    while drained < WHEEL_DRAIN_CEILING {
        if !event::poll(Duration::ZERO)? {
            break;
        }
        let event = event::read()?;
        if is_wheel(&event) != Some(kind) {
            stashed.push_back(event);
            break;
        }
        drained += 1;
    }
    Ok(drained)
}

// Longest parameter run held before deciding this is not a report after all.
// An SGR report is `<button;column;row`, so three numbers and two semicolons.
const REPORT_HELD_CEILING: usize = 20;

/// Suppresses an SGR mouse report that crossterm tore in half.
///
/// Crossterm decides a lone escape byte is the Esc key whenever the terminal
/// read that delivered it stopped short of filling crossterm's buffer. A wheel
/// report split across two reads therefore arrives as Esc followed by its own
/// text as ordinary characters, and forwarding those to the focused guest types
/// `[<65;176;43M` into its prompt. Crowded asked the parent terminal for these
/// reports, so they are never something the user pressed. Warp sends well over a
/// hundred per notch, which is what makes a read land on that boundary often
/// enough to see.
///
/// Keys are held only while the run still matches the report grammar. Anything
/// that breaks it is handed back in the order it was typed, so a user who
/// presses Esc and then types loses nothing. Esc itself is never held: it is
/// too useful to delay, and a stray one is invisible where the characters are
/// not.
#[derive(Default)]
struct TornMouseReport {
    after_escape: bool,
    held: Vec<KeyEvent>,
}

impl TornMouseReport {
    /// The keys to act on now. Empty while a possible report is in flight; the
    /// whole held run, oldest first, once it turns out not to be one.
    fn filter(&mut self, key: KeyEvent) -> Vec<KeyEvent> {
        if key.kind != KeyEventKind::Press {
            return vec![key];
        }
        if key.code == KeyCode::Esc {
            self.after_escape = true;
            let mut flushed = std::mem::take(&mut self.held);
            flushed.push(key);
            return flushed;
        }
        let character = match key.code {
            KeyCode::Char(character) => character,
            _ => {
                self.after_escape = false;
                let mut flushed = std::mem::take(&mut self.held);
                flushed.push(key);
                return flushed;
            }
        };
        // `M` and `m` end a report; the bound keeps a run that never terminates
        // from holding keys indefinitely.
        let continues = match self.held.len() {
            0 => self.after_escape && character == '[',
            1 => character == '<',
            2..=REPORT_HELD_CEILING => character.is_ascii_digit() || character == ';',
            _ => false,
        };
        if self.held.len() >= 2 && matches!(character, 'M' | 'm') {
            self.after_escape = false;
            self.held.clear();
            return Vec::new();
        }
        if !continues {
            self.after_escape = false;
            let mut flushed = std::mem::take(&mut self.held);
            flushed.push(key);
            return flushed;
        }
        self.held.push(key);
        Vec::new()
    }
}

// A transient hook state (starting/thinking/working) is only trusted while
// fresh. Once its sample is older than this and the delivery gate
// demonstrably shows the screen ready, the ready screen wins: a hook that
// stopped reporting must not pin the roster to a stale self-report forever.
const PULSE_FRESHNESS_WINDOW: Duration = Duration::from_secs(30);

struct DeliveryFuse {
    used: usize,
    limit: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryGate {
    AwaitingIntro,
    IntroSent,
    IntroRunning,
    Ready,
    MessageSent,
    MessageRunning,
}

impl DeliveryGate {
    fn new(needs_intro: bool) -> Self {
        if needs_intro {
            Self::AwaitingIntro
        } else {
            Self::Ready
        }
    }

    fn observe(&mut self, input_ready: bool) {
        *self = match (*self, input_ready) {
            (Self::IntroSent, false) => Self::IntroRunning,
            (Self::IntroRunning, true) => Self::Ready,
            (Self::MessageSent, false) => Self::MessageRunning,
            (Self::MessageRunning, true) => Self::Ready,
            (state, _) => state,
        };
    }

    /// True once this room may receive its one-time house-rules intro:
    /// either its own readiness heuristic reports ready, or `waited` has
    /// crossed `ceiling`. The ceiling is the shared fallback for every
    /// guest whose heuristic can get stuck (see `INTRO_READINESS_CEILING`);
    /// it only unblocks the intro, never `can_deliver`, so a genuinely busy
    /// guest still isn't interrupted for later messages.
    fn can_send_intro(self, input_ready: bool, waited: Duration, ceiling: Duration) -> bool {
        self == Self::AwaitingIntro && (input_ready || waited >= ceiling)
    }

    fn intro_sent(&mut self) {
        *self = Self::IntroSent;
    }

    fn can_deliver(self, input_ready: bool) -> bool {
        self == Self::Ready && input_ready
    }

    fn message_sent(&mut self) {
        *self = Self::MessageSent;
    }

    fn is_starting(self) -> bool {
        matches!(
            self,
            Self::AwaitingIntro | Self::IntroSent | Self::IntroRunning
        )
    }
}

/// The delivery gate for a pane that just respawned. A genuinely resumed
/// pane skips the intro whisper; every other respawn path (fresh spawn,
/// restart, ClearContext, Configure) resends it exactly as before.
/// A room that resumed a conversation carries its own history and needs no
/// intro. A resume that could not be honoured leaves the room starting fresh,
/// which is indistinguishable from any other fresh room and takes the intro.
fn gate_after_control(needs_intro: bool, resumed: bool) -> DeliveryGate {
    DeliveryGate::new(needs_intro && !resumed)
}

/// Whether this key event requests a restart of the focused pane. F5 forces
/// a restart even while the child is online; Ctrl+R keeps its long-standing
/// meaning and restarts only an offline pane, because on a live child the
/// same chord must keep forwarding a literal 0x12 byte (e.g. shell reverse
/// search). Extracted from the event-loop arm so the online/offline x
/// key-chord matrix is unit-testable without a terminal.
fn force_restart_requested(key: KeyEvent, online: bool) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }
    if key.code == KeyCode::F(5) {
        return true;
    }
    key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL && !online
}

impl DeliveryFuse {
    /// Create a new delivery fuse. A limit of 0 means unlimited (never trips).
    fn new(limit: usize) -> Self {
        Self { used: 0, limit }
    }

    fn record(&mut self) {
        if self.limit == 0 {
            return;
        }
        self.used = self.used.saturating_add(1).min(self.limit);
    }

    fn remaining(&self) -> usize {
        if self.limit == 0 {
            return 0;
        }
        self.limit - self.used
    }

    /// Returns true when the fuse has tripped. A limit of 0 means unlimited
    /// and never returns true.
    fn is_tripped(&self) -> bool {
        self.limit != 0 && self.used >= self.limit
    }

    fn reset(&mut self) {
        self.used = 0;
    }

    fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        if self.limit != 0 && self.used > self.limit {
            self.used = self.limit;
        }
    }
}

/// How long a pane's output must stay quiet before automation treats it as
/// idle-and-ready. Headroom-wrapped panes get extra grace to absorb the
/// spawn-then-exec hop's own quiet gap (see `HEADROOM_STARTUP_GRACE`).
fn quiet_threshold(headroom_active: bool) -> Duration {
    if headroom_active {
        HOUSE_RULES_QUIET + HEADROOM_STARTUP_GRACE
    } else {
        HOUSE_RULES_QUIET
    }
}

/// The readiness fallback that unblocks a stuck room's intro. Headroom
/// rooms get no fixed ceiling: the wrapper's own bootstrap pause is real
/// startup, not a stuck heuristic, so a fixed timeout must never paste the
/// intro into a wrapper that is still launching its guest. Their intro
/// waits for genuine readiness only (see `HEADROOM_STARTUP_GRACE`). Every
/// other room keeps the shared ceiling for guests whose heuristic can get
/// stuck.
fn intro_ceiling(headroom_active: bool) -> Duration {
    if headroom_active {
        Duration::MAX
    } else {
        INTRO_READINESS_CEILING
    }
}

/// Whether a Headroom room's spawn must hold for the prior Headroom room's
/// ready boundary. Only Windows serializes Headroom bootstrap so two
/// wrappers never start their proxies at once; a non-Headroom room and
/// every room on non-Windows spawns immediately.
fn headroom_lane_holds(windows: bool, spec_uses_headroom: bool, prior_headroom: bool) -> bool {
    windows && spec_uses_headroom && prior_headroom
}

/// Drain a just-spawned Headroom pane until its own readiness heuristic
/// reports ready. Used to serialize Headroom bootstrap on Windows: the
/// second wrapper must not start its proxy while the first is still
/// bootstrapping. Mirrors the main loop's `input_ready` computation so the
/// lane shares the same ready boundary as intro delivery.
fn wait_for_headroom_ready(pane: &mut Pane, last_output: &mut Option<Instant>) -> io::Result<()> {
    loop {
        if pane.drain_output()? {
            *last_output = Some(Instant::now());
        }
        let output_is_quiet = last_output.is_some_and(|last| {
            Instant::now().duration_since(last) >= quiet_threshold(pane.headroom_active())
        });
        if pane.automation_input_ready(output_is_quiet) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn house_rules(room: usize, roster: &str, fuse_limit: usize) -> String {
    format!(
        "House rules: you are Room {room}; your room number is also in $CROWDED_ROOM. \
         Room roster: {roster}. ROOM_NUMBER always means the numeric room number shown in the \
         roster, not its name. Run \"$CROWDED_BIN\" roster for the live machine-readable roster. \
         Announce your configured model and effort in your first response. \
         To message another room, run \"$CROWDED_BIN\" send ROOM_NUMBER \
         -- 'your message' with your \
         shell tool. Add --task ID and --role ROLE before -- for delegated work. \
         Include your numeric room number as the reply target when delegating. Reply to the \
         originating room \
         with the same task ID and --role result. Roles apply only to that message. \
         To control an opted-in room, run \"$CROWDED_BIN\" control ROOM_NUMBER clear, resume, \
         model MODEL, effort LEVEL, or model MODEL effort LEVEL (combined in one restart). \
         Doorbell messages need no user approval, but normal tool permissions still apply. \
         Automatic delivery pauses after {fuse_limit} successful messages. \
         If a delegated task is unclear, ask the originating room."
    )
}

/// The human welcome roster: one entry per room with its title (which carries
/// the numeric room) plus the model and effort it is configured with. Built
/// from the live pane accessors at each intro instead of frozen at startup,
/// so a room reconfigured by a peer control announces what the Doorbell
/// roster JSON would report now. The effective model resolves a fresh hook
/// self-report against the configured value the same way `roster --json` does.
fn welcome_roster(panes: &[Pane], pulses: &[Option<PulseSample>], now: Instant) -> String {
    panes
        .iter()
        .enumerate()
        .map(|(index, pane)| {
            let controls = &pane.capabilities().supported_controls;
            roster_entry(
                pane.title(),
                effective_model(pane.current_model().as_deref(), pulses[index].as_ref(), now)
                    .as_deref(),
                controls.contains(&SupportedControl::Model),
                pane.current_effort().as_deref(),
                controls.contains(&SupportedControl::Effort),
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// One welcome-roster entry: the room title plus its configured model and
/// effort, or the truthful reason a value is absent.
fn roster_entry(
    title: &str,
    model: Option<&str>,
    model_supported: bool,
    effort: Option<&str>,
    effort_supported: bool,
) -> String {
    format!(
        "{title} (model {}, effort {})",
        configured_value(model, model_supported),
        configured_value(effort, effort_supported),
    )
}

/// A configured control value, or why there is none: "unconfigured" when the
/// adapter accepts the control but no value was set, "unsupported" when it
/// cannot accept one (a terminal room, or effort on a guest with no stable
/// effort launch option).
fn configured_value(value: Option<&str>, supported: bool) -> &str {
    match (value, supported) {
        (Some(value), _) => value,
        (None, true) => "unconfigured",
        (None, false) => "unsupported",
    }
}

fn message_with_hat(task: Option<&str>, role: Option<&str>, body: &str) -> String {
    match (task, role) {
        (None, None) => body.to_owned(),
        (Some(task), None) => format!("[task: {task}]\n{body}"),
        (None, Some(role)) => format!("[requested role: {role}]\n{body}"),
        (Some(task), Some(role)) => {
            format!("[task: {task} | requested role: {role}]\n{body}")
        }
    }
}

/// A self-reported hook pulse timestamped at Doorbell receipt, so the roster
/// resolver can tell a live transient state from a stale one. Carries the
/// optional model the hook reports it is running, which shares the same
/// freshness window as the state it arrived with.
#[derive(Clone)]
struct PulseSample {
    state: PulseState,
    model: Option<String>,
    received_at: Instant,
}

impl PulseSample {
    fn now(state: PulseState, model: Option<String>) -> Self {
        Self {
            state,
            model,
            received_at: Instant::now(),
        }
    }

    fn is_stale(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.received_at) > PULSE_FRESHNESS_WINDOW
    }
}

/// Apply one pulse to a room's sample and detail accumulators. A pulse whose
/// detail payload is non-empty is detail-only: it must not overwrite the
/// sample's state or `received_at`, must not synthesize a sample when none
/// exists, and only refreshes the sample's model in place. A pulse without
/// detail is a normal heartbeat and replaces the sample.
fn apply_pulse_to_room(
    room_pulses: &mut [Option<PulseSample>],
    room_details: &mut [RoomDetail],
    index: usize,
    state: PulseState,
    model: Option<String>,
    detail: Option<Vec<DetailEvent>>,
) {
    let has_detail = detail.as_ref().is_some_and(|events| !events.is_empty());
    if !has_detail {
        room_pulses[index] = Some(PulseSample::now(state, model.clone()));
    }
    if let Some(events) = detail {
        for event in events {
            apply_detail_event(&mut room_details[index], event);
        }
    }
    if has_detail
        && let Some(model) = model
        && let Some(sample) = &mut room_pulses[index]
    {
        sample.model = Some(model);
    }
}

/// The model a room is effectively running. The operator `control model` is an
/// explicit override and always wins; otherwise a fresh hook self-report of
/// model (one that arrived inside the pulse freshness window) fills the gap
/// when the operator left the model unconfigured. A stale or absent self-report
/// leaves the configured value untouched, so guests that never send a model
/// keep reporting null/unconfigured.
fn effective_model(
    configured: Option<&str>,
    pulse: Option<&PulseSample>,
    now: Instant,
) -> Option<String> {
    match configured {
        Some(model) => Some(model.to_owned()),
        None => pulse
            .filter(|sample| !sample.is_stale(now))
            .and_then(|sample| sample.model.clone()),
    }
}

/// The resolved pulse for one room: the state the TUI and the JSON roster
/// both show, plus the source that produced it, so the two surfaces cannot
/// drift apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedPulse {
    state: PulseState,
    source: PulseSource,
}

fn roster_state(
    online: bool,
    gate: DeliveryGate,
    input_ready: bool,
    pulse: Option<PulseSample>,
    now: Instant,
) -> ResolvedPulse {
    if !online {
        return ResolvedPulse {
            state: PulseState::Offline,
            source: PulseSource::Offline,
        };
    }
    let deliverable = gate.can_deliver(input_ready);
    let Some(sample) = pulse else {
        let state = if deliverable {
            PulseState::Ready
        } else if gate.is_starting() {
            PulseState::Starting
        } else {
            PulseState::Working
        };
        return ResolvedPulse {
            state,
            source: PulseSource::Gate,
        };
    };
    match sample.state {
        // Terminal self-reports stay authoritative.
        PulseState::Error | PulseState::Offline => ResolvedPulse {
            state: sample.state,
            source: PulseSource::Hook,
        },
        // A stale transient hook state (the hook stopped reporting) must not
        // override a screen the delivery gate demonstrably shows ready.
        PulseState::Starting | PulseState::Thinking | PulseState::Working
            if deliverable && sample.is_stale(now) =>
        {
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Readiness,
            }
        }
        // A genuinely starting room keeps its own self-report until the gate
        // independently confirms it can receive messages; a legitimate
        // startup never reaches Ready (gate.is_starting() implies
        // !can_deliver), so this only overrides the stale case.
        PulseState::Starting if !deliverable => ResolvedPulse {
            state: PulseState::Starting,
            source: PulseSource::Hook,
        },
        // Fresh transient self-reports keep their existing priority.
        PulseState::Thinking | PulseState::Working => ResolvedPulse {
            state: sample.state,
            source: PulseSource::Hook,
        },
        // A fresh "ready" self-report agrees with the gate.
        PulseState::Ready if deliverable => ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Hook,
        },
        // A "ready" self-report that contradicts the gate falls back to the
        // gate/screen inference, exactly as before.
        PulseState::Ready if gate.is_starting() => ResolvedPulse {
            state: PulseState::Starting,
            source: PulseSource::Gate,
        },
        PulseState::Ready => ResolvedPulse {
            state: PulseState::Working,
            source: PulseSource::Gate,
        },
        // Fresh "starting" with a deliverable gate: readiness won over the
        // self-report (the resumed-room case).
        PulseState::Starting => ResolvedPulse {
            state: PulseState::Ready,
            source: PulseSource::Readiness,
        },
    }
}

/// The Room Pulse panel's label for one room. The self-reported `pulse`
/// alone is not enough: a resumed room skips the intro whisper, so no Stop
/// hook ever follows its SessionStart "starting" self-report to correct it.
/// Routing through `roster_state` cross-checks the delivery gate and live
/// `input_ready` reading the same way `crowded roster --json` already does,
/// so the panel and the JSON roster agree on both the state and its source.
fn pulse_label(
    pane: &Pane,
    gate: DeliveryGate,
    input_ready: bool,
    pulse: Option<PulseSample>,
    now: Instant,
) -> String {
    if !pane.is_online() {
        "offline".to_owned()
    } else if !pane.needs_intro() {
        "terminal".to_owned()
    } else {
        let resolved = roster_state(true, gate, input_ready, pulse.clone(), now);
        let hook_age = pulse
            .filter(|_| resolved.source == PulseSource::Hook)
            .map(|sample| now.saturating_duration_since(sample.received_at));
        resolved_label(resolved, hook_age)
    }
}

/// The Room Pulse entry style for one room: the same cyan cue as the
/// focused pane's border, so the panel and the room grid agree on focus.
fn pulse_entry_style(index: usize, focused: usize) -> Style {
    if index == focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

/// The visible Room Pulse state word for a resolved state, plus the hook age
/// when one is known. The state source is reported separately by
/// `crowded roster --json`; this compact panel shows only the state.
fn resolved_label(resolved: ResolvedPulse, hook_age: Option<Duration>) -> String {
    match hook_age {
        Some(age) => format!("{} · {}s", resolved.state.label(), age.as_secs()),
        None => resolved.state.label().to_owned(),
    }
}

/// Compact single-letter badges for the Room Pulse title line: "H" for an
/// active headroom, "S" for a captured session, space-joined. Empty when
/// neither is active so the title line stays short for realistic room names.
fn pulse_badges(headroom: bool, session: bool) -> String {
    let mut markers = Vec::new();
    if headroom {
        markers.push("H");
    }
    if session {
        markers.push("S");
    }
    markers.join(" ")
}

/// The Room Pulse Total line: the sum of every known per-room cost. `captured`
/// marks which rooms have a captured session; a room's cost is "known" only
/// when it parses as a `$...` figure. Rooms without a captured session are
/// excluded from both K and N, and sessions with an unknown cost count toward
/// N but not K while adding nothing to the sum. Returns None when no room has
/// a captured session, so no Total line is shown.
fn pulse_total(costs: &[Option<(Instant, String)>], captured: &[bool]) -> Option<String> {
    let mut total = 0.0f64;
    let mut known = 0usize;
    let mut n = 0usize;
    for (index, &has_session) in captured.iter().enumerate() {
        if !has_session {
            continue;
        }
        n += 1;
        if let Some(Some((_, cost))) = costs.get(index)
            && let Some(parsed) = cost.strip_prefix('$').and_then(|s| s.parse::<f64>().ok())
        {
            total += parsed;
            known += 1;
        }
    }
    if n == 0 {
        return None;
    }
    Some(if known == n {
        format!("Total: ${total:.6}")
    } else {
        format!("Total ({known}/{n} known): ${total:.6}")
    })
}

/// Render the full Room Pulse line list: each room's title and state line, a
fn resolve_state_color(state_str: &str) -> Option<Color> {
    let word = state_str.split_whitespace().next()?;
    match word {
        "offline" | "error" => Some(Color::Red),
        "ready" => Some(Color::Green),
        "thinking" | "working" | "starting" => Some(Color::Yellow),
        _ => None,
    }
}

/// blank separator between rooms, and the optional Total line. `costs` holds
/// one optional cost string per room (the rendered `$...` or "unknown" figure
/// produced by the cost cache). `focused` selects the cyan-highlighted room,
/// matching `pulse_entry_style`.
fn render_pulse_lines(
    titles: &[&str],
    headrooms: &[bool],
    sessions: &[bool],
    states: &[&str],
    costs: &[Option<(Instant, String)>],
    details: &[RoomDetail],
    focused: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    for index in 0..titles.len() {
        if index > 0 {
            lines.push(Line::default());
        }
        let badges = pulse_badges(headrooms[index], sessions[index]);
        let title = if badges.is_empty() {
            titles[index].to_owned()
        } else {
            format!("{}  {badges}", titles[index])
        };
        let cost = costs
            .get(index)
            .and_then(Option::as_ref)
            .map(|(_, cost)| format!(" · {cost}"))
            .unwrap_or_default();
        let style = pulse_entry_style(index, focused);
        lines.push(Line::styled(title, style));
        let state_style =
            resolve_state_color(states[index]).map_or(Style::default(), |c| Style::default().fg(c));
        lines.push(Line::styled(
            format!("  {}{cost}", states[index]),
            state_style,
        ));
        if let Some(detail) = details.get(index) {
            const AGENT_LIMIT: usize = 2;
            const TODO_LIMIT: usize = 2;
            if !detail.sub_agents.is_empty() {
                lines.push(Line::from("  sub-agents:"));
                for agent in detail.sub_agents.iter().take(AGENT_LIMIT) {
                    let label = if agent.kind.is_empty() {
                        if agent.id.is_empty() {
                            "task".to_owned()
                        } else {
                            agent.id.clone()
                        }
                    } else {
                        agent.kind.clone()
                    };
                    let label = if label.chars().count() > 20 {
                        let t: String = label.chars().take(20).collect();
                        format!("{t}…")
                    } else {
                        label
                    };
                    lines.push(Line::from(format!("    - {} ({})", label, agent.status)));
                }
                if detail.sub_agents.len() > AGENT_LIMIT {
                    lines.push(Line::from(format!(
                        "    - +{} more",
                        detail.sub_agents.len() - AGENT_LIMIT
                    )));
                }
            }
            let pending: Vec<_> = detail
                .todos
                .iter()
                .filter(|todo| todo.status != "completed")
                .collect();
            if !pending.is_empty() {
                lines.push(Line::from("  todos:"));
                for todo in pending.iter().take(TODO_LIMIT) {
                    let content = if todo.content.chars().count() > 22 {
                        let t: String = todo.content.chars().take(22).collect();
                        format!("{t}…")
                    } else {
                        todo.content.clone()
                    };
                    lines.push(Line::from(format!("    - {} [{}]", content, todo.status)));
                }
                if pending.len() > TODO_LIMIT {
                    lines.push(Line::from(format!(
                        "    - +{} more",
                        pending.len() - TODO_LIMIT
                    )));
                }
            }
        }
    }
    if let Some(total) = pulse_total(costs, sessions) {
        lines.push(Line::styled(total, Style::default()));
    }
    lines
}

struct PendingSubmit<'a> {
    queue: &'a mut VecDeque<(Instant, usize, bool)>,
    now: Instant,
}

fn inject_ready_pending(
    pending: &mut VecDeque<(u64, usize)>,
    submit: PendingSubmit<'_>,
    mailroom: &mut Mailroom,
    panes: &mut [Pane],
    gates: &mut [DeliveryGate],
    input_ready: &[bool],
    fuse: &mut DeliveryFuse,
) -> (usize, usize) {
    let mut injected = 0;
    let mut failed = 0;
    let candidates = pending.len();
    for _ in 0..candidates {
        if fuse.is_tripped() {
            break;
        }
        let Some((id, target)) = pending.pop_front() else {
            break;
        };
        if !gates[target].can_deliver(input_ready[target]) {
            pending.push_back((id, target));
            continue;
        }
        match mailroom.inject(id, &mut panes[target]) {
            Ok(()) => {
                gates[target].message_sent();
                submit.queue.push_back((submit.now, target, false));
                fuse.record();
                injected += 1;
            }
            Err(_) => failed += 1,
        }
    }
    (injected, failed)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmitDisposition {
    /// Keep carrying the record; no action this pass.
    Wait,
    /// The whisper was consumed; retire the record without resending.
    Retire,
    /// The whisper is stalled in the composer; send one submit and retire.
    Resend,
}

fn resend_whisper_submit_due(
    resendable: bool,
    saw_busy: bool,
    input_ready: bool,
    waited: Duration,
) -> SubmitDisposition {
    if !resendable {
        // Shell rooms submit inside the command itself, so idle means done;
        // cap the tracking window so a long-running command cannot carry an
        // entry forever.
        return if input_ready || waited >= INTRO_READINESS_CEILING {
            SubmitDisposition::Retire
        } else {
            SubmitDisposition::Wait
        };
    }
    if saw_busy && input_ready {
        // The pane was busy (consuming the whisper) and is idle again.
        return SubmitDisposition::Retire;
    }
    if waited >= INTRO_READINESS_CEILING {
        // A pane that was never busy reads as ready even while a paste sits
        // unsubmitted in its composer, so past the ceiling the only honest
        // conclusion is a stalled paste; resubmit it exactly once.
        return if saw_busy {
            SubmitDisposition::Retire
        } else {
            SubmitDisposition::Resend
        };
    }
    SubmitDisposition::Wait
}

fn resend_whisper_submits(
    pending: &mut VecDeque<(Instant, usize, bool)>,
    panes: &mut [Pane],
    input_ready: &[bool],
    now: Instant,
) -> io::Result<()> {
    for _ in 0..pending.len() {
        let Some((injected_at, target, saw_busy)) = pending.pop_front() else {
            break;
        };
        let ready = input_ready[target];
        let resendable = panes[target].transport() == "raw";
        match resend_whisper_submit_due(
            resendable,
            saw_busy,
            ready,
            now.duration_since(injected_at),
        ) {
            SubmitDisposition::Wait => {
                pending.push_back((injected_at, target, saw_busy || !ready));
            }
            SubmitDisposition::Retire => {}
            SubmitDisposition::Resend => {
                panes[target].resend_whisper_submit()?;
            }
        }
    }
    Ok(())
}

fn pane_size(outer: Rect) -> PtySize {
    // The border consumes one cell on every side. `saturating_sub` stops tiny
    // terminal sizes from wrapping below zero, and the PTY itself stays >= 1×1.
    PtySize {
        rows: outer.height.saturating_sub(2).max(1),
        cols: outer.width.saturating_sub(2).max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn pane_has_parser_viewport(outer: Rect) -> bool {
    // vt100 needs at least a 2×2 inner area to wrap output safely.
    outer.width > 3 && outer.height > 3
}

fn pane_areas(area: Rect, room_count: usize) -> Vec<Rect> {
    if room_count == 0 {
        return Vec::new();
    }

    // Use the smallest roughly-square grid that fits every room. A partial
    // final row expands to use the space instead of leaving an empty pane.
    let columns = (1..=room_count)
        .find(|columns| columns.saturating_mul(*columns) >= room_count)
        .unwrap_or(room_count);
    let rows = room_count.div_ceil(columns);
    let row_areas = Layout::vertical(vec![Constraint::Fill(1); rows]).split(area);
    let mut areas = Vec::with_capacity(room_count);

    for (row, row_area) in row_areas.iter().enumerate() {
        let rooms_in_row = (room_count - row * columns).min(columns);
        areas.extend(
            Layout::horizontal(vec![Constraint::Fill(1); rooms_in_row])
                .split(*row_area)
                .iter()
                .copied(),
        );
    }
    areas
}

fn content_areas(area: Rect) -> (Rect, Rect, Rect) {
    let [body, status] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let [rooms, pulse] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(36)]).areas(body);
    (rooms, pulse, status)
}

fn mail_popup_area(area: Rect) -> Rect {
    let [_, middle, _] = Layout::vertical([
        Constraint::Percentage(15),
        Constraint::Percentage(70),
        Constraint::Percentage(15),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Percentage(10),
        Constraint::Percentage(80),
        Constraint::Percentage(10),
    ])
    .areas(middle);
    popup
}

fn config_popup_area(area: Rect) -> Rect {
    let [_, middle, _] = Layout::vertical([
        Constraint::Percentage(20),
        Constraint::Percentage(60),
        Constraint::Percentage(20),
    ])
    .areas(area);
    let [_, popup, _] = Layout::horizontal([
        Constraint::Percentage(15),
        Constraint::Percentage(70),
        Constraint::Percentage(15),
    ])
    .areas(middle);
    popup
}

/// Validate and persist the config overlay's edited fields for the selected
/// room. Changed fields are validated first so any invalid value leaves the
/// file untouched; nothing is written when no field changed. Returns `true`
/// when the overlay should close.
fn save_config_state(
    state: &mut ConfigOverlayState,
    panes: &mut [Pane],
    fuse: &mut DeliveryFuse,
    fuse_size: &mut usize,
    notice: &mut Option<String>,
) -> bool {
    let new_size = match parse_fuse_size_input(&state.fuse_input) {
        Ok(value) => value,
        Err(msg) => {
            state.error = Some(msg);
            return false;
        }
    };
    let allow_changed = state.allow_input.trim() != state.allow_original;
    let tier_changed = state.tier_input.trim() != state.tier_original;
    let cost_changed = state.cost_input.trim() != state.cost_original;
    let caps_changed = state.caps_input.trim() != state.caps_original;

    let allow = if allow_changed {
        match parse_allow_control_input(&state.allow_input) {
            Ok(value) => Some(value),
            Err(msg) => {
                state.error = Some(msg);
                return false;
            }
        }
    } else {
        None
    };
    let tier = if tier_changed {
        match parse_model_tier_input(&state.tier_input) {
            Ok(value) => Some(value),
            Err(msg) => {
                state.error = Some(msg);
                return false;
            }
        }
    } else {
        None
    };
    let cost = if cost_changed {
        match parse_cost_tier_input(&state.cost_input) {
            Ok(value) => Some(value),
            Err(msg) => {
                state.error = Some(msg);
                return false;
            }
        }
    } else {
        None
    };
    let caps = if caps_changed {
        match parse_capabilities_input(&state.caps_input) {
            Ok(value) => Some(value),
            Err(msg) => {
                state.error = Some(msg);
                return false;
            }
        }
    } else {
        None
    };

    let updates = RoomFieldUpdate {
        allow_control: allow,
        model_tier: tier.clone(),
        cost_tier: cost.clone(),
        capabilities: caps.clone(),
    };
    let fuse_changed = new_size != state.fuse_original;

    let path = crowded_toml_path();
    if fuse_changed {
        if let Err(error) = persist_fuse_size(&path, new_size) {
            state.error = Some(error.to_string());
            return false;
        }
        fuse.set_limit(new_size);
        *fuse_size = new_size;
    }
    if !updates.is_empty() {
        if let Err(error) = persist_room_fields(&path, state.selected, &updates) {
            state.error = Some(error.to_string());
            return false;
        }
        if let Some(value) = updates.allow_control {
            panes[state.selected].set_allow_control(value);
        }
        if tier.is_some() || cost.is_some() || caps.is_some() {
            let existing = panes[state.selected].scheduling().unwrap_or_default();
            panes[state.selected].set_scheduling(RoomScheduling {
                model_tier: tier.or(existing.model_tier),
                cost_tier: cost.or(existing.cost_tier),
                capabilities: caps.unwrap_or(existing.capabilities),
            });
        }
    }
    *notice = Some(if !fuse_changed && updates.is_empty() {
        "no config changes to save".to_owned()
    } else {
        format!("room {} config saved", state.selected + 1)
    });
    true
}

// This guard owns the changes we make to the *parent* terminal. Rust calls
// `Drop::drop` automatically when the value leaves scope, including after `?`.
struct TerminalGuard {
    raw: bool,
    alternate: bool,
    cursor_hidden: bool,
    mouse_captured: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut guard = Self {
            raw: true,
            alternate: false,
            cursor_hidden: false,
            mouse_captured: false,
        };
        execute!(io::stdout(), EnterAlternateScreen)?;
        guard.alternate = true;
        execute!(io::stdout(), Hide)?;
        guard.cursor_hidden = true;
        // Wheel scrolling needs the parent terminal to report mouse events.
        // The flag lets Drop undo this on every exit path.
        let mut stdout = io::stdout();
        stdout.write_all(ENABLE_WHEEL_REPORTING)?;
        stdout.flush()?;
        guard.mouse_captured = true;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Undo setup in reverse order. Cleanup is best-effort because Drop
        // cannot return an error, and restoring as much as possible is safest.
        if self.mouse_captured {
            let mut stdout = io::stdout();
            let _ = stdout.write_all(DISABLE_WHEEL_REPORTING);
            let _ = stdout.flush();
        }
        if self.cursor_hidden {
            let _ = execute!(io::stdout(), Show);
        }
        if self.alternate {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
        if self.raw {
            let _ = disable_raw_mode();
        }
    }
}

pub(crate) fn run() -> Result<(), Box<dyn std::error::Error>> {
    let resolved = room_specs()?;
    let resumed = vec![false; resolved.specs.len()];
    run_with(
        resolved.specs,
        resumed,
        resolved.fuse_size,
        resolved.token_pricing,
    )
}

/// The `crowded resume` entry point: same room resolution as a plain
/// launch, but every supported guest starts with its resume-most-recent
/// flag already applied.
pub(crate) fn run_resumed() -> Result<(), Box<dyn std::error::Error>> {
    let mut resolved = room_specs_resumed()?;
    let resumed = pane::resume_supported_specs(&mut resolved.specs);
    run_with(
        resolved.specs,
        resumed,
        resolved.fuse_size,
        resolved.token_pricing,
    )
}

fn run_with(
    specs: Vec<RoomSpec>,
    resumed: Vec<bool>,
    mut fuse_size: usize,
    token_pricing: Vec<TokenPricing>,
) -> Result<(), Box<dyn std::error::Error>> {
    let room_count = specs.len();
    debug_assert_eq!(resumed.len(), room_count);
    // Each room receives only its own capability token.
    let doorbell = Doorbell::start(room_count)?;
    // `?` returns early on an error. The guards below still run their Drop code.
    let _terminal_guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (rooms, _, _) = content_areas(terminal.size()?.into());
    let areas = pane_areas(rooms, room_count);
    let mut panes = Vec::with_capacity(room_count);
    // Anchors `INTRO_READINESS_CEILING`: each room's own spawn instant, not
    // a shared one, so a slow room later in this loop isn't penalized for
    // rooms spawned before it.
    let mut spawned_at = Vec::with_capacity(room_count);
    let mut last_output = vec![None::<Instant>; room_count];
    // Windows serializes Headroom bootstrap: the next Headroom room's spawn
    // waits for the prior one to reach its ready boundary, so two wrappers
    // never start their proxies at once. Non-Headroom rooms and every room
    // on non-Windows spawn immediately, exactly as before.
    let mut prior_headroom: Option<usize> = None;
    for (index, (spec, area)) in specs.into_iter().zip(areas).enumerate() {
        let headroom = pane::spec_uses_headroom(&spec);
        if headroom_lane_holds(cfg!(windows), headroom, prior_headroom.is_some()) {
            if let Some(prior) = prior_headroom {
                wait_for_headroom_ready(&mut panes[prior], &mut last_output[prior])?;
            }
        }
        panes.push(Pane::spawn(
            spec,
            pane_size(area),
            doorbell.guest_environment(index)?,
        )?);
        spawned_at.push(Instant::now());
        if headroom {
            prior_headroom = Some(index);
        }
    }
    let mut delivery_gates = panes
        .iter()
        .enumerate()
        .map(|(index, pane)| DeliveryGate::new(pane.needs_intro() && !resumed[index]))
        .collect::<Vec<_>>();
    let mut focused = 0;
    let mut input_mode = InputMode::Normal;
    let mut notice: Option<String> = None;
    let mut mailroom = Mailroom::new(100);
    let mut fuse = DeliveryFuse::new(fuse_size);
    let mut delivery_paused = false;
    let mut pending = VecDeque::<(u64, usize)>::new();
    let mut submit_pending = VecDeque::<(Instant, usize, bool)>::new();
    let mut room_pulses = vec![None::<PulseSample>; room_count];
    let mut room_details = vec![RoomDetail::default(); room_count];
    let mut detail_last_refresh = vec![None::<Instant>; room_count];
    let mut pulse_costs = vec![None::<(Instant, String)>; room_count];
    // Pulse cost and room detail collection do blocking filesystem / sqlite
    // work (opencode sqlite, codex rollout walks, claude transcript reads).
    // Running them synchronously on the main thread blocks input handling for
    // the duration of the I/O (empirically ~20s when detail I/O stalls), so
    // both are offloaded to background threads and the main loop only polls
    // their results via non-blocking `try_recv`.
    let (detail_tx, detail_rx) = mpsc::channel::<(usize, RoomDetail)>();
    let (cost_tx, cost_rx) = mpsc::channel::<(usize, (Instant, String))>();
    let mut detail_pending = vec![false; room_count];
    let mut cost_pending = vec![false; room_count];
    // Holds the event that ended a wheel burst, so draining never discards it,
    // and any key run handed back by the torn-report filter.
    let mut stashed_events: VecDeque<Event> = VecDeque::new();
    let mut torn_report = TornMouseReport::default();

    loop {
        let now = Instant::now();
        for (index, pane) in panes.iter_mut().enumerate() {
            if pane.drain_output()? {
                last_output[index] = Some(now);
            }
        }
        let input_ready: Vec<_> = panes
            .iter()
            .enumerate()
            .map(|(index, pane)| {
                let output_is_quiet = last_output[index].is_some_and(|last| {
                    now.duration_since(last) >= quiet_threshold(pane.headroom_active())
                });
                pane.automation_input_ready(output_is_quiet)
            })
            .collect();
        resend_whisper_submits(&mut submit_pending, &mut panes, &input_ready, now)?;
        for index in 0..room_count {
            delivery_gates[index].observe(input_ready[index]);
            let waited = now.duration_since(spawned_at[index]);
            if delivery_gates[index].can_send_intro(
                input_ready[index],
                waited,
                intro_ceiling(panes[index].headroom_active()),
            ) {
                let roster = welcome_roster(&panes, &room_pulses, now);
                match panes[index].send_whisper(
                    "The Crowded Room",
                    &house_rules(index + 1, &roster, fuse_size),
                ) {
                    Ok(()) => {
                        delivery_gates[index].intro_sent();
                        panes[index].begin_session_capture();
                    }
                    Err(error) => {
                        // Leave the gate at `AwaitingIntro` so the next loop
                        // iteration retries; a failed write must not claim a
                        // successful intro or start session capture.
                        notice = Some(format!(
                            "Could not teach {} the house rules: {error}",
                            panes[index].title()
                        ));
                    }
                }
            }
        }
        if !delivery_paused {
            let (injected, failed) = inject_ready_pending(
                &mut pending,
                PendingSubmit {
                    queue: &mut submit_pending,
                    now,
                },
                &mut mailroom,
                &mut panes,
                &mut delivery_gates,
                &input_ready,
                &mut fuse,
            );
            if fuse.is_tripped() {
                delivery_paused = true;
                notice = Some(format!(
                    "Delivery fuse tripped after {injected} queued injections; {} remain",
                    pending.len()
                ));
            } else if injected > 0 || failed > 0 {
                notice = Some(format!(
                    "Queued delivery: {injected} injected, {failed} failed"
                ));
            }
        }
        while let Ok(event) = doorbell.try_recv() {
            let envelope = match event {
                DoorbellEvent::Roster(request) => {
                    request.reply(
                        panes
                            .iter()
                            .enumerate()
                            .map(|(index, pane)| {
                                let resolved = roster_state(
                                    pane.is_online(),
                                    delivery_gates[index],
                                    input_ready[index],
                                    room_pulses[index].clone(),
                                    now,
                                );
                                RosterRoom {
                                    room: index + 1,
                                    name: pane.name().to_owned(),
                                    guest: pane.guest(),
                                    vendor: pane.vendor().to_owned(),
                                    transport: pane.transport().to_owned(),
                                    state: resolved.state,
                                    state_source: resolved.source,
                                    allow_control: pane.allows_control(),
                                    model: effective_model(
                                        pane.current_model().as_deref(),
                                        room_pulses[index].as_ref(),
                                        now,
                                    ),
                                    effort: pane.current_effort(),
                                    cost: pane::usage_cost(
                                        &pane.guest(),
                                        pane.cwd(),
                                        pane.title(),
                                        &token_pricing,
                                    )
                                    .map(|cost| format!("${cost:.6}"))
                                    .unwrap_or_else(|| "unknown".to_owned()),
                                    headroom: pane.headroom_active(),
                                    pulse_age_ms: room_pulses[index].as_ref().map(|sample| {
                                        now.saturating_duration_since(sample.received_at)
                                            .as_millis()
                                            as u64
                                    }),
                                    capabilities: pane.capabilities(),
                                    scheduling: pane.scheduling(),
                                }
                            })
                            .collect(),
                    );
                    continue;
                }
                DoorbellEvent::Pulse(pulse) => {
                    apply_pulse_to_room(
                        &mut room_pulses,
                        &mut room_details,
                        pulse.from,
                        pulse.state,
                        pulse.model,
                        pulse.detail,
                    );
                    continue;
                }
                DoorbellEvent::Control(control) => {
                    let source = panes[control.from].title().to_owned();
                    let target = panes[control.to].title().to_owned();
                    let label = control.action.label();
                    if !panes[control.to].allows_control() {
                        control.reply_failed(format!("{target} does not allow peer control"));
                        notice = Some(format!("{source} control rejected by {target}"));
                        continue;
                    }

                    // Reports whether the room came back holding a prior
                    // conversation, which only a resume can do and only when it
                    // was actually honoured.
                    let result = (|| -> Result<bool, Box<dyn std::error::Error>> {
                        terminal.autoresize()?;
                        let (rooms, _, _) = content_areas(terminal.size()?.into());
                        let size = pane_size(pane_areas(rooms, room_count)[control.to]);
                        match &control.action {
                            ControlAction::ClearContext => {
                                panes[control.to].clear_context(size).map(|_| false)
                            }
                            ControlAction::Resume => panes[control.to].resume_context(size),
                            ControlAction::Configure { model, effort } => panes[control.to]
                                .configure(
                                    model.as_deref(),
                                    effort.as_ref().map(|e| e.label()),
                                    size,
                                )
                                .map(|_| false),
                        }
                    })();
                    match result {
                        Ok(resumed) => {
                            delivery_gates[control.to] =
                                gate_after_control(panes[control.to].needs_intro(), resumed);
                            last_output[control.to] = None;
                            spawned_at[control.to] = Instant::now();
                            room_pulses[control.to] = match &control.action {
                                ControlAction::Resume => None,
                                _ => Some(PulseSample::now(PulseState::Starting, None)),
                            };
                            control.reply_applied();
                            notice = Some(format!("{source} told {target} to {label}"));
                        }
                        Err(error) => {
                            control.reply_failed(error.to_string());
                            notice =
                                Some(format!("{source} could not {label} on {target}: {error}"));
                        }
                    }
                    continue;
                }
                DoorbellEvent::Message(envelope) => envelope,
            };
            let source = panes[envelope.from].title().to_owned();
            let target = panes[envelope.to].title().to_owned();
            let body = message_with_hat(
                envelope.task.as_deref(),
                envelope.role.as_deref(),
                &envelope.body,
            );
            if delivery_paused || !delivery_gates[envelope.to].can_deliver(input_ready[envelope.to])
            {
                if pending.len() >= 100 {
                    envelope.reply_failed("delivery queue is full");
                    continue;
                }
                let reason = if delivery_gates[envelope.to].is_starting() {
                    "room starting"
                } else if !input_ready[envelope.to] {
                    "room busy"
                } else if fuse.is_tripped() {
                    "fuse tripped"
                } else {
                    "delivery paused"
                };
                let id = mailroom.queue(source, target, body, reason);
                pending.push_back((id, envelope.to));
                envelope.reply_queued(id);
                notice = Some(format!("Envelope #{id:04} queued: {reason}"));
            } else {
                let (id, result) = mailroom.deliver(source, &mut panes[envelope.to], body);
                match result {
                    Ok(()) => {
                        envelope.reply_injected(id);
                        delivery_gates[envelope.to].message_sent();
                        submit_pending.push_back((now, envelope.to, false));
                        fuse.record();
                        if fuse.is_tripped() {
                            delivery_paused = true;
                            notice = Some(format!(
                                "Envelope #{id:04} injected; delivery fuse tripped at {}",
                                fuse.limit
                            ));
                        } else {
                            notice = Some(format!(
                                "Doorbell envelope #{id:04} injected • fuse {}/{}",
                                fuse.remaining(),
                                fuse.limit
                            ));
                        }
                    }
                    Err(error) => {
                        envelope.reply_failed(error.to_string());
                        notice = Some(format!("Envelope #{id:04} failed: {error}"));
                    }
                }
            }
        }

        // Drain any completed background results without blocking.
        while let Ok((index, (refreshed, cost))) = cost_rx.try_recv() {
            if index < room_count {
                pulse_costs[index] = Some((refreshed, cost));
                cost_pending[index] = false;
            }
        }
        while let Ok((index, detail)) = detail_rx.try_recv() {
            if index < room_count {
                room_details[index] = detail;
                detail_pending[index] = false;
            }
        }

        // Schedule pulse-cost refreshes off-thread. The in-memory
        // `has_captured_session` check stays synchronous; the expensive
        // `usage_cost` (transcript reads, rollout walks, sqlite) runs in a
        // background thread so the 16ms input poll is never stalled.
        for index in 0..room_count {
            let captured = panes[index].has_captured_session();
            if !captured {
                pulse_costs[index] = None;
                continue;
            }
            let guest = panes[index].guest().to_owned();
            let cwd = panes[index].cwd().to_path_buf();
            let title = panes[index].title().to_owned();
            let pricing = token_pricing.clone();
            schedule_cost_collection(
                CostIdentity {
                    index,
                    guest: guest.clone(),
                    cwd: cwd.clone(),
                    title: title.clone(),
                    pricing: pricing.clone(),
                },
                CostScheduleCtx {
                    now,
                    cache: &mut pulse_costs[index],
                    pending: &mut cost_pending[index],
                    tx: cost_tx.clone(),
                },
                |g, c, t, p| {
                    pane::usage_cost(&g, &c, &t, &p)
                        .map(|cost| format!("${cost:.6}"))
                        .unwrap_or_else(|| "unknown".to_owned())
                },
            );
        }
        {
            for index in 0..room_count {
                let guest = panes[index].guest().to_owned();
                let cwd = panes[index].cwd().to_path_buf();
                let title = panes[index].title().to_owned();
                schedule_detail_collection(
                    RoomDetailIdentity {
                        index,
                        guest: guest.clone(),
                        cwd: cwd.clone(),
                        title: title.clone(),
                    },
                    DetailScheduleCtx {
                        now,
                        last_refresh: &mut detail_last_refresh[index],
                        pending: &mut detail_pending[index],
                        tx: detail_tx.clone(),
                    },
                    |g, c, t| {
                        let lower = g.to_ascii_lowercase();
                        let sid = pane::session_state::lookup(&lower, &c, &t);
                        match sid {
                            Some(id) => collect_detail(&g, &c, &id).unwrap_or_default(),
                            None => RoomDetail::default(),
                        }
                    },
                );
            }
        }

        for pane in &mut panes {
            pane.apply_scroll();
        }
        terminal.draw(|frame| {
            let (rooms, pulse, status) = content_areas(frame.area());
            let areas = pane_areas(rooms, room_count);
            for (index, (pane, area)) in panes.iter().zip(areas.iter()).enumerate() {
                let border_style = if !pane.is_online() {
                    Style::default().fg(Color::Red)
                } else if index == focused {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default()
                };
                let title = if pane.is_online() {
                    format!(" {} ", pane.title())
                } else {
                    format!(" {} [OFFLINE] ", pane.title())
                };
                let block = Block::bordered().title(title).border_style(border_style);

                if pane_has_parser_viewport(*area) {
                    // PseudoTerminal paints this pane's VT screen into its block.
                    frame.render_widget(PseudoTerminal::new(pane.screen()).block(block), *area);
                } else {
                    // At absurdly small sizes, draw only the safe outer border.
                    frame.render_widget(block, *area);
                }
            }

            let titles: Vec<&str> = panes.iter().map(|pane| pane.title()).collect();
            let headrooms: Vec<bool> = panes.iter().map(|pane| pane.headroom_active()).collect();
            let sessions: Vec<bool> = panes.iter().map(|pane| pane.has_captured_session()).collect();
            let states: Vec<String> = panes
                .iter()
                .enumerate()
                .map(|(index, pane)| {
                    pulse_label(
                        pane,
                        delivery_gates[index],
                        input_ready[index],
                        room_pulses[index].clone(),
                        now,
                    )
                })
                .collect();
            let states: Vec<&str> = states.iter().map(String::as_str).collect();
            let pulse_lines = render_pulse_lines(
                &titles,
                &headrooms,
                &sessions,
                &states,
                &pulse_costs,
                &room_details,
                focused,
            );
            frame.render_widget(
                Paragraph::new(Text::from(pulse_lines))
                    .block(Block::bordered().title(" Room Pulse "))
                    .wrap(Wrap { trim: false }),
                pulse,
            );

            let status_text = match &input_mode {
                InputMode::Normal => notice.clone().unwrap_or_else(|| {
                    if fuse.is_tripped() {
                        format!(
                            " FUSE TRIPPED: {}/{} delivered • {} queued • F3: reset & resume ",
                            fuse.used,
                            fuse.limit,
                            pending.len()
                        )
                    } else if delivery_paused {
                        format!(
                            " DELIVERY PAUSED: {} queued • fuse {}/{} • F3: resume ",
                            pending.len(),
                            fuse.remaining(),
                            fuse.limit
                        )
                    } else {
                        format!(
                            " F1 config | Tab focus | ^W whisper | F2 mail | F3 pause | F4 intro | {}/{} | ^Q quit ",
                            fuse.remaining(),
                            fuse.limit
                        )
                    }
                }),
                InputMode::Composing(message) => {
                    let target = (focused + 1) % panes.len();
                    if panes[target].is_online() {
                        format!(
                            " Whisper {} → {}: {}_",
                            panes[focused].title(),
                            panes[target].title(),
                            message
                        )
                    } else {
                        format!(
                            " Whisper {} → {} [OFFLINE]: {}_  •  Esc: cancel",
                            panes[focused].title(),
                            panes[target].title(),
                            message
                        )
                    }
                }
                InputMode::MailLog => format!(
                    " Mailroom: {} envelope(s) • {} • fuse {}/{} • {} • F2 or Esc: close ",
                    mailroom.len(),
                    if delivery_paused {
                        "delivery paused"
                    } else {
                        "auto-delivery on"
                    },
                    fuse.remaining(),
                    fuse.limit,
                    doorbell.path().display()
                ),
                InputMode::Config(state) => {
                    if let Some(err) = &state.error {
                        format!(" Config room {} • Error: {err} • Enter: save • Esc: cancel ", state.selected + 1)
                    } else {
                        format!(" Config room {} • Enter: save • Esc: cancel ", state.selected + 1)
                    }
                }

            };
            let status_style = match (&input_mode, notice.is_some()) {
                (InputMode::Composing(_), _) => Style::default().fg(Color::Yellow),
                (InputMode::MailLog, _) => Style::default().fg(Color::Cyan),
                (InputMode::Config(_), _) => Style::default().fg(Color::Yellow),
                (InputMode::Normal, true) => Style::default().fg(Color::Yellow),
                (InputMode::Normal, false) => Style::default().fg(Color::DarkGray),
            };
            frame.render_widget(Paragraph::new(status_text).style(status_style), status);

            if matches!(&input_mode, InputMode::MailLog) {
                let popup = mail_popup_area(frame.area());
                frame.render_widget(Clear, popup);
                frame.render_widget(
                    Paragraph::new(mailroom.summary())
                        .block(Block::bordered().title(" Mailroom "))
                        .wrap(Wrap { trim: false }),
                    popup,
                );
            }

            if let InputMode::Config(state) = &input_mode {
                let popup = config_popup_area(frame.area());
                frame.render_widget(Clear, popup);
                let mut lines: Vec<Line> = Vec::new();
                lines.push(Line::from(" Loaded rooms from crowded.toml:"));
                for (idx, pane) in panes.iter().enumerate() {
                    let scheduling = pane.scheduling();
                    let tier = scheduling
                        .as_ref()
                        .and_then(|entry| entry.model_tier.as_deref())
                        .unwrap_or("-");
                    let cost = scheduling
                        .as_ref()
                        .and_then(|entry| entry.cost_tier.as_deref())
                        .unwrap_or("-");
                    let caps = scheduling
                        .as_ref()
                        .map(|entry| {
                            if entry.capabilities.is_empty() {
                                "-".to_owned()
                            } else {
                                entry.capabilities.join(",")
                            }
                        })
                        .unwrap_or_else(|| "-".to_owned());
                    let marker = if idx == state.selected { ">" } else { " " };
                    lines.push(Line::from(format!(
                        " {marker} {}: {}  allow={} tier={} cost={} caps={}",
                        idx + 1,
                        pane.name(),
                        pane.allows_control(),
                        tier,
                        cost,
                        caps
                    )));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(format!(
                    " Editing room {}:",
                    state.selected + 1
                )));
                let fields = [
                    (ConfigField::FuseSize, format!(" fuse_size = {}_", state.fuse_input)),
                    (
                        ConfigField::AllowControl,
                        format!(" allow_control = {}_", state.allow_input),
                    ),
                    (
                        ConfigField::ModelTier,
                        format!(" model_tier = {}_", state.tier_input),
                    ),
                    (
                        ConfigField::CostTier,
                        format!(" cost_tier = {}_", state.cost_input),
                    ),
                    (
                        ConfigField::Capabilities,
                        format!(" capabilities = {}_", state.caps_input),
                    ),
                ];
                for (field, text) in fields {
                    if field == state.field {
                        lines.push(Line::styled(
                            text,
                            Style::default().fg(Color::Cyan),
                        ));
                    } else {
                        lines.push(Line::from(text));
                    }
                }
                if let Some(err) = &state.error {
                    lines.push(Line::styled(
                        format!(" Error: {err}"),
                        Style::default().fg(Color::Red),
                    ));
                }
                lines.push(Line::from(""));
                lines.push(Line::from(
                    " Tab: room • ↑/↓: field • type: edit • Enter: save • Esc: cancel • F1: close ",
                ));
                frame.render_widget(
                    Paragraph::new(Text::from(lines))
                        .block(Block::bordered().title(" Config (F1) "))
                        .wrap(Wrap { trim: false }),
                    popup,
                );
            }


        })?;
        for pane in &mut panes {
            pane.restore_scroll();
        }

        for pane in &mut panes {
            pane.poll_exit()?;
        }

        let next_event = match stashed_events.pop_front() {
            Some(event) => Some(event),
            None if event::poll(Duration::from_millis(16))? => Some(event::read()?),
            None => None,
        };
        // A wheel report crossterm failed to keep whole arrives as keys. Act on
        // what the filter releases: the first key now, the rest next time round.
        let next_event = match next_event {
            Some(Event::Key(key)) => {
                let mut released = torn_report.filter(key).into_iter().map(Event::Key);
                let first = released.next();
                for (offset, event) in released.enumerate() {
                    stashed_events.insert(offset, event);
                }
                first
            }
            other => other,
        };
        if let Some(event) = next_event {
            // One notch, one scroll action, however many times it was reported.
            if let Some(kind) = is_wheel(&event) {
                drain_wheel_burst(kind, &mut stashed_events)?;
            }
            match event {
                Event::Key(key)
                    if key.code == KeyCode::Char('q')
                        && key.modifiers == KeyModifiers::CONTROL
                        && key.kind == KeyEventKind::Press =>
                {
                    break;
                }
                Event::Key(key)
                    if key.code == KeyCode::F(2)
                        && key.kind == KeyEventKind::Press
                        && !matches!(
                            &input_mode,
                            InputMode::Composing(_) | InputMode::Config(_)
                        ) =>
                {
                    input_mode = match input_mode {
                        InputMode::MailLog => InputMode::Normal,
                        InputMode::Normal => InputMode::MailLog,
                        InputMode::Config(_) => unreachable!(),
                        InputMode::Composing(_) => unreachable!(),
                    };
                    notice = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::F(1)
                        && key.kind == KeyEventKind::Press
                        && !matches!(&input_mode, InputMode::Composing(_)) =>
                {
                    input_mode = match input_mode {
                        InputMode::Config(_) => InputMode::Normal,
                        InputMode::Normal => InputMode::Config(Box::new(ConfigOverlayState::new(
                            &panes, 0, fuse_size,
                        ))),
                        InputMode::MailLog => InputMode::Config(Box::new(ConfigOverlayState::new(
                            &panes, 0, fuse_size,
                        ))),
                        InputMode::Composing(_) => unreachable!(),
                    };
                    notice = None;
                }
                Event::Key(key)
                    if key.code == KeyCode::F(3)
                        && key.kind == KeyEventKind::Press
                        && !matches!(
                            &input_mode,
                            InputMode::Composing(_) | InputMode::Config(_)
                        ) =>
                {
                    if delivery_paused {
                        if fuse.is_tripped() {
                            fuse.reset();
                        }
                        delivery_paused = false;
                        notice = Some(format!(
                            "Automatic delivery resumed; {} queued",
                            pending.len()
                        ));
                    } else {
                        delivery_paused = true;
                        notice = Some("Automatic delivery paused".to_owned());
                    }
                }
                Event::Key(key)
                    if key.code == KeyCode::F(4)
                        && key.kind == KeyEventKind::Press
                        && matches!(&input_mode, InputMode::Normal) =>
                {
                    let roster = welcome_roster(&panes, &room_pulses, now);
                    match panes[focused].send_whisper(
                        "The Crowded Room",
                        &house_rules(focused + 1, &roster, fuse_size),
                    ) {
                        Ok(()) => {
                            delivery_gates[focused].intro_sent();
                            panes[focused].begin_session_capture();
                            notice = Some(format!(
                                "{} reintroduced to the room",
                                panes[focused].title()
                            ));
                        }
                        Err(error) => {
                            notice = Some(format!(
                                "Could not reintroduce {}: {error}",
                                panes[focused].title()
                            ));
                        }
                    }
                }
                Event::Key(key) if force_restart_requested(key, panes[focused].is_online()) => {
                    terminal.autoresize()?;
                    let (rooms, _, _) = content_areas(terminal.size()?.into());
                    let area = pane_areas(rooms, room_count)[focused];
                    match panes[focused].restart(pane_size(area)) {
                        Ok(()) => {
                            delivery_gates[focused] =
                                DeliveryGate::new(panes[focused].needs_intro());
                            last_output[focused] = None;
                            spawned_at[focused] = Instant::now();
                            room_pulses[focused] =
                                Some(PulseSample::now(PulseState::Starting, None));
                            notice = Some(format!("{} restarted", panes[focused].title()));
                        }
                        Err(error) => {
                            notice = Some(format!(
                                "Could not restart {}: {error}",
                                panes[focused].title()
                            ));
                        }
                    }
                }

                Event::Key(key) => match &mut input_mode {
                    InputMode::Composing(message) => {
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            match key.code {
                                KeyCode::Esc if key.kind == KeyEventKind::Press => {
                                    input_mode = InputMode::Normal;
                                }
                                KeyCode::Enter if key.kind == KeyEventKind::Press => {
                                    let target = (focused + 1) % panes.len();
                                    if panes[target].is_online() {
                                        let message = std::mem::take(message);
                                        let source = panes[focused].title().to_owned();
                                        input_mode = InputMode::Normal;
                                        if delivery_paused
                                            || !delivery_gates[target]
                                                .can_deliver(input_ready[target])
                                        {
                                            let destination = panes[target].title().to_owned();
                                            let id = mailroom.queue(
                                                source,
                                                destination,
                                                message,
                                                "room not ready",
                                            );
                                            pending.push_back((id, target));
                                            notice = Some(format!("Envelope #{id:04} queued"));
                                        } else {
                                            let (id, result) = mailroom.deliver(
                                                source,
                                                &mut panes[target],
                                                message,
                                            );
                                            notice = Some(match result {
                                                Ok(()) => {
                                                    delivery_gates[target].message_sent();
                                                    submit_pending.push_back((now, target, false));
                                                    format!("Envelope #{id:04} injected")
                                                }
                                                Err(error) => {
                                                    format!("Envelope #{id:04} failed: {error}")
                                                }
                                            });
                                        }
                                    }
                                }
                                KeyCode::Backspace => {
                                    message.pop();
                                }
                                KeyCode::Char(c)
                                    if matches!(
                                        key.modifiers,
                                        KeyModifiers::NONE | KeyModifiers::SHIFT
                                    ) =>
                                {
                                    message.push(c);
                                }
                                _ => {}
                            }
                        }
                    }
                    InputMode::Normal => {
                        if key.code == KeyCode::Char('w')
                            && key.modifiers == KeyModifiers::CONTROL
                            && key.kind == KeyEventKind::Press
                        {
                            let target = (focused + 1) % panes.len();
                            if panes[target].is_online() {
                                input_mode = InputMode::Composing(String::new());
                                notice = None;
                            } else {
                                notice = Some(format!("{} is offline", panes[target].title()));
                            }
                        } else if key.code == KeyCode::Tab
                            && key.modifiers == KeyModifiers::NONE
                            && key.kind == KeyEventKind::Press
                        {
                            focused = (focused + 1) % panes.len();
                            notice = None;
                        } else if key.code == KeyCode::PageUp && key.kind == KeyEventKind::Press {
                            if panes[focused].is_alternate_screen() {
                                let _ = panes[focused].forward_page_up();
                            } else {
                                let rows = panes[focused].visible_height();
                                panes[focused].scroll_up(rows);
                            }
                        } else if key.code == KeyCode::PageDown && key.kind == KeyEventKind::Press {
                            if panes[focused].is_alternate_screen() {
                                let _ = panes[focused].forward_page_down();
                            } else {
                                let rows = panes[focused].visible_height();
                                panes[focused].scroll_down(rows);
                            }
                        } else if panes[focused].is_online() {
                            panes[focused].write_key(key)?;
                        } else {
                            notice = Some(format!(
                                "{} is offline; press Ctrl+R to revive it",
                                panes[focused].title()
                            ));
                        }
                    }
                    InputMode::MailLog => {
                        if key.code == KeyCode::Esc && key.kind == KeyEventKind::Press {
                            input_mode = InputMode::Normal;
                        }
                    }
                    InputMode::Config(state) => {
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            match key.code {
                                KeyCode::Esc if key.kind == KeyEventKind::Press => {
                                    input_mode = InputMode::Normal;
                                    notice = None;
                                }
                                KeyCode::Enter if key.kind == KeyEventKind::Press => {
                                    let saved = save_config_state(
                                        state,
                                        &mut panes,
                                        &mut fuse,
                                        &mut fuse_size,
                                        &mut notice,
                                    );
                                    if saved {
                                        input_mode = InputMode::Normal;
                                    }
                                }
                                KeyCode::Tab if key.kind == KeyEventKind::Press => {
                                    state.selected = (state.selected + 1) % panes.len();
                                    state.error = None;
                                    state.resync(&panes);
                                }
                                KeyCode::Up if key.kind == KeyEventKind::Press => {
                                    state.field = state.field.prev();
                                    state.error = None;
                                }
                                KeyCode::Down if key.kind == KeyEventKind::Press => {
                                    state.field = state.field.next();
                                    state.error = None;
                                }
                                KeyCode::Backspace => {
                                    state.edit(|input| {
                                        input.pop();
                                    });
                                }
                                KeyCode::Char(ch)
                                    if matches!(
                                        key.modifiers,
                                        KeyModifiers::NONE | KeyModifiers::SHIFT
                                    ) =>
                                {
                                    state.edit(|input| input.push(ch));
                                }
                                _ => {}
                            }
                        }
                    }
                },
                Event::Resize(_, _) => {
                    terminal.autoresize()?;
                    let (rooms, _, _) = content_areas(terminal.size()?.into());
                    let areas = pane_areas(rooms, room_count);
                    for (pane, area) in panes.iter_mut().zip(areas.iter()) {
                        pane.resize(pane_size(*area))?;
                    }
                }
                // Wheel routing depends on whether the focused pane is in
                // alternate-screen mode: primary screen scrolls parent
                // scrollback, alternate screen forwards as PageUp/PageDown.
                Event::Mouse(mouse) => {
                    if matches!(&input_mode, InputMode::Normal) {
                        if panes[focused].is_alternate_screen() {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    let _ = panes[focused].forward_wheel(true);
                                }
                                MouseEventKind::ScrollDown => {
                                    let _ = panes[focused].forward_wheel(false);
                                }
                                _ => {}
                            }
                        } else {
                            match mouse.kind {
                                MouseEventKind::ScrollUp => {
                                    panes[focused].scroll_up(WHEEL_SCROLL_STEP);
                                }
                                MouseEventKind::ScrollDown => {
                                    panes[focused].scroll_down(WHEEL_SCROLL_STEP);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Explicit cleanup makes the normal exit path clear; both guards remain
    // as fallbacks for errors and early returns.
    for pane in &mut panes {
        pane.cleanup();
    }
    terminal.show_cursor()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn released(keys: &str) -> String {
        // Feeds one key at a time, as the event loop does, and collects what the
        // filter lets through. `\x1b` stands for the bare Esc crossterm reports
        // when a read ends on the escape byte of a sequence.
        let mut filter = TornMouseReport::default();
        let mut out = String::new();
        for character in keys.chars() {
            let code = if character == '\x1b' {
                KeyCode::Esc
            } else {
                KeyCode::Char(character)
            };
            for key in filter.filter(KeyEvent::new(code, KeyModifiers::NONE)) {
                match key.code {
                    KeyCode::Esc => out.push('\x1b'),
                    KeyCode::Char(character) => out.push(character),
                    _ => unreachable!(),
                }
            }
        }
        out
    }

    #[test]
    fn a_torn_wheel_report_never_reaches_the_guest() {
        // What the user saw typed into a Codex prompt.
        assert_eq!(released("\x1b[<65;176;43M"), "\x1b");
        // Back to back, as a burst delivers them.
        assert_eq!(released("\x1b[<64;20;10M\x1b[<64;20;10M"), "\x1b\x1b");
        // A release report ends with `m` rather than `M`.
        assert_eq!(released("\x1b[<0;5;5m"), "\x1b");
    }

    #[test]
    fn typing_after_escape_survives_in_order() {
        // Nothing here is a report, so every key must come back as pressed.
        assert_eq!(released("\x1b[1] fix"), "\x1b[1] fix");
        assert_eq!(released("\x1bhello"), "\x1bhello");
        assert_eq!(released("[<65;176;43M"), "[<65;176;43M");
        assert_eq!(released("\x1b[<9x"), "\x1b[<9x");
        // A run that never terminates is released rather than held forever.
        let digits = "1".repeat(REPORT_HELD_CEILING + 2);
        assert_eq!(
            released(&format!("\x1b[<{digits}")),
            format!("\x1b[<{digits}")
        );
    }

    #[test]
    fn only_matching_wheel_directions_are_drained() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};

        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };

        let up = wheel(MouseEventKind::ScrollUp);
        let down = wheel(MouseEventKind::ScrollDown);
        assert_eq!(is_wheel(&up), Some(MouseEventKind::ScrollUp));
        assert_eq!(is_wheel(&down), Some(MouseEventKind::ScrollDown));

        // A burst only absorbs its own direction; the opposite direction and
        // every non-wheel event must end the drain so they are handled instead.
        assert_ne!(is_wheel(&down), is_wheel(&up));
        assert_eq!(is_wheel(&wheel(MouseEventKind::Moved)), None);
        assert_eq!(
            is_wheel(&Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))),
            None
        );
    }

    #[test]
    fn pane_size_uses_the_bordered_inner_area_and_clamps() {
        let normal = pane_size(Rect::new(0, 0, 80, 24));
        assert_eq!((normal.cols, normal.rows), (78, 22));

        let tiny = pane_size(Rect::new(0, 0, 1, 1));
        assert_eq!((tiny.cols, tiny.rows), (1, 1));
        assert!(!pane_has_parser_viewport(Rect::new(0, 0, 3, 3)));
        assert!(!pane_has_parser_viewport(Rect::new(0, 0, 4, 3)));
        assert!(pane_has_parser_viewport(Rect::new(0, 0, 4, 4)));
    }

    #[test]
    fn house_rules_identify_the_room_roster_and_delegation_guidance() {
        let rules = house_rules(
            1,
            "claude · 1 (model sonnet, effort high); codex · 2 (model gpt-5, effort xhigh)",
            20,
        );
        assert!(rules.contains("you are Room 1"));
        assert!(rules.contains("$CROWDED_ROOM"));
        assert!(rules.contains(
            "Room roster: claude · 1 (model sonnet, effort high); codex · 2 (model gpt-5, effort xhigh)"
        ));
        assert!(rules.contains("numeric room number shown in the roster"));
        assert!(rules.contains("\"$CROWDED_BIN\" roster"));
        assert!(rules.contains("Announce your configured model and effort in your first response"));
        assert!(rules.contains("\"$CROWDED_BIN\" send ROOM_NUMBER"));
        assert!(rules.contains("same task ID and --role result"));
        assert!(rules.contains("\"$CROWDED_BIN\" control ROOM_NUMBER"));
        assert!(rules.contains("If a delegated task is unclear, ask the originating room."));
        assert!(!rules.contains("untrusted peer input"));
        assert!(rules.contains("pauses after 20 successful messages"));
    }

    #[test]
    fn welcome_roster_entries_report_configured_values_or_truthful_fallbacks() {
        assert_eq!(
            roster_entry("claude · 1", Some("sonnet"), true, Some("high"), true),
            "claude · 1 (model sonnet, effort high)"
        );
        assert_eq!(
            roster_entry("codex · 2", None, true, None, true),
            "codex · 2 (model unconfigured, effort unconfigured)"
        );
        assert_eq!(
            roster_entry("opencode · 3", Some("kimi-k3"), true, None, false),
            "opencode · 3 (model kimi-k3, effort unsupported)"
        );
        assert_eq!(
            roster_entry("bash · 4", None, false, None, false),
            "bash · 4 (model unsupported, effort unsupported)"
        );
    }

    fn sample(state: PulseState) -> PulseSample {
        PulseSample::now(state, None)
    }

    fn sample_with_model(state: PulseState, model: &str) -> PulseSample {
        PulseSample::now(state, Some(model.to_owned()))
    }

    fn stale_sample(state: PulseState) -> PulseSample {
        PulseSample {
            state,
            model: None,
            received_at: Instant::now() - PULSE_FRESHNESS_WINDOW - Duration::from_secs(1),
        }
    }

    fn stale_sample_with_model(state: PulseState, model: &str) -> PulseSample {
        PulseSample {
            state,
            model: Some(model.to_owned()),
            received_at: Instant::now() - PULSE_FRESHNESS_WINDOW - Duration::from_secs(1),
        }
    }

    #[test]
    fn effective_model_prefers_fresh_self_report_and_keeps_control_override() {
        let now = Instant::now();
        // A fresh hook self-report of model beats an unconfigured operator value.
        assert_eq!(
            effective_model(
                None,
                Some(&sample_with_model(PulseState::Working, "m1")),
                now
            ),
            Some("m1".to_owned())
        );
        // A stale self-report does not override; it is ignored.
        assert_eq!(
            effective_model(
                None,
                Some(&stale_sample_with_model(PulseState::Working, "m1")),
                now
            ),
            None
        );
        // The operator control model wins as an explicit override even against
        // a fresh self-report.
        assert_eq!(
            effective_model(
                Some("configured"),
                Some(&sample_with_model(PulseState::Working, "m1")),
                now
            ),
            Some("configured".to_owned())
        );
        // No self-report model and no configured value stays null/unconfigured.
        assert_eq!(effective_model(None, None, now), None);
        assert_eq!(
            effective_model(None, Some(&sample(PulseState::Ready)), now),
            None
        );
    }

    #[test]
    fn roster_state_prefers_offline_pulses_and_real_readiness() {
        let now = Instant::now();
        assert_eq!(
            roster_state(
                false,
                DeliveryGate::Ready,
                true,
                Some(sample(PulseState::Ready)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Offline,
                source: PulseSource::Offline,
            }
        );
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                true,
                Some(sample(PulseState::Ready)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Hook,
            }
        );
        // A self-reported Ready that contradicts a busy screen falls back to
        // the gate/screen inference, exactly as before.
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                false,
                Some(sample(PulseState::Ready)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Working,
                source: PulseSource::Gate,
            }
        );
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                true,
                Some(sample(PulseState::Thinking)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Thinking,
                source: PulseSource::Hook,
            }
        );
    }

    #[test]
    fn stale_transient_pulse_yields_to_a_demonstrably_ready_screen() {
        let now = Instant::now();
        // A hook that self-reported "thinking"/"working" long ago must not
        // override a screen the delivery gate demonstrably shows ready; the
        // resolved source names the readiness override explicitly.
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                true,
                Some(stale_sample(PulseState::Thinking)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Readiness,
            }
        );
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                true,
                Some(stale_sample(PulseState::Working)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Readiness,
            }
        );
        // Terminal self-reports stay authoritative even when stale.
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                true,
                Some(stale_sample(PulseState::Error)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Error,
                source: PulseSource::Hook,
            }
        );
        // Without a demonstrably ready screen the stale transient report
        // still stands (the gate cannot confirm deliverability).
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                false,
                Some(stale_sample(PulseState::Working)),
                now
            ),
            ResolvedPulse {
                state: PulseState::Working,
                source: PulseSource::Hook,
            }
        );
    }

    #[test]
    fn pulse_sample_marks_a_fresh_transient_self_report_as_fresh() {
        let now = Instant::now();
        assert!(!sample(PulseState::Thinking).is_stale(now));
        assert!(stale_sample(PulseState::Thinking).is_stale(now));
    }

    #[test]
    fn detail_only_pulse_does_not_refresh_stale_sample_or_create_state() {
        let now = Instant::now();
        let stale = stale_sample(PulseState::Thinking);
        let stale_at = stale.received_at;
        let mut room_pulses = vec![Some(stale)];
        let mut room_details = vec![RoomDetail::default()];
        apply_pulse_to_room(
            &mut room_pulses,
            &mut room_details,
            0,
            PulseState::Working,
            Some("new-model".to_owned()),
            Some(vec![DetailEvent::SubAgentStarted {
                id: "a1".to_owned(),
                kind: "Task".to_owned(),
            }]),
        );
        let sample = room_pulses[0].as_ref().unwrap();
        assert_eq!(sample.state, PulseState::Thinking);
        assert_eq!(sample.received_at, stale_at);
        assert!(sample.is_stale(now));
        assert_eq!(sample.model, Some("new-model".to_owned()));
        assert_eq!(room_details[0].sub_agents.len(), 1);

        // No-prior-sample case must not synthesize a visible state.
        let mut empty_pulses: Vec<Option<PulseSample>> = vec![None];
        let mut empty_details = vec![RoomDetail::default()];
        apply_pulse_to_room(
            &mut empty_pulses,
            &mut empty_details,
            0,
            PulseState::Working,
            Some("m2".to_owned()),
            Some(vec![DetailEvent::TodoUpsert {
                id: "t1".to_owned(),
                content: "x".to_owned(),
                status: "pending".to_owned(),
            }]),
        );
        assert!(
            empty_pulses[0].is_none(),
            "detail-only pulse must not create a sample when none existed"
        );
        assert_eq!(empty_details[0].todos.len(), 1);
    }

    #[test]
    fn tui_pulse_label_drops_source_and_shows_state_plus_age() {
        assert_eq!(
            resolved_label(
                ResolvedPulse {
                    state: PulseState::Ready,
                    source: PulseSource::Readiness,
                },
                None,
            ),
            "ready"
        );
        assert_eq!(
            resolved_label(
                ResolvedPulse {
                    state: PulseState::Thinking,
                    source: PulseSource::Hook,
                },
                Some(Duration::from_secs(8)),
            ),
            "thinking · 8s"
        );
        assert_eq!(
            resolved_label(
                ResolvedPulse {
                    state: PulseState::Offline,
                    source: PulseSource::Offline,
                },
                None,
            ),
            "offline"
        );
        assert_eq!(
            resolved_label(
                ResolvedPulse {
                    state: PulseState::Working,
                    source: PulseSource::Gate,
                },
                None,
            ),
            "working"
        );
    }

    #[test]
    fn pulse_badges_are_compact_and_conditional() {
        assert_eq!(pulse_badges(false, false), "");
        assert_eq!(pulse_badges(true, false), "H");
        assert_eq!(pulse_badges(false, true), "S");
        assert_eq!(pulse_badges(true, true), "H S");
    }

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn render_pulse_lines_separates_rooms_and_appends_total() {
        let costs = vec![
            Some((Instant::now(), "$0.100000".to_owned())),
            Some((Instant::now(), "$0.200000".to_owned())),
            Some((Instant::now(), "$0.050000".to_owned())),
        ];
        let lines = render_pulse_lines(
            &["Kiwi K2.7 code · 3", "Claude · 1", "DeepSeek · 4"],
            &[true, false, false],
            &[true, true, true],
            &["ready · 5s", "thinking · 8s", "working"],
            &costs,
            &[],
            0,
        );
        let text: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            text,
            vec![
                "Kiwi K2.7 code · 3  H S",
                "  ready · 5s · $0.100000",
                "",
                "Claude · 1  S",
                "  thinking · 8s · $0.200000",
                "",
                "DeepSeek · 4  S",
                "  working · $0.050000",
                "Total: $0.350000",
            ]
        );
    }

    #[test]
    fn pulse_total_sums_only_known_costs() {
        // All captured rooms known: no K/N qualifier.
        let costs = vec![
            Some((Instant::now(), "$0.100000".to_owned())),
            Some((Instant::now(), "$0.250000".to_owned())),
            Some((Instant::now(), "$0.050000".to_owned())),
        ];
        let all = vec![true, true, true];
        assert_eq!(
            pulse_total(&costs, &all),
            Some("Total: $0.400000".to_owned())
        );

        // One unknown among captured: K/N qualifier, unknown adds nothing.
        let costs = vec![
            Some((Instant::now(), "$0.100000".to_owned())),
            Some((Instant::now(), "unknown".to_owned())),
            Some((Instant::now(), "$0.050000".to_owned())),
        ];
        assert_eq!(
            pulse_total(&costs, &all),
            Some("Total (2/3 known): $0.150000".to_owned())
        );

        // A captured room with an unknown cost counts toward N but not K,
        // while a room that never captured a session is excluded from both
        // even when its cost figure is known.
        let costs = vec![
            Some((Instant::now(), "$0.100000".to_owned())),
            Some((Instant::now(), "$0.050000".to_owned())),
            Some((Instant::now(), "unknown".to_owned())),
        ];
        let partial = vec![true, false, true];
        assert_eq!(
            pulse_total(&costs, &partial),
            Some("Total (1/2 known): $0.100000".to_owned())
        );

        // No captured sessions: line omitted entirely.
        let none = vec![false, false, false];
        assert_eq!(pulse_total(&costs, &none), None);
    }
    #[test]
    fn quiet_threshold_grants_headroom_panes_extra_startup_grace() {
        assert_eq!(quiet_threshold(false), HOUSE_RULES_QUIET);
        assert_eq!(
            quiet_threshold(true),
            HOUSE_RULES_QUIET + HEADROOM_STARTUP_GRACE
        );
        assert!(quiet_threshold(true) > quiet_threshold(false));
    }

    #[test]
    fn headroom_intro_waits_for_genuine_readiness_past_the_shared_ceiling() {
        // A Headroom-wrapped room still bootstrapping must not have its
        // intro forced by the generic fixed ceiling: the wrapper's own
        // startup pause is real work, so no timeout may paste early.
        let gate = DeliveryGate::new(true);
        assert!(!gate.can_send_intro(
            false,
            INTRO_READINESS_CEILING + Duration::from_secs(1),
            intro_ceiling(true),
        ));
        // Genuine readiness still sends the Headroom intro immediately.
        assert!(gate.can_send_intro(true, Duration::ZERO, intro_ceiling(true)));
        // Non-Headroom rooms keep the shared fixed ceiling fallback.
        assert!(gate.can_send_intro(false, INTRO_READINESS_CEILING, intro_ceiling(false)));
        assert_eq!(intro_ceiling(false), INTRO_READINESS_CEILING);
        assert!(intro_ceiling(true) > intro_ceiling(false));
    }

    #[test]
    fn headroom_startup_lane_serializes_only_windows_headroom_spawns() {
        // Windows: a second Headroom room holds for the prior one's ready
        // boundary.
        assert!(headroom_lane_holds(true, true, true));
        // The first Headroom room has no prior to wait on.
        assert!(!headroom_lane_holds(true, true, false));
        // A non-Headroom room never holds, even on Windows.
        assert!(!headroom_lane_holds(true, false, true));
        // Non-Windows platforms keep startup fully concurrent.
        assert!(!headroom_lane_holds(false, true, true));
        assert!(!headroom_lane_holds(false, false, true));
    }

    #[test]
    fn pane_areas_tile_any_number_of_rooms() {
        let areas = pane_areas(Rect::new(0, 0, 120, 40), 3);
        assert_eq!(
            areas,
            [
                Rect::new(0, 0, 60, 20),
                Rect::new(60, 0, 60, 20),
                Rect::new(0, 20, 120, 20),
            ]
        );
        assert!(pane_areas(Rect::default(), 0).is_empty());

        let (rooms, pulse, status) = content_areas(Rect::new(0, 0, 120, 40));
        assert_eq!(rooms, Rect::new(0, 0, 84, 39));
        assert_eq!(pulse, Rect::new(84, 0, 36, 39));
        assert_eq!(status, Rect::new(0, 39, 120, 1));
    }

    #[test]
    fn pulse_entry_style_highlights_only_the_focused_room() {
        assert_eq!(pulse_entry_style(1, 1), Style::default().fg(Color::Cyan));
        assert_eq!(pulse_entry_style(0, 1), Style::default());
        assert_eq!(pulse_entry_style(2, 1), Style::default());
    }

    fn state_line_style(lines: &[Line], room_index: usize) -> Style {
        let offset = if room_index > 0 { room_index } else { 0 };
        let idx = room_index * 2 + offset;
        lines.get(idx + 1).map_or(Style::default(), |l| l.style)
    }

    #[test]
    fn render_pulse_state_color_offline_red() {
        let lines = render_pulse_lines(
            &["A · 1", "B · 2"],
            &[false, false],
            &[false, false],
            &["offline", "ready"],
            &[None, None],
            &[],
            1,
        );
        assert_eq!(state_line_style(&lines, 0), Style::default().fg(Color::Red));
    }

    #[test]
    fn render_pulse_state_color_error_red() {
        let lines = render_pulse_lines(
            &["A · 1"],
            &[false],
            &[false],
            &["error · 3s"],
            &[None],
            &[],
            1,
        );
        assert_eq!(state_line_style(&lines, 0), Style::default().fg(Color::Red));
    }

    #[test]
    fn render_pulse_state_color_ready_green() {
        let lines = render_pulse_lines(
            &["A · 1"],
            &[false],
            &[true],
            &["ready · 2s"],
            &[None],
            &[],
            1,
        );
        assert_eq!(
            state_line_style(&lines, 0),
            Style::default().fg(Color::Green)
        );
    }

    #[test]
    fn render_pulse_state_color_thinking_yellow() {
        let lines = render_pulse_lines(
            &["A · 1"],
            &[false],
            &[false],
            &["thinking · 5s"],
            &[None],
            &[],
            1,
        );
        assert_eq!(
            state_line_style(&lines, 0),
            Style::default().fg(Color::Yellow)
        );
    }

    #[test]
    fn render_pulse_state_color_working_yellow() {
        let lines = render_pulse_lines(
            &["A · 1"],
            &[false],
            &[false],
            &["working"],
            &[None],
            &[],
            1,
        );
        assert_eq!(
            state_line_style(&lines, 0),
            Style::default().fg(Color::Yellow)
        );
    }

    #[test]
    fn render_pulse_state_color_starting_yellow() {
        let lines = render_pulse_lines(
            &["A · 1"],
            &[false],
            &[false],
            &["starting · 1s"],
            &[None],
            &[],
            1,
        );
        assert_eq!(
            state_line_style(&lines, 0),
            Style::default().fg(Color::Yellow)
        );
    }

    #[test]
    fn render_pulse_state_color_unknown_unstyled() {
        let lines = render_pulse_lines(&["A · 1"], &[false], &[false], &["bogus"], &[None], &[], 1);
        assert_eq!(state_line_style(&lines, 0), Style::default());
    }

    #[test]
    fn render_pulse_focused_title_stays_cyan() {
        let lines = render_pulse_lines(
            &["A · 1", "B · 2"],
            &[false, false],
            &[false, false],
            &["offline", "ready"],
            &[None, None],
            &[],
            0,
        );
        assert_eq!(lines[0].style, Style::default().fg(Color::Cyan));
        assert_eq!(lines[1].style, Style::default().fg(Color::Red));
    }

    #[test]
    fn message_hats_are_visible_but_optional() {
        assert_eq!(message_with_hat(None, None, "hello"), "hello");
        assert_eq!(
            message_with_hat(Some("parser-fix"), Some("reviewer"), "inspect this"),
            "[task: parser-fix | requested role: reviewer]\ninspect this"
        );
    }

    #[test]
    fn delivery_fuse_trips_and_resets_at_its_limit() {
        let mut fuse = DeliveryFuse::new(2);
        fuse.record();
        assert_eq!(fuse.remaining(), 1);
        assert!(!fuse.is_tripped());
        fuse.record();
        assert!(fuse.is_tripped());
        fuse.reset();
        assert_eq!(fuse.remaining(), 2);
    }

    #[test]
    fn delivery_fuse_zero_limit_never_trips() {
        let mut fuse = DeliveryFuse::new(0);
        for _ in 0..100 {
            fuse.record();
        }
        assert!(!fuse.is_tripped());
        assert_eq!(fuse.remaining(), 0);
    }

    #[test]
    fn delivery_gate_waits_for_each_tui_busy_cycle() {
        assert_eq!(DeliveryGate::new(false), DeliveryGate::Ready);

        let mut gate = DeliveryGate::new(true);
        assert!(gate.can_send_intro(true, Duration::ZERO, INTRO_READINESS_CEILING));
        assert!(!gate.can_deliver(true));

        gate.intro_sent();
        gate.observe(true);
        assert_eq!(gate, DeliveryGate::IntroSent);
        gate.observe(false);
        gate.observe(true);
        assert!(gate.can_deliver(true));

        gate.message_sent();
        assert!(!gate.can_deliver(true));
        gate.observe(false);
        gate.observe(true);
        assert!(gate.can_deliver(true));
    }

    #[test]
    fn whisper_submit_resend_confirms_by_busy_then_idle_and_resends_stalled() {
        // Idle-after-processing: the pane went busy consuming the whisper and
        // is idle again, so the record retires without any resend.
        assert_eq!(
            resend_whisper_submit_due(true, true, true, Duration::ZERO),
            SubmitDisposition::Retire
        );
        // Ready-looking with staged text: never busy, ready the whole time,
        // past the ceiling -- must resubmit the stalled paste.
        assert_eq!(
            resend_whisper_submit_due(true, false, true, INTRO_READINESS_CEILING),
            SubmitDisposition::Resend
        );
        // Inside the window a ready-looking pane is still uncertain: wait.
        assert_eq!(
            resend_whisper_submit_due(true, false, true, Duration::from_secs(1)),
            SubmitDisposition::Wait
        );
        // Busy (consuming) inside the window: wait.
        assert_eq!(
            resend_whisper_submit_due(true, false, false, Duration::from_secs(1)),
            SubmitDisposition::Wait
        );
        // Still busy past the ceiling: consumed, retire without a resend.
        assert_eq!(
            resend_whisper_submit_due(true, true, false, INTRO_READINESS_CEILING),
            SubmitDisposition::Retire
        );
        // Shell rooms carry the submit inside the command: idle means done,
        // and a long-running command retires at the ceiling.
        assert_eq!(
            resend_whisper_submit_due(false, false, true, Duration::ZERO),
            SubmitDisposition::Retire
        );
        assert_eq!(
            resend_whisper_submit_due(false, false, false, Duration::from_secs(1)),
            SubmitDisposition::Wait
        );
        assert_eq!(
            resend_whisper_submit_due(false, false, false, INTRO_READINESS_CEILING),
            SubmitDisposition::Retire
        );
    }

    #[test]
    fn intro_ceiling_sends_a_stuck_rooms_intro_without_a_ready_heuristic() {
        // A Codex spinner that never quiets, or an OpenCode marker that
        // never matches the live layout, both look like this: the readiness
        // heuristic reports not-ready forever. Before the ceiling, the
        // room must not receive its intro out of turn.
        let gate = DeliveryGate::new(true);
        assert!(!gate.can_send_intro(false, Duration::from_secs(14), INTRO_READINESS_CEILING));
        // Once the ceiling elapses, the shared fallback sends it anyway.
        assert!(gate.can_send_intro(false, Duration::from_secs(15), INTRO_READINESS_CEILING));
        // A room whose heuristic genuinely reports ready never has to wait.
        assert!(DeliveryGate::new(true).can_send_intro(
            true,
            Duration::ZERO,
            INTRO_READINESS_CEILING
        ));
        // Ready rooms need no intro at all, ceiling or not.
        assert!(!DeliveryGate::Ready.can_send_intro(
            false,
            Duration::from_secs(999),
            INTRO_READINESS_CEILING
        ));
    }

    #[test]
    fn roster_state_does_not_stick_on_a_resumed_rooms_stale_starting_self_report() {
        // Resume skips the intro whisper, so the resumed process's own
        // SessionStart hook can self-report "starting" with no later Stop
        // hook to self-report "ready". Once the gate independently confirms
        // deliverability, that must win over the stale self-report and be
        // sourced as a readiness override, not a fresh hook.
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::Ready,
                true,
                Some(sample(PulseState::Starting)),
                Instant::now()
            ),
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Readiness,
            }
        );
        // A genuine startup (gate not yet Ready) still reports Starting.
        assert_eq!(
            roster_state(
                true,
                DeliveryGate::AwaitingIntro,
                false,
                Some(sample(PulseState::Starting)),
                Instant::now()
            ),
            ResolvedPulse {
                state: PulseState::Starting,
                source: PulseSource::Hook,
            }
        );
    }

    #[test]
    fn resume_control_resets_pulse_so_roster_shows_ready_immediately() {
        let resume_gate = gate_after_control(true, true);
        assert_eq!(resume_gate, DeliveryGate::Ready);
        assert_eq!(
            roster_state(true, resume_gate, true, None, Instant::now()),
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Gate,
            }
        );
        assert_eq!(
            roster_state(
                true,
                resume_gate,
                true,
                Some(sample(PulseState::Starting)),
                Instant::now()
            ),
            ResolvedPulse {
                state: PulseState::Ready,
                source: PulseSource::Readiness,
            }
        );
        let clear_gate = gate_after_control(true, false);
        assert_eq!(clear_gate, DeliveryGate::AwaitingIntro);
        assert_eq!(
            roster_state(
                true,
                clear_gate,
                true,
                Some(sample(PulseState::Starting)),
                Instant::now()
            ),
            ResolvedPulse {
                state: PulseState::Starting,
                source: PulseSource::Hook,
            }
        );
        assert_eq!(
            roster_state(true, clear_gate, true, None, Instant::now()),
            ResolvedPulse {
                state: PulseState::Starting,
                source: PulseSource::Gate,
            }
        );
    }

    #[test]
    fn resume_control_skips_intro_but_clear_and_configure_resend_it() {
        assert_eq!(gate_after_control(true, true), DeliveryGate::Ready);
        // Clear and configure never resume, so they resend the intro.
        assert_eq!(gate_after_control(true, false), DeliveryGate::AwaitingIntro);
        // Terminal panes take no intro either way, including across a resume.
        assert_eq!(gate_after_control(false, true), DeliveryGate::Ready);
        assert_eq!(gate_after_control(false, false), DeliveryGate::Ready);
    }

    #[test]
    fn force_restart_key_is_f5_or_an_offline_ctrl_r() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let f5 = KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE);
        assert!(
            force_restart_requested(f5, true),
            "F5 must restart an online pane"
        );
        assert!(
            force_restart_requested(f5, false),
            "F5 must restart an offline pane"
        );
        let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(
            !force_restart_requested(ctrl_r, true),
            "Ctrl+R on an online pane must still forward a literal 0x12 byte"
        );
        assert!(
            force_restart_requested(ctrl_r, false),
            "Ctrl+R on an offline pane must keep reviving it"
        );
    }

    /// A resume that could not be honoured leaves the room starting fresh with
    /// no history on screen. Treating it as resumed would cost that room both
    /// its intro and its session capture.
    #[test]
    fn a_resume_that_started_fresh_still_takes_the_intro() {
        assert_eq!(gate_after_control(true, false), DeliveryGate::AwaitingIntro);
    }

    #[test]
    fn normal_mode_d_is_not_intercepted_as_detail_shortcut() {
        let source = std::fs::read_to_string("src/app.rs").unwrap();
        let detail_variant = format!("{}::{}", "InputMode", "Detail");
        assert!(
            !source.contains(&detail_variant),
            "Detail input mode must have been removed"
        );
        let view = format!("{}View", "RoomDetail");
        assert!(
            !source.contains(&view),
            "Detail view type must have been removed with the modal"
        );
        let helper = format!("{}_{}", "detail", "for_room");
        assert!(
            !source.contains(&helper),
            "on-demand helper must be gone (replaced by cached refresh)"
        );
    }

    #[test]
    fn render_pulse_lines_renders_inline_detail_concisely() {
        let details = vec![
            RoomDetail {
                sub_agents: vec![
                    crate::room_detail::SubAgent {
                        id: "a1".to_owned(),
                        kind: "Task".to_owned(),
                        status: "running".to_owned(),
                    },
                    crate::room_detail::SubAgent {
                        id: "a2".to_owned(),
                        kind: "Explore".to_owned(),
                        status: "completed".to_owned(),
                    },
                ],
                todos: vec![crate::room_detail::TodoItem {
                    id: "t1".to_owned(),
                    content: "Build it".to_owned(),
                    status: "pending".to_owned(),
                }],
            },
            RoomDetail::default(),
        ];
        let costs = vec![None, None];
        let lines = render_pulse_lines(
            &["Claude \u{00b7} 1", "Codex \u{00b7} 2"],
            &[false, false],
            &[true, true],
            &["ready", "working"],
            &costs,
            &details,
            0,
        );
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.iter().any(|t| t.contains("sub-agents:")), "{text:?}");
        assert!(
            text.iter().any(|t| t.contains("Task (running)")),
            "{text:?}"
        );
        assert!(
            text.iter().any(|t| t.contains("Explore (completed)")),
            "{text:?}"
        );
        assert!(text.iter().any(|t| t.contains("todos:")), "{text:?}");
        assert!(
            text.iter().any(|t| t.contains("Build it [pending]")),
            "{text:?}"
        );
        let detail_lines = text
            .iter()
            .filter(|t| t.contains("sub-agents:") || t.contains("todos:"))
            .count();
        assert_eq!(detail_lines, 2);
    }

    #[test]
    fn render_pulse_lines_truncates_narrow_detail() {
        let long_content = "a".repeat(50);
        let details = vec![RoomDetail {
            sub_agents: vec![
                crate::room_detail::SubAgent {
                    id: "a1".to_owned(),
                    kind: "Task".to_owned(),
                    status: "running".to_owned(),
                },
                crate::room_detail::SubAgent {
                    id: "a2".to_owned(),
                    kind: "Task".to_owned(),
                    status: "running".to_owned(),
                },
                crate::room_detail::SubAgent {
                    id: "a3".to_owned(),
                    kind: "Task".to_owned(),
                    status: "running".to_owned(),
                },
                crate::room_detail::SubAgent {
                    id: "a4".to_owned(),
                    kind: "Task".to_owned(),
                    status: "running".to_owned(),
                },
            ],
            todos: vec![crate::room_detail::TodoItem {
                id: "t1".to_owned(),
                content: long_content.clone(),
                status: "pending".to_owned(),
            }],
        }];
        let costs = vec![None];
        let lines = render_pulse_lines(
            &["Claude \u{00b7} 1"],
            &[false],
            &[true],
            &["ready"],
            &costs,
            &details,
            0,
        );
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.iter().any(|t| t.contains("+2 more")), "{text:?}");
        assert!(text.iter().any(|t| t.contains("\u{2026}")), "{text:?}");
        assert!(
            !text.iter().any(|t| t.contains(&long_content)),
            "should be truncated"
        );
    }

    #[test]
    fn detail_collection_is_cached_outside_render_path() {
        let details = vec![RoomDetail::default()];
        let costs = vec![None];
        let first = render_pulse_lines(
            &["Claude \u{00b7} 1"],
            &[false],
            &[true],
            &["ready"],
            &costs,
            &details,
            0,
        );
        let second = render_pulse_lines(
            &["Claude \u{00b7} 1"],
            &[false],
            &[true],
            &["ready"],
            &costs,
            &details,
            0,
        );
        assert_eq!(first.len(), second.len());
        let source = std::fs::read_to_string("src/app.rs").unwrap();
        // Render itself must be pure; collection happens outside via bounded cache.
        let render_start = source.find("fn render_pulse_lines").unwrap();
        let snippet = &source[render_start..render_start + 1500];
        assert!(
            !snippet.contains("thread::spawn"),
            "render must not spawn threads"
        );
        assert!(
            !snippet.contains("collect_detail("),
            "render must not scan artifacts"
        );
        assert!(
            source.contains("DETAIL_REFRESH"),
            "bounded cache interval must exist"
        );
        assert!(
            source.contains("detail_last_refresh"),
            "cache must be maintained outside draw"
        );
    }
    #[test]
    fn pulse_panel_renders_concisely_at_real_width_and_excludes_completed_todos() {
        use ratatui::{
            Terminal,
            backend::TestBackend,
            layout::Rect,
            text::Text,
            widgets::{Paragraph, Wrap},
        };
        let details = vec![RoomDetail {
            sub_agents: vec![crate::room_detail::SubAgent {
                id: "a1".to_owned(),
                kind: "Task".to_owned(),
                status: "running".to_owned(),
            }],
            todos: vec![
                crate::room_detail::TodoItem {
                    id: "t1".to_owned(),
                    content: "Retrieve envelope + verify git t.. and resolve src..".to_owned(),
                    status: "completed".to_owned(),
                },
                crate::room_detail::TodoItem {
                    id: "t2".to_owned(),
                    content: "Build feature".to_owned(),
                    status: "pending".to_owned(),
                },
                crate::room_detail::TodoItem {
                    id: "t3".to_owned(),
                    content: "Verify no conflict".to_owned(),
                    status: "in_progress".to_owned(),
                },
            ],
        }];
        let costs = vec![None];
        let lines = render_pulse_lines(
            &["DeepSeek \u{00b7} 4"],
            &[true],
            &[true],
            &["ready"],
            &costs,
            &details,
            0,
        );
        let backend = TestBackend::new(36, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    Paragraph::new(Text::from(lines.clone())).wrap(Wrap { trim: false }),
                    Rect {
                        x: 0,
                        y: 0,
                        width: 36,
                        height: 12,
                    },
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let mut rendered = String::new();
        for y in 0..12 {
            for x in 0..36 {
                rendered.push_str(buffer.cell((x, y)).unwrap().symbol());
            }
            rendered.push('\n');
        }
        assert!(
            !rendered.contains("Retrieve envelope"),
            "completed todo should be absent, got: {rendered}"
        );
        assert!(rendered.contains("Build feature"), "{rendered}");
        assert!(rendered.contains("Verify no conflict"), "{rendered}");
        let non_empty = rendered.lines().filter(|l| !l.trim().is_empty()).count();
        assert!(
            non_empty <= 10,
            "pulse panel should be concise at 36 width, got {non_empty} lines: {rendered}"
        );
        assert!(
            !rendered.contains("t.."),
            "should not contain heavily truncated verbose text"
        );
    }

    #[test]
    fn background_collection_does_not_block_main_thread() {
        use std::{
            path::Path,
            sync::mpsc,
            thread,
            time::{Duration, Instant},
        };
        let (tx, rx) = mpsc::channel::<(usize, RoomDetail)>();
        let mut last_refresh = None;
        let mut pending = false;
        let now = Instant::now();
        let start = Instant::now();
        let scheduled = super::schedule_detail_collection(
            RoomDetailIdentity {
                index: 0,
                guest: "codex".to_owned(),
                cwd: Path::new("/tmp").to_path_buf(),
                title: "Codex \u{00b7} 2".to_owned(),
            },
            DetailScheduleCtx {
                now,
                last_refresh: &mut last_refresh,
                pending: &mut pending,
                tx: tx.clone(),
            },
            |guest, _cwd, title| {
                thread::sleep(Duration::from_millis(250));
                assert_eq!(guest, "codex");
                assert_eq!(title, "Codex \u{00b7} 2");
                RoomDetail {
                    sub_agents: vec![crate::room_detail::SubAgent {
                        id: "slow".to_owned(),
                        kind: "Task".to_owned(),
                        status: "running".to_owned(),
                    }],
                    todos: vec![],
                }
            },
        );
        assert!(scheduled, "should schedule");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(50),
            "scheduler blocked for {elapsed:?}, should be <50ms even with 250ms collector"
        );
        assert!(
            rx.try_recv().is_err(),
            "should be pending while background sleeps"
        );
        // Pending must block a second immediate schedule
        let mut last2 = last_refresh;
        let mut pending2 = pending;
        let scheduled2 = super::schedule_detail_collection(
            RoomDetailIdentity {
                index: 0,
                guest: "codex".to_owned(),
                cwd: Path::new("/tmp").to_path_buf(),
                title: "Codex \u{00b7} 2".to_owned(),
            },
            DetailScheduleCtx {
                now,
                last_refresh: &mut last2,
                pending: &mut pending2,
                tx: tx.clone(),
            },
            |_, _, _| panic!("should not be called while pending"),
        );
        assert!(!scheduled2, "should not reschedule while pending");
        thread::sleep(Duration::from_millis(300));
        let (idx, detail) = rx.try_recv().expect("background should have completed");
        assert_eq!(idx, 0);
        assert_eq!(detail.sub_agents[0].id, "slow");
        // Simulate main loop clearing pending after recv
        pending = false;
        let later = now + Duration::from_secs(3);
        let scheduled3 = super::schedule_detail_collection(
            RoomDetailIdentity {
                index: 0,
                guest: "codex".to_owned(),
                cwd: Path::new("/tmp").to_path_buf(),
                title: "Codex \u{00b7} 2".to_owned(),
            },
            DetailScheduleCtx {
                now: later,
                last_refresh: &mut last_refresh,
                pending: &mut pending,
                tx,
            },
            |_, _, _| RoomDetail::default(),
        );
        assert!(
            scheduled3,
            "should reschedule after interval and pending cleared"
        );
    }
}
