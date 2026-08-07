//! The Crowded Room: direct programs sharing a small PTY umbrella.

mod app;
mod checker;
mod command;
mod config;
mod doorbell;
mod initializer;
mod mailroom;
mod pane;
mod plugins;
mod toolbox;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    if command::internal_launcher_requested() {
        std::process::exit(command::run_internal_launcher()?);
    }

    match std::env::args().nth(1).as_deref() {
        Some("send") => doorbell::send_command(),
        Some("control") => doorbell::control_command(),
        Some("resume") => app::run_resumed(),
        Some("pulse") => doorbell::pulse_command(),
        Some("roster") => doorbell::roster_command(),
        Some("check") => checker::command(),
        Some("init") => initializer::command(),
        Some("plugin") => plugins::command(),
        Some("toolbox") => toolbox::command(),
        _ => app::run(),
    }
}
