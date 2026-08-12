//! Feeds a captured guest byte stream through the same parser Crowded uses and
//! reports what the wheel would have to work with: whether the guest took the
//! alternate screen, and how many scrollback rows exist to scroll through.
//!
//! Usage: `cargo run --example guest_scroll_probe -- <capture.bin> [rows] [cols]`

use std::env;
use std::fs;
use std::io;

use tui_term::vt100::Parser;

fn main() -> io::Result<()> {
    let path = env::args()
        .nth(1)
        .expect("usage: guest_scroll_probe <capture.bin> [rows] [cols]");
    let rows: u16 = env::args()
        .nth(2)
        .and_then(|a| a.parse().ok())
        .unwrap_or(40);
    let cols: u16 = env::args()
        .nth(3)
        .and_then(|a| a.parse().ok())
        .unwrap_or(120);

    let bytes = fs::read(&path)?;
    let mut parser = Parser::new(rows, cols, 1000);
    parser.process(&bytes);

    println!("bytes           {}", bytes.len());
    println!("alternate screen {}", parser.screen().alternate_screen());

    // set_scrollback clamps to what the parser actually retained, so the
    // highest offset that still moves the view is the usable scrollback depth.
    let mut depth = 0;
    for offset in 1..=1000 {
        parser.screen_mut().set_scrollback(offset);
        if parser.screen().scrollback() != offset {
            break;
        }
        depth = offset;
    }
    parser.screen_mut().set_scrollback(0);
    println!("scrollback rows  {depth}");

    // A fourth argument dumps the rendered screen, which is how two captures
    // taken either side of a keypress can be compared for actual movement
    // rather than for how many bytes the guest happened to emit.
    if env::args().nth(4).is_some() {
        println!("---- screen ----");
        println!("{}", parser.screen().contents());
    }
    Ok(())
}
