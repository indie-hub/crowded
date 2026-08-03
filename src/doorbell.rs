//! Authenticated local envelopes entering through a Unix socket.

#[cfg(unix)]
mod client_unix;
mod commands;
mod protocol;
#[cfg(unix)]
#[path = "doorbell/server_unix.rs"]
mod server;
#[cfg(not(unix))]
#[path = "doorbell/server_unsupported.rs"]
mod server;

pub(crate) use commands::{control_command, pulse_command, roster_command, send_command};
pub(crate) use protocol::{ControlAction, DoorbellEvent, PulseState, RosterRoom};
pub(crate) use server::Doorbell;
