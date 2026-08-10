//! Authenticated local envelopes entering through a Unix socket.

#[cfg(unix)]
mod client_unix;
#[cfg(windows)]
mod client_windows;
mod commands;
mod protocol;
#[cfg(unix)]
#[path = "doorbell/server_unix.rs"]
mod server;
#[cfg(windows)]
#[path = "doorbell/server_windows.rs"]
mod server;
#[cfg(not(any(unix, windows)))]
#[path = "doorbell/server_unsupported.rs"]
mod server;

pub(crate) use commands::{control_command, pulse_command, roster_command, send_command};
pub(crate) use protocol::{
    ControlAction, DoorbellEvent, Effort, ModelCatalogue, PulseSource, PulseState,
    RoomCapabilities, RosterRoom,
};
pub(crate) use server::Doorbell;
