//! Placeholder local server until a platform-specific Doorbell transport exists.

use std::{io, path::Path, sync::mpsc::TryRecvError};

use crate::pane::GuestEnvironment;

use super::protocol::DoorbellEvent;

pub(crate) struct Doorbell;

impl Doorbell {
    pub(crate) fn start(_: usize) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Doorbell local transport is not available on this platform",
        ))
    }

    pub(crate) fn guest_environment(&self, _: usize) -> io::Result<GuestEnvironment> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Doorbell local transport is not available on this platform",
        ))
    }

    pub(crate) fn try_recv(&self) -> Result<DoorbellEvent, TryRecvError> {
        Err(TryRecvError::Disconnected)
    }

    pub(crate) fn path(&self) -> &Path {
        Path::new("")
    }
}
