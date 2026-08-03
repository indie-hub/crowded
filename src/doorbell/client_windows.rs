//! Windows named-pipe transport for Doorbell command-line clients.

use std::{
    env,
    ffi::OsStr,
    fs::File,
    io::{self, BufReader, Write},
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::Path,
    thread,
    time::Duration,
};

use windows_sys::Win32::{
    Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{CreateFileW, FILE_SHARE_NONE, OPEN_EXISTING},
};

use super::protocol::{WireRequest, WireResponse};

const CONNECT_ATTEMPTS: u32 = 10;
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(20);

pub(super) fn send_request(
    request: &WireRequest,
) -> Result<WireResponse, Box<dyn std::error::Error>> {
    let path = env::var_os("CROWDED_SOCKET").ok_or("CROWDED_SOCKET is not set")?;
    send_to(Path::new(&path), request)
}

fn send_to(path: &Path, request: &WireRequest) -> Result<WireResponse, Box<dyn std::error::Error>> {
    let mut stream = connect(path)?;
    serde_json::to_writer(&mut stream, request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(serde_json::from_reader(BufReader::new(stream))?)
}

// ponytail: bounded retry smooths the brief window between one client
// disconnecting and the server creating its next pipe instance. Upgrade
// path: have the server pre-create its next instance before it finishes
// handling the current one, if this retry ever shows up in practice.
fn connect(path: &Path) -> io::Result<File> {
    let wide: Vec<u16> = OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    for attempt in 1..=CONNECT_ATTEMPTS {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_NONE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            return Ok(unsafe { File::from_raw_handle(handle as _) });
        }
        let error = io::Error::last_os_error();
        let retryable = matches!(
            error.raw_os_error(),
            Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PIPE_BUSY as i32
        );
        if !retryable || attempt == CONNECT_ATTEMPTS {
            return Err(error);
        }
        thread::sleep(CONNECT_RETRY_DELAY);
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Write},
        path::PathBuf,
        thread,
    };

    use windows_sys::Win32::{
        Foundation::{ERROR_PIPE_CONNECTED, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::PIPE_ACCESS_DUPLEX,
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
            PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        },
    };

    use super::*;
    use crate::doorbell::protocol::RosterRequest;

    fn create_test_pipe(wide_name: &[u16]) -> HANDLE {
        let handle = unsafe {
            CreateNamedPipeW(
                wide_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                0,
                std::ptr::null(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE);
        handle
    }

    #[test]
    fn sends_json_line_and_reads_response() {
        let path = PathBuf::from(format!(
            r"\\.\pipe\crowded-client-test-{}",
            std::process::id()
        ));
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = create_test_pipe(&wide) as usize;
        let request = WireRequest::Roster(RosterRequest {
            token: "token".into(),
            id: "request".into(),
        });
        let server = thread::spawn(move || {
            let handle = handle as HANDLE;
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
                || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
            assert!(connected);
            let mut stream = unsafe { File::from_raw_handle(handle as _) };
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            assert!(matches!(
                serde_json::from_str(&line),
                Ok(WireRequest::Roster(_))
            ));
            serde_json::to_writer(&mut stream, &WireResponse::accepted("listed", None)).unwrap();
            stream.write_all(b"\n").unwrap();
        });

        assert_eq!(send_to(&path, &request).unwrap().status, "listed");
        server.join().unwrap();
    }
}
