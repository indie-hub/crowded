//! One room: its guest command, PTY, terminal screen, and lifecycle.

use std::{
    env,
    ffi::{OsStr, OsString},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, SystemTime},
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{Child, MasterPty, PtySize, native_pty_system};
use tui_term::vt100::{Parser, Screen};

use crate::{
    command::{ResolvedCommand, headroom_on_path},
    config::{RoomSpec, Transport},
};

mod controls;
mod session_state;

const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";
// ponytail: ConPTY only needs a valid DSR here; report the parser's exact
// cursor if a guest later depends on cursor-aware terminal negotiation.
const CURSOR_POSITION_RESPONSE: &[u8] = b"\x1b[1;1R";

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

/// Apply each guest's "resume most recent conversation" flag ahead of
/// spawn, for the `crowded resume` CLI entry point. Guests without a
/// Conductor adapter (shell rooms, unrecognized programs) are left
/// unchanged rather than failing the whole launch over one room.
///
/// Returns, per spec, whether resume args were actually applied. The
/// caller uses this to tell which panes genuinely resumed and can skip
/// their intro whisper.
pub(crate) fn resume_supported_specs(specs: &mut [RoomSpec]) -> Vec<bool> {
    specs
        .iter_mut()
        .map(|spec| controls::add_resume_args(spec).is_ok())
        .collect()
}

fn opencode_input_ready(screen: &str) -> bool {
    // Resumed sessions show prior conversation history in the viewport.
    // Checking the entire screen for "esc interrupt" is too strict: history
    // may contain that phrase from a previous thinking turn or from code
    // in the transcript, even though the current prompt is idle. Only
    // the prompt area (tail of the screen) matters, and only the busy
    // indicators near the last prompt matter.
    let tail = screen
        .get(screen.len().saturating_sub(1200)..)
        .unwrap_or(screen);
    let lines: Vec<&str> = tail.lines().collect();
    // Narrow panes wrap the marker phrase itself across a line boundary
    // (e.g. "...ctrl+p" / "commands..."), so a single line's contents can't
    // be trusted alone. Join each line with the next one (space-separated,
    // collapsing the wrap) before searching for the marker.
    let prompt_idx = (0..lines.len()).rposition(|i| {
        let joined = lines[i..(i + 3).min(lines.len())].join(" ");
        joined.contains("Ask anything")
            || (joined.contains("ctrl+p") && joined.contains("commands"))
    });
    let Some(idx) = prompt_idx else {
        return false;
    };
    // Prompt must be near the bottom of the visible tail. A prompt
    // far above (e.g. old history with Ask anything at the top) must
    // not count; the current idle prompt lives at the bottom.
    if lines.len() - idx > 6 {
        return false;
    }
    // Only the prompt line and the next couple of lines can contain the
    // busy indicators when the UI is actually busy. History's "esc
    // interrupt" far above the prompt must not block readiness.
    let window = &lines[idx..(idx + 3).min(lines.len())];
    let prompt_area = window.join("\n");
    !prompt_area.contains("esc interrupt") && !prompt_area.contains("exit shell mode")
}

#[cfg(not(windows))]
const RAW_SUBMIT_DELAY: Duration = Duration::from_millis(150);
#[cfg(windows)]
const RAW_SUBMIT_DELAY: Duration = Duration::from_secs(1);

fn whisper_parts(
    transport: Transport,
    bracketed_paste: bool,
    source: &str,
    message: &str,
) -> (Vec<u8>, Option<Vec<u8>>) {
    let note = format!("[whisper from {source}] {message}");
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
            Some(vec![b'\r']),
        ),
        // Other raw TUIs may classify a rapid prompt+Enter sequence as one paste.
        Transport::Raw => (note.into_bytes(), Some(vec![b'\r'])),
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
    writer: Box<dyn Write + Send>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    response_tail: Vec<u8>,
    parser: Parser,
    headroom_active: bool,
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

impl Pane {
    pub(crate) fn spawn(
        spec: RoomSpec,
        size: PtySize,
        environment: GuestEnvironment,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // A PTY has two ends: the child receives the slave; we keep the master.
        let pty = native_pty_system().openpty(size)?;
        // portable-pty defaults an omitted cwd to HOME rather than inheriting
        // Crowded's directory, so the project directory must be explicit.
        let cwd = working_directory(spec.cwd.as_deref())?;
        let path = environment_value(&spec.variables, "PATH");
        let path_ext = environment_value(&spec.variables, "PATHEXT");
        let (program, args, headroom_active) = headroom_launch(
            &spec.program,
            &spec.args,
            spec.use_headroom,
            &spec.headroom_args,
            &path,
            &path_ext,
        );
        let launch =
            ResolvedCommand::resolve_with_environment(&program, &args, &cwd, path, path_ext)?
                .portable()?;
        let (mut command, tree) = launch.into_parts();
        command.cwd(&cwd);
        command.env("PWD", &cwd);
        for (key, value) in &spec.variables {
            command.env(key, value);
        }
        for (key, value) in &environment.variables {
            command.env(key, value);
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
        let writer = pty.master.take_writer()?;
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

        Ok(Self {
            spec,
            cwd,
            spawned_at,
            captured_session: session_state::fresh_capture_cell(),
            environment,
            child,
            master: pty.master,
            writer,
            output_rx,
            response_tail: Vec::new(),
            // vt100 assumes room for wrapping and a double-width character.
            parser: Parser::new(size.rows.max(2), size.cols.max(2), 0),
            headroom_active,
        })
    }

    /// Best-effort capture of this pane's exact underlying vendor session id,
    /// anchored to this pane's own process spawn instant (not the intro-sent
    /// event) so a `since` taken later never wrongly rejects an artifact this
    /// spawn already wrote. Runs on a background thread bounded to
    /// `session_state::SESSION_CAPTURE_GRACE` (OpenCode uses its own, longer
    /// bound), so it never blocks the UI loop.
    /// Resumed panes skip the intro and therefore never call this.
    pub(crate) fn begin_session_capture(&self) {
        let Ok(vendor) = controls::cli_vendor(&self.spec) else {
            return;
        };
        // The room identity (spec.title) keys the persisted entry so this room
        // only ever resumes its own captured id, even when a sibling room
        // shares the same (vendor, cwd). The `since` anchor is this pane's
        // spawn instant: a ClearContext/Configure/restart respawns via
        // `spawn` (fresh `spawned_at`), so a new capture supersedes the stale
        // pre-clear id; a resume skips the intro and never gets here.
        session_state::capture_async(
            vendor,
            self.cwd.clone(),
            self.spec.title.clone(),
            self.spawned_at,
            Arc::clone(&self.captured_session),
        );
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
            respond_to_terminal_queries(&mut *self.writer, &mut self.response_tail, &bytes)?;
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
        if !self.is_online() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "room is offline",
            ));
        }
        self.writer.write_all(bytes)?;
        self.writer.flush()
    }

    pub(crate) fn is_online(&self) -> bool {
        self.child.child.is_some()
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

    pub(crate) fn headroom_active(&self) -> bool {
        self.headroom_active
    }

    pub(crate) fn needs_intro(&self) -> bool {
        self.spec.transport == Transport::Raw
    }

    pub(crate) fn screen(&self) -> &Screen {
        self.parser.screen()
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

    pub(crate) fn send_whisper(&mut self, source: &str, message: &str) -> io::Result<()> {
        let bracketed_paste = controls::uses_bracketed_paste(&self.spec);
        let (body, submit) = whisper_parts(self.spec.transport, bracketed_paste, source, message);
        self.write_bytes(&body)?;
        if let Some(submit) = submit {
            // Raw TUIs can keep Enter in paste/newline mode briefly after input.
            // ponytail: this briefly blocks the UI; schedule it asynchronously
            // only if a measured 150 ms pause becomes noticeable.
            thread::sleep(RAW_SUBMIT_DELAY);
            self.write_bytes(&submit)?;
        }
        Ok(())
    }

    pub(crate) fn poll_exit(&mut self) -> io::Result<()> {
        self.child.poll_exit().map(|_| ())
    }

    pub(crate) fn clear_context(
        &mut self,
        size: PtySize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.reconfigure(size, controls::clear_resume_args)
    }

    pub(crate) fn resume_context(
        &mut self,
        size: PtySize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.reconfigure(size, controls::add_resume_args)
    }

    pub(crate) fn configure(
        &mut self,
        model: Option<&str>,
        effort: Option<&str>,
        size: PtySize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.reconfigure(size, |spec| {
            if let Some(model) = model {
                controls::set_model(spec, model)?;
            }
            if let Some(effort) = effort {
                controls::set_effort(spec, effort)?;
            }
            Ok(())
        })
    }

    pub(crate) fn current_model(&self) -> Option<String> {
        controls::current_model(&self.spec)
    }

    pub(crate) fn current_effort(&self) -> Option<String> {
        controls::current_effort(&self.spec)
    }

    fn reconfigure(
        &mut self,
        size: PtySize,
        configure: impl FnOnce(&mut RoomSpec) -> io::Result<()>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut spec = self.spec.clone();
        configure(&mut spec)?;
        let replacement = Self::spawn(spec, size, self.environment.clone())?;
        self.cleanup();
        *self = replacement;
        Ok(())
    }

    pub(crate) fn restart(&mut self, size: PtySize) -> Result<(), Box<dyn std::error::Error>> {
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
    use std::{
        ffi::OsStr,
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

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
            headroom_args: Vec::new(),
        }
    }

    fn arguments(spec: &RoomSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
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
        assert_eq!(submit, Some(vec![b'\r']));

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
        assert_eq!(submit, Some(vec![b'\r']));
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
}
