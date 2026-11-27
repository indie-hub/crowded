//! Ratatui rendering, focus, note composition, and the main event loop.

use std::{
    collections::VecDeque,
    ffi::OsString,
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

/// Test seam: records a room the Headroom lane waits on.
#[cfg(not(test))]
fn note_headroom_wait(_room: usize) {}

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
        false,
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
        bool,
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
        true,
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
        Some(pane::CodexMint::Ready { uuid, .. }) => {
            let mut spec = spec;
            pane::resume_session(&mut spec, uuid)?;
            factory.interactive(spec, size, environment, native)
        }
        Some(pane::CodexMint::Unreported { prompt, generation }) => {
            let base_args = spec.args.clone();
            let base_variables = spec.variables.clone();
            let mut launched = spec;
            pane::controls::set_initial_prompt(&mut launched, prompt)?;
            launched.variables.push((
                OsString::from("CROWDED_LAUNCH_GENERATION"),
                OsString::from(generation.to_string()),
            ));
            let mut pane = factory.interactive(launched, size, environment, native)?;
            pane.restore_launch_view(base_args, base_variables);
            Ok(pane)
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
                note_headroom_wait(prior);
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
        panes[index] =
            factory.unavailable(spec, sizes[index], environments[index].clone(), &failure)?;
        delivery_gates[index] = DeliveryGate::new(false);
        spawned_at[index] = now;
        last_output[index] = None;
        if let (Some(router), Some(mint)) = (router.as_deref_mut(), codex[index].as_ref()) {
            pane::bind_codex_room(router, panes, index, mint)?;
        }
        let message = format!(
            "{}: {failure}; the room was not started",
            panes[index].title()
        );
        eprintln!("{message}");
        append_notice(notice, message, NoticeSeverity::Failure);
    }
    retry_codex_reports(router.as_deref_mut(), panes, notice);
    check_codex_report_timeouts(router, panes, spawned_at, notice);
    Ok(())
}

/// One visible notice when a launch has waited too long for its session
/// report. The room is not torn down and its mail is not released: a late
/// report still binds this generation. No exec fallback is started.
fn check_codex_report_timeouts(
    router: Option<&mut NativeRouter>,
    panes: &mut [Pane],
    spawned_at: &[Instant],
    notice: &mut Notices,
) {
    let Some(router) = router else {
        return;
    };
    for (index, pane) in panes.iter_mut().enumerate() {
        if !pane.codex_native() || pane.codex_no_report_notice() {
            continue;
        }
        let Some(binding) = router.binding_for(index) else {
            continue;
        };
        if !crate::native_session::is_unreported_placeholder(&binding) {
            continue;
        }
        if spawned_at
            .get(index)
            .is_some_and(|started| started.elapsed() >= NO_REPORT_AFTER)
        {
            pane.set_codex_no_report_notice();
            append_notice(
                notice,
                format!(
                    "{}: no Codex session report; approve Crowded's hook in Codex /hooks, or trust the folder, then Ctrl+R. Mail stays held.",
                    pane.title()
                ),
                NoticeSeverity::Failure,
            );
        }
    }
}

/// Completes a Codex report whose rollout was not readable yet. The mail
/// stayed held on the same launch placeholder, so binding it now releases
/// exactly that launch's mail and nothing from an older one.
fn retry_codex_reports(
    router: Option<&mut NativeRouter>,
    panes: &mut [Pane],
    notice: &mut Notices,
) {
    let Some(router) = router else {
        return;
    };
    for (index, pane) in panes.iter_mut().enumerate() {
        if !pane.codex_native() {
            continue;
        }
        let Some(session) = pane.captured_session_id() else {
            continue;
        };
        let Some(binding) = router.binding_for(index) else {
            continue;
        };
        if !crate::native_session::is_unreported_placeholder(&binding)
            || !pane::codex_rollout_verified(pane.cwd(), &session)
        {
            continue;
        }
        let title = pane.title().to_owned();
        if let Err(error) =
            crate::pane::session_state::upsert_reported("codex", pane.cwd(), &title, &session)
        {
            append_notice(
                notice,
                format!("{title}: session report could not be saved: {error}"),
                NoticeSeverity::Failure,
            );
            continue;
        }
        let room = pane.codex_room_for(session.clone());
        let destination = (index + 1).to_string();
        if let Err(error) = router.rebind_room(crate::native_route::RoomEntry {
            index,
            title: title.clone(),
            binding: crate::codex_native::binding(&destination, &session, binding.generation),
            backend: crate::native_route::Backend::Codex(Arc::new(room)),
        }) {
            append_notice(
                notice,
                format!("{title}: could not bind the reported session: {error}"),
                NoticeSeverity::Failure,
            );
        }
    }
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
fn launch_failure(
    title: &str,
    room: usize,
    cause: impl std::fmt::Display,
) -> Box<dyn std::error::Error> {
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
    let generation =
        pane::next_generation().map_err(|error| launch_failure(&title, index + 1, error))?;
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
                .attach(&cwd, index + 1, session, generation)
                .map_err(|error| launch_failure(&title, index + 1, error))?;
            panes[index].record_session(session);
            session.clone()
        } else {
            inbox
                .prepare(&cwd, index + 1, generation)
                .map_err(|error| launch_failure(&title, index + 1, error))?;
            // Internal placeholder only: never passed to Claude or persisted as
            // a Claude session; replaced when the hook reports the real session.
            crate::native_session::unreported_session(&(index + 1).to_string(), generation)
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

/// Clears one owned OpenCode room onto a new session on the same server and
/// moves the router with it. Mail held for the previous launch stays held and
/// is never re-sent to the new session; only mail held pending a clear adopts
/// it. The advice to clear again assumes the router still holds a binding for
/// the room, which a failed rebind leaves but an unbound room does not.
#[allow(clippy::too_many_arguments)]
pub(crate) fn relaunch_opencode_room(
    index: usize,
    size: portable_pty::PtySize,
    panes: &mut [Pane],
    router: Option<&mut NativeRouter>,
    delivery_gates: &mut [DeliveryGate],
    last_output: &mut [Option<Instant>],
    spawned_at: &mut [Instant],
    room_pulses: &mut [Option<PulseSample>],
    now: Instant,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let title = panes[index].title().to_owned();
    // A dead server cannot clear onto a new session on itself: point at the
    // revive path instead of failing on the first unreachable request.
    match panes[index].poll_opencode_server_exit() {
        Ok(true) | Err(_) => {
            return Err(launch_failure(
                &title,
                index + 1,
                "its owned OpenCode server has exited; press Ctrl+R to revive it",
            ));
        }
        Ok(false) => {}
    }
    let generation =
        pane::next_generation().map_err(|error| launch_failure(&title, index + 1, error))?;
    // A failed clear changes nothing, so the room keeps its old session and
    // the old run keeps going.
    let (binding, abort) = panes[index]
        .clear_opencode_session(size, generation)
        .map_err(|error| launch_failure(&title, index + 1, error))?;
    // The pane already runs a new terminal on the new session, so its gate,
    // timestamp and pulse must describe that terminal whatever the router does.
    delivery_gates[index] = DeliveryGate::new(false);
    last_output[index] = None;
    spawned_at[index] = now;
    room_pulses[index] = Some(PulseSample::now(PulseState::Starting, None));
    if let Some(router) = router {
        // The pane and its server already run the new session. If the router
        // cannot follow, mail is held rather than pasted, but the room is out of
        // step until it is cleared again: say so instead of hiding it.
        let outcome = match router.server_for(index) {
            Some(server) => router.rebind_room(crate::native_route::RoomEntry {
                index,
                title: title.clone(),
                binding: binding.clone(),
                backend: crate::native_route::Backend::OpenCode(server),
            }),
            None => Err(io::Error::other("that room has no owned OpenCode server")),
        };
        if let Err(error) = outcome {
            let warning = abort
                .map(|warning| format!("; also {warning}"))
                .unwrap_or_default();
            return Err(io::Error::other(format!(
                "{title}: the room now runs session {} but the router could not follow it ({error}); the mail is held, so run `control {} clear` again{warning}",
                binding.session,
                index + 1
            ))
            .into());
        }
    }
    Ok(abort)
}

/// Marks rooms whose owned OpenCode server died: reaps the wrapper child,
/// kills the looping TUI so the room reads offline, and keeps the router
/// binding so the outbox holds the room's mail instead of pasting it.
/// One Failure notice per room, naming the room; already-offline rooms are
/// not reported again. Never touches another room's server.
pub(crate) fn detect_dead_opencode_servers(
    panes: &mut [Pane],
    mut router: Option<&mut NativeRouter>,
    notice: &mut Notices,
) {
    for (index, pane) in panes.iter_mut().enumerate() {
        if !pane.opencode_native() {
            continue;
        }
        // A failed poll fails safe to dead: a broken server is never treated
        // as live.
        let exited = pane.poll_opencode_server_exit().unwrap_or(true);
        if !exited {
            continue;
        }
        // Already offline: retry a pending hold an earlier pass could not
        // mark, so later mail is still adopted on revive. Silent: the room
        // already has its notice.
        if !pane.is_online() {
            if let Some(router) = router.as_deref_mut()
                && !router.is_pending(index)
                && router.binding_for(index).is_some()
            {
                let _ = router.mark_pending(index);
            }
            continue;
        }
        let title = pane.title().to_owned();
        pane.cleanup();
        // Between sessions: new mail is held with a visible reason and is
        // adopted onto the revived binding, like a clear's pending hold. A
        // failed mark is surfaced honestly instead of claiming a hold the
        // outbox never took: the room stays offline either way, but mail may
        // then follow the old binding, and later passes retry the mark above.
        let mark = match router.as_deref_mut() {
            Some(router) => router
                .mark_pending(index)
                .err()
                .map(|error| error.to_string()),
            None => Some("native delivery is unavailable".to_owned()),
        };
        match mark {
            None => notice.push(
                format!(
                    "{title}: its owned OpenCode server exited; the room is offline and its mail is held; press Ctrl+R to revive it"
                ),
                NoticeSeverity::Failure,
            ),
            Some(error) => notice.push(
                format!(
                    "{title}: its owned OpenCode server exited; the room is offline but its pending hold could not be marked ({error}), so mail may follow the old binding; press Ctrl+R to revive it"
                ),
                NoticeSeverity::Failure,
            ),
        }
    }
}

/// Moves one lane onto its revived server: swaps the lane's server, then
/// rebinds under the new generation like a clear. Fails before touching the
/// pane: the caller leaves the room offline on its previous binding, and the
/// outbox keeps the lane's mail held. Only the revived server's recorded
/// processes are ever affected, via its own drop on failure.
/// Restores the previous server in the lane map when the rebind fails.
/// Revives one offline owned-OpenCode room on a new server through the same
/// launch path as startup, then rebinds the router under a new generation
/// like a clear. Held mail for the room's lane is adopted onto the new
/// binding and delivered once; the caller reintroduces the house rules like
/// a clear. A live room is never revived. A failed revive leaves the room
/// offline on its previous binding with its reason.
#[allow(clippy::too_many_arguments)]
pub(crate) fn revive_opencode_room(
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
    if !panes[index].opencode_native() {
        return Err(launch_failure(
            &title,
            index + 1,
            "only a room on an owned OpenCode server can be revived this way",
        ));
    }
    match panes[index].poll_opencode_server_exit() {
        Ok(false) if panes[index].is_online() => {
            return Err(launch_failure(
                &title,
                index + 1,
                "the room is still online; use clear for a new session",
            ));
        }
        Err(error) if panes[index].is_online() => {
            let _ = error;
        }
        _ => {}
    }
    let old = panes[index].opencode_binding().ok_or_else(|| {
        launch_failure(&title, index + 1, "that room has no owned OpenCode session")
    })?;
    let generation =
        pane::next_generation().map_err(|error| launch_failure(&title, index + 1, error))?;
    let spec = panes[index].room_spec().clone();
    let launch =
        pane::opencode_revive_launch(&spec, &environments[index], old.room.clone(), generation)
            .map_err(|error| launch_failure(&title, index + 1, error))?;
    let room = crate::native_launch::begin(launch)
        .wait(crate::native_launch::LAUNCH_TIMEOUT)
        .map_err(|error| launch_failure(&title, index + 1, error))?;
    // (1) The new owned server above is already running. (2) Prepare the
    // replacement TUI without touching the pane: any failure drops the new
    // server by its recorded processes and leaves the room offline on its
    // previous binding.
    let prepared = panes[index]
        .prepare_revived_tui(size, &room)
        .map_err(|error| launch_failure(&title, index + 1, error))?;
    let server = room.server();
    let binding = room.binding().clone();
    // (3) Rebind the router with the hold kept: the lane stays pending, so a
    // failure drops the prepared terminal and the new server and leaves the
    // room offline on its previous binding with its mail still held.
    if let Some(router) = router.as_deref_mut() {
        router
            .rebind_revived_room_keep_pending(crate::native_route::RoomEntry {
                index,
                title: title.clone(),
                binding: binding.clone(),
                backend: crate::native_route::Backend::OpenCode(server.clone()),
            })
            .map_err(|error| launch_failure(&title, index + 1, error))?;
    }
    // (4) Commit: marker then pane swap. A failure reports that the router
    // follows the new session but the room could not start; the hold is kept
    // so the mail stays held.
    let binding = panes[index]
        .commit_revived_tui(prepared, room)
        .map_err(|error| {
            launch_failure(
                &title,
                index + 1,
                format!(
                    "the router follows session {} but the room could not start ({error}); press Ctrl+R again",
                    binding.session
                ),
            )
        })?;
    if let Some(router) = router.as_deref_mut() {
        router.release_revived_hold(index).map_err(|error| {
            launch_failure(
                &title,
                index + 1,
                format!(
                    "the room now runs session {} but its hold could not be released ({}); the mail is held, run control {} clear again",
                    binding.session,
                    error,
                    index + 1
                ),
            )
        })?;
    }
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
    if panes[index].opencode_native() {
        return revive_opencode_room(
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
    pub(crate) mailroom: &'a mut Mailroom,
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
    mailroom: &'a mut Mailroom,
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
        mailroom,
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
    let now = Instant::now();
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
        now,
    );
    let restarted = matches!(&result, Some((_, severity)) if *severity == NoticeSeverity::Info);
    if let Some((text, severity)) = result {
        state.notice.push(text, severity);
    }
    // A revived owned-OpenCode room runs a fresh session: reteach the house
    // rules through the router exactly like a clear does.
    if restarted
        && state.panes[focused].opencode_native()
        && let Some(router) = state.router.as_deref_mut()
    {
        let roster = welcome_roster(state.panes, state.room_pulses, now);
        let message = house_rules(focused + 1, &roster, state.fuse_size);
        let title = state.panes[focused].title().to_owned();
        let (text, severity) =
            reintroduce_native_room(&title, router, state.mailroom, focused, &message);
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
        report.launch,
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
    launch: Option<u64>,
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
    if panes[from].codex_native() {
        // A report may only turn this launch's own placeholder into a session.
        // A real binding is either a resumed session or a report already
        // applied, so a later report is stale and must not retarget mail.
        if !current
            .as_ref()
            .is_some_and(|binding| crate::native_session::is_unreported_placeholder(binding))
        {
            return Ok(format!(
                "ignored: {title} is not waiting for a session report"
            ));
        }
        if launch != current.as_ref().map(|binding| binding.generation) {
            return Ok(format!(
                "ignored: the report does not name launch {}",
                current
                    .as_ref()
                    .map(|binding| binding.generation)
                    .unwrap_or(0)
            ));
        }
        match pane::codex_rollout_status(&panes[from].cwd(), &session) {
            pane::controls::CodexRollout::Verified => {}
            pane::controls::CodexRollout::WrongCwd => {
                return Ok(format!(
                    "ignored: the reported session belongs to another directory"
                ));
            }
            pane::controls::CodexRollout::NotYet => {
                panes[from].record_session(&session);
                return Ok(format!(
                    "{title}: waiting for the reported session's rollout"
                ));
            }
        }
    }
    // The first report of a launch turns its unreported placeholder into the
    // real session: the launch keeps its generation, so mail saved against the
    // placeholder still belongs to this launch. Any later report is a session
    // switch and takes a new generation, which holds older mail as stale.
    let generation = match crate::native_session::generation_for_report(current.as_ref()) {
        crate::native_session::ReportedGeneration::Keep(generation) => generation,
        crate::native_session::ReportedGeneration::Allocate => {
            crate::native_launch::GenerationAllocator::new(
                crate::pane::session_state::state_directory(),
            )
            .next()
            .map_err(|error| format!("{title} could not take a launch generation: {error}"))?
        }
    };
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
        let server = router
            .server_for(from)
            .ok_or_else(|| format!("{title} has no owned OpenCode server to rebind"))?;
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
    let (text, severity) = match send_native_mail(panes, router, mailroom, source, to, body, reply)
    {
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
    resume: bool,
    fuse_size: usize,
    token_pricing: Vec<TokenPricing>,
    startup_notice: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // A hang-up or a kill from outside ends the loop below like Ctrl+Q does.
    crate::shutdown::install();
    let result = run_rooms(
        specs,
        resumed,
        resume,
        fuse_size,
        token_pricing,
        startup_notice,
    );
    crate::native_launch::wait_for_reaps();
    result
}

pub(crate) trait DoorbellSource {
    fn try_recv(&mut self)
    -> Result<crate::doorbell::DoorbellEvent, std::sync::mpsc::TryRecvError>;
    fn path(&self) -> &std::path::Path;
}

impl DoorbellSource for crate::doorbell::Doorbell {
    fn try_recv(
        &mut self,
    ) -> Result<crate::doorbell::DoorbellEvent, std::sync::mpsc::TryRecvError> {
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
    detect_dead_opencode_servers(panes, router.as_mut(), notice);
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
                                        .as_millis() as u64
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
                let result = (|| -> Result<(bool, Option<String>), Box<dyn std::error::Error>> {
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
                    let opencode_native = panes[control.to].opencode_native();
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
                        .map(|()| (false, None)),
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
                        .map(|()| (true, None)),
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
                        .map(|()| (false, None)),
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
                        .map(|()| (true, None)),
                        ControlAction::ClearContext if opencode_native => relaunch_opencode_room(
                            control.to,
                            size,
                            panes,
                            router.as_mut(),
                            delivery_gates,
                            last_output,
                            spawned_at,
                            room_pulses,
                            now,
                        )
                        .map(|warning| (false, warning)),
                        ControlAction::ClearContext => panes[control.to]
                            .clear_context(size)
                            .map(|()| (false, None)),
                        ControlAction::Resume => panes[control.to]
                            .resume_context(size)
                            .map(|resumed| (resumed, None)),
                        ControlAction::Configure { model, effort } => panes[control.to]
                            .configure(model.as_deref(), effort.as_ref().map(|e| e.label()), size)
                            .map(|()| (false, None)),
                    }
                })();
                match result {
                    Ok((resumed, warning)) => {
                        delivery_gates[control.to] =
                            gate_after_control(panes[control.to].needs_intro(), resumed);
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
                        // The clear succeeded and the room works; the warning
                        // only says the old run may still be going.
                        if let Some(warning) = warning {
                            append_notice(notice, warning, NoticeSeverity::Failure);
                        }
                        // A clear starts a new conversation, so the room must
                        // be taught the house rules again. A native room takes
                        // them through the router, never as a paste. Codex is
                        // excluded: its rules ride in as the exec prompt.
                        if panes[control.to].is_native()
                            && !panes[control.to].codex_native()
                            && matches!(&control.action, ControlAction::ClearContext)
                            && let Some(router) = router.as_mut()
                        {
                            let roster = welcome_roster(panes, room_pulses, now);
                            let message = house_rules(control.to + 1, &roster, fuse_size);
                            let title = panes[control.to].title().to_owned();
                            let (text, severity) = reintroduce_native_room(
                                &title, router, mailroom, control.to, &message,
                            );
                            append_notice(notice, text, severity);
                        }
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
        detect_dead_opencode_servers(panes, router.as_mut(), notice);
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
                            format!("Automatic delivery resumed; {} queued", pending.len()),
                            NoticeSeverity::Info,
                        );
                    } else {
                        delivery_paused = true;
                        notice.push("Automatic delivery paused".to_owned(), NoticeSeverity::Info);
                    }
                }
                Event::Key(key)
                    if key.code == KeyCode::F(4)
                        && key.kind == KeyEventKind::Press
                        && matches!(&input_mode, InputMode::Normal) =>
                {
                    let roster = welcome_roster(&panes, &room_pulses, now);
                    if let Some(router) = router.as_mut().filter(|router| router.is_native(focused))
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
                                    format!("{} reintroduced to the room", panes[focused].title()),
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
                Event::Key(key)
                    if run_rooms_key_event(
                        &Event::Key(key),
                        &mut loop_state(
                            focused,
                            fuse_size,
                            &mut |index| {
                                terminal
                                    .autoresize()
                                    .map_err(|error| io::Error::other(error.to_string()))?;
                                let (rooms, _, _) = content_areas(
                                    terminal
                                        .size()
                                        .map_err(|error| io::Error::other(error.to_string()))?
                                        .into(),
                                );
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
                            &mut mailroom,
                            &mut notice,
                        ),
                    )? =>
                {
                    match &mut input_mode {
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
                                                        submit_pending
                                                            .push_back((now, target, false));
                                                        (
                                                            format!("Envelope #{id:04} injected"),
                                                            NoticeSeverity::Info,
                                                        )
                                                    }
                                                    Err(error) => (
                                                        format!(
                                                            "Envelope #{id:04} failed: {error}"
                                                        ),
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
                            } else if key.code == KeyCode::PageUp && key.kind == KeyEventKind::Press
                            {
                                if panes[focused].is_alternate_screen() {
                                    let _ = panes[focused].forward_page_up();
                                } else {
                                    let rows = panes[focused].visible_height();
                                    panes[focused].scroll_up(rows);
                                }
                            } else if key.code == KeyCode::PageDown
                                && key.kind == KeyEventKind::Press
                            {
                                if panes[focused].is_alternate_screen() {
                                    let _ = panes[focused].forward_page_down();
                                } else {
                                    let rows = panes[focused].visible_height();
                                    panes[focused].scroll_down(rows);
                                }
                            } else if panes[focused].is_online() {
                                if let Some((message, severity)) =
                                    forward_key(&mut panes[focused], key)
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
                            } else if key.code == KeyCode::Down && key.kind == KeyEventKind::Press {
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
                    }
                }
                Event::Resize(_, _) => {
                    terminal
                        .autoresize()
                        .map_err(|error| io::Error::other(error.to_string()))?;
                    let (rooms, _, _) = content_areas(
                        terminal
                            .size()
                            .map_err(|error| io::Error::other(error.to_string()))?
                            .into(),
                    );
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
    terminal
        .show_cursor()
        .map_err(|error| io::Error::other(error.to_string()))?;
    Ok(())
}

fn run_rooms(
    mut specs: Vec<RoomSpec>,
    resumed: Vec<bool>,
    resume: bool,
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
    // such room, run in parallel and bounded, unless `crowded resume` reopens
    // a stored session whose rollout is still on disk.
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
    let (mut codex, codex_notices) =
        pane::start_codex_mints(&mut specs, &environments, &codex_prompts, resume);
    let mut mint_notices: Vec<String> = codex_notices
        .into_iter()
        .chain(
            codex
                .iter()
                .enumerate()
                .filter_map(|(index, mint)| match mint {
                    Some(pane::CodexMint::Failed(reason)) => Some(format!(
                        "{}: {reason}; the room was not started",
                        specs[index].title
                    )),
                    _ => None,
                }),
        )
        .collect();
    for message in &mint_notices {
        eprintln!("{message}");
    }
    // A native Claude room's session id is Crowded's to choose, so it is fixed
    // and saved before the terminal starts: no model turn is involved.
    let (mut claude_starts, mut claude_notices) = pane::start_claude_sessions(&mut specs, resume);
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
        claude_notices.extend(pane::bind_claude_rooms(router, &specs, &mut claude_starts)?);
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

/// Test-only rig shared with the native delivery lifecycle harness: only the
/// pieces the harness drives are visible outside [`tests`].
#[cfg(all(test, unix))]
pub(crate) use tests::{
    ReportDispatch, report_test_opencode_room, report_test_pane, report_test_spec, report_test_stub,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    /// Every room the Headroom lane waited on, on this thread only, so a test
    /// can assert which rooms were waited on instead of timing the wait.
    pub(crate) static HEADROOM_WAITS: std::cell::RefCell<Vec<usize>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn note_headroom_wait(room: usize) {
    HEADROOM_WAITS.with(|waits| waits.borrow_mut().push(room));
}

/// A Codex room with no session report gets one visible notice, then waits.
#[cfg(not(test))]
const NO_REPORT_AFTER: Duration = Duration::from_secs(20);
#[cfg(test)]
const NO_REPORT_AFTER: Duration = Duration::from_millis(100);
