//! One room: its guest command, PTY, terminal screen, and lifecycle.

use std::{
    env,
    ffi::OsString,
    io::{self, Read, Write},
    path::Path,
    sync::mpsc,
    thread,
    time::Duration,
};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tui_term::vt100::{Parser, Screen};

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

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Transport {
    Shell,
    Raw,
}

#[derive(Clone)]
pub(crate) struct RoomSpec {
    title: String,
    program: OsString,
    transport: Transport,
}

impl RoomSpec {
    fn new(program: OsString, transport: Transport, room_number: usize) -> Self {
        let guest = Path::new(program.as_os_str())
            .file_name()
            .unwrap_or(program.as_os_str())
            .to_string_lossy();
        Self {
            title: format!("{guest} · {room_number}"),
            program,
            transport,
        }
    }

    fn parse(value: OsString, room_number: usize) -> io::Result<Self> {
        let value = value.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "guest spec must be valid UTF-8",
            )
        })?;
        let (kind, program) = value.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "guest must be shell:PROGRAM or raw:PROGRAM",
            )
        })?;
        if program.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "guest program cannot be empty",
            ));
        }
        let transport = match kind {
            "shell" => Transport::Shell,
            "raw" => Transport::Raw,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "guest kind must be shell or raw",
                ));
            }
        };
        Ok(Self::new(program.into(), transport, room_number))
    }
}

pub(crate) fn room_specs() -> io::Result<[RoomSpec; 2]> {
    let guests: Vec<_> = env::args_os().skip(1).collect();
    match guests.as_slice() {
        [] => {
            let shell = env::var_os("SHELL").unwrap_or_else(|| {
                if cfg!(windows) {
                    "cmd.exe".into()
                } else {
                    "/bin/sh".into()
                }
            });
            Ok([
                RoomSpec::new(shell.clone(), Transport::Shell, 1),
                RoomSpec::new(shell, Transport::Shell, 2),
            ])
        }
        [left, right] => Ok([
            RoomSpec::parse(left.clone(), 1)?,
            RoomSpec::parse(right.clone(), 2)?,
        ]),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: crowded [shell:PROGRAM|raw:PROGRAM] [shell:PROGRAM|raw:PROGRAM]",
        )),
    }
}

const RAW_SUBMIT_DELAY: Duration = Duration::from_millis(150);

fn whisper_parts(transport: Transport, source: &str, message: &str) -> (Vec<u8>, Option<Vec<u8>>) {
    let note = format!("[whisper from {source}] {message}");
    // ponytail: `Shell` currently means POSIX shell; add cmd/PowerShell
    // encoders only when Windows becomes a real target.
    match transport {
        // Shell rooms need a quoted command; raw rooms accept the note itself.
        Transport::Shell => (
            format!("printf '%s\\n' {}\r", shell_quote(&note)).into_bytes(),
            None,
        ),
        // Raw TUIs may classify a rapid prompt+Enter sequence as one paste.
        Transport::Raw => (note.into_bytes(), Some(vec![b'\r'])),
    }
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

    pub(crate) fn screen(&self) -> &Screen {
        self.parser.screen()
    }

    pub(crate) fn send_whisper(&mut self, source: &str, message: &str) -> io::Result<()> {
        let (body, submit) = whisper_parts(self.spec.transport, source, message);
        self.write_bytes(&body)?;
        if let Some(submit) = submit {
            // Codex keeps Enter in paste/newline mode for 120 ms after a burst.
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

    #[test]
    fn whisper_quotes_shell_metacharacters() {
        assert_eq!(
            String::from_utf8(
                whisper_parts(Transport::Shell, "Room 1", "it's $HOME; echo nope",).0
            )
            .unwrap(),
            "printf '%s\\n' '[whisper from Room 1] it'\"'\"'s $HOME; echo nope'\r"
        );
        let (body, submit) = whisper_parts(Transport::Raw, "Claude", "hello");
        assert_eq!(body, b"[whisper from Claude] hello");
        assert_eq!(submit, Some(vec![b'\r']));
    }

    #[test]
    fn guest_spec_selects_transport_and_name() {
        let guest = RoomSpec::parse("raw:codex".into(), 2).unwrap();
        assert_eq!(guest.transport, Transport::Raw);
        assert_eq!(guest.program, OsString::from("codex"));
        assert_eq!(guest.title, "codex · 2");
    }

    #[test]
    fn shift_tab_uses_the_terminal_backtab_sequence() {
        let key = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(key_bytes(key), Some(b"\x1b[Z".to_vec()));
    }
}
