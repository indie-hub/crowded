//! The Crowded Room: direct programs sharing a small PTY umbrella.

mod app;
mod checker;
mod command;
mod config;
mod doorbell;
mod initializer;
mod mailroom;
mod mcp_cli;
mod pane;
mod plugins;
mod toolbox;

const USAGE: &str = "\
crowded — The Crowded Room: direct programs sharing a small PTY umbrella.

Usage:
  crowded GUEST GUEST [GUEST...]   Run one or more guests in a shared pane
  crowded <subcommand> [options]

Subcommands:
  send       Send a message to a room
  control    Control a room (clear, resume, model, effort)
  resume     Resume an existing session
  pulse      Send a heartbeat pulse
  roster     List rooms and their status
  check      Run pre-flight checks
  init       Initialize a new project
  plugin     Manage plugins
  toolbox    Toolbox utilities
";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    if command::internal_launcher_requested() {
        std::process::exit(command::run_internal_launcher()?);
    }

    match std::env::args().nth(1).as_deref() {
        Some("--help") | Some("-h") => {
            print!("{USAGE}");
            std::process::exit(0);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_lists_all_subcommands() {
        let expected = [
            "send", "control", "resume", "pulse", "roster", "check", "init", "plugin", "toolbox",
        ];
        for cmd in expected {
            assert!(USAGE.contains(cmd), "help text missing subcommand: {cmd}",);
        }
    }
}
