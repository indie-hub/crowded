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
    command::{ResolvedCommand, headroom_on_path},
    config::{RoomScheduling, RoomSpec, Transport},
    doorbell::RoomCapabilities,
    native_launch::{self, GenerationAllocator, NativeRoom},
};

mod controls;

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
    // Native-delivery rooms always start a fresh explicit session, so they
    // neither take part in the repair nor receive resume arguments.
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
            tree.terminate();
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
}

impl PaneWriter {
    fn spawn(mut inner: Box<dyn Write + Send>) -> Self {
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
    /// A session Crowded minted earlier for this room; resume it.
    Ready(String),
    /// The exec phase is running; its result arrives on this channel.
    Pending(mpsc::Receiver<Result<Minted, String>>),
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

/// The exact command that mints a room's session: the exec-compatible part of
/// the room's launch options, through the same wrapper as its terminal.
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

/// Chooses each native Claude room's session before its terminal starts: resume
/// a session the room's hook reported, otherwise start fresh and learn it from
/// the hook.
pub(crate) fn start_claude_sessions(
    specs: &mut [RoomSpec],
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
            let start =
                if let Some(stored) = stored_claude_session(&cwd, &spec.title, &spec.variables) {
                    controls::set_resume_session(spec, &stored)?;
                    ClaudeStart::Resume(stored)
                } else {
                    ClaudeStart::Fresh
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
const CODEX_MINT_TIMEOUT: Duration = Duration::from_secs(180);
#[cfg(test)]
const CODEX_MINT_TIMEOUT: Duration = Duration::from_secs(5);

/// Starts each native Codex room's session without waiting for any of them.
///
/// A room with a session this program minted for its own directory and title is
/// ready at once and runs no exec. Every other room (including one with a
/// legacy generic capture) spawns a worker that creates a session with
/// `codex exec` and its initial prompt, saves it durably, and reports the UUID.
/// Each mint is bounded on its own worker, so a slow room never delays another.
pub(crate) fn start_codex_mints(
    specs: &mut [RoomSpec],
    environments: &[GuestEnvironment],
    prompts: &[Option<String>],
) -> Vec<Option<CodexMint>> {
    let mut results: Vec<Option<CodexMint>> = (0..specs.len()).map(|_| None).collect();
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
        // of ownership.
        let stored = session_state::lookup_minted("codex", &cwd, &spec.title)
            .or_else(|| session_state::lookup_reported("codex", &cwd, &spec.title))
            .filter(|id| codex_native::valid_uuid(id));
        if let Some(uuid) = stored {
            if controls::set_resume_session(spec, &uuid).is_ok() {
                results[index] = Some(CodexMint::Ready(uuid));
            } else {
                results[index] = Some(CodexMint::Failed(format!(
                    "{}: could not prepare the stored session",
                    spec.title
                )));
            }
            continue;
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
        results[index] = Some(start_codex_mint(spec, cwd, &environments[index], prompt));
    }
    results
}

/// Runs one room's `codex exec` mint on its own worker. The result is saved by
/// the consumer, never here: a worker whose launch has been replaced must not
/// persist its session.
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
                        *slot = Some(CodexMint::Ready(minted.uuid));
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
    // launch can never match this binding.
    let generation = GenerationAllocator::new(session_state::state_directory()).next()?;
    let (room, reason) = match mint {
        CodexMint::Ready(uuid) => {
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
        CodexMint::Failed(reason) => (None, Some(reason.clone())),
    };
    router.bind_codex(vec![crate::native_route::CodexEntry {
        index,
        title,
        room,
        generation,
        reason,
    }])
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
    let mut child = command.spawn().map_err(|error| {
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
        let mut guest = opencode_launch(&spec)?;
        if let Some(native) = &native {
            attach_to_native_server(&mut guest, native)?;
        }
        let path = environment_value(&guest.variables, "PATH");
        let path_ext = environment_value(&guest.variables, "PATHEXT");
        let (program, args, tui_headroom) = if codex_native_spec(&spec) {
            let (program, args) = codex_phase_command(&spec, &guest.args);
            (program, args, spec_uses_headroom(&spec))
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
            spec_uses_headroom(&spec)
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
        if claude_native_spec(&spec) {
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
        let child = ChildGuard::new(pty.slave.spawn_command(command)?, tree)?;
        // The parent must not keep a second slave handle alive.
        drop(pty.slave);

        // Input and output use separate master handles so reading can block
        // on another thread while the UI remains responsive.
        let writer = PaneWriter::spawn(pty.master.take_writer()?);
        let mut reader = pty.master.try_clone_reader()?;
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
        let captured_session = match controls::cli_vendor(&spec) {
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
            return Ok((spec, CodexMint::Ready(uuid)));
        }
        controls::clear_resume_args(&mut spec)?;
        let mint = start_codex_mint(
            &spec,
            self.cwd.clone(),
            &self.environment,
            prompt.to_owned(),
        );
        Ok((spec, mint))
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
    #[allow(dead_code)]
    pub(crate) fn opencode_native(&self) -> bool {
        self.native.is_some() && owned_server_room(&self.spec)
    }

    /// Clears an owned OpenCode room: the durable fresh-state marker, a new
    /// session on the same server, then a TUI respawn that attaches to it.
    ///
    /// Each failure path is defined: a marker that cannot be saved or a session
    /// that cannot be created changes nothing; a respawn that fails puts the
    /// old binding back on the server and the room, so the old terminal, its
    /// arguments and `binding()` are as before. The marker deliberately stays
    /// written once set, even if the clear fails later: a stale session id must
    /// never be resumed.
    #[allow(dead_code)]
    pub(crate) fn clear_opencode_session(
        &mut self,
        size: PtySize,
        generation: u64,
    ) -> Result<crate::native_outbox::Binding, Box<dyn std::error::Error>> {
        if !self.opencode_native() {
            return Err(io::Error::other(
                "only a room on an owned OpenCode server can start a new session this way",
            )
            .into());
        }
        // A marker that cannot be saved fails the clear: the old session is
        // still trusted, so a fresh launch could strand it.
        let vendor = controls::cli_vendor(&self.spec)?;
        let cwd = working_directory(self.spec.cwd.as_deref())?;
        session_state::clear_room(vendor.key(), &cwd, &self.spec.title)?;
        let native = self
            .native
            .as_mut()
            .expect("open code native room has its server");
        let previous = native.binding().clone();
        let binding = native.clear_session(generation)?;
        // Build the replacement before giving up the server, then roll the
        // session back if the terminal cannot be restarted.
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
                         ({rollback}): this room needs a restart"
                    ))
                    .into()),
                };
            }
        };
        let native = self.native.take();
        self.cleanup();
        *self = replacement;
        self.native = native;
        Ok(binding)
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
mod tests {
    use super::*;
    use crate::{
        app::{DeliveryFuse, DeliveryGate, PendingSubmit, inject_ready_pending},
        mailroom::Mailroom,
    };
    use std::{
        ffi::OsStr,
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    static CLAUDE_CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    struct ClaudeConfigDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl ClaudeConfigDirGuard {
        fn new() -> Self {
            let lock = CLAUDE_CONFIG_ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("CLAUDE_CONFIG_DIR");
            let dir = tempfile::tempdir().unwrap();
            // Safety: serialized by `CLAUDE_CONFIG_ENV_LOCK` for the test process.
            unsafe {
                std::env::set_var("CLAUDE_CONFIG_DIR", dir.path());
            }
            Self {
                _lock: lock,
                previous,
                _dir: dir,
            }
        }

        fn path(&self) -> &Path {
            self._dir.path()
        }
    }

    #[cfg(unix)]
    impl Drop for ClaudeConfigDirGuard {
        fn drop(&mut self) {
            // Safety: serialized by `CLAUDE_CONFIG_ENV_LOCK` for the test process.
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("CLAUDE_CONFIG_DIR", value),
                    None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
                }
            }
        }
    }

    #[cfg(unix)]
    fn materialize_claude_session(config_dir: &Path, cwd: &Path, session: &str) {
        // Claude's observed directory encoding, spelled out here so this helper
        // cannot drift together with the production rule it checks.
        let encoded: String = cwd
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let dir = config_dir.join("projects").join(encoded);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{session}.jsonl")), b"{}\n").unwrap();
    }

    fn room_spec(program: &str, args: &[&str]) -> RoomSpec {
        RoomSpec {
            name: program.to_owned(),
            vendor: "unknown".to_owned(),
            title: program.to_owned(),
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            transport: Transport::Raw,
            cwd: None,
            variables: Vec::new(),
            allow_control: true,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
            native_delivery: false,
        }
    }

    #[cfg(unix)]
    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            assert!(
                Instant::now() < deadline,
                "{} never appeared",
                path.display()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn native_test_size() -> PtySize {
        PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[cfg(unix)]
    fn native_spec(program: &Path, cwd: &Path, args: &[&str]) -> RoomSpec {
        let mut spec = room_spec(program.to_str().unwrap(), args);
        spec.native_delivery = true;
        spec.cwd = Some(cwd.to_path_buf());
        spec
    }

    /// Launches one native room through the real launch path and spawns its pane.
    #[cfg(unix)]
    fn spawn_native_pane(
        root: &Path,
        cwd: &Path,
        name: &str,
        args: &[&str],
        token: &str,
    ) -> (Pane, native_launch::tests::FakeRoom) {
        let fake = native_launch::tests::fake_room(root, name, "v2.2.5");
        let spec = native_spec(&fake.program, cwd, args);
        let environment = GuestEnvironment::new([("CROWDED_TEST_TOKEN", token.into())]);
        let room = launch_native_rooms(
            std::slice::from_ref(&spec),
            std::slice::from_ref(&environment),
        )
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
        .unwrap();
        let pane =
            Pane::spawn_with_native(spec, native_test_size(), environment, Some(room)).unwrap();
        (pane, fake)
    }

    #[cfg(unix)]
    #[test]
    fn clearing_an_opencode_room_attaches_the_tui_to_a_new_session_on_the_same_server() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut pane, fake) = spawn_native_pane(
            root.path(),
            cwd.path(),
            "clear-one",
            &["--model", "provider/alpha"],
            "token-clear",
        );
        let title = pane.spec.title.clone();
        let old = pane.native.as_ref().unwrap().binding().clone();
        let binding = pane.clear_opencode_session(native_test_size(), 9).unwrap();

        assert_ne!(binding.session, old.session);
        assert_eq!(binding.generation, 9);
        assert_eq!(binding.endpoint, old.endpoint);
        assert_eq!(pane.native.as_ref().unwrap().binding(), &binding);
        // The terminal's real arguments, as the stand-in received them.
        let argv = fake.dir.join("tui_argv");
        let started = Instant::now();
        let mut seen = String::new();
        while started.elapsed() < Duration::from_secs(5) {
            seen = fs::read_to_string(&argv).unwrap_or_default();
            if seen.contains(&binding.session) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let attached = seen
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|pair| pair[0] == "--session")
            .map(|pair| pair[1].to_owned());
        assert_eq!(
            attached.as_deref(),
            Some(binding.session.as_str()),
            "{seen}"
        );
        assert_ne!(attached.as_deref(), Some(old.session.as_str()), "{seen}");
        assert_eq!(
            session_state::lookup("opencode", cwd.path(), &title).as_deref(),
            Some(session_state::FRESH_STATE_MARKER)
        );

        // The owned server survived the respawn and serves only the new session.
        let server = pane.native.as_ref().unwrap().server();
        let refused = server
            .queue_async(&old, format!("msg_{}", "a".repeat(64)), "old".into())
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(refused, crate::opencode_native::Delivery::NotSent);
        let accepted = server
            .queue_async(&binding, format!("msg_{}", "b".repeat(64)), "new".into())
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_ne!(accepted, crate::opencode_native::Delivery::NotSent);
        fake.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_new_session_leaves_the_opencode_pane_untouched() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut pane, fake) = spawn_native_pane(
            root.path(),
            cwd.path(),
            "clear-fail",
            &["--model", "provider/alpha"],
            "token-fail",
        );
        let title = pane.spec.title.clone();
        let old = pane.native.as_ref().unwrap().binding().clone();
        let old_args = pane.spec.args.clone();
        fake.shutdown();

        let error = pane
            .clear_opencode_session(native_test_size(), 9)
            .unwrap_err();
        assert!(error.to_string().contains("session"), "{error}");
        assert_eq!(pane.spec.args, old_args);
        assert_eq!(pane.native.as_ref().unwrap().binding(), &old);
        // The marker was written first and stays: a stale id must never be
        // resumed, even though no new session was created.
        assert_eq!(
            session_state::lookup("opencode", cwd.path(), &title).as_deref(),
            Some(session_state::FRESH_STATE_MARKER)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_terminal_restart_puts_the_old_session_back() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut pane, fake) = spawn_native_pane(
            root.path(),
            cwd.path(),
            "clear-rollback",
            &["--model", "provider/alpha"],
            "token-rollback",
        );
        let title = pane.spec.title.clone();
        let old = pane.native.as_ref().unwrap().binding().clone();
        let old_args = pane.native.as_ref().unwrap().tui_arguments();
        let created = fake
            .state
            .sessions
            .load(std::sync::atomic::Ordering::SeqCst);
        // Only the respawn resolves the program, so this fails there and not
        // before the session has already been created and bound.
        pane.spec.program = OsString::from("/nonexistent/crowded-clear/opencode");

        let error = pane
            .clear_opencode_session(native_test_size(), 9)
            .unwrap_err();
        assert!(!error.to_string().is_empty());
        // The clear got as far as creating a second session, then rolled back.
        assert_eq!(
            fake.state
                .sessions
                .load(std::sync::atomic::Ordering::SeqCst),
            created + 1
        );
        assert_eq!(
            session_state::lookup("opencode", cwd.path(), &title).as_deref(),
            Some(session_state::FRESH_STATE_MARKER)
        );
        // The room and the server agree on the old session again.
        let native = pane.native.as_ref().unwrap();
        assert_eq!(native.binding(), &old);
        assert_eq!(native.tui_arguments(), old_args);
        let server = native.server();
        let accepted = server
            .queue_async(&old, format!("msg_{}", "e".repeat(64)), "old".into())
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_ne!(accepted, crate::opencode_native::Delivery::NotSent);
        // The old terminal was never replaced, so it is still running.
        assert!(!pane.child.poll_exit().unwrap());
        fake.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn an_opencode_clear_whose_marker_cannot_be_saved_changes_nothing() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut pane, fake) = spawn_native_pane(
            root.path(),
            cwd.path(),
            "clear-marker",
            &["--model", "provider/alpha"],
            "token-marker",
        );
        let old = pane.native.as_ref().unwrap().binding().clone();
        let old_args = pane.spec.args.clone();
        let path = session_state::state_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"not a state file").unwrap();

        let error = pane
            .clear_opencode_session(native_test_size(), 9)
            .unwrap_err();
        assert!(error.to_string().contains("cleared marker"), "{error}");
        assert_eq!(pane.spec.args, old_args);
        assert_eq!(pane.native.as_ref().unwrap().binding(), &old);
        fake.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn only_a_room_on_an_owned_opencode_server_can_start_a_new_session() {
        let _state = session_state::StateRootGuard::isolated();
        let mut pane = Pane::spawn(
            room_spec("sh", &["-c", "exec sleep 5"]),
            native_test_size(),
            GuestEnvironment::new([]),
        )
        .unwrap();
        assert!(!pane.opencode_native());
        let error = pane
            .clear_opencode_session(native_test_size(), 9)
            .unwrap_err();
        assert!(
            error.to_string().contains("owned OpenCode server"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_terminal_and_server_share_the_room_environment_and_only_the_terminal_gets_arguments()
    {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut one, fake_one) = spawn_native_pane(
            root.path(),
            cwd.path(),
            "one",
            &["--model", "provider/alpha"],
            "token-one",
        );
        let (two, fake_two) = spawn_native_pane(
            root.path(),
            cwd.path(),
            "two",
            &["--model", "provider/beta"],
            "token-two",
        );

        for (fake, model, other, token) in [
            (&fake_one, "provider/alpha", "provider/beta", "token-one"),
            (&fake_two, "provider/beta", "provider/alpha", "token-two"),
        ] {
            let server_config = wait_for_file(&fake.dir.join("config"));
            let tui_config = wait_for_file(&fake.dir.join("tui_config"));
            // The agent runs in the server, so the server sees the room's
            // configuration and model, and exactly what the terminal sees.
            assert_eq!(server_config, tui_config);
            assert!(server_config.contains(model), "{server_config}");
            assert!(!server_config.contains(other), "{server_config}");
            assert_eq!(wait_for_file(&fake.dir.join("token")), token);
            assert_eq!(wait_for_file(&fake.dir.join("tui_token")), token);
            // The password is the server's own and reaches the terminal by environment.
            let password = wait_for_file(&fake.dir.join("password"));
            assert_eq!(wait_for_file(&fake.dir.join("tui_password")), password);
            let argv = wait_for_file(&fake.dir.join("tui_argv"));
            assert!(argv.starts_with("--server http://127.0.0.1:"));
            assert!(argv.contains("--session ses_"));
            assert!(!argv.contains("--standalone"));
            assert!(!argv.contains("--model"));
            assert!(!argv.contains(&password));
            assert_eq!(
                wait_for_file(&fake.dir.join("argv")),
                "serve --hostname 127.0.0.1 --port 0"
            );
        }
        // Same directory, different servers and sessions.
        assert_ne!(
            wait_for_file(&fake_one.dir.join("pid")),
            wait_for_file(&fake_two.dir.join("pid"))
        );

        // The server and its session are created once per launch, so an
        // in-place relaunch is refused visibly and leaves the room running.
        let refused = |error: Box<dyn std::error::Error>| {
            assert!(error.to_string().contains("native delivery rooms cannot"));
        };
        refused(one.restart(native_test_size()).err().unwrap());
        refused(one.clear_context(native_test_size()).err().unwrap());
        // The refusal happens before the durable fresh-state marker is written.
        assert!(session_state::lookup("opencode", &one.cwd, &one.spec.title).is_none());
        refused(one.resume_context(native_test_size()).err().unwrap());
        refused(
            one.configure(Some("provider/model"), None, native_test_size())
                .err()
                .unwrap(),
        );
        assert!(one.is_native());
        let server_pid = wait_for_file(&fake_one.dir.join("pid"));
        one.cleanup();
        drop(one);
        drop(two);
        native_launch::wait_for_reaps();
        assert!(!native_launch::tests::process_alive(&server_pid));
    }

    #[cfg(unix)]
    #[test]
    fn a_native_room_takes_no_automated_terminal_input_but_keeps_human_typing() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut pane, _fake) = spawn_native_pane(root.path(), cwd.path(), "one", &[], "t");

        // Intro, queued delivery, doorbell sends and the manual whisper all end
        // here, and the Enter resubmit too.
        assert!(!pane.needs_intro());
        for error in [
            pane.send_whisper("Room 2", "hello").unwrap_err(),
            pane.resend_whisper_submit().unwrap_err(),
        ] {
            assert_eq!(error.kind(), io::ErrorKind::Unsupported);
            assert_eq!(error.to_string(), NATIVE_PASTE_REFUSED);
        }
        let mut mailroom = Mailroom::new(10);
        let (id, result) = mailroom.deliver("Room 2".to_owned(), &mut pane, "hello".to_owned());
        assert!(result.is_err());
        assert!(mailroom.summary().contains("FAILED"), "{id}");
        assert!(!mailroom.summary().contains("INJECTED"));
        // The reintroduction key cannot start a guessed session capture.
        assert!(!pane.begin_session_capture());
        assert!(!pane.has_captured_session());
        // A queued envelope for the room is never injected: it fails visibly.
        let mut mailroom = Mailroom::new(10);
        let id = mailroom.queue(
            "Room 2".to_owned(),
            pane.title().to_owned(),
            "hi".into(),
            "q",
        );
        let mut pending = std::collections::VecDeque::from([(id, 0usize)]);
        let mut gates = vec![DeliveryGate::new(false)];
        let mut submit = std::collections::VecDeque::new();
        let pass = run_inject_pass(
            &mut pending,
            &mut mailroom,
            std::slice::from_mut(&mut pane),
            &mut gates,
            &[true],
            &mut submit,
        );
        assert_eq!(pass, (0, 1));
        assert!(submit.is_empty());
        assert!(!mailroom.summary().contains("INJECTED"));
        // The person at the keyboard is not affected.
        pane.write_bytes(b"x").unwrap();
    }

    /// Every automated entry point against a real PTY child: nothing reaches
    /// the terminal, and the person's own typing still does.
    #[cfg(unix)]
    #[test]
    fn nothing_automated_reaches_a_native_terminal_but_human_typing_does() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (pane, fake) = spawn_native_pane(root.path(), cwd.path(), "one", &[], "t");
        wait_for_file(&fake.dir.join("tui_argv"));
        let mut panes = vec![pane];

        // Intro and F4 reintroduce, manual whisper, Ctrl+W composer and doorbell
        // delivery all end in send_whisper (through Mailroom::deliver or directly).
        assert!(panes[0].send_whisper("Room 2", "intro").is_err());
        let mut mailroom = Mailroom::new(10);
        let (_, delivered) = mailroom.deliver("Room 2".to_owned(), &mut panes[0], "mail".into());
        assert!(delivered.is_err());
        // Queued delivery.
        let id = mailroom.queue("Room 2".to_owned(), "t".to_owned(), "queued".into(), "q");
        let mut pending = std::collections::VecDeque::from([(id, 0usize)]);
        let mut gates = vec![DeliveryGate::new(false)];
        let mut submit = std::collections::VecDeque::new();
        run_inject_pass(
            &mut pending,
            &mut mailroom,
            &mut panes,
            &mut gates,
            &[true],
            &mut submit,
        );
        // A stale Enter resubmit record that is due.
        let now = Instant::now();
        let mut stale =
            std::collections::VecDeque::from([(now - Duration::from_secs(120), 0usize, false)]);
        crate::app::resend_whisper_submits(&mut stale, &mut panes, &[true], now);
        assert!(stale.is_empty());

        // None of it reached the terminal; the person's typing does.
        thread::sleep(Duration::from_millis(500));
        assert!(!fake.dir.join("tui_stdin").exists());
        panes[0].write_bytes(b"ok\r").unwrap();
        assert_eq!(wait_for_file(&fake.dir.join("tui_stdin")), "ok");
    }

    #[cfg(unix)]
    #[test]
    fn a_native_room_ignores_legacy_session_entries() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = native_launch::tests::fake_room(root.path(), "one", "v2.2.5");
        let spec = native_spec(&fake.program, cwd.path(), &[]);
        let working = working_directory(spec.cwd.as_deref()).unwrap();
        // A same-title entry from an earlier, non-native run of this room.
        session_state::upsert("opencode", &working, &spec.title, "foreign-session");
        let environment = GuestEnvironment::new([]);
        let room = launch_native_rooms(
            std::slice::from_ref(&spec),
            std::slice::from_ref(&environment),
        )
        .unwrap()
        .remove(0)
        .unwrap();
        let session = room.binding().session.clone();
        let pane =
            Pane::spawn_with_native(spec, native_test_size(), environment, Some(room)).unwrap();
        assert!(!pane.has_captured_session());
        assert!(!pane.begin_session_capture());
        assert!(!pane.has_captured_session());
        // The legacy entry is untouched and the owned session is not recorded under it.
        assert_eq!(
            session_state::lookup("opencode", &pane.cwd, &pane.spec.title).as_deref(),
            Some("foreign-session")
        );
        assert_ne!(session, "foreign-session");
    }

    #[cfg(unix)]
    #[test]
    fn native_room_starts_its_owned_server_through_headroom() {
        use std::os::unix::fs::PermissionsExt;
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = native_launch::tests::fake_room(root.path(), "one", "v2.2.5");
        let bin = temporary_bin_directory();
        let wrapper = bin.join("headroom");
        fs::write(
            &wrapper,
            "#!/bin/sh\n[ \"$1\" = \"wrap\" ] && shift\nprog=\"$1\"; shift\nwhile [ \"${1#--}\" != \"$1\" ]; do shift; done\nexec \"$prog\" \"$@\"\n",
        )
        .unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700)).unwrap();

        let mut spec = native_spec(
            &fake.program,
            cwd.path(),
            &["--model", "opencode-go/mimo-v2.5"],
        );
        spec.use_headroom = true;
        spec.headroom_args = vec!["--no-serena".into()];
        spec.variables.push((
            "PATH".into(),
            format!("{}:{}", bin.display(), env::var("PATH").unwrap_or_default()).into(),
        ));
        let environment = GuestEnvironment::new([]);
        let room = launch_native_rooms(
            std::slice::from_ref(&spec),
            std::slice::from_ref(&environment),
        )
        .unwrap()
        .remove(0)
        .unwrap();
        // The wrapper passed the server arguments through unchanged.
        assert_eq!(
            fs::read_to_string(fake.dir.join("argv")).unwrap(),
            "serve --hostname 127.0.0.1 --port 0"
        );
        // Headroom replaces the inline config, so the room document is a file.
        let config_path = fs::read_to_string(fake.dir.join("config_path")).unwrap();
        assert!(!config_path.is_empty());
        assert!(
            fs::read_to_string(&config_path)
                .unwrap()
                .contains("opencode-go/mimo-v2.5"),
            "the wrapped server reads the room config from {config_path}"
        );
        let pane =
            Pane::spawn_with_native(spec, native_test_size(), environment, Some(room)).unwrap();
        assert!(pane.headroom_active());
        // The TUI only attaches; it is not wrapped a second time.
        let deadline = Instant::now() + Duration::from_secs(5);
        let tui = loop {
            if let Ok(text) = fs::read_to_string(fake.dir.join("tui_argv")) {
                break text;
            }
            assert!(Instant::now() < deadline, "the TUI never started");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(tui.starts_with("--server "), "{tui}");
        assert!(!tui.contains("wrap"), "{tui}");
        drop(pane);
        native_launch::wait_for_reaps();
        fs::remove_dir_all(bin).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_native_rooms_start_servers_each_launch_is_a_new_generation_and_a_failure_cancels_all() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();

        // A room without the setting starts nothing and writes no state.
        let ordinary = room_spec("opencode", &[]);
        let rooms = launch_native_rooms(
            std::slice::from_ref(&ordinary),
            &[GuestEnvironment::new([])],
        )
        .unwrap();
        assert!(rooms[0].is_none());
        assert!(!session_state::state_directory().exists());

        let good = native_launch::tests::fake_room(root.path(), "good", "v2.2.5");
        let spec = native_spec(&good.program, cwd.path(), &[]);
        let environments = [GuestEnvironment::new([]), GuestEnvironment::new([])];
        let first = launch_native_rooms(&[spec.clone(), ordinary.clone()], &environments).unwrap();
        assert_eq!(first[0].as_ref().unwrap().binding().room, "1.1");
        assert_eq!(first[0].as_ref().unwrap().binding().generation, 1);
        assert!(first[1].is_none());
        drop(first);
        let second = launch_native_rooms(&[ordinary, spec.clone()], &environments).unwrap();
        assert_eq!(second[1].as_ref().unwrap().binding().room, "2.2");
        assert_eq!(second[1].as_ref().unwrap().binding().generation, 2);
        drop(second);
        native_launch::wait_for_reaps();

        // One room that cannot start is a visible error naming it, and the
        // servers already started for the others are cleaned up.
        let old = native_launch::tests::fake_room(root.path(), "old", "v2.1.0");
        let stale = native_spec(&old.program, cwd.path(), &[]);
        let error = launch_native_rooms(&[spec, stale], &environments)
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("native delivery could not start")
        );
        assert!(error.to_string().contains("OpenCode 2.2.5 or newer"));
        let pid = wait_for_file(&good.dir.join("pid"));
        native_launch::wait_for_reaps();
        assert!(!native_launch::tests::process_alive(&pid));
    }

    /// Runs the real OpenCode TUI against a real owned server in an isolated
    /// environment; no model call is made.
    #[cfg(unix)]
    #[test]
    #[ignore]
    fn isolated_real_tui_attaches_to_the_owned_server() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut spec = native_spec(Path::new("opencode"), cwd.path(), &[]);
        spec.variables = native_launch::tests::isolated_variables(root.path());
        let environment = GuestEnvironment::new([]);
        let room = launch_native_rooms(
            std::slice::from_ref(&spec),
            std::slice::from_ref(&environment),
        )
        .unwrap()
        .remove(0)
        .unwrap();
        let port = room.tui_arguments()[1]
            .to_string_lossy()
            .rsplit(':')
            .next()
            .unwrap()
            .to_owned();
        let mut pane =
            Pane::spawn_with_native(spec, native_test_size(), environment, Some(room)).unwrap();
        // The TUI keeps a connection to the owned server's port.
        let deadline = Instant::now() + Duration::from_secs(15);
        let connected = loop {
            let _ = pane.drain_output();
            let lsof = std::process::Command::new("lsof")
                .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:ESTABLISHED"])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&lsof.stdout).lines().count() > 1 {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            thread::sleep(Duration::from_millis(200));
        };
        pane.cleanup();
        drop(pane);
        native_launch::wait_for_reaps();
        assert!(connected, "the TUI never connected to the owned server");
    }

    #[test]
    fn resume_runs_native_rooms_fresh_and_hands_the_notice_to_the_run() {
        let _state = session_state::StateRootGuard::isolated();
        let mut native = room_spec("opencode", &[]);
        native.native_delivery = true;
        let native_title = native.title.clone();
        let resolved = crate::config::ResolvedConfig {
            specs: vec![native, room_spec("claude", &[])],
            fuse_size: 7,
            token_pricing: Vec::new(),
        };
        let mut seen = None;
        crate::app::resume_with(resolved, |specs, resumed, fuse, _pricing, notice| {
            seen = Some((specs, resumed, fuse, notice));
            Ok(())
        })
        .unwrap();
        let (specs, resumed, fuse, notice) = seen.expect("the run was started");
        assert_eq!(fuse, 7);
        // The native room is not resumed and keeps its arguments; another is.
        assert_eq!(resumed, [false, true]);
        assert!(specs[0].args.is_empty());
        assert!(!specs[1].args.is_empty());
        // The user is told, and only about the native room.
        let notice = notice.expect("the notice reaches the run");
        let _ = &native_title;
        assert!(notice.contains("native room 1 "), "{notice}");
        assert!(!notice.contains('2'), "{notice}");
        // Without a native room there is nothing to say.
        let plain = crate::config::ResolvedConfig {
            specs: vec![room_spec("claude", &[])],
            fuse_size: 7,
            token_pricing: Vec::new(),
        };
        let mut plain_notice = Some("unset".to_owned());
        crate::app::resume_with(plain, |_, _, _, _, notice| {
            plain_notice = notice;
            Ok(())
        })
        .unwrap();
        assert!(plain_notice.is_none());
    }

    #[test]
    fn a_room_supplied_password_is_stripped_before_the_server_sees_the_environment() {
        let mut spec = room_spec("opencode", &[]);
        spec.variables = vec![
            ("OPENCODE_PASSWORD".into(), "from-the-room".into()),
            ("KEEP_ME".into(), "1".into()),
        ];
        let environment = GuestEnvironment::new([
            ("OPENCODE_PASSWORD", "from-the-doorbell".into()),
            ("CROWDED_TEST_TOKEN", "t".into()),
        ]);
        let variables = native_variables(&spec, &environment).unwrap();
        assert!(variables.iter().all(|(key, _)| key != "OPENCODE_PASSWORD"));
        assert!(
            variables
                .iter()
                .any(|(key, value)| key == "KEEP_ME" && value == "1")
        );
        assert!(
            variables
                .iter()
                .any(|(key, value)| key == "CROWDED_TEST_TOKEN" && value == "t")
        );
    }

    #[cfg(unix)]
    fn spawn_ordinary_pane() -> Pane {
        Pane::spawn(
            room_spec("/bin/sh", &["-c", "sleep 20"]),
            native_test_size(),
            GuestEnvironment::new([]),
        )
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn app_decisions_about_native_rooms() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (native, _fake) = spawn_native_pane(root.path(), cwd.path(), "one", &[], "t");
        let ordinary = spawn_ordinary_pane();
        let mut offline = scroll_fixture(4, 20);
        offline.child.child = None;
        let title = native.title().to_owned();
        let panes = vec![native, ordinary];

        // The detail panel reads legacy session entries a native room lacks.
        assert_eq!(crate::app::rooms_needing_detail(&panes), [1]);

        // Ctrl+W reaches a native room too: it goes through the outbox, not
        // the terminal, so the composer is allowed.
        assert_eq!(crate::app::compose_target(&panes, 1), Ok(0));
        let _ = &title;
        // From the native room the next room is an online ordinary one.
        assert_eq!(crate::app::compose_target(&panes, 0), Ok(1));
        // An offline ordinary room is reported as offline, not as native.
        let panes = vec![panes.into_iter().next().unwrap(), offline];
        let error = crate::app::compose_target(&panes, 0).unwrap_err();
        assert!(error.contains("offline"), "{error}");
    }

    #[test]
    fn native_flag_and_owned_server_must_agree() {
        let mut native = room_spec("opencode", &[]);
        native.native_delivery = true;
        let error =
            Pane::spawn_with_native(native, native_test_size(), GuestEnvironment::new([]), None)
                .err()
                .unwrap();
        assert!(error.to_string().contains("has no owned server"));
    }

    #[test]
    fn native_rooms_are_skipped_by_resume_and_the_user_is_told() {
        let mut native = room_spec("opencode", &[]);
        native.native_delivery = true;
        let ordinary = room_spec("claude", &[]);
        let mut specs = vec![native, ordinary];
        let resumed = resume_supported_specs(&mut specs);
        assert!(!resumed[0]);
        assert!(specs[0].args.is_empty());
        let notice = native_resume_notice(&specs).unwrap();
        assert_eq!(notice, "Resume: native room 1 started a new session");
        // Every native room is named, by number, in a short line.
        let mut many: Vec<RoomSpec> = (0..8).map(|_| room_spec("claude", &[])).collect();
        for index in [1, 4, 7] {
            many[index] = room_spec("opencode", &[]);
            many[index].native_delivery = true;
        }
        let notice = native_resume_notice(&many).unwrap();
        assert_eq!(notice, "Resume: native room 2,5,8 started a new session");
        assert!(notice.len() < 60);
        assert!(native_resume_notice(&specs[1..]).is_none());
        // A native Codex room resumes its stored session, so it is not named.
        let mut codex = room_spec("codex", &[]);
        codex.native_delivery = true;
        assert!(native_resume_notice(&[codex]).is_none());
    }

    fn arguments(spec: &RoomSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn opencode_model_switch_builds_session_request_with_variant() {
        let body = opencode_model_switch_body("provider/model#high").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "model": {"id": "model", "providerID": "provider", "variant": "high"}
            })
        );
        assert_eq!(
            opencode_model_switch_args("ses-exact", &body),
            [
                "api",
                "--standalone",
                "--data",
                body.as_str(),
                "POST",
                "/api/session/ses-exact/model"
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn opencode_model_switch_omits_missing_variant() {
        let body = opencode_model_switch_body("provider/model").unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"model": {"id": "model", "providerID": "provider"}})
        );
    }

    #[test]
    fn opencode_model_switch_rejects_malformed_model() {
        for model in ["model", "/model", "provider/", "provider/model#", "p/m#v#x"] {
            assert!(opencode_model_switch_body(model).is_err(), "{model}");
        }
    }

    #[test]
    fn opencode_model_switch_resolves_only_resumed_sessions() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = temporary_bin_directory();
        let fresh = room_spec("opencode", &["--model", "provider/model"]);
        assert_eq!(opencode_resume_session_id(&fresh, &cwd).unwrap(), None);

        let other = room_spec("claude", &["--continue"]);
        assert_eq!(opencode_resume_session_id(&other, &cwd).unwrap(), None);

        let continued = room_spec("opencode", &["--continue"]);
        assert!(
            opencode_resume_session_id(&continued, &cwd)
                .unwrap_err()
                .to_string()
                .contains("use clear first")
        );

        let mut mapped = continued.clone();
        mapped.title = "mapped room".into();
        session_state::upsert("opencode", &cwd, &mapped.title, "ses-mapped");
        assert_eq!(
            opencode_resume_session_id(&mapped, &cwd)
                .unwrap()
                .as_deref(),
            Some("ses-mapped")
        );

        let exact = room_spec("opencode", &["--session", "ses-exact", "--continue"]);
        assert_eq!(
            opencode_resume_session_id(&exact, &cwd).unwrap().as_deref(),
            Some("ses-exact")
        );
        fs::remove_dir_all(cwd).unwrap();
    }

    #[cfg(unix)]
    fn fake_opencode(root: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable = bin.join("opencode");
        fs::write(&executable, format!("#!/bin/sh\n{script}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[test]
    fn opencode_model_switch_failure_keeps_the_running_room() {
        let _state = session_state::StateRootGuard::isolated();
        let root = temporary_bin_directory();
        let bin = fake_opencode(
            &root,
            "if [ \"$1\" = api ]; then printf '%s\\n' \"$@\" > \"$SWITCH_LOG\"; exit 23; fi; exec /bin/sleep 60",
        );
        let log = root.join("api-args.txt");
        let mut spec = room_spec(
            "opencode",
            &["--session", "ses-exact", "--model", "provider/old"],
        );
        spec.cwd = Some(root.clone());
        spec.variables
            .push(("PATH".into(), bin.as_os_str().to_os_string()));
        spec.variables
            .push(("SWITCH_LOG".into(), log.as_os_str().to_os_string()));
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut pane = Pane::spawn(spec, size, GuestEnvironment::new([])).unwrap();
        let original_pid = pane.child.child.as_ref().unwrap().process_id();
        let malformed = pane
            .configure(Some("bad-model"), None, size)
            .unwrap_err()
            .to_string();
        assert!(malformed.contains("provider/model[#variant]"));
        assert_eq!(
            pane.child.child.as_ref().unwrap().process_id(),
            original_pid
        );
        assert!(!log.exists());
        let error = pane
            .configure(Some("provider/new#high"), None, size)
            .unwrap_err()
            .to_string();
        assert!(error.contains("exit status: 23"), "{error}");
        assert_eq!(
            pane.child.child.as_ref().unwrap().process_id(),
            original_pid
        );
        assert_eq!(
            fs::read_to_string(log).unwrap(),
            "api\n--standalone\n--data\n{\"model\":{\"id\":\"new\",\"providerID\":\"provider\",\"variant\":\"high\"}}\nPOST\n/api/session/ses-exact/model\n"
        );
        pane.cleanup();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_continue_keeps_the_running_opencode_room() {
        let _state = session_state::StateRootGuard::isolated();
        let root = temporary_bin_directory();
        let bin = fake_opencode(
            &root,
            "if [ \"$1\" = api ]; then printf called > \"$SWITCH_LOG\"; exit 0; fi; exec /bin/sleep 60",
        );
        let log = root.join("api-called");
        let mut spec = room_spec("opencode", &["--continue", "--model", "provider/old"]);
        spec.cwd = Some(root.clone());
        spec.variables
            .push(("PATH".into(), bin.as_os_str().to_os_string()));
        spec.variables
            .push(("SWITCH_LOG".into(), log.as_os_str().to_os_string()));
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut pane = Pane::spawn(spec, size, GuestEnvironment::new([])).unwrap();
        let original_pid = pane.child.child.as_ref().unwrap().process_id();
        let error = pane
            .configure(Some("provider/new"), None, size)
            .unwrap_err()
            .to_string();
        assert!(error.contains("continued OpenCode session"));
        assert!(error.contains("use clear first"));
        assert_eq!(
            pane.child.child.as_ref().unwrap().process_id(),
            original_pid
        );
        assert!(!log.exists());
        pane.cleanup();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn fresh_opencode_model_change_skips_the_api_call() {
        let _state = session_state::StateRootGuard::isolated();
        let root = temporary_bin_directory();
        let bin = fake_opencode(
            &root,
            "if [ \"$1\" = api ]; then printf called > \"$SWITCH_LOG\"; exit 0; fi; exec /bin/sleep 60",
        );
        let log = root.join("api-called");
        let mut spec = room_spec("opencode", &["--model", "provider/old"]);
        spec.cwd = Some(root.clone());
        spec.variables
            .push(("PATH".into(), bin.as_os_str().to_os_string()));
        spec.variables
            .push(("SWITCH_LOG".into(), log.as_os_str().to_os_string()));
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut pane = Pane::spawn(spec, size, GuestEnvironment::new([])).unwrap();
        pane.configure(Some("provider/new"), None, size).unwrap();
        assert!(!log.exists());
        pane.cleanup();
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_opencode_model_change_does_not_call_the_opencode_api() {
        let root = temporary_bin_directory();
        let bin = fake_opencode(
            &root,
            "if [ \"$1\" = api ]; then printf called > \"$SWITCH_LOG\"; exit 0; fi; exec /bin/sleep 60",
        );
        std::os::unix::fs::symlink(bin.join("opencode"), bin.join("claude")).unwrap();
        let log = root.join("api-called");
        let mut spec = room_spec("claude", &["--model", "old"]);
        spec.cwd = Some(root.clone());
        spec.variables
            .push(("PATH".into(), bin.as_os_str().to_os_string()));
        spec.variables
            .push(("SWITCH_LOG".into(), log.as_os_str().to_os_string()));
        let size = PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut pane = Pane::spawn(spec, size, GuestEnvironment::new([])).unwrap();
        pane.configure(Some("new"), None, size).unwrap();
        assert!(!log.exists());
        pane.cleanup();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opencode_launch_moves_model_to_config_without_changing_spec() {
        for model_args in [
            vec!["--model", "opencode-go/mimo-v2.5#high"],
            vec!["-m", "opencode-go/mimo-v2.5#high"],
            vec!["--model=opencode-go/mimo-v2.5#high"],
            vec!["-m=opencode-go/mimo-v2.5#high"],
        ] {
            let mut args = model_args;
            args.extend(["--yolo", "--session", "ses_test", "--continue"]);
            let spec = room_spec("opencode", &args);
            let launch = opencode_launch(&spec).unwrap();
            assert_eq!(arguments(&spec), args);
            assert_eq!(
                arguments(&launch),
                [
                    "--standalone",
                    "--yolo",
                    "--session",
                    "ses_test",
                    "--continue"
                ]
            );
            let config: serde_json::Value = serde_json::from_str(
                &environment_value(&launch.variables, "OPENCODE_CONFIG_CONTENT")
                    .unwrap()
                    .to_string_lossy(),
            )
            .unwrap();
            assert_eq!(config["model"], "opencode-go/mimo-v2.5#high");
        }
    }

    #[test]
    fn opencode_launch_preserves_config_and_strips_duplicate_model_options() {
        let mut spec = room_spec(
            "opencode",
            &[
                "--standalone",
                "--model",
                "provider/selected",
                "-m",
                "provider/other",
                "--model=provider/third",
                "--yolo",
            ],
        );
        spec.variables.push(("OPENCODE_CONFIG_CONTENT".into(), r#"{"model":"old/model","permissions":[{"action":"shell","resource":"*","effect":"ask"}],"mcp":{"shared":{"type":"local","command":["fake"]}},"plugin":["fake-plugin"]}"#.into()));
        let launch = opencode_launch(&spec).unwrap();
        assert_eq!(arguments(&launch), ["--standalone", "--yolo"]);
        let before: serde_json::Value =
            serde_json::from_str(&spec.variables[0].1.to_string_lossy()).unwrap();
        let after: serde_json::Value =
            serde_json::from_str(&launch.variables[0].1.to_string_lossy()).unwrap();
        assert_eq!(after["model"], "provider/selected");
        for key in ["permissions", "mcp", "plugin"] {
            assert_eq!(after[key], before[key]);
        }
    }

    #[test]
    fn opencode_launch_without_model_only_adds_standalone() {
        let spec = room_spec("opencode", &["--yolo", "--continue"]);
        let launch = opencode_launch(&spec).unwrap();
        assert_eq!(arguments(&launch), ["--standalone", "--yolo", "--continue"]);
        assert_eq!(launch.variables, spec.variables);
    }

    #[test]
    fn opencode_launch_sample() {
        let mut spec = room_spec("opencode", &["--model", "opencode-go/mimo-v2.5", "--yolo"]);
        spec.variables.push((
            "OPENCODE_CONFIG_CONTENT".into(),
            r#"{"providers":{"opencode-go":{"settings":{"apiKey":"scratch-no-call"}}}}"#.into(),
        ));
        let launch = opencode_launch(&spec).unwrap();
        assert_eq!(arguments(&launch), ["--standalone", "--yolo"]);
        println!(
            "child argv: {:?}; OPENCODE_CONFIG_CONTENT={}",
            arguments(&launch),
            launch.variables[0].1.to_string_lossy()
        );
    }

    #[test]
    fn opencode_launch_leaves_other_guests_unchanged() {
        for program in ["claude", "codex", "sh"] {
            let mut spec = room_spec(program, &["--model", "unchanged", "--continue"]);
            spec.variables.push(("EXAMPLE".into(), "unchanged".into()));
            let launch = opencode_launch(&spec).unwrap();
            assert_eq!(launch.program, spec.program);
            assert_eq!(launch.args, spec.args);
            assert_eq!(launch.variables, spec.variables);
        }
    }

    #[test]
    fn opencode_launch_adapts_before_headroom_wrapping() {
        let (directory, path, ext) = headroom_path();
        let mut spec = room_spec("opencode", &["--model", "opencode-go/mimo-v2.5", "--yolo"]);
        spec.use_headroom = true;
        spec.headroom_args = vec!["--budget".into(), "5000".into()];
        let launch = opencode_launch(&spec).unwrap();
        let (program, args, active) = headroom_launch(
            &launch.program,
            &launch.args,
            launch.use_headroom,
            &launch.headroom_args,
            &path,
            &ext,
        );
        assert!(active);
        assert_eq!(program, OsString::from("headroom"));
        assert_eq!(
            args,
            [
                "wrap",
                "opencode",
                "--budget",
                "5000",
                "--standalone",
                "--yolo"
            ]
            .map(OsString::from)
        );
        assert!(
            environment_value(&launch.variables, "OPENCODE_CONFIG_CONTENT")
                .unwrap()
                .to_string_lossy()
                .contains("opencode-go/mimo-v2.5")
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_server_command_wraps_the_server_only_when_headroom_is_active() {
        let mut spec = room_spec("opencode", &[]);
        spec.headroom_args = vec!["--no-serena".into()];
        let (program, prefix) = native_server_command(&spec, false);
        assert_eq!(program, OsString::from("opencode"));
        assert!(prefix.is_empty());
        let (program, prefix) = native_server_command(&spec, true);
        assert_eq!(Path::new(&program).file_name().unwrap(), "headroom");
        assert_eq!(
            prefix,
            ["wrap", "opencode", "--no-serena", "--"].map(OsString::from)
        );
    }

    fn temporary_bin_directory() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = env::temp_dir().join(format!(
            "crowded-pane-test-{}-{nonce}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        directory
    }

    #[test]
    fn whisper_quotes_shell_metacharacters() {
        assert_eq!(
            String::from_utf8(
                whisper_parts(Transport::Shell, false, "Room 1", "it's $HOME; echo nope",).0
            )
            .unwrap(),
            "printf '%s\\n' '[whisper from Room 1] it'\"'\"'s $HOME; echo nope'\r"
        );
        let (body, submit) = whisper_parts(Transport::Raw, false, "Claude", "hello");
        assert_eq!(body, b"[whisper from Claude] hello");
        assert_eq!(submit, Some(RAW_SUBMIT_BYTES.to_vec()));

        let (body, submit) = whisper_parts(
            Transport::Raw,
            true,
            "OpenCode · 3",
            "[task: exercise | requested role: result]\nstatus: done",
        );
        assert_eq!(
            body,
            b"\x1b[200~[whisper from OpenCode \xc2\xb7 3] [task: exercise | requested role: result]\nstatus: done\x1b[201~"
        );
        assert_eq!(submit, Some(RAW_SUBMIT_BYTES.to_vec()));
    }

    #[test]
    fn opencode_room_whispers_as_a_bracketed_paste() {
        let spec = room_spec("opencode", &[]);
        let bracketed = controls::uses_bracketed_paste(&spec);
        assert!(bracketed, "an OpenCode room must use a bracketed paste");
        let (body, submit) = whisper_parts(spec.transport, bracketed, "Room 4", "hello");
        assert_eq!(body, b"\x1b[200~[whisper from Room 4] hello\x1b[201~");
        assert_eq!(submit, Some(RAW_SUBMIT_BYTES.to_vec()));
    }

    #[test]
    fn whisper_submit_is_a_distinct_write_after_the_paste_end_marker() {
        let (mut pane, captured) = pane_with_capture(4, 20);
        // Agent rooms take a bracketed paste, the path where the dropped
        // submit was reported.
        pane.spec.program = "codex".into();
        pane.send_whisper("Room 1", "hello").unwrap();
        let expected_body = b"\x1b[200~[whisper from Room 1] hello\x1b[201~";
        let mut expected = expected_body.to_vec();
        expected.extend_from_slice(RAW_SUBMIT_BYTES);
        wait_for_captured(&captured, &expected);
        let bytes = captured.lock().unwrap().clone();
        assert!(
            bytes.starts_with(expected_body),
            "the paste body must be written first, got {:?}",
            bytes
        );
        let submit = &bytes[expected_body.len()..];
        assert_eq!(
            submit, RAW_SUBMIT_BYTES,
            "the submit must be exactly the platform submit bytes (bare CR on Unix, CRLF on \
             Windows), written as its own trailing write and never merged into the paste body"
        );
        assert!(
            submit.starts_with(b"\r"),
            "the submit must always lead with a carriage return"
        );
    }

    #[test]
    fn shift_tab_uses_the_terminal_backtab_sequence() {
        let key = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(key_bytes(key), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn cursor_position_query_is_answered_across_output_chunks() {
        let mut response = Vec::new();
        let mut tail = Vec::new();
        respond_to_terminal_queries(&mut response, &mut tail, b"before\x1b[").unwrap();
        respond_to_terminal_queries(&mut response, &mut tail, b"6nafter").unwrap();
        assert_eq!(response, CURSOR_POSITION_RESPONSE);
    }

    #[test]
    fn rooms_without_cwd_use_crowdeds_launch_directory() {
        assert_eq!(
            working_directory(None).unwrap(),
            env::current_dir().unwrap()
        );
    }

    #[test]
    fn opencode_is_ready_only_at_its_idle_normal_prompt() {
        assert!(opencode_input_ready("Ask anything... tab agents"));
        assert!(opencode_input_ready("tab agents  ctrl+p commands"));
        assert!(!opencode_input_ready("Ask anything... esc interrupt"));
        assert!(!opencode_input_ready("ctrl+p commands  esc interrupt"));
        assert!(!opencode_input_ready(
            "Run a command... ctrl+p commands  esc exit shell mode"
        ));
    }

    #[test]
    fn opencode_v2_space_padded_home_screen_reports_ready() {
        let rows = ["Ask anything…", "", "Build auto", "", "ctrl+p commands"];
        for padding in [13, 80] {
            let screen = rows
                .into_iter()
                .chain(std::iter::repeat_n("", padding))
                .chain(["2.0.20", ""])
                .map(|line| format!("{line:120}\n"))
                .collect::<String>();
            assert!(opencode_input_ready(&screen));
            assert!(!opencode_input_ready(&format!("{screen}esc interrupt")));
        }
    }

    #[cfg(unix)]
    #[test]
    fn opencode_wrapped_rooms_keep_distinct_models_after_inline_config_is_replaced() {
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let bin = fake_opencode(root.path(), "exit 0");
        let headroom = bin.join("headroom");
        fs::copy(bin.join("opencode"), &headroom).unwrap();
        fs::write(&headroom, "#!/bin/sh\nexport OPENCODE_CONFIG_CONTENT='{\"model\":\"shared/default\"}'\nif [ -n \"$OPENCODE_CONFIG\" ]; then cat \"$OPENCODE_CONFIG\"; else printf '%s' \"$OPENCODE_CONFIG_CONTENT\"; fi\nsleep 2\n").unwrap();
        let search_path = env::join_paths(
            std::iter::once(bin).chain(env::split_paths(&env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let mut panes = Vec::new();
        for model in ["opencode-go/mimo-v2.5", "opencode-go/deepseek-v4-flash"] {
            let mut spec = room_spec("opencode", &["--model", model]);
            spec.cwd = Some(root.path().to_path_buf());
            spec.use_headroom = true;
            spec.variables.push(("PATH".into(), search_path.clone()));
            let mut pane = Pane::spawn(
                spec,
                PtySize {
                    rows: 8,
                    cols: 160,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                GuestEnvironment::new([]),
            )
            .unwrap();
            let started = Instant::now();
            while started.elapsed() < Duration::from_secs(1) {
                pane.drain_output().unwrap();
                if pane.parser.screen().contents().contains(model) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            assert!(pane.parser.screen().contents().contains(model));
            panes.push(pane);
        }
        let paths: Vec<_> = panes
            .iter()
            .map(|pane| pane._opencode_config.as_ref().unwrap().path().to_path_buf())
            .collect();
        assert_ne!(paths[0], paths[1]);
        let size = PtySize {
            rows: 8,
            cols: 160,
            pixel_width: 0,
            pixel_height: 0,
        };
        panes[0].restart(size).unwrap();
        panes[1].resume_context(size).unwrap();
        let replacement_paths: Vec<_> = panes
            .iter()
            .map(|pane| pane._opencode_config.as_ref().unwrap().path().to_path_buf())
            .collect();
        assert_ne!(paths, replacement_paths);
        assert!(paths.iter().all(|path| !path.exists()));
        assert!(replacement_paths.iter().all(|path| path.exists()));
        drop(panes);
        assert!(replacement_paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn opencode_wrapped_config_merges_plain_json_base_with_room_values_winning() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("custom.json");
        fs::write(&path, r#"{"model":"base/model","mcp":{"shared":{"command":["base"]},"room":{"command":["old"]}},"plugin":["base-plugin"],"settings":{"preserved":true,"selected":"base"}}"#).unwrap();
        let mut spec = room_spec("opencode", &["--model", "room/model"]);
        spec.cwd = Some(root.path().to_path_buf());
        spec.variables
            .push(("OPENCODE_CONFIG".into(), "custom.json".into()));
        spec.variables.push((
            "OPENCODE_CONFIG_CONTENT".into(),
            r#"{"mcp":{"room":{"command":["new"]}},"settings":{"selected":"room"}}"#.into(),
        ));
        let mut launch = opencode_launch(&spec).unwrap();
        let file = opencode_wrapped_config(&mut launch, true).unwrap().unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(file.path()).unwrap()).unwrap();
        assert_eq!(file.path().parent(), path.parent());
        assert_eq!(config["model"], "room/model");
        assert_eq!(
            config["mcp"]["shared"]["command"],
            serde_json::json!(["base"])
        );
        assert_eq!(config["mcp"]["room"]["command"], serde_json::json!(["new"]));
        assert_eq!(config["plugin"], serde_json::json!(["base-plugin"]));
        assert_eq!(config["settings"]["preserved"], true);
        assert_eq!(config["settings"]["selected"], "base");
        assert!(fs::read_to_string(&path).unwrap().contains("base/model"));
    }

    #[test]
    fn opencode_wrapped_config_excludes_ambient_plugins_and_providers() {
        let mut spec = room_spec("opencode", &["--model", "room/model"]);
        spec.variables.push((
            "OPENCODE_CONFIG_CONTENT".into(),
            r#"{"plugin":["ambient-plugin"],"providers":{"headroom":{}},"mcp":{"crowded":{"enabled":false}}}"#.into(),
        ));
        let mut launch = opencode_launch(&spec).unwrap();
        let file = opencode_wrapped_config(&mut launch, true).unwrap().unwrap();
        let config: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(file.path()).unwrap()).unwrap();
        assert_eq!(config["model"], "room/model");
        assert_eq!(config["mcp"]["crowded"]["enabled"], false);
        let mcp = config["mcp"].as_object().unwrap();
        assert!(
            mcp.keys().all(|key| key == "crowded" || key == "headroom"),
            "{config}"
        );

        let headroom_plugins = config
            .get("plugin")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|entries| {
                !entries.is_empty()
                    && entries.iter().all(|value| {
                        value.as_str().is_some_and(|name| {
                            name.contains("headroom") && name != "ambient-plugin"
                        })
                    })
            });
        assert!(
            config.get("plugin").is_none() || headroom_plugins,
            "ambient plugins must be excluded; only headroom auto-injected plugins are tolerated: {config}"
        );

        let headroom_providers = |value: &serde_json::Value| {
            value.as_object().is_some_and(|providers| {
                !providers.is_empty() && providers.keys().all(|key| key == "headroom")
            })
        };
        let headroom_provider = config.get("provider").is_some_and(headroom_providers);
        let headroom_providers_plural = config.get("providers").is_some_and(headroom_providers);
        assert!(
            config.get("provider").is_none() || headroom_provider,
            "ambient providers must be excluded; only headroom auto-injected provider is tolerated: {config}"
        );
        assert!(
            config.get("providers").is_none() || headroom_providers_plural,
            "ambient providers must be excluded; only headroom auto-injected providers are tolerated: {config}"
        );

        let keys = config.as_object().unwrap();
        for key in keys.keys() {
            assert!(
                matches!(
                    key.as_str(),
                    "model" | "mcp" | "plugin" | "provider" | "providers"
                ),
                "unexpected top-level key {key}: {config}"
            );
        }
        assert!(keys.contains_key("model") && keys.contains_key("mcp"));
        if headroom_plugins {
            assert!(keys.contains_key("plugin"));
        }
        if headroom_provider {
            assert!(keys.contains_key("provider"));
        }
        if headroom_providers_plural {
            assert!(keys.contains_key("providers"));
        }
    }

    #[test]
    fn opencode_wrapped_config_rejects_unreadable_file_with_path() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("missing.json");
        let mut spec = room_spec("opencode", &["--model", "room/model"]);
        spec.variables
            .push(("OPENCODE_CONFIG".into(), path.as_os_str().into()));
        let mut launch = opencode_launch(&spec).unwrap();
        let error = opencode_wrapped_config(&mut launch, true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn opencode_wrapped_config_rejects_jsonc_with_path_and_parse_reason() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("custom.jsonc");
        fs::write(&path, "{ // JSONC comment\n\"model\":\"base/model\"\n}").unwrap();
        let mut spec = room_spec("opencode", &["--model", "room/model"]);
        spec.variables
            .push(("OPENCODE_CONFIG".into(), path.as_os_str().into()));
        let mut launch = opencode_launch(&spec).unwrap();
        let error = opencode_wrapped_config(&mut launch, true).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()));
        assert!(message.contains("plain JSON (JSONC is unsupported)"));
        assert!(message.contains("line 1"));
    }
    #[test]
    fn opencode_v2_captured_idle_home_screens_report_ready() {
        const CONTENT_ENABLED: &str = concat!(
            "┃  Ask anything… \"What is the tech stack of this project?\"\n",
            "┃\n",
            "┃  Build · MiMo V2.5 OpenCode Go\n",
            "╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n",
            "/…/opencode-v2-room2-re…/content-enabled  shift+tab agents  ctrl+p commands\n",
            "\n\n\n\n\n\n\n\n\n\n\n\n\n"
        );
        const VAL_LAUNCH: &str = concat!(
            "┃  Ask anything… \"What is the tech stack of this project?\"\n",
            "┃\n",
            "┃  Build auto · MiMo V2.5 OpenCode Go\n",
            "╹▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀\n",
            "/…/opencode-v2-room2-research/val-launch  shift+tab agents  ctrl+p commands\n",
            "\n\n\n\n\n\n\n\n\n\n\n\n\n"
        );
        assert!(opencode_input_ready(CONTENT_ENABLED));
        assert!(opencode_input_ready(VAL_LAUNCH));
    }

    #[test]
    fn opencode_v2_busy_screen_status_blocks_input() {
        const LOOPBACK_BUSY: &str = concat!(
            "┃  readiness probe\n",
            "\n",
            "──────────────────────────────── ⠼ Compaction ────────────────────────────────\n",
            "\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n",
            "⬝⬝⬝■■■■■ esc interrupt  shift+tab agents  ctrl+p commands\n"
        );
        assert!(!opencode_input_ready(LOOPBACK_BUSY));
        assert!(!opencode_input_ready(
            "Ask anything\nctrl+p commands\nesc again to interrupt"
        ));
    }

    #[test]
    fn opencode_resumed_history_with_esc_interrupt_still_reports_ready() {
        let screen = format!(
            "{}{}{}{}",
            "history line 1\n".repeat(30),
            "esc interrupt was shown earlier\n",
            "history line\n".repeat(10),
            "Ask anything"
        );
        assert!(opencode_input_ready(&screen));
        let busy_prompt = "Ask anything  esc interrupt";
        assert!(!opencode_input_ready(busy_prompt));
        let long_history = format!("{}{}", "Ask anything\n", "x\n".repeat(500));
        assert!(!opencode_input_ready(&long_history));
    }

    #[test]
    fn opencode_ready_marker_wrapped_across_lines_by_a_narrow_pane_still_reports_ready() {
        let screen = "history line\n".repeat(10)
            + " /Users/bruno/Documents/ 184.9K (18%) ctrl+p    \n"
            + " Development/bruno/                   commands  \n"
            + " crowded                                        \n";
        assert!(opencode_input_ready(&screen));
    }

    #[test]
    fn opencode_narrow_31_cols_ask_truncated_still_reports_ready() {
        // At 31 cols (inner crowded resume with 3 rooms on 80x24 outer),
        // "Ask anything" is truncated to "Ask tests\"" and no longer matches.
        // The status footer's "ctrl+p commands" hint survives truncation on
        // its own lines and is enough to anchor the prompt.
        let screen = "history line\n".repeat(5)
            + "Ask tests\"\n"
            + "Build\n"
            + "ctrl+p    \n"
            + "commands  \n";
        assert!(opencode_input_ready(&screen));
        // Busy still blocks
        assert!(!opencode_input_ready(
            "Ask tests\"\nBuild\nctrl+p    \ncommands  \nesc interrupt"
        ));
    }

    #[test]
    fn conductor_rewrites_native_launch_options() {
        let mut claude = room_spec("claude", &["--continue", "--model", "old", "--effort=low"]);
        controls::set_model(&mut claude, "sonnet").unwrap();
        controls::set_effort(&mut claude, "high").unwrap();
        controls::clear_resume_args(&mut claude).unwrap();
        assert_eq!(
            arguments(&claude),
            ["--effort", "high", "--model", "sonnet"]
        );

        let mut codex = room_spec(
            "codex",
            &[
                "-c",
                "sandbox_mode=\"workspace-write\"",
                "-c",
                "model_reasoning_effort=\"low\"",
                "resume",
                "--last",
            ],
        );
        controls::set_effort(&mut codex, "xhigh").unwrap();
        assert_eq!(
            &arguments(&codex)[..4],
            [
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-c",
                "sandbox_mode=\"workspace-write\"",
            ]
        );
        controls::clear_resume_args(&mut codex).unwrap();
        assert_eq!(arguments(&codex).len(), 4);

        let mut opencode = room_spec(
            "opencode",
            &["--continue", "--session", "session-1", "--model=old"],
        );
        controls::clear_resume_args(&mut opencode).unwrap();
        assert_eq!(arguments(&opencode), ["--model=old"]);
    }

    fn headroom_path() -> (PathBuf, Option<OsString>, Option<OsString>) {
        let directory = temporary_bin_directory();
        let ext = if cfg!(windows) {
            fs::write(directory.join("headroom.exe"), "").unwrap();
            Some(OsString::from(".exe;.cmd"))
        } else {
            fs::write(directory.join("headroom"), "").unwrap();
            None
        };
        (
            directory.clone(),
            Some(directory.as_os_str().to_os_string()),
            ext,
        )
    }

    #[test]
    fn spawn_launch_wraps_through_headroom_only_when_flag_and_binary_agree() {
        let (directory, path, ext) = headroom_path();
        let original = &["--continue", "--model", "sonnet"];

        let headroom_args = [OsString::from("--budget"), OsString::from("5000")];
        let (program, args, active) = headroom_launch(
            OsStr::new("claude"),
            &original.iter().map(OsString::from).collect::<Vec<_>>(),
            true,
            &headroom_args,
            &path,
            &ext,
        );
        assert_eq!(program, OsString::from("headroom"));
        // headroom's own args land after the wrapped program (its subcommand
        // name), before the environment's own arguments.
        assert_eq!(
            args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>(),
            [
                "wrap",
                "claude",
                "--budget",
                "5000",
                "--continue",
                "--model",
                "sonnet"
            ]
        );
        assert!(active);

        let (program, args, active) = headroom_launch(
            OsStr::new("claude"),
            &original.iter().map(OsString::from).collect::<Vec<_>>(),
            false,
            &[],
            &path,
            &ext,
        );
        assert_eq!(program, OsString::from("claude"));
        assert_eq!(
            args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>(),
            original
        );
        assert!(!active);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn spawn_launch_falls_back_unchanged_when_headroom_is_missing() {
        let empty = temporary_bin_directory();
        let path = Some(empty.as_os_str().to_os_string());
        let original = &["--continue"];
        let args: Vec<OsString> = original.iter().map(OsString::from).collect();

        let (program, args, active) =
            headroom_launch(OsStr::new("claude"), &args, true, &[], &path, &None);
        assert_eq!(program, OsString::from("claude"));
        assert_eq!(
            args.iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>(),
            original
        );
        assert!(!active);
        fs::remove_dir_all(empty).unwrap();
    }

    #[test]
    fn spec_uses_headroom_mirrors_the_launch_wrap_decision() {
        let (directory, path, ext) = headroom_path();
        let mut spec = room_spec("claude", &["--continue"]);
        spec.variables
            .push((OsString::from("PATH"), path.clone().unwrap()));
        if let Some(ext) = ext {
            spec.variables.push((OsString::from("PATHEXT"), ext));
        }
        // Flag set and binary present: wrapped, exactly as the launch is.
        spec.use_headroom = true;
        assert!(spec_uses_headroom(&spec));
        // Flag cleared: never wrapped even with the binary present.
        spec.use_headroom = false;
        assert!(!spec_uses_headroom(&spec));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resume_supported_specs_skips_guests_without_a_conductor_adapter() {
        // Isolate the captured-session lookup so parallel tests that seed the
        // session state cannot leak ids into this resume path.
        let _state = session_state::StateRootGuard::isolated();

        let claude = room_spec("claude", &["--model", "sonnet"]);
        let mut shell = room_spec("/bin/sh", &[]);
        shell.transport = Transport::Shell;
        let shell_args_before = shell.args.clone();
        let unknown = room_spec("raw:editor", &[]);

        let mut specs = [claude, shell, unknown];
        let resumed = resume_supported_specs(&mut specs);

        assert_eq!(arguments(&specs[0]), ["--model", "sonnet", "--continue"]);
        // No Conductor adapter for a shell room: left byte-identical.
        assert_eq!(specs[1].args, shell_args_before);
        // Reports, per spec, whether resume args were actually applied.
        assert_eq!(resumed, [true, false, false]);
    }
    fn scroll_fixture(rows: u16, cols: u16) -> Pane {
        let spec = room_spec("fixture", &[]);
        let pty = native_pty_system()
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let writer = PaneWriter::spawn(pty.master.take_writer().unwrap());
        let (_, output_rx) = mpsc::channel();
        Pane {
            spec,
            cwd: env::current_dir().unwrap(),
            spawned_at: SystemTime::now(),
            captured_session: session_state::fresh_capture_cell(),
            environment: GuestEnvironment::new([]),
            child: ChildGuard {
                child: None,
                #[cfg(windows)]
                tree: None,
            },
            master: pty.master,
            writer,
            output_rx,
            response_tail: Vec::new(),
            parser: Parser::new(rows, cols, SCROLLBACK_LINES),
            scroll_offset: 0,
            headroom_active: false,
            _opencode_config: None,
            native: None,
        }
    }

    #[test]
    fn a_native_codex_pane_never_writes_automated_text_to_its_terminal() {
        let (mut pane, captured) = pane_with_capture(4, 20);
        pane.spec.program = "codex".into();
        pane.spec.native_delivery = true;
        assert!(pane.codex_native() && pane.is_native());
        // The intro is agent text: it goes through the router, never the PTY.
        assert!(!pane.needs_intro());
        let error = pane.send_whisper("Room A", "hello").unwrap_err();
        assert!(error.to_string().contains(NATIVE_PASTE_REFUSED), "{error}");
        // Neither may a resubmit or a relaunch paste into it.
        assert!(pane.resend_whisper_submit().is_err());
        assert!(
            pane.restart(PtySize {
                rows: 4,
                cols: 20,
                pixel_width: 0,
                pixel_height: 0,
            })
            .is_err()
        );
        assert!(captured.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    fn codex_stub_for_pane(root: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[cfg(unix)]
    fn pump_router(
        router: &mut crate::native_route::NativeRouter,
        mailroom: &mut Mailroom,
        done: impl Fn(&str) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            router.tick(Instant::now(), mailroom, false, false);
            let summary = mailroom.summary();
            if done(&summary) {
                return;
            }
            assert!(Instant::now() < deadline, "never finished:\n{summary}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The exact code the event loop runs for a doorbell message never touches
    /// a native room's terminal, whatever state its mail is in.
    #[cfg(unix)]
    #[test]
    fn the_doorbell_message_path_never_writes_to_a_native_terminal() {
        use crate::app::{DeliveryFuse, DeliveryGate, route_doorbell_message};
        use crate::doorbell::DoorbellEnvelope;
        use crate::native_route::{CodexEntry, NativeRouter};
        use std::collections::VecDeque;
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (mut pane, captured) = pane_with_capture(4, 20);
        pane.spec.program = "codex".into();
        pane.spec.native_delivery = true;
        let mut panes = vec![pane];
        let mut mailroom = Mailroom::new(50);
        let path = root.path().join("state").join("native-outbox.json");
        let mut router = NativeRouter::start(path, Vec::new()).unwrap();
        let mut fuse = DeliveryFuse::new(0);
        let mut paused = false;
        let mut notice = crate::app::Notices::default();
        let mut deliver = |panes: &mut Vec<Pane>,
                           router: &mut NativeRouter,
                           mailroom: &mut Mailroom,
                           body: &str| {
            let (mut envelope, _answers) = DoorbellEnvelope::for_test();
            envelope.from = 0;
            envelope.to = 0;
            envelope.body = body.to_owned();
            let mut gates = vec![DeliveryGate::new(false)];
            let mut pending = VecDeque::new();
            let mut submits = VecDeque::new();
            route_doorbell_message(
                envelope,
                panes,
                Some(router),
                mailroom,
                &mut gates,
                &[true],
                &mut pending,
                &mut submits,
                &mut fuse,
                &mut paused,
                &mut notice,
                Instant::now(),
            );
            assert!(
                pending.is_empty() && submits.is_empty(),
                "native mail was queued for paste"
            );
            // The writer thread is asynchronous; give a stray write time to land.
            thread::sleep(Duration::from_millis(100));
            notice.ordered_lines().join("; ")
        };

        // Unbound: the router does not know the room at all.
        deliver(&mut panes, &mut router, &mut mailroom, "unbound");
        assert!(
            captured.lock().unwrap().is_empty(),
            "unbound mail reached the terminal"
        );

        // Not started: refused with the launch's reason, nothing pasted.
        router
            .bind_codex(vec![CodexEntry {
                index: 0,
                title: "Room A".into(),
                room: None,
                generation: 1,
                reason: Some("`codex exec` failed: model unavailable".into()),
            }])
            .unwrap();
        let notice = deliver(&mut panes, &mut router, &mut mailroom, "unstarted");
        assert!(
            notice.contains("not saved") && notice.contains("model unavailable"),
            "{notice}"
        );
        assert!(
            captured.lock().unwrap().is_empty(),
            "unstarted mail reached the terminal"
        );

        // Failed: the CLI rejects it; the failure is visible and nothing is pasted.
        let failing = codex_stub_for_pane(
            root.path(),
            "failing",
            "printf 'Error: no rollout found\\n' >&2\nexit 1",
        );
        let room = crate::codex_native::room(
            failing.into_os_string(),
            cwd.path(),
            "thread-a".into(),
            Vec::new(),
        );
        router
            .bind_codex(vec![CodexEntry {
                index: 0,
                title: "Room A".into(),
                room: Some(Arc::new(room)),
                generation: 2,
                reason: None,
            }])
            .unwrap();
        let notice = deliver(&mut panes, &mut router, &mut mailroom, "failing");
        assert!(notice.contains("saved for native delivery"), "{notice}");
        pump_router(&mut router, &mut mailroom, |view| {
            view.contains("FAILED: no rollout found")
        });
        assert!(
            captured.lock().unwrap().is_empty(),
            "failed mail reached the terminal"
        );

        // Uncertain: it may have submitted, so it is never resent or pasted.
        let hanging = codex_stub_for_pane(
            root.path(),
            "hanging",
            "printf 'Queued message sub_1 for thread %s.\\n' 'thread-b'\nexec sleep 30",
        );
        let room = crate::codex_native::room(
            hanging.into_os_string(),
            cwd.path(),
            "thread-b".into(),
            Vec::new(),
        );
        router
            .bind_codex(vec![CodexEntry {
                index: 0,
                title: "Room A".into(),
                room: Some(Arc::new(room)),
                generation: 3,
                reason: None,
            }])
            .unwrap();
        deliver(&mut panes, &mut router, &mut mailroom, "hanging");
        pump_router(&mut router, &mut mailroom, |view| {
            view.contains("UNCERTAIN: may have been accepted")
        });
        thread::sleep(Duration::from_millis(100));
        assert!(
            captured.lock().unwrap().is_empty(),
            "uncertain mail reached the terminal"
        );
    }

    /// A doorbell source the test fills by hand; the real loop polls it the
    /// same way it polls the production doorbell.
    #[cfg(unix)]
    struct FakeDoorbell {
        events: std::collections::VecDeque<crate::doorbell::DoorbellEvent>,
    }

    #[cfg(unix)]
    impl crate::app::DoorbellSource for FakeDoorbell {
        fn try_recv(
            &mut self,
        ) -> Result<crate::doorbell::DoorbellEvent, std::sync::mpsc::TryRecvError> {
            self.events
                .pop_front()
                .ok_or(std::sync::mpsc::TryRecvError::Empty)
        }

        fn path(&self) -> &std::path::Path {
            std::path::Path::new("test-doorbell.sock")
        }
    }

    /// One native Codex room driven by the real per-pass event-loop body, with
    /// its terminal captured so any paste is visible.
    #[cfg(unix)]
    struct IterationHarness {
        panes: Vec<Pane>,
        captured: Arc<Mutex<Vec<u8>>>,
        router: Option<crate::native_route::NativeRouter>,
        mailroom: Mailroom,
        gates: Vec<crate::app::DeliveryGate>,
        last_output: Vec<Option<Instant>>,
        spawned_at: Vec<Instant>,
        pulses: Vec<Option<crate::app::PulseSample>>,
        details: Vec<crate::room_detail::RoomDetail>,
        pending: std::collections::VecDeque<(u64, usize)>,
        submits: std::collections::VecDeque<(Instant, usize, bool)>,
        fuse: crate::app::DeliveryFuse,
        paused: bool,
        notice: crate::app::Notices,
        input_ready_out: Vec<bool>,
        codex: Vec<Option<CodexMint>>,
        sizes: [PtySize; 1],
        environments: Vec<GuestEnvironment>,
        doorbell: FakeDoorbell,
    }

    #[cfg(unix)]
    impl IterationHarness {
        fn new(state: &Path) -> Self {
            let (mut pane, captured) = pane_with_capture(4, 20);
            pane.spec.program = "codex".into();
            pane.spec.native_delivery = true;
            let router = crate::native_route::NativeRouter::start(
                state.join("native-outbox.json"),
                Vec::new(),
            )
            .unwrap();
            let now = Instant::now();
            Self {
                panes: vec![pane],
                captured,
                router: Some(router),
                mailroom: Mailroom::new(50),
                gates: vec![crate::app::DeliveryGate::new(false)],
                last_output: vec![None],
                spawned_at: vec![now],
                pulses: vec![None],
                details: vec![crate::room_detail::RoomDetail::default()],
                pending: std::collections::VecDeque::new(),
                submits: std::collections::VecDeque::new(),
                fuse: crate::app::DeliveryFuse::new(0),
                paused: false,
                notice: crate::app::Notices::default(),
                input_ready_out: Vec::new(),
                codex: vec![None],
                sizes: [small_size(); 1],
                environments: vec![GuestEnvironment::new([])],
                doorbell: FakeDoorbell {
                    events: std::collections::VecDeque::new(),
                },
            }
        }

        fn router_mut(&mut self) -> &mut crate::native_route::NativeRouter {
            self.router.as_mut().expect("the router stays mounted")
        }

        fn bind(
            &mut self,
            room: Option<std::sync::Arc<crate::codex_native::Room>>,
            generation: u64,
            reason: Option<String>,
        ) {
            use crate::native_route::CodexEntry;
            self.router_mut()
                .bind_codex(vec![CodexEntry {
                    index: 0,
                    title: "Room A".into(),
                    room,
                    generation,
                    reason,
                }])
                .unwrap();
        }

        fn healthy_room(
            program: &Path,
            cwd: &Path,
            thread: &str,
        ) -> std::sync::Arc<crate::codex_native::Room> {
            std::sync::Arc::new(crate::codex_native::room(
                program.as_os_str().to_owned(),
                cwd,
                thread.to_owned(),
                Vec::new(),
            ))
        }

        fn send(&mut self, body: &str) -> crate::doorbell::TestAnswers {
            use crate::doorbell::DoorbellEnvelope;
            let (mut envelope, answers) = DoorbellEnvelope::for_test();
            envelope.from = 0;
            envelope.to = 0;
            envelope.body = body.to_owned();
            self.doorbell
                .events
                .push_back(crate::doorbell::DoorbellEvent::Message(envelope));
            answers
        }

        fn push_control(&mut self, action: crate::doorbell::ControlAction) {
            let (reply, _answers) = std::sync::mpsc::sync_channel(1);
            self.doorbell
                .events
                .push_back(crate::doorbell::DoorbellEvent::Control(
                    crate::doorbell::DoorbellControl {
                        from: 0,
                        to: 0,
                        action,
                        reply,
                    },
                ));
        }

        /// Replaces room 1 with a real Codex pane started in `resume`'s thread.
        fn codex_pane_online(
            &mut self,
            program: &Path,
            cwd: &Path,
            title: &str,
            mint: &str,
            resume: &str,
        ) {
            let spec = codex_mint_spec(program, cwd, title, mint);
            self.panes[0] = crate::app::spawn_room(
                &mut crate::app::LivePanes,
                spec,
                small_size(),
                GuestEnvironment::new([]),
                None,
                Some(&CodexMint::Ready(resume.to_owned())),
                None,
            )
            .unwrap();
        }

        /// Replaces room 1 with the placeholder a crashed native room shows.
        fn codex_pane_offline(&mut self, program: &Path, cwd: &Path, title: &str, mint: &str) {
            let spec = codex_mint_spec(program, cwd, title, mint);
            self.panes[0] =
                Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                    .unwrap();
        }

        fn loop_state<'a>(
            &'a mut self,
            focused: usize,
            size: &'a mut dyn FnMut(usize) -> io::Result<PtySize>,
        ) -> crate::app::LoopState<'a> {
            crate::app::loop_state(
                focused,
                100,
                size,
                &mut self.panes,
                self.router.as_mut(),
                &mut self.codex,
                &mut self.gates,
                &mut self.last_output,
                &mut self.spawned_at,
                &mut self.pulses,
                &self.environments,
                &mut self.notice,
            )
        }

        fn iterate(&mut self) {
            let now = Instant::now();
            crate::app::run_rooms_iteration(
                &mut self.doorbell,
                None::<&mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>>,
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
                1,
                now,
            )
            .unwrap();
        }

        fn pump_until(&mut self, what: &str, done: impl Fn(&str) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                self.iterate();
                if done(&self.mailroom.summary()) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "{what} never finished:\n{}",
                    self.mailroom.summary()
                );
                thread::sleep(Duration::from_millis(20));
            }
        }

        fn assert_silent(&self, what: &str) {
            // The writer thread is asynchronous; give a stray write time to land.
            thread::sleep(Duration::from_millis(100));
            assert!(
                self.captured.lock().unwrap().is_empty(),
                "{what} reached the terminal"
            );
        }
    }

    /// The real event-loop pass routes every native Codex state through the
    /// router; none of unbound, not-started, failed or uncertain mail may
    /// touch the room's terminal.
    #[cfg(unix)]
    #[test]
    fn run_rooms_iteration_never_writes_native_codex_mail_to_the_terminal() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let mut rig = IterationHarness::new(root.path());

        // Unbound: the router does not know the room at all.
        let answers = rig.send("unbound");
        rig.iterate();
        assert!(
            answers.read().1 == "rejected",
            "unbound mail must be refused"
        );
        rig.assert_silent("unbound mail");

        // Not started: refused with the launch's reason, nothing pasted.
        rig.bind(
            None,
            1,
            Some("`codex exec` failed: model unavailable".into()),
        );
        let answers = rig.send("unstarted");
        rig.iterate();
        let (_, status, _, error) = answers.read();
        assert_eq!(status, "rejected");
        assert!(
            error.unwrap().contains("model unavailable"),
            "the reason must stay visible"
        );
        assert!(
            rig.notice
                .ordered_lines()
                .join("; ")
                .contains("model unavailable")
        );
        rig.assert_silent("unstarted mail");

        // Failed: the CLI rejects it; the failure is visible, nothing pasted.
        let failing = codex_stub_for_pane(
            root.path(),
            "failing",
            "printf 'Error: no rollout found\\n' >&2\nexit 1",
        );
        rig.bind(
            Some(IterationHarness::healthy_room(
                &failing,
                cwd.path(),
                "thread-a",
            )),
            2,
            None,
        );
        rig.send("failing");
        rig.pump_until("failed mail", |view| {
            view.contains("FAILED: no rollout found")
        });
        rig.assert_silent("failed mail");

        // Uncertain: it may have submitted, so it is never resent or pasted.
        let hanging = codex_stub_for_pane(
            root.path(),
            "hanging",
            "printf 'Queued message sub_1 for thread %s.\\n' 'thread-b'\nexec sleep 30",
        );
        rig.bind(
            Some(IterationHarness::healthy_room(
                &hanging,
                cwd.path(),
                "thread-b",
            )),
            3,
            None,
        );
        rig.send("hanging");
        rig.pump_until("uncertain mail", |view| {
            view.contains("UNCERTAIN: may have been accepted")
        });
        rig.assert_silent("uncertain mail");
    }

    /// The same real pass delivers a healthy room's mail through the router:
    /// the doorbell hears "accepted" and the terminal still sees nothing.
    #[cfg(unix)]
    #[test]
    fn run_rooms_iteration_delivers_healthy_codex_mail_through_the_router() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let program = codex_stub_for_pane(
            root.path(),
            "codex",
            "printf 'Queued message sub_1 for thread thread-a.\\n'",
        );
        let mut rig = IterationHarness::new(root.path());
        rig.bind(
            Some(IterationHarness::healthy_room(
                &program,
                cwd.path(),
                "thread-a",
            )),
            1,
            None,
        );
        let answers = rig.send("hello healthy");
        rig.pump_until("healthy mail", |view| view.contains("ACCEPTED by Codex"));
        let (ok, status, _, _) = answers.read();
        assert!(
            ok && status == "accepted",
            "healthy mail must be accepted through the router"
        );
        rig.assert_silent("healthy mail");
    }

    #[cfg(unix)]
    const MINTED_A: &str = "11111111-1111-4111-8111-111111111111";
    #[cfg(unix)]
    const MINTED_B: &str = "22222222-2222-4222-8222-222222222222";
    #[cfg(unix)]
    const MINTED_C: &str = "55555555-5555-4555-8555-555555555555";
    #[cfg(unix)]
    const UNRELATED: &str = "33333333-3333-4333-8333-333333333333";
    #[cfg(unix)]
    const LEGACY: &str = "44444444-4444-4444-8444-444444444444";

    #[cfg(unix)]
    fn codex_mint_spec(program: &Path, cwd: &Path, title: &str, token: &str) -> RoomSpec {
        let mut spec = room_spec("codex", &[]);
        spec.title = title.to_owned();
        spec.program = program.as_os_str().to_owned();
        spec.cwd = Some(cwd.to_path_buf());
        spec.native_delivery = true;
        spec.variables = vec![(OsString::from("CROWDED_TEST_MINT"), OsString::from(token))];
        spec
    }

    /// A stand-in codex: `exec` reports the room's own token, and a `resume`
    /// terminal stays alive so the pane is online.
    #[cfg(unix)]
    fn mint_stub(root: &Path, log: &Path, delay: &str) -> PathBuf {
        codex_stub_for_pane(
            root,
            "codex",
            &format!(
                "if [ \"$1\" = resume ]; then exec sleep 30; fi\nprintf '%s\\n' \"$@\" >> '{log}'\n{delay}\nprintf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\"\nprintf '{{\"type\":\"turn.completed\"}}\\n'",
                log = log.display()
            ),
        )
    }

    #[cfg(unix)]
    fn unrelated_rollout(home: &Path, cwd: &Path, id: &str) {
        let day = home.join(".codex/sessions/2026/08/07");
        fs::create_dir_all(&day).unwrap();
        fs::write(
            day.join("rollout-unrelated.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{}\"}}}}\n",
                cwd.display()
            ),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn args_after<'a>(args: &'a [OsString], flag: &str) -> &'a [OsString] {
        let index = args.iter().position(|arg| arg == flag).unwrap();
        &args[index + 1..]
    }

    /// Starts the production mint path and waits for every worker to report.
    #[cfg(unix)]
    fn mint_all(
        specs: &mut [RoomSpec],
        environments: &[GuestEnvironment],
        prompts: &[Option<String>],
    ) -> Vec<Option<CodexMint>> {
        let mut mints = start_codex_mints(specs, environments, prompts);
        let deadline = Instant::now() + Duration::from_secs(9);
        while mints
            .iter()
            .any(|mint| matches!(mint, Some(CodexMint::Pending(_))))
        {
            let _ = poll_codex_mints(&mut mints);
            assert!(Instant::now() < deadline, "a mint never finished");
            thread::sleep(Duration::from_millis(10));
        }
        mints
    }

    #[cfg(unix)]
    fn ready(mint: &Option<CodexMint>) -> Option<&str> {
        match mint {
            Some(CodexMint::Ready(uuid)) => Some(uuid),
            _ => None,
        }
    }

    #[cfg(unix)]
    fn failed(mint: &Option<CodexMint>) -> Option<&str> {
        match mint {
            Some(CodexMint::Failed(reason)) => Some(reason),
            _ => None,
        }
    }

    /// A stand-in codex whose exec waits for a gate, so a relaunch can be
    /// observed while its mint is still running.
    #[cfg(unix)]
    fn gated_codex_stub(root: &Path, log: &Path, gate: &Path, queued_for: &str) -> PathBuf {
        codex_stub_for_pane(
            root,
            "codex",
            &format!(
                "printf '%s\\n' \"$@\" >> '{log}'\n\
                 case \"$1\" in\n\
                 resume) exec sleep 30 ;;\n\
                 queue) printf 'Queued message sub_1 for thread {queued_for}.\\n' ; exit 0 ;;\n\
                 esac\n\
                 while [ ! -e '{gate}' ]; do sleep 0.05; done\n\
                 printf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\"\n\
                 printf '{{\"type\":\"turn.completed\"}}\\n'",
                log = log.display(),
                gate = gate.display(),
            ),
        )
    }

    /// Ctrl+R on an offline native Codex room comes back in its stored thread,
    /// under a new generation, with no second exec.
    #[cfg(unix)]
    #[test]
    fn restarting_an_offline_codex_room_resumes_its_stored_session() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("argv.log");
        let stub = gated_codex_stub(root.path(), &log, &root.path().join("gate"), MINTED_A);
        session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();

        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_offline(&stub, cwd.path(), "Room A", MINTED_B);
        let old = crate::native_launch::GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        rig.bind(
            Some(IterationHarness::healthy_room(&stub, cwd.path(), UNRELATED)),
            old,
            None,
        );

        crate::app::restart_room(
            0,
            small_size(),
            &mut rig.panes,
            rig.router.as_mut(),
            &mut rig.codex,
            &mut rig.gates,
            &mut rig.last_output,
            &mut rig.spawned_at,
            &mut rig.pulses,
            &rig.environments,
            100,
            Instant::now(),
        )
        .unwrap();

        // The terminal came back in the stored thread, not a minted one.
        let args: Vec<String> = rig.panes[0]
            .room_spec()
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.ends_with(&["resume".to_owned(), MINTED_A.to_owned()]),
            "{args:?}"
        );
        assert!(rig.panes[0].is_online());
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(binding.session, MINTED_A);
        assert!(binding.generation > old, "a relaunch gets a new generation");
        // The resumed terminal names the stored thread; no exec ran.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log_text = fs::read_to_string(&log).unwrap_or_default();
            if log_text.lines().any(|line| line == "resume") {
                assert!(!log_text.lines().any(|line| line == "exec"), "{log_text}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the resumed terminal never started: {log_text}"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// `crowded control N clear` mints a new thread, marks the old one cleared
    /// before relaunching, rebinds under a new generation, and never delivers
    /// mail saved for the old thread.
    #[cfg(unix)]
    #[test]
    fn clearing_a_codex_room_mints_a_new_thread_and_holds_old_mail() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("argv.log");
        let gate = root.path().join("gate");
        let stub = gated_codex_stub(root.path(), &log, &gate, MINTED_B);
        session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();

        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_online(&stub, cwd.path(), "Room A", MINTED_B, MINTED_A);
        let old = crate::native_launch::GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        rig.bind(
            Some(IterationHarness::healthy_room(&stub, cwd.path(), MINTED_A)),
            old,
            None,
        );
        // One pass so the router sees the room as running.
        rig.iterate();

        // Delivery stays paused until after the clear, so this mail is saved
        // for the old thread and not sent.
        rig.paused = true;
        let _ = rig.send("saved before the clear");
        rig.iterate();

        rig.push_control(crate::doorbell::ControlAction::ClearContext);
        rig.iterate();

        // The durable cleared marker is written before the mint finishes, so
        // `crowded resume` can no longer restore the old thread.
        assert_eq!(
            session_state::lookup_minted("codex", cwd.path(), "Room A"),
            None
        );
        assert_eq!(
            session_state::lookup("codex", cwd.path(), "Room A").as_deref(),
            Some(session_state::FRESH_STATE_MARKER)
        );
        assert!(!rig.panes[0].is_online(), "the room shows itself starting");

        // Let the mint finish; the room comes back in the new thread.
        rig.paused = false;
        fs::write(&gate, b"go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            rig.iterate();
            let bound = rig
                .router
                .as_ref()
                .and_then(|router| router.binding_for(0))
                .is_some_and(|binding| binding.session == MINTED_B);
            if bound {
                break;
            }
            assert!(Instant::now() < deadline, "the new thread was never bound");
            thread::sleep(Duration::from_millis(20));
        }
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert!(binding.generation > old, "a relaunch gets a new generation");
        assert_eq!(
            session_state::lookup_minted("codex", cwd.path(), "Room A").as_deref(),
            Some(MINTED_B)
        );

        // Exactly one exec created the new thread; the old mail is held.
        let log_text = fs::read_to_string(&log).unwrap();
        assert_eq!(
            log_text.lines().filter(|line| *line == "exec").count(),
            1,
            "{log_text}"
        );
        rig.pump_until("old mail held", |view| {
            view.contains("HELD: sent for an earlier launch")
        });
        assert!(
            !log_text.lines().any(|line| line == "queue"),
            "old mail reached Codex"
        );

        // A reintroduction now reaches the new thread.
        crate::app::reintroduce_native_room(
            "Room A",
            rig.router.as_mut().unwrap(),
            &mut rig.mailroom,
            0,
            "House rules",
        );
        rig.pump_until("reintroduction", |view| view.contains("ACCEPTED by Codex"));
        let log_text = fs::read_to_string(&log).unwrap();
        assert!(
            log_text.lines().any(|line| line == MINTED_B),
            "the reintroduction must name the new thread: {log_text}"
        );
    }

    #[cfg(unix)]
    fn claude_native_spec_for(stub: &Path, cwd: &Path, title: &str) -> RoomSpec {
        let mut spec = RoomSpec {
            name: "claude".to_owned(),
            vendor: "claude".to_owned(),
            title: title.to_owned(),
            program: stub.as_os_str().to_owned(),
            args: Vec::new(),
            transport: Transport::Raw,
            cwd: Some(cwd.to_path_buf()),
            variables: Vec::new(),
            allow_control: true,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
            native_delivery: true,
        };
        spec.args.clear();
        spec
    }

    /// A minted Claude session is never resumed: Crowded must not guess
    /// sessions, so a room with no reported session starts fresh.
    #[cfg(unix)]
    #[test]
    fn a_native_claude_room_never_resumes_a_minted_session() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        session_state::upsert_minted("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        let mut specs = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];

        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Fresh)),
            "{:?}",
            starts[0]
        );
        assert!(
            !specs[0].args.iter().any(|a| a == "--resume"),
            "{:?}",
            specs[0].args
        );
        assert!(
            !specs[0].args.iter().any(|a| a == "--session-id"),
            "{:?}",
            specs[0].args
        );
    }

    /// A reported Claude session is resumed only when its exact session file
    /// exists for this cwd; a missing/unverifiable session starts fresh.
    #[cfg(unix)]
    #[test]
    fn a_reported_claude_session_resumes_only_when_its_session_file_exists() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");

        // Reported id with no session file: never pass --resume/--session-id.
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        let mut specs = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];
        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Fresh)),
            "{:?}",
            starts[0]
        );
        assert!(
            !specs[0].args.iter().any(|a| a == "--resume"),
            "{:?}",
            specs[0].args
        );
        assert!(
            !specs[0].args.iter().any(|a| a == "--session-id"),
            "{:?}",
            specs[0].args
        );
        assert_eq!(
            session_state::lookup_reported("claude", cwd.path(), "Claude · 1"),
            Some(MINTED_A.to_owned()),
            "a check that cannot confirm the file must not erase the reported id"
        );

        // Same reported id with its exact session file present: resume it.
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let mut specs = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];
        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Resume(id)) if id == MINTED_A),
            "{:?}",
            starts[0]
        );
        assert_eq!(
            args_after(&specs[0].args, "--resume").first(),
            Some(&OsString::from(MINTED_A)),
            "{:?}",
            specs[0].args
        );
        assert!(
            !specs[0].args.iter().any(|a| a == "--session-id"),
            "{:?}",
            specs[0].args
        );
    }

    /// The session check reads the config directory of the room's own launch
    /// environment, not only Crowded's.
    #[cfg(unix)]
    #[test]
    fn a_room_own_config_dir_is_checked_for_the_session_file() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        let room_config = tempfile::tempdir().unwrap();

        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(room_config.path(), cwd.path(), MINTED_A);
        let mut spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        spec.variables.push((
            OsString::from("CLAUDE_CONFIG_DIR"),
            room_config.path().as_os_str().to_owned(),
        ));
        let (starts, notices) = start_claude_sessions(std::slice::from_mut(&mut spec));
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Resume(id)) if id == MINTED_A),
            "{:?}",
            starts[0]
        );
    }

    /// Claude encodes a cwd by replacing every character that is not a letter
    /// or digit with `-`; dotted and underscored cwds still resume.
    #[cfg(unix)]
    #[test]
    fn claude_projects_encode_dotted_and_underscored_cwds() {
        const MERIDIAN: &str = "/Users/bruno/.meridian";
        const MERIDIAN_ENCODED: &str = "-Users-bruno--meridian";
        const UNDERSCORED: &str =
            "/Users/bruno/Documents/Development/FVR/05_ai-projects/AI-Workshop/workshop-ai-2026";
        const UNDERSCORED_ENCODED: &str =
            "-Users-bruno-Documents-Development-FVR-05-ai-projects-AI-Workshop-workshop-ai-2026";

        assert_eq!(
            claude_project_dir_name(Path::new(MERIDIAN)),
            MERIDIAN_ENCODED
        );
        assert_eq!(
            claude_project_dir_name(Path::new(UNDERSCORED)),
            UNDERSCORED_ENCODED
        );

        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let base = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");

        for name in ["plainproject", "dot.project", "under_score_project"] {
            let cwd = base.path().join(name);
            fs::create_dir_all(&cwd).unwrap();
            session_state::upsert_reported("claude", &cwd, "Claude · 1", MINTED_A).unwrap();
            materialize_claude_session(_claude_config.path(), &cwd, MINTED_A);
            let mut specs = vec![claude_native_spec_for(&stub, &cwd, "Claude · 1")];
            let (starts, notices) = start_claude_sessions(&mut specs);
            assert!(notices.is_empty(), "{notices:?}");
            assert!(
                matches!(&starts[0], Some(ClaudeStart::Resume(id)) if id == MINTED_A),
                "{name}: {:?}",
                starts[0]
            );
        }

        // A reported id with no session file for its cwd starts fresh.
        let absent = base.path().join("abs.ent");
        fs::create_dir_all(&absent).unwrap();
        session_state::upsert_reported("claude", &absent, "Claude · 1", MINTED_A).unwrap();
        let mut specs = vec![claude_native_spec_for(&stub, &absent, "Claude · 1")];
        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Fresh)),
            "{:?}",
            starts[0]
        );
    }

    /// A fresh Claude room starts without guessing a session and learns it
    /// from the hook.
    #[cfg(unix)]
    #[test]
    fn a_native_claude_start_starts_fresh_and_waits_for_report() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        let mut specs = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];

        let (mut sessions, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            matches!(&sessions[0], Some(ClaudeStart::Fresh)),
            "{:?}",
            sessions[0]
        );
        assert!(session_state::lookup_minted("claude", cwd.path(), "Claude · 1").is_none());
        assert!(session_state::lookup_reported("claude", cwd.path(), "Claude · 1").is_none());
        assert!(
            !specs[0].args.iter().any(|a| a == "--session-id"),
            "{:?}",
            specs[0].args
        );
        assert!(
            !specs[0].args.iter().any(|a| a == "--resume"),
            "{:?}",
            specs[0].args
        );

        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut sessions)
                .unwrap()
                .is_empty()
        );
        assert!(
            crate::claude_inbox::bootstrap_file(cwd.path(), 1)
                .unwrap()
                .exists()
        );
        let binding = router
            .binding_for(0)
            .expect("fresh room is deliverable via bootstrap");
        assert!(
            binding.session.starts_with("unreported-"),
            "fresh room must not be bound to a guessed Claude session: {}",
            binding.session
        );
        assert!(
            session_state::lookup_reported("claude", cwd.path(), "Claude · 1").is_none(),
            "no Claude session is known until reported"
        );

        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let mut again = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];
        let (again_starts, _) = start_claude_sessions(&mut again);
        assert!(
            matches!(&again_starts[0], Some(ClaudeStart::Resume(id)) if id == MINTED_A),
            "{again_starts:?}"
        );
        assert_eq!(
            args_after(&again[0].args, "--resume").first(),
            Some(&OsString::from(MINTED_A))
        );
    }

    /// Two fresh Claude rooms in the same cwd get distinct per-room bootstrap
    /// files and inbox endpoints so deliver resolves each room's own connection.
    /// Rebind promotes a room's bootstrap to its reported session file without
    /// touching the other room.
    #[cfg(unix)]
    #[test]
    fn two_fresh_claude_rooms_in_one_cwd_have_distinct_bootstrap_inboxes() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        let mut specs = vec![
            claude_native_spec_for(&stub, cwd.path(), "Claude · 1"),
            claude_native_spec_for(&stub, cwd.path(), "Claude · 2"),
        ];

        let (mut sessions, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        assert!(
            matches!(&sessions[0], Some(ClaudeStart::Fresh)),
            "{:?}",
            sessions[0]
        );
        assert!(
            matches!(&sessions[1], Some(ClaudeStart::Fresh)),
            "{:?}",
            sessions[1]
        );

        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut sessions)
                .unwrap()
                .is_empty()
        );

        let boot1 = crate::claude_inbox::bootstrap_file(cwd.path(), 1).unwrap();
        let boot2 = crate::claude_inbox::bootstrap_file(cwd.path(), 2).unwrap();
        assert!(boot1.exists(), "room 1 bootstrap must exist");
        assert!(boot2.exists(), "room 2 bootstrap must exist");
        assert_ne!(boot1, boot2, "bootstrap files must be per room");
        assert!(
            !cwd.path()
                .join(".crowded/claude-inbox/_bootstrap.json")
                .exists(),
            "shared bootstrap file must not exist"
        );

        let c1: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&boot1).unwrap()).unwrap();
        let c2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&boot2).unwrap()).unwrap();
        assert_ne!(
            c1["token"], c2["token"],
            "each room must have its own token"
        );
        assert!(c1["url"].as_str().is_some_and(|u| u.starts_with("http://")));
        assert!(c2["url"].as_str().is_some_and(|u| u.starts_with("http://")));
        // Different inbox instances may bind different loopback ports and always
        // have different tokens, so each bootstrap resolves to its own endpoint.
        assert!(
            c1["url"] != c2["url"] || c1["token"] != c2["token"],
            "each room must resolve to its own endpoint"
        );

        // Reporting room 1's session replaces only room 1's bootstrap with its
        // session file; room 2 remains on its own bootstrap.
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        router
            .rebind_claude(0, "Claude · 1".to_owned(), cwd.path(), MINTED_A, 2)
            .unwrap();
        assert!(
            !boot1.exists(),
            "room 1 bootstrap must be removed on report"
        );
        assert!(
            crate::claude_inbox::connection_file(cwd.path(), MINTED_A)
                .unwrap()
                .exists(),
            "room 1 session connection file must exist"
        );
        assert!(boot2.exists(), "room 2 bootstrap must remain untouched");
        let c2_after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&boot2).unwrap()).unwrap();
        assert_eq!(c2, c2_after, "room 2 endpoint must be unchanged");
    }

    /// Every message to a bound Claude room is held, not just the first.
    #[cfg(unix)]
    #[test]
    fn every_message_to_a_claude_room_is_held() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let mut spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let (starts, _) = start_claude_sessions(std::slice::from_mut(&mut spec));
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Resume(id)) if id == MINTED_A),
            "{:?}",
            starts[0]
        );
        let mut panes = vec![
            crate::app::spawn_room(
                &mut crate::app::LivePanes,
                spec,
                small_size(),
                GuestEnvironment::new([]),
                None,
                None,
                None,
            )
            .unwrap(),
        ];
        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        bind_claude_room(&mut router, &mut panes, 0, MINTED_A).unwrap();

        let mut mailroom = Mailroom::new(10);
        let deadline = Instant::now() + Duration::from_secs(20);
        for body in ["first", "second", "third"] {
            let (replied, reply) = std::sync::mpsc::sync_channel(1);
            router
                .send(
                    &mut mailroom,
                    "Sender",
                    0,
                    body,
                    Some(Box::new(move |_, reply| {
                        let _ = replied.send(reply);
                    })),
                )
                .unwrap();
            loop {
                router.tick(Instant::now(), &mut mailroom, false, false);
                if let Ok(outcome) = reply.try_recv() {
                    assert_eq!(
                        outcome,
                        crate::native_route::NativeReply::Held(
                            "no Claude mod connected".to_owned()
                        ),
                        "{body}"
                    );
                    break;
                }
                assert!(Instant::now() < deadline, "{body} was never answered");
                thread::sleep(Duration::from_millis(20));
            }
        }
        let shown = mailroom.summary();
        assert_eq!(shown.matches("HELD:").count(), 3, "{shown}");
        assert!(!shown.contains("QUEUED"), "{shown}");
        drop(router);
        let mut restarted = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        let mut mailroom = Mailroom::new(10);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            restarted.tick(Instant::now(), &mut mailroom, true, false);
            let shown = mailroom.summary();
            if shown.matches("HELD:").count() == 3 && shown.contains("third") {
                break;
            }
            assert!(Instant::now() < deadline, "not durable: {shown}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A relaunch whose new generation cannot be allocated starts no process
    /// and leaves the old pane exactly as it was.
    #[cfg(unix)]
    #[test]
    fn a_claude_relaunch_that_cannot_take_a_generation_starts_nothing() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("claude.log");
        let stub = codex_stub_for_pane(
            root.path(),
            "claude",
            &format!(
                "printf '%s\\n' \"$@\" >> '{}'\\nexec sleep 30",
                log.display()
            ),
        );
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();
        bind_claude_room(rig.router.as_mut().unwrap(), &mut rig.panes, 0, MINTED_A).unwrap();
        let args_before = rig.panes[0].room_spec().args.clone();
        // A corrupt generation counter makes the next allocation fail.
        fs::create_dir_all(session_state::state_directory()).unwrap();
        fs::write(
            session_state::state_directory().join("native-generation"),
            b"not-a-number",
        )
        .unwrap();

        let f5 = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::F(5),
            crossterm::event::KeyModifiers::NONE,
        );
        let outcome = crate::app::restart_key(
            f5,
            small_size(),
            0,
            &mut rig.panes,
            rig.router.as_mut(),
            &mut rig.codex,
            &mut rig.gates,
            &mut rig.last_output,
            &mut rig.spawned_at,
            &mut rig.pulses,
            &rig.environments,
            100,
            Instant::now(),
        );
        assert!(outcome.is_some(), "the failure must be reported");
        assert!(!rig.panes[0].is_online(), "no terminal may start");
        assert_eq!(rig.panes[0].room_spec().args, args_before);
        assert!(!log.exists(), "no process was spawned");
        assert_eq!(
            rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
            MINTED_A,
            "the old binding stays"
        );
        let _ = fs::remove_file(session_state::state_directory().join("native-generation"));
    }

    /// Resume with no stored Claude id writes nothing, not even a marker over
    /// another vendor's entry for the same room.
    #[cfg(unix)]
    #[test]
    fn a_claude_resume_with_no_id_leaves_the_state_file_untouched() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        session_state::upsert_reported("codex", cwd.path(), "Claude · 1", MINTED_B).unwrap();
        let before = fs::read(session_state::state_path()).unwrap();
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();

        rig.push_control(crate::doorbell::ControlAction::Resume);
        rig.iterate();

        let shown = rig.notice.ordered_lines().join("; ");
        assert!(
            shown.contains("no stored Claude session to resume"),
            "{shown}"
        );
        assert_eq!(
            fs::read(session_state::state_path()).unwrap(),
            before,
            "a failed resume must not write anything"
        );
    }

    /// A clear whose terminal cannot start reports the cause and the way out;
    /// the new session may already be recorded or bound, but no terminal is left.
    /// This failure point is a documented residual, not a rollback.
    #[cfg(unix)]
    #[test]
    fn a_clear_that_cannot_spawn_reports_the_cause_and_control_clear() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();
        bind_claude_room(rig.router.as_mut().unwrap(), &mut rig.panes, 0, MINTED_A).unwrap();
        // The executable disappears before the relaunch can start it.
        fs::remove_file(&stub).unwrap();

        rig.push_control(crate::doorbell::ControlAction::ClearContext);
        rig.iterate();

        let shown = rig.notice.ordered_lines().join("; ");
        assert!(shown.contains("use control 1 clear"), "{shown}");
        assert!(
            shown.contains("recorded or bound on a session that did not start"),
            "{shown}"
        );
        assert!(!rig.panes[0].is_online(), "no terminal may be left");
    }

    /// A clear whose router bind fails after the state write and the router maps
    /// have begun changing reports the cause and the way out; the new id may be
    /// recorded and the binding may be the new one, but no terminal is left.
    /// This failure point is a documented residual, not a rollback.
    #[cfg(unix)]
    #[test]
    fn a_clear_whose_bind_fails_after_the_state_write_reports_the_cause_and_control_clear() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();
        bind_claude_room(rig.router.as_mut().unwrap(), &mut rig.panes, 0, MINTED_A).unwrap();
        rig.router.as_ref().unwrap().poison_switching_adapter();

        rig.push_control(crate::doorbell::ControlAction::ClearContext);
        rig.iterate();

        let shown = rig.notice.ordered_lines().join("; ");
        assert!(
            shown.contains("native adapter state is poisoned") || shown.contains("poison"),
            "cause missing: {shown}"
        );
        assert!(shown.contains("use control 1 clear"), "{shown}");
        assert!(
            shown.contains("recorded or bound on a session that did not start"),
            "{shown}"
        );
        assert!(!rig.panes[0].is_online(), "no terminal may be left");
    }

    /// A clear whose generation cannot be allocated fails before anything is
    /// written: no process, the old pane and old id unchanged.
    #[cfg(unix)]
    #[test]
    fn a_clear_that_cannot_take_a_generation_persists_nothing() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("claude.log");
        let stub = codex_stub_for_pane(
            root.path(),
            "claude",
            &format!(
                "printf '%s\\n' \"$@\" >> '{}'\\nexec sleep 30",
                log.display()
            ),
        );
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();
        bind_claude_room(rig.router.as_mut().unwrap(), &mut rig.panes, 0, MINTED_A).unwrap();
        let args_before = rig.panes[0].room_spec().args.clone();
        fs::create_dir_all(session_state::state_directory()).unwrap();
        fs::write(
            session_state::state_directory().join("native-generation"),
            b"not-a-number",
        )
        .unwrap();

        rig.push_control(crate::doorbell::ControlAction::ClearContext);
        rig.iterate();

        assert!(!rig.panes[0].is_online(), "no terminal may start");
        assert_eq!(rig.panes[0].room_spec().args, args_before);
        assert!(!log.exists(), "no process was spawned");
        assert_eq!(
            session_state::lookup_reported("claude", cwd.path(), "Claude · 1").as_deref(),
            Some(MINTED_A),
            "the old id is untouched"
        );
        assert_eq!(
            rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
            MINTED_A
        );
        let _ = fs::remove_file(session_state::state_directory().join("native-generation"));
    }

    /// A native Claude room's session is Crowded's: a user resume form is
    /// refused with a plain reason and nothing is saved.
    #[cfg(unix)]
    #[test]
    fn a_native_claude_room_refuses_a_user_resume_form() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        let mut spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        spec.args = vec!["--resume".into(), MINTED_A.into()];

        let (starts, notices) = start_claude_sessions(std::slice::from_mut(&mut spec));
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Failed(_))),
            "{starts:?}"
        );
        let shown = notices.join("; ");
        assert!(
            shown.contains("session is learned from the room"),
            "{shown}"
        );
        assert_eq!(
            session_state::lookup("claude", cwd.path(), "Claude · 1"),
            None
        );
    }

    /// A Claude room whose preparation fails is not started: the production
    /// decisions reach start_rooms as an explicit failure and no process runs.
    #[cfg(unix)]
    #[test]
    fn a_failed_claude_preparation_starts_no_process() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("claude.log");
        let stub = codex_stub_for_pane(
            root.path(),
            "claude",
            &format!(
                "printf '%s\\n' \"$@\" >> '{}'\nexec sleep 30",
                log.display()
            ),
        );
        let mut spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        spec.args = vec!["--resume".into(), MINTED_A.into()];
        let (starts, notices) = start_claude_sessions(std::slice::from_mut(&mut spec));
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Failed(_))),
            "{starts:?}"
        );
        let mut factory = RecordingPanes::default();
        let mut last_output = vec![None];
        crate::app::start_rooms(
            &mut factory,
            vec![spec],
            &[small_size()],
            &[GuestEnvironment::new([])],
            vec![None],
            &[None],
            &starts,
            &mut last_output,
            false,
            Duration::from_millis(5),
            &mut crate::app::Notices::default(),
        )
        .unwrap();
        assert_eq!(factory.kinds(), ["unavailable"]);
        assert!(!notices.is_empty());
        assert!(!log.exists(), "a process was spawned");
    }

    /// The event loop's own restart arm turns a real F5 key into the relaunch.
    #[cfg(unix)]
    #[test]
    fn the_event_loop_restart_arm_relaunches_a_claude_room() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();
        bind_claude_room(rig.router.as_mut().unwrap(), &mut rig.panes, 0, MINTED_A).unwrap();

        let f5 = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::F(5),
            crossterm::event::KeyModifiers::NONE,
        );
        let (text, _) = crate::app::restart_key(
            f5,
            small_size(),
            0,
            &mut rig.panes,
            rig.router.as_mut(),
            &mut rig.codex,
            &mut rig.gates,
            &mut rig.last_output,
            &mut rig.spawned_at,
            &mut rig.pulses,
            &rig.environments,
            100,
            Instant::now(),
        )
        .expect("F5 must reach the launch");
        assert!(text.ends_with("restarted"), "{text}");
        assert_eq!(
            args_after(&rig.panes[0].room_spec().args, "--resume").first(),
            Some(&OsString::from(MINTED_A))
        );
    }

    /// Restart resumes the reported session, resume without one fails
    /// visibly, and a clear starts fresh without guessing a session.
    #[cfg(unix)]
    #[test]
    fn a_native_claude_room_restarts_resumes_and_clears() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("claude.log");
        let stub = codex_stub_for_pane(
            root.path(),
            "claude",
            &format!(
                "printf 'LAUNCH\\n' >> '{0}'\nprintf '%s\\n' \"$@\" >> '{0}'\nexec sleep 30",
                log.display()
            ),
        );
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");

        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();
        bind_claude_room(rig.router.as_mut().unwrap(), &mut rig.panes, 0, MINTED_A).unwrap();
        let old = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        rig.paused = true;
        let _ = rig.send("saved before the relaunch");
        rig.iterate();

        let f5 = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::F(5),
            crossterm::event::KeyModifiers::NONE,
        );
        let (text, _) = crate::app::restart_key(
            f5,
            small_size(),
            0,
            &mut rig.panes,
            rig.router.as_mut(),
            &mut rig.codex,
            &mut rig.gates,
            &mut rig.last_output,
            &mut rig.spawned_at,
            &mut rig.pulses,
            &rig.environments,
            100,
            Instant::now(),
        )
        .expect("F5 restarts the room");
        assert!(text.ends_with("restarted"), "{text}");
        assert_eq!(
            args_after(&rig.panes[0].room_spec().args, "--resume").first(),
            Some(&OsString::from(MINTED_A)),
            "a restart resumes the reported session"
        );
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert!(binding.generation > old.generation);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !log.exists() {
            rig.iterate();
            assert!(
                Instant::now() < deadline,
                "the fake claude was never launched"
            );
            thread::sleep(Duration::from_millis(20));
        }
        rig.paused = false;
        rig.pump_until("old mail held", |view| {
            view.contains("saved before the relaunch") && view.contains("HELD:")
        });

        // Resume with no reported session names the way out.
        session_state::clear_room("claude", cwd.path(), "Claude · 1").unwrap();
        rig.push_control(crate::doorbell::ControlAction::Resume);
        rig.iterate();
        let shown = rig.notice.ordered_lines().join("; ");
        assert!(
            shown.contains("no stored Claude session to resume"),
            "{shown}"
        );
        assert!(shown.contains("control 1 clear"), "{shown}");

        // A clear starts fresh without guessing a session.
        let before = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == "LAUNCH")
            .count();
        rig.push_control(crate::doorbell::ControlAction::ClearContext);
        rig.iterate();
        assert!(session_state::lookup_reported("claude", cwd.path(), "Claude · 1").is_none());
        assert!(
            !rig.panes[0]
                .room_spec()
                .args
                .iter()
                .any(|a| a == "--session-id" || a == "--resume"),
            "{:?}",
            rig.panes[0].room_spec().args
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| *l == "LAUNCH")
            .count()
            <= before
        {
            rig.iterate();
            assert!(Instant::now() < deadline, "the clear never launched");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The Ctrl+R and F5 key events reach the real dispatch: an offline native
    /// Codex room is revived, while an online one still forwards Ctrl+R.
    #[cfg(unix)]
    #[test]
    fn the_restart_key_events_drive_the_native_relaunch() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("argv.log");
        let stub = gated_codex_stub(root.path(), &log, &root.path().join("gate"), MINTED_B);
        session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();

        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_offline(&stub, cwd.path(), "Room A", MINTED_B);
        let old = crate::native_launch::GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        rig.bind(
            Some(IterationHarness::healthy_room(&stub, cwd.path(), UNRELATED)),
            old,
            None,
        );

        let ctrl_r = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('r'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let (text, severity) = crate::app::restart_key(
            ctrl_r,
            small_size(),
            0,
            &mut rig.panes,
            rig.router.as_mut(),
            &mut rig.codex,
            &mut rig.gates,
            &mut rig.last_output,
            &mut rig.spawned_at,
            &mut rig.pulses,
            &rig.environments,
            100,
            Instant::now(),
        )
        .expect("Ctrl+R on an offline native room must restart it");
        assert!(text.ends_with("restarted"), "{text}");
        assert_eq!(severity, crate::native_route::NoticeSeverity::Info);
        // Same thread, new generation, and no exec.
        let args: Vec<String> = rig.panes[0]
            .room_spec()
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.ends_with(&["resume".to_owned(), MINTED_A.to_owned()]),
            "{args:?}"
        );
        assert!(rig.panes[0].is_online());
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(binding.session, MINTED_A);
        assert!(binding.generation > old);

        // An online pane keeps forwarding Ctrl+R as a literal 0x12 byte.
        let args_before = rig.panes[0].room_spec().args.clone();
        assert!(
            crate::app::restart_key(
                ctrl_r,
                small_size(),
                0,
                &mut rig.panes,
                rig.router.as_mut(),
                &mut rig.codex,
                &mut rig.gates,
                &mut rig.last_output,
                &mut rig.spawned_at,
                &mut rig.pulses,
                &rig.environments,
                100,
                Instant::now(),
            )
            .is_none(),
            "Ctrl+R must not restart an online room"
        );
        assert_eq!(rig.panes[0].room_spec().args, args_before);

        // F5 forces a restart even while the room is online.
        let f5 = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::F(5),
            crossterm::event::KeyModifiers::NONE,
        );
        let (text, _) = crate::app::restart_key(
            f5,
            small_size(),
            0,
            &mut rig.panes,
            rig.router.as_mut(),
            &mut rig.codex,
            &mut rig.gates,
            &mut rig.last_output,
            &mut rig.spawned_at,
            &mut rig.pulses,
            &rig.environments,
            100,
            Instant::now(),
        )
        .expect("F5 must restart an online room");
        assert!(text.ends_with("restarted"), "{text}");
        let log_text = fs::read_to_string(&log).unwrap_or_default();
        assert!(!log_text.lines().any(|line| line == "exec"), "{log_text}");
    }

    /// F5 drives the loop's restart seam for an offline native Claude room:
    /// the room comes back on its native path with a new generation and one exec.
    #[cfg(unix)]
    #[test]
    fn f5_drives_the_loop_restart_seam_for_a_native_claude_room() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("claude.exec");
        let stub = codex_stub_for_pane(
            root.path(),
            "claude",
            &format!(
                "printf 'exec\\n' >> '{}'\nprintf '%s\\n' \"$@\" >> '{}'\nexec sleep 30",
                log.display(),
                log.display()
            ),
        );
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] =
            Pane::spawn_unavailable(spec, small_size(), GuestEnvironment::new([]), "offline")
                .unwrap();
        bind_claude_room(rig.router.as_mut().unwrap(), &mut rig.panes, 0, MINTED_A).unwrap();
        let old = rig
            .router
            .as_ref()
            .unwrap()
            .binding_for(0)
            .unwrap()
            .generation;

        let f5 = crossterm::event::Event::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
        let mut size = |_index: usize| Ok(small_size());
        assert!(!crate::app::run_rooms_key_event(&f5, &mut rig.loop_state(0, &mut size)).unwrap());

        let shown = rig.notice.ordered_lines().join("; ");
        assert!(shown.ends_with("restarted"), "{shown}");
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert!(
            binding.generation > old,
            "the native generation must advance"
        );
        let args: Vec<String> = rig.panes[0]
            .room_spec()
            .args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--resume" && pair[1] == binding.session),
            "{args:?}"
        );
        let launched = || {
            fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "exec")
                .count()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while launched() < 1 {
            assert!(
                Instant::now() < deadline,
                "the fake claude was never relaunched"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(launched(), 1, "one exec per relaunch");
    }

    /// Ctrl+R on an offline native Codex room reaches the same loop seam and
    /// resumes its stored session under a new generation, with no second exec.
    #[cfg(unix)]
    #[test]
    fn offline_ctrl_r_drives_the_loop_restart_seam_for_a_native_codex_room() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("codex.calls");
        let stub = gated_codex_stub(root.path(), &log, &root.path().join("gate"), MINTED_B);
        session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();
        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_offline(&stub, cwd.path(), "Room A", MINTED_B);
        let old = crate::native_launch::GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        rig.bind(
            Some(IterationHarness::healthy_room(&stub, cwd.path(), MINTED_B)),
            old,
            None,
        );

        let ctrl_r =
            crossterm::event::Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let mut size = |_index: usize| Ok(small_size());
        assert!(
            !crate::app::run_rooms_key_event(&ctrl_r, &mut rig.loop_state(0, &mut size)).unwrap()
        );

        let shown = rig.notice.ordered_lines().join("; ");
        assert!(shown.ends_with("restarted"), "{shown}");
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(binding.session, MINTED_A);
        assert!(binding.generation > old);
        let resumed = || {
            fs::read_to_string(&log)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with("resume"))
                .count()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while resumed() < 1 {
            assert!(Instant::now() < deadline, "the fake codex never resumed");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(resumed(), 1, "one resume per relaunch, no second exec");
        let log_text = fs::read_to_string(&log).unwrap_or_default();
        assert!(!log_text.lines().any(|line| line.starts_with("exec")));
    }

    /// With two native Claude rooms and focus on room 1, F5 relaunches only
    /// room 1: a wrong focused index or a key sent to the wrong pane is red.
    #[cfg(unix)]
    #[test]
    fn f5_at_focus_one_relaunches_only_the_claude_room() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub_root0 = tempfile::tempdir().unwrap();
        let stub_root1 = tempfile::tempdir().unwrap();
        let log0 = stub_root0.path().join("claude.log");
        let log1 = stub_root1.path().join("claude.log");
        let stub0 = codex_stub_for_pane(
            stub_root0.path(),
            "claude",
            &format!("printf 'exec\\n' >> '{}'\nexec sleep 30", log0.display()),
        );
        let stub1 = codex_stub_for_pane(
            stub_root1.path(),
            "claude",
            &format!("printf 'exec\\n' >> '{}'\nexec sleep 30", log1.display()),
        );
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        session_state::upsert_reported("claude", cwd.path(), "Claude · 2", MINTED_B).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_B);
        let mut rig = IterationHarness::new(root.path());
        rig.panes = vec![
            Pane::spawn_unavailable(
                claude_native_spec_for(&stub0, cwd.path(), "Claude · 1"),
                small_size(),
                GuestEnvironment::new([]),
                "offline",
            )
            .unwrap(),
            Pane::spawn_unavailable(
                claude_native_spec_for(&stub1, cwd.path(), "Claude · 2"),
                small_size(),
                GuestEnvironment::new([]),
                "offline",
            )
            .unwrap(),
        ];
        rig.gates = vec![crate::app::DeliveryGate::new(false); 2];
        rig.last_output = vec![None; 2];
        rig.spawned_at = vec![Instant::now(); 2];
        rig.pulses = vec![None; 2];
        rig.details = vec![crate::room_detail::RoomDetail::default(); 2];
        rig.codex = vec![None, None];
        rig.environments = vec![GuestEnvironment::new([]), GuestEnvironment::new([])];
        {
            let router = rig.router.as_mut().unwrap();
            bind_claude_room(router, &mut rig.panes, 0, MINTED_A).unwrap();
            bind_claude_room(router, &mut rig.panes, 1, MINTED_B).unwrap();
        }
        let gen0 = rig
            .router
            .as_ref()
            .unwrap()
            .binding_for(0)
            .unwrap()
            .generation;
        let gen1 = rig
            .router
            .as_ref()
            .unwrap()
            .binding_for(1)
            .unwrap()
            .generation;

        let f5 = crossterm::event::Event::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE));
        let requested = std::cell::Cell::new(usize::MAX);
        let mut size = |index: usize| {
            requested.set(index);
            Ok(small_size())
        };
        assert!(!crate::app::run_rooms_key_event(&f5, &mut rig.loop_state(1, &mut size)).unwrap());
        assert_eq!(requested.get(), 1, "the step sizes the focused pane");

        let shown = rig.notice.ordered_lines().join("; ");
        assert!(shown.contains("Claude · 2 restarted"), "{shown}");
        assert!(!shown.contains("Claude · 1 restarted"), "{shown}");
        assert_eq!(
            rig.router
                .as_ref()
                .unwrap()
                .binding_for(0)
                .unwrap()
                .generation,
            gen0
        );
        assert!(
            rig.router
                .as_ref()
                .unwrap()
                .binding_for(1)
                .unwrap()
                .generation
                > gen1
        );
        let launched = |path: &Path| {
            fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter(|line| *line == "exec")
                .count()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while launched(&log1) < 1 {
            assert!(Instant::now() < deadline, "room 1 was never relaunched");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(launched(&log1), 1);
        assert_eq!(launched(&log0), 0, "room 0 must stay untouched");
    }

    /// The same two-room focus check for an offline native Codex room: only
    /// room 1 resumes, under a new generation, with room 0 unchanged.
    #[cfg(unix)]
    #[test]
    fn ctrl_r_at_focus_one_relaunches_only_the_codex_room() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub_root0 = tempfile::tempdir().unwrap();
        let stub_root1 = tempfile::tempdir().unwrap();
        let log0 = stub_root0.path().join("codex.calls");
        let log1 = stub_root1.path().join("codex.calls");
        let stub0 = gated_codex_stub(
            stub_root0.path(),
            &log0,
            &stub_root0.path().join("gate"),
            MINTED_B,
        );
        let stub1 = gated_codex_stub(
            stub_root1.path(),
            &log1,
            &stub_root1.path().join("gate"),
            MINTED_C,
        );
        session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();
        let mut rig = IterationHarness::new(root.path());
        rig.panes[0] = Pane::spawn_unavailable(
            codex_mint_spec(&stub0, cwd.path(), "Room A", MINTED_B),
            small_size(),
            GuestEnvironment::new([]),
            "offline",
        )
        .unwrap();
        rig.panes.push(
            Pane::spawn_unavailable(
                codex_mint_spec(&stub1, cwd.path(), "Room B", MINTED_C),
                small_size(),
                GuestEnvironment::new([]),
                "offline",
            )
            .unwrap(),
        );
        rig.gates.push(crate::app::DeliveryGate::new(false));
        rig.last_output.push(None);
        rig.spawned_at.push(Instant::now());
        rig.pulses.push(None);
        rig.details.push(crate::room_detail::RoomDetail::default());
        rig.codex.push(None);
        rig.environments.push(GuestEnvironment::new([]));
        let gen0 = GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        let gen1 = gen0 + 1;
        rig.router_mut()
            .bind_codex(vec![
                crate::native_route::CodexEntry {
                    index: 0,
                    title: "Room A".into(),
                    room: Some(IterationHarness::healthy_room(&stub0, cwd.path(), MINTED_A)),
                    generation: gen0,
                    reason: None,
                },
                crate::native_route::CodexEntry {
                    index: 1,
                    title: "Room B".into(),
                    room: Some(IterationHarness::healthy_room(&stub1, cwd.path(), MINTED_B)),
                    generation: gen1,
                    reason: None,
                },
            ])
            .unwrap();

        let ctrl_r =
            crossterm::event::Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let requested = std::cell::Cell::new(usize::MAX);
        let mut size = |index: usize| {
            requested.set(index);
            Ok(small_size())
        };
        assert!(
            !crate::app::run_rooms_key_event(&ctrl_r, &mut rig.loop_state(1, &mut size)).unwrap()
        );
        assert_eq!(requested.get(), 1, "the step sizes the focused pane");
        let shown = rig.notice.ordered_lines().join("; ");
        assert!(shown.contains("Room B restarted"), "{shown}");
        assert!(!shown.contains("Room A restarted"), "{shown}");
        assert_eq!(
            rig.router
                .as_ref()
                .unwrap()
                .binding_for(0)
                .unwrap()
                .generation,
            gen0
        );
        let launched = |path: &Path| {
            fs::read_to_string(path)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with("exec"))
                .count()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while launched(&log1) < 1 {
            assert!(Instant::now() < deadline, "room 1 was never relaunched");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(launched(&log1), 1);
        assert_eq!(launched(&log0), 0, "room 0 must stay untouched");
        let prompt = fs::read_to_string(&log1).unwrap_or_default();
        assert!(
            prompt.contains("Automatic delivery pauses after 100 successful messages"),
            "the seam must pass the loop's fuse size: {prompt}"
        );
    }

    /// An ordinary key is not consumed by the step: it reaches the focused
    /// pane's input handling, while the other pane sees nothing (INV-5).
    #[cfg(unix)]
    #[test]
    fn an_ordinary_key_is_forwarded_to_the_focused_pane_only() {
        let (pane0, captured0) = pane_with_capture(4, 20);
        let (pane1, captured1) = pane_with_capture(4, 20);
        let mut panes = vec![pane0, pane1];
        let mut codex = [None, None];
        let mut gates = [
            crate::app::DeliveryGate::new(false),
            crate::app::DeliveryGate::new(false),
        ];
        let mut last_output = [None, None];
        let mut spawned_at = [Instant::now(), Instant::now()];
        let mut pulses = [None, None];
        let environments = [GuestEnvironment::new([]), GuestEnvironment::new([])];
        let mut notice = crate::app::Notices::default();
        let mut size = |_index: usize| Ok(small_size());
        let key = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        let event = crossterm::event::Event::Key(key);
        let mut state = crate::app::loop_state(
            1,
            100,
            &mut size,
            &mut panes,
            None,
            &mut codex,
            &mut gates,
            &mut last_output,
            &mut spawned_at,
            &mut pulses,
            &environments,
            &mut notice,
        );
        assert!(crate::app::run_rooms_key_event(&event, &mut state).unwrap());
        assert!(notice.ordered_lines().is_empty());
        assert!(crate::app::forward_key(&mut panes[1], key).is_none());
        wait_for_captured(&captured1, b"a");
        assert!(
            captured0.lock().unwrap().is_empty(),
            "the other pane must see nothing"
        );
    }

    /// The seam leaves an online Ctrl+R alone and the literal 0x12 byte still
    /// reaches the child, so reverse search keeps working (INV-5).
    #[cfg(unix)]
    #[test]
    fn an_online_ctrl_r_is_not_a_restart_and_forwards_0x12() {
        let (mut pane, captured) = pane_with_capture(4, 20);
        assert!(pane.is_online());
        let ctrl_r =
            crossterm::event::Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        let mut notice = crate::app::Notices::default();
        let mut size = |_index: usize| Ok(small_size());
        let mut codex = [None];
        let mut gates = [crate::app::DeliveryGate::new(false)];
        let mut last_output = [None];
        let mut spawned_at = [Instant::now()];
        let mut pulses = [None];
        let environments = [GuestEnvironment::new([])];
        let mut context = crate::app::loop_state(
            0,
            100,
            &mut size,
            std::slice::from_mut(&mut pane),
            None,
            &mut codex,
            &mut gates,
            &mut last_output,
            &mut spawned_at,
            &mut pulses,
            &environments,
            &mut notice,
        );
        assert!(crate::app::run_rooms_key_event(&ctrl_r, &mut context).unwrap());
        assert!(notice.ordered_lines().is_empty());
        let key = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert!(crate::app::forward_key(&mut pane, key).is_none());
        wait_for_captured(&captured, &[0x12]);
    }

    /// A clear whose durable marker cannot be saved must not start a mint or
    /// change the room: the previous session stays the one a resume would use.
    #[cfg(unix)]
    #[test]
    fn a_clear_whose_marker_cannot_be_saved_changes_nothing() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("argv.log");
        let stub = gated_codex_stub(root.path(), &log, &root.path().join("gate"), MINTED_B);
        session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();
        // A leftover temp file makes the atomic save fail where its rename
        // would otherwise land.
        fs::create_dir_all(session_state::state_directory()).unwrap();
        fs::write(
            session_state::state_directory()
                .join(format!("session-state.{}.tmp", std::process::id())),
            b"blocked",
        )
        .unwrap();

        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_offline(&stub, cwd.path(), "Room A", MINTED_B);
        let old = crate::native_launch::GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        rig.bind(
            Some(IterationHarness::healthy_room(&stub, cwd.path(), MINTED_A)),
            old,
            None,
        );
        let args_before = rig.panes[0].room_spec().args.clone();

        rig.push_control(crate::doorbell::ControlAction::ClearContext);
        rig.iterate();
        // Give a mint that wrongly started time to write its arguments.
        thread::sleep(Duration::from_millis(150));

        let text = rig.notice.ordered_lines().join("; ");
        assert!(
            text.contains("the cleared marker could not be saved"),
            "{text}"
        );
        // Nothing else moved: same stored session, same pane, same binding.
        assert_eq!(
            session_state::lookup_minted("codex", cwd.path(), "Room A").as_deref(),
            Some(MINTED_A)
        );
        assert!(!log.exists(), "no mint may start");
        assert_eq!(rig.panes[0].room_spec().args, args_before);
        assert!(!rig.panes[0].is_online());
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(
            (binding.session.as_str(), binding.generation),
            (MINTED_A, old)
        );
    }

    /// Two clears on one room: whichever mint finishes last, the durable
    /// session and the router must both name the newer thread.
    #[cfg(unix)]
    #[test]
    fn a_superseded_clear_mint_never_overwrites_the_newer_session() {
        for older_finishes_first in [true, false] {
            let _state = session_state::StateRootGuard::isolated();
            let cwd = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let log = root.path().join("argv.log");
            let gate_a = root.path().join("gate-a");
            let gate_b = root.path().join("gate-b");
            let stub = codex_stub_for_pane(
                root.path(),
                "codex",
                &format!(
                    "printf '%s\\n' \"$@\" >> '{log}'\n\
                     case \"$1\" in\n\
                     resume) exec sleep 30 ;;\n\
                     esac\n\
                     case \"$CROWDED_TEST_MINT\" in\n\
                     '{MINTED_A}') gate='{gate_a}' ;;\n\
                     '{MINTED_B}') gate='{gate_b}' ;;\n\
                     *) gate='' ;;\n\
                     esac\n\
                     while [ -n \"$gate\" ] && [ ! -e \"$gate\" ]; do sleep 0.05; done\n\
                     printf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\"",
                    log = log.display(),
                    gate_a = gate_a.display(),
                    gate_b = gate_b.display(),
                ),
            );
            session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_C).unwrap();

            let mut rig = IterationHarness::new(root.path());
            rig.codex_pane_online(&stub, cwd.path(), "Room A", MINTED_A, MINTED_C);
            let old =
                crate::native_launch::GenerationAllocator::new(session_state::state_directory())
                    .next()
                    .unwrap();
            rig.bind(
                Some(IterationHarness::healthy_room(&stub, cwd.path(), MINTED_C)),
                old,
                None,
            );
            rig.iterate();

            rig.push_control(crate::doorbell::ControlAction::ClearContext);
            rig.iterate();
            // The second clear starts the newer mint in the same room.
            rig.panes[0].spec.variables = vec![(
                OsString::from("CROWDED_TEST_MINT"),
                OsString::from(MINTED_B),
            )];
            rig.push_control(crate::doorbell::ControlAction::ClearContext);
            rig.iterate();

            if older_finishes_first {
                fs::write(&gate_a, b"go").unwrap();
                for _ in 0..20 {
                    rig.iterate();
                    thread::sleep(Duration::from_millis(5));
                }
            }
            fs::write(&gate_b, b"go").unwrap();

            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                rig.iterate();
                let bound = rig
                    .router
                    .as_ref()
                    .and_then(|router| router.binding_for(0))
                    .is_some_and(|binding| binding.session == MINTED_B);
                if bound {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the newer thread was never bound"
                );
                thread::sleep(Duration::from_millis(20));
            }

            // The superseded mint finishes after the newer one is bound, in
            // either order; it must never become the durable session.
            fs::write(&gate_a, b"go").unwrap();
            thread::sleep(Duration::from_millis(150));
            for _ in 0..5 {
                rig.iterate();
            }

            assert_eq!(
                session_state::lookup_minted("codex", cwd.path(), "Room A").as_deref(),
                Some(MINTED_B),
                "durable state must follow the newer clear (older first: {older_finishes_first})"
            );
            assert_eq!(
                rig.router.as_ref().unwrap().binding_for(0).unwrap().session,
                MINTED_B
            );
        }
    }

    /// A clear whose mint fails leaves the room offline, unbound and named,
    /// never half-bound to the thread it could not create.
    #[cfg(unix)]
    #[test]
    fn a_failed_clear_mint_leaves_the_room_offline_with_its_reason() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(
            root.path(),
            "codex",
            "if [ \"$1\" = resume ]; then exec sleep 30; fi\n\
             printf 'Error: mint failed\\n' >&2\nexit 1",
        );
        session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();

        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_online(&stub, cwd.path(), "Room A", MINTED_B, MINTED_A);
        let old = crate::native_launch::GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        rig.bind(
            Some(IterationHarness::healthy_room(&stub, cwd.path(), MINTED_A)),
            old,
            None,
        );
        rig.iterate();

        rig.push_control(crate::doorbell::ControlAction::ClearContext);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            rig.iterate();
            let view = rig.notice.ordered_lines().join("; ");
            if view.contains("the room was not started") {
                assert!(view.contains("mint failed"), "{view}");
                break;
            }
            assert!(Instant::now() < deadline, "the failed mint was never shown");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!rig.panes[0].is_online());
        assert!(
            rig.router.as_ref().unwrap().binding_for(0).is_none(),
            "a failed relaunch must not keep a binding"
        );
        assert_eq!(
            session_state::lookup("codex", cwd.path(), "Room A").as_deref(),
            Some(session_state::FRESH_STATE_MARKER)
        );
    }

    /// Resume means the stored session; with none, the room stays as it was
    /// and the error names the missing session.
    #[cfg(unix)]
    #[test]
    fn resuming_a_codex_room_without_a_stored_session_names_the_problem() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("argv.log");
        let stub = gated_codex_stub(root.path(), &log, &root.path().join("gate"), MINTED_A);

        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_offline(&stub, cwd.path(), "Room A", MINTED_A);
        // A sibling entry makes the durable state file exist, so a write by the
        // refused resume would show up as changed bytes.
        session_state::upsert_minted("codex", cwd.path(), "Sibling", MINTED_B).unwrap();
        let state_before = fs::read(session_state::state_path()).unwrap();
        let old = crate::native_launch::GenerationAllocator::new(session_state::state_directory())
            .next()
            .unwrap();
        rig.bind(
            Some(IterationHarness::healthy_room(&stub, cwd.path(), MINTED_B)),
            old,
            None,
        );
        let args_before = rig.panes[0].room_spec().args.clone();

        rig.push_control(crate::doorbell::ControlAction::Resume);
        rig.iterate();

        let text = rig.notice.ordered_lines().join("; ");
        assert!(
            text.contains("no stored Codex session to resume; use control 1 clear"),
            "{text}"
        );
        // Nothing was launched or changed: same pane, same binding, same bytes.
        assert!(!rig.panes[0].is_online());
        assert_eq!(rig.panes[0].room_spec().args, args_before);
        assert!(!log.exists());
        let binding = rig.router.as_ref().unwrap().binding_for(0).unwrap();
        assert_eq!(
            (binding.session.as_str(), binding.generation),
            (MINTED_B, old)
        );
        assert_eq!(fs::read(session_state::state_path()).unwrap(), state_before);
        assert_eq!(session_state::lookup("codex", cwd.path(), "Room A"), None);
    }

    /// A Codex room still refuses model and effort changes; the refusal comes
    /// before any state is written.
    #[cfg(unix)]
    #[test]
    fn a_codex_room_still_refuses_model_and_effort_changes() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = gated_codex_stub(
            root.path(),
            &root.path().join("argv.log"),
            &root.path().join("gate"),
            MINTED_A,
        );

        let mut rig = IterationHarness::new(root.path());
        rig.codex_pane_offline(&stub, cwd.path(), "Room A", MINTED_A);

        rig.push_control(crate::doorbell::ControlAction::Configure {
            model: Some("provider/model".to_owned()),
            effort: None,
        });
        rig.iterate();

        let text = rig.notice.ordered_lines().join("; ");
        assert!(
            text.contains(
                "native delivery rooms cannot be restarted, cleared, resumed or reconfigured yet"
            ),
            "{text}"
        );
        assert_eq!(session_state::lookup("codex", cwd.path(), "Room A"), None);
    }

    #[cfg(unix)]
    #[test]
    fn two_codex_rooms_in_one_directory_mint_and_resume_only_their_own_session() {
        let cwd = tempfile::tempdir().unwrap();
        // Both room orders, with an unrelated rollout already in the directory.
        for order in [[MINTED_A, MINTED_B], [MINTED_B, MINTED_A]] {
            let _state = session_state::StateRootGuard::isolated();
            let home = controls::HomeDirGuard::isolated();
            unrelated_rollout(home.path(), cwd.path(), UNRELATED);
            let root = tempfile::tempdir().unwrap();
            let log = root.path().join("exec.log");
            let stub = mint_stub(root.path(), &log, "sleep 0.2");
            let mut specs: Vec<RoomSpec> = order
                .iter()
                .enumerate()
                .map(|(index, token)| {
                    codex_mint_spec(&stub, cwd.path(), &format!("Codex · {}", index + 1), token)
                })
                .collect();
            let environments = vec![GuestEnvironment::new([]), GuestEnvironment::new([])];
            let prompts = vec![
                Some("House rules".to_owned()),
                Some("House rules".to_owned()),
            ];
            let mints = mint_all(&mut specs, &environments, &prompts);

            for (index, token) in order.iter().enumerate() {
                assert_eq!(ready(&mints[index]), Some(*token), "room {index}");
                assert_eq!(
                    session_state::lookup_minted("codex", cwd.path(), &specs[index].title)
                        .as_deref(),
                    Some(*token)
                );
                // The terminal phase resumes exactly the minted session.
                resume_session(&mut specs[index], token).unwrap();
                assert_eq!(
                    args_after(&specs[index].args, "resume"),
                    [OsString::from(*token)]
                );
            }
            // One exec per room; the unrelated rollout is never adopted.
            let execs = fs::read_to_string(&log)
                .unwrap()
                .lines()
                .filter(|line| *line == "exec")
                .count();
            assert_eq!(execs, 2, "one exec per room");
            for mint in mints.iter().flatten() {
                if let CodexMint::Ready(uuid) = mint {
                    assert_ne!(uuid, UNRELATED);
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_legacy_generic_mapping_is_not_trusted_and_the_room_mints_a_new_session() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("exec-ran");
        let stub = codex_stub_for_pane(
            root.path(),
            "codex",
            &format!(
                "if [ \"$1\" = resume ]; then exec sleep 30; fi\ntouch '{marker}'\nprintf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\"",
                marker = marker.display()
            ),
        );
        // An entry written by the old generic capture, with no provenance.
        session_state::upsert("codex", cwd.path(), "Codex · 1", LEGACY);
        assert_eq!(
            session_state::lookup_minted("codex", cwd.path(), "Codex · 1"),
            None
        );
        let mut specs = vec![codex_mint_spec(&stub, cwd.path(), "Codex · 1", MINTED_A)];
        let prompts = vec![Some("House rules".to_owned())];
        let mints = mint_all(&mut specs, &[GuestEnvironment::new([])], &prompts);
        assert_eq!(ready(&mints[0]), Some(MINTED_A));
        assert!(marker.exists(), "a legacy entry must not skip the exec");
        assert_eq!(
            session_state::lookup_minted("codex", cwd.path(), "Codex · 1").as_deref(),
            Some(MINTED_A)
        );
        resume_session(&mut specs[0], MINTED_A).unwrap();
        assert_eq!(
            args_after(&specs[0].args, "resume"),
            [OsString::from(MINTED_A)]
        );
        assert_ne!(LEGACY, MINTED_A);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_exec_leaves_the_room_not_started_and_unbound() {
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let failing = codex_stub_for_pane(
            root.path(),
            "codex",
            "printf 'warning: noise\\nError: model unavailable\\n' >&2\nexit 1",
        );
        let mut specs = vec![codex_mint_spec(&failing, cwd.path(), "Codex · 1", MINTED_A)];
        let prompts = vec![Some("House rules".to_owned())];
        let mints = mint_all(&mut specs, &[GuestEnvironment::new([])], &prompts);
        let reason = failed(&mints[0])
            .expect("the room must not be started")
            .to_owned();
        assert!(reason.contains("model unavailable"), "{reason}");
        assert_eq!(
            session_state::lookup_minted("codex", cwd.path(), "Codex · 1"),
            None
        );
        assert!(!specs[0].args.iter().any(|arg| arg == "resume"));

        // The production startup decision starts no process for this room.
        let mut pane = crate::app::spawn_room(
            &mut crate::app::LivePanes,
            specs.remove(0),
            PtySize {
                rows: 6,
                cols: 60,
                pixel_width: 0,
                pixel_height: 0,
            },
            GuestEnvironment::new([]),
            None,
            mints[0].as_ref(),
            None,
        )
        .unwrap();
        assert!(!pane.is_online() && pane.is_native() && !pane.needs_intro());
        assert!(pane.screen().contents().contains("model unavailable"));
        assert!(pane.send_whisper("Sender", "hello").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_restart_resumes_the_minted_session_without_another_exec() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("exec-ran");
        let stub = codex_stub_for_pane(
            root.path(),
            "codex",
            &format!(
                "if [ \"$1\" = resume ]; then exec sleep 30; fi\ntouch '{marker}'\nprintf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\"",
                marker = marker.display()
            ),
        );
        session_state::upsert_minted("codex", cwd.path(), "Codex · 1", MINTED_A).unwrap();
        let mut specs = vec![codex_mint_spec(&stub, cwd.path(), "Codex · 1", MINTED_B)];
        let prompts = vec![Some("House rules".to_owned())];
        let mints = start_codex_mints(&mut specs, &[GuestEnvironment::new([])], &prompts);
        assert_eq!(ready(&mints[0]), Some(MINTED_A));
        assert!(!marker.exists(), "a restart must not run exec again");
        assert_eq!(
            args_after(&specs[0].args, "resume"),
            [OsString::from(MINTED_A)]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_restart_resumes_the_reported_session_without_another_exec() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("exec-ran");
        let stub = codex_stub_for_pane(
            root.path(),
            "codex",
            &format!(
                "if [ \"$1\" = resume ]; then exec sleep 30; fi\ntouch '{marker}'\nprintf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\"",
                marker = marker.display()
            ),
        );
        session_state::upsert_reported(
            "codex",
            cwd.path(),
            "Codex · 1",
            "33333333-3333-4333-8333-333333333333",
        )
        .unwrap();
        let mut specs = vec![codex_mint_spec(&stub, cwd.path(), "Codex · 1", MINTED_B)];
        let prompts = vec![Some("House rules".to_owned())];
        let mints = start_codex_mints(&mut specs, &[GuestEnvironment::new([])], &prompts);
        assert_eq!(
            ready(&mints[0]),
            Some("33333333-3333-4333-8333-333333333333")
        );
        assert!(!marker.exists(), "a restart must not run exec again");
        assert_eq!(
            args_after(&specs[0].args, "resume"),
            [OsString::from("33333333-3333-4333-8333-333333333333")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_mint_command_keeps_interactive_options_out_of_exec_and_on_resume() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("exec.log");
        // Rejects any interactive-only option after exec, like the real CLI.
        let stub = codex_stub_for_pane(
            root.path(),
            "codex",
            &format!(
                "for a in \"$@\"; do case \"$a\" in --search|--ask-for-approval|--no-alt-screen|-a) echo \"unexpected argument $a\" >&2; exit 2;; esac; done\nprintf '%s\\n' \"$@\" >> '{log}'\nprintf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\"",
                log = log.display()
            ),
        );
        let mut specs = vec![codex_mint_spec(&stub, cwd.path(), "Codex · 1", MINTED_A)];
        specs[0].args = vec![
            "--search".into(),
            "--ask-for-approval".into(),
            "never".into(),
            "--no-alt-screen".into(),
            "-c".into(),
            "model_reasoning_effort=\"high\"".into(),
            "-m".into(),
            "gpt-5".into(),
            "-s".into(),
            "workspace-write".into(),
        ];
        let prompts = vec![Some("House rules".to_owned())];
        let mints = mint_all(&mut specs, &[GuestEnvironment::new([])], &prompts);
        assert_eq!(ready(&mints[0]), Some(MINTED_A), "{:?}", failed(&mints[0]));
        // Exec received the accepted options only.
        let executed = fs::read_to_string(&log).unwrap();
        for kept in [
            "-c",
            "model_reasoning_effort",
            "-m",
            "gpt-5",
            "-s",
            "workspace-write",
        ] {
            assert!(
                executed.contains(kept),
                "{kept} missing from exec: {executed}"
            );
        }
        for dropped in ["--search", "--ask-for-approval", "--no-alt-screen", "never"] {
            assert!(
                !executed.lines().any(|line| line == dropped),
                "{dropped} reached exec"
            );
        }
        // The resumed terminal keeps every interactive option.
        resume_session(&mut specs[0], MINTED_A).unwrap();
        assert_eq!(
            args_after(&specs[0].args, "resume"),
            [OsString::from(MINTED_A)]
        );
        for kept in [
            "--search",
            "--ask-for-approval",
            "never",
            "--no-alt-screen",
            "-c",
            "-m",
            "-s",
        ] {
            assert!(
                specs[0].args.iter().any(|arg| arg == kept),
                "{kept} missing from resume"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_mint_that_cannot_be_saved_leaves_the_room_not_started() {
        let state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(
            root.path(),
            "codex",
            &format!(
                "if [ \"$1\" = resume ]; then exec sleep 30; fi\nprintf '{{\"type\":\"thread.started\",\"thread_id\":\"%s\"}}\\n' \"$CROWDED_TEST_MINT\""
            ),
        );
        // The state directory cannot be created, so the minted session cannot
        // be stored.
        fs::write(state.path().join(".crowded"), b"in the way").unwrap();
        let mut specs = vec![codex_mint_spec(&stub, cwd.path(), "Codex · 1", MINTED_A)];
        let prompts = vec![Some("House rules".to_owned())];
        let mints = mint_all(&mut specs, &[GuestEnvironment::new([])], &prompts);
        drop(state);
        let reason = failed(&mints[0])
            .expect("an unsaved session must not be ready")
            .to_owned();
        assert!(reason.contains(MINTED_A), "{reason}");
        assert!(reason.contains("could not be saved"), "{reason}");
        assert_eq!(
            session_state::lookup_minted("codex", cwd.path(), "Codex · 1"),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_mint_does_not_delay_the_rooms_whose_mints_finished() {
        // The mints store their sessions under the process-wide state root, which
        // other tests replace or break; holding the guard keeps this test on its
        // own directory.
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = mint_stub(root.path(), &root.path().join("exec.log"), "sleep 30");
        let fast_a = codex_mint_spec(&stub, cwd.path(), "Codex · 1", MINTED_A);
        let fast_b = codex_mint_spec(&stub, cwd.path(), "Codex · 2", MINTED_B);
        let hung = codex_mint_spec(&stub, cwd.path(), "Codex · 3", MINTED_C);
        // Rooms 1 and 2 finish at once; room 3's stub hangs.
        let mut specs = vec![fast_a, fast_b, hung];
        for (index, spec) in specs.iter_mut().enumerate() {
            if index < 2 {
                spec.variables
                    .push((OsString::from("CROWDED_TEST_DELAY"), OsString::from("0")));
            }
        }
        // The stub sleeps only when asked; the hung room asks for a long wait
        // through its own delay variable.
        specs[2]
            .variables
            .push((OsString::from("CROWDED_TEST_DELAY"), OsString::from("30")));
        let stub_path = stub.clone();
        let script = fs::read_to_string(&stub_path).unwrap();
        fs::write(
            &stub_path,
            script.replace("sleep 30", "sleep \"${CROWDED_TEST_DELAY:-0}\"\nif [ \"$1\" = resume ]; then exec sleep 30; fi"),
        )
        .unwrap();

        let environments = vec![
            GuestEnvironment::new([]),
            GuestEnvironment::new([]),
            GuestEnvironment::new([]),
        ];
        let prompts = vec![
            Some("House rules".to_owned()),
            Some("House rules".to_owned()),
            Some("House rules".to_owned()),
        ];
        let started = Instant::now();
        let mut mints = start_codex_mints(&mut specs, &environments, &prompts);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "minting blocked startup"
        );
        let deadline = Instant::now() + Duration::from_secs(8);
        while ready(&mints[0]).is_none() || ready(&mints[1]).is_none() {
            let _ = poll_codex_mints(&mut mints);
            assert!(
                Instant::now() < deadline,
                "fast rooms waited for the hung one: {:?} {:?}",
                failed(&mints[0]),
                failed(&mints[1])
            );
            thread::sleep(Duration::from_millis(10));
        }
        assert!(matches!(mints[2], Some(CodexMint::Pending(_))));
        // The finished rooms can start now; the pending room is held.
        for index in 0..2 {
            let pane = crate::app::spawn_room(
                &mut crate::app::LivePanes,
                specs[index].clone(),
                PtySize {
                    rows: 4,
                    cols: 20,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                GuestEnvironment::new([]),
                None,
                mints[index].as_ref(),
                None,
            )
            .unwrap();
            assert!(pane.is_online());
        }
    }

    /// Records which kind of pane the production start code asks for, and hands
    /// back a pane that has no process.
    #[cfg(unix)]
    #[derive(Default)]
    struct RecordingPanes {
        calls: Vec<(&'static str, String, Vec<OsString>)>,
    }

    #[cfg(unix)]
    impl RecordingPanes {
        fn record(&mut self, kind: &'static str, spec: RoomSpec) -> Pane {
            self.calls
                .push((kind, spec.title.clone(), spec.args.clone()));
            let (mut pane, _) = pane_with_capture(4, 20);
            pane.spec = spec;
            // Only an interactive pane has a process, as in production.
            if kind != "interactive" {
                pane.child.child = None;
            }
            pane
        }

        fn kinds(&self) -> Vec<&'static str> {
            self.calls.iter().map(|call| call.0).collect()
        }
    }

    #[cfg(unix)]
    impl crate::app::PaneFactory for RecordingPanes {
        fn interactive(
            &mut self,
            spec: RoomSpec,
            _size: PtySize,
            _environment: GuestEnvironment,
            _native: Option<NativeRoom>,
        ) -> Result<Pane, Box<dyn std::error::Error>> {
            Ok(self.record("interactive", spec))
        }

        fn pending(
            &mut self,
            spec: RoomSpec,
            _size: PtySize,
            _environment: GuestEnvironment,
        ) -> Result<Pane, Box<dyn std::error::Error>> {
            Ok(self.record("pending", spec))
        }

        fn unavailable(
            &mut self,
            spec: RoomSpec,
            _size: PtySize,
            _environment: GuestEnvironment,
            _reason: &str,
        ) -> Result<Pane, Box<dyn std::error::Error>> {
            Ok(self.record("unavailable", spec))
        }
    }

    #[cfg(unix)]
    fn small_size() -> PtySize {
        PtySize {
            rows: 4,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    #[cfg(unix)]
    #[test]
    fn startup_starts_an_interactive_codex_only_for_a_completed_mint() {
        let cwd = tempfile::tempdir().unwrap();
        let program = Path::new("codex");
        let specs: Vec<RoomSpec> = (1..=4)
            .map(|number| {
                codex_mint_spec(program, cwd.path(), &format!("Codex · {number}"), MINTED_A)
            })
            .collect();
        let (_sender, receiver) = mpsc::channel();
        let codex = vec![
            Some(CodexMint::Pending(receiver)),
            Some(CodexMint::Failed("`codex exec` failed".to_owned())),
            Some(CodexMint::Ready(MINTED_B.to_owned())),
            None,
        ];
        let mut factory = RecordingPanes::default();
        let mut last_output = vec![None; 4];
        let (panes, spawned) = crate::app::start_rooms(
            &mut factory,
            specs,
            &[small_size(); 4],
            &vec![GuestEnvironment::new([]); 4],
            vec![None, None, None, None],
            &codex,
            &[None, None, None, None],
            &mut last_output,
            false,
            std::time::Duration::from_millis(5),
            &mut crate::app::Notices::default(),
        )
        .unwrap();
        assert_eq!(panes.len(), 4);
        assert_eq!(spawned.len(), 4);
        // Pending and failed mints start no process; the completed one resumes
        // exactly its minted session; a room that never had a mint is unchanged.
        assert_eq!(
            factory.kinds(),
            ["pending", "unavailable", "interactive", "interactive"]
        );
        assert_eq!(
            args_after(&factory.calls[2].2, "resume"),
            [OsString::from(MINTED_B)]
        );
        assert!(!factory.calls[3].2.iter().any(|arg| arg == "resume"));
    }

    /// Runs the startup loop with the Windows Headroom lane on, and reports
    /// whether it finished within `wait`.
    #[cfg(unix)]
    fn headroom_startup_finishes(
        codex: Vec<Option<CodexMint>>,
        wait: Duration,
        deadline: Duration,
    ) -> bool {
        let cwd = tempfile::tempdir().unwrap();
        let (_directory, path, _ext) = headroom_path();
        let specs: Vec<RoomSpec> = (0..codex.len())
            .map(|index| {
                let mut spec = codex_mint_spec(
                    Path::new("codex"),
                    cwd.path(),
                    &format!("Codex · {}", index + 1),
                    MINTED_A,
                );
                spec.use_headroom = true;
                spec.variables
                    .push((OsString::from("PATH"), path.clone().unwrap()));
                assert!(spec_uses_headroom(&spec));
                spec
            })
            .collect();
        let count = specs.len();
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let mut last_output = vec![None; count];
            let result = crate::app::start_rooms(
                &mut RecordingPanes::default(),
                specs,
                &vec![small_size(); count],
                &vec![GuestEnvironment::new([]); count],
                (0..count).map(|_| None).collect(),
                &codex,
                &(0..count).map(|_| None).collect::<Vec<_>>(),
                &mut last_output,
                true,
                deadline,
                &mut crate::app::Notices::default(),
            );
            let _ = done.send(result.is_ok());
        });
        finished.recv_timeout(wait) == Ok(true)
    }

    #[cfg(unix)]
    #[test]
    fn the_headroom_lane_never_waits_on_a_placeholder_room() {
        let (_sender, receiver) = mpsc::channel();
        let codex = vec![
            Some(CodexMint::Pending(receiver)),
            Some(CodexMint::Failed("`codex exec` failed".to_owned())),
            Some(CodexMint::Ready(MINTED_B.to_owned())),
        ];
        assert!(
            headroom_startup_finishes(codex, Duration::from_secs(5), Duration::from_millis(5)),
            "a later Headroom room waited for a room that has no process"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_silent_running_headroom_room_proceeds_after_the_injected_deadline() {
        let codex = vec![None, None];
        assert!(
            headroom_startup_finishes(codex, Duration::from_millis(500), Duration::from_millis(5)),
            "a silent running Headroom room did not proceed after the injected deadline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_headroom_lane_still_waits_on_a_running_room() {
        // Both rooms have a process, and the first never reports ready.
        let codex = vec![None, None];
        assert!(
            !headroom_startup_finishes(codex, Duration::from_millis(500), Duration::from_secs(10)),
            "the lane stopped waiting for a running Headroom room"
        );
    }

    /// An exited pane has no process left to become ready, so the lane must
    /// not wait on it at all.
    #[cfg(unix)]
    #[test]
    fn wait_for_headroom_ready_returns_promptly_when_the_pane_has_exited() {
        let (mut pane, _) = pane_with_capture(4, 20);
        pane.child.child = None;
        assert!(!pane.is_online(), "the pane must have no process");
        let mut last_output = None;
        let started = Instant::now();
        let timed_out = crate::app::wait_for_headroom_ready(
            &mut pane,
            &mut last_output,
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(!timed_out, "an exited pane must never be waited on");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an exited pane must return promptly"
        );
    }

    /// A pane that is already quiet-and-ready must not burn the deadline.
    #[cfg(unix)]
    #[test]
    fn wait_for_headroom_ready_returns_promptly_once_the_pane_is_ready() {
        let (mut pane, _) = pane_with_capture(4, 20);
        assert!(pane.is_online(), "the pane must still be running");
        // Quiet long past any threshold; the fixture guest is not OpenCode,
        // so quiet output alone means ready.
        let mut last_output = Some(Instant::now() - Duration::from_secs(60));
        let started = Instant::now();
        let timed_out = crate::app::wait_for_headroom_ready(
            &mut pane,
            &mut last_output,
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(!timed_out, "a ready pane must never be waited on");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a ready pane must return promptly"
        );
    }

    /// A silent but live predecessor still holds the lane for the injected
    /// deadline, then yields: the next room starts and the notice names the
    /// room it held for.
    #[cfg(unix)]
    #[test]
    fn a_silent_headroom_predecessor_yields_after_the_deadline_and_names_its_room() {
        let cwd = tempfile::tempdir().unwrap();
        let (_directory, path, _ext) = headroom_path();
        let specs: Vec<RoomSpec> = (0..2)
            .map(|index| {
                let mut spec = codex_mint_spec(
                    Path::new("codex"),
                    cwd.path(),
                    &format!("Codex · {}", index + 1),
                    MINTED_A,
                );
                spec.use_headroom = true;
                spec.variables
                    .push((OsString::from("PATH"), path.clone().unwrap()));
                assert!(spec_uses_headroom(&spec));
                spec
            })
            .collect();
        let codex = vec![None, None];
        let (done, finished) = mpsc::channel();
        thread::spawn(move || {
            let mut factory = RecordingPanes::default();
            let mut last_output = vec![None; 2];
            let mut notice = crate::app::Notices::default();
            let result = crate::app::start_rooms(
                &mut factory,
                specs,
                &vec![small_size(); 2],
                &vec![GuestEnvironment::new([]); 2],
                vec![None, None],
                &codex,
                &[None, None],
                &mut last_output,
                true,
                Duration::from_millis(500),
                &mut notice,
            );
            let _ = done.send((result.is_ok(), factory.kinds(), notice));
        });
        let (started, kinds, notice) = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("start_rooms yields after the injected deadline");
        assert!(started, "both rooms start once the deadline passes");
        assert_eq!(kinds, ["interactive", "interactive"]);
        let notice = notice.ordered_lines().join("; ");
        assert!(
            notice.contains("Codex · 1") && notice.contains("Headroom did not become ready"),
            "{notice}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_completed_mint_starts_one_resume_and_binds_without_dropping_earlier_rooms() {
        use crate::app::{DeliveryGate, PulseSample, start_rooms};
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let stubs: Vec<PathBuf> = (0..3)
            .map(|index| {
                fs::create_dir(root.path().join(format!("room{index}"))).unwrap();
                codex_stub_for_pane(
                    &root.path().join(format!("room{index}")),
                    "codex",
                    &format!(
                        "printf '%s\\n' \"$@\" >> '{log}'\nprintf 'Queued message sub_1 for thread %s.\\n' x",
                        log = root.path().join(format!("queue{index}")).display()
                    ),
                )
            })
            .collect();
        let specs: Vec<RoomSpec> = stubs
            .iter()
            .enumerate()
            .map(|(index, stub)| {
                codex_mint_spec(
                    stub,
                    cwd.path(),
                    &format!("Codex · {}", index + 1),
                    MINTED_A,
                )
            })
            .collect();
        let senders: Vec<_> = (0..3).map(|_| mpsc::channel()).collect();
        let (senders, receivers): (Vec<_>, Vec<_>) = senders.into_iter().unzip();
        let mut codex: Vec<Option<CodexMint>> = receivers
            .into_iter()
            .map(|rx| Some(CodexMint::Pending(rx)))
            .collect();
        let mut factory = RecordingPanes::default();
        let mut last_output = vec![None; 3];
        let environments = vec![GuestEnvironment::new([]); 3];
        let sizes = [small_size(); 3];
        let (mut panes, mut spawned_at) = start_rooms(
            &mut factory,
            specs,
            &sizes,
            &environments,
            vec![None, None, None],
            &codex,
            &[None, None, None],
            &mut last_output,
            false,
            std::time::Duration::from_millis(5),
            &mut crate::app::Notices::default(),
        )
        .unwrap();
        assert_eq!(factory.kinds(), ["pending"; 3]);
        factory.calls.clear();

        let path = root.path().join("state").join("native-outbox.json");
        let mut router = crate::native_route::NativeRouter::start(path, Vec::new()).unwrap();
        bind_codex_rooms(&mut router, &mut panes, &codex).unwrap();
        let mut gates: Vec<DeliveryGate> = (0..3).map(|_| DeliveryGate::new(false)).collect();
        let mut pulses: Vec<Option<PulseSample>> = vec![None; 3];
        let mut notice = crate::app::Notices::default();
        let mut complete =
            |factory: &mut RecordingPanes,
             codex: &mut Vec<Option<CodexMint>>,
             panes: &mut Vec<Pane>,
             router: &mut crate::native_route::NativeRouter| {
                crate::app::complete_mints(
                    factory,
                    codex,
                    panes,
                    Some(router),
                    &mut gates,
                    &mut spawned_at,
                    &mut last_output,
                    &mut pulses,
                    &sizes,
                    &environments,
                    &mut notice,
                    Instant::now(),
                )
                .unwrap();
            };

        // Nothing has finished: no process starts.
        complete(&mut factory, &mut codex, &mut panes, &mut router);
        assert!(factory.calls.is_empty());

        // Room 1 finishes: exactly one resume of its own session.
        senders[0]
            .send(Ok(Minted {
                uuid: MINTED_A.to_owned(),
                cwd: cwd.path().to_path_buf(),
                title: "Codex · 1".to_owned(),
            }))
            .unwrap();
        complete(&mut factory, &mut codex, &mut panes, &mut router);
        assert_eq!(factory.kinds(), ["interactive"]);
        assert_eq!(
            args_after(&factory.calls[0].2, "resume"),
            [OsString::from(MINTED_A)]
        );

        // Room 2 fails and room 3 is still pending: neither starts a process.
        senders[1]
            .send(Err("`codex exec` failed".to_owned()))
            .unwrap();
        complete(&mut factory, &mut codex, &mut panes, &mut router);
        assert_eq!(factory.kinds(), ["interactive", "unavailable"]);

        // Room 3 finishes later; room 1 must still be deliverable.
        senders[2]
            .send(Ok(Minted {
                uuid: MINTED_B.to_owned(),
                cwd: cwd.path().to_path_buf(),
                title: "Codex · 3".to_owned(),
            }))
            .unwrap();
        complete(&mut factory, &mut codex, &mut panes, &mut router);
        assert_eq!(
            factory.kinds(),
            ["interactive", "unavailable", "interactive"]
        );
        assert_eq!(
            args_after(&factory.calls[2].2, "resume"),
            [OsString::from(MINTED_B)]
        );

        let mut mailroom = Mailroom::new(50);
        for index in [0usize, 2] {
            router
                .send(&mut mailroom, "Sender", index, "hi", None)
                .unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(root.path().join("queue0").exists() && root.path().join("queue2").exists()) {
            router.tick(Instant::now(), &mut mailroom, false, false);
            assert!(
                Instant::now() < deadline,
                "an earlier room was dropped:\n{}",
                mailroom.summary()
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(!root.path().join("queue1").exists());
    }

    #[cfg(unix)]
    #[test]
    fn headroom_wraps_the_exec_and_resume_phases_identically() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let headroom = bin.join("headroom");
        fs::write(&headroom, "#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&headroom, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let cwd = tempfile::tempdir().unwrap();
        let mut spec = codex_mint_spec(&headroom, cwd.path(), "Codex · 1", MINTED_A);
        spec.program = "codex".into();
        spec.use_headroom = true;
        spec.headroom_args = vec![OsString::from("--no-serena")];
        spec.variables
            .push((OsString::from("PATH"), bin.as_os_str().to_owned()));

        let exec = crate::codex_native::exec_arguments(
            &[OsString::from("--dangerously-bypass-approvals-and-sandbox")],
            "House rules",
        );
        let resume: Vec<OsString> = vec![OsString::from("resume"), OsString::from(MINTED_A)];
        let (exec_program, exec_args) = codex_phase_command(&spec, &exec);
        let (resume_program, resume_args) = codex_phase_command(&spec, &resume);
        assert_eq!(exec_program, OsString::from("headroom"));
        assert_eq!(resume_program, OsString::from("headroom"));
        let prefix = [
            OsString::from("wrap"),
            OsString::from("codex"),
            OsString::from("--no-serena"),
        ];
        assert_eq!(&exec_args[..3], prefix);
        assert_eq!(&resume_args[..3], prefix);
        assert_eq!(&exec_args[3..], exec);
        assert_eq!(&resume_args[3..], resume);

        // The exec phase sees the same variables as the terminal phase.
        let environment = GuestEnvironment::new([
            ("PATH", bin.as_os_str().to_owned()),
            ("CROWDED_ROOM", OsString::from("1")),
        ]);
        let variables = codex_phase_variables(&spec, &environment);
        let look = |name: &str| {
            variables
                .iter()
                .filter(|(key, _)| key == OsStr::new(name))
                .count()
        };
        assert_eq!(look("PATH"), 1, "the environment replaces the room value");
        assert_eq!(look("CROWDED_ROOM"), 1);
        assert_eq!(look("CROWDED_TEST_MINT"), 1);

        // With Headroom off both phases run the guest directly.
        spec.use_headroom = false;
        let (exec_program, exec_args) = codex_phase_command(&spec, &exec);
        let (resume_program, resume_args) = codex_phase_command(&spec, &resume);
        assert_eq!(exec_program, OsString::from("codex"));
        assert_eq!(resume_program, OsString::from("codex"));
        assert_eq!(exec_args, exec);
        assert_eq!(resume_args, resume);
    }

    #[cfg(unix)]
    #[test]
    fn the_mint_runs_through_the_same_headroom_wrapper_as_the_terminal() {
        use std::os::unix::fs::PermissionsExt;
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("headroom.log");
        let codex = mint_stub(root.path(), &root.path().join("exec.log"), "sleep 0");
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let headroom = bin.join("headroom");
        fs::write(
            &headroom,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{log}'\nshift\nexec \"$@\"\n",
                log = log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&headroom, fs::Permissions::from_mode(0o700)).unwrap();
        let mut spec = codex_mint_spec(&codex, cwd.path(), "Codex · 1", MINTED_A);
        spec.use_headroom = true;
        spec.headroom_args = vec![OsString::from("--no-serena")];
        spec.variables
            .push((OsString::from("PATH"), bin.as_os_str().to_owned()));
        let prompts = vec![Some("House rules".to_owned())];
        let mints = mint_all(&mut [spec], &[GuestEnvironment::new([])], &prompts);
        assert_eq!(ready(&mints[0]), Some(MINTED_A));
        let wrapped = fs::read_to_string(&log).expect("the mint ran without Headroom");
        let lines: Vec<&str> = wrapped.lines().collect();
        assert_eq!(lines[0], "wrap");
        assert_eq!(lines[2], "--no-serena");
        assert_eq!(lines[3], "exec");
    }

    #[cfg(unix)]
    #[test]
    fn a_reaped_child_is_marked_dead_before_the_native_outbox_is_ticked() {
        use crate::native_route::{CodexEntry, NativeRouter};
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let count = root.path().join("count");
        let program = codex_stub_for_pane(
            root.path(),
            "codex",
            &format!(
                "printf x >> '{}'\nprintf 'Queued message sub_1 for thread %s.\\n' 'thread-a'",
                count.display()
            ),
        );
        let mut router = NativeRouter::start(
            root.path().join("state").join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        router
            .bind_codex(vec![CodexEntry {
                index: 0,
                title: "Room A".into(),
                room: Some(Arc::new(crate::codex_native::room(
                    program.into_os_string(),
                    cwd.path(),
                    "thread-a".into(),
                    Vec::new(),
                ))),
                generation: 1,
                reason: None,
            }])
            .unwrap();
        let (mut pane, _captured) = pane_with_capture(4, 20);
        pane.spec.program = "codex".into();
        pane.spec.native_delivery = true;
        let mut panes = vec![pane];
        let mut mailroom = Mailroom::new(50);
        router
            .send(&mut mailroom, "Sender", 0, "queued", None)
            .unwrap();
        // Paused passes save the message without sending it.
        for _ in 0..20 {
            crate::app::tick_native(
                &mut router,
                &panes,
                Instant::now(),
                &mut mailroom,
                true,
                false,
            );
            thread::sleep(Duration::from_millis(10));
        }

        // The child is reaped before the next pass.
        panes[0].child.child = None;
        assert!(!panes[0].is_online());
        crate::app::tick_native(
            &mut router,
            &panes,
            Instant::now(),
            &mut mailroom,
            false,
            false,
        );
        pump_router(&mut router, &mut mailroom, |view| {
            view.contains("HELD: the Codex room process is no longer running")
        });
        assert!(!count.exists(), "mail was sent to a dead room's thread");

        // The pass must refresh liveness before it lets the outbox schedule:
        // a child reaped in the previous iteration is already gone here.
        let source = include_str!("app.rs");
        let production = &source[..source.find("#[cfg(test)]").unwrap()];
        let body = &production[production.find("pub(crate) fn tick_native(").unwrap()..];
        let body = &body[..body.find("\n}\n").unwrap()];
        let refresh = body.find("refresh_codex_liveness(panes, router)").unwrap();
        let tick = body.find("router.tick(").unwrap();
        assert!(
            refresh < tick,
            "liveness must be refreshed before the outbox is ticked"
        );
    }

    #[test]
    fn scroll_retains_history_and_clamps_at_both_limits() {
        let mut pane = scroll_fixture(4, 20);
        for index in 0..10 {
            pane.parser.process(format!("line {index}\r\n").as_bytes());
        }
        assert_eq!(pane.scroll_offset, 0);
        pane.scroll_up(2);
        assert_eq!(pane.scroll_offset, 2);
        pane.scroll_up(usize::MAX);
        let retained = pane.scroll_offset;
        assert!(retained > 2, "history was retained ({retained})");
        pane.scroll_down(usize::MAX);
        assert_eq!(pane.scroll_offset, 0);
        assert_eq!(pane.parser.screen().scrollback(), 0);
    }

    #[test]
    fn scroll_applies_only_during_a_frame_and_restores_after() {
        let mut pane = scroll_fixture(4, 20);
        for index in 0..10 {
            pane.parser.process(format!("line {index}\r\n").as_bytes());
        }
        pane.scroll_up(3);
        pane.apply_scroll();
        assert_eq!(pane.parser.screen().scrollback(), 3);
        pane.restore_scroll();
        assert_eq!(pane.parser.screen().scrollback(), 0);
        assert_eq!(pane.visible_height(), 4);
    }

    #[test]
    fn page_keys_are_not_forwarded_to_the_child_pty() {
        for key in [KeyCode::PageUp, KeyCode::PageDown] {
            assert_eq!(
                key_bytes(KeyEvent::new(key, KeyModifiers::NONE)),
                None,
                "{key:?} must be intercepted by the app, never encoded to the PTY"
            );
        }
    }

    #[test]
    fn scroll_is_parsed_only_by_the_view_never_by_readiness() {
        let mut pane = scroll_fixture(4, 20);
        for index in 0..10 {
            pane.parser.process(format!("line {index}\r\n").as_bytes());
        }
        pane.scroll_up(2);
        let live_bottom = pane.parser.screen().contents();
        pane.apply_scroll();
        let scrolled = pane.parser.screen().contents();
        pane.restore_scroll();
        assert_eq!(pane.parser.screen().contents(), live_bottom);
        assert_ne!(scrolled, live_bottom);
    }

    use std::sync::{Arc, Mutex};
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FakeChild;
    impl portable_pty::Child for FakeChild {
        fn try_wait(&mut self) -> io::Result<Option<portable_pty::ExitStatus>> {
            Ok(None)
        }
        fn wait(&mut self) -> io::Result<portable_pty::ExitStatus> {
            Ok(portable_pty::ExitStatus::with_exit_code(0))
        }
        fn process_id(&self) -> Option<u32> {
            Some(1)
        }
        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }
    impl portable_pty::ChildKiller for FakeChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(FakeChild)
        }
    }

    fn pane_with_capture(rows: u16, cols: u16) -> (Pane, Arc<Mutex<Vec<u8>>>) {
        let mut pane = scroll_fixture(rows, cols);
        let captured = Arc::new(Mutex::new(Vec::new()));
        pane.writer = PaneWriter::spawn(Box::new(SharedWriter(captured.clone())));
        pane.child = ChildGuard {
            child: Some(Box::new(FakeChild) as Box<dyn portable_pty::Child + Send + Sync>),
            #[cfg(windows)]
            tree: None,
        };
        (pane, captured)
    }

    /// Waits, up to a bounded time, for the writer thread to deliver exactly
    /// `expected`, so tests that read a capturing writer do not race the thread.
    fn wait_for_captured(captured: &Arc<Mutex<Vec<u8>>>, expected: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if captured.lock().unwrap().as_slice() == expected {
                return;
            }
            if Instant::now() >= deadline {
                panic!(
                    "expected captured bytes {expected:?}, got {:?} after 2s",
                    captured.lock().unwrap()
                );
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn restart_replaces_the_pane_while_the_previous_child_is_still_alive() {
        let (mut pane, _) = pane_with_capture(4, 20);
        // FakeChild reports itself still running, so this pane is online and
        // plain Ctrl+R would never revive it -- the case F5 must cover.
        assert!(pane.is_online());
        // The fixture program name is a placeholder; use an always-present
        // binary so the replacement genuinely spawns on every platform.
        pane.spec.program = if cfg!(windows) {
            OsString::from("cmd.exe")
        } else {
            OsString::from("/bin/sh")
        };
        pane.restart(PtySize {
            rows: 4,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
        assert!(pane.is_online(), "the replacement pane must be online");
    }

    fn enter_alternate_screen(pane: &mut Pane) {
        pane.parser.process(b"\x1b[?1049h");
        assert!(pane.is_alternate_screen(), "must be in alternate screen");
    }

    #[test]
    fn an_inline_tui_scrolling_inside_a_region_builds_scrollback() {
        let mut pane = scroll_fixture(6, 20);
        // How Codex renders: a top-anchored scroll region with its input box
        // parked on the rows below, advanced with CSI S rather than by writing
        // past the last row. A row leaving the top of the screen belongs in the
        // scrollback even though a region is set, so the wheel has something to
        // move; this fails against a vt100 that discards it.
        pane.parser.process(b"\x1b[1;4r");
        for index in 0..20 {
            pane.parser
                .process(format!("\x1b[S\x1b[4;1Hline {index}").as_bytes());
        }
        assert!(
            !pane.is_alternate_screen(),
            "Codex stays on the primary screen"
        );
        pane.scroll_up(3);
        assert_eq!(
            pane.scroll_offset, 3,
            "the wheel must move retained history"
        );
    }

    #[test]
    fn alternate_screen_page_keys_forward_exact_bytes() {
        let (mut pane, captured) = pane_with_capture(4, 20);
        // Primary screen: PageUp scrolls parent, does not write CSI.
        for index in 0..10 {
            pane.parser.process(format!("line {index}\r\n").as_bytes());
        }
        let before = pane.scroll_offset;
        pane.scroll_up(pane.visible_height());
        assert!(pane.scroll_offset > before);
        assert!(captured.lock().unwrap().is_empty());

        // Alternate screen: PageUp must forward CSI 5~ and not affect scroll_offset.
        enter_alternate_screen(&mut pane);
        let scroll_before = pane.scroll_offset;
        pane.forward_page_up().unwrap();
        assert_eq!(pane.scroll_offset, scroll_before);
        wait_for_captured(&captured, b"\x1b[5~");
        assert_eq!(captured.lock().unwrap().as_slice(), b"\x1b[5~");
        captured.lock().unwrap().clear();
        pane.forward_page_down().unwrap();
        wait_for_captured(&captured, b"\x1b[6~");
        assert_eq!(captured.lock().unwrap().as_slice(), b"\x1b[6~");
    }

    #[test]
    fn alternate_screen_wheel_forwards_as_page_navigation() {
        let (mut pane, captured) = pane_with_capture(4, 20);
        enter_alternate_screen(&mut pane);
        pane.forward_wheel(true).unwrap();
        wait_for_captured(&captured, b"\x1b[5~");
        assert_eq!(captured.lock().unwrap().as_slice(), b"\x1b[5~");
        // Must not contain raw SGR mouse encoding
        assert!(
            !captured.lock().unwrap().windows(2).any(|w| w == b"[<"),
            "wheel must not leak SGR"
        );
        captured.lock().unwrap().clear();
        pane.forward_wheel(false).unwrap();
        wait_for_captured(&captured, b"\x1b[6~");
        assert_eq!(captured.lock().unwrap().as_slice(), b"\x1b[6~");
        captured.lock().unwrap().clear();
        // Re-entering primary should not forward wheel as CSI
        pane.parser.process(b"\x1b[?1049l");
        assert!(!pane.is_alternate_screen());
    }

    #[test]
    fn primary_screen_wheel_and_page_still_scroll_parent() {
        let (mut pane, captured) = pane_with_capture(4, 20);
        for index in 0..10 {
            pane.parser.process(format!("line {index}\r\n").as_bytes());
        }
        assert!(!pane.is_alternate_screen());
        pane.scroll_up(3);
        assert_eq!(pane.scroll_offset, 3);
        assert!(captured.lock().unwrap().is_empty());
        pane.scroll_down(3);
        assert_eq!(pane.scroll_offset, 0);
        // Wheel in primary must also scroll, not forward, so we test scroll_up directly
        pane.scroll_up(3);
        assert_eq!(pane.scroll_offset, 3);
    }

    /// Kills a spawned guest by process id from outside its owning thread, so a
    /// test can unblock a stuck PTY write without joining that thread.
    #[cfg(unix)]
    fn kill_child_process(pid: Option<u32>) {
        if let Some(pid) = pid {
            let _ = std::process::Command::new("kill")
                .arg("-9")
                .arg(pid.to_string())
                .output();
        }
    }

    #[cfg(unix)]
    fn whisper_test_size() -> PtySize {
        PtySize {
            rows: 8,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }
    }

    // A full-screen TUI such as OpenCode sets its tty to raw mode, where the
    // kernel input queue is about a kibibyte. Enqueuing a 64 KiB whisper must
    // still return promptly, because a pane writer thread does the blocking
    // write instead of the UI thread.
    #[cfg(unix)]
    #[test]
    fn send_whisper_returns_promptly_when_the_child_never_reads_stdin() {
        let _state = session_state::StateRootGuard::isolated();
        let spec = room_spec("sh", &["-c", "stty raw -echo; exec /bin/sleep 600"]);
        let mut pane = Pane::spawn(spec, whisper_test_size(), GuestEnvironment::new([])).unwrap();
        let pid = pane.child.child.as_ref().unwrap().process_id();
        // Let the child enter raw mode before the write is attempted.
        thread::sleep(Duration::from_millis(300));
        let message = "x".repeat(64 * 1024);
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(
                pane.send_whisper("room-9", &message)
                    .map_err(|e| e.to_string()),
            );
        });
        let outcome = rx.recv_timeout(Duration::from_secs(3));
        kill_child_process(pid);
        match outcome {
            Ok(Ok(())) => {}
            other => panic!(
                "send_whisper must return Ok within 3s even when the raw-mode child never \
                 reads its input: {other:?}"
            ),
        }
    }

    // Control: the same raw-mode 64 KiB write with a child that drains stdin.
    #[cfg(unix)]
    #[test]
    fn send_whisper_returns_when_the_child_drains_stdin() {
        let _state = session_state::StateRootGuard::isolated();
        let spec = room_spec("sh", &["-c", "stty raw -echo; cat > /dev/null"]);
        let mut pane = Pane::spawn(spec, whisper_test_size(), GuestEnvironment::new([])).unwrap();
        let pid = pane.child.child.as_ref().unwrap().process_id();
        thread::sleep(Duration::from_millis(300));
        let message = "x".repeat(64 * 1024);
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(
                pane.send_whisper("room-9", &message)
                    .map_err(|e| e.to_string()),
            );
        });
        let outcome = rx.recv_timeout(Duration::from_secs(3));
        kill_child_process(pid);
        match outcome {
            Ok(Ok(())) => {}
            other => {
                panic!("send_whisper must return within 3s when the child drains stdin: {other:?}")
            }
        }
    }

    // The writer thread owns the blocking write, so a full queue is the only way
    // a caller learns the guest is behind. It must fail fast, not grow.
    #[cfg(unix)]
    #[test]
    fn a_full_write_queue_returns_would_block_promptly() {
        let _state = session_state::StateRootGuard::isolated();
        let spec = room_spec("sh", &["-c", "stty raw -echo; exec /bin/sleep 600"]);
        let mut pane = Pane::spawn(spec, whisper_test_size(), GuestEnvironment::new([])).unwrap();
        let pid = pane.child.child.as_ref().unwrap().process_id();
        thread::sleep(Duration::from_millis(300));
        let job = vec![b'x'; 16 * 1024];
        let start = Instant::now();
        let mut accepted = 0usize;
        let error = loop {
            match pane.write_bytes(&job) {
                Ok(()) => accepted += 1,
                Err(error) => break error,
            }
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "queue never filled after {accepted} jobs"
            );
        };
        let elapsed = start.elapsed();
        kill_child_process(pid);
        assert_eq!(
            error.kind(),
            io::ErrorKind::WouldBlock,
            "expected WouldBlock, got {error:?}"
        );
        assert!(
            accepted <= WRITER_QUEUE_JOBS + 4,
            "queue grew past its {WRITER_QUEUE_JOBS}-job bound: {accepted} jobs"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "WouldBlock took {elapsed:?}"
        );
    }

    // Each whisper is one queued job, so two consecutive whispers must land as
    // body, submit, body, submit and never interleave.
    #[test]
    fn consecutive_whispers_reach_the_writer_in_order() {
        let (mut pane, captured) = pane_with_capture(4, 20);
        let bracketed = controls::uses_bracketed_paste(&pane.spec);
        let (body_a, submit_a) = whisper_parts(pane.spec.transport, bracketed, "Room A", "first");
        let (body_b, submit_b) = whisper_parts(pane.spec.transport, bracketed, "Room B", "second");
        pane.send_whisper("Room A", "first").unwrap();
        pane.send_whisper("Room B", "second").unwrap();
        let mut expected = body_a;
        expected.extend_from_slice(&submit_a.unwrap());
        expected.extend_from_slice(&body_b);
        expected.extend_from_slice(&submit_b.unwrap());
        wait_for_captured(&captured, &expected);
    }

    // Once the writer thread fails it exits; later callers must not enqueue into
    // a dead queue and must see BrokenPipe.
    #[test]
    fn a_failed_writer_makes_later_writes_fail_with_broken_pipe() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(io::Error::other("writer failed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut pane = scroll_fixture(4, 20);
        pane.writer = PaneWriter::spawn(Box::new(FailingWriter));
        pane.child = ChildGuard {
            child: Some(Box::new(FakeChild) as Box<dyn portable_pty::Child + Send + Sync>),
            #[cfg(windows)]
            tree: None,
        };
        // The first job is accepted; the thread then hits the failing write.
        pane.write_bytes(b"x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pane.writer.failed.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "writer never reported failure");
            thread::sleep(Duration::from_millis(5));
        }
        let error = pane.write_bytes(b"x").unwrap_err();
        assert_eq!(
            error.kind(),
            io::ErrorKind::BrokenPipe,
            "expected BrokenPipe, got {error:?}"
        );
    }

    // A cursor-position reply that cannot be queued must not fail draining: the
    // output still has to reach the parser.
    #[test]
    fn drain_output_stays_ok_when_the_writer_has_stopped() {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(io::Error::other("writer failed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut pane = scroll_fixture(4, 20);
        pane.writer = PaneWriter::spawn(Box::new(FailingWriter));
        pane.child = ChildGuard {
            child: Some(Box::new(FakeChild) as Box<dyn portable_pty::Child + Send + Sync>),
            #[cfg(windows)]
            tree: None,
        };
        // Stop the writer thread so any reply enqueue fails with BrokenPipe.
        pane.write_bytes(b"x").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pane.writer.finished.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "writer never stopped");
            thread::sleep(Duration::from_millis(5));
        }
        let (output_tx, output_rx) = mpsc::channel();
        output_tx.send(CURSOR_POSITION_QUERY.to_vec()).unwrap();
        pane.output_rx = output_rx;
        assert!(
            pane.drain_output().unwrap(),
            "output must still drain when the reply cannot be written"
        );
    }

    // Dropping a pane whose writer thread is blocked on a non-reading guest must
    // return promptly and let the thread end once the child is killed.
    #[cfg(unix)]
    #[test]
    fn dropping_a_pane_with_a_blocked_writer_completes() {
        let _state = session_state::StateRootGuard::isolated();
        let spec = room_spec("sh", &["-c", "stty raw -echo; exec /bin/sleep 600"]);
        let mut pane = Pane::spawn(spec, whisper_test_size(), GuestEnvironment::new([])).unwrap();
        thread::sleep(Duration::from_millis(300));
        let job = vec![b'x'; 16 * 1024];
        for _ in 0..(WRITER_QUEUE_JOBS + 8) {
            if pane.write_bytes(&job).is_err() {
                break;
            }
        }
        let finished = Arc::clone(&pane.writer.finished);
        let start = Instant::now();
        drop(pane);
        let drop_elapsed = start.elapsed();
        assert!(
            drop_elapsed < Duration::from_secs(3),
            "drop took {drop_elapsed:?}"
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        while !finished.load(Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "writer thread did not finish after the pane was dropped"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    // A raw-mode pane whose guest never reads, with its write queue already full.
    #[cfg(unix)]
    fn saturated_raw_pane() -> Pane {
        let spec = room_spec("sh", &["-c", "stty raw -echo; exec /bin/sleep 600"]);
        let mut pane = Pane::spawn(spec, whisper_test_size(), GuestEnvironment::new([])).unwrap();
        // Let the child enter raw mode before the writes are attempted.
        thread::sleep(Duration::from_millis(300));
        let job = vec![b'x'; 16 * 1024];
        while pane.write_bytes(&job).is_ok() {}
        pane
    }

    // A pane whose writer thread has already failed, so every later enqueue is
    // a BrokenPipe.
    fn pane_with_stopped_writer(rows: u16, cols: u16) -> Pane {
        struct FailingWriter;
        impl std::io::Write for FailingWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(io::Error::other("writer failed"))
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (mut pane, _captured) = pane_with_capture(rows, cols);
        pane.writer = PaneWriter::spawn(Box::new(FailingWriter));
        let _ = pane.write_bytes(b"x");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !pane.writer.finished.load(Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "stopped writer never finished");
            thread::sleep(Duration::from_millis(5));
        }
        pane
    }

    // Queue saturation must not end the event loop: a key that cannot be
    // enqueued surfaces as a notice instead of a propagated error.
    #[cfg(unix)]
    #[test]
    fn a_key_that_cannot_be_queued_becomes_a_notice() {
        let mut pane = saturated_raw_pane();
        let notice = crate::app::forward_key(
            &mut pane,
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
        );
        assert!(
            notice.is_some(),
            "a failed key enqueue must surface as a notice"
        );
    }

    // A stopped writer means the resubmit record is dropped, never propagated.
    #[test]
    fn resubmit_drops_the_record_when_the_writer_has_stopped() {
        let mut pane = pane_with_stopped_writer(4, 20);
        let now = Instant::now();
        let stale = now - Duration::from_secs(30);
        let mut pending = std::collections::VecDeque::from([(stale, 0usize, false)]);
        crate::app::resend_whisper_submits(
            &mut pending,
            std::slice::from_mut(&mut pane),
            &[true],
            now,
        );
        assert!(
            pending.is_empty(),
            "a stopped writer must drop the resubmit record"
        );
    }

    // A full queue is transient: the resubmit record is kept for the next frame.
    #[cfg(unix)]
    #[test]
    fn resubmit_keeps_the_record_when_the_queue_is_full() {
        let mut pane = saturated_raw_pane();
        let now = Instant::now();
        let stale = now - Duration::from_secs(30);
        let mut pending = std::collections::VecDeque::from([(stale, 0usize, false)]);
        crate::app::resend_whisper_submits(
            &mut pending,
            std::slice::from_mut(&mut pane),
            &[true],
            now,
        );
        assert_eq!(
            pending.len(),
            1,
            "a full queue must keep the resubmit record for retry"
        );
    }

    // A writer that blocks its first write until released, then drains
    // instantly: it lets a test fill a pane's write queue and later drain it.
    // It raises `entered` on its first write, proving the writer thread has
    // taken a job and cannot free another slot until released.
    struct GatedWriter {
        reading: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }

    impl std::io::Write for GatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.entered.store(true, Ordering::SeqCst);
            while !self.reading.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn pane_with_gated_writer(
        rows: u16,
        cols: u16,
        reading: bool,
    ) -> (Pane, Arc<AtomicBool>, Arc<AtomicBool>) {
        let (mut pane, _captured) = pane_with_capture(rows, cols);
        let gate = Arc::new(AtomicBool::new(reading));
        let entered = Arc::new(AtomicBool::new(false));
        pane.writer = PaneWriter::spawn(Box::new(GatedWriter {
            reading: Arc::clone(&gate),
            entered: Arc::clone(&entered),
        }));
        (pane, gate, entered)
    }

    // A writer that models a slowly scheduled thread: its first write parks
    // for `delay` before proceeding, then blocks like the gate. The queue
    // fills while it parks, so a fill that returns before it proceeds leaves
    // a slot the writer frees afterwards.
    struct DelayedGatedWriter {
        delay: Duration,
        reading: Arc<AtomicBool>,
        entered: Arc<AtomicBool>,
    }

    impl std::io::Write for DelayedGatedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.entered.load(Ordering::SeqCst) {
                thread::sleep(self.delay);
                self.entered.store(true, Ordering::SeqCst);
            }
            while !self.reading.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn pane_with_delayed_writer(
        rows: u16,
        cols: u16,
        delay: Duration,
    ) -> (Pane, Arc<AtomicBool>, Arc<AtomicBool>) {
        let (mut pane, _captured) = pane_with_capture(rows, cols);
        let gate = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        pane.writer = PaneWriter::spawn(Box::new(DelayedGatedWriter {
            delay,
            reading: Arc::clone(&gate),
            entered: Arc::clone(&entered),
        }));
        (pane, gate, entered)
    }

    fn fill_pane_queue(pane: &mut Pane, entered: &AtomicBool) {
        let job = vec![b'x'; 16 * 1024];
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            while pane.write_bytes(&job).is_ok() {}
            // The writer thread may take a queued job the moment filling
            // stops, freeing a slot the next production write could use.
            // Only return once the writer is known to hold a job (its first
            // write ran, so it cannot take another while blocked) and a
            // further write is still refused; any slot it frees while waiting
            // is refilled by the loop above. The sleeps are poll granularity,
            // not timing: correctness comes from the observed condition.
            if entered.load(Ordering::SeqCst) && pane.write_bytes(&job).is_err() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the write queue never stayed full"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn run_inject_pass(
        pending: &mut std::collections::VecDeque<(u64, usize)>,
        mailroom: &mut Mailroom,
        panes: &mut [Pane],
        gates: &mut [DeliveryGate],
        input_ready: &[bool],
        submit: &mut std::collections::VecDeque<(Instant, usize, bool)>,
    ) -> (usize, usize) {
        inject_ready_pending(
            pending,
            PendingSubmit {
                queue: submit,
                now: Instant::now(),
            },
            mailroom,
            panes,
            gates,
            input_ready,
            &mut DeliveryFuse::new(0),
        )
    }

    // A WouldBlock delivery is transient: the envelope stays pending and its
    // Mailroom status stays queued, not failed.
    #[test]
    fn a_would_block_delivery_stays_pending_and_queued() {
        let (mut pane, gate, entered) = pane_with_gated_writer(4, 20, false);
        fill_pane_queue(&mut pane, &entered);
        let mut mailroom = Mailroom::new(100);
        let id = mailroom.queue(
            "Room A".into(),
            pane.title().to_owned(),
            "hello".into(),
            "awaiting injection",
        );
        let mut pending = std::collections::VecDeque::from([(id, 0usize)]);
        let mut gates = vec![DeliveryGate::new(false)];
        let mut submit = std::collections::VecDeque::new();
        let counts = run_inject_pass(
            &mut pending,
            &mut mailroom,
            std::slice::from_mut(&mut pane),
            &mut gates,
            &[true],
            &mut submit,
        );
        assert_eq!(counts, (0, 0));
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0], (id, 0));
        let summary = mailroom.summary();
        assert!(summary.contains("QUEUED"), "expected QUEUED, got {summary}");
        assert!(!summary.contains("FAILED"), "unexpected FAILED: {summary}");
        gate.store(true, Ordering::SeqCst);
    }

    // After the guest starts reading, the kept envelope is delivered exactly
    // once on a later pass.
    #[test]
    fn a_requeued_delivery_is_later_injected_exactly_once() {
        let (mut pane, gate, entered) = pane_with_gated_writer(4, 20, false);
        fill_pane_queue(&mut pane, &entered);
        let mut mailroom = Mailroom::new(100);
        let id = mailroom.queue(
            "Room A".into(),
            pane.title().to_owned(),
            "hello".into(),
            "awaiting injection",
        );
        let mut pending = std::collections::VecDeque::from([(id, 0usize)]);
        let mut gates = vec![DeliveryGate::new(false)];
        let mut submit = std::collections::VecDeque::new();
        let first = run_inject_pass(
            &mut pending,
            &mut mailroom,
            std::slice::from_mut(&mut pane),
            &mut gates,
            &[true],
            &mut submit,
        );
        assert_eq!(first, (0, 0));
        assert_eq!(pending.len(), 1);
        // Let the guest start reading: the queue drains, freeing a slot.
        gate.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while pane.write_bytes(b"x").is_err() {
            assert!(Instant::now() < deadline, "queue never drained");
            thread::sleep(Duration::from_millis(5));
        }
        let second = run_inject_pass(
            &mut pending,
            &mut mailroom,
            std::slice::from_mut(&mut pane),
            &mut gates,
            &[true],
            &mut submit,
        );
        assert_eq!(second, (1, 0));
        assert!(pending.is_empty());
        assert_eq!(mailroom.summary().matches("INJECTED").count(), 1);
    }

    // A blocked room keeps its envelopes in order while another room's envelope
    // in the same pass is still delivered.
    #[test]
    fn a_blocked_room_keeps_order_while_another_room_still_delivers() {
        let (mut busy, busy_gate, busy_entered) = pane_with_gated_writer(4, 20, false);
        fill_pane_queue(&mut busy, &busy_entered);
        let (free, _free_gate, _free_entered) = pane_with_gated_writer(4, 20, true);
        let mut mailroom = Mailroom::new(100);
        let first = mailroom.queue(
            "Room A".into(),
            busy.title().to_owned(),
            "first".into(),
            "awaiting injection",
        );
        let other = mailroom.queue(
            "Room A".into(),
            free.title().to_owned(),
            "other".into(),
            "awaiting injection",
        );
        let second = mailroom.queue(
            "Room A".into(),
            busy.title().to_owned(),
            "second".into(),
            "awaiting injection",
        );
        // The other room's envelope sits between the two blocked-room ones.
        let mut pending =
            std::collections::VecDeque::from([(first, 0usize), (other, 1), (second, 0)]);
        let mut gates = vec![DeliveryGate::new(false), DeliveryGate::new(false)];
        let mut submit = std::collections::VecDeque::new();
        let counts = run_inject_pass(
            &mut pending,
            &mut mailroom,
            &mut [busy, free],
            &mut gates,
            &[true, true],
            &mut submit,
        );
        assert_eq!(counts, (1, 0));
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0], (first, 0));
        assert_eq!(pending[1], (second, 0));
        assert_eq!(mailroom.summary().matches("INJECTED").count(), 1);
        busy_gate.store(true, Ordering::SeqCst);
    }

    // The fill waits for a slowly scheduled writer instead of trusting one
    // instant: with a 150 ms delayed start the queue is still full after the
    // fill returns, because the writer was observed holding a job first. The
    // old 20 ms settle returns before the writer proceeds, so a write then
    // succeeds once the writer takes its job.
    #[test]
    fn a_fill_survives_a_slow_writer_start_of_150_ms() {
        let (mut pane, gate, entered) = pane_with_delayed_writer(4, 20, Duration::from_millis(150));
        fill_pane_queue(&mut pane, &entered);
        assert!(
            entered.load(Ordering::SeqCst),
            "the fill returns only after the writer took a job"
        );
        assert!(
            pane.write_bytes(&vec![b'x'; 16 * 1024]).is_err(),
            "the queue stays full after a slow writer start"
        );
        gate.store(true, Ordering::SeqCst);
    }

    // Same as above with a 500 ms delayed start: the wait is a condition,
    // not a tuned sleep, so a longer delay only waits longer.
    #[test]
    fn a_fill_survives_a_slow_writer_start_of_500_ms() {
        let (mut pane, gate, entered) = pane_with_delayed_writer(4, 20, Duration::from_millis(500));
        fill_pane_queue(&mut pane, &entered);
        assert!(
            entered.load(Ordering::SeqCst),
            "the fill returns only after the writer took a job"
        );
        assert!(
            pane.write_bytes(&vec![b'x'; 16 * 1024]).is_err(),
            "the queue stays full after a slow writer start"
        );
        gate.store(true, Ordering::SeqCst);
    }

    // A non-WouldBlock error is still a hard failure: dropped, counted failed,
    // and marked failed.
    #[test]
    fn a_non_would_block_error_is_dropped_and_marked_failed() {
        // A pane with no child is offline, so its whisper fails NotConnected.
        let mut pane = scroll_fixture(4, 20);
        assert!(!pane.is_online());
        let mut mailroom = Mailroom::new(100);
        let id = mailroom.queue(
            "Room A".into(),
            pane.title().to_owned(),
            "hello".into(),
            "awaiting injection",
        );
        let mut pending = std::collections::VecDeque::from([(id, 0usize)]);
        let mut gates = vec![DeliveryGate::new(false)];
        let mut submit = std::collections::VecDeque::new();
        let counts = run_inject_pass(
            &mut pending,
            &mut mailroom,
            std::slice::from_mut(&mut pane),
            &mut gates,
            &[true],
            &mut submit,
        );
        assert_eq!(counts, (0, 1));
        assert!(pending.is_empty());
        let summary = mailroom.summary();
        assert!(summary.contains("FAILED"), "expected FAILED, got {summary}");
        assert!(!summary.contains("QUEUED"), "unexpected QUEUED: {summary}");
    }

    // A direct send is not retried, so a full queue is recorded as FAILED even
    // though the pending path records the same error as QUEUED.
    #[test]
    fn a_direct_delivery_marks_would_block_as_failed() {
        let (mut pane, gate, entered) = pane_with_gated_writer(4, 20, false);
        fill_pane_queue(&mut pane, &entered);
        let mut mailroom = Mailroom::new(100);
        let (_, result) = mailroom.deliver("Room A".to_owned(), &mut pane, "hello".to_owned());
        assert_eq!(
            result.unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "a full queue must report WouldBlock to the direct sender"
        );
        let summary = mailroom.summary();
        assert!(summary.contains("FAILED"), "expected FAILED, got {summary}");
        assert!(!summary.contains("QUEUED"), "unexpected QUEUED: {summary}");
        gate.store(true, Ordering::SeqCst);
    }

    // A gated writer that also records everything it writes, so a test can see
    // exactly which bytes (and how many Enters) reached the pane.
    struct GatedCaptureWriter {
        reading: Arc<AtomicBool>,
        captured: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for GatedCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            while !self.reading.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
            }
            self.captured.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn pane_with_gated_capture(
        rows: u16,
        cols: u16,
        reading: bool,
    ) -> (Pane, Arc<AtomicBool>, Arc<Mutex<Vec<u8>>>) {
        let (mut pane, _captured) = pane_with_capture(rows, cols);
        let gate = Arc::new(AtomicBool::new(reading));
        let captured = Arc::new(Mutex::new(Vec::new()));
        pane.writer = PaneWriter::spawn(Box::new(GatedCaptureWriter {
            reading: Arc::clone(&gate),
            captured: Arc::clone(&captured),
        }));
        (pane, gate, captured)
    }

    fn wait_for_no_unwritten_input(pane: &Pane) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while pane.has_unwritten_input() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }

    // A queued or in-flight whisper is reported as unwritten until the writer
    // finishes it.
    #[test]
    fn unwritten_input_is_reported_until_the_writer_drains() {
        let (mut pane, gate, _entered) = pane_with_gated_writer(4, 20, false);
        pane.send_whisper("Room A", "hello").unwrap();
        assert!(pane.has_unwritten_input(), "a queued whisper is unwritten");
        gate.store(true, Ordering::SeqCst);
        assert!(wait_for_no_unwritten_input(&pane), "writer never drained");
        assert!(!pane.has_unwritten_input());
    }

    // The regression: an old record whose whisper is still unwritten is kept and
    // its clock restarts, so only the original Enter reaches the pane.
    #[test]
    fn an_old_record_with_unwritten_input_is_not_resent() {
        let (mut pane, gate, captured) = pane_with_gated_capture(4, 20, false);
        pane.send_whisper("Room A", "hello").unwrap();
        let injected_at = Instant::now();
        let future = injected_at + Duration::from_secs(30);
        let mut pending = std::collections::VecDeque::from([(injected_at, 0usize, false)]);
        crate::app::resend_whisper_submits(
            &mut pending,
            std::slice::from_mut(&mut pane),
            &[true],
            future,
        );
        assert_eq!(
            pending.len(),
            1,
            "an unwritten whisper must keep its record"
        );
        assert_eq!(
            pending[0],
            (future, 0, false),
            "the record's clock must restart while the whisper is unwritten"
        );
        // The gate opens; only the original body and its Enter are written.
        gate.store(true, Ordering::SeqCst);
        assert!(wait_for_no_unwritten_input(&pane), "writer never drained");
        let bytes = captured.lock().unwrap().clone();
        assert_eq!(
            bytes.iter().filter(|byte| **byte == b'\r').count(),
            1,
            "exactly one Enter must reach the pane, got {bytes:?}"
        );
    }

    // Once the writer has drained, an old record that was never seen busy is
    // still resent exactly once.
    #[test]
    fn an_old_record_is_resent_once_after_the_writer_drains() {
        let (mut pane, _gate, captured) = pane_with_gated_capture(4, 20, true);
        pane.send_whisper("Room A", "hello").unwrap();
        assert!(wait_for_no_unwritten_input(&pane), "writer never drained");
        let injected_at = Instant::now();
        let future = injected_at + Duration::from_secs(30);
        let mut pending = std::collections::VecDeque::from([(injected_at, 0usize, false)]);
        crate::app::resend_whisper_submits(
            &mut pending,
            std::slice::from_mut(&mut pane),
            &[true],
            future,
        );
        assert!(pending.is_empty(), "a resend retires the record");
        let deadline = Instant::now() + Duration::from_secs(2);
        while captured
            .lock()
            .unwrap()
            .iter()
            .filter(|byte| **byte == b'\r')
            .count()
            < 2
        {
            assert!(Instant::now() < deadline, "resend Enter never written");
            thread::sleep(Duration::from_millis(5));
        }
        let bytes = captured.lock().unwrap().clone();
        assert_eq!(
            bytes.iter().filter(|byte| **byte == b'\r').count(),
            2,
            "the resend must add exactly one Enter, got {bytes:?}"
        );
    }

    // A stopped writer is not awaited: its record is resolved at once.
    #[test]
    fn a_stopped_writer_resolves_the_record_without_waiting() {
        let mut pane = pane_with_stopped_writer(4, 20);
        assert!(
            !pane.has_unwritten_input(),
            "a stopped writer has no in-flight input"
        );
        let injected_at = Instant::now();
        let future = injected_at + Duration::from_secs(30);
        let mut pending = std::collections::VecDeque::from([(injected_at, 0usize, false)]);
        crate::app::resend_whisper_submits(
            &mut pending,
            std::slice::from_mut(&mut pane),
            &[true],
            future,
        );
        assert!(
            pending.is_empty(),
            "a stopped writer must resolve the record instead of keeping it"
        );
    }

    // The unwritten counter must return to zero after a drain and after a
    // WouldBlock rejection, never underflowing.
    #[test]
    fn the_unwritten_counter_returns_to_zero() {
        let (mut pane, _gate, _entered) = pane_with_gated_writer(4, 20, true);
        for _ in 0..100 {
            let _ = pane.write_bytes(b"x");
        }
        assert!(
            wait_for_no_unwritten_input(&pane),
            "counter never returned to zero after enqueues"
        );
        let (mut pane, gate, entered) = pane_with_gated_writer(4, 20, false);
        fill_pane_queue(&mut pane, &entered);
        gate.store(true, Ordering::SeqCst);
        assert!(
            wait_for_no_unwritten_input(&pane),
            "counter stuck after a WouldBlock rejection and drain"
        );
    }

    /// A Claude launch writes the bootstrap inbox connection file before the
    /// terminal starts, and its child never inherits the nested-session marker.
    #[cfg(unix)]
    #[test]
    fn a_claude_launch_writes_its_inbox_file_and_a_clean_environment() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("started.log");
        let stub = codex_stub_for_pane(
            root.path(),
            "claude",
            &format!(
                "if [ -f .crowded/claude-inbox/*.json ]; then printf 'file\\n' >> '{0}'; else printf 'missing\\n' >> '{0}'; fi\nprintf '%s|%s\\n' \"${{CLAUDE_CODE_CHILD_SESSION-unset}}\" \"${{CROWDED_TEST_KEEP-unset}}\" >> '{0}'\nexec sleep 30",
                marker.display()
            ),
        );
        let mut specs = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];
        let (mut starts, _) = start_claude_sessions(&mut specs);
        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut starts)
                .unwrap()
                .is_empty()
        );
        let pane = crate::app::spawn_room(
            &mut crate::app::LivePanes,
            specs.remove(0),
            small_size(),
            GuestEnvironment::new([
                ("CLAUDE_CODE_CHILD_SESSION", "child".into()),
                ("CROWDED_TEST_KEEP", "kept".into()),
            ]),
            None,
            None,
            starts[0].as_ref(),
        )
        .unwrap();
        wait_for_file(&marker);
        let deadline = Instant::now() + Duration::from_secs(5);
        while fs::read_to_string(&marker).unwrap().lines().count() < 2 {
            assert!(Instant::now() < deadline, "the stub wrote one line");
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            "file\nunset|kept\n",
            "the inbox file exists before the child runs, without the child marker"
        );
        drop(pane);
    }

    /// Clearing a Claude room moves its inbox file to the new launch and leaves
    /// the old receiver and its mail behind.
    #[cfg(unix)]
    #[test]
    fn a_claude_clear_moves_the_inbox_file_and_holds_old_mail() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
        let mut specs = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];
        let (mut starts, _) = start_claude_sessions(&mut specs);
        assert!(
            matches!(&starts[0], Some(ClaudeStart::Resume(id)) if id == MINTED_A),
            "{:?}",
            starts[0]
        );
        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut starts)
                .unwrap()
                .is_empty()
        );
        let mut panes = vec![
            Pane::spawn_unavailable(
                specs.remove(0),
                small_size(),
                GuestEnvironment::new([]),
                "offline",
            )
            .unwrap(),
        ];
        let old = crate::claude_inbox::mod_of(cwd.path(), MINTED_A);
        let registered = crate::claude_inbox::FakeMod::register(&old.url, &old.token);

        const SECOND: &str = "3f8f8f8f-3333-4333-8333-333333333333";
        bind_claude_room_at(
            &mut router,
            &mut panes,
            0,
            SECOND,
            next_generation().unwrap(),
        )
        .unwrap();

        assert_eq!(registered.next_status(), 401, "the old receiver is refused");
        assert!(
            !crate::claude_inbox::connection_file(cwd.path(), MINTED_A)
                .unwrap()
                .exists(),
            "the cleared launch's connection file is removed"
        );
        assert!(
            crate::claude_inbox::connection_file(cwd.path(), SECOND)
                .unwrap()
                .exists(),
            "the new launch has its own connection file"
        );
        let mut mailroom = Mailroom::new(10);
        let (replied, reply) = std::sync::mpsc::sync_channel(1);
        router
            .send(
                &mut mailroom,
                "Sender",
                0,
                "after clear",
                Some(Box::new(move |_, reply| {
                    let _ = replied.send(reply);
                })),
            )
            .unwrap();
        pump_router(&mut router, &mut mailroom, |shown| {
            shown.contains("HELD: no Claude mod connected")
        });
        assert_eq!(
            reply.recv_timeout(Duration::from_secs(5)).unwrap(),
            crate::native_route::NativeReply::Held("no Claude mod connected".to_owned())
        );
        let relaunched = crate::claude_inbox::mod_of(cwd.path(), SECOND);
        let fresh = crate::claude_inbox::FakeMod::register(&relaunched.url, &relaunched.token);
        assert!(!fresh.receiver.is_empty());
    }

    #[cfg(unix)]
    fn claude_started_spec(
        root: &Path,
        cwd: &Path,
        title: &str,
        config_dir: &Path,
    ) -> (Vec<RoomSpec>, Vec<Option<ClaudeStart>>, String) {
        let stub = codex_stub_for_pane(root, "claude", "exec sleep 30");
        session_state::upsert_reported("claude", cwd, title, MINTED_A).unwrap();
        materialize_claude_session(config_dir, cwd, MINTED_A);
        let mut specs = vec![claude_native_spec_for(&stub, cwd, title)];
        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        let session = match &starts[0] {
            Some(ClaudeStart::Resume(session)) => session.clone(),
            other => panic!("{other:?}"),
        };
        (specs, starts, session)
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_start_extracts_its_mod_and_passes_plugin_dir() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let (specs, _, session) =
            claude_started_spec(root.path(), cwd.path(), "Claude · 1", _claude_config.path());
        let dir = cwd.path().join(".crowded").join("claude-mod");
        assert_eq!(
            specs[0].args,
            vec![
                OsString::from("--resume"),
                OsString::from(&session),
                OsString::from("--plugin-dir"),
                dir.as_os_str().into(),
            ]
        );
        for (name, contents) in CLAUDE_MOD_FILES {
            let bytes = fs::read(dir.join(name)).unwrap();
            assert_eq!(&String::from_utf8(bytes.clone()).unwrap(), contents);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    fs::metadata(dir.join(name)).unwrap().permissions().mode() & 0o777,
                    0o600,
                    "{name} is private"
                );
            }
        }
        // A second start with the same bundle rewrites nothing.
        let before: Vec<std::time::SystemTime> = CLAUDE_MOD_FILES
            .iter()
            .map(|(name, _)| fs::metadata(dir.join(name)).unwrap().modified().unwrap())
            .collect();
        let _ = claude_started_spec(root.path(), cwd.path(), "Claude · 1", _claude_config.path());
        for ((name, _), stamped) in CLAUDE_MOD_FILES.iter().zip(&before) {
            assert_eq!(
                &fs::metadata(dir.join(name)).unwrap().modified().unwrap(),
                stamped,
                "{name} was rewritten without a content change"
            );
        }
        // A changed file is restored on the next start.
        fs::write(dir.join("hooks/deliver.mjs"), "tampered").unwrap();
        let _ = claude_started_spec(root.path(), cwd.path(), "Claude · 1", _claude_config.path());
        assert_eq!(
            fs::read_to_string(dir.join("hooks/deliver.mjs")).unwrap(),
            CLAUDE_MOD_FILES[2].1
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_claude_spawn_passes_plugin_dir_once() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("argv.log");
        let body = format!("echo \"$*\" >> '{0}'\nexec sleep 30", marker.display());
        let stub = codex_stub_for_pane(root.path(), "claude", &body);
        let mut specs = vec![claude_native_spec_for(&stub, cwd.path(), "Claude · 1")];
        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        let pane = crate::app::spawn_room(
            &mut crate::app::LivePanes,
            specs.remove(0),
            small_size(),
            GuestEnvironment::new([]),
            None,
            None,
            starts[0].as_ref(),
        )
        .unwrap();
        let argv = wait_for_file(&marker);
        let want = cwd.path().join(".crowded").join("claude-mod");
        let flag = format!("--plugin-dir {}", want.display());
        assert_eq!(argv.matches("--plugin-dir").count(), 1, "{argv}");
        assert!(argv.contains(&flag), "{argv}");
        drop(pane);
    }

    #[cfg(unix)]
    #[test]
    fn every_claude_relaunch_passes_plugin_dir() {
        let _state = session_state::StateRootGuard::isolated();
        let _claude_config = ClaudeConfigDirGuard::new();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fn resume_args(session: &str, dir: &Path) -> Vec<OsString> {
            vec![
                OsString::from("--resume"),
                OsString::from(session),
                OsString::from("--plugin-dir"),
                dir.as_os_str().into(),
            ]
        }
        fn fresh_args(dir: &Path) -> Vec<OsString> {
            vec![OsString::from("--plugin-dir"), dir.as_os_str().into()]
        }
        let (mut specs, _, stored) =
            claude_started_spec(root.path(), cwd.path(), "Claude · 1", _claude_config.path());
        let dir = cwd.path().join(".crowded").join("claude-mod");
        let pane = Pane::spawn_unavailable(
            specs.remove(0),
            small_size(),
            GuestEnvironment::new([]),
            "offline",
        )
        .unwrap();
        let (spec, session) = pane.relaunch_claude(ClaudeRelaunch::Resume, 1).unwrap();
        assert_eq!(session.as_deref(), Some(stored.as_str()));
        assert_eq!(spec.args, resume_args(&stored, &dir));
        let (spec, session) = pane.relaunch_claude(ClaudeRelaunch::Restart, 1).unwrap();
        assert_eq!(session.as_deref(), Some(stored.as_str()));
        assert_eq!(spec.args, resume_args(&stored, &dir));
        let (spec, session) = pane.relaunch_claude(ClaudeRelaunch::Clear, 1).unwrap();
        assert_eq!(session, None);
        assert_eq!(spec.args, fresh_args(&dir));
        // A title with no stored session still fails instead of starting.
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        let fresh = Pane::spawn_unavailable(
            claude_native_spec_for(&stub, cwd.path(), "Claude · 2"),
            small_size(),
            GuestEnvironment::new([]),
            "offline",
        )
        .unwrap();
        let error = match fresh.relaunch_claude(ClaudeRelaunch::Resume, 2) {
            Ok(_) => panic!("a resume without a session must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("control 2 clear"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_user_plugin_dir_is_left_alone() {
        let _state = session_state::StateRootGuard::isolated();
        let cwd = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        let mut spec = claude_native_spec_for(&stub, cwd.path(), "Claude · 1");
        spec.args = vec!["--plugin-dir".into(), "/user/mod".into()];
        let mut specs = vec![spec];
        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        assert!(matches!(&starts[0], Some(ClaudeStart::Fresh)));
        let mut plugin_dirs = 0;
        for arg in &specs[0].args {
            plugin_dirs += usize::from(arg == "--plugin-dir");
        }
        assert_eq!(plugin_dirs, 1);
        assert!(
            !cwd.path().join(".crowded").join("claude-mod").exists(),
            "no mod is extracted for an explicit user choice"
        );
        // A room whose mod directory cannot be written does not start.
        let stuck = tempfile::tempdir().unwrap();
        fs::create_dir_all(stuck.path().join(".crowded")).unwrap();
        fs::write(stuck.path().join(".crowded").join("claude-mod"), "blocked").unwrap();
        let mut specs = vec![claude_native_spec_for(&stub, stuck.path(), "Claude · 2")];
        let (starts, notices) = start_claude_sessions(&mut specs);
        assert!(matches!(&starts[0], Some(ClaudeStart::Failed(_))));
        assert!(notices.iter().any(|n| n.contains("was not started")));
    }

    /// The real embedded mod delivers through a production-bound room, then
    /// again after a session-report rebind through the same receiver.
    #[cfg(unix)]
    #[test]
    fn the_embedded_mod_delivers_through_a_report_rebind() {
        use std::process::Command;
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("SKIP: node is not on PATH, cannot run the embedded mod");
            return;
        }
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _claude_config = ClaudeConfigDirGuard::new();
        let (specs, mut starts, first) =
            claude_started_spec(root.path(), cwd.path(), "Claude · 1", _claude_config.path());
        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut starts)
                .unwrap()
                .is_empty()
        );
        let here = std::env::current_dir().unwrap();
        let mod_path = here.join("src/claude_mod/hooks/deliver.mjs");
        let driver = root.path().join("drive-mod.mjs");
        let marker = root.path().join("mod.log");
        fs::write(
            &driver,
            r#"import { appendFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
const [modPath, cwd, session, marker] = process.argv.slice(2);
// The real Claude mod sandbox has no `process`; hide it so a stray use fails here too.
const room = process.env.CROWDED_ROOM;
delete globalThis.process;
const { register } = await import(pathToFileURL(modPath).href);
const log = (t) => appendFileSync(marker, `${t}\n`);
const $ = {
  fs: { exists: async (p) => readFile(p).then(() => true, () => false), read: (p) => readFile(p, "utf8") },
  session: { id: async () => session },
  env: { get: async (name) => (name === "CROWDED_ROOM" ? room : undefined) },
  http: { fetch: async (url, init) => fetch(url, init).then(async (r) => ({ ok: r.ok, status: r.status, text: await r.text() })) },
  clock: { every: (ms, cb) => setInterval(() => { void cb(); }, ms) },
  ui: { status: (t) => log(`STATUS ${t}`) },
  prompt: { submit: async (input) => { log(`SUBMITTED ${input.text}`); return {}; } },
};
let handler = null;
register((event, fn) => { handler = fn; });
await handler($, { cwd, isInteractive: true }, async (e) => e);
"#,
        )
        .unwrap();
        let mut child = Command::new("node")
            .arg(&driver)
            .arg(mod_path)
            .arg(cwd.path())
            .arg(&first)
            .arg(&marker)
            .env("CROWDED_ROOM", "1")
            .stdout(Stdio::null())
            .spawn()
            .expect("node runs the driver");
        // The mod's own poll is the only clock; the bound only stops a broken
        // driver hanging the suite.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if fs::read_to_string(&marker)
                .unwrap_or_default()
                .contains("STATUS Crowded inbox connected")
            {
                break;
            }
            assert!(Instant::now() < deadline, "the mod never connected");
            thread::sleep(Duration::from_millis(20));
        }
        let mut mailroom = Mailroom::new(50);
        router
            .send(&mut mailroom, "Sender", 0, "first", None)
            .unwrap();
        pump_router(&mut router, &mut mailroom, |shown| {
            shown.contains("ACCEPTED by Claude (submitted; not proof it was acted on)")
        });
        const SECOND: &str = "3f8f8f8f-3333-4333-8333-333333333333";
        let generation = next_generation().unwrap();
        let title = "Claude · 1".to_owned();
        router
            .rebind_claude(0, title, cwd.path(), SECOND, generation)
            .unwrap();
        router
            .send(&mut mailroom, "Sender", 0, "second", None)
            .unwrap();
        pump_router(&mut router, &mut mailroom, |shown| {
            shown.matches("ACCEPTED by Claude (submitted;").count() == 2
        });
        child.kill().ok();
        let _ = child.wait();
        let log = fs::read_to_string(&marker).unwrap();
        for want in [
            "STATUS Crowded inbox connected",
            "SUBMITTED [whisper from Sender] first",
            "SUBMITTED [whisper from Sender] second",
        ] {
            assert!(log.contains(want), "{log}");
        }
    }

    /// Two fresh Claude rooms in one cwd deliver end-to-end through their own
    /// per-room bootstrap connections with no cross-delivery; reporting room 1's
    /// session promotes only room 1's bootstrap to its session file.
    #[cfg(unix)]
    #[test]
    fn two_fresh_claude_rooms_deliver_end_to_end_in_one_cwd() {
        use std::process::{Command, Stdio};
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("SKIP: node is not on PATH, cannot run the embedded mod");
            return;
        }
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let stub = codex_stub_for_pane(root.path(), "claude", "exec sleep 30");
        let mut specs = vec![
            claude_native_spec_for(&stub, cwd.path(), "Claude · 1"),
            claude_native_spec_for(&stub, cwd.path(), "Claude · 2"),
        ];
        let (mut starts, notices) = start_claude_sessions(&mut specs);
        assert!(notices.is_empty(), "{notices:?}");
        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut starts)
                .unwrap()
                .is_empty()
        );

        let boot1 = crate::claude_inbox::bootstrap_file(cwd.path(), 1).unwrap();
        let boot2 = crate::claude_inbox::bootstrap_file(cwd.path(), 2).unwrap();
        let c1: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&boot1).unwrap()).unwrap();
        let c2: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&boot2).unwrap()).unwrap();
        assert_ne!(
            c1["token"], c2["token"],
            "rooms must have unique bootstrap identities"
        );
        assert_ne!(boot1, boot2);
        assert!(
            !cwd.path()
                .join(".crowded/claude-inbox/_bootstrap.json")
                .exists()
        );

        let mod_path = std::env::current_dir()
            .unwrap()
            .join("src/claude_mod/hooks/deliver.mjs");
        let driver = root.path().join("drive-mod.mjs");
        fs::write(
            &driver,
            r#"import { appendFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
const [modPath, cwd, session, marker] = process.argv.slice(2);
// The real Claude mod sandbox has no `process`; hide it so a stray use fails here too.
const room = process.env.CROWDED_ROOM;
delete globalThis.process;
const { register } = await import(pathToFileURL(modPath).href);
const log = (t) => appendFileSync(marker, `${t}\n`);
const $ = {
  fs: { exists: async (p) => readFile(p).then(() => true, () => false), read: (p) => readFile(p, "utf8") },
  session: { id: async () => session },
  env: { get: async (name) => (name === "CROWDED_ROOM" ? room : undefined) },
  http: { fetch: async (url, init) => fetch(url, init).then(async (r) => ({ ok: r.ok, status: r.status, text: await r.text() })) },
  clock: { every: (ms, cb) => setInterval(() => { void cb(); }, ms) },
  ui: { status: (t) => log(`STATUS ${t}`) },
  prompt: { submit: async (input) => { log(`SUBMITTED ${input.text}`); return {}; } },
};
let handler = null;
register((event, fn) => { handler = fn; });
await handler($, { cwd, isInteractive: true }, async (e) => e);
"#,
        )
        .unwrap();
        let marker1 = root.path().join("room1.log");
        let marker2 = root.path().join("room2.log");
        // Session ids whose connection files do not exist force deliver.mjs to
        // resolve each room's per-room bootstrap via CROWDED_ROOM.
        let mut child1 = Command::new("node")
            .arg(&driver)
            .arg(&mod_path)
            .arg(cwd.path())
            .arg("pending-session-room-1")
            .arg(&marker1)
            .env("CROWDED_ROOM", "1")
            .stdout(Stdio::null())
            .spawn()
            .expect("node runs room 1 mod");
        let mut child2 = Command::new("node")
            .arg(&driver)
            .arg(&mod_path)
            .arg(cwd.path())
            .arg("pending-session-room-2")
            .arg(&marker2)
            .env("CROWDED_ROOM", "2")
            .stdout(Stdio::null())
            .spawn()
            .expect("node runs room 2 mod");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let a = fs::read_to_string(&marker1)
                .unwrap_or_default()
                .contains("STATUS Crowded inbox connected");
            let b = fs::read_to_string(&marker2)
                .unwrap_or_default()
                .contains("STATUS Crowded inbox connected");
            if a && b {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mods never connected: a={a} b={b}"
            );
            thread::sleep(Duration::from_millis(20));
        }

        let mut mailroom = Mailroom::new(50);
        router
            .send(&mut mailroom, "Sender", 0, "to-room-1", None)
            .unwrap();
        pump_router(&mut router, &mut mailroom, |shown| {
            shown.matches("ACCEPTED by Claude (submitted;").count() == 1
        });
        router
            .send(&mut mailroom, "Sender", 1, "to-room-2", None)
            .unwrap();
        pump_router(&mut router, &mut mailroom, |shown| {
            shown.matches("ACCEPTED by Claude (submitted;").count() == 2
        });
        let log1 = fs::read_to_string(&marker1).unwrap();
        let log2 = fs::read_to_string(&marker2).unwrap();
        assert!(
            log1.contains("SUBMITTED [whisper from Sender] to-room-1"),
            "{log1}"
        );
        assert!(
            !log1.contains("to-room-2"),
            "room 1 must not receive room 2 mail: {log1}"
        );
        assert!(
            log2.contains("SUBMITTED [whisper from Sender] to-room-2"),
            "{log2}"
        );
        assert!(
            !log2.contains("to-room-1"),
            "room 2 must not receive room 1 mail: {log2}"
        );

        // Report room 1's session: its bootstrap becomes its session file; room 2
        // keeps its own bootstrap and endpoint unchanged.
        session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
        let generation = next_generation().unwrap();
        router
            .rebind_claude(0, "Claude · 1".to_owned(), cwd.path(), MINTED_A, generation)
            .unwrap();
        assert!(
            !boot1.exists(),
            "room 1 bootstrap must be removed on report"
        );
        assert!(
            crate::claude_inbox::connection_file(cwd.path(), MINTED_A)
                .unwrap()
                .exists(),
            "room 1 session connection file must exist"
        );
        assert!(boot2.exists(), "room 2 bootstrap must remain untouched");
        let c2_after: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&boot2).unwrap()).unwrap();
        assert_eq!(c2, c2_after, "room 2 endpoint must be unchanged");

        router
            .send(&mut mailroom, "Sender", 0, "to-room-1-after-report", None)
            .unwrap();
        pump_router(&mut router, &mut mailroom, |shown| {
            shown.matches("ACCEPTED by Claude (submitted;").count() == 3
        });
        let log1 = fs::read_to_string(&marker1).unwrap();
        let log2 = fs::read_to_string(&marker2).unwrap();
        assert!(
            log1.contains("SUBMITTED [whisper from Sender] to-room-1-after-report"),
            "{log1}"
        );
        assert!(
            !log2.contains("to-room-1-after-report"),
            "room 2 must not receive room 1 mail: {log2}"
        );

        child1.kill().ok();
        child2.kill().ok();
        let _ = child1.wait();
        let _ = child2.wait();
    }

    /// A stale session file for another room must not be used: in a room-2
    /// environment, deliver.mjs must refuse a connection file whose room does
    /// not match and must never reach/register with room 1.
    #[cfg(unix)]
    #[test]
    fn deliver_refuses_a_session_file_owned_by_another_room() {
        use std::process::{Command, Stdio};
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("SKIP: node is not on PATH, cannot run the embedded mod");
            return;
        }
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _claude_config = ClaudeConfigDirGuard::new();
        let (specs, mut starts, session1) =
            claude_started_spec(root.path(), cwd.path(), "Claude · 1", _claude_config.path());
        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut starts)
                .unwrap()
                .is_empty()
        );

        let mod_path = std::env::current_dir()
            .unwrap()
            .join("src/claude_mod/hooks/deliver.mjs");
        let driver = root.path().join("drive-mod.mjs");
        fs::write(
            &driver,
            r#"import { appendFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
const [modPath, cwd, session, marker] = process.argv.slice(2);
// The real Claude mod sandbox has no `process`; hide it so a stray use fails here too.
const room = process.env.CROWDED_ROOM;
delete globalThis.process;
const { register } = await import(pathToFileURL(modPath).href);
const log = (t) => appendFileSync(marker, `${t}\n`);
const $ = {
  fs: { exists: async (p) => readFile(p).then(() => true, () => false), read: (p) => readFile(p, "utf8") },
  session: { id: async () => session },
  env: { get: async (name) => (name === "CROWDED_ROOM" ? room : undefined) },
  http: { fetch: async (url, init) => fetch(url, init).then(async (r) => ({ ok: r.ok, status: r.status, text: await r.text() })) },
  clock: { every: (ms, cb) => setInterval(() => { void cb(); }, ms) },
  ui: { status: (t) => log(`STATUS ${t}`) },
  prompt: { submit: async (input) => { log(`SUBMITTED ${input.text}`); return {}; } },
};
let handler = null;
register((event, fn) => { handler = fn; });
await handler($, { cwd, isInteractive: true }, async (e) => e);
"#,
        )
        .unwrap();

        // Legitimate room 1 mod connected to its own session file.
        let marker1 = root.path().join("room1.log");
        let mut child1 = Command::new("node")
            .arg(&driver)
            .arg(&mod_path)
            .arg(cwd.path())
            .arg(&session1)
            .arg(&marker1)
            .env("CROWDED_ROOM", "1")
            .stdout(Stdio::null())
            .spawn()
            .expect("node runs room 1 mod");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if fs::read_to_string(&marker1)
                .unwrap_or_default()
                .contains("STATUS Crowded inbox connected")
            {
                break;
            }
            assert!(Instant::now() < deadline, "room 1 mod never connected");
            thread::sleep(Duration::from_millis(20));
        }

        // Rogue room-2 environment given room 1's session id: must refuse the
        // mismatched session file and never reach/register with room 1.
        let marker2 = root.path().join("room2.log");
        let mut child2 = Command::new("node")
            .arg(&driver)
            .arg(&mod_path)
            .arg(cwd.path())
            .arg(&session1)
            .arg(&marker2)
            .env("CROWDED_ROOM", "2")
            .stdout(Stdio::null())
            .spawn()
            .expect("node runs rogue mod");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let log = fs::read_to_string(&marker2).unwrap_or_default();
            if log.contains("STATUS Crowded inbox not connected") {
                break;
            }
            assert!(Instant::now() < deadline, "rogue mod never refused: {log}");
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(300));
        let rogue = fs::read_to_string(&marker2).unwrap_or_default();
        assert!(
            !rogue.contains("STATUS Crowded inbox connected"),
            "rogue mod must not connect: {rogue}"
        );
        assert!(
            !rogue.contains("SUBMITTED"),
            "rogue mod must not submit: {rogue}"
        );

        // Room 1 still receives its own mail through its own connection.
        let mut mailroom = Mailroom::new(50);
        router
            .send(&mut mailroom, "Sender", 0, "only-room-1", None)
            .unwrap();
        pump_router(&mut router, &mut mailroom, |shown| {
            shown.contains("ACCEPTED by Claude (submitted; not proof it was acted on)")
        });
        let log1 = fs::read_to_string(&marker1).unwrap();
        assert!(
            log1.contains("SUBMITTED [whisper from Sender] only-room-1"),
            "{log1}"
        );

        child1.kill().ok();
        child2.kill().ok();
        let _ = child1.wait();
        let _ = child2.wait();
    }

    /// With a connection file present, deliver.mjs must fail closed (no
    /// registration) when CROWDED_ROOM is missing or non-numeric.
    #[cfg(unix)]
    #[test]
    fn deliver_fails_closed_without_a_valid_crowded_room() {
        use std::process::{Command, Stdio};
        if Command::new("node").arg("--version").output().is_err() {
            eprintln!("SKIP: node is not on PATH, cannot run the embedded mod");
            return;
        }
        let _state = session_state::StateRootGuard::isolated();
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _claude_config = ClaudeConfigDirGuard::new();
        let (specs, mut starts, session1) =
            claude_started_spec(root.path(), cwd.path(), "Claude · 1", _claude_config.path());
        let mut router = crate::native_route::NativeRouter::start(
            session_state::state_directory().join("native-outbox.json"),
            Vec::new(),
        )
        .unwrap();
        assert!(
            bind_claude_rooms(&mut router, &specs, &mut starts)
                .unwrap()
                .is_empty()
        );

        let mod_path = std::env::current_dir()
            .unwrap()
            .join("src/claude_mod/hooks/deliver.mjs");
        let driver = root.path().join("drive-mod.mjs");
        fs::write(
            &driver,
            r#"import { appendFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";
const [modPath, cwd, session, marker] = process.argv.slice(2);
// The real Claude mod sandbox has no `process`; hide it so a stray use fails here too.
const room = process.env.CROWDED_ROOM;
delete globalThis.process;
const { register } = await import(pathToFileURL(modPath).href);
const log = (t) => appendFileSync(marker, `${t}\n`);
const $ = {
  fs: { exists: async (p) => readFile(p).then(() => true, () => false), read: (p) => readFile(p, "utf8") },
  session: { id: async () => session },
  env: { get: async (name) => (name === "CROWDED_ROOM" ? room : undefined) },
  http: { fetch: async (url, init) => fetch(url, init).then(async (r) => ({ ok: r.ok, status: r.status, text: await r.text() })) },
  clock: { every: (ms, cb) => setInterval(() => { void cb(); }, ms) },
  ui: { status: (t) => log(`STATUS ${t}`) },
  prompt: { submit: async (input) => { log(`SUBMITTED ${input.text}`); return {}; } },
};
let handler = null;
register((event, fn) => { handler = fn; });
await handler($, { cwd, isInteractive: true }, async (e) => e);
"#,
        )
        .unwrap();

        for (name, env_value) in [("missing", None), ("non-numeric", Some("abc"))] {
            let marker = root.path().join(format!("{name}.log"));
            let mut cmd = Command::new("node");
            cmd.arg(&driver)
                .arg(&mod_path)
                .arg(cwd.path())
                .arg(&session1)
                .arg(&marker)
                .stdout(Stdio::null());
            match env_value {
                None => {
                    cmd.env_remove("CROWDED_ROOM");
                }
                Some(value) => {
                    cmd.env("CROWDED_ROOM", value);
                }
            }
            let mut child = cmd.spawn().expect("node runs mod");
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let log = fs::read_to_string(&marker).unwrap_or_default();
                if log.contains("STATUS Crowded inbox not connected") {
                    break;
                }
                assert!(Instant::now() < deadline, "{name}: never refused: {log}");
                thread::sleep(Duration::from_millis(20));
            }
            thread::sleep(Duration::from_millis(200));
            let log = fs::read_to_string(&marker).unwrap_or_default();
            assert!(
                !log.contains("STATUS Crowded inbox connected"),
                "{name}: must not connect: {log}"
            );
            child.kill().ok();
            let _ = child.wait();
        }
        let _ = router; // keep inbox alive
    }

    #[cfg(unix)]
    mod production_loop {
        use super::*;
        use crate::app::{DoorbellSource, EventSource, Notices};
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

        use ratatui::{Terminal, backend::TestBackend};
        use std::{
            collections::VecDeque,
            fs, io,
            path::{Path, PathBuf},
            time::{Duration, Instant},
        };

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
                Self {
                    steps: steps.into(),
                }
            }
        }

        impl EventSource for ScriptedEvents {
            fn poll(&mut self, _timeout: Duration) -> io::Result<bool> {
                match self.steps.front() {
                    Some(ScriptedStep::Event(_)) => Ok(true),
                    Some(ScriptedStep::Idle) => {
                        self.steps.pop_front();
                        Ok(false)
                    }
                    None => Ok(false),
                }
            }

            fn read(&mut self) -> io::Result<Event> {
                match self.steps.pop_front() {
                    Some(ScriptedStep::Event(event)) => Ok(event),
                    _ => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "no scripted event",
                    )),
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

        fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done() {
                assert!(Instant::now() < deadline, "{what} never happened");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn the_production_loop_forwards_an_ordinary_key_only_to_the_focused_pane() {
            let cwd = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let (pane0, captured0) = pane_with_capture(4, 20);
            let (pane1, captured1) = pane_with_capture(4, 20);
            let mut panes = vec![pane0, pane1];
            let mut router = None;
            let mut codex = vec![None, None];
            let environments = vec![GuestEnvironment::new([]), GuestEnvironment::new([])];
            let now = Instant::now();
            let mut spawned_at = vec![now, now];
            let mut last_output = vec![None, None];
            let mut pane_sizes = vec![small_size(), small_size()];
            let mut notice = Notices::default();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            // Tab moves focus from 0 to 1; 'a' must reach only pane 1.
            let mut events = ScriptedEvents::new(vec![
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('a'), KeyModifiers::NONE),
                key(KeyCode::Char('q'), KeyModifiers::CONTROL),
            ]);
            let mut doorbell = SilentDoorbell::new(root.path().join("doorbell.sock"));

            crate::app::run_loop(
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
                &[false, false],
                2,
            )
            .unwrap();

            wait_for_captured(&captured1, b"a");
            assert!(
                captured0.lock().unwrap().is_empty(),
                "the unfocused pane must receive nothing: {:?}",
                captured0.lock().unwrap()
            );
            let _ = cwd;
        }

        #[test]
        fn the_production_loop_forwards_online_ctrl_r_as_0x12_to_the_focused_pane() {
            let root = tempfile::tempdir().unwrap();
            let (pane0, captured0) = pane_with_capture(4, 20);
            let (mut pane1, captured1) = pane_with_capture(4, 20);
            pane1.spec.program = "codex".into();
            pane1.spec.native_delivery = true;
            assert!(pane1.is_online(), "the focused codex pane must be online");
            let mut panes = vec![pane0, pane1];
            let mut router = None;
            let mut codex = vec![None, None];
            let environments = vec![GuestEnvironment::new([]), GuestEnvironment::new([])];
            let now = Instant::now();
            let mut spawned_at = vec![now, now];
            let mut last_output = vec![None, None];
            let mut pane_sizes = vec![small_size(), small_size()];
            let mut notice = Notices::default();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut events = ScriptedEvents::new(vec![
                key(KeyCode::Tab, KeyModifiers::NONE),
                key(KeyCode::Char('r'), KeyModifiers::CONTROL),
                key(KeyCode::Char('q'), KeyModifiers::CONTROL),
            ]);
            let mut doorbell = SilentDoorbell::new(root.path().join("doorbell.sock"));

            crate::app::run_loop(
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
                &[false, false],
                2,
            )
            .unwrap();

            wait_for_captured(&captured1, b"\x12");
            assert!(
                captured0.lock().unwrap().is_empty(),
                "the unfocused pane must receive nothing: {:?}",
                captured0.lock().unwrap()
            );
            assert!(
                notice.ordered_lines().is_empty(),
                "an online Ctrl+R must not restart or emit notices: {:?}",
                notice.ordered_lines()
            );
        }

        #[test]
        fn the_production_loop_relaunches_only_the_focused_claude_room_on_f5() {
            let _state = session_state::StateRootGuard::isolated();
            let _claude_config = super::ClaudeConfigDirGuard::new();
            let cwd = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let stub_root0 = tempfile::tempdir().unwrap();
            let stub_root1 = tempfile::tempdir().unwrap();
            let log0 = stub_root0.path().join("claude.log");
            let log1 = stub_root1.path().join("claude.log");
            let stub0 = codex_stub_for_pane(
                stub_root0.path(),
                "claude",
                &format!("printf 'exec\\n' >> '{}'\nexec sleep 30", log0.display()),
            );
            let stub1 = codex_stub_for_pane(
                stub_root1.path(),
                "claude",
                &format!("printf 'exec\\n' >> '{}'\nexec sleep 30", log1.display()),
            );
            session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
            session_state::upsert_reported("claude", cwd.path(), "Claude · 2", MINTED_B).unwrap();
            super::materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
            super::materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_B);
            let mut panes = vec![
                Pane::spawn_unavailable(
                    claude_native_spec_for(&stub0, cwd.path(), "Claude · 1"),
                    small_size(),
                    GuestEnvironment::new([]),
                    "offline",
                )
                .unwrap(),
                Pane::spawn_unavailable(
                    claude_native_spec_for(&stub1, cwd.path(), "Claude · 2"),
                    small_size(),
                    GuestEnvironment::new([]),
                    "offline",
                )
                .unwrap(),
            ];
            let mut router = Some(
                crate::native_route::NativeRouter::start(
                    root.path().join("native-outbox.json"),
                    Vec::new(),
                )
                .unwrap(),
            );
            {
                let router = router.as_mut().unwrap();
                bind_claude_room(router, &mut panes, 0, MINTED_A).unwrap();
                bind_claude_room(router, &mut panes, 1, MINTED_B).unwrap();
            }
            let gen0 = router.as_ref().unwrap().binding_for(0).unwrap().generation;
            let gen1 = router.as_ref().unwrap().binding_for(1).unwrap().generation;
            let mut codex = vec![None, None];
            let environments = vec![GuestEnvironment::new([]), GuestEnvironment::new([])];
            let now = Instant::now();
            let mut spawned_at = vec![now, now];
            let mut last_output = vec![None, None];
            let mut pane_sizes = vec![small_size(), small_size()];
            let mut notice = Notices::default();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            // Tab to focus room 1, F5 restarts focused room only, then quit.
            let mut events = ScriptedEvents::steps(vec![
                ScriptedStep::Event(key(KeyCode::Tab, KeyModifiers::NONE)),
                ScriptedStep::Event(key(KeyCode::F(5), KeyModifiers::NONE)),
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Event(key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ]);
            let mut doorbell = SilentDoorbell::new(root.path().join("doorbell.sock"));

            crate::app::run_loop(
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
                &[true, true],
                2,
            )
            .unwrap();

            let shown = notice.ordered_lines().join("; ");
            assert!(shown.contains("Claude · 2 restarted"), "{shown}");
            assert!(!shown.contains("Claude · 1 restarted"), "{shown}");
            assert_eq!(
                router.as_ref().unwrap().binding_for(0).unwrap().generation,
                gen0
            );
            assert!(router.as_ref().unwrap().binding_for(1).unwrap().generation > gen1);
            let args1: Vec<String> = panes[1]
                .room_spec()
                .args
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            assert!(
                args1.windows(2).any(|pair| {
                    (pair[0] == "--session-id" || pair[0] == "--resume")
                        && pair[1] == router.as_ref().unwrap().binding_for(1).unwrap().session
                }),
                "room 1 must relaunch on its bound session: {args1:?}"
            );
            let args0: Vec<String> = panes[0]
                .room_spec()
                .args
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            assert!(
                !args0.iter().any(|arg| arg == "exec"),
                "room 0 must stay untouched: {args0:?}"
            );
            assert_eq!(
                fs::read_to_string(&log0).unwrap_or_default(),
                "",
                "room 0 must not be launched"
            );
        }

        #[test]
        fn the_production_loop_relaunches_only_the_focused_offline_codex_room_on_ctrl_r() {
            let _state = session_state::StateRootGuard::isolated();
            let cwd = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let stub_root0 = tempfile::tempdir().unwrap();
            let stub_root1 = tempfile::tempdir().unwrap();
            let log0 = stub_root0.path().join("codex.calls");
            let log1 = stub_root1.path().join("codex.calls");
            let stub0 = gated_codex_stub(
                stub_root0.path(),
                &log0,
                &stub_root0.path().join("gate"),
                MINTED_B,
            );
            let stub1 = gated_codex_stub(
                stub_root1.path(),
                &log1,
                &stub_root1.path().join("gate"),
                MINTED_C,
            );
            // Room A has a trusted stored session so a restart would resume it;
            // room B has none, so the focused restart must mint a new one.
            session_state::upsert_minted("codex", cwd.path(), "Room A", MINTED_A).unwrap();
            let mut panes = vec![
                Pane::spawn_unavailable(
                    codex_mint_spec(&stub0, cwd.path(), "Room A", MINTED_B),
                    small_size(),
                    GuestEnvironment::new([]),
                    "offline",
                )
                .unwrap(),
                Pane::spawn_unavailable(
                    codex_mint_spec(&stub1, cwd.path(), "Room B", MINTED_C),
                    small_size(),
                    GuestEnvironment::new([]),
                    "offline",
                )
                .unwrap(),
            ];
            let mut router = Some(
                crate::native_route::NativeRouter::start(
                    root.path().join("native-outbox.json"),
                    Vec::new(),
                )
                .unwrap(),
            );
            let gen0 =
                crate::native_launch::GenerationAllocator::new(session_state::state_directory())
                    .next()
                    .unwrap();
            let gen1 = gen0 + 1;
            {
                use crate::native_route::CodexEntry;
                router
                    .as_mut()
                    .unwrap()
                    .bind_codex(vec![
                        CodexEntry {
                            index: 0,
                            title: "Room A".into(),
                            room: Some(IterationHarness::healthy_room(
                                &stub0,
                                cwd.path(),
                                MINTED_A,
                            )),
                            generation: gen0,
                            reason: None,
                        },
                        CodexEntry {
                            index: 1,
                            title: "Room B".into(),
                            room: Some(IterationHarness::healthy_room(
                                &stub1,
                                cwd.path(),
                                MINTED_B,
                            )),
                            generation: gen1,
                            reason: None,
                        },
                    ])
                    .unwrap();
            }
            let mut codex = vec![None, None];
            let environments = vec![GuestEnvironment::new([]), GuestEnvironment::new([])];
            let now = Instant::now();
            let mut spawned_at = vec![now, now];
            let mut last_output = vec![None, None];
            let mut pane_sizes = vec![small_size(), small_size()];
            let mut notice = Notices::default();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut events = ScriptedEvents::steps(vec![
                ScriptedStep::Event(key(KeyCode::Tab, KeyModifiers::NONE)),
                ScriptedStep::Event(key(KeyCode::Char('r'), KeyModifiers::CONTROL)),
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Event(key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ]);
            let mut doorbell = SilentDoorbell::new(root.path().join("doorbell.sock"));

            crate::app::run_loop(
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
                &[true, true],
                2,
            )
            .unwrap();

            let launched = |path: &Path| {
                fs::read_to_string(path)
                    .unwrap_or_default()
                    .lines()
                    .filter(|line| line.starts_with("exec"))
                    .count()
            };
            wait_until("room 1 relaunched", || launched(&log1) >= 1);
            assert_eq!(launched(&log1), 1, "room 1 must relaunch exactly once");
            assert_eq!(launched(&log0), 0, "room 0 must stay untouched");
            let shown = notice.ordered_lines().join("; ");
            assert!(shown.contains("Room B restarted"), "{shown}");
            assert!(!shown.contains("Room A restarted"), "{shown}");
            assert_eq!(
                router.as_ref().unwrap().binding_for(0).unwrap().generation,
                gen0
            );
            let prompt = fs::read_to_string(&log1).unwrap_or_default();
            assert!(
                prompt.contains("Automatic delivery pauses after 100 successful messages"),
                "the loop must pass its fuse size to the relaunch: {prompt}"
            );
            assert!(
                pane_sizes[1].rows > 0 && pane_sizes[1].cols > 0,
                "focused pane sized by the loop: {:?}",
                pane_sizes[1]
            );
        }
        /// The loop must size the focused pane it restarts, not pane 0. With
        /// three rooms the bottom pane spans the full width, so a size taken
        /// from pane 0 is visibly wrong.
        #[test]
        fn the_production_loop_sizes_the_focused_pane_not_pane_0_on_restart() {
            let _state = session_state::StateRootGuard::isolated();
            let _claude_config = super::ClaudeConfigDirGuard::new();
            let cwd = tempfile::tempdir().unwrap();
            let root = tempfile::tempdir().unwrap();
            let stub_root0 = tempfile::tempdir().unwrap();
            let stub_root1 = tempfile::tempdir().unwrap();
            let stub_root2 = tempfile::tempdir().unwrap();
            let stub0 = codex_stub_for_pane(stub_root0.path(), "claude", "exec sleep 30");
            let stub1 = codex_stub_for_pane(stub_root1.path(), "claude", "exec sleep 30");
            let stub2 = codex_stub_for_pane(stub_root2.path(), "claude", "exec sleep 30");
            session_state::upsert_reported("claude", cwd.path(), "Claude · 1", MINTED_A).unwrap();
            session_state::upsert_reported("claude", cwd.path(), "Claude · 2", MINTED_B).unwrap();
            session_state::upsert_reported("claude", cwd.path(), "Claude · 3", MINTED_C).unwrap();
            super::materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_A);
            super::materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_B);
            super::materialize_claude_session(_claude_config.path(), cwd.path(), MINTED_C);
            let mut panes = vec![
                Pane::spawn_unavailable(
                    claude_native_spec_for(&stub0, cwd.path(), "Claude · 1"),
                    small_size(),
                    GuestEnvironment::new([]),
                    "offline",
                )
                .unwrap(),
                Pane::spawn_unavailable(
                    claude_native_spec_for(&stub1, cwd.path(), "Claude · 2"),
                    small_size(),
                    GuestEnvironment::new([]),
                    "offline",
                )
                .unwrap(),
                Pane::spawn_unavailable(
                    claude_native_spec_for(&stub2, cwd.path(), "Claude · 3"),
                    small_size(),
                    GuestEnvironment::new([]),
                    "offline",
                )
                .unwrap(),
            ];
            let mut router = Some(
                crate::native_route::NativeRouter::start(
                    root.path().join("native-outbox.json"),
                    Vec::new(),
                )
                .unwrap(),
            );
            {
                let router = router.as_mut().unwrap();
                bind_claude_room(router, &mut panes, 0, MINTED_A).unwrap();
                bind_claude_room(router, &mut panes, 1, MINTED_B).unwrap();
                bind_claude_room(router, &mut panes, 2, MINTED_C).unwrap();
            }
            let mut codex = vec![None, None, None];
            let environments = vec![
                GuestEnvironment::new([]),
                GuestEnvironment::new([]),
                GuestEnvironment::new([]),
            ];
            let now = Instant::now();
            let mut spawned_at = vec![now, now, now];
            let mut last_output = vec![None, None, None];
            let mut pane_sizes = vec![small_size(), small_size(), small_size()];
            let mut notice = Notices::default();
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            let mut events = ScriptedEvents::steps(vec![
                ScriptedStep::Event(key(KeyCode::Tab, KeyModifiers::NONE)),
                ScriptedStep::Event(key(KeyCode::Tab, KeyModifiers::NONE)),
                ScriptedStep::Event(key(KeyCode::F(5), KeyModifiers::NONE)),
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Idle,
                ScriptedStep::Event(key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ]);
            let mut doorbell = SilentDoorbell::new(root.path().join("doorbell.sock"));

            crate::app::run_loop(
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
                &[true, true, true],
                3,
            )
            .unwrap();

            let shown = notice.ordered_lines().join("; ");
            assert!(shown.contains("Claude · 3 restarted"), "{shown}");
            // The bottom room spans the full rooms area; it must not inherit the
            // narrower top-left pane's width when restarted.
            assert!(
                panes[2].visible_width() > panes[0].visible_width(),
                "focused pane must keep its own width, not pane 0's: pane0={} pane2={}",
                panes[0].visible_width(),
                panes[2].visible_width()
            );
        }
    }
}
