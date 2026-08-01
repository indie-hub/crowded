//! Authenticated local envelopes entering through a Unix socket.

mod commands;
mod protocol;
mod server;

pub(crate) use commands::{control_command, pulse_command, roster_command, send_command};
pub(crate) use protocol::{ControlAction, DoorbellEvent, PulseState, RosterRoom};
pub(crate) use server::Doorbell;
