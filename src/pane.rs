//! One room: its guest command, PTY, terminal screen, and lifecycle.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tui_term::vt100::{Parser, Screen};

use crate::{
    codex_native,
    command::{
        ResolvedCommand, headroom_on_path, pty_spawn_retrying_text_busy, spawn_retrying_text_busy,
    },
    config::{RoomScheduling, RoomSpec, Transport},
    doorbell::RoomCapabilities,
    native_launch::{self, GenerationAllocator, NativeRoom},
};

pub(crate) mod controls;

#[cfg(target_os = "linux")]
mod pty_io;

pub(crate) mod session_state;

pub(crate) use session_state::usage_cost;

/// Shown for every automated send to a native room.
pub(crate) const NATIVE_PASTE_REFUSED: &str =
    "native delivery room: terminal paste is disabled; its mail goes through the native outbox";

const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
// ponytail: ConPTY only needs a valid DSR here; report the parser's exact
// cursor if a guest later depends on cursor-aware terminal negotiation.
const CURSOR_POSITION_RESPONSE: &[u8] = b"\x1b[1;1R";
// Bounded per-pane scrollback: the vt100 parser retains at most this many
// scrolled-off rows, so history grows predictably no matter how chatty a
// guest is. The parser stores a full cell grid per retained row, which is
// why the bound is explicit rather than unbounded.
const SCROLLBACK_LINES: usize = 1000;

/// How often an unverified Codex report may walk `~/.codex/sessions`. The walk
/// is recursive and the event loop runs about 60 passes a second.
const ROLLOUT_RETRY_INTERVAL: Duration = Duration::from_secs(1);

fn rollout_retry_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|last| now.saturating_duration_since(last) >= ROLLOUT_RETRY_INTERVAL)
}

/// Consumes one retry slot before the expensive check. The verifier is injected
/// so a test can count calls without touching the filesystem.
pub(crate) fn rollout_retry_check(
    last: &mut Option<Instant>,
    now: Instant,
    cwd: &Path,
    session: &str,
    mut verify: impl FnMut(&Path, &str) -> bool,
) -> bool {
    if !rollout_retry_due(*last, now) {
        return false;
    }
    *last = Some(now);
    verify(cwd, session)
}

pub(crate) fn respond_to_terminal_queries(
    writer: &mut dyn Write,
    tail: &mut Vec<u8>,
    bytes: &[u8],
) -> io::Result<()> {
    let mut scan = Vec::with_capacity(tail.len() + bytes.len());
    scan.append(tail);
    scan.extend_from_slice(bytes);
    let queries = scan
        .windows(CURSOR_POSITION_QUERY.len())
        .filter(|window| *window == CURSOR_POSITION_QUERY)
        .count();
    tail.extend_from_slice(&scan[scan.len().saturating_sub(CURSOR_POSITION_QUERY.len() - 1)..]);
    for _ in 0..queries {
        writer.write_all(CURSOR_POSITION_RESPONSE)?;
    }
    if queries > 0 {
        writer.flush()?;
    }
    Ok(())
}

fn key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    // Crossterm gives us structured key events; a PTY understands only bytes.
    // `None` means this small encoder does not support that event.
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return None;
    }

    if key.modifiers == KeyModifiers::CONTROL {
        return match key.code {
            KeyCode::Char(c) if c.is_ascii_alphabetic() => {
                // ASCII control keys are A=1, B=2, ... Z=26.
                Some(vec![c.to_ascii_uppercase() as u8 - b'@'])
            }
            _ => None,
        };
    }

    if !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) {
        return None;
    }

    match key.code {
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        // Terminals encode Shift+Tab as CSI Z, not as a modified tab byte.
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        _ => None,
    }
}

fn shell_quote(text: &str) -> String {
    // POSIX shells end a single-quoted string at `'`. This sequence briefly
    // leaves the quote, inserts a literal apostrophe, then starts it again.
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn working_directory(configured: Option<&Path>) -> io::Result<PathBuf> {
    let launch_directory = env::current_dir()?;
    let directory = match configured {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => launch_directory.join(path),
        None => launch_directory,
    };
    if !directory.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "room working directory does not exist: {}",
                directory.display()
            ),
        ));
    }
    Ok(directory)
}

fn environment_value(variables: &[(OsString, OsString)], name: &str) -> Option<OsString> {
    variables
        .iter()
        .rev()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
        .or_else(|| env::var_os(name))
}

/// Decide the actually-launched program and arguments for a room.
///
/// When `use_headroom` is set and the `headroom` wrapper is installed on the
/// room's PATH, the launch becomes `headroom wrap <original-program>
/// <headroom_args...> <original-args...>` (`headroom wrap` takes the tool
/// name as its own subcommand, e.g. `headroom wrap claude`; `headroom_args`
/// are flags for that `wrap` subcommand itself, e.g. `--port`, `--memory`,
/// and must land after the tool name but before the tool's own args, which
/// are passed through unrecognized). Otherwise the original command is
/// launched unchanged and missing headroom is a silent fallback, not an
/// error. Returns `(program, args, headroom_active)`.
fn headroom_launch(
    program: &OsStr,
    args: &[OsString],
    use_headroom: bool,
    headroom_args: &[OsString],
    path: &Option<OsString>,
    path_ext: &Option<OsString>,
) -> (OsString, Vec<OsString>, bool) {
    if use_headroom && headroom_on_path(path.as_deref(), path_ext.as_deref()) {
        let mut wrapped = Vec::with_capacity(args.len() + headroom_args.len() + 2);
        wrapped.push(OsString::from("wrap"));
        wrapped.push(program.to_os_string());
        wrapped.extend(headroom_args.iter().cloned());
        wrapped.extend(args.iter().cloned());
        (OsString::from("headroom"), wrapped, true)
    } else {
        (program.to_os_string(), args.to_vec(), false)
    }
}

/// The process that runs a native room's owned server: OpenCode itself, or the
/// Headroom wrapper around it. `headroom wrap` takes the tool name, then its
/// own flags; the server arguments follow and reach OpenCode unchanged.
fn native_server_command(spec: &RoomSpec, headroom: bool) -> (OsString, Vec<OsString>) {
    if !headroom {
        return (spec.program.clone(), Vec::new());
    }
    // Resolve against the room's PATH: the spawned process would otherwise be
    // found through Crowded's own environment instead of the room's.
    let path = environment_value(&spec.variables, "PATH");
    let path_ext = environment_value(&spec.variables, "PATHEXT");
    let wrapper = crate::command::program_on_path(
        OsStr::new("headroom"),
        path.as_deref(),
        path_ext.as_deref(),
    )
    .map(PathBuf::into_os_string)
    .unwrap_or_else(|_| OsString::from("headroom"));
    let mut prefix = Vec::with_capacity(spec.headroom_args.len() + 3);
    prefix.push(OsString::from("wrap"));
    prefix.push(spec.program.clone());
    prefix.extend(spec.headroom_args.iter().cloned());
    // Without the separator Headroom's own click parser would consume the
    // server's `--port 0` (a colliding option) instead of passing it on.
    prefix.push(OsString::from("--"));
    (wrapper, prefix)
}

/// Whether a room's launch is wrapped through `headroom`, decided from the
/// spec alone (the same PATH/PATHEXT resolution and binary check that
/// `Pane::spawn` performs). The startup lane needs this decision before a
/// pane exists, so it can serialize Headroom bootstrap on Windows without
/// first spawning the wrapper to find out.
pub(crate) fn spec_uses_headroom(spec: &RoomSpec) -> bool {
    let path = environment_value(&spec.variables, "PATH");
    let path_ext = environment_value(&spec.variables, "PATHEXT");
    headroom_launch(
        &spec.program,
        &spec.args,
        spec.use_headroom,
        &spec.headroom_args,
        &path,
        &path_ext,
    )
    .2
}

/// Apply each guest's "resume most recent conversation" flag ahead of
/// spawn, for the `crowded resume` CLI entry point. Guests without a
/// Conductor adapter (shell rooms, unrecognized programs) are left
/// unchanged rather than failing the whole launch over one room.
///
/// Returns, per spec, whether resume args were actually applied. The
/// caller uses this to tell which panes genuinely resumed and can skip
/// their intro whisper. A room that was refused its recorded session
/// starts fresh, so it reports `false` and receives the normal intro and
/// session capture despite `add_resume_args` having succeeded.
pub(crate) fn resume_supported_specs(specs: &mut [RoomSpec]) -> Vec<bool> {
    // Runs across the whole slate first: which OpenCode session belongs to
    // which room cannot be decided from one room's view alone.
    // Native-delivery rooms choose their own session before their terminal
    // starts, so they neither take part in the repair nor receive resume
    // arguments here.
    let slate: Vec<RoomSpec> = specs
        .iter()
        .filter(|spec| !spec.native_delivery)
        .cloned()
        .collect();
    let refused = controls::repair_opencode_session_mappings(&slate);
    specs
        .iter_mut()
        .map(|spec| {
            !spec.native_delivery && controls::add_resume_args(spec, &refused).unwrap_or(false)
        })
        .collect()
}

fn opencode_input_ready(screen: &str) -> bool {
    // Resumed sessions show prior conversation history in the viewport.
    // Checking the entire screen for "esc interrupt" is too strict: history
    // may contain that phrase from a previous thinking turn or from code
    // in the transcript, even though the current prompt is idle. Only
    // the prompt area (tail of the screen) matters, and only busy indicators
    // after the last prompt matter.
    // Ignore terminal padding, including blank rows above the version footer.
    // ponytail: last 40 meaningful rows; track prompt bounds if layouts need more.
    let mut lines: Vec<&str> = screen
        .trim_end()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .rev()
        .take(40)
        .collect();
    lines.reverse();
    // Narrow panes wrap the marker phrase itself across a line boundary
    // (e.g. "...ctrl+p" / "commands..."), so a single line's contents can't
    // be trusted alone. Join each line with the next one (space-separated,
    // collapsing the wrap) before searching for the marker.
    let ask_idx = (0..lines.len()).rposition(|i| {
        let joined = lines[i..(i + 3).min(lines.len())].join(" ");
        joined.contains("Ask anything")
    });
    let footer_idx = (0..lines.len()).rposition(|i| {
        let joined = lines[i..(i + 3).min(lines.len())].join(" ");
        joined.contains("ctrl+p") && joined.contains("commands")
    });
    // The v2 home prompt is centered, so pair it with the nearby status footer
    // instead of treating the final build-version row as the prompt position.
    let prompt_idx = match (ask_idx, footer_idx) {
        (Some(ask), Some(footer)) if footer >= ask && footer - ask <= 8 => Some(ask),
        (Some(ask), _) if lines.len() - ask <= 6 => Some(ask),
        (_, Some(footer)) => Some(footer),
        (Some(ask), None) => Some(ask),
        (None, None) => None,
    };
    let Some(idx) = prompt_idx else {
        return false;
    };
    let paired_home_prompt = ask_idx
        .is_some_and(|ask| footer_idx.is_some_and(|footer| footer >= ask && footer - ask <= 8));
    if !paired_home_prompt && lines.len() - idx > 6 {
        return false;
    }
    let prompt_area = lines[idx..].join("\n");
    !prompt_area.contains("esc interrupt")
        && !prompt_area.contains("again to interrupt")
        && !prompt_area.contains("exit shell mode")
}

#[cfg(not(windows))]
const RAW_SUBMIT_DELAY: Duration = Duration::from_millis(150);
#[cfg(windows)]
const RAW_SUBMIT_DELAY: Duration = Duration::from_secs(1);

// Windows raw-mode/ConPTY readers can consume a lone CR as a line-buffer
// terminator without ever delivering an Enter key, so a whisper's trailing
// submit silently drops and the note lands un-submitted. CRLF is the line
// terminator those readers reliably recognize, so Windows emits `\r\n`;
// Unix raw TUIs submit on a bare CR and must keep it. The submit stays a
// separate write, RAW_SUBMIT_DELAY after the bracketed-paste end marker,
// so it is never read as pasted content -- this constant only picks the
// byte form that survives the reader.
#[cfg(windows)]
const RAW_SUBMIT_BYTES: &[u8] = b"\r\n";
#[cfg(not(windows))]
const RAW_SUBMIT_BYTES: &[u8] = b"\r";

/// The text of a whisper as the guest sees it, whatever carries it.
pub(crate) fn whisper_note(source: &str, message: &str) -> String {
    format!("[whisper from {source}] {message}")
}

fn whisper_parts(
    transport: Transport,
    bracketed_paste: bool,
    source: &str,
    message: &str,
) -> (Vec<u8>, Option<Vec<u8>>) {
    let note = whisper_note(source, message);
    // ponytail: `Shell` currently means POSIX shell; add cmd/PowerShell
    // encoders only when Windows becomes a real target.
    match transport {
        // Shell rooms need a quoted command; raw rooms accept the note itself.
        Transport::Shell => (
            format!("printf '%s\\n' {}\r", shell_quote(&note)).into_bytes(),
            None,
        ),
        // A native paste event keeps long agent messages together before Enter.
        Transport::Raw if bracketed_paste => (
            format!("\x1b[200~{note}\x1b[201~").into_bytes(),
            Some(RAW_SUBMIT_BYTES.to_vec()),
        ),
        // Other raw TUIs may classify a rapid prompt+Enter sequence as one paste.
        Transport::Raw => (note.into_bytes(), Some(RAW_SUBMIT_BYTES.to_vec())),
    }
}

// `Box<dyn Child>` means "a heap-owned value implementing the Child trait".
// The Option lets cleanup `take()` ownership exactly once.
struct ChildGuard {
    child: Option<Box<dyn Child + Send + Sync>>,
    #[cfg(windows)]
    tree: Option<crate::command::ProcessTree>,
}

impl ChildGuard {
    fn new(
        child: Box<dyn Child + Send + Sync>,
        tree: Option<crate::command::ProcessTree>,
    ) -> io::Result<Self> {
        #[cfg(windows)]
        let mut child = child;
        #[cfg(windows)]
        let tree = match tree {
            Some(tree) => tree,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("Windows process tree is missing"));
            }
        };
        #[cfg(not(windows))]
        let _ = tree;
        Ok(Self {
            child: Some(child),
            #[cfg(windows)]
            tree: Some(tree),
        })
    }

    fn poll_exit(&mut self) -> io::Result<bool> {
        let exited = match self.child.as_mut() {
            Some(child) => child.try_wait()?.is_some(),
            None => true,
        };
        if exited {
            // `try_wait` has reaped the process, so the handle can go away.
            self.child.take();
            #[cfg(windows)]
            self.tree.take();
        }
        Ok(exited)
    }

    fn cleanup(&mut self) {
        // Taking the handle makes explicit cleanup and Drop safe to repeat.
        #[cfg(windows)]
        if let Some(tree) = self.tree.take() {
            if let Err(error) = tree.terminate() {
                eprintln!("crowded: failed to terminate the Windows job: {error}");
            }
        }
        if let Some(mut child) = self.child.take() {
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

// A pane's PTY writes go through one writer thread so a guest that stops
// reading its input cannot block the UI thread. The queue is bounded so a
// runaway producer fails fast instead of growing without limit.
// ponytail: 256 jobs is the ceiling; raise it only if a real workload needs
// deeper buffering, since each job also holds its bytes.
const WRITER_QUEUE_JOBS: usize = 256;

/// One unit of PTY input. `followup` carries an optional pause and trailing
/// bytes so a whisper's body, submit delay, and submit bytes are written as a
/// unit that no other queued write can interleave with.
struct WriteJob {
    bytes: Vec<u8>,
    followup: Option<(Duration, Vec<u8>)>,
}

/// Owns the sending end of a pane's write queue. Dropping it closes the queue,
/// then the writer thread ends after any in-flight write completes.
struct PaneWriter {
    jobs: mpsc::SyncSender<WriteJob>,
    failed: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    /// Jobs accepted but not yet fully written. Callers use it to avoid ageing
    /// a submit-tracking record while its whisper is still queued.
    unwritten: Arc<AtomicUsize>,
    /// Set when the owning pane is dropped: a writer stuck on a guest that
    /// stopped reading must not outlive its pane.
    stopped: Arc<AtomicBool>,
}

impl PaneWriter {
    fn spawn(inner: Box<dyn Write + Send>) -> Self {
        Self::spawn_stoppable(inner, Arc::new(AtomicBool::new(false)))
    }

    fn spawn_stoppable(mut inner: Box<dyn Write + Send>, stopped: Arc<AtomicBool>) -> Self {
        let (jobs, rx) = mpsc::sync_channel(WRITER_QUEUE_JOBS);
        let failed = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let unwritten = Arc::new(AtomicUsize::new(0));
        let failed_flag = Arc::clone(&failed);
        let finished_flag = Arc::clone(&finished);
        let unwritten_flag = Arc::clone(&unwritten);
        thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                let result = write_job(&mut *inner, job);
                // The job is no longer waiting, whatever the write result was.
                unwritten_flag.fetch_sub(1, Ordering::SeqCst);
                if result.is_err() {
                    failed_flag.store(true, Ordering::SeqCst);
                    break;
                }
            }
            finished_flag.store(true, Ordering::SeqCst);
        });
        Self {
            jobs,
            failed,
            finished,
            unwritten,
            stopped,
        }
    }

    fn enqueue(&self, job: WriteJob) -> io::Result<()> {
        if self.failed.load(Ordering::SeqCst) || self.finished.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "room writer has stopped",
            ));
        }
        // Count before the handoff so the writer thread can never decrement a
        // job the caller has not counted yet.
        self.unwritten.fetch_add(1, Ordering::SeqCst);
        match self.jobs.try_send(job) {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => {
                self.unwritten.fetch_sub(1, Ordering::SeqCst);
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "room is not reading its input",
                ))
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                self.unwritten.fetch_sub(1, Ordering::SeqCst);
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "room writer has stopped",
                ))
            }
        }
    }

    fn write_in_flight(&self) -> bool {
        self.unwritten.load(Ordering::SeqCst) > 0
            && !self.failed.load(Ordering::SeqCst)
            && !self.finished.load(Ordering::SeqCst)
    }
}

impl Drop for PaneWriter {
    fn drop(&mut self) {
        // The pane is going away; a writer waiting on a guest that stopped
        // reading returns from its wait and ends the thread.
        self.stopped.store(true, Ordering::SeqCst);
    }
}

fn write_job(inner: &mut dyn Write, job: WriteJob) -> io::Result<()> {
    inner.write_all(&job.bytes)?;
    inner.flush()?;
    if let Some((pause, bytes)) = job.followup {
        thread::sleep(pause);
        inner.write_all(&bytes)?;
        inner.flush()?;
    }
    Ok(())
}

// Lets `respond_to_terminal_queries` keep taking `&mut dyn Write`: a reply is
// enqueued like any other write instead of running on the caller's thread.
impl Write for PaneWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.enqueue(WriteJob {
            bytes: buf.to_vec(),
            followup: None,
        })?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// One Pane owns everything required to drive one child terminal.
pub(crate) struct Pane {
    spec: RoomSpec,
    /// The effective absolute working directory this room launches in; also
    /// the key (with vendor) used to persist its captured session id.
    cwd: PathBuf,
    /// The instant this pane's guest process was spawned. Discovery for the
    /// session-id capture is anchored to this timestamp, not to the later
    /// intro-sent event: Codex writes its session artifact at process spawn
    /// (before the intro is delivered) and a `since` taken at intro time
    /// would wrongly reject it; OpenCode's row is committed much later (at
    /// first message) but must still postdate this spawn, not an earlier one
    /// in the same directory.
    spawned_at: SystemTime,
    /// Shared confirmation that the background session-id capture succeeded;
    /// written once by the capture thread, read per frame for the Room Pulse
    /// tag (plain in-memory, never a file read).
    captured_session: session_state::CapturedSession,
    environment: GuestEnvironment,
    child: ChildGuard,
    master: Box<dyn MasterPty + Send>,
    writer: PaneWriter,
    output_rx: mpsc::Receiver<Vec<u8>>,
    response_tail: Vec<u8>,
    parser: Parser,
    /// Rows of retained history scrolled into view (0 = live bottom). The
    /// offset lives in the app's event handling and is pushed into the parser
    /// only while a frame is being drawn, so automation that reads the live
    /// screen (readiness, whisper delivery) never sees the scrolled view.
    scroll_offset: usize,
    headroom_active: bool,
    _opencode_config: Option<tempfile::NamedTempFile>,
    /// The owned OpenCode server behind a native-delivery room. Dropping the
    /// pane hands the server to a reaper thread instead of killing it inline.
    native: Option<NativeRoom>,
    /// Whether the no-session-report notice was already shown for this launch.
    codex_no_report_notice: bool,
    /// When this launch last walked the Codex rollout tree for an unverified
    /// report. `None` until the first check.
    pub(crate) rollout_check_at: Option<Instant>,
}

#[derive(Clone)]
pub(crate) struct GuestEnvironment {
    variables: Vec<(&'static str, OsString)>,
}

impl GuestEnvironment {
    pub(crate) fn new<const N: usize>(variables: [(&'static str, OsString); N]) -> Self {
        Self {
            variables: variables.into(),
        }
    }
}

/// Whether this native room runs on a Crowded-owned OpenCode server. A Codex
/// native room keeps its own process and is queued by its captured thread.
fn owned_server_room(spec: &RoomSpec) -> bool {
    spec.native_delivery
        && matches!(
            controls::cli_vendor(spec),
            Ok(controls::CliVendor::OpenCode)
        )
}

fn opencode_launch(spec: &RoomSpec) -> io::Result<RoomSpec> {
    let mut launch = spec.clone();
    let guest = Path::new(spec.program.as_os_str())
        .file_name()
        .unwrap_or(spec.program.as_os_str());
    if !guest.to_string_lossy().eq_ignore_ascii_case("opencode") {
        return Ok(launch);
    }
    if let Some(model) = controls::scan_option(&spec.args, &["--model", "-m"]) {
        let existing = environment_value(&spec.variables, "OPENCODE_CONFIG_CONTENT")
            .map(|value| value.to_string_lossy().into_owned());
        let config =
            crate::config::opencode_mcp_config(existing.as_deref(), &[], &[], Some(&model))?;
        launch
            .variables
            .retain(|(key, _)| key != "OPENCODE_CONFIG_CONTENT");
        launch
            .variables
            .push(("OPENCODE_CONFIG_CONTENT".into(), config.into()));
    }
    controls::strip_options(&mut launch.args, &["--model", "-m"]);
    if !launch.args.iter().any(|arg| arg == "--standalone") {
        launch.args.insert(0, "--standalone".into());
    }
    Ok(launch)
}

/// Points a launch that `opencode_launch` prepared at a Crowded-owned server
/// instead of OpenCode's private standalone one.
fn attach_to_native_server(launch: &mut RoomSpec, native: &NativeRoom) -> io::Result<()> {
    let Some(position) = launch.args.iter().position(|arg| arg == "--standalone") else {
        return Err(io::Error::other("native delivery needs an OpenCode launch"));
    };
    launch
        .args
        .splice(position..=position, native.tui_arguments());
    Ok(())
}

/// The server password reaches the TUI only through its environment, set on
/// the command itself so it is never stored in a room spec.
fn native_environment(command: &mut CommandBuilder, native: &NativeRoom) {
    command.env("OPENCODE_PASSWORD", native.tui_password());
}

/// The environment a native room's terminal runs with: the launch variables
/// (room configuration, model override) and the doorbell variables. The owned
/// server runs the agent, so it must see exactly this, not only the parent's
/// environment. The password is added separately and cannot be overridden.
fn native_variables(
    spec: &RoomSpec,
    environment: &GuestEnvironment,
) -> io::Result<native_launch::Variables> {
    let mut variables = opencode_launch(spec)?.variables;
    for (key, value) in &environment.variables {
        variables.retain(|(existing, _)| existing != OsStr::new(key));
        variables.push((OsString::from(*key), value.clone()));
    }
    variables.retain(|(key, _)| key != OsStr::new("OPENCODE_PASSWORD"));
    Ok(variables)
}

/// Where the native outbox keeps its saved mail: the private runtime state
/// directory, so mail survives a restart of Crowded.
pub(crate) fn native_outbox_path() -> PathBuf {
    session_state::state_directory().join("native-outbox.json")
}

/// Opens the outbox and binds every native room to it. Rooms without the
/// setting make this a no-op, so their delivery is exactly what it was.
pub(crate) fn start_native_router(
    specs: &[RoomSpec],
    natives: &[Option<NativeRoom>],
) -> io::Result<Option<crate::native_route::NativeRouter>> {
    // A native Codex room has no owned server to hand over, but it still needs
    // the outbox: its mail must be held, never pasted.
    if !specs.iter().any(|spec| spec.native_delivery) {
        return Ok(None);
    }
    let rooms: Vec<crate::native_route::RoomEntry> = specs
        .iter()
        .zip(natives)
        .enumerate()
        .filter_map(|(index, (spec, room))| {
            room.as_ref().map(|room| crate::native_route::RoomEntry {
                index,
                title: spec.title.clone(),
                binding: room.binding().clone(),
                backend: crate::native_route::Backend::OpenCode(room.server()),
            })
        })
        .collect();
    crate::native_route::NativeRouter::start(native_outbox_path(), rooms).map(Some)
}

/// What the launch phase knows about a native Codex room's session.
pub(crate) enum CodexMint {
    /// A session exists for this room, either reopened from the store or
    /// freshly minted.
    Ready {
        uuid: String,
        /// True when the stored session was reopened, false for a new session.
        #[allow(dead_code)] // Production binds either way; tests assert which path ran.
        reused: bool,
    },
    /// The exec phase is running; its result arrives on this channel.
    #[allow(dead_code)] // Retired mint path, still covered by tests.
    Pending(mpsc::Receiver<Result<Minted, String>>),
    /// The TUI is starting with the house rules as its positional prompt; its
    /// session id will arrive from the room's own SessionStart hook. The
    /// generation travels to the hook so its report can name this launch.
    Unreported { prompt: String, generation: u64 },
    /// No session was created, so the room must not start.
    Failed(String),
}

/// A finished mint: the new thread and the room identity that will own it.
/// The session is saved when the result is consumed, so a launch that has been
/// superseded can never overwrite a newer session.
pub(crate) struct Minted {
    uuid: String,
    cwd: PathBuf,
    title: String,
}

/// A mint that completed after the room set was already running.
pub(crate) enum CodexUpdate {
    Minted { index: usize },
    Failed { index: usize, reason: String },
}

/// How a native Codex room comes back when it is relaunched in place.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodexRelaunch {
    /// Keep a trusted stored session; mint one only when there is none.
    Restart,
    /// Always start a new thread, after clearing the old one durably.
    Clear,
    /// Resume the stored session, and fail when there is none.
    Resume,
}

/// How a native Claude room's next launch should treat its session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaudeRelaunch {
    /// Keep a stored session; choose a fresh one only when there is none.
    Restart,
    /// Start a new session after clearing the old one durably.
    Clear,
    /// Resume the stored session, and fail when there is none.
    Resume,
}

/// The hold reason shown while a room's session is still being created.
pub(crate) const CODEX_STARTING: &str = "`codex exec` is still creating this room's session";

/// The variables a Codex phase runs with: the room's own, then Crowded's, so
/// the exec phase sees exactly what the terminal will.
fn codex_phase_variables(
    spec: &RoomSpec,
    environment: &GuestEnvironment,
) -> Vec<(OsString, OsString)> {
    let mut variables = spec.variables.clone();
    for (key, value) in &environment.variables {
        variables.retain(|(existing, _)| existing != OsStr::new(key));
        variables.push((OsString::from(*key), value.clone()));
    }
    variables
}

/// A room's capability matrix before a pane exists (its launch does not need
/// the pane's screen or process).
pub(crate) fn spec_capabilities(spec: &RoomSpec) -> RoomCapabilities {
    controls::capabilities(spec)
}

/// A room's configured model before a pane exists.
pub(crate) fn spec_model(spec: &RoomSpec) -> Option<String> {
    controls::current_model(spec)
}

/// A room's configured effort before a pane exists.
pub(crate) fn spec_effort(spec: &RoomSpec) -> Option<String> {
    controls::current_effort(spec)
}

/// The program and arguments a Codex phase launches with. The exec mint and the
/// resumed terminal share this decision, so Headroom cannot apply to one and
/// not the other.
fn codex_phase_command(spec: &RoomSpec, arguments: &[OsString]) -> (OsString, Vec<OsString>) {
    let path = environment_value(&spec.variables, "PATH");
    let path_ext = environment_value(&spec.variables, "PATHEXT");
    let (program, arguments, _) = headroom_launch(
        &spec.program,
        arguments,
        spec.use_headroom,
        &spec.headroom_args,
        &path,
        &path_ext,
    );
    (program, arguments)
}

/// Points a room's terminal at the session it was given.
pub(crate) fn resume_session(spec: &mut RoomSpec, uuid: &str) -> io::Result<()> {
    controls::set_resume_session(spec, uuid)
}

/// Whether a reported Codex thread has a rollout that belongs to this cwd.
pub(crate) fn codex_rollout_verified(cwd: &Path, session: &str) -> bool {
    controls::codex_rollout_verified(cwd, session)
}

/// A missing rollout is retried; a rollout in another directory is rejected.
pub(crate) fn codex_rollout_status(cwd: &Path, session: &str) -> controls::CodexRollout {
    controls::codex_rollout_status(cwd, session)
}

/// The exact command that mints a room's session: the exec-compatible part of
/// the room's launch options, through the same wrapper as its terminal.
#[allow(dead_code)] // Retired mint path, still covered by tests.
fn mint_command(spec: &RoomSpec, prompt: &str) -> (OsString, Vec<OsString>) {
    let exec = codex_native::exec_arguments(&spec.args, prompt);
    codex_phase_command(spec, &exec)
}

/// Whether a spec is a native room Crowded drives as a Claude process.
pub(crate) fn claude_native_spec(spec: &RoomSpec) -> bool {
    spec.native_delivery && matches!(controls::cli_vendor(spec), Ok(controls::CliVendor::Claude))
}

/// A native Claude room's session is learned from the room's hook, so a
/// user-supplied resume form is refused instead of silently replaced.
fn refuse_claude_session_args(spec: &RoomSpec) -> io::Result<()> {
    const OWNED: [&str; 6] = [
        "--resume",
        "-r",
        "--continue",
        "-c",
        "--session-id",
        "--fork-session",
    ];
    let conflict = spec.args.iter().any(|argument| {
        let argument = argument.to_string_lossy();
        OWNED
            .iter()
            .any(|owned| argument == *owned || argument.starts_with(&format!("{owned}=")))
    });
    if !conflict {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "{}: a native Claude room's session is learned from the room; remove --resume, --continue, --session-id and --fork-session from its arguments",
            spec.title
        ),
    ))
}

/// The Claude config directory a room's own process sees: `CLAUDE_CONFIG_DIR`
/// when its launch environment sets one, otherwise `$HOME/.claude`. The
/// variables are the room's, so a room launched with its own config directory
/// is checked against that directory and not against Crowded's. If neither can
/// be determined, existence checks fail safe.
fn claude_config_dir(variables: &[(OsString, OsString)]) -> Option<PathBuf> {
    if let Some(dir) = environment_value(variables, "CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    environment_value(variables, "HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".claude"))
}

/// The `projects` directory name Claude derives from a cwd: every character
/// that is not an ASCII letter or digit becomes `-`, so
/// `/Users/bruno/.meridian` maps to `-Users-bruno--meridian`. A cwd Claude
/// encodes differently (very long paths it may truncate) matches no directory
/// and therefore never resumes; only an exact name is accepted.
fn claude_project_dir_name(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Whether the exact Claude session file for this room's cwd exists. Claude
/// stores sessions at `<config>/projects/<cwd-encoded>/<session-id>.jsonl`.
/// If the layout or config dir cannot be determined, this fails safe to
/// `false`.
fn claude_session_file_exists(
    cwd: &Path,
    session: &str,
    variables: &[(OsString, OsString)],
) -> bool {
    if !codex_native::valid_uuid(session) {
        return false;
    }
    let Some(config) = claude_config_dir(variables) else {
        return false;
    };
    let encoded = claude_project_dir_name(cwd);
    if encoded.is_empty() {
        return false;
    }
    config
        .join("projects")
        .join(encoded)
        .join(format!("{session}.jsonl"))
        .is_file()
}

/// The reported Claude session that is safe to resume: only when the room's
/// hook reported it and its exact session file exists for this cwd. A reported
/// id whose file cannot be found is not resumed this launch, but it is kept:
/// the check can only fail safe, so a miss must never erase what the room
/// reported. The next report replaces it.
fn stored_claude_session(
    cwd: &Path,
    title: &str,
    variables: &[(OsString, OsString)],
) -> Option<String> {
    let session = session_state::lookup_reported("claude", cwd, title)
        .filter(|id| codex_native::valid_uuid(id))?;
    claude_session_file_exists(cwd, &session, variables).then_some(session)
}

/// The mod Crowded embeds for its Claude rooms: the manifest, the hook
/// registration and the delivery script, exactly as validated.
const MOD_MANIFEST: &str = include_str!("claude_mod/.claude-plugin/plugin.json");
const MOD_HOOKS: &str = include_str!("claude_mod/hooks/hooks.json");
const MOD_DELIVER: &str = include_str!("claude_mod/hooks/deliver.mjs");
const CLAUDE_MOD_FILES: &[(&str, &str)] = &[
    (".claude-plugin/plugin.json", MOD_MANIFEST),
    ("hooks/hooks.json", MOD_HOOKS),
    ("hooks/deliver.mjs", MOD_DELIVER),
];

/// The digest of the embedded mod bundle, so a launch rewrites the extracted
/// files only when their content differs.
fn claude_mod_digest() -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for (name, contents) in CLAUDE_MOD_FILES {
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(contents.as_bytes());
        digest.update([0]);
    }
    format!("{:x}", digest.finalize())
}

/// The digest of the extracted files, or `None` when one is missing or
/// unreadable.
fn extracted_mod_digest(dir: &Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for (name, _) in CLAUDE_MOD_FILES {
        let contents = fs::read(dir.join(name)).ok()?;
        digest.update(name.as_bytes());
        digest.update([0]);
        digest.update(&contents);
        digest.update([0]);
    }
    Some(format!("{:x}", digest.finalize()))
}

/// Extracts the embedded Claude mod into the room's directory, rewriting the
/// files only when their content digest differs. A room whose mod cannot be
/// written must not start, like the other Claude start failures.
fn ensure_claude_mod(cwd: &Path) -> io::Result<PathBuf> {
    let dir = cwd.join(".crowded").join("claude-mod");
    session_state::create_private_directory(&dir)?;
    if extracted_mod_digest(&dir).as_deref() != Some(&claude_mod_digest()) {
        for (name, contents) in CLAUDE_MOD_FILES {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                session_state::create_private_directory(parent)?;
            }
            crate::claude_inbox::write_private(&path, contents)?;
        }
    }
    Ok(dir)
}

/// Extracts the mod and equips the spec, in one step so no launch path can
/// pass the flag without the files behind it. A spec with an explicit user
/// `--plugin-dir` keeps it and gets no Crowded mod; Crowded's own flag from
/// an earlier launch is replaced so the order stays deterministic.
fn equip_claude_mod(spec: &mut RoomSpec, cwd: &Path) -> io::Result<()> {
    let dir = cwd.join(".crowded").join("claude-mod");
    let own = dir.to_string_lossy().into_owned();
    let mut kept: Vec<OsString> = Vec::with_capacity(spec.args.len());
    let mut skipped = spec.args.iter();
    while let Some(arg) = skipped.next() {
        let text = arg.to_string_lossy();
        if text == "--plugin-dir" {
            let pair = skipped.next();
            if pair.is_some_and(|value| value.to_string_lossy() == own) {
                continue;
            }
            // A user value: keep the flag and stop managing this spec.
            return Ok(());
        }
        if text == format!("--plugin-dir={own}") {
            continue;
        }
        if text.starts_with("--plugin-dir=") {
            return Ok(());
        }
        kept.push(arg.clone());
    }
    spec.args = kept;
    let dir = ensure_claude_mod(cwd)?;
    spec.args.push("--plugin-dir".into());
    spec.args.push(dir.to_string_lossy().into_owned().into());
    Ok(())
}

/// What a native Claude room's launch decided before its terminal starts.
#[derive(Clone, Debug)]
pub(crate) enum ClaudeStart {
    /// Resume a session the room's hook reported earlier.
    Resume(String),
    /// Start fresh and learn the session from the room's hook.
    Fresh,
    /// The room must not start, with the reason to show.
    Failed(String),
}

/// Chooses each native Claude room's session before its terminal starts: with
/// `resume`, reopen a session the room's hook reported; otherwise start fresh
/// and learn the session from the hook. A plain launch never reads the store.
pub(crate) fn start_claude_sessions(
    specs: &mut [RoomSpec],
    resume: bool,
) -> (Vec<Option<ClaudeStart>>, Vec<String>) {
    let mut starts = (0..specs.len()).map(|_| None).collect::<Vec<_>>();
    let mut notices = Vec::new();
    for (index, spec) in specs.iter_mut().enumerate() {
        if !claude_native_spec(spec) {
            continue;
        }
        let prepared: io::Result<ClaudeStart> = (|| {
            refuse_claude_session_args(spec)?;
            let cwd = working_directory(spec.cwd.as_deref())?;
            controls::clear_resume_args(spec)?;
            let start = match resume
                .then(|| stored_claude_session(&cwd, &spec.title, &spec.variables))
                .flatten()
            {
                Some(stored) => {
                    controls::set_resume_session(spec, &stored)?;
                    ClaudeStart::Resume(stored)
                }
                None => ClaudeStart::Fresh,
            };
            equip_claude_mod(spec, &cwd)?;
            Ok(start)
        })();
        match prepared {
            Ok(start) => starts[index] = Some(start),
            Err(error) => {
                let reason = format!("{}: {error}", spec.title);
                notices.push(format!("{reason}; the room was not started"));
                starts[index] = Some(ClaudeStart::Failed(reason));
            }
        }
    }
    (starts, notices)
}

/// Binds one Claude room under a new launch generation.
#[cfg(test)]
pub(crate) fn bind_claude_room(
    router: &mut crate::native_route::NativeRouter,
    panes: &mut [Pane],
    index: usize,
    session: &str,
) -> io::Result<()> {
    let generation = next_generation()?;
    bind_claude_room_at(router, panes, index, session, generation)
}

/// The launch generation a Claude binding is given, allocated before anything
/// is persisted so a failure here writes nothing.
pub(crate) fn next_generation() -> io::Result<u64> {
    GenerationAllocator::new(session_state::state_directory()).next()
}

#[cfg(test)]
pub(crate) fn bind_claude_room_at(
    router: &mut crate::native_route::NativeRouter,
    panes: &mut [Pane],
    index: usize,
    session: &str,
    generation: u64,
) -> io::Result<()> {
    let title = panes[index].title().to_owned();
    router
        .claude_inbox(index)?
        .attach(panes[index].cwd(), index + 1, session, generation)?;
    panes[index].record_session(session);
    router.bind_claude(vec![crate::native_route::ClaudeEntry {
        index,
        title,
        session: Some(session.to_owned()),
        generation,
        reason: None,
    }])
}

/// Binds every native Claude room before its terminal starts. A room resuming a
/// reported session is bound to it; a fresh room starts unbound and learns its
/// session from the hook.
pub(crate) fn bind_claude_rooms(
    router: &mut crate::native_route::NativeRouter,
    specs: &[RoomSpec],
    starts: &mut [Option<ClaudeStart>],
) -> io::Result<Vec<String>> {
    let mut entries = Vec::new();
    let mut notices = Vec::new();
    for (index, start) in starts.iter_mut().enumerate() {
        let Some(current) = start else {
            continue;
        };
        let generation = GenerationAllocator::new(session_state::state_directory()).next()?;
        match current {
            ClaudeStart::Failed(reason) => {
                entries.push(crate::native_route::ClaudeEntry {
                    index,
                    title: specs[index].title.clone(),
                    session: None,
                    generation,
                    reason: Some(reason.clone()),
                });
            }
            ClaudeStart::Resume(session) => {
                let cwd = working_directory(specs[index].cwd.as_deref())?;
                let prepared = router
                    .claude_inbox(index)
                    .and_then(|inbox| inbox.attach(&cwd, index + 1, session, generation));
                if let Err(error) = prepared {
                    let reason = format!("{}: {error}", specs[index].title);
                    notices.push(format!("{reason}; the room was not started"));
                    *current = ClaudeStart::Failed(reason.clone());
                    entries.push(crate::native_route::ClaudeEntry {
                        index,
                        title: specs[index].title.clone(),
                        session: None,
                        generation,
                        reason: Some(reason),
                    });
                    continue;
                }
                entries.push(crate::native_route::ClaudeEntry {
                    index,
                    title: specs[index].title.clone(),
                    session: Some(session.clone()),
                    generation,
                    reason: None,
                });
            }
            ClaudeStart::Fresh => {
                let cwd = working_directory(specs[index].cwd.as_deref())?;
                let room = index + 1;
                let prepared = router
                    .claude_inbox(index)
                    .and_then(|inbox| inbox.prepare(&cwd, room, generation));
                if let Err(error) = prepared {
                    let reason = format!("{}: {error}", specs[index].title);
                    notices.push(format!("{reason}; the room was not started"));
                    *current = ClaudeStart::Failed(reason.clone());
                    entries.push(crate::native_route::ClaudeEntry {
                        index,
                        title: specs[index].title.clone(),
                        session: None,
                        generation,
                        reason: Some(reason),
                    });
                    continue;
                }
                // Bound for delivery through the per-room bootstrap inbox before
                // the hook reports the real Claude session; the placeholder is
                // internal and never passed to Claude or persisted as a session.
                entries.push(crate::native_route::ClaudeEntry {
                    index,
                    title: specs[index].title.clone(),
                    session: Some(crate::native_session::unreported_session(
                        &room.to_string(),
                        generation,
                    )),
                    generation,
                    reason: None,
                });
            }
        }
    }
    router.bind_claude(entries)?;
    Ok(notices)
}

/// Whether a spec is a native room that receives mail through Codex.
pub(crate) fn codex_native_spec(spec: &RoomSpec) -> bool {
    spec.native_delivery && matches!(controls::cli_vendor(spec), Ok(controls::CliVendor::Codex))
}

#[cfg(not(test))]
#[allow(dead_code)] // Retired mint path, still covered by tests.
const CODEX_MINT_TIMEOUT: Duration = Duration::from_secs(180);
#[allow(dead_code)]
#[cfg(test)]
const CODEX_MINT_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts each native Codex room's session without waiting for any of them.
///
/// With `resume`, a room whose stored session (minted, or reported by the
/// room's hook) still has its rollout on disk reopens it and runs no exec. A
/// plain launch never reads the store, so it always starts a new conversation.
/// Every other room spawns a worker that creates a session with `codex exec`
/// and its initial prompt, saves it durably, and reports the UUID. Each mint is
/// bounded on its own worker, so a slow room never delays another.
pub(crate) fn start_codex_mints(
    specs: &mut [RoomSpec],
    _environments: &[GuestEnvironment],
    prompts: &[Option<String>],
    resume: bool,
) -> (Vec<Option<CodexMint>>, Vec<String>) {
    let mut results: Vec<Option<CodexMint>> = (0..specs.len()).map(|_| None).collect();
    let mut notices = Vec::new();
    for (index, spec) in specs.iter_mut().enumerate() {
        if !codex_native_spec(spec) {
            continue;
        }
        let cwd = match working_directory(spec.cwd.as_deref()) {
            Ok(cwd) => cwd,
            Err(error) => {
                results[index] = Some(CodexMint::Failed(format!(
                    "could not resolve {}: {error}",
                    spec.title
                )));
                continue;
            }
        };
        // Only a session Crowded minted itself or the room's own hook reported
        // may be resumed; a generic capture from an older build is not proof
        // of ownership. Only a rollout still on disk makes it resumable.
        let stored = resume
            .then(|| {
                session_state::lookup_minted("codex", &cwd, &spec.title)
                    .or_else(|| session_state::lookup_reported("codex", &cwd, &spec.title))
            })
            .flatten()
            .filter(|id| codex_native::valid_uuid(id));
        if let Some(uuid) = stored {
            if controls::codex_rollout_verified(&cwd, &uuid) {
                results[index] = match controls::set_resume_session(spec, &uuid) {
                    Ok(()) => Some(CodexMint::Ready { uuid, reused: true }),
                    Err(_) => Some(CodexMint::Failed(format!(
                        "{}: could not prepare the stored session",
                        spec.title
                    ))),
                };
                continue;
            }
            // The stored entry stays until a successful mint replaces it, so a
            // failed exec still leaves the last good session to resume later.
            notices.push(format!(
                "{}: stored session {uuid} has no rollout on disk; starting a new session",
                spec.title
            ));
        }
        let prompt = match prompts.get(index).and_then(|prompt| prompt.clone()) {
            Some(prompt) => prompt,
            None => {
                results[index] = Some(CodexMint::Failed(format!(
                    "{}: no initial prompt was available",
                    spec.title
                )));
                continue;
            }
        };
        if let Err(error) = controls::clear_resume_args(spec) {
            results[index] = Some(CodexMint::Failed(format!("{}: {error}", spec.title)));
            continue;
        }
        let generation = match GenerationAllocator::new(session_state::state_directory()).next() {
            Ok(generation) => generation,
            Err(error) => {
                results[index] = Some(CodexMint::Failed(format!(
                    "{}: could not take a launch generation: {error}",
                    spec.title
                )));
                continue;
            }
        };
        results[index] = Some(CodexMint::Unreported { prompt, generation });
    }
    (results, notices)
}

/// Runs one room's `codex exec` mint on its own worker. The result is saved by
/// the consumer, never here: a worker whose launch has been replaced must not
/// persist its session.
#[allow(dead_code)] // Retired mint path, still covered by tests.
fn start_codex_mint(
    spec: &RoomSpec,
    cwd: PathBuf,
    environment: &GuestEnvironment,
    prompt: String,
) -> CodexMint {
    let (program, arguments) = mint_command(spec, &prompt);
    let variables = codex_phase_variables(spec, environment);
    let title = spec.title.clone();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result =
            codex_native::start_session(&program, &arguments, &cwd, &variables, CODEX_MINT_TIMEOUT)
                .map(|uuid| Minted { uuid, cwd, title });
        let _ = sender.send(result);
    });
    CodexMint::Pending(receiver)
}

/// Collects every mint that finished since the previous pass. The session is
/// saved here, on the consumer, before the room is reported ready; a mint whose
/// slot was replaced by a newer launch is never consumed and never saved.
pub(crate) fn poll_codex_mints(mints: &mut [Option<CodexMint>]) -> Vec<CodexUpdate> {
    let mut updates = Vec::new();
    for (index, slot) in mints.iter_mut().enumerate() {
        let Some(CodexMint::Pending(receiver)) = slot else {
            continue;
        };
        match receiver.try_recv() {
            Ok(Ok(minted)) => {
                match session_state::upsert_minted(
                    "codex",
                    &minted.cwd,
                    &minted.title,
                    &minted.uuid,
                ) {
                    Ok(()) => {
                        *slot = Some(CodexMint::Ready {
                            uuid: minted.uuid,
                            reused: false,
                        });
                        updates.push(CodexUpdate::Minted { index });
                    }
                    Err(error) => {
                        let reason =
                            format!("minted session {} could not be saved: {error}", minted.uuid);
                        *slot = Some(CodexMint::Failed(reason.clone()));
                        updates.push(CodexUpdate::Failed { index, reason });
                    }
                }
            }
            Ok(Err(reason)) => {
                *slot = Some(CodexMint::Failed(reason.clone()));
                updates.push(CodexUpdate::Failed { index, reason });
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                let reason = "`codex exec` stopped without a result".to_owned();
                *slot = Some(CodexMint::Failed(reason.clone()));
                updates.push(CodexUpdate::Failed { index, reason });
            }
        }
    }
    updates
}

/// Binds one native Codex room: the minted session when it is ready, otherwise
/// a held room with the reason it is not.
pub(crate) fn bind_codex_room(
    router: &mut crate::native_route::NativeRouter,
    panes: &mut [Pane],
    index: usize,
    mint: &CodexMint,
) -> io::Result<()> {
    let title = panes[index].title().to_owned();
    // Every launch allocates its own generation, so mail saved for an earlier
    // launch can never match this binding. A TUI-first launch already took its
    // generation when it was started, so the hook can name that same launch.
    let generation = match mint {
        CodexMint::Unreported { generation, .. } => *generation,
        _ => GenerationAllocator::new(session_state::state_directory()).next()?,
    };
    let (room, reason) = match mint {
        CodexMint::Ready { uuid, .. } => {
            panes[index].record_session(uuid);
            let room = crate::codex_native::room(
                panes[index].spec.program.clone(),
                panes[index].cwd(),
                uuid.clone(),
                panes[index].native_delivery_variables(),
            );
            (Some(Arc::new(room)), None)
        }
        CodexMint::Pending(_) => (None, Some(CODEX_STARTING.to_owned())),
        CodexMint::Unreported { .. } => {
            let placeholder =
                crate::native_session::unreported_session(&(index + 1).to_string(), generation);
            let room = crate::codex_native::room(
                panes[index].spec.program.clone(),
                panes[index].cwd(),
                placeholder,
                panes[index].native_delivery_variables(),
            );
            (Some(Arc::new(room)), None)
        }
        CodexMint::Failed(reason) => (None, Some(reason.clone())),
    };
    router.bind_codex(vec![crate::native_route::CodexEntry {
        index,
        title,
        room,
        generation,
        reason,
    }])?;
    if matches!(mint, CodexMint::Unreported { .. }) {
        router.mark_pending(index)?;
    }
    Ok(())
}

/// Binds every native Codex room at startup.
pub(crate) fn bind_codex_rooms(
    router: &mut crate::native_route::NativeRouter,
    panes: &mut [Pane],
    mints: &[Option<CodexMint>],
) -> io::Result<()> {
    for (index, mint) in mints.iter().enumerate() {
        if let Some(mint) = mint {
            bind_codex_room(router, panes, index, mint)?;
        }
    }
    Ok(())
}

/// `crowded resume` cannot reattach an owned-server room's earlier session yet,
/// so that room starts a new one; the user is told instead of it happening
/// silently. A native Codex room resumes the session Crowded stored for it.
pub(crate) fn native_resume_notice(specs: &[RoomSpec]) -> Option<String> {
    // Room numbers, not titles, so a long list still fits a narrow status line.
    let rooms: Vec<String> = specs
        .iter()
        .enumerate()
        .filter(|(_, spec)| owned_server_room(spec))
        .map(|(index, _)| (index + 1).to_string())
        .collect();
    if rooms.is_empty() {
        return None;
    }
    Some(format!(
        "Resume: native room {} started a new session",
        rooms.join(",")
    ))
}

/// Starts the Crowded-owned OpenCode server of every native-delivery room, in
/// parallel on tracked workers, and waits for them. This runs before the
/// terminal is taken over, so a slow start never freezes the event loop; a
/// room that cannot start fails the launch and cancels the others, with no
/// fallback to paste or a private server.
pub(crate) fn launch_native_rooms(
    specs: &[RoomSpec],
    environments: &[GuestEnvironment],
) -> io::Result<Vec<Option<NativeRoom>>> {
    debug_assert_eq!(specs.len(), environments.len());
    let named = |spec: &RoomSpec, error: io::Error| {
        io::Error::other(format!(
            "{}: native delivery could not start: {error}",
            spec.title
        ))
    };
    let mut launches = Vec::with_capacity(specs.len());
    let mut allocator = None;
    for (index, (spec, environment)) in specs.iter().zip(environments).enumerate() {
        if !owned_server_room(spec) {
            launches.push(None);
            continue;
        }
        let allocator = allocator
            .get_or_insert_with(|| GenerationAllocator::new(session_state::state_directory()));
        let launch = (|| {
            let cwd = working_directory(spec.cwd.as_deref())?;
            let mut variables = native_variables(spec, environment)?;
            let generation = allocator.next()?;
            let headroom = spec_uses_headroom(spec);
            let (program, prefix) = native_server_command(spec, headroom);
            // Headroom replaces the inline configuration of the process it
            // wraps, so the server reads the room's document from a file.
            let config = if headroom {
                write_opencode_config(&mut variables, spec.cwd.as_deref())?
            } else {
                None
            };
            Ok(native_launch::begin(native_launch::NativeLaunch {
                guest: spec.program.clone(),
                program,
                prefix,
                cwd,
                // The generation is part of the lane, so mail for an earlier
                // launch of this room can never block or reach the new one.
                room: format!("{}.{generation}", index + 1),
                generation,
                variables,
                config,
            }))
        })()
        .map_err(|error| named(spec, error))?;
        launches.push(Some(launch));
    }
    let deadline = Instant::now() + native_launch::LAUNCH_TIMEOUT;
    let mut rooms = Vec::with_capacity(specs.len());
    for (spec, launch) in specs.iter().zip(launches) {
        rooms.push(match launch {
            None => None,
            Some(launch) => {
                let remaining = deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1));
                Some(launch.wait(remaining).map_err(|error| named(spec, error))?)
            }
        });
    }
    Ok(rooms)
}

/// Builds one owned-server launch for reviving a dead room. The lane (room
/// key) stays the caller's, so held mail for it is adopted onto the new
/// binding like a clear; only the generation is new. Same variables,
/// headroom and config as startup, so the revived server runs exactly what a
/// fresh launch would.
pub(crate) fn opencode_revive_launch(
    spec: &RoomSpec,
    environment: &GuestEnvironment,
    room_key: String,
    generation: u64,
) -> io::Result<native_launch::NativeLaunch> {
    let cwd = working_directory(spec.cwd.as_deref())?;
    let mut variables = native_variables(spec, environment)?;
    let headroom = spec_uses_headroom(spec);
    let (program, prefix) = native_server_command(spec, headroom);
    let config = if headroom {
        write_opencode_config(&mut variables, spec.cwd.as_deref())?
    } else {
        None
    };
    Ok(native_launch::NativeLaunch {
        guest: spec.program.clone(),
        program,
        prefix,
        cwd,
        room: room_key,
        generation,
        variables,
        config,
    })
}

fn opencode_wrapped_config(
    guest: &mut RoomSpec,
    headroom_active: bool,
) -> io::Result<Option<tempfile::NamedTempFile>> {
    if !headroom_active
        || !Path::new(&guest.program)
            .file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("opencode"))
    {
        return Ok(None);
    }
    write_opencode_config(&mut guest.variables, guest.cwd.as_deref())
}

/// Headroom replaces `OPENCODE_CONFIG_CONTENT` with its own, so the room's
/// document is written to a private file that survives the wrapper.
fn write_opencode_config(
    variables: &mut Vec<(OsString, OsString)>,
    cwd: Option<&Path>,
) -> io::Result<Option<tempfile::NamedTempFile>> {
    let Some(content) = environment_value(variables, "OPENCODE_CONFIG_CONTENT") else {
        return Ok(None);
    };
    let mut room: serde_json::Value = serde_json::from_str(&content.to_string_lossy())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let values = room.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "OPENCODE_CONFIG_CONTENT must contain a JSON object",
        )
    })?;
    values.retain(|key, _| matches!(key.as_str(), "model" | "mcp"));
    let mut config_directory = None;
    let content = match environment_value(variables, "OPENCODE_CONFIG") {
        Some(path) => {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                working_directory(cwd)?.join(path)
            };
            let original = std::fs::read_to_string(&path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot read OpenCode config {}: {error}", path.display()),
                )
            })?;
            config_directory = path.parent().map(Path::to_path_buf);
            let mut base: serde_json::Value = serde_json::from_str(&original).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "OpenCode config {} must be plain JSON (JSONC is unsupported): {error}",
                        path.display()
                    ),
                )
            })?;
            if !base.is_object() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "OpenCode config {} must contain a JSON object",
                        path.display()
                    ),
                ));
            }
            merge_opencode_room_config(&mut base, room);
            serde_json::to_string(&base)?
        }
        None => serde_json::to_string(&room)?,
    };
    // Headroom replaces inline config; a private config document survives wrapping.
    let mut file = match config_directory {
        Some(directory) => tempfile::NamedTempFile::new_in(directory)?,
        None => tempfile::NamedTempFile::new()?,
    };
    file.write_all(content.as_bytes())?;
    file.flush()?;
    variables.retain(|(key, _)| key != "OPENCODE_CONFIG");
    variables.push(("OPENCODE_CONFIG".into(), file.path().as_os_str().into()));
    Ok(Some(file))
}

fn merge_opencode_room_config(base: &mut serde_json::Value, room: serde_json::Value) {
    match (base, room) {
        (serde_json::Value::Object(base), serde_json::Value::Object(room)) => {
            for (key, value) in room {
                if let Some(original) = base.get_mut(&key) {
                    merge_opencode_room_config(original, value);
                } else {
                    base.insert(key, value);
                }
            }
        }
        (base, room) => *base = room,
    }
}

fn opencode_model_switch_body(model: &str) -> io::Result<String> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "OpenCode model must have the form provider/model[#variant]",
        )
    };
    let mut parts = model.split('#');
    let reference = parts.next().ok_or_else(invalid)?;
    let variant = parts.next();
    if parts.next().is_some() {
        return Err(invalid());
    }
    let (provider, id) = reference.split_once('/').ok_or_else(invalid)?;
    if [provider, id]
        .iter()
        .any(|part| part.trim().is_empty() || part.chars().any(char::is_whitespace))
        || variant
            .is_some_and(|part| part.trim().is_empty() || part.chars().any(char::is_whitespace))
    {
        return Err(invalid());
    }

    let model = match variant {
        Some(variant) => serde_json::json!({
            "id": id,
            "providerID": provider,
            "variant": variant,
        }),
        None => serde_json::json!({ "id": id, "providerID": provider }),
    };
    serde_json::to_string(&serde_json::json!({ "model": model }))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

#[cfg(test)]
mod opencode_model_switch_body_tests {
    use super::opencode_model_switch_body;

    #[test]
    fn accepts_slash_containing_model_ids_and_variant() {
        for (reference, id, provider, variant) in [
            (
                "lmstudio/mistralai/devstral-small-2-2512",
                "mistralai/devstral-small-2-2512",
                "lmstudio",
                None,
            ),
            (
                "lmstudio/prism-ml/bonsai-27b",
                "prism-ml/bonsai-27b",
                "lmstudio",
                None,
            ),
            ("opencode-go/mimo-v2.5", "mimo-v2.5", "opencode-go", None),
            (
                "openrouter/anthropic/claude-sonnet-4.5#high",
                "anthropic/claude-sonnet-4.5",
                "openrouter",
                Some("high"),
            ),
        ] {
            let body: serde_json::Value =
                serde_json::from_str(&opencode_model_switch_body(reference).unwrap()).unwrap();
            let expected = match variant {
                Some(variant) => serde_json::json!({
                    "id": id,
                    "providerID": provider,
                    "variant": variant,
                }),
                None => serde_json::json!({ "id": id, "providerID": provider }),
            };
            assert_eq!(body["model"], expected, "{reference}");
        }
    }

    #[test]
    fn rejects_malformed_model_references() {
        for model in [
            "model",
            "/model",
            "provider/",
            "provider/model##high",
            "provider /model",
            "provider/model name",
            "provider/model#high low",
        ] {
            assert!(opencode_model_switch_body(model).is_err(), "{model}");
        }
    }
}

fn opencode_model_switch_args(session_id: &str, body: &str) -> Vec<OsString> {
    [
        "api".into(),
        "--standalone".into(),
        "--data".into(),
        body.into(),
        "POST".into(),
        format!("/api/session/{session_id}/model").into(),
    ]
    .into()
}

fn opencode_resume_session_id(spec: &RoomSpec, cwd: &Path) -> io::Result<Option<String>> {
    if controls::cli_vendor(spec)? != controls::CliVendor::OpenCode {
        return Ok(None);
    }
    let is_resuming = spec.args.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        matches!(arg.as_ref(), "--session" | "-s" | "--continue" | "-c")
            || arg.starts_with("--session=")
            || arg.starts_with("-s=")
    });
    if !is_resuming {
        return Ok(None);
    }
    if let Some(id) =
        controls::scan_option(&spec.args, &["--session", "-s"]).filter(|id| !id.is_empty())
    {
        return Ok(Some(id));
    }
    if let Some(id) = session_state::lookup("opencode", cwd, &spec.title)
        .filter(|id| id != session_state::FRESH_STATE_MARKER)
    {
        return Ok(Some(id));
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "cannot switch the model of a continued OpenCode session without a recorded session id; use clear first",
    ))
}

fn switch_opencode_session_model(
    spec: &RoomSpec,
    environment: &GuestEnvironment,
    cwd: &Path,
    session_id: &str,
    body: &str,
) -> io::Result<()> {
    let mut command = Command::new(&spec.program);
    command
        .args(opencode_model_switch_args(session_id, body))
        .current_dir(cwd)
        .env("PWD", cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in &spec.variables {
        command.env(key, value);
    }
    for (key, value) in &environment.variables {
        command.env(key, value);
    }
    let mut child = spawn_retrying_text_busy(&mut command).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to run OpenCode model switch: {error}"),
        )
    })?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(io::Error::other(format!(
                        "OpenCode model switch failed with exit status {status}"
                    )))
                };
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            Ok(None) => {}
        }
        if started.elapsed() >= Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "OpenCode model switch timed out after 15 seconds",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

impl Pane {
    pub(crate) fn spawn(
        spec: RoomSpec,
        size: PtySize,
        environment: GuestEnvironment,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_with_native(spec, size, environment, None)
    }

    /// A native-delivery room must be launched with its owned server, and no
    /// other room may be: nothing falls back to paste or a private server.
    pub(crate) fn spawn_with_native(
        spec: RoomSpec,
        size: PtySize,
        environment: GuestEnvironment,
        native: Option<NativeRoom>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut pane = Self::spawn_attached(&spec, size, environment, native.as_ref())?;
        pane.native = native;
        Ok(pane)
    }

    /// The same spawn, borrowing the server instead of taking it, so a caller
    /// that must keep the [`NativeRoom`] can build a replacement pane first and
    /// move the server in only once that succeeds.
    fn spawn_attached(
        spec: &RoomSpec,
        size: PtySize,
        environment: GuestEnvironment,
        native: Option<&NativeRoom>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Only an OpenCode native room runs on a Crowded-owned server. A Codex
        // native room keeps its own process and is queued through the CLI.
        let owned_server = owned_server_room(spec);
        if owned_server != native.is_some() {
            return Err(io::Error::other(if owned_server {
                "native delivery room has no owned server"
            } else {
                "owned server given to a room without native delivery"
            })
            .into());
        }
        // A native room's agent runs in its owned server, not in this TUI:
        // the server is wrapped, and the terminal only attaches to it.
        let native_room = native.is_some();
        // A PTY has two ends: the child receives the slave; we keep the master.
        let pty = native_pty_system().openpty(size)?;
        // portable-pty defaults an omitted cwd to HOME rather than inheriting
        // Crowded's directory, so the project directory must be explicit.
        let cwd = working_directory(spec.cwd.as_deref())?;
        let mut guest = opencode_launch(spec)?;
        if let Some(native) = &native {
            attach_to_native_server(&mut guest, native)?;
        }
        let path = environment_value(&guest.variables, "PATH");
        let path_ext = environment_value(&guest.variables, "PATHEXT");
        let (program, args, tui_headroom) = if codex_native_spec(spec) {
            let (program, args) = codex_phase_command(spec, &guest.args);
            (program, args, spec_uses_headroom(spec))
        } else {
            headroom_launch(
                &spec.program,
                &guest.args,
                spec.use_headroom && !native_room,
                &spec.headroom_args,
                &path,
                &path_ext,
            )
        };
        let headroom_active = if native_room {
            spec_uses_headroom(spec)
        } else {
            tui_headroom
        };
        let opencode_config = opencode_wrapped_config(&mut guest, tui_headroom)?;
        let launch =
            ResolvedCommand::resolve_with_environment(&program, &args, &cwd, path, path_ext)?
                .portable()?;
        let (mut command, tree) = launch.into_parts();
        command.cwd(&cwd);
        command.env("PWD", &cwd);
        for (key, value) in &guest.variables {
            command.env(key, value);
        }
        for (key, value) in &environment.variables {
            command.env(key, value);
        }
        if claude_native_spec(spec) {
            // A nested-session marker would make this Claude Code child stop
            // keeping transcripts, so the room never inherits one.
            command.env_remove("CLAUDE_CODE_CHILD_SESSION");
        }
        if let Some(native) = &native {
            native_environment(&mut command, native);
        }
        // Anchor session-id discovery to the instant the guest process spawns.
        // Codex writes its session artifact at spawn, before any intro is
        // delivered, so the capture `since` must be this instant; OpenCode's
        // row (committed much later) must still postdate it.
        let spawned_at = SystemTime::now();
        let child = ChildGuard::new(
            pty_spawn_retrying_text_busy(|| pty.slave.spawn_command(command.clone()))?,
            tree,
        )?;
        // The parent must not keep a second slave handle alive.
        drop(pty.slave);

        // Input and output use separate master handles so reading can block
        // on another thread while the UI remains responsive. Linux cannot end
        // a write blocked on a guest that stopped reading, so there the writer
        // waits on a non-blocking duplicate and stops with its pane.
        #[cfg(target_os = "linux")]
        let (writer, mut reader) = {
            let fd = pty
                .master
                .as_raw_fd()
                .ok_or_else(|| io::Error::other("the Linux pty master has no descriptor"))?;
            let stopped = Arc::new(AtomicBool::new(false));
            let writer_file = pty_io::nonblocking_dup(fd)?;
            let reader_file = pty_io::nonblocking_dup(fd)?;
            (
                PaneWriter::spawn_stoppable(
                    Box::new(pty_io::PtyWriter::new(writer_file, Arc::clone(&stopped))),
                    stopped,
                ),
                Box::new(pty_io::PtyReader::new(reader_file)) as Box<dyn Read + Send>,
            )
        };
        #[cfg(not(target_os = "linux"))]
        let (writer, mut reader) = (
            PaneWriter::spawn(pty.master.take_writer()?),
            pty.master.try_clone_reader()?,
        );
        let (output_tx, output_rx) = mpsc::channel();
        // `move` transfers the reader and sender into the new thread.
        thread::spawn(move || {
            let mut bytes = [0; 4096];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 || output_tx.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });

        // A resumed (or otherwise relaunched) room whose exact session id is
        // already persisted opens with that id seeded, so the Room Pulse cost
        // gate is true immediately instead of only after an intro-triggered
        // capture -- which a resume deliberately skips.
        let captured_session = match controls::cli_vendor(spec) {
            // A native room's session is the one it creates on this launch, so a
            // persisted legacy id is never trusted. Codex captures the new
            // thread itself; OpenCode's owned server creates it.
            Ok(vendor) if !spec.native_delivery => {
                session_state::initial_capture_cell(vendor.key(), &cwd, &spec.title)
            }
            _ => session_state::fresh_capture_cell(),
        };

        Ok(Self {
            spec: spec.clone(),
            cwd,
            spawned_at,
            captured_session,
            environment,
            child,
            master: pty.master,
            writer,
            output_rx,
            response_tail: Vec::new(),
            // vt100 assumes room for wrapping and a double-width character.
            parser: Parser::new(size.rows.max(2), size.cols.max(2), SCROLLBACK_LINES),
            scroll_offset: 0,
            headroom_active,
            _opencode_config: opencode_config,
            native: None,
            codex_no_report_notice: false,
            rollout_check_at: None,
        })
    }

    /// A native room whose session is still being created. It has no child and
    /// receives no input; its mail is held until the session arrives.
    pub(crate) fn spawn_pending(
        spec: RoomSpec,
        size: PtySize,
        environment: GuestEnvironment,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_placeholder(spec, size, environment, "starting session...")
    }

    /// A native room whose session could not be created. It has no child and
    /// never receives terminal input: the reason is painted into its screen and
    /// its mail is held by the router.
    pub(crate) fn spawn_unavailable(
        spec: RoomSpec,
        size: PtySize,
        environment: GuestEnvironment,
        reason: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::spawn_placeholder(spec, size, environment, reason)
    }

    fn spawn_placeholder(
        spec: RoomSpec,
        size: PtySize,
        environment: GuestEnvironment,
        message: &str,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let cwd = working_directory(spec.cwd.as_deref())?;
        let pty = native_pty_system().openpty(size)?;
        let writer = PaneWriter::spawn(pty.master.take_writer()?);
        // No reader and no sender: draining output ends immediately.
        let (_output_tx, output_rx) = mpsc::channel();
        let mut parser = Parser::new(size.rows.max(2), size.cols.max(2), SCROLLBACK_LINES);
        parser.process(format!("{message}\r\n").as_bytes());
        Ok(Self {
            spec,
            cwd,
            spawned_at: SystemTime::now(),
            captured_session: session_state::fresh_capture_cell(),
            environment,
            child: ChildGuard {
                child: None,
                #[cfg(windows)]
                tree: None,
            },
            master: pty.master,
            writer,
            output_rx,
            response_tail: Vec::new(),
            parser,
            scroll_offset: 0,
            headroom_active: false,
            _opencode_config: None,
            native: None,
            codex_no_report_notice: false,
            rollout_check_at: None,
        })
    }

    /// Best-effort capture of this pane's exact underlying vendor session id,
    /// anchored to this pane's own process spawn instant (not the intro-sent
    /// event) so a `since` taken later never wrongly rejects an artifact this
    /// spawn already wrote. Runs on a background thread bounded to
    /// `session_state::SESSION_CAPTURE_GRACE` (OpenCode uses its own, longer
    /// bound), so it never blocks the UI loop.
    /// Resumed panes skip the intro and therefore never call this.
    pub(crate) fn begin_session_capture(&self) -> bool {
        // A native room's session is the one its owned server created; legacy
        // discovery would pick a session by directory and time instead.
        if self.native.is_some() {
            return false;
        }
        let Ok(vendor) = controls::cli_vendor(&self.spec) else {
            return false;
        };
        // The room identity (spec.title) keys the persisted entry so this room
        // only ever resumes its own captured id, even when a sibling room
        // shares the same (vendor, cwd). The `since` anchor is this pane's
        // spawn instant: a ClearContext/Configure/restart respawns via
        // `spawn` (fresh `spawned_at`), so a new capture supersedes the stale
        // pre-clear id; a resume skips the intro and never gets here.
        let opencode_model = if vendor == controls::CliVendor::OpenCode {
            controls::current_model(&self.spec)
        } else {
            None
        };
        session_state::capture_async(
            vendor,
            self.cwd.clone(),
            self.spec.title.clone(),
            self.spawned_at,
            Arc::clone(&self.captured_session),
            opencode_model,
        );
        true
    }

    /// Whether this pane's exact session id has been captured yet. A plain
    /// in-memory read of the shared capture cell -- no disk I/O.
    pub(crate) fn has_captured_session(&self) -> bool {
        session_state::has_captured_session(&self.captured_session)
    }

    pub(crate) fn drain_output(&mut self) -> io::Result<bool> {
        // Drain everything currently waiting without blocking the UI thread.
        let mut received = false;
        while let Ok(bytes) = self.output_rx.try_recv() {
            received = true;
            // A cursor-position reply is best effort: a full or stopped write
            // queue must not fail output draining and end the app.
            let _ = respond_to_terminal_queries(&mut self.writer, &mut self.response_tail, &bytes);
            self.parser.process(&bytes);
        }
        Ok(received)
    }

    pub(crate) fn write_key(&mut self, key: KeyEvent) -> io::Result<()> {
        if let Some(bytes) = key_bytes(key) {
            self.write_bytes(&bytes)?;
        }
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.push_write(WriteJob {
            bytes: bytes.to_vec(),
            followup: None,
        })
    }

    fn push_write(&self, job: WriteJob) -> io::Result<()> {
        if !self.is_online() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "room is offline",
            ));
        }
        self.writer.enqueue(job)
    }

    pub(crate) fn is_online(&self) -> bool {
        self.child.child.is_some()
    }

    /// Whether any accepted pane input is still waiting to be written. A stopped
    /// writer reports false: it will never write the job, so callers must not
    /// wait on it.
    pub(crate) fn has_unwritten_input(&self) -> bool {
        self.writer.write_in_flight()
    }

    pub(crate) fn title(&self) -> &str {
        &self.spec.title
    }

    pub(crate) fn name(&self) -> &str {
        &self.spec.name
    }

    pub(crate) fn vendor(&self) -> &str {
        &self.spec.vendor
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub(crate) fn guest(&self) -> String {
        Path::new(self.spec.program.as_os_str())
            .file_name()
            .unwrap_or(self.spec.program.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    pub(crate) fn transport(&self) -> &'static str {
        match self.spec.transport {
            Transport::Shell => "shell",
            Transport::Raw => "raw",
        }
    }

    pub(crate) fn allows_control(&self) -> bool {
        self.spec.allow_control
    }

    /// Update `allow_control` in place so the change applies to the live pane
    /// without a respawn; `allows_control` reads `spec.allow_control` directly.
    pub(crate) fn set_allow_control(&mut self, value: bool) {
        self.spec.allow_control = value;
    }

    /// Replace the scheduling metadata in place so the config overlay stays
    /// consistent with what was persisted.
    pub(crate) fn set_scheduling(&mut self, scheduling: RoomScheduling) {
        self.spec.scheduling = Some(scheduling);
    }

    pub(crate) fn headroom_active(&self) -> bool {
        self.headroom_active
    }

    pub(crate) fn needs_intro(&self) -> bool {
        // The intro goes through the selected delivery path; a native room never
        // receives it as a terminal paste.
        self.spec.transport == Transport::Raw && !self.spec.native_delivery
    }

    pub(crate) fn screen(&self) -> &Screen {
        self.parser.screen()
    }

    /// The number of visible rows in this pane's viewport, used as the Page
    /// Up / Page Down step so each press scrolls exactly one screen.
    pub(crate) fn visible_height(&self) -> usize {
        usize::from(self.parser.screen().size().0)
    }

    /// The number of visible columns in this pane's viewport.
    #[cfg(test)]
    pub(crate) fn visible_width(&self) -> usize {
        usize::from(self.parser.screen().size().1)
    }

    /// Scroll the retained view toward the top of the buffer. `set_scrollback`
    /// clamps to the actual retained history, so this can never overshoot.
    pub(crate) fn scroll_up(&mut self, rows: usize) {
        let offset = self.scroll_offset.saturating_add(rows);
        self.parser.screen_mut().set_scrollback(offset);
        self.scroll_offset = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Scroll the retained view toward the live bottom. The offset saturates
    /// at zero, so repeated presses land on the live screen and stop.
    pub(crate) fn scroll_down(&mut self, rows: usize) {
        let offset = self.scroll_offset.saturating_sub(rows);
        self.parser.screen_mut().set_scrollback(offset);
        self.scroll_offset = self.parser.screen().scrollback();
        self.parser.screen_mut().set_scrollback(0);
    }

    /// Push this pane's scroll offset into the parser so the next frame
    /// renders the retained history. Called around the draw pass only.
    pub(crate) fn apply_scroll(&mut self) {
        self.parser.screen_mut().set_scrollback(self.scroll_offset);
    }

    /// Restore the parser to the live bottom after the frame. Automation
    /// reads (readiness, whisper delivery) must see the live screen, never
    /// a view the user scrolled away from.
    pub(crate) fn restore_scroll(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub(crate) fn is_alternate_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    pub(crate) fn forward_page_up(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[5~")
    }

    pub(crate) fn forward_page_down(&mut self) -> io::Result<()> {
        self.write_bytes(b"\x1b[6~")
    }

    pub(crate) fn forward_wheel(&mut self, up: bool) -> io::Result<()> {
        if up {
            self.forward_page_up()
        } else {
            self.forward_page_down()
        }
    }

    pub(crate) fn automation_input_ready(&self, output_is_quiet: bool) -> bool {
        let guest = Path::new(self.spec.program.as_os_str())
            .file_name()
            .unwrap_or(self.spec.program.as_os_str())
            .to_string_lossy();
        if guest.eq_ignore_ascii_case("opencode") {
            // OpenCode exposes its actual normal-mode prompt on the rendered
            // screen; silence alone also occurs during startup and shell mode.
            return opencode_input_ready(&self.parser.screen().contents());
        }
        output_is_quiet
    }

    /// The room's launch configuration, for a path that must start it again
    /// with a session that became available later.
    pub(crate) fn room_spec(&self) -> &RoomSpec {
        &self.spec
    }

    /// Whether this room's mail is meant to travel through the native outbox.
    pub(crate) fn is_native(&self) -> bool {
        self.spec.native_delivery
    }

    /// Whether this native room receives mail through `codex queue` rather
    /// than an owned OpenCode server.
    pub(crate) fn codex_native(&self) -> bool {
        codex_native_spec(&self.spec)
    }

    /// Whether this room is a Claude process Crowded gave its session.
    pub(crate) fn claude_native(&self) -> bool {
        claude_native_spec(&self.spec)
    }

    /// Prepares this native Claude room's next launch. A reported session is
    /// resumed; otherwise the room starts fresh and learns its session from the
    /// hook. An explicit resume with no reported session fails.
    pub(crate) fn relaunch_claude(
        &self,
        action: ClaudeRelaunch,
        room: usize,
    ) -> io::Result<(RoomSpec, Option<String>)> {
        let mut spec = self.spec.clone();
        let session = match action {
            ClaudeRelaunch::Clear => {
                session_state::clear_room("claude", &self.cwd, &self.spec.title)?;
                None
            }
            ClaudeRelaunch::Resume => {
                Some(stored_claude_session(
                    &self.cwd,
                    &self.spec.title,
                    &self.native_delivery_variables(),
                )
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "{}: no stored Claude session to resume; use control {room} clear to start a new one",
                        self.spec.title
                    ))
                })?)
            }
            ClaudeRelaunch::Restart => stored_claude_session(
                &self.cwd,
                &self.spec.title,
                &self.native_delivery_variables(),
            ),
        };
        controls::clear_resume_args(&mut spec)?;
        if let Some(session) = &session {
            controls::set_resume_session(&mut spec, session)?;
        }
        equip_claude_mod(&mut spec, &self.cwd)?;
        Ok((spec, session))
    }

    /// Records the session this room was given, so the pulse cost and detail
    /// read the right one without waiting for a capture.
    pub(crate) fn record_session(&self, uuid: &str) {
        if let Ok(mut guard) = self.captured_session.lock() {
            *guard = Some(uuid.to_owned());
        }
    }

    /// The session this pane is waiting to bind, if a report named one.
    pub(crate) fn captured_session_id(&self) -> Option<String> {
        self.captured_session
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    }

    pub(crate) fn codex_no_report_notice(&self) -> bool {
        self.codex_no_report_notice
    }

    pub(crate) fn set_codex_no_report_notice(&mut self) {
        self.codex_no_report_notice = true;
    }

    /// Runs the expensive rollout check only when this launch's retry slot is
    /// due. A late rollout is still found: the slot opens again every second.
    pub(crate) fn rollout_verified_when_due(
        &mut self,
        now: Instant,
        session: &str,
        verify: impl FnMut(&Path, &str) -> bool,
    ) -> bool {
        rollout_retry_check(&mut self.rollout_check_at, now, &self.cwd, session, verify)
    }

    /// The process was launched from a one-shot argv view; the pane keeps the
    /// room's base arguments and variables so a later clear cannot inherit the
    /// house-rules prompt or the launch-generation variable.
    pub(crate) fn restore_launch_view(
        &mut self,
        args: Vec<OsString>,
        variables: Vec<(OsString, OsString)>,
    ) {
        self.spec.args = args;
        self.spec.variables = variables;
    }

    /// The queue target for the room's current process after its hook reported
    /// a new thread: same program, directory and environment, new session.
    pub(crate) fn codex_room_for(&self, thread: String) -> codex_native::Room {
        codex_native::room(
            self.spec.program.clone(),
            &self.cwd,
            thread,
            self.native_delivery_variables(),
        )
    }

    /// The environment the queue command must see: the same variables as the
    /// room's own process, so it reads the same session store.
    pub(crate) fn native_delivery_variables(&self) -> Vec<(OsString, OsString)> {
        codex_phase_variables(&self.spec, &self.environment)
    }

    /// Until native routing exists, no automated terminal input may reach a
    /// native room; the human's own typing is a different path and unaffected.
    fn refuse_native_paste(&self) -> io::Result<()> {
        if self.spec.native_delivery {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                NATIVE_PASTE_REFUSED,
            ));
        }
        Ok(())
    }

    pub(crate) fn send_whisper(&mut self, source: &str, message: &str) -> io::Result<()> {
        self.refuse_native_paste()?;
        let bracketed_paste = controls::uses_bracketed_paste(&self.spec);
        let (body, submit) = whisper_parts(self.spec.transport, bracketed_paste, source, message);
        // The pause and submit bytes ride in the same job as the body so no
        // other queued write can land between a paste and its Enter.
        self.push_write(WriteJob {
            bytes: body,
            followup: submit.map(|bytes| (RAW_SUBMIT_DELAY, bytes)),
        })
    }

    pub(crate) fn resend_whisper_submit(&mut self) -> io::Result<()> {
        self.refuse_native_paste()?;
        self.write_bytes(RAW_SUBMIT_BYTES)
    }

    pub(crate) fn poll_exit(&mut self) -> io::Result<()> {
        self.child.poll_exit().map(|_| ())
    }

    /// Prepares this native Codex room's next launch without starting it.
    ///
    /// The returned spec already carries the resume arguments when a session is
    /// reused. A clear writes the durable cleared marker before anything runs,
    /// so a later `crowded resume` cannot restore the old thread. A session is
    /// trusted only when Crowded minted it or the room's own hook reported it,
    /// for this vendor, directory and title.
    pub(crate) fn relaunch_codex(
        &self,
        action: CodexRelaunch,
        prompt: &str,
        room: usize,
    ) -> io::Result<(RoomSpec, CodexMint)> {
        let stored = || {
            session_state::lookup_minted("codex", &self.cwd, &self.spec.title)
                .or_else(|| session_state::lookup_reported("codex", &self.cwd, &self.spec.title))
                .filter(|id| codex_native::valid_uuid(id))
        };
        let mut spec = self.spec.clone();
        let session = match action {
            CodexRelaunch::Clear => {
                session_state::clear_room("codex", &self.cwd, &self.spec.title)?;
                None
            }
            CodexRelaunch::Resume => Some(stored().ok_or_else(|| {
                io::Error::other(format!(
                    "{}: no stored Codex session to resume; use control {room} clear to start a new one",
                    self.spec.title
                ))
            })?),
            CodexRelaunch::Restart => stored(),
        };
        if let Some(uuid) = session {
            controls::set_resume_session(&mut spec, &uuid)?;
            return Ok((spec, CodexMint::Ready { uuid, reused: true }));
        }
        controls::clear_resume_args(&mut spec)?;
        let generation = GenerationAllocator::new(session_state::state_directory()).next()?;
        Ok((
            spec,
            CodexMint::Unreported {
                prompt: prompt.to_owned(),
                generation,
            },
        ))
    }

    pub(crate) fn clear_context(
        &mut self,
        size: PtySize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Refuse before the durable fresh-state marker below is written.
        self.refuse_native_relaunch()?;
        // Record durable fresh-state intent before reconfiguration so a later
        // resume from the pre-clear spawn never restores the stale session.
        // A marker that cannot be saved fails the clear: the old session is
        // still trusted, so a fresh launch could strand it.
        let vendor = controls::cli_vendor(&self.spec)?;
        let cwd = working_directory(self.spec.cwd.as_deref())?;
        session_state::clear_room(vendor.key(), &cwd, &self.spec.title)?;
        self.reconfigure(size, controls::clear_resume_args)
    }

    /// Whether this room runs on a Crowded-owned OpenCode server, so a clear
    /// can start a new session on it without relaunching the server.
    pub(crate) fn opencode_native(&self) -> bool {
        self.native.is_some() && owned_server_room(&self.spec)
    }

    /// Whether this room's owned OpenCode server is gone: its wrapper child
    /// exited (reaped here) or its endpoint no longer accepts a connection.
    /// False for rooms without an owned server. Never touches another room.
    pub(crate) fn poll_opencode_server_exit(&self) -> io::Result<bool> {
        let Some(native) = self.native.as_ref() else {
            return Ok(false);
        };
        if !owned_server_room(&self.spec) {
            return Ok(false);
        }
        native.poll_server_exit()
    }

    /// This room's current owned-server binding, if it has one.
    pub(crate) fn opencode_binding(&self) -> Option<crate::native_outbox::Binding> {
        self.native.as_ref().map(|native| native.binding().clone())
    }

    /// The arguments this room's terminal was started with, when it runs on an
    /// owned server. Only the test rig asks for them.
    #[cfg(all(test, unix))]
    pub(crate) fn native_arguments(&self) -> Option<Vec<std::ffi::OsString>> {
        self.native.as_ref().map(NativeRoom::tui_arguments)
    }

    /// Clears an owned OpenCode room: the durable fresh-state marker, a new
    /// session on the same server, then a TUI respawn that attaches to it, and
    /// finally an abort of the old session's in-flight run.
    ///
    /// Each failure path is defined: a session that cannot be created, a
    /// respawn that fails, or a marker that cannot be saved changes nothing
    /// persisted or bound: the old binding is put back on the server and the
    /// room, so the old terminal, its arguments and `binding()` are as
    /// before, and the old run is not aborted. The abort runs last so a
    /// failed abort can never leave the room without a working session; its
    /// failure is returned as a warning, because the clear itself succeeded.
    /// if the clear fails later: a stale session id must never be resumed.
    pub(crate) fn clear_opencode_session(
        &mut self,
        size: PtySize,
        generation: u64,
    ) -> Result<(crate::native_outbox::Binding, Option<String>), Box<dyn std::error::Error>> {
        if !self.opencode_native() {
            return Err(io::Error::other(
                "only a room on an owned OpenCode server can start a new session this way",
            )
            .into());
        }
        let native = self
            .native
            .as_mut()
            .expect("open code native room has its server");
        let previous = native.binding().clone();
        let binding = native.clear_session(generation)?;
        // Build the replacement before touching persisted state or giving up
        // the server, then roll the session back if the terminal cannot be
        // restarted, so a failed spawn changes nothing persisted either.
        let replacement = match Self::spawn_attached(
            &self.spec,
            size,
            self.environment.clone(),
            self.native.as_ref(),
        ) {
            Ok(replacement) => replacement,
            Err(error) => {
                let restored = self
                    .native
                    .as_mut()
                    .expect("open code native room has its server")
                    .restore_binding(&previous);
                return match restored {
                    Ok(()) => Err(error),
                    Err(rollback) => Err(io::Error::other(format!(
                        "the new session was created but the terminal could not be restarted \
                         ({error}), and restoring the previous session also failed \
                         ({rollback}): run the control clear command for this room again, \
                         or restart Crowded"
                    ))
                    .into()),
                };
            }
        };
        // A marker that cannot be saved fails the clear: the replacement is
        // dropped and the old session is restored, so the previous stored
        // session is never replaced by a room that did not start.
        let vendor = controls::cli_vendor(&self.spec)?;
        let cwd = working_directory(self.spec.cwd.as_deref())?;
        if let Err(marker) = session_state::clear_room(vendor.key(), &cwd, &self.spec.title) {
            let _replacement = replacement;
            let restored = self
                .native
                .as_mut()
                .expect("open code native room has its server")
                .restore_binding(&previous);
            return match restored {
                Ok(()) => Err(marker.into()),
                Err(rollback) => Err(io::Error::other(format!(
                    "the new session was created but its fresh-state marker could not be saved \
                     ({marker}), and restoring the previous session also failed \
                     ({rollback}): run the control clear command for this room again, \
                     or restart Crowded"
                ))
                .into()),
            };
        }
        let native = self.native.take();
        self.cleanup();
        *self = replacement;
        self.native = native;
        // The clear is complete before the old run is stopped, so a failed
        // abort can never leave the room without a working session.
        let abort = self
            .native
            .as_ref()
            .expect("open code native room keeps its server")
            .server()
            .interrupt_session(&previous.session)
            .err()
            .map(|error| {
                format!(
                    "{}: the previous OpenCode session {} could not be interrupted \
                     ({error}); it may still be running",
                    self.spec.title, previous.session
                )
            });
        Ok((binding, abort))
    }

    /// Builds the replacement TUI for a revived owned OpenCode room without
    /// touching the pane: no swap, no persisted state. Any failure changes
    /// nothing; the caller drops the new server by its recorded processes.
    pub(crate) fn prepare_revived_tui(
        &self,
        size: PtySize,
        room: &crate::native_launch::NativeRoom,
    ) -> Result<Pane, Box<dyn std::error::Error>> {
        if !self.opencode_native() {
            return Err(io::Error::other(
                "only a room on an owned OpenCode server can be revived this way",
            )
            .into());
        }
        Self::spawn_attached(&self.spec, size, self.environment.clone(), Some(room))
    }

    /// Commits a prepared replacement TUI: saves the durable fresh-state
    /// marker, then swaps the pane onto the new server. The marker goes
    /// first, so a failure leaves the pane on its previous binding with its
    /// stored session; the caller drops the prepared terminal and the new
    /// server by their recorded processes.
    pub(crate) fn commit_revived_tui(
        &mut self,
        replacement: Pane,
        room: crate::native_launch::NativeRoom,
    ) -> Result<crate::native_outbox::Binding, Box<dyn std::error::Error>> {
        if !self.opencode_native() {
            return Err(io::Error::other(
                "only a room on an owned OpenCode server can be revived this way",
            )
            .into());
        }
        let vendor = controls::cli_vendor(&self.spec)?;
        let cwd = working_directory(self.spec.cwd.as_deref())?;
        session_state::clear_room(vendor.key(), &cwd, &self.spec.title)?;
        let _old = self.native.take();
        self.cleanup();
        *self = replacement;
        self.native = Some(room);
        Ok(self
            .native
            .as_ref()
            .expect("revived room keeps its server")
            .binding()
            .clone())
    }

    /// Returns whether a conversation was actually resumed. A room whose
    /// recorded session cannot be verified as its own starts fresh instead, and
    /// then still needs its intro and a new session captured.
    pub(crate) fn resume_context(
        &mut self,
        size: PtySize,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        self.refuse_native_relaunch()?;
        // One room resuming on its own request has no slate to be judged
        // against, so there is no refusal to honour. The objective checks
        // inside `add_resume_args` still apply.
        self.reconfigure(size, |spec| controls::add_resume_args(spec, &[]))
    }

    pub(crate) fn configure(
        &mut self,
        model: Option<&str>,
        effort: Option<&str>,
        size: PtySize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Refuse before any model-switch subprocess runs.
        self.refuse_native_relaunch()?;
        let mut spec = self.spec.clone();
        let vendor = controls::cli_vendor(&spec)?;
        if let Some(model) = model {
            controls::set_model(&mut spec, model)?;
        }
        if let Some(effort) = effort {
            controls::set_effort(&mut spec, effort)?;
        }
        if let (controls::CliVendor::OpenCode, Some(model)) = (vendor, model) {
            let body = opencode_model_switch_body(model)?;
            if let Some(session_id) = opencode_resume_session_id(&self.spec, &self.cwd)? {
                switch_opencode_session_model(
                    &self.spec,
                    &self.environment,
                    &self.cwd,
                    &session_id,
                    &body,
                )?;
            }
        }
        let replacement = Self::spawn(spec, size, self.environment.clone())?;
        self.cleanup();
        *self = replacement;
        Ok(())
    }

    pub(crate) fn current_model(&self) -> Option<String> {
        controls::current_model(&self.spec)
    }

    pub(crate) fn current_effort(&self) -> Option<String> {
        controls::current_effort(&self.spec)
    }

    pub(crate) fn capabilities(&self) -> RoomCapabilities {
        controls::capabilities(&self.spec)
    }

    pub(crate) fn scheduling(&self) -> Option<RoomScheduling> {
        self.spec.scheduling.clone()
    }

    fn reconfigure<T>(
        &mut self,
        size: PtySize,
        configure: impl FnOnce(&mut RoomSpec) -> io::Result<T>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        self.refuse_native_relaunch()?;
        let mut spec = self.spec.clone();
        let outcome = configure(&mut spec)?;
        let replacement = Self::spawn(spec, size, self.environment.clone())?;
        self.cleanup();
        *self = replacement;
        Ok(outcome)
    }

    /// A native room's server and session are created once per launch. Relaunching
    /// in place would need a second, asynchronous launch, so it is refused
    /// visibly rather than falling back to a private server.
    fn refuse_native_relaunch(&self) -> io::Result<()> {
        if self.spec.native_delivery {
            return Err(io::Error::other(
                "native delivery rooms cannot be restarted, cleared, resumed or reconfigured yet",
            ));
        }
        Ok(())
    }

    pub(crate) fn restart(&mut self, size: PtySize) -> Result<(), Box<dyn std::error::Error>> {
        self.refuse_native_relaunch()?;
        let replacement = Self::spawn(self.spec.clone(), size, self.environment.clone())?;
        self.cleanup();
        *self = replacement;
        Ok(())
    }

    pub(crate) fn resize(&mut self, size: PtySize) -> Result<(), Box<dyn std::error::Error>> {
        // The real PTY follows its pane exactly. Only the parser keeps its
        // private 2×2 safety minimum.
        self.master.resize(size)?;
        self.parser
            .screen_mut()
            .set_size(size.rows.max(2), size.cols.max(2));
        Ok(())
    }

    pub(crate) fn cleanup(&mut self) {
        self.child.cleanup();
    }
}

#[cfg(test)]
mod tests;
