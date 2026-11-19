//! Ratatui rendering, focus, note composition, and the main event loop.

use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{Arc, mpsc},
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
    backend::{Backend, CrosstermBackend},
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
    native_route::{self, NativeRouter, NoticeSeverity},
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

pub(crate) struct DeliveryFuse {
    used: usize,
    limit: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryGate {
    AwaitingIntro,
    IntroSent,
    IntroRunning,
    Ready,
    MessageSent,
    MessageRunning,
}

impl DeliveryGate {
    pub(crate) fn new(needs_intro: bool) -> Self {
        if needs_intro {
            Self::AwaitingIntro
        } else {
            Self::Ready
        }
    }

    fn observe_pulse(&mut self, state: PulseState, input_ready: bool) {
        match state {
            PulseState::Thinking | PulseState::Working => self.observe(false),
            PulseState::Ready => self.observe(input_ready),
            _ => {}
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
    pub(crate) fn new(limit: usize) -> Self {
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
pub(crate) const HEADROOM_READY_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) fn wait_for_headroom_ready(
    pane: &mut Pane,
    last_output: &mut Option<Instant>,
    deadline: Duration,
) -> io::Result<bool> {
    // Returns Ok(true) when the deadline elapsed without readiness (caller
    // proceeds and surfaces a notice); Ok(false) on readiness, process exit or
    // shutdown.
    let started = Instant::now();
    loop {
        if crate::shutdown::requested() {
            return Ok(false);
        }
        if !pane.is_online() {
            return Ok(false);
        }
        if pane.drain_output()? {
            *last_output = Some(Instant::now());
        }
        let output_is_quiet = last_output.is_some_and(|last| {
            Instant::now().duration_since(last) >= quiet_threshold(pane.headroom_active())
        });
        if pane.automation_input_ready(output_is_quiet) {
            return Ok(false);
        }
        if started.elapsed() >= deadline {
            return Ok(true);
        }
        let remaining = deadline.saturating_sub(started.elapsed());
        std::thread::sleep(Duration::from_millis(25).min(remaining));
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
         If a delegated task is unclear, ask the originating room. \
         You do not need to do anything now: just check the roster and stand by \
         until a message arrives."
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

/// The welcome roster before any pane exists, read from each room's configured
/// controls. A fresh native Codex room needs its intro before its terminal
/// starts, so this cannot wait for live panes.
fn welcome_roster_specs(specs: &[RoomSpec]) -> String {
    specs
        .iter()
        .map(|spec| {
            let controls = pane::spec_capabilities(spec).supported_controls;
            roster_entry(
                &spec.title,
                pane::spec_model(spec).as_deref(),
                controls.contains(&SupportedControl::Model),
                pane::spec_effort(spec).as_deref(),
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
pub(crate) struct PulseSample {
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
    } else if !pane.needs_intro() && !pane.is_native() {
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

pub(crate) struct PendingSubmit<'a> {
    pub(crate) queue: &'a mut VecDeque<(Instant, usize, bool)>,
    pub(crate) now: Instant,
}

pub(crate) fn inject_ready_pending(
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
    // Once a target's queue is full in this pass, later envelopes for it must
    // wait too, so a later message can never overtake an earlier one.
    let mut blocked: Vec<usize> = Vec::new();
    let candidates = pending.len();
    for _ in 0..candidates {
        if fuse.is_tripped() {
            break;
        }
        let Some((id, target)) = pending.pop_front() else {
            break;
        };
        if blocked.contains(&target) {
            pending.push_back((id, target));
            continue;
        }
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                // Transient: keep the envelope and stop trying this room until a
                // later pass, in original order.
                pending.push_back((id, target));
                blocked.push(target);
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

pub(crate) fn resend_whisper_submits(
    pending: &mut VecDeque<(Instant, usize, bool)>,
    panes: &mut [Pane],
    input_ready: &[bool],
    now: Instant,
) {
    for _ in 0..pending.len() {
        let Some((injected_at, target, saw_busy)) = pending.pop_front() else {
            break;
        };
        // Do not age the record while its whisper is still queued or in flight:
        // restart the clock so the resubmit ceiling counts from the moment the
        // writer has actually written it.
        if panes[target].has_unwritten_input() {
            pending.push_back((now, target, saw_busy));
            continue;
        }
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
                // A full queue is transient, so keep the record and retry next
                // frame; any other failure means the writer is gone, drop it.
                match panes[target].resend_whisper_submit() {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        pending.push_back((injected_at, target, saw_busy));
                    }
                    Err(_) => {}
                }
            }
        }
    }
}

/// Sends one key to a pane, turning an enqueue failure into a user notice so a
/// transiently full or stopped write queue cannot end the event loop. A
/// rejected key is a failure, chosen here where the text is produced.
pub(crate) fn forward_key(pane: &mut Pane, key: KeyEvent) -> Option<(String, NoticeSeverity)> {
    pane.write_key(key).err().map(|error| {
        (
            format!("{} rejected input: {error}", pane.title()),
            NoticeSeverity::Failure,
        )
    })
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

/// Opens or closes the mailroom popup for the F2 key and the Esc close path.
/// Opening resets the scroll but keeps the notice history, so the failures
/// the status row points at are still there when the popup renders. Closing
/// is an explicit dismiss and clears the history. The other view changes
/// that clear (Tab, starting a whisper, opening or leaving settings) keep
/// doing so; nothing else touches the history.
fn toggle_mail_log(input_mode: &mut InputMode, mail_scroll: &mut u16, notice: &mut Notices) {
    match input_mode {
        InputMode::MailLog => {
            *input_mode = InputMode::Normal;
            notice.clear();
        }
        InputMode::Normal => {
            *input_mode = InputMode::MailLog;
            *mail_scroll = 0;
        }
        InputMode::Config(_) => unreachable!(),
        InputMode::Composing(_) => unreachable!(),
    }
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
    notice: &mut Notices,
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
    append_notice(
        notice,
        if !fuse_changed && updates.is_empty() {
            "no config changes to save".to_owned()
        } else {
            format!("room {} config saved", state.selected + 1)
        },
        NoticeSeverity::Info,
    );
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
        None,
    )
}

/// The `crowded resume` entry point: same room resolution as a plain
/// launch, but every supported guest starts with its resume-most-recent
/// flag already applied.
pub(crate) fn run_resumed() -> Result<(), Box<dyn std::error::Error>> {
        resume_with(room_specs_resumed()?, run_with)
}

/// The part of `crowded resume` that decides what each room does and what the
/// user is told, separated from the terminal so it can be exercised.
pub(crate) fn resume_with(
    mut resolved: crate::config::ResolvedConfig,
    run: impl FnOnce(
        Vec<RoomSpec>,
        Vec<bool>,
        usize,
        Vec<TokenPricing>,
        Option<String>,
    ) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let resumed = pane::resume_supported_specs(&mut resolved.specs);
    let notice = pane::native_resume_notice(&resolved.specs);
    run(
        resolved.specs,
        resumed,
        resolved.fuse_size,
        resolved.token_pricing,
        notice,
    )
}

/// Runs the launch first and only then takes over the terminal, so a slow
/// launch happens while the terminal is still usable.
pub(crate) fn launch_then_enter<N, G>(
    launch: impl FnOnce() -> io::Result<N>,
    enter: impl FnOnce() -> io::Result<G>,
) -> io::Result<(N, G)> {
    let launched = launch()?;
    Ok((launched, enter()?))
}

/// Rooms whose detail panel reads a legacy session entry. An owned-server room
/// has none; a Codex native room captures a real thread, so its detail stays.
pub(crate) fn rooms_needing_detail(panes: &[Pane]) -> Vec<usize> {
    (0..panes.len())
        .filter(|&index| !panes[index].is_native() || panes[index].codex_native())
        .collect()
}

/// The room the Ctrl+W composer would write to, or the notice that explains
/// why it cannot be written to.
pub(crate) fn compose_target(panes: &[Pane], focused: usize) -> Result<usize, String> {
        let target = (focused + 1) % panes.len();
    if panes[target].is_online() {
        Ok(target)
    } else {
        Err(format!("{} is offline", panes[target].title()))
    }
}

/// The three panes a room can start as. The event loop uses [`LivePanes`];
/// tests substitute a recorder, so the exact start decisions run without a
/// terminal.
pub(crate) trait PaneFactory {
    /// A running interactive process.
    fn interactive(
        &mut self,
        spec: RoomSpec,
        size: portable_pty::PtySize,
        environment: pane::GuestEnvironment,
        native: Option<crate::native_launch::NativeRoom>,
    ) -> Result<Pane, Box<dyn std::error::Error>>;
    /// A placeholder while the room's session is being created.
    fn pending(
        &mut self,
        spec: RoomSpec,
        size: portable_pty::PtySize,
        environment: pane::GuestEnvironment,
    ) -> Result<Pane, Box<dyn std::error::Error>>;
    /// A placeholder showing why the room was not started.
    fn unavailable(
        &mut self,
        spec: RoomSpec,
        size: portable_pty::PtySize,
        environment: pane::GuestEnvironment,
        reason: &str,
    ) -> Result<Pane, Box<dyn std::error::Error>>;
}

/// Starts real processes.
pub(crate) struct LivePanes;

impl PaneFactory for LivePanes {
    fn interactive(
        &mut self,
        spec: RoomSpec,
        size: portable_pty::PtySize,
        environment: pane::GuestEnvironment,
        native: Option<crate::native_launch::NativeRoom>,
    ) -> Result<Pane, Box<dyn std::error::Error>> {
        Pane::spawn_with_native(spec, size, environment, native)
    }

    fn pending(
        &mut self,
        spec: RoomSpec,
        size: portable_pty::PtySize,
        environment: pane::GuestEnvironment,
    ) -> Result<Pane, Box<dyn std::error::Error>> {
        Pane::spawn_pending(spec, size, environment)
    }

    fn unavailable(
        &mut self,
        spec: RoomSpec,
        size: portable_pty::PtySize,
        environment: pane::GuestEnvironment,
        reason: &str,
    ) -> Result<Pane, Box<dyn std::error::Error>> {
        Pane::spawn_unavailable(spec, size, environment, reason)
    }
}

/// Starts one room's process for the state its launch reached. Both the
/// startup path and the mint-completion path call this, so a room without a
/// session can never start an interactive Codex.
pub(crate) fn spawn_room(
    factory: &mut impl PaneFactory,
    spec: RoomSpec,
    size: portable_pty::PtySize,
    environment: pane::GuestEnvironment,
    native: Option<crate::native_launch::NativeRoom>,
    codex: Option<&pane::CodexMint>,
    claude: Option<&pane::ClaudeStart>,
) -> Result<Pane, Box<dyn std::error::Error>> {
    if let Some(pane::ClaudeStart::Failed(reason)) = claude {
        return factory.unavailable(spec, size, environment, reason);
    }
    match codex {
        Some(pane::CodexMint::Failed(reason)) => {
            factory.unavailable(spec, size, environment, reason)
        }
        Some(pane::CodexMint::Pending(_)) => factory.pending(spec, size, environment),
        Some(pane::CodexMint::Ready(uuid)) => {
            let mut spec = spec;
            pane::resume_session(&mut spec, uuid)?;
            factory.interactive(spec, size, environment, native)
        }
        None => factory.interactive(spec, size, environment, native),
    }
}

/// Starts every room in order and reports when each one started.
pub(crate) fn start_rooms(
    factory: &mut impl PaneFactory,
    specs: Vec<RoomSpec>,
    sizes: &[portable_pty::PtySize],
    environments: &[pane::GuestEnvironment],
    natives: Vec<Option<crate::native_launch::NativeRoom>>,
    codex: &[Option<pane::CodexMint>],
    claude: &[Option<pane::ClaudeStart>],
    last_output: &mut [Option<Instant>],
    serialize_headroom: bool,
    headroom_ready_timeout: Duration,
    notice: &mut Notices,
) -> Result<(Vec<Pane>, Vec<Instant>), Box<dyn std::error::Error>> {
    let mut panes: Vec<Pane> = Vec::with_capacity(specs.len());
    // Anchors `INTRO_READINESS_CEILING`: each room's own spawn instant, not
    // a shared one, so a slow room later in this loop isn't penalized for
    // rooms spawned before it.
    let mut spawned_at = Vec::with_capacity(specs.len());
    // Windows serializes Headroom bootstrap: the next Headroom room's spawn
    // waits for the prior one to reach its ready boundary, so two wrappers
    // never start their proxies at once. Non-Headroom rooms and every room
    // on non-Windows spawn immediately, exactly as before.
    let mut prior_headroom: Option<usize> = None;
    let mut natives = natives.into_iter();
    for (index, spec) in specs.into_iter().enumerate() {
        let headroom = pane::spec_uses_headroom(&spec);
        if headroom_lane_holds(serialize_headroom, headroom, prior_headroom.is_some()) {
            if let Some(prior) = prior_headroom {
                let timed_out = wait_for_headroom_ready(
                    &mut panes[prior],
                    &mut last_output[prior],
                    headroom_ready_timeout,
                )?;
                if timed_out {
                    let room = panes[prior].title().to_owned();
                    append_notice(
                        notice,
                        format!("{room}: Headroom did not become ready in time; continuing"),
                        NoticeSeverity::Failure,
                    );
                }
            }
        }
        // The owned server starts here, before the event loop, because the
        // loop cannot wait for it; a room that cannot start its server fails
        // startup instead of falling back to paste or a private server. A
        // Codex room whose session is still being created, or was not created,
        // starts no interactive process: its mail is held instead.
        let native = natives.next().flatten();
        panes.push(spawn_room(
            factory,
            spec,
            sizes[index],
            environments[index].clone(),
            native,
            codex[index].as_ref(),
            claude[index].as_ref(),
        )?);
        spawned_at.push(Instant::now());
        // A placeholder has no process and never produces output, so a later
        // room could wait on it forever.
        if headroom && panes[index].is_online() {
            prior_headroom = Some(index);
        }
    }
    Ok((panes, spawned_at))
}

/// Applies every mint that finished since the previous pass: a room whose
/// session was still being created becomes usable the moment its own mint
/// finishes, and nothing waits for a slower room.
#[allow(clippy::too_many_arguments)]
pub(crate) fn complete_mints(
    factory: &mut impl PaneFactory,
    codex: &mut [Option<pane::CodexMint>],
    panes: &mut [Pane],
    mut router: Option<&mut NativeRouter>,
    delivery_gates: &mut [DeliveryGate],
    spawned_at: &mut [Instant],
    last_output: &mut [Option<Instant>],
    room_pulses: &mut [Option<PulseSample>],
    sizes: &[portable_pty::PtySize],
    environments: &[pane::GuestEnvironment],
    notice: &mut Notices,
    now: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    for update in pane::poll_codex_mints(codex) {
        let (index, failure) = match update {
            pane::CodexUpdate::Minted { index } => {
                let spec = panes[index].room_spec().clone();
                match spawn_room(
                    factory,
                    spec,
                    sizes[index],
                    environments[index].clone(),
                    None,
                    codex[index].as_ref(),
                    None,
                ) {
                    Ok(started) => {
                        panes[index] = started;
                        delivery_gates[index] = DeliveryGate::new(false);
                        spawned_at[index] = now;
                        last_output[index] = None;
                        room_pulses[index] = Some(PulseSample::now(PulseState::Starting, None));
                        if let (Some(router), Some(mint)) =
                            (router.as_deref_mut(), codex[index].as_ref())
                        {
                            pane::bind_codex_room(router, panes, index, mint)?;
                        }
                        continue;
                    }
                    Err(error) => {
                        let reason = format!("could not start the resumed session: {error}");
                        codex[index] = Some(pane::CodexMint::Failed(reason.clone()));
                        (index, reason)
                    }
                }
            }
            pane::CodexUpdate::Failed { index, reason } => (index, reason),
        };
        let spec = panes[index].room_spec().clone();
        panes[index] = factory.unavailable(spec, sizes[index], environments[index].clone(), &failure)?;
        delivery_gates[index] = DeliveryGate::new(false);
        spawned_at[index] = now;
        last_output[index] = None;
        if let (Some(router), Some(mint)) = (router.as_deref_mut(), codex[index].as_ref()) {
            pane::bind_codex_room(router, panes, index, mint)?;
        }
        let message = format!("{}: {failure}; the room was not started", panes[index].title());
        eprintln!("{message}");
        append_notice(notice, message, NoticeSeverity::Failure);
    }
    Ok(())
}

/// Relaunches one native Codex room in place. A trusted stored session starts
/// at once; a mint runs on its own worker and the next pass binds the room when
/// it finishes. A clear's durable marker is already written by then.
#[allow(clippy::too_many_arguments)]
pub(crate) fn relaunch_codex_room(
    action: pane::CodexRelaunch,
    index: usize,
    size: portable_pty::PtySize,
    prompt: &str,
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    codex: &mut [Option<pane::CodexMint>],
    delivery_gates: &mut [DeliveryGate],
    last_output: &mut [Option<Instant>],
    spawned_at: &mut [Instant],
    room_pulses: &mut [Option<PulseSample>],
    environments: &[pane::GuestEnvironment],
    now: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let (spec, mint) = panes[index].relaunch_codex(action, prompt, index + 1)?;
    panes[index].cleanup();
    // A room that cannot start again is replaced by the visible reason, never
    // left half-bound to the session it could not launch.
    let (started, failure) = match spawn_room(
        &mut LivePanes,
        spec.clone(),
        size,
        environments[index].clone(),
        None,
        Some(&mint),
        None,
    ) {
        Ok(started) => (started, None),
        Err(error) => {
            let reason = format!("could not relaunch the room: {error}");
            let failed = pane::CodexMint::Failed(reason.clone());
            let failed = spawn_room(
                &mut LivePanes,
                spec,
                size,
                environments[index].clone(),
                None,
                Some(&failed),
                None,
            )?;
            (failed, Some(reason))
        }
    };
    panes[index] = started;
    let mint = match &failure {
        Some(reason) => pane::CodexMint::Failed(reason.clone()),
        None => mint,
    };
    if let Some(router) = router {
        pane::bind_codex_room(router, panes, index, &mint)?;
    }
    codex[index] = Some(mint);
    delivery_gates[index] = DeliveryGate::new(false);
    last_output[index] = None;
    spawned_at[index] = now;
    room_pulses[index] = Some(PulseSample::now(PulseState::Starting, None));
    match failure {
        Some(reason) => Err(io::Error::other(reason).into()),
        None => Ok(()),
    }
}

/// Relaunches one native Claude room in place. A reported session is resumed;
/// otherwise the room starts fresh and learns its session from the hook.
#[allow(clippy::too_many_arguments)]
/// A failure in the relaunch sequence is reported with the way out; nothing
/// is rolled back, so the room may be left recorded or bound on a session that
/// did not start.
fn launch_failure(title: &str, room: usize, cause: impl std::fmt::Display) -> Box<dyn std::error::Error> {
    io::Error::other(format!(
        "{title}: {cause}; the room may be recorded or bound on a session that did not start; use control {room} clear to start a new one"
    ))
    .into()
}

pub(crate) fn relaunch_claude_room(
    action: pane::ClaudeRelaunch,
    index: usize,
    size: portable_pty::PtySize,
    panes: &mut [Pane],
    mut router: Option<&mut NativeRouter>,
    delivery_gates: &mut [DeliveryGate],
    last_output: &mut [Option<Instant>],
    spawned_at: &mut [Instant],
    room_pulses: &mut [Option<PulseSample>],
    environments: &[pane::GuestEnvironment],
    now: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    let title = panes[index].title().to_owned();
    let generation = pane::next_generation()
        .map_err(|error| launch_failure(&title, index + 1, error))?;
    let (spec, session) = panes[index]
        .relaunch_claude(action, index + 1)
        .map_err(|error| launch_failure(&title, index + 1, error))?;
    if let Some(router) = router.as_deref_mut() {
        let cwd = panes[index].cwd().to_path_buf();
        let inbox = router
            .claude_inbox(index)
            .map_err(|error| launch_failure(&title, index + 1, error))?;
        let entry_session = if let Some(session) = &session {
            inbox
                .attach(&cwd, session, generation)
                .map_err(|error| launch_failure(&title, index + 1, error))?;
            panes[index].record_session(session);
            session.clone()
        } else {
            inbox
                .prepare(&cwd, index + 1, generation)
                .map_err(|error| launch_failure(&title, index + 1, error))?;
            // Internal placeholder only: never passed to Claude or persisted as
            // a Claude session; replaced when the hook reports the real session.
            format!("unreported-{}-{generation}", index + 1)
        };
        router
            .bind_claude(vec![crate::native_route::ClaudeEntry {
                index,
                title: title.clone(),
                session: Some(entry_session),
                generation,
                reason: None,
            }])
            .map_err(|error| launch_failure(&title, index + 1, error))?;
    }
    panes[index].cleanup();
    let started = spawn_room(
        &mut LivePanes,
        spec,
        size,
        environments[index].clone(),
        None,
        None,
        None,
    )
    .map_err(|error| launch_failure(&title, index + 1, error))?;
    panes[index] = started;
    delivery_gates[index] = DeliveryGate::new(false);
    last_output[index] = None;
    spawned_at[index] = now;
    room_pulses[index] = Some(PulseSample::now(PulseState::Starting, None));
    Ok(())
}

/// Revives one room after F5 or an offline Ctrl+R. A native Codex room comes
/// back into its session; every other room respawns exactly as before.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restart_room(
    index: usize,
    size: portable_pty::PtySize,
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    codex: &mut [Option<pane::CodexMint>],
    delivery_gates: &mut [DeliveryGate],
    last_output: &mut [Option<Instant>],
    spawned_at: &mut [Instant],
    room_pulses: &mut [Option<PulseSample>],
    environments: &[pane::GuestEnvironment],
    fuse_size: usize,
    now: Instant,
) -> Result<(), Box<dyn std::error::Error>> {
    if panes[index].claude_native() {
        return relaunch_claude_room(
            pane::ClaudeRelaunch::Restart,
            index,
            size,
            panes,
            router,
            delivery_gates,
            last_output,
            spawned_at,
            room_pulses,
            environments,
            now,
        );
    }
    if panes[index].codex_native() {
        let roster = welcome_roster(panes, room_pulses, now);
        let prompt = house_rules(index + 1, &roster, fuse_size);
        return relaunch_codex_room(
            pane::CodexRelaunch::Restart,
            index,
            size,
            &prompt,
            panes,
            router,
            codex,
            delivery_gates,
            last_output,
            spawned_at,
            room_pulses,
            environments,
            now,
        );
    }
    panes[index].restart(size)?;
    delivery_gates[index] = DeliveryGate::new(panes[index].needs_intro());
    last_output[index] = None;
    spawned_at[index] = now;
    room_pulses[index] = Some(PulseSample::now(PulseState::Starting, None));
    Ok(())
}

/// Applies the restart a key event asks for, if any, and returns the notice to
/// show. This is the only place Ctrl+R/F5 turns into a launch, so the decision
/// can be driven without a terminal.
#[allow(clippy::too_many_arguments)]
pub(crate) fn restart_key(
    key: KeyEvent,
    size: portable_pty::PtySize,
    focused: usize,
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    codex: &mut [Option<pane::CodexMint>],
    delivery_gates: &mut [DeliveryGate],
    last_output: &mut [Option<Instant>],
    spawned_at: &mut [Instant],
    room_pulses: &mut [Option<PulseSample>],
    environments: &[pane::GuestEnvironment],
    fuse_size: usize,
    now: Instant,
) -> Option<(String, NoticeSeverity)> {
    if !force_restart_requested(key, panes[focused].is_online()) {
        return None;
    }
    let title = panes[focused].title().to_owned();
    Some(
        match restart_room(
            focused,
            size,
            panes,
            router,
            codex,
            delivery_gates,
            last_output,
            spawned_at,
            room_pulses,
            environments,
            fuse_size,
            now,
        ) {
            Ok(()) => (format!("{title} restarted"), NoticeSeverity::Info),
            Err(error) => (
                format!("Could not restart {title}: {error}"),
                NoticeSeverity::Failure,
            ),
        },
    )
}

/// The loop state the per-event step reads. The loop borrows its locals into
/// this once and never decides a key's fate itself.
pub(crate) struct LoopState<'a> {
    pub(crate) focused: usize,
    pub(crate) fuse_size: usize,
    pub(crate) pane_size: &'a mut dyn FnMut(usize) -> io::Result<PtySize>,
    pub(crate) panes: &'a mut [Pane],
    pub(crate) router: Option<&'a mut NativeRouter>,
    pub(crate) codex: &'a mut [Option<pane::CodexMint>],
    pub(crate) delivery_gates: &'a mut [DeliveryGate],
    pub(crate) last_output: &'a mut [Option<Instant>],
    pub(crate) spawned_at: &'a mut [Instant],
    pub(crate) room_pulses: &'a mut [Option<PulseSample>],
    pub(crate) environments: &'a [pane::GuestEnvironment],
    pub(crate) notice: &'a mut Notices,
}

/// Builds the step's state from the loop's locals. Tests call this same wiring
/// function, so a change to any field it passes is visible to a named test.
#[allow(clippy::too_many_arguments)]
pub(crate) fn loop_state<'a>(
    focused: usize,
    fuse_size: usize,
    pane_size: &'a mut dyn FnMut(usize) -> io::Result<PtySize>,
    panes: &'a mut [Pane],
    router: Option<&'a mut NativeRouter>,
    codex: &'a mut [Option<pane::CodexMint>],
    delivery_gates: &'a mut [DeliveryGate],
    last_output: &'a mut [Option<Instant>],
    spawned_at: &'a mut [Instant],
    room_pulses: &'a mut [Option<PulseSample>],
    environments: &'a [pane::GuestEnvironment],
    notice: &'a mut Notices,
) -> LoopState<'a> {
    LoopState {
        focused,
        fuse_size,
        pane_size,
        panes,
        router,
        codex,
        delivery_gates,
        last_output,
        spawned_at,
        room_pulses,
        environments,
        notice,
    }
}

/// The one step the loop calls for a key event. It owns the restart guard, the
/// focused pane, the launcher arguments and the notice. Returns true when the
/// key was not consumed and the loop's input handling should still see it.
pub(crate) fn run_rooms_key_event(
    event: &Event,
    state: &mut LoopState<'_>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Event::Key(key) = event else {
        return Ok(true);
    };
    let focused = state.focused;
    if !force_restart_requested(*key, state.panes[focused].is_online()) {
        return Ok(true);
    }
    let size = (state.pane_size)(focused)?;
    let result = restart_key(
        *key,
        size,
        focused,
        state.panes,
        state.router.as_deref_mut(),
        state.codex,
        state.delivery_gates,
        state.last_output,
        state.spawned_at,
        state.room_pulses,
        state.environments,
        state.fuse_size,
        Instant::now(),
    );
    if let Some((text, severity)) = result {
        state.notice.push(text, severity);
    }
    Ok(false)
}

/// Sends doorbell mail to a native room through the router. This is the only
/// native delivery path: a room never falls back to terminal paste, not even
/// when the router cannot take the message.
pub(crate) fn send_native_mail(
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    mailroom: &mut Mailroom,
    from: &str,
    to: usize,
    message: &str,
    reply: Option<native_route::Reply>,
) -> Result<(), String> {
    if let Some(router) = router {
        return router.send(mailroom, from, to, message, reply);
    }
    let error = format!(
        "{} cannot receive native mail: native delivery is unavailable",
        panes[to].title()
    );
    if let Some(reply) = reply {
        reply(0, native_route::NativeReply::Failed(error.clone()));
    }
    Err(error)
}

/// Applies one authenticated session report from a room's own hook. The room
/// is never written to: rebinding only swaps the router's binding (and, for
/// Codex, the persisted reported id), so no path here can become a paste.
/// Every outcome is a notice; the wire answer carries the short reason.
pub(crate) fn report_native_session(
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    report: crate::doorbell::DoorbellSessionReport,
    notice: &mut Notices,
) {
    match apply_session_report(
        panes,
        router,
        report.from,
        report.session.clone(),
        report.pending,
    ) {
        Ok(status) => {
            append_notice(notice, status.clone(), NoticeSeverity::Info);
            report.reply_recorded(&status);
        }
        Err(reason) => {
            append_notice(notice, reason.clone(), NoticeSeverity::Failure);
            report.reply_failed(reason);
        }
    }
}

/// The rebind decision behind [`report_native_session`]: `Ok` is the status
/// to record (including ignored reports, which change nothing), `Err` names
/// a failure that also holds the room with its reason.
fn apply_session_report(
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    from: usize,
    session: Option<String>,
    pending: bool,
) -> Result<String, String> {
    let router = router.ok_or_else(|| "native delivery is unavailable".to_owned())?;
    if from >= panes.len() {
        return Err("a session report named an unknown room".to_owned());
    }
    let title = panes[from].title().to_owned();
    if !panes[from].is_native() {
        return Ok(format!(
            "ignored: {title} is not a native OpenCode, Codex or Claude room"
        ));
    }
    let current = router.binding_for(from);
    let Some(session) = session else {
        if !pending {
            return Ok("ignored: a session report needs a session id or --pending".to_owned());
        }
        if current.is_none() {
            return Ok(format!("ignored: {title} has no native binding to hold"));
        }
        router
            .mark_pending(from)
            .map_err(|error| format!("{title} could not be held: {error}"))?;
        return Ok(format!(
            "{title}: session pending after clear; new mail will be held"
        ));
    };
    if let Some(binding) = current.as_ref()
        && binding.session == session
    {
        // The same id is a no-op for the binding: no generation bump, no
        // retarget of other mail. Clearing still runs so pending-held mail is
        // released even if this run never saw the pending mark.
        let was_pending = router.is_pending(from);
        router
            .clear_pending(from)
            .map_err(|error| format!("{title} could not resume: {error}"))?;
        return Ok(if was_pending {
            format!("{title} is back on {session}; pending cleared")
        } else {
            format!("{title} is already on {session}")
        });
    }
    if panes[from].codex_native() && !crate::codex_native::valid_uuid(&session) {
        return Ok(format!(
            "ignored: the reported session is not a Codex thread id"
        ));
    }
    if panes[from].claude_native() && !crate::codex_native::valid_uuid(&session) {
        return Ok(format!(
            "ignored: the reported session is not a Claude session id"
        ));
    }
    let generation = crate::native_launch::GenerationAllocator::new(
        crate::pane::session_state::state_directory(),
    )
    .next()
    .map_err(|error| format!("{title} could not take a launch generation: {error}"))?;
    let destination = (from + 1).to_string();
    if panes[from].codex_native() {
        if let Err(error) = crate::pane::session_state::upsert_reported(
            "codex",
            panes[from].cwd(),
            &title,
            &session,
        ) {
            let reason = format!("session report for {session} could not be saved: {error}");
            let _ = router.unbind_room(from, reason.clone());
            return Err(reason);
        }
        let room = panes[from].codex_room_for(session.clone());
        panes[from].record_session(&session);
        router
            .rebind_room(crate::native_route::RoomEntry {
                index: from,
                title: title.clone(),
                binding: crate::codex_native::binding(&destination, &session, generation),
                backend: crate::native_route::Backend::Codex(Arc::new(room)),
            })
            .map_err(|error| format!("{title} could not be rebound: {error}"))?;
    } else if panes[from].claude_native() {
        // The room's own hook is the only other source of a Claude id.
        if let Err(error) = crate::pane::session_state::upsert_reported(
            "claude",
            panes[from].cwd(),
            &title,
            &session,
        ) {
            let reason = format!("session report for {session} could not be saved: {error}");
            let _ = router.unbind_room(from, reason.clone());
            return Err(reason);
        }
        panes[from].record_session(&session);
        router
            .rebind_claude(from, title.clone(), panes[from].cwd(), &session, generation)
            .map_err(|error| format!("{title} could not be rebound: {error}"))?;
    } else {
        let Some(current_binding) = current.clone() else {
            return Ok(format!(
                "ignored: {title} has no OpenCode binding to rebind"
            ));
        };
        let server = router.server_for(from).ok_or_else(|| {
            format!("{title} has no owned OpenCode server to rebind")
        })?;
        router
            .rebind_room(crate::native_route::RoomEntry {
                index: from,
                title: title.clone(),
                binding: crate::native_outbox::Binding {
                    room: current_binding.room.clone(),
                    session: session.clone(),
                    endpoint: current_binding.endpoint.clone(),
                    generation,
                },
                backend: crate::native_route::Backend::OpenCode(server),
            })
            .map_err(|error| format!("{title} could not be rebound: {error}"))?;
    }
    Ok(format!("{title} now follows reported session {session}"))
}

/// Reintroduces one native room through the router: the house rules travel
/// exactly like the startup intro, and the result is the notice to show.
/// The F4 handler calls this; a test runs it directly.
pub(crate) fn reintroduce_native_room(
    title: &str,
    router: &mut NativeRouter,
    mailroom: &mut Mailroom,
    to: usize,
    message: &str,
) -> (String, NoticeSeverity) {
    match router.send(mailroom, "The Crowded Room", to, message, None) {
        Ok(()) => (
            format!("{title} reintroduced to the room"),
            NoticeSeverity::Info,
        ),
        Err(error) => (
            format!("Could not reintroduce {title}: {error}"),
            NoticeSeverity::Failure,
        ),
    }
}

/// Keeps each native Codex room's liveness current. Called before the outbox
/// is ticked and after a child is reaped, so a queued message can never be sent
/// into a session whose process is gone.
pub(crate) fn refresh_codex_liveness(panes: &[Pane], router: &mut NativeRouter) {
    for (index, pane) in panes.iter().enumerate() {
        if pane.codex_native() {
            router.set_codex_live(index, pane.is_online());
        }
    }
}

/// One event-loop pass for the native outbox: liveness first, then scheduling.
pub(crate) fn tick_native(
    router: &mut NativeRouter,
    panes: &[Pane],
    now: Instant,
    mailroom: &mut Mailroom,
    paused: bool,
    fused: bool,
) -> Vec<(String, NoticeSeverity)> {
    refresh_codex_liveness(panes, router);
    router.tick(now, mailroom, paused, fused)
}

/// The native doorbell branch in one call. Its whole result, including a
/// failure, is a notice; nothing here can reach the room's terminal.
pub(crate) fn deliver_native_doorbell(
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    mailroom: &mut Mailroom,
    source: &str,
    target: &str,
    to: usize,
    body: &str,
    reply: Option<native_route::Reply>,
    fuse: &mut DeliveryFuse,
    delivery_paused: &mut bool,
    notice: &mut Notices,
) {
    let (text, severity) = match send_native_mail(panes, router, mailroom, source, to, body, reply) {
        Ok(()) => {
            fuse.record();
            if fuse.is_tripped() {
                *delivery_paused = true;
                (
                    format!(
                        "{source} → {target} saved; delivery fuse tripped at {}",
                        fuse.limit
                    ),
                    NoticeSeverity::Failure,
                )
            } else {
                (
                    format!("{source} → {target} saved for native delivery"),
                    NoticeSeverity::Info,
                )
            }
        }
        Err(error) => (
            format!("{source} → {target} not saved: {error}"),
            NoticeSeverity::Failure,
        ),
    };
    append_notice(notice, text, severity);
}

/// Everything the event loop does with one doorbell message. A native room's
/// mail only ever goes through the router; the terminal writers below it are
/// reached only for rooms that are not native, so no native failure, hold or
/// uncertainty can become a paste.
pub(crate) fn route_doorbell_message(
    envelope: crate::doorbell::DoorbellEnvelope,
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    mailroom: &mut Mailroom,
    delivery_gates: &mut [DeliveryGate],
    input_ready: &[bool],
    pending: &mut VecDeque<(u64, usize)>,
    submit_pending: &mut VecDeque<(Instant, usize, bool)>,
    fuse: &mut DeliveryFuse,
    delivery_paused: &mut bool,
    notice: &mut Notices,
    now: Instant,
) {
    let source = panes[envelope.from].title().to_owned();
    let target = panes[envelope.to].title().to_owned();
    let body = message_with_hat(
        envelope.task.as_deref(),
        envelope.role.as_deref(),
        &envelope.body,
    );
    if panes[envelope.to].is_native() {
        let to = envelope.to;
        deliver_native_doorbell(
            panes,
            router,
            mailroom,
            &source,
            &target,
            to,
            &body,
            Some(doorbell_reply(envelope)),
            fuse,
            delivery_paused,
            notice,
        );
        return;
    }
    if *delivery_paused || !delivery_gates[envelope.to].can_deliver(input_ready[envelope.to]) {
        if pending.len() >= 100 {
            envelope.reply_failed("delivery queue is full");
            return;
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
        append_notice(
            notice,
            format!("Envelope #{id:04} queued: {reason}"),
            NoticeSeverity::Info,
        );
        return;
    }
    let (id, result) = mailroom.deliver(source, &mut panes[envelope.to], body);
    match result {
        Ok(()) => {
            envelope.reply_injected(id);
            delivery_gates[envelope.to].message_sent();
            submit_pending.push_back((now, envelope.to, false));
            fuse.record();
            if fuse.is_tripped() {
                *delivery_paused = true;
                append_notice(
                    notice,
                    format!(
                        "Envelope #{id:04} injected; delivery fuse tripped at {}",
                        fuse.limit
                    ),
                    NoticeSeverity::Failure,
                );
            } else {
                append_notice(
                    notice,
                    format!(
                        "Doorbell envelope #{id:04} injected • fuse {}/{}",
                        fuse.remaining(),
                        fuse.limit
                    ),
                    NoticeSeverity::Info,
                );
            }
        }
        Err(error) => {
            envelope.reply_failed(error.to_string());
            append_notice(
                notice,
                format!("Envelope #{id:04} failed: {error}"),
                NoticeSeverity::Failure,
            );
        }
    }
}

/// Turns a native outcome into the doorbell's answer, exactly as far as it got:
/// "accepted" (OpenCode took it), "queued" (saved, still waiting), "held"
/// (saved, not sent, needs attention), "uncertain" (never resent), or a failure.
pub(crate) fn doorbell_reply(envelope: crate::doorbell::DoorbellEnvelope) -> native_route::Reply {
    Box::new(move |id, outcome| match outcome {
        native_route::NativeReply::Accepted => envelope.reply_native("accepted", id),
        native_route::NativeReply::Queued => envelope.reply_native("queued", id),
        native_route::NativeReply::Held(_) => envelope.reply_native("held", id),
        native_route::NativeReply::Uncertain => envelope.reply_native("uncertain", id),
        native_route::NativeReply::Failed(reason) => envelope.reply_failed(reason),
    })
}

/// Owned OpenCode servers are killed on reaper threads, so Crowded waits for
/// them on every way out instead of leaving a server behind.
fn run_with(
    specs: Vec<RoomSpec>,
    resumed: Vec<bool>,
        fuse_size: usize,
    token_pricing: Vec<TokenPricing>,
    startup_notice: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
        // A hang-up or a kill from outside ends the loop below like Ctrl+Q does.
    crate::shutdown::install();
    let result = run_rooms(specs, resumed, fuse_size, token_pricing, startup_notice);
    crate::native_launch::wait_for_reaps();
    result
}

pub(crate) trait DoorbellSource {
    fn try_recv(&mut self) -> Result<crate::doorbell::DoorbellEvent, std::sync::mpsc::TryRecvError>;
    fn path(&self) -> &std::path::Path;
}

impl DoorbellSource for crate::doorbell::Doorbell {
    fn try_recv(&mut self) -> Result<crate::doorbell::DoorbellEvent, std::sync::mpsc::TryRecvError> {
        crate::doorbell::Doorbell::try_recv(self)
    }

    fn path(&self) -> &std::path::Path {
        crate::doorbell::Doorbell::path(self)
    }
}

/// The terminal events the loop polls for. Tests supply a scripted source; the
/// real run reads from crossterm.
pub(crate) trait EventSource {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool>;
    fn read(&mut self) -> io::Result<Event>;
}

pub(crate) struct CrosstermEvents;

impl EventSource for CrosstermEvents {
    fn poll(&mut self, timeout: Duration) -> io::Result<bool> {
        event::poll(timeout)
    }

    fn read(&mut self) -> io::Result<Event> {
        event::read()
    }
}

/// The notices this run has to show. Bounded so a long-lived run cannot grow
/// them without limit: at most [`MAX_NOTICES`] entries are kept, the oldest
/// is dropped and counted, consecutive identical texts collapse into one
/// entry with a repeat count, and a failure is never dropped to make room for
/// a routine status. The severity travels with the notice from the code that
/// produced it; nothing here guesses it from the wording.
#[derive(Clone, Default, PartialEq)]
pub(crate) struct Notices {
    entries: Vec<Notice>,
    dropped: usize,
}

#[derive(Clone, PartialEq, Debug)]
struct Notice {
    text: String,
    severity: NoticeSeverity,
    repeats: usize,
}

const MAX_NOTICES: usize = 16;

impl Notices {
    /// Adds one notice, collapsing a repeat of the previous one and making
    /// room without ever displacing a failure for a routine status.
    pub(crate) fn push(&mut self, text: String, severity: NoticeSeverity) {
        if let Some(last) = self.entries.last_mut() {
            if last.text == text && last.severity == severity {
                last.repeats += 1;
                return;
            }
        }
        if self.entries.len() >= MAX_NOTICES {
            match self
                .entries
                .iter()
                .position(|entry| entry.severity != NoticeSeverity::Failure)
            {
                Some(victim) => {
                    self.entries.remove(victim);
                    self.dropped += 1;
                }
                None if severity != NoticeSeverity::Failure => {
                    self.dropped += 1;
                    return;
                }
                None => {
                    self.entries.remove(0);
                    self.dropped += 1;
                }
            }
        }
        self.entries.push(Notice {
            text,
            severity,
            repeats: 1,
        });
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.dropped = 0;
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn line(entry: &Notice) -> String {
        if entry.repeats > 1 {
            format!("{} (x{})", entry.text, entry.repeats)
        } else {
            entry.text.clone()
        }
    }

    /// Every notice with failures first, for the full list and the popup.
    /// Within each group the order stays oldest first.
    pub(crate) fn ordered_lines(&self) -> Vec<String> {
        let failures = self
            .entries
            .iter()
            .filter(|entry| entry.severity == NoticeSeverity::Failure)
            .map(Self::line);
        let infos = self
            .entries
            .iter()
            .filter(|entry| entry.severity != NoticeSeverity::Failure)
            .map(Self::line);
        failures.chain(infos).collect()
    }

    /// The one-row status line. One notice on its own is exactly its own
    /// text; with more than one, the newest failure leads behind a count so
    /// an earlier routine status can never hide it, clipped with an ellipsis
    /// to the width it has. When the width cannot fit enough of the message
    /// to act on, the line points at the mailroom instead of a bare count.
    fn status_line(&self, width: u16) -> Option<String> {
        let first = self.entries.first()?;
        if self.entries.len() == 1 && first.repeats == 1 {
            return Some(first.text.clone());
        }
        let failures = self
            .entries
            .iter()
            .filter(|entry| entry.severity == NoticeSeverity::Failure)
            .count();
        let (head, latest) = if failures > 0 {
            let label = if failures == 1 { "failure" } else { "failures" };
            let newest = self
                .entries
                .iter()
                .rev()
                .find(|entry| entry.severity == NoticeSeverity::Failure)?;
            (format!("{failures} {label}: "), newest)
        } else {
            (
                format!("{} notices: ", self.entries.len()),
                self.entries.last()?,
            )
        };
        let room = (width as usize).saturating_sub(head.chars().count());
        // A clipped fragment shorter than this cannot name a room and a
        // reason, so name the mailroom instead of showing a bare count.
        const USEFUL_CHARS: usize = 12;
        if room < USEFUL_CHARS {
            const POINTER: &str = "see mailroom";
            let budget = (width as usize).saturating_sub(head.chars().count());
            if budget == 0 {
                return Some(clipped(&head, width as usize));
            }
            return Some(format!("{head}{}", clipped(POINTER, budget)));
        }
        Some(format!("{head}{}", clipped(&Self::line(latest), room)))
    }
}

/// Renders the mailroom popup into a buffer: the notice history first with
/// failures before routine statuses, then the envelopes. The widget scrolls,
/// so every line stays reachable no matter how many envelopes precede it.
fn render_mail_popup(
    area: Rect,
    buf: &mut ratatui::buffer::Buffer,
    mailroom: &Mailroom,
    notice: &Notices,
    scroll: u16,
) {
    use ratatui::widgets::Widget;
    Widget::render(Clear, area, buf);
    // The full list lives here: the one row can only ever show the newest
    // failure, so every notice stays reachable by scrolling. Failures sort
    // first, ahead of queued envelopes.
    Widget::render(
        Paragraph::new(mail_popup_text(mailroom, notice))
            .block(Block::bordered().title(" Mailroom "))
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
        buf,
    );
}

/// The mailroom popup body: the notice history first with failures
/// before routine statuses, then the envelope history. Notices lead so a
/// failure is visible even with a full mailroom; the widget scrolls, so
/// every line stays reachable.
fn mail_popup_text(mailroom: &Mailroom, notice: &Notices) -> String {
    let mut text = String::new();
    if !notice.is_empty() {
        text.push_str("Notices (failures first; Up/Down or wheel to scroll)\n");
        for line in notice.ordered_lines() {
            text.push_str("  ");
            text.push_str(&line);
            text.push('\n');
        }
        if notice.dropped > 0 {
            text.push_str(&format!("  ({} older notices dropped)\n", notice.dropped));
        }
        text.push('\n');
    }
    text.push_str(&mailroom.summary());
    text
}

/// `text` cut to `budget` characters, with an ellipsis in the last one.
fn clipped(text: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if text.chars().count() <= budget {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(budget - 1).collect();
    out.push('…');
    out
}

/// Adds one notice to the ones this iteration has produced. Nothing inside
/// an iteration replaces an earlier notice: a later status line is appended,
/// so a failure reported first is still there when the UI reads the result.
fn append_notice(notice: &mut Notices, text: String, severity: NoticeSeverity) {
    notice.push(text, severity);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_rooms_iteration<D, B>(
    doorbell: &mut D,
    mut terminal: Option<&mut Terminal<B>>,
    panes: &mut [Pane],
    router: &mut Option<NativeRouter>,
    mailroom: &mut Mailroom,
    delivery_gates: &mut [DeliveryGate],
    last_output: &mut [Option<Instant>],
    spawned_at: &mut [Instant],
    room_pulses: &mut [Option<PulseSample>],
    room_details: &mut [RoomDetail],
    pending: &mut VecDeque<(u64, usize)>,
    submit_pending: &mut VecDeque<(Instant, usize, bool)>,
    fuse: &mut DeliveryFuse,
    delivery_paused: &mut bool,
    notice: &mut Notices,
    input_ready_out: &mut Vec<bool>,
    codex: &mut [Option<pane::CodexMint>],
    pane_sizes: &[portable_pty::PtySize],
    environments: &[pane::GuestEnvironment],
    token_pricing: &[TokenPricing],
    fuse_size: usize,
    room_count: usize,
    now: Instant,
) -> Result<(), Box<dyn std::error::Error>>
where
    D: DoorbellSource,
    B: Backend,
{
    if crate::shutdown::requested() {
        return Ok(());
    }
    complete_mints(
        &mut LivePanes,
        codex,
        panes,
        router.as_mut(),
        delivery_gates,
        spawned_at,
        last_output,
        room_pulses,
        pane_sizes,
        environments,
        notice,
        now,
    )?;
    if let Some(router) = router.as_mut() {
        let notices = tick_native(
            router,
            panes,
            now,
            mailroom,
            *delivery_paused,
            fuse.is_tripped(),
        );
        // Every notice this pass drained reaches the UI, in order: none is
        // dropped just because another one arrived with it.
        for (text, severity) in notices {
            append_notice(notice, text, severity);
        }
    }
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
    *input_ready_out = input_ready.clone();
    resend_whisper_submits(submit_pending, panes, &input_ready, now);
    for index in 0..room_count {
        delivery_gates[index].observe(input_ready[index]);
        let waited = now.duration_since(spawned_at[index]);
        if delivery_gates[index].can_send_intro(
            input_ready[index],
            waited,
            intro_ceiling(panes[index].headroom_active()),
        ) {
            let roster = welcome_roster(panes, room_pulses, now);
            match panes[index].send_whisper(
                "The Crowded Room",
                &house_rules(index + 1, &roster, fuse_size),
            ) {
                Ok(()) => {
                    delivery_gates[index].intro_sent();
                    panes[index].begin_session_capture();
                }
                Err(error) => {
                    append_notice(
                        notice,
                        format!(
                            "Could not teach {} the house rules: {error}",
                            panes[index].title()
                        ),
                        NoticeSeverity::Failure,
                    );
                }
            }
        }
    }
    if !*delivery_paused {
        let (injected, failed) = inject_ready_pending(
            pending,
            PendingSubmit {
                queue: submit_pending,
                now,
            },
            mailroom,
            panes,
            delivery_gates,
            &input_ready,
            fuse,
        );
        if fuse.is_tripped() {
            *delivery_paused = true;
            append_notice(
                notice,
                format!(
                    "Delivery fuse tripped after {injected} queued injections; {} remain",
                    pending.len()
                ),
                NoticeSeverity::Failure,
            );
        } else if injected > 0 || failed > 0 {
            append_notice(
                notice,
                format!("Queued delivery: {injected} injected, {failed} failed"),
                if failed > 0 {
                    NoticeSeverity::Failure
                } else {
                    NoticeSeverity::Info
                },
            );
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
                                    token_pricing,
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
                if pulse.detail.as_ref().is_none_or(|events| events.is_empty()) {
                    delivery_gates[pulse.from].observe_pulse(pulse.state, input_ready[pulse.from]);
                }
                apply_pulse_to_room(
                    room_pulses,
                    room_details,
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
                    append_notice(
                        notice,
                        format!("{source} control rejected by {target}"),
                        NoticeSeverity::Failure,
                    );
                    continue;
                }
                let result = (|| -> Result<bool, Box<dyn std::error::Error>> {
                    let size = match terminal.as_mut() {
                        Some(term) => {
                            term.autoresize()?;
                            let (rooms, _, _) = content_areas(term.size()?.into());
                            pane_size(pane_areas(rooms, room_count)[control.to])
                        }
                        None => pane_sizes[control.to],
                    };
                    let codex_native = panes[control.to].codex_native();
                    let claude_native = panes[control.to].claude_native();
                    let prompt = if codex_native {
                        house_rules(
                            control.to + 1,
                            &welcome_roster(panes, room_pulses, now),
                            fuse_size,
                        )
                    } else {
                        String::new()
                    };
                    match &control.action {
                        ControlAction::ClearContext if claude_native => relaunch_claude_room(
                            pane::ClaudeRelaunch::Clear,
                            control.to,
                            size,
                            panes,
                            router.as_mut(),
                            delivery_gates,
                            last_output,
                            spawned_at,
                            room_pulses,
                            environments,
                            now,
                        )
                        .map(|()| false),
                        ControlAction::Resume if claude_native => relaunch_claude_room(
                            pane::ClaudeRelaunch::Resume,
                            control.to,
                            size,
                            panes,
                            router.as_mut(),
                            delivery_gates,
                            last_output,
                            spawned_at,
                            room_pulses,
                            environments,
                            now,
                        )
                        .map(|()| true),
                        ControlAction::ClearContext if codex_native => relaunch_codex_room(
                            pane::CodexRelaunch::Clear,
                            control.to,
                            size,
                            &prompt,
                            panes,
                            router.as_mut(),
                            codex,
                            delivery_gates,
                            last_output,
                            spawned_at,
                            room_pulses,
                            environments,
                            now,
                        )
                        .map(|()| false),
                        ControlAction::Resume if codex_native => relaunch_codex_room(
                            pane::CodexRelaunch::Resume,
                            control.to,
                            size,
                            &prompt,
                            panes,
                            router.as_mut(),
                            codex,
                            delivery_gates,
                            last_output,
                            spawned_at,
                            room_pulses,
                            environments,
                            now,
                        )
                        .map(|()| true),
                        ControlAction::ClearContext => {
                            panes[control.to].clear_context(size).map(|_| false)
                        }
                        ControlAction::Resume => panes[control.to].resume_context(size),
                        ControlAction::Configure { model, effort } => panes[control.to]
                            .configure(model.as_deref(), effort.as_ref().map(|e| e.label()), size)
                            .map(|_| false),
                    }
                })();
                match result {
                    Ok(resumed) => {
                        delivery_gates[control.to] = gate_after_control(panes[control.to].needs_intro(), resumed);
                        last_output[control.to] = None;
                        spawned_at[control.to] = Instant::now();
                        room_pulses[control.to] = match &control.action {
                            // A native room's process is new even when its
                            // conversation is the same one.
                            ControlAction::Resume if !panes[control.to].is_native() => None,
                            _ => Some(PulseSample::now(PulseState::Starting, None)),
                        };
                        control.reply_applied();
                        append_notice(
                            notice,
                            format!("{source} told {target} to {label}"),
                            NoticeSeverity::Info,
                        );
                    }
                    Err(error) => {
                        control.reply_failed(error.to_string());
                        append_notice(
                            notice,
                            format!("{source} could not {label} on {target}: {error}"),
                            NoticeSeverity::Failure,
                        );
                    }
                }
                continue;
            }
            DoorbellEvent::SessionReport(report) => {
                report_native_session(panes, router.as_mut(), report, notice);
                continue;
            }
            DoorbellEvent::Message(envelope) => envelope,
        };
        route_doorbell_message(
            envelope,
            panes,
            router.as_mut(),
            mailroom,
            delivery_gates,
            &input_ready,
            pending,
            submit_pending,
            fuse,
            delivery_paused,
            notice,
            now,
        );
    }
    Ok(())
}



/// The production event loop: pumps mints, output, the outbox and the doorbell,
/// draws, then reads and handles terminal events until Ctrl+Q or a shutdown
/// request. The event source and terminal are injected so a test can run this
/// same loop with scripted events and a fake terminal.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_loop<B, E, D>(
    terminal: &mut Terminal<B>,
    events: &mut E,
    doorbell: &mut D,
    mut panes: &mut [Pane],
    router: &mut Option<NativeRouter>,
    mut codex: &mut [Option<pane::CodexMint>],
    environments: &[pane::GuestEnvironment],
    mut spawned_at: &mut [Instant],
    mut last_output: &mut [Option<Instant>],
    pane_sizes: &mut Vec<portable_pty::PtySize>,
    token_pricing: &[TokenPricing],
    mut fuse_size: usize,
    mut notice: &mut Notices,
    resumed: &[bool],
    room_count: usize,
) -> Result<(), Box<dyn std::error::Error>>
where
    B: Backend,
    E: EventSource,
    D: DoorbellSource,
{
    let mut delivery_gates = panes
        .iter()
        .enumerate()
        .map(|(index, pane)| DeliveryGate::new(pane.needs_intro() && !resumed[index]))
        .collect::<Vec<_>>();
    let mut focused = 0;
    let mut input_mode = InputMode::Normal;
    // How far the mailroom popup is scrolled. Reset whenever it opens; the
    // notice history itself is kept on open and cleared on dismiss, so the
    // failures the status row points at are still there to scroll to.
    let mut mail_scroll: u16 = 0;
    // A Codex room is bound to the session the launch phase gave it before any
    // mail is sent. A room that was not started stays native and unbound with
    // its reason, so its mail is held instead of being pasted.
    if let Some(router) = router.as_mut() {
        pane::bind_codex_rooms(router, &mut panes, &codex)?;
    }
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

    // A native room is introduced through its outbox, like all of its mail.
    if let Some(router) = router.as_mut() {
        let now = Instant::now();
        for index in 0..room_count {
            // A native Codex room already received its intro from the exec
            // turn that created its session, and a room that was not started
            // has nowhere to receive one.
            if router.is_native(index) && codex[index].is_none() {
                let roster = welcome_roster(&panes, &room_pulses, now);
                if let Err(error) = router.send(
                    &mut mailroom,
                    "The Crowded Room",
                    index,
                    &house_rules(index + 1, &roster, fuse_size),
                    None,
                ) {
                    notice.push(
                        format!(
                            "Could not teach {} the house rules: {error}",
                            panes[index].title()
                        ),
                        NoticeSeverity::Failure,
                    );
                }
            }
        }
    }

        loop {
        if crate::shutdown::requested() {
            break;
        }
        let now = Instant::now();
        let mut input_ready = vec![false; room_count];
        run_rooms_iteration(
            doorbell,
            Some(&mut *terminal),
            panes,
            router,
            &mut mailroom,
            &mut delivery_gates,
            last_output,
            spawned_at,
            &mut room_pulses,
            &mut room_details,
            &mut pending,
            &mut submit_pending,
            &mut fuse,
            &mut delivery_paused,
            notice,
            &mut input_ready,
            codex,
            pane_sizes,
            environments,
            token_pricing,
            fuse_size,
            room_count,
            now,
        )?;

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
            let pricing = token_pricing.to_vec();
            schedule_cost_collection(
                CostIdentity {
                    index,
                    guest: guest.clone(),
                    cwd: cwd.clone(),
                    title: title.clone(),
                    pricing: pricing.to_vec(),
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
                                    for index in rooms_needing_detail(&panes) {
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

        for pane in panes.iter_mut() {
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
                InputMode::Normal => notice.status_line(status.width).unwrap_or_else(|| {
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
            let status_style = match (&input_mode, !notice.is_empty()) {
                (InputMode::Composing(_), _) => Style::default().fg(Color::Yellow),
                (InputMode::MailLog, _) => Style::default().fg(Color::Cyan),
                (InputMode::Config(_), _) => Style::default().fg(Color::Yellow),
                (InputMode::Normal, true) => Style::default().fg(Color::Yellow),
                (InputMode::Normal, false) => Style::default().fg(Color::DarkGray),
            };
            frame.render_widget(Paragraph::new(status_text).style(status_style), status);

            if matches!(&input_mode, InputMode::MailLog) {
                let popup = mail_popup_area(frame.area());
                render_mail_popup(popup, frame.buffer_mut(), &mailroom, &notice, mail_scroll);
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


        }).map_err(|error| io::Error::other(error.to_string()))?;
        for pane in panes.iter_mut() {
            pane.restore_scroll();
        }

        for pane in panes.iter_mut() {
            pane.poll_exit()?;
        }
        // A child reaped here must not leave its room marked live for the next
        // pass, or for a doorbell handled later in this one.
        if let Some(router) = router.as_mut() {
            refresh_codex_liveness(&panes, router);
        }

        let next_event = match stashed_events.pop_front() {
            Some(event) => Some(event),
            None if events.poll(Duration::from_millis(16))? => Some(events.read()?),
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
                    toggle_mail_log(&mut input_mode, &mut mail_scroll, &mut notice);
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
                    notice.clear();
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
                        notice.push(
                            format!(
                                "Automatic delivery resumed; {} queued",
                                pending.len()
                            ),
                            NoticeSeverity::Info,
                        );
                    } else {
                        delivery_paused = true;
                        notice.push(
                            "Automatic delivery paused".to_owned(),
                            NoticeSeverity::Info,
                        );
                    }
                }
                Event::Key(key)
                    if key.code == KeyCode::F(4)
                        && key.kind == KeyEventKind::Press
                        && matches!(&input_mode, InputMode::Normal) =>
                {
                                        let roster = welcome_roster(&panes, &room_pulses, now);
                    if let Some(router) = router
                        .as_mut()
                        .filter(|router| router.is_native(focused))
                    {
                        let message = house_rules(focused + 1, &roster, fuse_size);
                        let title = panes[focused].title().to_owned();
                        let (text, severity) = reintroduce_native_room(
                            &title,
                            router,
                            &mut mailroom,
                            focused,
                            &message,
                        );
                        notice.push(text, severity);
                    } else {
                    match panes[focused].send_whisper(
                        "The Crowded Room",
                        &house_rules(focused + 1, &roster, fuse_size),
                    ) {
                        Ok(()) => {
                            delivery_gates[focused].intro_sent();
                            panes[focused].begin_session_capture();
                            notice.push(
                                format!(
                                    "{} reintroduced to the room",
                                    panes[focused].title()
                                ),
                                NoticeSeverity::Info,
                            );
                        }
                        Err(error) => {
                                                        notice.push(
                                format!(
                                    "Could not reintroduce {}: {error}",
                                    panes[focused].title()
                                ),
                                NoticeSeverity::Failure,
                            );
                        }
                    }
                    }
                }
                Event::Key(key) if run_rooms_key_event(
                    &Event::Key(key),
                    &mut loop_state(
                        focused,
                        fuse_size,
                        &mut |index| {
                            terminal.autoresize().map_err(|error| io::Error::other(error.to_string()))?;
                            let (rooms, _, _) = content_areas(terminal.size().map_err(|error| io::Error::other(error.to_string()))?.into());
                            Ok(pane_size(pane_areas(rooms, room_count)[index]))
                        },
                        &mut panes,
                        router.as_mut(),
                        &mut codex,
                        &mut delivery_gates,
                        &mut last_output,
                        &mut spawned_at,
                        &mut room_pulses,
                        &environments,
                        &mut notice,
                    ),
                )? => match &mut input_mode {
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
                                        if let Some(router) = router
                                            .as_mut()
                                            .filter(|router| router.is_native(target))
                                        {
                                            let (text, severity) = match router.send(
                                                &mut mailroom,
                                                &source,
                                                target,
                                                &message,
                                                None,
                                            ) {
                                                Ok(()) => (
                                                    "Saved for native delivery".to_owned(),
                                                    NoticeSeverity::Info,
                                                ),
                                                Err(error) => (
                                                    format!("Not saved: {error}"),
                                                    NoticeSeverity::Failure,
                                                ),
                                            };
                                            notice.push(text, severity);
                                        } else if delivery_paused
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
                                            notice.push(
                                                format!("Envelope #{id:04} queued"),
                                                NoticeSeverity::Info,
                                            );
                                        } else {
                                            let (id, result) = mailroom.deliver(
                                                source,
                                                &mut panes[target],
                                                message,
                                            );
                                            let (text, severity) = match result {
                                                Ok(()) => {
                                                    delivery_gates[target].message_sent();
                                                    submit_pending.push_back((now, target, false));
                                                    (
                                                        format!("Envelope #{id:04} injected"),
                                                        NoticeSeverity::Info,
                                                    )
                                                }
                                                Err(error) => (
                                                    format!("Envelope #{id:04} failed: {error}"),
                                                    NoticeSeverity::Failure,
                                                ),
                                            };
                                            notice.push(text, severity);
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
                                                                                    match compose_target(&panes, focused) {
                                Ok(_) => {
                                    input_mode = InputMode::Composing(String::new());
                                    notice.clear();
                                }
                                Err(reason) => notice.push(reason, NoticeSeverity::Info),
                            }
                        } else if key.code == KeyCode::Tab
                            && key.modifiers == KeyModifiers::NONE
                            && key.kind == KeyEventKind::Press
                        {
                            focused = (focused + 1) % panes.len();
                            notice.clear();
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
                            if let Some((message, severity)) = forward_key(&mut panes[focused], key)
                            {
                                notice.push(message, severity);
                            }
                        } else {
                            notice.push(
                                format!(
                                    "{} is offline; press Ctrl+R to revive it",
                                    panes[focused].title()
                                ),
                                NoticeSeverity::Info,
                            );
                        }
                    }
                    InputMode::MailLog => {
                        if key.code == KeyCode::Esc && key.kind == KeyEventKind::Press {
                            toggle_mail_log(&mut input_mode, &mut mail_scroll, &mut notice);
                        } else if key.code == KeyCode::Up && key.kind == KeyEventKind::Press {
                            mail_scroll = mail_scroll.saturating_sub(1);
                        } else if key.code == KeyCode::Down && key.kind == KeyEventKind::Press
                        {
                            mail_scroll = mail_scroll.saturating_add(1);
                        } else if key.code == KeyCode::PageUp && key.kind == KeyEventKind::Press
                        {
                            mail_scroll = mail_scroll.saturating_sub(10);
                        } else if key.code == KeyCode::PageDown
                            && key.kind == KeyEventKind::Press
                        {
                            mail_scroll = mail_scroll.saturating_add(10);
                        }
                    }
                    InputMode::Config(state) => {
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                            match key.code {
                                KeyCode::Esc if key.kind == KeyEventKind::Press => {
                                    input_mode = InputMode::Normal;
                                    notice.clear();
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
                    terminal.autoresize().map_err(|error| io::Error::other(error.to_string()))?;
                    let (rooms, _, _) = content_areas(terminal.size().map_err(|error| io::Error::other(error.to_string()))?.into());
                    let areas = pane_areas(rooms, room_count);
                    *pane_sizes = areas.iter().map(|area| pane_size(*area)).collect();
                    for (pane, size) in panes.iter_mut().zip(pane_sizes.iter()) {
                        pane.resize(*size)?;
                    }
                }
                // Wheel routing depends on whether the focused pane is in
                // alternate-screen mode: primary screen scrolls parent
                // scrollback, alternate screen forwards as PageUp/PageDown.
                Event::Mouse(mouse) => {
                    if matches!(&input_mode, InputMode::MailLog) {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                mail_scroll = mail_scroll.saturating_sub(3);
                            }
                            MouseEventKind::ScrollDown => {
                                mail_scroll = mail_scroll.saturating_add(3);
                            }
                            _ => {}
                        }
                    } else if matches!(&input_mode, InputMode::Normal) {
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
    for pane in panes.iter_mut() {
        pane.cleanup();
    }
    terminal.show_cursor().map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}


fn run_rooms(
    mut specs: Vec<RoomSpec>,
    resumed: Vec<bool>,
        fuse_size: usize,
    token_pricing: Vec<TokenPricing>,
    startup_notice: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let room_count = specs.len();
    debug_assert_eq!(resumed.len(), room_count);
    // Each room receives only its own capability token.
    let mut doorbell = Doorbell::start(room_count)?;
    // Owned OpenCode servers start before the terminal is taken over, in
    // parallel, so a slow start never freezes the event loop; a room that
    // cannot start fails the launch instead of falling back to paste.
    let environments = (0..room_count)
        .map(|index| doorbell.guest_environment(index))
        .collect::<Result<Vec<_>, _>>()?;
    if specs.iter().any(|spec| spec.native_delivery) {
        eprintln!("Starting native rooms...");
    }
    // A native Codex room's session is created before its terminal starts, so
    // its binding is known instead of discovered. That is one model turn per
    // such room, run in parallel and bounded.
    let codex_prompts: Vec<Option<String>> = {
        let roster = welcome_roster_specs(&specs);
        specs
            .iter()
            .enumerate()
            .map(|(index, spec)| {
                pane::codex_native_spec(spec).then(|| house_rules(index + 1, &roster, fuse_size))
            })
            .collect()
    };
    if codex_prompts.iter().any(Option::is_some) {
        eprintln!("Creating native Codex sessions (one model turn per room)...");
    }
    let mut codex = pane::start_codex_mints(&mut specs, &environments, &codex_prompts);
    let mut mint_notices: Vec<String> = codex
        .iter()
        .enumerate()
        .filter_map(|(index, mint)| match mint {
            Some(pane::CodexMint::Failed(reason)) => Some(format!(
                "{}: {reason}; the room was not started",
                specs[index].title
            )),
            _ => None,
        })
        .collect();
    for message in &mint_notices {
        eprintln!("{message}");
    }
    // A native Claude room's session id is Crowded's to choose, so it is fixed
    // and saved before the terminal starts: no model turn is involved.
    let (mut claude_starts, mut claude_notices) = pane::start_claude_sessions(&mut specs);
    for message in &claude_notices {
        eprintln!("{message}");
    }
        // `?` returns early on an error. The guards below still run their Drop code.
        let ((natives, router), _terminal_guard) = launch_then_enter(
        || {
            let natives = pane::launch_native_rooms(&specs, &environments)?;
            let router = pane::start_native_router(&specs, &natives)?;
            Ok((natives, router))
        },
        TerminalGuard::enter,
    )?;
    let mut router: Option<NativeRouter> = router;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (rooms, _, _) = content_areas(terminal.size()?.into());
    let areas = pane_areas(rooms, room_count);
    let mut last_output = vec![None::<Instant>; room_count];
    let mut pane_sizes: Vec<_> = areas.iter().map(|area| pane_size(*area)).collect();
    let mut notice = Notices::default();
    if let Some(text) = startup_notice {
        notice.push(text, NoticeSeverity::Info);
    }
    // Every Claude session and binding exists before any terminal starts.
    if let Some(router) = router.as_mut() {
        claude_notices.extend(pane::bind_claude_rooms(
            router,
            &specs,
            &mut claude_starts,
        )?);
    }
    let (mut panes, mut spawned_at) = start_rooms(
        &mut LivePanes,
        specs,
        &pane_sizes,
        &environments,
        natives,
        &codex,
        &claude_starts,
        &mut last_output,
        cfg!(windows),
        HEADROOM_READY_TIMEOUT,
        &mut notice,
    )?;
    for message in mint_notices.drain(..) {
        notice.push(message, NoticeSeverity::Failure);
    }
    for message in claude_notices.drain(..) {
        notice.push(message, NoticeSeverity::Failure);
    }
    run_loop(
        &mut terminal,
        &mut CrosstermEvents,
        &mut doorbell,
        &mut panes,
        &mut router,
        &mut codex,
        &environments,
        &mut spawned_at,
        &mut last_output,
        &mut pane_sizes,
        &token_pricing,
        fuse_size,
        &mut notice,
        &resumed,
        room_count,
    )
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doorbell_answers_say_exactly_how_far_a_native_message_got() {
        use crate::doorbell::DoorbellEnvelope;
        use native_route::NativeReply::*;
        let answer = |outcome| {
            let (envelope, answers) = DoorbellEnvelope::for_test();
            doorbell_reply(envelope)(7, outcome);
            answers.read()
        };
        assert_eq!(answer(Accepted), (true, "accepted".to_owned(), Some(7), None));
        assert_eq!(answer(Queued), (true, "queued".to_owned(), Some(7), None));
        assert_eq!(answer(Held("why".to_owned())), (true, "held".to_owned(), Some(7), None));
        assert_eq!(answer(Uncertain), (true, "uncertain".to_owned(), Some(7), None));
        assert_eq!(
            answer(Failed("HTTP 401".to_owned())),
            (false, "rejected".to_owned(), None, Some("HTTP 401".to_owned()))
        );
    }

    /// Intro, doorbell mail, the Ctrl+W composer and F4 reintroduction are the
    /// four ways agent text reaches a room; each must offer a native room to the
    /// router before it can fall through to a terminal write.
    #[test]
    fn all_four_agent_text_entry_points_route_native_rooms_to_the_router() {
        let source = include_str!("app.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        assert_eq!(production.matches("router.send(").count(), 4);
        // The terminal writers that remain are the two for ordinary rooms.
        assert_eq!(production.matches(".send_whisper(").count(), 2);
        // The native delivery helper has no terminal writer of its own, so an
        // error there cannot turn into a paste.
        let helper = &production[production.find("fn send_native_mail(").unwrap()..];
        let helper = &helper[..helper.find("\n}\n").unwrap()];
        assert!(!helper.contains(".send_whisper("));
        assert!(!helper.contains(".write_bytes("));
    }

    /// The session-report path never touches a terminal either: it only
    /// swaps router bindings and tells the outbox to hold or adopt mail.
    #[test]
    fn session_reporting_reaches_no_terminal_writer() {
        let source = include_str!("app.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        for name in ["fn report_native_session(", "fn apply_session_report("] {
            let helper = &production[production.find(name).unwrap()..];
            let helper = &helper[..helper.find("\n}\n").unwrap()];
            assert!(!helper.contains(".send_whisper("), "{name} must not paste");
            assert!(!helper.contains(".write_bytes("), "{name} must not paste");
            assert!(!helper.contains("println!"), "{name} stays out of stdout");
        }
    }

    /// F4 on a native OpenCode room runs exactly the startup intro delivery:
    /// the same house-rules text travels through the router, OpenCode accepts
    /// it, and each prompt asks for a turn. A second press with an unchanged
    /// roster resends the identical text, like the key handler does.
    #[cfg(unix)]
    #[test]
    fn f4_reintroduction_reaches_a_native_room_exactly_like_the_startup_intro() {
        use crate::native_launch::{self, tests::fake_room};
        use crate::native_route::{Backend, NativeRouter, RoomEntry};
        use std::time::{Duration, Instant};

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room(root.path(), "a", "v2.2.5");
        let room = native_launch::start(native_launch::NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1.1".to_owned(),
            1,
            &[],
        ))
        .unwrap();
        let entry = RoomEntry {
            index: 0,
            title: "Room A".to_owned(),
            binding: room.binding().clone(),
            backend: Backend::OpenCode(room.server()),
        };
        let path = root.path().join("state").join("native-outbox.json");
        let mut router = NativeRouter::start(path, vec![entry]).unwrap();
        let mut mailroom = Mailroom::new(50);

        let intro = house_rules(1, "Room A (model m, effort unsupported)", 0);
        for _ in 0..2 {
            let (notice, severity) =
                reintroduce_native_room("Room A", &mut router, &mut mailroom, 0, &intro);
            assert_eq!(notice, "Room A reintroduced to the room");
            assert_eq!(severity, NoticeSeverity::Info);
        }

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            router.tick(Instant::now(), &mut mailroom, false, false);
            let view = mailroom.summary();
            if view.matches("ACCEPTED by OpenCode").count() == 2 {
                break;
            }
            assert!(Instant::now() < deadline, "never accepted:\n{view}");
            std::thread::sleep(Duration::from_millis(20));
        }
        let received = fake.state.received.lock().unwrap().clone();
        let expected = crate::pane::whisper_note("The Crowded Room", &intro);
        assert_eq!(
            received,
            vec![
                (received[0].0.clone(), expected.clone()),
                (received[1].0.clone(), expected),
            ]
        );
        assert_ne!(received[0].0, received[1].0);
        assert_eq!(*fake.state.resumes.lock().unwrap(), [true, true]);
    }

    #[cfg(all(test, unix))]
    fn report_test_stub(root: &std::path::Path, name: &str, log: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        // As an interactive pane (no arguments) the stub records everything
        // written to the terminal, so a test can prove no report path ever
        // writes to a room. As `codex queue` it answers like the CLI.
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = queue ]; then\n  printf '%s\\n' \"$@\" >> '{}'\n  thread=\"\"\n  prev=\"\"\n  for a in \"$@\"; do\n    if [ \"$prev\" = \"--thread\" ]; then thread=\"$a\"; fi\n    prev=\"$a\"\n  done\n  printf 'Queued message sub_1 for thread %s.\\n' \"$thread\"\nelse\n  exec cat > '{}'\nfi\n",
                root.join(format!("{name}.args")).display(),
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(all(test, unix))]
    fn report_test_spec(
        program: &std::path::Path,
        cwd: &std::path::Path,
        title: &str,
        native: bool,
    ) -> RoomSpec {
        RoomSpec {
            name: "test".to_owned(),
            vendor: "test".to_owned(),
            title: title.to_owned(),
            program: program.as_os_str().to_owned(),
            args: Vec::new(),
            transport: crate::config::Transport::Raw,
            cwd: Some(cwd.to_path_buf()),
            variables: Vec::new(),
            allow_control: true,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
            native_delivery: native,
        }
    }

    #[cfg(all(test, unix))]
    fn report_test_pane(
        spec: RoomSpec,
        native: Option<crate::native_launch::NativeRoom>,
        codex: Option<&pane::CodexMint>,
    ) -> Pane {
        spawn_room(
            &mut LivePanes,
            spec,
            PtySize {
                rows: 6,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            },
            pane::GuestEnvironment::new([]),
            native,
            codex,
            None,
        )
        .unwrap()
    }

    #[cfg(all(test, unix))]
    fn apply_test_report(
        panes: &mut [Pane],
        router: Option<&mut NativeRouter>,
        from: usize,
        session: Option<&str>,
        pending: bool,
    ) -> String {
        let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
        let mut notice = Notices::default();
        report_native_session(
            panes,
            router,
            crate::doorbell::DoorbellSessionReport {
                from,
                session: session.map(str::to_owned),
                pending,
                reply,
            },
            &mut notice,
        );
        notice.ordered_lines().join("; ")
    }

    #[cfg(all(test, unix))]
    fn pump_router(router: &mut NativeRouter, mailroom: &mut Mailroom, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            router.tick(Instant::now(), mailroom, false, false);
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(all(test, unix))]
    fn report_test_opencode_room(
        root: &std::path::Path,
        cwd: &std::path::Path,
        session: &str,
    ) -> (
        Vec<Pane>,
        Option<NativeRouter>,
        crate::native_launch::tests::FakeRoom,
    ) {
        use crate::native_launch::{self, tests::fake_room};
        let program = report_test_stub(root, "opencode", &root.join("calls"));
        // The fake is named for the session the tests rebind onto: its
        // echo answers every prompt with ses_b.
        let fake = fake_room(root, "b", "v2.2.5");
        // Consume the launch generation a real startup would have taken, so
        // the report allocates the next one like production does.
        let _launch_generation =
            crate::native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
                .next()
                .unwrap();
        let native = native_launch::start(native_launch::NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd,
            "1.1".to_owned(),
            1,
            &[],
        ))
        .unwrap();
        let specs = vec![report_test_spec(&program, cwd, "Opencode · 1", true)];
        let server = native.server();
        // The room starts bound to the given session. Tests that need mail to
        // stay unsent use an id the fake server never echoes; tests that
        // deliver use the fake's own session.
        let entry = crate::native_route::RoomEntry {
            index: 0,
            title: "Opencode · 1".to_owned(),
            binding: crate::native_outbox::Binding {
                room: native.binding().room.clone(),
                session: session.to_owned(),
                endpoint: native.binding().endpoint.clone(),
                generation: 1,
            },
            backend: crate::native_route::Backend::OpenCode(server),
        };
        let path = root.join("state").join("native-outbox.json");
        let router = NativeRouter::start(path, vec![entry]).unwrap();
        let panes = vec![report_test_pane(
            specs.into_iter().next().unwrap(),
            Some(native),
            None,
        )];
        (panes, Some(router), fake)
    }

    #[cfg(all(test, unix))]
    #[test]
    fn a_pending_report_holds_new_mail_without_sending_or_pasting() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
        let mut mailroom = Mailroom::new(50);
        let notice = apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        assert!(notice.contains("session pending after clear"), "{notice}");
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "held", None)
            .unwrap();
        pump_router(router.as_mut().unwrap(), &mut mailroom, 300);
        let view = mailroom.summary();
        assert!(view.contains("HELD: session pending after clear"), "{view}");
        assert!(
            fake.state.received.lock().unwrap().is_empty(),
            "nothing is sent while pending"
        );
    }

    #[cfg(all(test, unix))]
    #[test]
    fn an_immediate_report_after_pending_delivers_once_paused() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
        let mut mailroom = Mailroom::new(50);
        // Pending, mail, then the new session id with no scheduling pass in
        // between: the hold is already on the mail when the rebind adopts it.
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "held", None)
            .unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        // Paused: nothing moves, but nothing is lost either.
        let end = Instant::now() + Duration::from_millis(300);
        while Instant::now() < end {
            router.as_mut().unwrap().tick(Instant::now(), &mut mailroom, true, false);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fake.state.received.lock().unwrap().is_empty());
        pump_router(router.as_mut().unwrap(), &mut mailroom, 500);
        let received = fake.state.received.lock().unwrap().clone();
        assert_eq!(received.len(), 1, "exactly one delivery: {received:?}");
        assert_eq!(received[0].1, "[whisper from Room X] held");
    }

    #[cfg(all(test, unix))]
    #[test]
    fn an_immediate_report_after_pending_delivers_once_fused() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
        let mut mailroom = Mailroom::new(50);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "held", None)
            .unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        let end = Instant::now() + Duration::from_millis(300);
        while Instant::now() < end {
            router.as_mut().unwrap().tick(Instant::now(), &mut mailroom, false, true);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(fake.state.received.lock().unwrap().is_empty());
        pump_router(router.as_mut().unwrap(), &mut mailroom, 500);
        let received = fake.state.received.lock().unwrap().clone();
        assert_eq!(received.len(), 1, "exactly one delivery: {received:?}");
        assert_eq!(received[0].1, "[whisper from Room X] held");
    }

    #[cfg(all(test, unix))]
    #[test]
    fn rapid_consecutive_reports_rebind_once_and_deliver_once() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
        let mut mailroom = Mailroom::new(50);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "new", None)
            .unwrap();
        let first = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        assert!(first.contains("now follows reported session ses_b"), "{first}");
        let second = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        assert!(second.contains("already on ses_b"), "{second}");
        let binding = router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(binding.generation, 2, "one rebind, one generation bump");
        // Wait for the delivery itself instead of a fixed budget, then give a
        // duplicate a moment to appear before asserting exactly once.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            router
                .as_mut()
                .unwrap()
                .tick(Instant::now(), &mut mailroom, false, false);
            if !fake.state.received.lock().unwrap().is_empty() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the report mail was never delivered:\n{}",
                mailroom.summary()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // A second delivery, if one were coming, would leave on a schedule
        // inside this window.
        std::thread::sleep(Duration::from_millis(400));
        router
            .as_mut()
            .unwrap()
            .tick(Instant::now(), &mut mailroom, false, false);
        let received = fake.state.received.lock().unwrap().clone();
        assert_eq!(received.len(), 1, "exactly one delivery: {received:?}");
        assert_eq!(received[0].1, "[whisper from Room X] new");
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
    }

    #[cfg(all(test, unix))]
    #[test]
    fn the_same_session_id_while_pending_releases_mail_without_a_new_generation() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, _fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut mailroom = Mailroom::new(50);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "held", None)
            .unwrap();
        pump_router(router.as_mut().unwrap(), &mut mailroom, 300);
        assert!(mailroom.summary().contains("HELD: session pending after clear"));
        let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        assert!(notice.contains("pending cleared"), "{notice}");
        pump_router(router.as_mut().unwrap(), &mut mailroom, 300);
        let view = mailroom.summary();
        assert!(view.contains("ACCEPTED by OpenCode"), "{view}");
        assert_eq!(
            router.as_ref().unwrap().binding_for(0).unwrap().generation,
            1,
            "the same id never bumps the generation"
        );
    }

    /// Mail saved before the pending signal belongs to the session that
    /// ended: an immediate report for that same session must find nothing to
    /// release, because no scheduling pass ran between the two commands.
    #[cfg(all(test, unix))]
    #[test]
    fn mail_saved_before_the_pending_signal_is_never_released_by_a_same_id_report() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut mailroom = Mailroom::new(50);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
        let received = fake.state.received.lock().unwrap().clone();
        assert!(
            received.is_empty(),
            "mail saved before the pending signal is never delivered: {received:?}"
        );
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
    }

    /// Two pending signals in a row still leave earlier mail held: the
    /// boundary is the first command, so a later report of the same session
    /// has nothing to release either.
    #[cfg(all(test, unix))]
    #[test]
    fn mail_saved_before_two_pending_reports_stays_held_when_the_room_returns() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut mailroom = Mailroom::new(50);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
        let received = fake.state.received.lock().unwrap().clone();
        assert!(
            received.is_empty(),
            "mail saved before the pending signal is never delivered: {received:?}"
        );
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
    }

    /// A pending signal followed by a different session keeps earlier mail
    /// held while the room moves on: the old session's mail is never resent
    /// to the new one.
    #[cfg(all(test, unix))]
    #[test]
    fn mail_saved_before_a_pending_signal_stays_held_when_a_new_session_arrives() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
        let mut mailroom = Mailroom::new(50);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        assert!(notice.contains("now follows reported session ses_b"), "{notice}");
        pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
        let received = fake.state.received.lock().unwrap().clone();
        assert!(
            received.is_empty(),
            "mail saved before the pending signal is never delivered: {received:?}"
        );
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
    }

    /// The pending report through the production handler while the outbox
    /// save fails: the old mail stays held, is never sent and is never
    /// marked accepted, even when the same session reports back at once.
    #[cfg(all(test, unix))]
    #[test]
    fn a_failed_pending_save_keeps_old_mail_held_through_the_handler() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut mailroom = Mailroom::new(50);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        // Drain the enqueue with delivery paused: seeing it queued proves the
        // save landed, and no schedule can have sent the mail yet.
        let settled = Instant::now() + Duration::from_secs(5);
        loop {
            router
                .as_mut()
                .unwrap()
                .tick(Instant::now(), &mut mailroom, true, false);
            if mailroom.summary().contains("QUEUED") {
                break;
            }
            assert!(Instant::now() < settled, "the enqueue never settled");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The state directory becomes a file, so every later save fails.
        let state_dir = root.path().join("state");
        std::fs::remove_dir_all(&state_dir).unwrap();
        std::fs::write(&state_dir, b"not a directory").unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        pump_router(router.as_mut().unwrap(), &mut mailroom, 400);
        let received = fake.state.received.lock().unwrap().clone();
        assert!(
            received.is_empty(),
            "old mail must never be sent while the hold is unsaved: {received:?}"
        );
        let view = mailroom.summary();
        assert!(view.contains("HELD"), "{view}");
        assert!(!view.contains("ACCEPTED"), "{view}");
    }

    /// Ticks until the outbox has answered every command sent so far, which
    /// is when nothing is still saving, collecting notices on the way.
    #[cfg(all(test, unix))]
    fn drain_answered(router: &mut Option<NativeRouter>, mailroom: &mut Mailroom) -> Vec<String> {
        let mut notices = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            notices.extend(
                router
                    .as_mut()
                    .unwrap()
                    .tick(Instant::now(), mailroom, true, false)
                    .into_iter()
                    .map(|(text, _)| text),
            );
            if !mailroom.summary().contains("SAVING") {
                return notices;
            }
            assert!(
                Instant::now() < deadline,
                "the outbox never answered:\n{}",
                mailroom.summary()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Two hold failures waiting for the next production iteration: one from
    /// the pending hold write, one from adopting the held mail once the disk
    /// is gone. The fake server stays alive for reports that rebind.
    #[cfg(all(test, unix))]
    fn rig_with_two_hold_failures(
        root: &std::path::Path,
        cwd: &std::path::Path,
    ) -> (ReportDispatch, crate::native_launch::tests::FakeRoom) {
        let (mut panes, mut router, fake) = report_test_opencode_room(root, cwd, "ses_b");
        let mut mailroom = Mailroom::new(50);
        let state_dir = root.join("state");
        let state_path = state_dir.join("native-outbox.json");

        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        // The pending report cannot write its hold...
        crate::native_outbox::fail_pending_hold_at(&state_path);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        // ...and the mail saved after it is held for the next session. Wait
        // for that save on disk before breaking anything, so there really is
        // held mail for the second failure to hit.
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "new", None)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let saved = std::fs::read_to_string(&state_path).unwrap_or_default();
            // The enqueue writes twice: once saved, then released onto the
            // pending hold. Wait for the second, or there is no held mail
            // for the adoption to fail on.
            if saved.contains("] new") && saved.contains("session pending after clear") {
                break;
            }
            assert!(Instant::now() < deadline, "the new mail was never saved");
            std::thread::sleep(Duration::from_millis(10));
        }
        // Now the disk goes away, so adopting that held mail fails too.
        std::fs::remove_dir_all(&state_dir).unwrap();
        std::fs::write(&state_dir, b"not a directory").unwrap();
        apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        // Give the outbox time to answer everything, so the one iteration
        // below has to carry both failures at once.
        std::thread::sleep(Duration::from_millis(1000));
        (ReportDispatch::new(panes, router), fake)
    }

    /// Two hold failures drained in one production iteration are both
    /// surfaced, in order: none is dropped because another arrived with it.
    #[cfg(all(test, unix))]
    #[test]
    fn two_hold_failures_in_one_iteration_are_both_surfaced() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
        let notice = rig.dispatch();
        assert_eq!(
            notice.matches("could not be held").count(),
            2,
            "both hold failures reach the UI in one iteration: {notice}"
        );
        assert!(
            notice.contains("the pending hold could not be written"),
            "the earlier failure is not dropped: {notice}"
        );
    }

    /// A same-session report processed after the failures adds its own line
    /// instead of replacing them.
    #[cfg(all(test, unix))]
    #[test]
    fn two_hold_failures_survive_a_same_session_report_in_one_iteration() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
        rig.push_report(0, Some("ses_b"), false);
        let notice = rig.dispatch();
        assert_eq!(
            notice.matches("could not be held").count(),
            2,
            "the report must not replace the failures: {notice}"
        );
        assert!(notice.contains("is already on ses_b"), "{notice}");
    }

    /// A rebind report processed after the failures keeps them too.
    #[cfg(all(test, unix))]
    #[test]
    fn two_hold_failures_survive_a_rebind_report_in_one_iteration() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
        rig.push_report(0, Some("ses-reported-9"), false);
        let notice = rig.dispatch();
        assert_eq!(
            notice.matches("could not be held").count(),
            2,
            "the rebind must not replace the failures: {notice}"
        );
        assert!(
            notice.contains("now follows reported session ses-reported-9"),
            "{notice}"
        );
    }

    /// A delivery status line processed after the failures keeps them too.
    #[cfg(all(test, unix))]
    #[test]
    fn two_hold_failures_survive_a_delivery_status_in_one_iteration() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
        // A message to the room itself: the delivery path writes its own
        // status line after the failures.
        rig.push_message(0, 0, "status line");
        let notice = rig.dispatch();
        assert_eq!(
            notice.matches("could not be held").count(),
            2,
            "the status must not replace the failures: {notice}"
        );
        assert!(
            notice.contains("saved for native delivery"),
            "a delivery status line is there too: {notice}"
        );
        assert!(
            notice.split("; ").count() >= 3,
            "failures and status are all present: {notice}"
        );
    }

    /// A failure notice survives an unknown-room report in the same
    /// iteration, and the report is still surfaced.
    #[cfg(all(test, unix))]
    #[test]
    fn a_failure_notice_survives_an_unknown_room_report_in_one_iteration() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
        rig.push_report(9, Some("ses-x"), false);
        let notice = rig.dispatch();
        assert_eq!(
            notice.matches("could not be held").count(),
            2,
            "the unknown room must not replace the failures: {notice}"
        );
        assert!(notice.contains("unknown room"), "{notice}");
    }

    /// One notice on its own is surfaced unchanged: no separator, nothing
    /// added, exactly the line the event produced.
    #[cfg(all(test, unix))]
    #[test]
    fn a_single_notice_is_surfaced_unchanged() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (panes, router, _fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut rig = ReportDispatch::new(panes, router);
        rig.push_report(0, Some("ses_b"), false);
        let notice = rig.dispatch();
        assert_eq!(
            notice, "Opencode · 1 is already on ses_b",
            "a lone notice is exactly as produced"
        );
    }

    /// Renders the production status line the way the UI does: the actual
    /// Paragraph, into an 80-column, one-row buffer.
    fn rendered_status(notices: &Notices) -> String {
        rendered_status_at(notices, 80)
    }

    /// Same as [`rendered_status`], at the given width.
    fn rendered_status_at(notices: &Notices, width: u16) -> String {
        use ratatui::buffer::Buffer;
        use ratatui::widgets::Widget;
        let area = ratatui::layout::Rect::new(0, 0, width, 1);
        let text = notices
            .status_line(area.width)
            .expect("a notice to show");
        let mut buffer = Buffer::empty(area);
        Widget::render(Paragraph::new(text), area, &mut buffer);
        (0..area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>()
    }

    #[test]
    fn two_hold_failures_render_a_count_and_a_failure_at_eighty_columns() {
        let mut notices = Notices::default();
        notices.push(
            "native delivery: Opencode · 1 could not be held: the pending hold could not be written"
                .to_owned(),
            NoticeSeverity::Failure,
        );
        notices.push(
            "native delivery: Opencode · 1 could not be held: Not a directory (os error 20)"
                .to_owned(),
            NoticeSeverity::Failure,
        );
        let rendered = rendered_status(&notices);
        assert!(rendered.starts_with("2 failures: "), "{rendered}");
        assert!(rendered.contains("could not be held"), "{rendered}");
    }

    #[test]
    fn a_long_routine_notice_does_not_hide_a_new_failure() {
        let mut notices = Notices::default();
        notices.push(
            format!(
                "Opencode · 1 is already on ses_b — {}",
                "x".repeat(260)
            ),
            NoticeSeverity::Info,
        );
        notices.push(
            "native delivery: Opencode · 1 could not be held: no space left on the device"
                .to_owned(),
            NoticeSeverity::Failure,
        );
        let rendered = rendered_status(&notices);
        assert!(rendered.starts_with("1 failure: "), "{rendered}");
        assert!(rendered.contains("could not be held"), "{rendered}");
        assert!(
            !rendered.starts_with("Opencode · 1 is already"),
            "the prior routine status must not lead: {rendered}"
        );
    }

    #[test]
    fn one_plain_notice_renders_exactly_as_before() {
        let text = "Opencode · 1 is already on ses_b";
        let mut notices = Notices::default();
        notices.push(text.to_owned(), NoticeSeverity::Info);
        assert_eq!(notices.status_line(80).as_deref(), Some(text));
        let rendered = rendered_status(&notices);
        assert!(rendered.starts_with(text), "{rendered}");
        assert!(!rendered.contains("failure"), "{rendered}");
        assert!(!rendered.contains("notices"), "{rendered}");
    }

    #[test]
    fn one_hundred_reports_stay_bounded_and_show_the_newest_status() {
        let mut notices = Notices::default();
        let mut newest = String::new();
        for index in 0..100 {
            newest = format!("Opencode · 1 is already on ses_b ({index})");
            notices.push(newest.clone(), NoticeSeverity::Info);
        }
        assert!(
            notices.entries.len() <= MAX_NOTICES,
            "the list stays bounded: {}",
            notices.entries.len()
        );
        assert_eq!(
            notices.dropped,
            100 - MAX_NOTICES,
            "the oldest are counted, not forgotten"
        );
        let rendered = rendered_status(&notices);
        assert!(
            rendered.contains(&newest),
            "the newest status is visible: {rendered}"
        );
    }

    #[test]
    fn a_routine_status_never_displaces_a_failure_when_the_list_is_full() {
        let mut notices = Notices::default();
        for index in 0..MAX_NOTICES {
            notices.push(
                format!(
                    "native delivery: room {index} could not be held: no space left"
                ),
                NoticeSeverity::Failure,
            );
        }
        notices.push(
            "Opencode · 1 is already on ses_b".to_owned(),
            NoticeSeverity::Info,
        );
        assert_eq!(
            notices
                .entries
                .iter()
                .filter(|entry| entry.severity == NoticeSeverity::Failure)
                .count(),
            MAX_NOTICES,
            "every slot still holds a failure"
        );
        assert_eq!(notices.dropped, 1, "the routine status was the one dropped");
        let rendered = rendered_status(&notices);
        assert!(
            rendered.starts_with(&format!("{MAX_NOTICES} failures: ")),
            "{rendered}"
        );
    }

    /// The one-row status line stays actionable at every width: a count and
    /// the newest failure at 80 and 40 columns, and at 20 columns a count
    /// plus an explicit mailroom pointer instead of a bare count.
    #[test]
    fn status_line_stays_actionable_at_eighty_forty_and_twenty_columns() {
        let mut notices = Notices::default();
        notices.push(
            "Opencode · 1 is already on ses_b".to_owned(),
            NoticeSeverity::Info,
        );
        notices.push(
            "native delivery: Opencode · 2 could not be held: no space left on the device"
                .to_owned(),
            NoticeSeverity::Failure,
        );
        let wide = rendered_status_at(&notices, 80);
        assert!(wide.starts_with("1 failure: "), "{wide}");
        assert!(wide.contains("could not be held"), "{wide}");
        let mid = rendered_status_at(&notices, 40);
        assert!(mid.starts_with("1 failure: "), "{mid}");
        assert!(mid.contains("native delivery"), "{mid}");
        let narrow = rendered_status_at(&notices, 20);
        assert!(narrow.starts_with("1 failure: "), "{narrow}");
        assert!(
            narrow.contains("see mail") || narrow.contains("could not be held"),
            "at 20 columns the row names a reason or points at the mailroom: {narrow}"
        );
        assert!(
            !narrow.trim_end_matches('…').ends_with(':'),
            "never just a bare count: {narrow}"
        );
    }

    /// Renders the production mailroom popup the way the UI does, through
    /// the same render helper the event loop calls, into the real 80x24
    /// layout.
    fn rendered_popup(
        mailroom: &Mailroom,
        notices: &Notices,
        scroll: u16,
    ) -> Vec<String> {
        use ratatui::buffer::Buffer;
        let area = ratatui::layout::Rect::new(0, 0, 80, 24);
        let popup = mail_popup_area(area);
        let mut buffer = Buffer::empty(area);
        render_mail_popup(popup, &mut buffer, mailroom, notices, scroll);
        (popup.y..popup.y + popup.height)
            .map(|y| {
                (popup.x..popup.x + popup.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// With 20 queued envelopes present, every failure is reachable: the
    /// popup lists failures first and scrolls, so no failure hides behind
    /// the envelopes.
    #[test]
    fn mailroom_popup_lists_failures_first_and_scrolls_past_envelopes() {
        let mut mailroom = Mailroom::new(100);
        for _ in 0..20 {
            mailroom.queue("Room A".to_owned(), "Room B".to_owned(), "hi".to_owned(), "room busy");
        }
        let mut notices = Notices::default();
        notices.push("Doorbell envelope #0001 injected".to_owned(), NoticeSeverity::Info);
        for index in 0..3 {
            notices.push(
                format!("native delivery: Room {index} could not be held: disk {index} full"),
                NoticeSeverity::Failure,
            );
        }
        let text = mail_popup_text(&mailroom, &notices);
        let first_failure = text.find("could not be held").expect("a failure in the text");
        let first_info = text.find("Doorbell envelope").expect("the info in the text");
        assert!(
            first_failure < first_info,
            "failures sort before routine statuses"
        );
        let top: String = rendered_popup(&mailroom, &notices, 0).join("\n");
        assert!(
            top.contains("could not be held"),
            "a failure is visible without scrolling: {top}"
        );
        for index in 0..3 {
            let needle = format!("disk {index} full");
            let mut seen = false;
            for scroll in (0..60).step_by(5) {
                let view: String = rendered_popup(&mailroom, &notices, scroll).join("\n");
                if view.contains(&needle) {
                    seen = true;
                    break;
                }
            }
            assert!(seen, "failure {index} is reachable by scrolling");
        }
    }

    /// F2 opens the Mailroom without discarding anything: the failures the
    /// status row pointed at are still in the history and visible in the
    /// production popup render after the real toggle transition.
    #[test]
    fn opening_the_mailroom_keeps_failures_visible_in_the_popup() {
        let mut mailroom = Mailroom::new(100);
        for _ in 0..20 {
            mailroom.queue("Room A".to_owned(), "Room B".to_owned(), "hi".to_owned(), "room busy");
        }
        let mut notices = Notices::default();
        notices.push(
            "native delivery: Room 0 could not be held: disk full".to_owned(),
            NoticeSeverity::Failure,
        );
        notices.push(
            "native delivery: Room 1 could not be held: disk full".to_owned(),
            NoticeSeverity::Failure,
        );
        notices.push("Doorbell envelope #0001 injected".to_owned(), NoticeSeverity::Info);
        let mut mode = InputMode::Normal;
        let mut scroll = 7;
        toggle_mail_log(&mut mode, &mut scroll, &mut notices);
        assert!(matches!(mode, InputMode::MailLog));
        assert_eq!(scroll, 0, "opening resets the scroll");
        assert_eq!(notices.entries.len(), 3, "opening keeps the history");
        let view: String = rendered_popup(&mailroom, &notices, scroll).join("\n");
        assert!(
            view.contains("Room 0 could not be held") && view.contains("Room 1 could not be held"),
            "both failures are visible after the F2 press: {view}"
        );
    }

    /// Leaving the Mailroom by the same toggle dismisses the history: the
    /// status row is empty afterwards, like the other view changes that
    /// clear (Tab, whisper, settings).
    #[test]
    fn closing_the_mailroom_dismisses_the_history() {
        let mut notices = Notices::default();
        notices.push(
            "native delivery: Room 0 could not be held: disk full".to_owned(),
            NoticeSeverity::Failure,
        );
        let mut mode = InputMode::MailLog;
        let mut scroll = 4;
        toggle_mail_log(&mut mode, &mut scroll, &mut notices);
        assert!(matches!(mode, InputMode::Normal));
        assert!(notices.entries.is_empty(), "dismissing clears the history");
        assert_eq!(notices.status_line(80), None);
    }

    /// A repeated routine status renders how many times it arrived, in the
    /// status row and in the popup history.
    #[test]
    fn repeated_routine_status_renders_its_repeat_count() {
        let text = "Opencode · 1 is already on ses_b";
        let mut notices = Notices::default();
        for _ in 0..3 {
            notices.push(text.to_owned(), NoticeSeverity::Info);
        }
        assert_eq!(notices.entries.len(), 1);
        assert_eq!(
            notices.status_line(80).as_deref(),
            Some("1 notices: Opencode · 1 is already on ses_b (x3)")
        );
        assert!(
            notices.ordered_lines().iter().any(|line| line.ends_with("(x3)")),
            "the popup keeps the count"
        );
    }

    /// A repeated failure renders how many times it arrived.
    #[test]
    fn repeated_failure_renders_its_repeat_count() {
        let text = "native delivery: Room 0 could not be held: disk full";
        let mut notices = Notices::default();
        for _ in 0..3 {
            notices.push(text.to_owned(), NoticeSeverity::Failure);
        }
        assert_eq!(
            notices.status_line(80).as_deref(),
            Some("1 failure: native delivery: Room 0 could not be held: disk full (x3)")
        );
    }

    /// A successful queued delivery is a routine status: `0 failed` never
    /// counts as a failure, so 16 zero-failed statuses cannot displace a
    /// real failure. Everything here runs through the production iteration.
    #[cfg(all(test, unix))]
    #[test]
    fn zero_failed_queued_delivery_is_info_and_never_displaces_a_failure() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log = root.path().join("writes");
        let stub = report_test_stub(root.path(), "room", &log);
        let offline = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room A", false),
            None,
            Some(&pane::CodexMint::Failed("gone".to_owned())),
        );
        let online = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room B", false),
            None,
            None,
        );
        let mut rig = ReportDispatch::new(vec![offline, online], None);
        let old = || Instant::now() - Duration::from_secs(60);
        rig.last_output = vec![Some(old()), Some(old())];
        // Sixteen distinct delivery failures through the production route.
        for index in 0..16 {
            rig.push_message(1, 0, &format!("poke {index}"));
            rig.dispatch_keep();
        }
        assert_eq!(rig.notice.entries.len(), 16, "sixteen failures are kept");
        assert!(
            rig.notice
                .entries
                .iter()
                .all(|entry| entry.severity == NoticeSeverity::Failure),
            "every seeded notice is a failure"
        );
        assert_eq!(rig.notice.dropped, 0);
        // One queued status while paused, then one zero-failed delivery
        // status once the room injects it.
        rig.paused = true;
        rig.push_message(0, 1, "hello");
        rig.dispatch_keep();
        rig.paused = false;
        rig.last_output = vec![Some(old()), Some(old())];
        rig.dispatch_keep();
        assert_eq!(rig.notice.entries.len(), 16, "still bounded");
        assert!(
            rig.notice
                .entries
                .iter()
                .all(|entry| entry.severity == NoticeSeverity::Failure),
            "no routine status displaced a failure"
        );
        assert_eq!(rig.notice.dropped, 2, "both routine statuses were dropped");
        assert!(
            !rig
                .notice
                .entries
                .iter()
                .any(|entry| entry.text.contains("Queued delivery")),
            "a zero-failed status was recorded as routine, not as a failure"
        );
    }

    /// A queued delivery with a real failure is a failure.
    #[cfg(all(test, unix))]
    #[test]
    fn failed_queued_delivery_is_a_failure() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log = root.path().join("writes");
        let stub = report_test_stub(root.path(), "room", &log);
        let offline = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room A", false),
            None,
            Some(&pane::CodexMint::Failed("gone".to_owned())),
        );
        let mut rig = ReportDispatch::new(vec![offline], None);
        rig.last_output = vec![Some(Instant::now() - Duration::from_secs(60))];
        rig.paused = true;
        rig.push_message(0, 0, "hello");
        rig.dispatch_keep();
        assert!(
            rig.notice
                .entries
                .iter()
                .any(|entry| entry.severity == NoticeSeverity::Info
                    && entry.text.contains("queued")),
            "the queued status is routine"
        );
        rig.paused = false;
        rig.last_output = vec![Some(Instant::now() - Duration::from_secs(60))];
        rig.dispatch_keep();
        let last = rig.notice.entries.last().expect("a delivery status");
        assert!(last.text.contains("1 failed"), "{last:?}");
        assert_eq!(last.severity, NoticeSeverity::Failure);
    }

    /// The capitalized intro failure is a failure, not a routine status.
    #[cfg(all(test, unix))]
    #[test]
    fn capitalised_teach_failure_is_a_failure() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log = root.path().join("writes");
        let stub = report_test_stub(root.path(), "room", &log);
        let offline = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room A", false),
            None,
            Some(&pane::CodexMint::Failed("gone".to_owned())),
        );
        let mut rig = ReportDispatch::new(vec![offline], None);
        rig.gates[0] = DeliveryGate::new(true);
        rig.spawned_at[0] = Instant::now() - Duration::from_secs(3600);
        rig.dispatch_keep();
        let only = rig.notice.entries.last().expect("an intro notice");
        assert!(only.text.starts_with("Could not teach"), "{only:?}");
        assert_eq!(only.severity, NoticeSeverity::Failure);
    }

    /// Hold failures are failures; same-session and rebind statuses are
    /// routine; an unknown-room report is a failure. All through the
    /// production iteration.
    #[cfg(all(test, unix))]
    #[test]
    fn session_notices_carry_producer_severity() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut rig, _fake) = rig_with_two_hold_failures(root.path(), cwd.path());
        rig.dispatch_keep();
        let holds: Vec<_> = rig
            .notice
            .entries
            .iter()
            .filter(|entry| entry.text.contains("could not be held"))
            .collect();
        assert_eq!(holds.len(), 2, "both hold failures reach the UI");
        assert!(
            holds
                .iter()
                .all(|entry| entry.severity == NoticeSeverity::Failure),
            "hold failures are failures"
        );
        rig.push_report(0, Some("ses_b"), false);
        rig.dispatch_keep();
        assert!(
            rig.notice
                .entries
                .iter()
                .any(|entry| entry.severity == NoticeSeverity::Info
                    && entry.text.contains("ses_b")),
            "a same-session status is routine"
        );
        rig.push_report(9, Some("ses-x"), false);
        rig.dispatch_keep();
        let last = rig.notice.entries.last().expect("an unknown-room notice");
        assert!(last.text.contains("unknown room"), "{last:?}");
        assert_eq!(last.severity, NoticeSeverity::Failure);
    }

    /// Native doorbell outcomes carry their severity: accepted mail is
    /// routine, a refused send is a failure.
    #[cfg(all(test, unix))]
    #[test]
    fn deliver_native_paths_carry_their_severity() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log = root.path().join("writes");
        let stub = report_test_stub(root.path(), "room", &log);
        // No router: the send is refused.
        let mut panes = vec![report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room A", false),
            None,
            Some(&pane::CodexMint::Failed("gone".to_owned())),
        )];
        let mut mailroom = Mailroom::new(50);
        let mut fuse = DeliveryFuse::new(100);
        let mut paused = false;
        let mut notice = Notices::default();
        deliver_native_doorbell(
            &mut panes,
            None,
            &mut mailroom,
            "Room X",
            "Room A",
            0,
            "hi",
            None,
            &mut fuse,
            &mut paused,
            &mut notice,
        );
        let refused = notice.entries.last().expect("a refusal notice");
        assert!(refused.text.contains("not saved"), "{refused:?}");
        assert_eq!(refused.severity, NoticeSeverity::Failure);
        // A live router accepts the same mail.
        let (mut panes, mut router, _fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let target = panes[0].title().to_owned();
        let mut notice = Notices::default();
        deliver_native_doorbell(
            &mut panes,
            router.as_mut(),
            &mut mailroom,
            "Room X",
            &target,
            0,
            "hi",
            None,
            &mut fuse,
            &mut paused,
            &mut notice,
        );
        let saved = notice.entries.last().expect("a saved notice");
        assert!(saved.text.contains("saved for native delivery"), "{saved:?}");
        assert_eq!(saved.severity, NoticeSeverity::Info);
    }

    /// A reintroduction that the router refuses is a failure (the accepted
    /// arm is covered by the F4 test asserting `Info`).
    #[cfg(all(test, unix))]
    #[test]
    fn refused_reintroduction_is_a_failure() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (_panes, mut router, _fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut mailroom = Mailroom::new(50);
        let (text, severity) =
            reintroduce_native_room("Ghost", router.as_mut().unwrap(), &mut mailroom, 7, "hi");
        assert!(text.contains("does not use native delivery"), "{text}");
        assert_eq!(severity, NoticeSeverity::Failure);
    }

    /// Peer control outcomes carry their severity: an applied control is
    /// routine, a rejected one is a failure.
    #[cfg(all(test, unix))]
    #[test]
    fn control_outcomes_carry_their_severity() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        use crate::doorbell::{DoorbellControl, DoorbellEvent};
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log = root.path().join("writes");
        // Named `codex` so the room has a control adapter and can resume.
        let stub = report_test_stub(root.path(), "codex", &log);
        let first = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room A", false),
            None,
            None,
        );
        let second = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room B", false),
            None,
            None,
        );
        let mut rig = ReportDispatch::new(vec![first, second], None);
        let old = || Instant::now() - Duration::from_secs(60);
        rig.last_output = vec![Some(old()), Some(old())];
        let push_control = |rig: &mut ReportDispatch| {
            let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
            rig.doorbell.events.push_back(DoorbellEvent::Control(DoorbellControl {
                from: 0,
                to: 1,
                action: crate::doorbell::ControlAction::Resume,
                reply,
            }));
        };
        push_control(&mut rig);
        rig.dispatch_keep();
        let told = rig.notice.entries.last().expect("a control notice");
        assert!(told.text.contains("told"), "{told:?}");
        assert_eq!(told.severity, NoticeSeverity::Info);
        rig.panes[1].set_allow_control(false);
        push_control(&mut rig);
        rig.dispatch_keep();
        let rejected = rig.notice.entries.last().expect("a rejection notice");
        assert!(rejected.text.contains("rejected"), "{rejected:?}");
        assert_eq!(rejected.severity, NoticeSeverity::Failure);
    }

    /// Fuse notices are failures; an ordinary injected envelope is routine.
    /// The room runs a `sleep` stub: it stays online and takes writes, but
    /// never produces output, so readiness never flips mid-test.
    #[cfg(all(test, unix))]
    #[test]
    fn fuse_notices_are_failures_and_injected_is_routine() {
        use std::os::unix::fs::PermissionsExt;
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let sleep = root.path().join("room");
        std::fs::write(&sleep, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&sleep, std::fs::Permissions::from_mode(0o700)).unwrap();
        let online = report_test_pane(
            report_test_spec(&sleep, cwd.path(), "Room B", false),
            None,
            None,
        );
        let mut rig = ReportDispatch::new(vec![online], None);
        let old = || Instant::now() - Duration::from_secs(60);
        rig.last_output = vec![Some(old())];
        rig.push_message(0, 0, "first");
        rig.dispatch_keep();
        let injected = rig.notice.entries.last().expect("an injected notice");
        assert!(injected.text.contains("injected"), "{injected:?}");
        assert_eq!(injected.severity, NoticeSeverity::Info);
        // One injection trips a fuse of one; the trip notice is a failure.
        rig.fuse = DeliveryFuse::new(1);
        rig.paused = true;
        rig.push_message(0, 0, "second");
        rig.push_message(0, 0, "third");
        rig.dispatch_keep();
        rig.paused = false;
        rig.last_output = vec![Some(old())];
        rig.gates[0] = DeliveryGate::new(false);
        // The pty echoes the first delivery back on its own schedule; wait
        // until the writer is idle and two passes leave the clock untouched,
        // so no late echo can flip readiness during the inject pass below.
        rig.paused = true;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut quiet_rounds = 0u32;
        while quiet_rounds < 2 {
            assert!(Instant::now() < deadline, "the pty echo never settled");
            rig.last_output = vec![Some(old())];
            let before = rig.last_output[0];
            rig.dispatch_keep();
            if !rig.panes[0].has_unwritten_input() && rig.last_output[0] == before {
                quiet_rounds += 1;
            } else {
                quiet_rounds = 0;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        rig.paused = false;
        rig.gates[0] = DeliveryGate::new(false);
        rig.dispatch_keep();
        let trip = rig.notice.entries.last().expect("a fuse notice");
        assert!(trip.text.contains("fuse tripped"), "{trip:?}");
        assert_eq!(trip.severity, NoticeSeverity::Failure);
    }

    /// A mint that failed to start the room is a failure.
    #[cfg(all(test, unix))]
    #[test]
    fn failed_mint_is_a_failure() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log = root.path().join("writes");
        let stub = report_test_stub(root.path(), "room", &log);
        let online = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room B", false),
            None,
            None,
        );
        let mut rig = ReportDispatch::new(vec![online], None);
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        rig.codex[0] = Some(pane::CodexMint::Pending(rx));
        rig.dispatch_keep();
        let mint = rig
            .notice
            .entries
            .iter()
            .find(|entry| entry.text.contains("was not started"))
            .expect("a mint notice");
        assert_eq!(mint.severity, NoticeSeverity::Failure);
    }

    /// A rejected key is a failure, chosen where the text is produced.
    #[cfg(all(test, unix))]
    #[test]
    fn forward_key_rejection_is_a_failure() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();

        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let log = root.path().join("writes");
        let stub = report_test_stub(root.path(), "room", &log);
        let mut pane = report_test_pane(
            report_test_spec(&stub, cwd.path(), "Room A", false),
            None,
            Some(&pane::CodexMint::Failed("gone".to_owned())),
        );
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('x'),
            crossterm::event::KeyModifiers::NONE,
        );
        let (text, severity) = forward_key(&mut pane, key).expect("a rejection");
        assert!(text.contains("rejected input"), "{text}");
        assert_eq!(severity, NoticeSeverity::Failure);
    }

    /// A pending-hold write failure belongs to the pending command: it shows
    /// up as a notice naming the room, and the mail queued around it keeps
    /// its own result instead of inheriting that failure.
    #[cfg(all(test, unix))]
    #[test]
    fn a_pending_save_failure_is_reported_as_its_own_failure() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut mailroom = Mailroom::new(50);
        let state_path = root.path().join("state").join("native-outbox.json");

        // Old mail, the pending report, then new mail with no pass in
        // between: the outbox answers the three commands in that order.
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        crate::native_outbox::fail_pending_hold_at(&state_path);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "new", None)
            .unwrap();

        let notices = drain_answered(&mut router, &mut mailroom);
        let view = mailroom.summary();
        assert!(
            !view.contains("FAILED"),
            "both messages keep their own result: {view}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("could not be held")
                    && notice.contains("Opencode · 1")),
            "the pending failure is a notice naming the room: {notices:?}"
        );

        // The new message was really saved, so clearing the mark delivers it,
        // while the mail from before the mark stays held.
        apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        pump_router(router.as_mut().unwrap(), &mut mailroom, 500);
        let received = fake.state.received.lock().unwrap().clone();
        assert_eq!(
            received.len(),
            1,
            "only the new message is sent: {received:?}"
        );
        assert!(received[0].1.contains("new"), "{received:?}");
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
    }

    /// Two pending reports with one failing write: the failure is reported
    /// once, the second report succeeds, and the room still accepts mail.
    #[cfg(all(test, unix))]
    #[test]
    fn two_pending_reports_with_one_failing_are_each_accounted_for() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, _fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let mut mailroom = Mailroom::new(50);
        let state_path = root.path().join("state").join("native-outbox.json");

        // Old mail gives the first report a hold to write.
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        crate::native_outbox::fail_pending_hold_at(&state_path);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        apply_test_report(&mut panes, router.as_mut(), 0, None, true);
        // Its own result proves the two reports above were both answered.
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "later", None)
            .unwrap();

        let notices = drain_answered(&mut router, &mut mailroom);
        let view = mailroom.summary();
        assert!(
            !view.contains("FAILED"),
            "the second report and the later mail keep their own result: {view}"
        );
        assert_eq!(
            notices
                .iter()
                .filter(|notice| notice.contains("could not be held"))
                .count(),
            1,
            "exactly one pending failure is reported: {notices:?}"
        );
        assert!(
            notices
                .iter()
                .any(|notice| notice.contains("Opencode · 1")),
            "the failure names the room: {notices:?}"
        );
        assert!(
            send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "after", None)
                .is_ok(),
            "a reported failure must not stop the room from accepting mail"
        );
    }

    /// Waits until a terminal's log stops changing: every write the pane
    /// accepted has reached its child by then, so a settled log is complete.
    #[cfg(all(test, unix))]
    fn settled_log(path: &std::path::Path) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut previous = std::fs::read(path).unwrap_or_default();
        let mut settled_at = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(50));
            let current = std::fs::read(path).unwrap_or_default();
            if current != previous {
                previous = current;
                settled_at = Instant::now();
            } else if !previous.is_empty() && settled_at.elapsed() >= Duration::from_millis(400) {
                return previous;
            }
            assert!(
                Instant::now() < deadline,
                "the terminal log never settled: {previous:?}"
            );
        }
    }

    /// The real event-loop pass, driven from a test. Reports enter as the
    /// doorbell events production receives, so the `SessionReport` arm itself
    /// runs for every case a test pushes.
    #[cfg(all(test, unix))]
    struct ReportDispatch {
        doorbell: ReportEvents,
        panes: Vec<Pane>,
        router: Option<NativeRouter>,
        mailroom: Mailroom,
        gates: Vec<DeliveryGate>,
        last_output: Vec<Option<Instant>>,
        spawned_at: Vec<Instant>,
        pulses: Vec<Option<PulseSample>>,
        details: Vec<RoomDetail>,
        pending: std::collections::VecDeque<(u64, usize)>,
        submits: std::collections::VecDeque<(Instant, usize, bool)>,
        fuse: DeliveryFuse,
        paused: bool,
        notice: Notices,
        input_ready_out: Vec<bool>,
        codex: Vec<Option<pane::CodexMint>>,
        sizes: Vec<PtySize>,
        environments: Vec<pane::GuestEnvironment>,
    }

    /// Queues doorbell events for [`ReportDispatch`] to drain exactly like
    /// the production loop drains them.
    #[cfg(all(test, unix))]
    struct ReportEvents {
        events: std::collections::VecDeque<DoorbellEvent>,
    }

    #[cfg(all(test, unix))]
    impl DoorbellSource for ReportEvents {
        fn try_recv(&mut self) -> Result<DoorbellEvent, std::sync::mpsc::TryRecvError> {
            self.events
                .pop_front()
                .ok_or(std::sync::mpsc::TryRecvError::Empty)
        }

        fn path(&self) -> &std::path::Path {
            std::path::Path::new("test-doorbell.sock")
        }
    }

    #[cfg(all(test, unix))]
    impl ReportDispatch {
        fn new(panes: Vec<Pane>, router: Option<NativeRouter>) -> Self {
            let now = Instant::now();
            let rooms = panes.len();
            Self {
                doorbell: ReportEvents {
                    events: std::collections::VecDeque::new(),
                },
                gates: (0..rooms)
                    .map(|_| DeliveryGate::new(false))
                    .collect(),
                last_output: vec![None; rooms],
                spawned_at: vec![now; rooms],
                pulses: vec![None; rooms],
                details: (0..rooms).map(|_| RoomDetail::default()).collect(),
                pending: std::collections::VecDeque::new(),
                submits: std::collections::VecDeque::new(),
                fuse: DeliveryFuse::new(0),
                paused: false,
                notice: Notices::default(),
                input_ready_out: Vec::new(),
                codex: (0..rooms).map(|_| None).collect(),
                sizes: (0..rooms)
                    .map(|_| PtySize {
                        rows: 6,
                        cols: 60,
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .collect(),
                environments: (0..rooms)
                    .map(|_| pane::GuestEnvironment::new([]))
                    .collect(),
                mailroom: Mailroom::new(50),
                panes,
                router,
            }
        }

        /// Queues one session report exactly as the doorbell hands it over.
        fn push_report(&mut self, from: usize, session: Option<&str>, pending: bool) {
            let (reply, _dropped) = std::sync::mpsc::sync_channel(1);
            self.doorbell
                .events
                .push_back(DoorbellEvent::SessionReport(
                    crate::doorbell::DoorbellSessionReport {
                        from,
                        session: session.map(str::to_owned),
                        pending,
                        reply,
                    },
                ));
        }

        /// Queues one doorbell message exactly as the doorbell hands it over.
        fn push_message(&mut self, from: usize, to: usize, body: &str) {
            let (mut envelope, _answers) = crate::doorbell::DoorbellEnvelope::for_test();
            envelope.from = from;
            envelope.to = to;
            envelope.body = body.to_owned();
            self.doorbell.events.push_back(DoorbellEvent::Message(envelope));
        }

        /// Runs one event-loop pass and returns the notice the dispatch left.
        fn dispatch(&mut self) -> String {
            let rooms = self.panes.len();
            let now = Instant::now();
            run_rooms_iteration(
                &mut self.doorbell,
                None::<&mut Terminal<CrosstermBackend<io::Stdout>>>,
                &mut self.panes,
                &mut self.router,
                &mut self.mailroom,
                &mut self.gates,
                &mut self.last_output,
                &mut self.spawned_at,
                &mut self.pulses,
                &mut self.details,
                &mut self.pending,
                &mut self.submits,
                &mut self.fuse,
                &mut self.paused,
                &mut self.notice,
                &mut self.input_ready_out,
                &mut self.codex,
                &self.sizes,
                &self.environments,
                &[],
                100,
                rooms,
                now,
            )
            .unwrap();
            std::mem::take(&mut self.notice).ordered_lines().join("; ")
        }

        /// Runs one event-loop pass like [`ReportDispatch::dispatch`], but
        /// keeps the notices so a test can check each one's severity.
        fn dispatch_keep(&mut self) {
            let rooms = self.panes.len();
            let now = Instant::now();
            run_rooms_iteration(
                &mut self.doorbell,
                None::<&mut Terminal<CrosstermBackend<io::Stdout>>>,
                &mut self.panes,
                &mut self.router,
                &mut self.mailroom,
                &mut self.gates,
                &mut self.last_output,
                &mut self.spawned_at,
                &mut self.pulses,
                &mut self.details,
                &mut self.pending,
                &mut self.submits,
                &mut self.fuse,
                &mut self.paused,
                &mut self.notice,
                &mut self.input_ready_out,
                &mut self.codex,
                &self.sizes,
                &self.environments,
                &[],
                100,
                rooms,
                now,
            )
            .unwrap();
        }
    }

    /// A Claude room's own report is the only other source of its session: a
    /// valid id rebinds under a new generation and is saved as reported; a
    /// non-UUID is ignored.
    #[cfg(all(test, unix))]
    #[test]
    fn a_claude_session_report_rebinds_and_a_non_uuid_is_ignored() {
        const FIRST: &str = "6a1d2f74-1f52-4e0f-9d61-0a1b2c3d4e5f";
        const SECOND: &str = "7b2e3a85-2a63-4f10-8e72-1b2c3d4e5f60";
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let program = report_test_stub(root.path(), "claude", &root.path().join("calls"));
        let spec = report_test_spec(&program, cwd.path(), "Claude · 1", true);
        let size = PtySize {
            rows: 6,
            cols: 60,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut panes = vec![
            spawn_room(
                &mut LivePanes,
                spec,
                size,
                pane::GuestEnvironment::new([]),
                None,
                None,
                None,
            )
            .unwrap(),
        ];
        let mut router = NativeRouter::start(root.path().join("outbox.json"), Vec::new()).unwrap();
        pane::bind_claude_room(&mut router, &mut panes, 0, FIRST).unwrap();
        let old = router.binding_for(0).unwrap();
        let mut rig = ReportDispatch::new(panes, Some(router));

        let shown = {
            rig.push_report(0, Some("not-a-uuid"), false);
            rig.dispatch()
        };
        assert!(shown.contains("not a Claude session id"), "{shown}");
        assert_eq!(
            rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
            FIRST
        );

        rig.push_report(0, Some(SECOND), false);
        rig.dispatch();
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(binding.session, SECOND);
        assert!(binding.generation > old.generation);
        assert_eq!(
            crate::pane::session_state::lookup_reported("claude", cwd.path(), "Claude · 1")
                .as_deref(),
            Some(SECOND)
        );
    }

    #[cfg(all(test, unix))]
    #[test]
    fn session_reports_never_touch_a_terminal() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // Room 0 is a bound native OpenCode room; room 1 is an ordinary
        // Claude room with a live terminal of its own. Both record every byte
        // written to them, so any terminal write is caught.
        let native_log = root.path().join("native.pty");
        let raw_log = root.path().join("raw.pty");
        let native_program = report_test_stub(root.path(), "opencode", &native_log);
        let raw_program = report_test_stub(root.path(), "claude", &raw_log);
        let fake = {
            use crate::native_launch::tests::fake_room;
            fake_room(root.path(), "server", "v2.2.5")
        };
        let mut specs = vec![
            report_test_spec(&native_program, cwd.path(), "Opencode · 1", true),
            report_test_spec(&raw_program, cwd.path(), "Claude · 2", false),
        ];
        let native = {
            use crate::native_launch;
            let _ = &fake;
            native_launch::start(native_launch::NativeLaunch::direct(
                fake.program.as_os_str(),
                cwd.path(),
                "1.1".to_owned(),
                1,
                &[],
            ))
            .unwrap()
        };
        let server = native.server();
        let binding = crate::native_outbox::Binding {
            room: native.binding().room.clone(),
            session: "ses-old-1".to_owned(),
            endpoint: native.binding().endpoint.clone(),
            generation: 1,
        };
        let router = Some(
            NativeRouter::start(
                root.path().join("state").join("native-outbox.json"),
                vec![crate::native_route::RoomEntry {
                    index: 0,
                    title: "Opencode · 1".to_owned(),
                    binding,
                    backend: crate::native_route::Backend::OpenCode(server),
                }],
            )
            .unwrap(),
        );
        let panes = vec![
            report_test_pane(specs.remove(0), Some(native), None),
            report_test_pane(specs.remove(0), None, None),
        ];
        // Pending, same id, a rebind, an unknown room and a wrong vendor,
        // each arriving as the doorbell event production dispatches, so the
        // real SessionReport arm runs for every case below.
        let mut rig = ReportDispatch::new(panes, router);
        rig.push_report(0, None, true);
        let notice = rig.dispatch();
        assert!(notice.contains("session pending after clear"), "{notice}");
        rig.push_report(0, Some("ses-old-1"), false);
        let notice = rig.dispatch();
        assert!(notice.contains("pending cleared"), "{notice}");
        rig.push_report(0, Some("ses-reported-9"), false);
        let notice = rig.dispatch();
        assert!(
            notice.contains("now follows reported session ses-reported-9"),
            "{notice}"
        );
        rig.push_report(9, Some("ses-x"), false);
        let notice = rig.dispatch();
        assert!(notice.contains("unknown room"), "{notice}");
        rig.push_report(1, Some("ses-x"), false);
        let notice = rig.dispatch();
        assert!(
            notice.contains("is not a native OpenCode, Codex or Claude room"),
            "{notice}"
        );
        // A room's PTY hands bytes to its child only on a line boundary, so
        // flush both queues with a marker of the test's own and wait for the
        // logs to settle: every byte a report wrote has reached the child by
        // then, and each log must hold nothing but that marker.
        for pane in rig.panes.iter_mut() {
            for key in [KeyCode::Char('Z'), KeyCode::Char('Q'), KeyCode::Char('\n')] {
                pane.write_key(KeyEvent::new(key, KeyModifiers::NONE))
                    .unwrap();
            }
        }
        // Wait out anything still queued for the terminals.
        let end = Instant::now() + Duration::from_secs(5);
        while rig.panes.iter().any(|pane| pane.has_unwritten_input()) {
            assert!(Instant::now() < end, "writes never drained");
            std::thread::sleep(Duration::from_millis(20));
        }
        for (name, log) in [("native", &native_log), ("raw", &raw_log)] {
            let bytes = settled_log(log);
            assert_eq!(
                bytes,
                b"ZQ\n".to_vec(),
                "session reports must never write to a terminal ({name} got {bytes:?})"
            );
        }
        // And the rebind it did perform landed on the right room.
        assert_eq!(
            rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
            "ses-reported-9"
        );
    }

    #[cfg(all(test, unix))]
    #[test]
    fn a_report_rebinds_only_the_reporting_room() {
        use crate::native_launch::{self, tests::fake_room};
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        // Consume the launch generation a real startup would have taken.
        let _launch_generation =
            crate::native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
                .next()
                .unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // Rooms 0 and 2 keep ordinary bindings; room 1 starts on a stale id.
        let fake_a = fake_room(root.path(), "a", "v2.2.5");
        let native_a = native_launch::start(native_launch::NativeLaunch::direct(
            fake_a.program.as_os_str(),
            cwd.path(),
            "1.1".to_owned(),
            1,
            &[],
        ))
        .unwrap();
        let fake_b = fake_room(root.path(), "b", "v2.2.5");
        let native_b = native_launch::start(native_launch::NativeLaunch::direct(
            fake_b.program.as_os_str(),
            cwd.path(),
            "2.1".to_owned(),
            1,
            &[],
        ))
        .unwrap();
        let program_a = report_test_stub(root.path(), "opencode", &root.path().join("a.calls"));
        let program_b = report_test_stub(root.path(), "opencode", &root.path().join("b.calls"));
        let program_c = report_test_stub(root.path(), "codex", &root.path().join("c.args"));
        let mut specs = vec![
            report_test_spec(&program_a, cwd.path(), "Opencode · 1", true),
            report_test_spec(&program_b, cwd.path(), "Opencode · 2", true),
            report_test_spec(&program_c, cwd.path(), "Codex · 3", true),
        ];
        let server_a = native_a.server();
        let server_b = native_b.server();
        let mut router = Some(
            NativeRouter::start(
                root.path().join("state").join("native-outbox.json"),
                vec![
                    crate::native_route::RoomEntry {
                        index: 0,
                        title: "Opencode · 1".to_owned(),
                        binding: native_a.binding().clone(),
                        backend: crate::native_route::Backend::OpenCode(server_a),
                    },
                    crate::native_route::RoomEntry {
                        index: 1,
                        title: "Opencode · 2".to_owned(),
                        binding: crate::native_outbox::Binding {
                            room: native_b.binding().room.clone(),
                            session: "ses-old-2".to_owned(),
                            endpoint: native_b.binding().endpoint.clone(),
                            generation: 1,
                        },
                        backend: crate::native_route::Backend::OpenCode(server_b),
                    },
                ],
            )
            .unwrap(),
        );
        let codex_room = {
            let stub = |thread: &str| {
                use std::os::unix::fs::PermissionsExt;
                let path = root.path().join(format!("codex-{thread}"));
                std::fs::write(
                    &path,
                    format!(
                        "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' '{thread}'\n",
                        root.path().join("codex.args").display()
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
                path
            };
            let program = stub("thread-a");
            crate::codex_native::room(
                program.as_os_str().to_owned(),
                cwd.path(),
                "thread-a".to_owned(),
                Vec::new(),
            )
        };
        let mut panes = vec![
            report_test_pane(specs.remove(0), Some(native_a), None),
            report_test_pane(specs.remove(0), Some(native_b), None),
            report_test_pane(
                specs.remove(0),
                None,
                Some(&pane::CodexMint::Ready(
                    "11111111-1111-4111-8111-111111111111".to_owned(),
                )),
            ),
        ];
        router
            .as_mut()
            .unwrap()
            .bind_codex(vec![crate::native_route::CodexEntry {
                index: 2,
                title: "Codex · 3".to_owned(),
                room: Some(std::sync::Arc::new(codex_room)),
                generation: 1,
                reason: None,
            }])
            .unwrap();
        let notice = apply_test_report(&mut panes, router.as_mut(), 1, Some("ses_b"), false);
        assert!(notice.contains("now follows reported session ses_b"), "{notice}");
        // The reporting room rebound with a new generation; the others are
        // untouched in binding, generation and adapter.
        let room0 = router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(room0.session, "ses_a");
        assert_eq!(room0.generation, 1);
        let room1 = router.as_ref().unwrap().binding_for(1).unwrap();
        assert_eq!(room1.session, "ses_b");
        assert_eq!(room1.generation, 2);
        assert_eq!(
            router.as_ref().unwrap().binding_for(2).unwrap().session,
            "thread-a"
        );
        let mut mailroom = Mailroom::new(50);
        for index in 0..3 {
            send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", index, "hi", None)
                .unwrap();
        }
        let end = Instant::now() + Duration::from_secs(20);
        loop {
            router.as_mut().unwrap().tick(Instant::now(), &mut mailroom, false, false);
            if mailroom.summary().matches("ACCEPTED").count() == 3 {
                break;
            }
            assert!(Instant::now() < end, "rooms did not stay deliverable");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(all(test, unix))]
    #[test]
    fn a_report_for_an_unknown_room_changes_nothing() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, _fake) = report_test_opencode_room(root.path(), cwd.path(), "ses_b");
        let notice = apply_test_report(&mut panes, router.as_mut(), 9, Some("ses-x"), false);
        assert!(notice.contains("unknown room"), "{notice}");
        assert!(router.as_ref().unwrap().binding_for(0).is_some());
    }

    #[cfg(all(test, unix))]
    #[test]
    fn a_reported_id_rebinds_the_opencode_room_and_delivers_to_it() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut panes, mut router, fake) =
            report_test_opencode_room(root.path(), cwd.path(), "ses-old-1");
        let mut mailroom = Mailroom::new(50);
        let old = router.as_ref().unwrap().binding_for(0).unwrap();
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "old", None)
            .unwrap();
        let end = Instant::now() + Duration::from_millis(300);
        while Instant::now() < end {
            router.as_mut().unwrap().tick(Instant::now(), &mut mailroom, true, false);
            std::thread::sleep(Duration::from_millis(20));
        }
        let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some("ses_b"), false);
        assert!(notice.contains("now follows reported session ses_b"), "{notice}");
        let binding = router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(binding.session, "ses_b");
        assert_eq!(binding.generation, 2);
        assert_eq!(
            binding.endpoint, old.endpoint,
            "the rebind stays on the same owned server"
        );
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "new", None)
            .unwrap();
        pump_router(router.as_mut().unwrap(), &mut mailroom, 500);
        let received = fake.state.received.lock().unwrap().clone();
        assert_eq!(received.len(), 1, "only post-report mail reaches the server");
        assert!(received[0].1.contains("new"), "{received:?}");
        let view = mailroom.summary();
        assert!(view.contains("HELD: sent for an earlier launch"), "{view}");
        assert!(view.contains("ACCEPTED by OpenCode"), "{view}");
    }

    #[cfg(all(test, unix))]
    #[test]
    fn a_codex_report_rebinds_persists_and_delivers_to_the_new_thread() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // The interactive pane and the queue CLI are two processes running
        // one stub: each gets its own file, so the pane's own redirection
        // can never truncate what the queue CLI wrote.
        let queue_args = root.path().join("codex.args");
        let program = report_test_stub(root.path(), "codex", &root.path().join("codex.log"));
        // Consume the launch generation a real startup would have taken.
        let _launch_generation =
            crate::native_launch::GenerationAllocator::new(crate::pane::session_state::state_directory())
                .next()
                .unwrap();
        let specs = vec![report_test_spec(&program, cwd.path(), "Codex · 1", true)];
        let mut router =
            pane::start_native_router(&specs, &[None]).expect("native rooms start a router");
        let mint = pane::CodexMint::Ready("11111111-1111-4111-8111-111111111111".to_owned());
        let mut panes = vec![report_test_pane(
            specs.into_iter().next().unwrap(),
            None,
            Some(&mint),
        )];
        router
            .as_mut()
            .unwrap()
            .bind_codex(vec![crate::native_route::CodexEntry {
                index: 0,
                title: "Codex · 1".to_owned(),
                room: Some(std::sync::Arc::new(panes[0].codex_room_for(
                    "11111111-1111-4111-8111-111111111111".to_owned(),
                ))),
                generation: 1,
                reason: None,
            }])
            .unwrap();
        let mut mailroom = Mailroom::new(50);
        let thread = "33333333-3333-4333-8333-333333333333";
        let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some(thread), false);
        assert!(notice.contains("now follows reported session"), "{notice}");
        assert_eq!(
            crate::pane::session_state::lookup_reported("codex", cwd.path(), "Codex · 1").as_deref(),
            Some(thread),
            "the reported thread survives a restart"
        );
        let binding = router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(binding.session, thread);
        assert_eq!(binding.generation, 2);
        send_native_mail(&mut panes, router.as_mut(), &mut mailroom, "Room X", 0, "hi codex", None)
            .unwrap();
        // Acceptance and the queue CLI's own argument log come from
        // different processes, so wait for both instead of assuming one
        // implies the other.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            router.as_mut().unwrap().tick(Instant::now(), &mut mailroom, false, false);
            let view = mailroom.summary();
            let logged = std::fs::read_to_string(&queue_args).unwrap_or_default();
            if view.contains("ACCEPTED by Codex") && logged.contains(thread) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never delivered to the reported thread:\n{view}\nlogged: {logged}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(all(test, unix))]
    #[test]
    fn a_report_for_a_room_without_native_delivery_changes_nothing() {
        use crate::native_launch::{self, tests::fake_room};
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let native_program = report_test_stub(root.path(), "opencode", &root.path().join("native.calls"));
        let raw_program = report_test_stub(root.path(), "claude", &root.path().join("raw.calls"));
        let fake = fake_room(root.path(), "server", "v2.2.5");
        let mut specs = vec![
            report_test_spec(&native_program, cwd.path(), "Opencode · 1", true),
            report_test_spec(&raw_program, cwd.path(), "Claude · 2", false),
        ];
        let mut natives = vec![
            native_launch::start(native_launch::NativeLaunch::direct(
                fake.program.as_os_str(),
                cwd.path(),
                "1.1".to_owned(),
                1,
                &[],
            ))
            .ok(),
            None,
        ];
        let mut router = pane::start_native_router(&specs, &natives).unwrap();
        let mut panes = vec![
            report_test_pane(specs.remove(0), natives.remove(0), None),
            report_test_pane(specs.remove(0), natives.remove(0), None),
        ];
        let notice = apply_test_report(&mut panes, router.as_mut(), 1, Some("ses-x"), false);
        assert!(notice.contains("not a native OpenCode, Codex or Claude room"), "{notice}");
        assert!(router.as_ref().unwrap().binding_for(1).is_none());
    }

    #[cfg(all(test, unix))]
    #[test]
    fn an_unsavable_codex_report_holds_the_room_and_keeps_the_corrupt_file() {
        let _state = crate::pane::session_state::StateRootGuard::isolated();
        let path = crate::pane::session_state::state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let program = report_test_stub(root.path(), "codex", &root.path().join("calls"));
        let specs = vec![report_test_spec(&program, cwd.path(), "Codex · 1", true)];
        let mut router = pane::start_native_router(&specs, &[None]).expect("router starts");
        let mint = pane::CodexMint::Ready("11111111-1111-4111-8111-111111111111".to_owned());
        let mut panes = vec![report_test_pane(specs.into_iter().next().unwrap(), None, Some(&mint))];
        router
            .as_mut()
            .unwrap()
            .bind_codex(vec![crate::native_route::CodexEntry {
                index: 0,
                title: "Codex · 1".to_owned(),
                room: Some(std::sync::Arc::new(panes[0].codex_room_for(
                    "11111111-1111-4111-8111-111111111111".to_owned(),
                ))),
                generation: 1,
                reason: None,
            }])
            .unwrap();
        let thread = "33333333-3333-4333-8333-333333333333";
        let mut mailroom = Mailroom::new(50);
        let notice = apply_test_report(&mut panes, router.as_mut(), 0, Some(thread), false);
        assert!(notice.contains(thread), "{notice}");
        assert!(notice.contains("could not be saved"), "{notice}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        let error = send_native_mail(
            &mut panes,
            router.as_mut(),
            &mut mailroom,
            "Room X",
            0,
            "held",
            None,
        )
        .unwrap_err();
        assert!(error.contains(thread), "the hold reason names the id: {error}");
    }

    #[test]
    fn a_signal_reaches_the_loop_and_the_reap_path() {
        let source = include_str!("app.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        // The signals are routed to the flag before anything is launched and the
        // reap wait follows the run on every way out, as for Ctrl+Q.
        let run_with = &production[production.find("fn run_with(").unwrap()..];
        let run_with = &run_with[..run_with.find("\n}\n").unwrap()];
        let install = run_with.find("shutdown::install()").expect("handlers are installed");
        let run = run_with.find("run_rooms(").expect("rooms run");
        let wait = run_with.find("wait_for_reaps()").expect("reaping is awaited");
        assert!(install < run && run < wait);
        // The loop leaves as soon as it is asked to, before doing anything else.
        let body = &production[production.find("fn run_loop").unwrap()..];
        let loop_start = body.find("    loop {\n").expect("event loop");
        let after = &body[loop_start + "    loop {\n".len()..];
        assert!(
            after.trim_start().starts_with("if crate::shutdown::requested() {\n            break;"),
            "the first thing the loop does is check for a shutdown request"
        );
    }

    #[test]
    fn terminal_is_taken_only_after_the_launch() {
        let order = std::cell::RefCell::new(Vec::new());
        let (launched, entered) = launch_then_enter(
            || {
                order.borrow_mut().push("launch");
                Ok(1)
            },
            || {
                order.borrow_mut().push("enter");
                Ok(2)
            },
        )
        .unwrap();
        assert_eq!((launched, entered), (1, 2));
        assert_eq!(*order.borrow(), ["launch", "enter"]);

        // A failed launch never takes the terminal.
        let order = std::cell::RefCell::new(Vec::new());
        let result = launch_then_enter(
            || -> io::Result<()> { Err(io::Error::other("no server")) },
            || {
                order.borrow_mut().push("enter");
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(order.borrow().is_empty());
    }

    #[test]
    fn run_rooms_reaches_the_terminal_only_through_launch_then_enter() {
        let source = include_str!("app.rs");
        let body = &source[source.find("fn run_rooms(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let launch = body.find("launch_then_enter(").expect("launch_then_enter is used");
        let enter = body.find("TerminalGuard::enter").expect("terminal is entered");
        assert!(launch < enter);
        assert_eq!(body.matches("TerminalGuard::enter").count(), 1);
        assert!(!body.contains("TerminalGuard::enter()"));
        assert!(body.contains("launch_native_rooms"));
    }

    /// The loop needs a real terminal, so its call site is checked here while
    /// the restart itself is driven through the seam by the named pane tests.
    #[test]
    fn run_rooms_dispatches_restart_keys_through_the_seam() {
        let source = include_str!("app.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        let body = &production[production.find("fn run_loop").unwrap()..];
        let body = &body[..body.find("\n}\n\n\nfn run_rooms").unwrap()];
        assert_eq!(
            body.matches("run_rooms_key_event(").count(),
            1,
            "the loop calls the key step exactly once"
        );
        assert_eq!(
            body.matches("loop_state(").count(),
            1,
            "the loop builds the step state in one place"
        );
        let call = body.find("run_rooms_key_event(").unwrap();
        assert!(
            body[..call].contains("Event::Key(key)"),
            "the step is called from the key arm"
        );
        let step = &production[production.find("fn run_rooms_key_event(").unwrap()..];
        let step = &step[..step.find("\n}\n").unwrap()];
        assert_eq!(
            step.matches("= restart_key(").count(),
            1,
            "the step owns the single restart_key call"
        );
        assert!(
            step.contains("force_restart_requested"),
            "the step owns the restart guard"
        );
        assert!(
            !body.contains("force_restart_requested") && !body.contains("restart_key("),
            "the loop leaves the restart decision to the step"
        );
    }

    /// The only direct writes to a pane's terminal that the app may contain.
    /// Anything else is a new automated writer that has not been reviewed
    /// against native rooms: classify it, guard it in the pane, then list it.
    #[test]
    fn app_has_no_unreviewed_direct_terminal_writer() {
        let source = include_str!("app.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        let writers = [
            ("write_bytes", 0),
            ("push_write", 0),
            ("enqueue", 0),
            // The person's own keystrokes and scroll keys.
            ("write_key", 1),
            ("forward_page_up", 1),
            ("forward_page_down", 1),
            // Automated writers; the pane refuses them for native rooms.
            ("send_whisper", 2),
            ("resend_whisper_submit", 1),
        ];
        for (method, expected) in writers {
            let calls = production.matches(&format!(".{method}(")).count();
            assert_eq!(calls, expected, "direct calls of .{method}( in app.rs");
        }
    }


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
        assert!(rules.contains(
            "You do not need to do anything now: just check the roster and stand by until a message arrives."
        ));
        // It is the last thing the room reads.
        assert!(rules.ends_with("until a message arrives."));
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
    fn native_pulse_label_follows_state_not_a_fixed_string() {
        let source = include_str!("app.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        // No special case that pins native rooms to a constant label.
        assert!(!production.contains("\"native\".to_owned()"));
        // Native rooms reach the shared roster_state path like any other.
        let fn_body = &production[production.find("fn pulse_label(").unwrap()..];
        let fn_body = &fn_body[..fn_body.find("\n}\n").unwrap()];
        assert!(fn_body.contains("roster_state("));
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
    fn delivery_gate_observes_fast_hook_busy_cycles_without_unlocking_a_busy_screen() {
        let mut gate = DeliveryGate::new(true);
        gate.intro_sent();
        gate.observe_pulse(PulseState::Ready, true);
        assert_eq!(gate, DeliveryGate::IntroSent);
        gate.observe_pulse(PulseState::Thinking, true);
        assert_eq!(gate, DeliveryGate::IntroRunning);
        gate.observe_pulse(PulseState::Ready, false);
        assert!(!gate.can_deliver(false));
        gate.observe_pulse(PulseState::Ready, true);
        assert!(gate.can_deliver(true));
        gate.message_sent();
        gate.observe_pulse(PulseState::Working, true);
        gate.observe_pulse(PulseState::Ready, true);
        assert!(gate.can_deliver(true));
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

    #[cfg(unix)]
    mod loop_behaviour {
        use super::*;
        use crate::config::RoomSpec;
        use crate::native_route::NativeRouter;
        use crate::pane::{GuestEnvironment, Pane};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        use portable_pty::PtySize;
        use ratatui::{Terminal, backend::TestBackend};
        use std::{
            collections::VecDeque,
            io,
            path::{Path, PathBuf},
            time::{Duration, Instant},
        };

        #[allow(dead_code)]
        enum ScriptedStep {
            Event(Event),
            Idle,
        }

        struct ScriptedEvents {
            steps: VecDeque<ScriptedStep>,
        }

        impl ScriptedEvents {
            fn new(events: Vec<Event>) -> Self {
                Self {
                    steps: events.into_iter().map(ScriptedStep::Event).collect(),
                }
            }

            #[allow(dead_code)]
            fn steps(steps: Vec<ScriptedStep>) -> Self {
                Self { steps: steps.into() }
            }
        }

        impl EventSource for ScriptedEvents {
            fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
                loop {
                    match self.steps.front() {
                        Some(ScriptedStep::Event(_)) => return Ok(true),
                        Some(ScriptedStep::Idle) => {
                            self.steps.pop_front();
                            return Ok(false);
                        }
                        None => return Ok(false),
                    }
                }
            }

            fn read(&mut self) -> io::Result<Event> {
                match self.steps.pop_front() {
                    Some(ScriptedStep::Event(event)) => Ok(event),
                    _ => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no scripted event")),
                }
            }
        }

        struct SilentDoorbell {
            path: PathBuf,
        }

        impl SilentDoorbell {
            fn new(path: PathBuf) -> Self {
                Self { path }
            }
        }

        impl DoorbellSource for SilentDoorbell {
            fn try_recv(
                &mut self,
            ) -> Result<crate::doorbell::DoorbellEvent, std::sync::mpsc::TryRecvError> {
                Err(std::sync::mpsc::TryRecvError::Empty)
            }

            fn path(&self) -> &Path {
                &self.path
            }
        }

        fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
            Event::Key(KeyEvent::new(code, modifiers))
        }

        fn shell_spec(program: &Path, cwd: &Path, title: &str) -> RoomSpec {
            RoomSpec {
                name: "sh".to_owned(),
                vendor: "shell".to_owned(),
                title: title.to_owned(),
                program: program.as_os_str().to_owned(),
                args: Vec::new(),
                transport: crate::config::Transport::Shell,
                cwd: Some(cwd.to_path_buf()),
                variables: Vec::new(),
                allow_control: false,
                use_headroom: false,
                scheduling: None,
                headroom_args: Vec::new(),
                native_delivery: false,
            }
        }

        fn pty_size() -> PtySize {
            PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            }
        }

        #[test]
        fn the_production_loop_runs_to_completion_on_ctrl_q_and_cleans_up() {
            let cwd = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let program = if cfg!(windows) {
                PathBuf::from("cmd.exe")
            } else {
                PathBuf::from("/bin/sh")
            };
            let spec = shell_spec(&program, cwd.path(), "Room A");
            let pane = Pane::spawn(spec, pty_size(), GuestEnvironment::new([])).unwrap();
            let mut panes = vec![pane];
            let mut router: Option<NativeRouter> = None;
            let mut codex = vec![None];
            let environments = vec![GuestEnvironment::new([])];
            let now = Instant::now();
            let mut spawned_at = vec![now];
            let mut last_output = vec![None];
            let mut pane_sizes = vec![pty_size()];
            let mut notice = Notices::default();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut events = ScriptedEvents::new(vec![key(KeyCode::Char('q'), KeyModifiers::CONTROL)]);
            let mut doorbell = SilentDoorbell::new(root.path().join("doorbell.sock"));

            run_loop(
                &mut terminal,
                &mut events,
                &mut doorbell,
                &mut panes,
                &mut router,
                &mut codex,
                &environments,
                &mut spawned_at,
                &mut last_output,
                &mut pane_sizes,
                &[],
                100,
                &mut notice,
                &[false],
                1,
            )
            .unwrap();

            // The loop returned only after running cleanup on the scripted exit.
            assert!(!panes[0].is_online(), "pane must be cleaned up on exit");
        }
    }

}
