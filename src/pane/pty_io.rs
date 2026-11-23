//! Linux terminal I/O that can always be stopped.
//!
//! A write to a pty master blocks once the guest stops reading, and Linux
//! leaves that write asleep after the slave closes, so the writer thread would
//! never return. This writer uses a non-blocking duplicate of the master, so a
//! stop request or a hung-up terminal ends it. `O_NONBLOCK` belongs to the open
//! file description shared with the reader, so the reader polls as well instead
//! of treating `WouldBlock` as the end of output.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::{AsRawFd, FromRawFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

unsafe extern "C" {
    fn dup(fd: i32) -> i32;
    fn fcntl(fd: i32, command: i32, ...) -> i32;
    fn poll(descriptors: *mut PollFd, count: u64, timeout: i32) -> i32;
}

/// How long a poll waits before the stop flag is checked again.
const POLL_TIMEOUT_MS: i32 = 200;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0o4000;
const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

/// Duplicates a descriptor and makes the shared description non-blocking.
pub(super) fn nonblocking_dup(fd: i32) -> io::Result<File> {
    let duplicate = unsafe { dup(fd) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: `dup` returned a descriptor this value now owns.
    let file = unsafe { File::from_raw_fd(duplicate) };
    let flags = unsafe { fcntl(duplicate, F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { fcntl(duplicate, F_SETFL, flags | O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

fn ready(file: &File, events: i16) -> io::Result<i16> {
    loop {
        let mut descriptor = PollFd {
            fd: file.as_raw_fd(),
            events,
            revents: 0,
        };
        let count = unsafe { poll(&mut descriptor, 1, POLL_TIMEOUT_MS) };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        return Ok(descriptor.revents);
    }
}

/// The pane's write side. Every wait is bounded, so dropping the pane always
/// ends the thread even when the guest stopped reading.
pub(super) struct PtyWriter {
    file: File,
    stopped: Arc<AtomicBool>,
}

impl PtyWriter {
    pub(super) fn new(file: File, stopped: Arc<AtomicBool>) -> Self {
        Self { file, stopped }
    }
}

impl Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            if self.stopped.load(Ordering::SeqCst) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "room writer stopped",
                ));
            }
            match self.file.write(buf) {
                Ok(written) => return Ok(written),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if ready(&self.file, POLLOUT | POLLHUP | POLLERR)? & (POLLHUP | POLLERR) != 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "the room terminal is gone",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The pane's read side. `POLLIN` wakes it at once; a poll timeout only checks
/// that the terminal is still there.
pub(super) struct PtyReader {
    file: File,
}

impl PtyReader {
    pub(super) fn new(file: File) -> Self {
        Self { file }
    }
}

impl Read for PtyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let events = ready(&self.file, POLLIN | POLLHUP | POLLERR)?;
            if events & POLLIN != 0 {
                match self.file.read(buf) {
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    result => return result,
                }
            } else if events & (POLLHUP | POLLERR) != 0 {
                return Ok(0);
            }
        }
    }
}
