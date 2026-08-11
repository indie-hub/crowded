//! Diagnostic probe for wheel input. Reproduces Crowded's exact terminal state
//! (raw mode + alternate screen + EnableMouseCapture) and records what the host
//! terminal actually delivers.
//!
//! `raw`    — bypasses crossterm's parser and logs each stdin read() boundary,
//!            which is what reveals whether the host splits an SGR sequence
//!            across separate deliveries.
//! `events` — logs crossterm's parsed events, which is what Crowded consumes.
//!
//! Run one mode, scroll the wheel a few notches, press q to quit.

use std::fs::File;
use std::io::{self, Read, Write};
use std::time::Instant;

use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

fn main() -> io::Result<()> {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "raw".to_owned());
    let log_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/tmp/wheel_probe.log".to_owned());
    let mut log = File::create(&log_path)?;

    // `narrow` requests exactly what Crowded now requests: button reporting and
    // SGR encoding, without the drag and any-motion modes. Wheel reports must
    // still arrive under it, or the narrower request is wrong.
    let narrow = mode == "narrow";

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    if narrow {
        io::stdout().write_all(b"\x1b[?1000h\x1b[?1006h")?;
        io::stdout().flush()?;
    } else {
        execute!(io::stdout(), EnableMouseCapture)?;
    }

    let result = match mode.as_str() {
        "events" | "narrow" => run_events(&mut log),
        _ => run_raw(&mut log),
    };

    if narrow {
        io::stdout().write_all(b"\x1b[?1006l\x1b[?1000l")?;
        io::stdout().flush()?;
    } else {
        execute!(io::stdout(), DisableMouseCapture)?;
    }
    execute!(io::stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()?;
    println!("probe log written to {log_path}");
    result
}

/// Each iteration prints one stdin read() boundary. A wheel notch arriving as
/// two or more lines is direct evidence the host fragments the sequence.
fn run_raw(log: &mut File) -> io::Result<()> {
    let mut out = io::stdout();
    let start = Instant::now();
    let mut stdin = io::stdin();
    let mut buf = [0u8; 1024];
    let mut last = start;

    write!(out, "RAW mode. Scroll the wheel, then press q.\r\n")?;
    out.flush()?;

    loop {
        let n = stdin.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let now = Instant::now();
        let bytes = &buf[..n];
        let line = format!(
            "t={:>8.3}ms gap={:>8.3}ms n={:<3} {:?} | {}",
            start.elapsed().as_secs_f64() * 1000.0,
            now.duration_since(last).as_secs_f64() * 1000.0,
            n,
            String::from_utf8_lossy(bytes),
            bytes
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        last = now;
        writeln!(log, "{line}")?;
        log.flush()?;
        write!(out, "{line}\r\n")?;
        out.flush()?;

        if bytes.contains(&b'q') {
            break;
        }
    }
    Ok(())
}

/// Logs crossterm's parsed view. Wheel notches must appear as Event::Mouse;
/// an Esc key followed by literal characters means the parse was defeated.
fn run_events(log: &mut File) -> io::Result<()> {
    let mut out = io::stdout();
    let start = Instant::now();
    let mut last = start;

    write!(out, "EVENTS mode. Scroll the wheel, then press q.\r\n")?;
    out.flush()?;

    loop {
        let ev = event::read()?;
        let now = Instant::now();
        let line = format!(
            "t={:>8.3}ms gap={:>8.3}ms {:?}",
            start.elapsed().as_secs_f64() * 1000.0,
            now.duration_since(last).as_secs_f64() * 1000.0,
            ev
        );
        last = now;
        writeln!(log, "{line}")?;
        log.flush()?;
        write!(out, "{line}\r\n")?;
        out.flush()?;

        if let Event::Key(key) = ev
            && key.code == KeyCode::Char('q')
        {
            break;
        }
    }
    Ok(())
}
