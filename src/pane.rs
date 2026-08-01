//! One room: its guest command, PTY, terminal screen, and lifecycle.

use std::{
    env,
    ffi::OsString,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tui_term::vt100::{Parser, Screen};

use crate::config::{RoomSpec, Transport};

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

fn opencode_input_ready(screen: &str) -> bool {
    (screen.contains("Ask anything") || screen.contains("ctrl+p commands"))
        && !screen.contains("esc interrupt")
        && !screen.contains("exit shell mode")
}

const RAW_SUBMIT_DELAY: Duration = Duration::from_millis(150);

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
        // Codex's native paste event avoids long messages swallowing Enter as a newline.
        Transport::Raw if bracketed_paste => (
            format!("\x1b[200~{note}\x1b[201~").into_bytes(),
            Some(vec![b'\r']),
        ),
        // Other raw TUIs may classify a rapid prompt+Enter sequence as one paste.
        Transport::Raw => (note.into_bytes(), Some(vec![b'\r'])),
    }
}

#[derive(Clone, Copy)]
enum CliVendor {
    Claude,
    Codex,
    OpenCode,
}

fn cli_vendor(spec: &RoomSpec) -> io::Result<CliVendor> {
    if spec.transport != Transport::Raw {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal rooms do not support agent controls",
        ));
    }
    let guest = Path::new(spec.program.as_os_str())
        .file_name()
        .unwrap_or(spec.program.as_os_str())
        .to_string_lossy();
    match guest.to_ascii_lowercase().as_str() {
        "claude" => Ok(CliVendor::Claude),
        "codex" => Ok(CliVendor::Codex),
        "opencode" => Ok(CliVendor::OpenCode),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{guest} has no Conductor adapter"),
        )),
    }
}

fn replace_option(args: &mut Vec<OsString>, aliases: &[&str], option: &str, value: &str) {
    let mut kept = Vec::with_capacity(args.len() + 2);
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if aliases.iter().any(|alias| argument == *alias) {
            index += 1;
            if index < args.len() {
                index += 1;
            }
            continue;
        }
        if aliases
            .iter()
            .any(|alias| argument.starts_with(&format!("{alias}=")))
        {
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    args.clear();
    args.push(option.into());
    args.push(value.into());
    args.append(&mut kept);
}

fn replace_codex_effort(args: &mut Vec<OsString>, effort: &str) {
    let mut kept = Vec::with_capacity(args.len() + 2);
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if matches!(argument.as_ref(), "-c" | "--config")
            && args.get(index + 1).is_some_and(|value| {
                value
                    .to_string_lossy()
                    .starts_with("model_reasoning_effort=")
            })
        {
            index += 2;
            continue;
        }
        if argument.starts_with("--config=model_reasoning_effort=") {
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    args.clear();
    args.push("-c".into());
    args.push(format!("model_reasoning_effort=\"{effort}\"").into());
    args.append(&mut kept);
}

fn strip_flags(args: &mut Vec<OsString>, flags: &[&str]) {
    args.retain(|argument| {
        let argument = argument.to_string_lossy();
        !flags
            .iter()
            .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
    });
}

fn strip_options(args: &mut Vec<OsString>, options: &[&str]) {
    let mut kept = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if options.iter().any(|option| argument == *option) {
            index += 1;
            if args
                .get(index)
                .is_some_and(|value| !value.to_string_lossy().starts_with('-'))
            {
                index += 1;
            }
            continue;
        }
        if options
            .iter()
            .any(|option| argument.starts_with(&format!("{option}=")))
        {
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    *args = kept;
}

fn clear_resume_args(spec: &mut RoomSpec) -> io::Result<()> {
    match cli_vendor(spec)? {
        CliVendor::Claude => {
            strip_flags(&mut spec.args, &["--continue", "-c", "--fork-session"]);
            strip_options(&mut spec.args, &["--resume", "-r", "--session-id"]);
        }
        CliVendor::Codex => {
            if let Some(resume) = spec.args.iter().position(|argument| {
                matches!(argument.to_string_lossy().as_ref(), "resume" | "fork")
            }) {
                spec.args.truncate(resume);
            }
        }
        CliVendor::OpenCode => {
            strip_flags(&mut spec.args, &["--continue", "-c", "--fork"]);
            strip_options(&mut spec.args, &["--session", "-s"]);
        }
    }
    Ok(())
}

// `Box<dyn Child>` means "a heap-owned value implementing the Child trait".
// The Option lets cleanup `take()` ownership exactly once.
struct ChildGuard {
    child: Option<Box<dyn Child + Send + Sync>>,
}

impl ChildGuard {
    fn poll_exit(&mut self) -> io::Result<bool> {
        let exited = match self.child.as_mut() {
            Some(child) => child.try_wait()?.is_some(),
            None => true,
        };
        if exited {
            // `try_wait` has reaped the process, so the handle can go away.
            self.child.take();
        }
        Ok(exited)
    }

    fn cleanup(&mut self) {
        // Taking the handle makes explicit cleanup and Drop safe to repeat.
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
    environment: GuestEnvironment,
    child: ChildGuard,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output_rx: mpsc::Receiver<Vec<u8>>,
    parser: Parser,
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
        let mut command = CommandBuilder::new(spec.program.clone());
        command.args(&spec.args);
        // portable-pty defaults an omitted cwd to HOME rather than inheriting
        // Crowded's directory, so the project directory must be explicit.
        let cwd = working_directory(spec.cwd.as_deref())?;
        command.cwd(&cwd);
        command.env("PWD", &cwd);
        for (key, value) in &spec.variables {
            command.env(key, value);
        }
        for (key, value) in &environment.variables {
            command.env(key, value);
        }
        let child = ChildGuard {
            child: Some(pty.slave.spawn_command(command)?),
        };
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
            environment,
            child,
            master: pty.master,
            writer,
            output_rx,
            // vt100 assumes room for wrapping and a double-width character.
            parser: Parser::new(size.rows.max(2), size.cols.max(2), 0),
        })
    }

    pub(crate) fn drain_output(&mut self) -> bool {
        // Drain everything currently waiting without blocking the UI thread.
        let mut received = false;
        while let Ok(bytes) = self.output_rx.try_recv() {
            received = true;
            self.parser.process(&bytes);
        }
        received
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
        let bracketed_paste = matches!(cli_vendor(&self.spec), Ok(CliVendor::Codex));
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
        self.reconfigure(size, clear_resume_args)
    }

    pub(crate) fn set_model(
        &mut self,
        model: &str,
        size: PtySize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.reconfigure(size, |spec| {
            cli_vendor(spec)?;
            replace_option(&mut spec.args, &["--model", "-m"], "--model", model);
            Ok(())
        })
    }

    pub(crate) fn set_effort(
        &mut self,
        effort: &str,
        size: PtySize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.reconfigure(size, |spec| {
            match cli_vendor(spec)? {
                CliVendor::Claude => {
                    replace_option(&mut spec.args, &["--effort"], "--effort", effort);
                }
                CliVendor::Codex => replace_codex_effort(&mut spec.args, effort),
                CliVendor::OpenCode => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "OpenCode does not expose a stable effort launch option",
                    ));
                }
            }
            Ok(())
        })
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

    fn room_spec(program: &str, args: &[&str]) -> RoomSpec {
        RoomSpec {
            name: program.to_owned(),
            title: program.to_owned(),
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            transport: Transport::Raw,
            cwd: None,
            variables: Vec::new(),
            allow_control: true,
        }
    }

    fn arguments(spec: &RoomSpec) -> Vec<String> {
        spec.args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
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
    fn conductor_rewrites_native_launch_options() {
        let mut claude = room_spec("claude", &["--continue", "--model", "old", "--effort=low"]);
        replace_option(&mut claude.args, &["--model", "-m"], "--model", "sonnet");
        replace_option(&mut claude.args, &["--effort"], "--effort", "high");
        clear_resume_args(&mut claude).unwrap();
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
        replace_codex_effort(&mut codex.args, "xhigh");
        assert_eq!(
            &arguments(&codex)[..4],
            [
                "-c",
                "model_reasoning_effort=\"xhigh\"",
                "-c",
                "sandbox_mode=\"workspace-write\"",
            ]
        );
        clear_resume_args(&mut codex).unwrap();
        assert_eq!(arguments(&codex).len(), 4);

        let mut opencode = room_spec(
            "opencode",
            &["--continue", "--session", "session-1", "--model=old"],
        );
        clear_resume_args(&mut opencode).unwrap();
        assert_eq!(arguments(&opencode), ["--model=old"]);
    }
}
