//! Authenticated Windows named-pipe transport for Doorbell events.

use std::{
    collections::{HashSet, VecDeque},
    env,
    ffi::OsStr,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    os::windows::{ffi::OsStrExt, io::FromRawHandle},
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    },
    Security::Authentication::Identity::RtlGenRandom,
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX,
    },
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    },
};

use super::protocol::*;
use crate::pane::GuestEnvironment;

static PIPE_NONCE: AtomicU64 = AtomicU64::new(1);
const PIPE_BUFFER_BYTES: u32 = 8192;
const WAKE_ATTEMPTS: u32 = 50;
const WAKE_RETRY_DELAY: Duration = Duration::from_millis(10);

pub(crate) struct Doorbell {
    path: PathBuf,
    tokens: Vec<String>,
    events: Receiver<DoorbellEvent>,
    stop: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
}

impl Doorbell {
    pub(crate) fn start(room_count: usize) -> io::Result<Self> {
        let nonce = PIPE_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = PathBuf::from(format!(r"\\.\pipe\crowded-{}-{nonce}", process::id()));

        let tokens = (0..room_count)
            .map(|_| capability_token())
            .collect::<io::Result<Vec<_>>>()?;
        let thread_tokens = tokens.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (event_tx, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        // Creating the first instance here makes startup failure observable and
        // guarantees Drop has a real listener to wake after start returns.
        // HANDLE is not Send, but its pointer-sized value can cross into the
        // listener thread and regain ownership there.
        let first_instance = create_instance(&path, true)? as usize;
        let thread_path = path.clone();
        let listener_thread = thread::spawn(move || {
            listener_loop(
                first_instance,
                thread_path,
                thread_tokens,
                room_count,
                event_tx,
                thread_stop,
            );
        });

        Ok(Self {
            path,
            tokens,
            events,
            stop,
            listener_thread: Some(listener_thread),
        })
    }

    pub(crate) fn guest_environment(&self, room_index: usize) -> io::Result<GuestEnvironment> {
        let token = self.tokens.get(room_index).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "unknown room for Doorbell")
        })?;
        Ok(GuestEnvironment::new([
            ("CROWDED_SOCKET", self.path.as_os_str().to_owned()),
            ("CROWDED_TOKEN", token.into()),
            ("CROWDED_ROOM", (room_index + 1).to_string().into()),
            ("CROWDED_BIN", env::current_exe()?.into_os_string()),
        ]))
    }

    pub(crate) fn try_recv(&self) -> Result<DoorbellEvent, TryRecvError> {
        self.events.try_recv()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Doorbell {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // ponytail: the listener thread may be blocked in a synchronous
        // ConnectNamedPipe call; a bounded dummy-client retry unblocks it and
        // detaches only if the listener cannot be reached. Upgrade path:
        // overlapped ConnectNamedPipe with a cancellable wait if shutdown
        // ever needs a stronger guarantee.
        if let Some(handle) = self.listener_thread.take() {
            let mut woke = handle.is_finished();
            for _ in 0..WAKE_ATTEMPTS {
                if woke || handle.is_finished() {
                    woke = true;
                    break;
                }
                match open_client(&self.path) {
                    Ok(dummy) => {
                        drop(dummy);
                        woke = true;
                        break;
                    }
                    Err(error) if retryable_connect(&error) => {
                        thread::sleep(WAKE_RETRY_DELAY);
                    }
                    Err(_) => break,
                }
            }
            // A failed wake must not turn shutdown into an unbounded hang.
            // Dropping a JoinHandle detaches only this already-stopping thread.
            if woke {
                let _ = handle.join();
            }
        }
    }
}

fn capability_token() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    let ok = unsafe { RtlGenRandom(bytes.as_mut_ptr().cast(), bytes.len() as u32) };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn wide_null(path: &Path) -> Vec<u16> {
    OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

// ponytail: a null security descriptor scopes the pipe to the creating
// process's default DACL rather than an explicit "current user only" ACL
// like Unix's 0600 permission bits. Upgrade path: build an explicit
// SECURITY_ATTRIBUTES restricting access to the current user SID if this
// ever needs Unix-equivalent hardening.
fn create_instance(path: &Path, first: bool) -> io::Result<HANDLE> {
    let wide = wide_null(path);
    let open_mode = PIPE_ACCESS_DUPLEX
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let handle = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            std::ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

fn retryable_connect(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(code) if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PIPE_BUSY as i32
    )
}

fn open_client(path: &Path) -> io::Result<File> {
    let wide = wide_null(path);
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
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(handle as _) })
}

fn listener_loop(
    first_instance: usize,
    path: PathBuf,
    tokens: Vec<String>,
    room_count: usize,
    events: SyncSender<DoorbellEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut recent_by_room = vec![VecDeque::<Instant>::new(); room_count];
    let mut seen = HashSet::<String>::new();
    let mut seen_order = VecDeque::<String>::new();
    let mut first_instance = Some(first_instance as HANDLE);

    while !stop.load(Ordering::Relaxed) {
        let handle = match first_instance.take() {
            Some(handle) => handle,
            None => match create_instance(&path, false) {
                Ok(handle) => handle,
                Err(_) => break,
            },
        };

        let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
            || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
        if !connected {
            unsafe { CloseHandle(handle) };
            continue;
        }

        let mut stream = unsafe { File::from_raw_handle(handle as _) };
        let request = read_request(&stream);
        let response = match request {
            Ok(WireRequest::Message(request)) => {
                if seen.contains(&request.id) {
                    Some(WireResponse::accepted("duplicate", None))
                } else {
                    match validate_request(&request, &tokens, room_count, &mut recent_by_room) {
                        Ok(from) => {
                            let (reply, reply_rx) = mpsc::sync_channel(1);
                            let event = DoorbellEvent::Message(DoorbellEnvelope {
                                from,
                                to: request.to - 1,
                                body: request.body,
                                task: request.task,
                                role: request.role,
                                reply,
                            });
                            match events.try_send(event) {
                                Ok(()) => {
                                    remember_id(request.id, &mut seen, &mut seen_order);
                                    reply_rx.recv_timeout(Duration::from_secs(2)).ok()
                                }
                                Err(TrySendError::Full(_)) => {
                                    Some(WireResponse::rejected("Doorbell queue is full"))
                                }
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                        }
                        Err(error) => Some(WireResponse::rejected(error)),
                    }
                }
            }
            Ok(WireRequest::Control(request)) => {
                if seen.contains(&request.id) {
                    Some(WireResponse::accepted("duplicate", None))
                } else {
                    match validate_control(&request, &tokens, room_count, &mut recent_by_room) {
                        Ok(from) => {
                            let (reply, reply_rx) = mpsc::sync_channel(1);
                            let event = DoorbellEvent::Control(DoorbellControl {
                                from,
                                to: request.to - 1,
                                action: request.action,
                                reply,
                            });
                            match events.try_send(event) {
                                Ok(()) => {
                                    remember_id(request.id, &mut seen, &mut seen_order);
                                    reply_rx.recv_timeout(Duration::from_secs(2)).ok()
                                }
                                Err(TrySendError::Full(_)) => {
                                    Some(WireResponse::rejected("Doorbell queue is full"))
                                }
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                        }
                        Err(error) => Some(WireResponse::rejected(error)),
                    }
                }
            }
            Ok(WireRequest::Pulse(request)) => {
                if seen.contains(&request.id) {
                    Some(WireResponse::accepted("duplicate", None))
                } else {
                    match validate_pulse(&request, &tokens) {
                        Ok(from) => match events.try_send(DoorbellEvent::Pulse(DoorbellPulse {
                            from,
                            state: request.state,
                        })) {
                            Ok(()) => {
                                remember_id(request.id, &mut seen, &mut seen_order);
                                Some(WireResponse::accepted("recorded", None))
                            }
                            Err(TrySendError::Full(_)) => {
                                Some(WireResponse::rejected("Doorbell queue is full"))
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        },
                        Err(error) => Some(WireResponse::rejected(error)),
                    }
                }
            }
            Ok(WireRequest::Roster(request)) => {
                match validate_roster(&request, &tokens, &mut recent_by_room) {
                    Ok(_) => {
                        let (reply, reply_rx) = mpsc::sync_channel(1);
                        match events.try_send(DoorbellEvent::Roster(DoorbellRoster { reply })) {
                            Ok(()) => reply_rx.recv_timeout(Duration::from_secs(2)).ok(),
                            Err(TrySendError::Full(_)) => {
                                Some(WireResponse::rejected("Doorbell queue is full"))
                            }
                            Err(TrySendError::Disconnected(_)) => break,
                        }
                    }
                    Err(error) => Some(WireResponse::rejected(error)),
                }
            }
            Err(error) => Some(WireResponse::rejected(error.to_string())),
        }
        .unwrap_or_else(|| WireResponse::rejected("Doorbell response timed out"));
        let _ = write_response(&mut stream, &response);
    }

    if let Some(handle) = first_instance {
        unsafe { CloseHandle(handle) };
    }
}

fn read_request(stream: &File) -> io::Result<WireRequest> {
    let mut line = String::new();
    BufReader::new(stream)
        .take((MAX_WIRE_BYTES + 1) as u64)
        .read_line(&mut line)?;
    if line.len() > MAX_WIRE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "envelope exceeds wire limit",
        ));
    }
    serde_json::from_str(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn remember_id(id: String, seen: &mut HashSet<String>, order: &mut VecDeque<String>) {
    seen.insert(id.clone());
    order.push_back(id);
    while order.len() > DEDUPE_CAPACITY {
        if let Some(expired) = order.pop_front() {
            seen.remove(&expired);
        }
    }
}

fn write_response(stream: &mut File, response: &WireResponse) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_tokens_are_random_and_doorbell_shuts_down_cleanly() {
        let doorbell = Doorbell::start(2).unwrap();
        assert_ne!(doorbell.tokens[0], doorbell.tokens[1]);
        drop(doorbell);
    }

    #[test]
    fn roster_is_authenticated_and_machine_readable() {
        let doorbell = Doorbell::start(2).unwrap();
        let path = doorbell.path().to_owned();
        let token = doorbell.tokens[0].clone();
        let client = thread::spawn(move || {
            let mut stream = open_client(&path).unwrap();
            serde_json::to_writer(
                &mut stream,
                &WireRequest::Roster(RosterRequest {
                    token,
                    id: "roster-roundtrip".to_owned(),
                }),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            serde_json::from_reader::<_, WireResponse>(BufReader::new(stream)).unwrap()
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match doorbell.try_recv() {
                Ok(DoorbellEvent::Roster(request)) => {
                    request.reply(vec![RosterRoom {
                        room: 2,
                        name: "Builder".to_owned(),
                        guest: "codex".to_owned(),
                        vendor: "openai".to_owned(),
                        transport: "raw".to_owned(),
                        state: PulseState::Ready,
                        state_source: PulseSource::Gate,
                        allow_control: true,
                        model: None,
                        effort: None,
                        cost: "unknown".to_owned(),
                        headroom: true,
                        pulse_age_ms: None,
                        capabilities: Default::default(),
                        scheduling: None,
                    }]);
                    break;
                }
                Ok(_) => panic!("unexpected Doorbell event"),
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("roster request did not arrive: {error}"),
            }
        }
        let response = serde_json::to_value(client.join().unwrap()).unwrap();
        assert_eq!(response["status"], "listed");
        assert_eq!(response["rooms"][0]["room"], 2);
        assert_eq!(response["rooms"][0]["vendor"], "openai");
        assert_eq!(response["rooms"][0]["headroom"], true);
    }
}
