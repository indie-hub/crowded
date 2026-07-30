//! The Crowded Room: direct programs sharing a small PTY umbrella.

mod app;
mod config;
mod doorbell;
mod mailroom;
mod pane;
mod plugins;
mod toolbox;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("send") => doorbell::send_command(),
        Some("pulse") => doorbell::pulse_command(),
        Some("plugin") => plugins::command(),
        Some("toolbox") => toolbox::command(),
        _ => app::run(),
    }
}
