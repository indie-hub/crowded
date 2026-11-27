//! Authenticated Windows named-pipe transport for Doorbell events.

use std::{
    collections::{HashSet, VecDeque},
    env,
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle},
    },
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
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    },
    Security::Authentication::Identity::RtlGenRandom,
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, OPEN_EXISTING,
        PIPE_ACCESS_DUPLEX,
    },
    System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe,
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
                REQUEST_READ_DEADLINE,
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
    read_deadline: Duration,
) {
    let mut recent_by_room = vec![VecDeque::<Instant>::new(); room_count];
    let mut seen = HashSet::<String>::new();
    let mut seen_order = VecDeque::<String>::new();
    let mut first_instance = Some(first_instance as HANDLE);
    let mut accept_failures = AcceptFailures::new();

    while !stop.load(Ordering::Relaxed) {
        let handle = match first_instance.take() {
            Some(handle) => handle,
            None => match create_instance(&path, false) {
                Ok(handle) => handle,
                Err(_) => {
                    if accept_failures.failed() {
                        break;
                    }
                    sleep_unless_stopped(accept_failures.backoff(), &stop);
                    continue;
                }
            },
        };

        let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) } != 0
            || io::Error::last_os_error().raw_os_error() == Some(ERROR_PIPE_CONNECTED as i32);
        if !connected {
            unsafe { CloseHandle(handle) };
            if accept_failures.failed() {
                break;
            }
            sleep_unless_stopped(accept_failures.backoff(), &stop);
            continue;
        }
        accept_failures.connected();

        let mut stream = unsafe { File::from_raw_handle(handle as _) };
        let request = read_request(&stream, read_deadline, &stop);
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
                                    Some(await_queued_reply(&reply_rx, REPLY_WAIT))
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
                                    Some(await_queued_reply(&reply_rx, REPLY_WAIT))
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
                            model: request.model,
                            detail: request.detail,
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
            Ok(WireRequest::SessionReport(request)) => {
                if seen.contains(&request.id) {
                    Some(WireResponse::accepted("duplicate", None))
                } else {
                    match validate_session_report(&request, &tokens) {
                        Ok(from) => {
                            let (reply, reply_rx) = mpsc::sync_channel(1);
                            let event = DoorbellEvent::SessionReport(DoorbellSessionReport {
                                from,
                                session: request.session,
                                pending: request.pending,
                                launch: request.launch,
                                reply,
                            });
                            match events.try_send(event) {
                                Ok(()) => {
                                    remember_id(request.id, &mut seen, &mut seen_order);
                                    Some(await_queued_reply(&reply_rx, REPLY_WAIT))
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

/// How long a read with nothing to consume waits before re-checking the
/// deadline and the stop flag.
const READ_POLL: Duration = Duration::from_millis(10);

/// Reads one newline-terminated request without blocking past `read_deadline`.
/// A blocking `read_line` on a pipe has no timeout, so a client that writes
/// half a line holds the only listener instance forever. Peeking the available
/// bytes keeps every read non-blocking; the deadline bounds waiting for the
/// rest of the line.
fn read_request(
    stream: &File,
    read_deadline: Duration,
    stop: &AtomicBool,
) -> io::Result<WireRequest> {
    let handle = stream.as_raw_handle() as HANDLE;
    let deadline = Instant::now() + read_deadline;
    let mut reader = stream;
    let mut bytes = Vec::with_capacity(4096);
    let mut chunk = [0_u8; 4096];
    loop {
        // Checked even while bytes arrive, so a client that keeps the pipe
        // non-empty cannot outlive the deadline or delay shutdown.
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "request read deadline exceeded",
            ));
        }
        if stop.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "doorbell is stopping",
            ));
        }
        let available = match peek_available(handle) {
            Ok(available) => available,
            // The client closed its end: parse what arrived, as the old
            // read_line EOF path did.
            Err(error) if error.raw_os_error() == Some(ERROR_BROKEN_PIPE as i32) => break,
            Err(error) => return Err(error),
        };
        if available == 0 {
            thread::sleep(READ_POLL);
            continue;
        }
        let capacity_left = MAX_WIRE_BYTES + 1 - bytes.len();
        let want = (available as usize).min(capacity_left).min(chunk.len());
        let count = reader.read(&mut chunk[..want])?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
        // Frame at the first newline: bytes after it are not part of this
        // request, and waiting for more input would hang a valid request.
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            if newline >= MAX_WIRE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "envelope exceeds wire limit",
                ));
            }
            bytes.truncate(newline + 1);
            break;
        }
        if bytes.len() > MAX_WIRE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "envelope exceeds wire limit",
            ));
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Bytes waiting in the pipe, without consuming them. A broken pipe surfaces
/// as an error and the caller treats it as EOF.
fn peek_available(handle: HANDLE) -> io::Result<u32> {
    let mut available = 0_u32;
    let ok = unsafe {
        PeekNamedPipe(
            handle,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            &mut available,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(available)
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
    use std::io::{BufRead, BufReader};

    use super::*;

    /// A listener on a private test pipe, plus the event end the test answers
    /// from. `read_deadline` is the value the listener passes to each read.
    struct TestPipe {
        path: PathBuf,
        events: Receiver<DoorbellEvent>,
        stop: Arc<AtomicBool>,
        token: String,
        listener: Option<JoinHandle<()>>,
    }

    impl TestPipe {
        fn start(read_deadline: Duration) -> Self {
            let nonce = PIPE_NONCE.fetch_add(1, Ordering::Relaxed);
            let path = PathBuf::from(format!(r"\\.\pipe\crowded-test-{}-{nonce}", process::id()));
            // HANDLE is not Send, so it crosses into the listener thread as a
            // pointer-sized value, exactly like the production start path.
            let first = create_instance(&path, true).unwrap() as usize;
            let token = "left".to_owned();
            let tokens = vec![token.clone(), "right".to_owned()];
            let (event_tx, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let loop_path = path.clone();
            let listener = thread::spawn(move || {
                listener_loop(
                    first,
                    loop_path,
                    tokens,
                    2,
                    event_tx,
                    thread_stop,
                    read_deadline,
                );
            });
            Self {
                path,
                events,
                stop,
                token,
                listener: Some(listener),
            }
        }
    }

    impl Drop for TestPipe {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // Unblock a ConnectNamedPipe wait; the stopped read returns at once.
            let _ = open_client(&self.path);
            if let Some(handle) = self.listener.take() {
                let _ = handle.join();
            }
        }
    }

    /// Connects to a pipe that may be between instances for a moment.
    fn connect_with_retry(path: &Path) -> File {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match open_client(path) {
                Ok(client) => return client,
                Err(error) if retryable_connect(&error) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("could not connect to the test pipe: {error}"),
            }
        }
    }

    fn answer_one_roster(events: &Receiver<DoorbellEvent>, deadline: Instant) {
        loop {
            match events.try_recv() {
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
                    return;
                }
                Ok(_) => panic!("unexpected Doorbell event"),
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("roster request did not arrive: {error}"),
            }
        }
    }

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

    /// The named-pipe transport must agree with the Unix one: a queued
    /// message that is never answered is pending, not rejected, and the id
    /// is remembered so a resend is a duplicate.
    #[test]
    fn a_queued_message_without_a_reply_is_pending_and_deduped() {
        let doorbell = Doorbell::start(2).unwrap();
        let path = doorbell.path().to_owned();
        let token = doorbell.tokens[0].clone();
        let client = thread::spawn(move || {
            let mut stream = open_client(&path).unwrap();
            serde_json::to_writer(
                &mut stream,
                &WireRequest::Message(MessageRequest {
                    token,
                    id: "pending-message".to_owned(),
                    to: 2,
                    body: "hello".to_owned(),
                    task: None,
                    role: None,
                    hop: 0,
                }),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            serde_json::from_reader::<_, WireResponse>(BufReader::new(stream)).unwrap()
        });

        let response = client.join().unwrap();
        assert!(response.ok);
        assert_eq!(response.status, "pending");
        assert_eq!(response.error, None);

        let event = doorbell.try_recv().expect("one event was queued");
        match event {
            DoorbellEvent::Message(envelope) => assert_eq!(envelope.body, "hello"),
            _ => panic!("unexpected doorbell event"),
        }
        assert!(matches!(doorbell.try_recv(), Err(TryRecvError::Empty)));

        let path = doorbell.path().to_owned();
        let token = doorbell.tokens[0].clone();
        let resend = thread::spawn(move || {
            let mut stream = open_client(&path).unwrap();
            serde_json::to_writer(
                &mut stream,
                &WireRequest::Message(MessageRequest {
                    token,
                    id: "pending-message".to_owned(),
                    to: 2,
                    body: "again".to_owned(),
                    task: None,
                    role: None,
                    hop: 0,
                }),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            serde_json::from_reader::<_, WireResponse>(BufReader::new(stream)).unwrap()
        });
        let response = resend.join().unwrap();
        assert!(response.ok);
        assert_eq!(response.status, "duplicate");
    }

    /// A client that writes half a line and holds the pipe open is rejected at
    /// the read deadline, and does not stop later clients from being served.
    #[test]
    fn a_stalled_partial_request_is_rejected_and_later_clients_are_served() {
        let pipe = TestPipe::start(Duration::from_millis(200));
        let mut stalled = connect_with_retry(&pipe.path);
        stalled.write_all(b"{\"kind\":").unwrap();
        stalled.flush().unwrap();

        let started = Instant::now();
        let mut line = String::new();
        BufReader::new(&stalled).read_line(&mut line).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the request read deadline must bound a stalled request"
        );
        let rejected: WireResponse = serde_json::from_str(&line).unwrap();
        assert!(!rejected.ok);
        assert_eq!(rejected.status, "rejected");

        // The stalled client is still open: a second authenticated client must
        // still complete a roster round trip on the next pipe instance.
        let path = pipe.path.clone();
        let token = pipe.token.clone();
        let client = thread::spawn(move || {
            let mut stream = connect_with_retry(&path);
            serde_json::to_writer(
                &mut stream,
                &WireRequest::Roster(RosterRequest {
                    token,
                    id: "after-stall".to_owned(),
                }),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            serde_json::from_reader::<_, WireResponse>(BufReader::new(stream)).unwrap()
        });
        answer_one_roster(&pipe.events, Instant::now() + Duration::from_secs(2));
        assert_eq!(client.join().unwrap().status, "listed");
        drop(stalled);
    }

    /// A valid request split across two writes, with a gap inside the read
    /// deadline, still parses.
    #[test]
    fn a_request_split_across_writes_within_the_deadline_parses() {
        let pipe = TestPipe::start(Duration::from_millis(500));
        let path = pipe.path.clone();
        let token = pipe.token.clone();
        let client = thread::spawn(move || {
            let mut stream = connect_with_retry(&path);
            let mut wire = serde_json::to_vec(&WireRequest::Message(MessageRequest {
                token,
                id: "split-request".to_owned(),
                to: 2,
                body: "hello in two writes".to_owned(),
                task: None,
                role: None,
                hop: 0,
            }))
            .unwrap();
            wire.push(b'\n');
            let split = wire.len() / 2;
            stream.write_all(&wire[..split]).unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(100));
            stream.write_all(&wire[split..]).unwrap();
            stream.flush().unwrap();
            serde_json::from_reader::<_, WireResponse>(BufReader::new(stream)).unwrap()
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match pipe.events.try_recv() {
                Ok(DoorbellEvent::Message(envelope)) => {
                    assert_eq!(envelope.body, "hello in two writes");
                    envelope.reply_injected(1);
                    break;
                }
                Ok(_) => panic!("unexpected Doorbell event"),
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("split request did not arrive: {error}"),
            }
        }
        let response = client.join().unwrap();
        assert!(response.ok);
        assert_eq!(response.status, "injected");
    }

    /// A client that keeps bytes arriving must not extend the read past the
    /// deadline: the deadline is checked on every iteration, not only when the
    /// pipe is empty.
    #[test]
    fn a_trickling_request_is_rejected_at_the_deadline() {
        let pipe = TestPipe::start(Duration::from_millis(200));
        let path = pipe.path.clone();
        let client = thread::spawn(move || {
            let mut stream = connect_with_retry(&path);
            let started = Instant::now();
            let mut wrote_for = Duration::ZERO;
            for _ in 0..40 {
                // No newline: only the deadline can end the read.
                if stream
                    .write_all(b"{")
                    .and_then(|()| stream.flush())
                    .is_err()
                {
                    break;
                }
                wrote_for = started.elapsed();
                thread::sleep(Duration::from_millis(50));
            }
            wrote_for
        });
        let wrote_for = client.join().unwrap();
        assert!(
            wrote_for < Duration::from_secs(1),
            "the read must end while bytes still arrive; the client wrote for {wrote_for:?}"
        );
    }

    /// Bytes after the first newline are not part of this request: the line is
    /// served without waiting for more input.
    #[test]
    fn trailing_bytes_after_the_newline_are_ignored() {
        let pipe = TestPipe::start(Duration::from_millis(500));
        let path = pipe.path.clone();
        let token = pipe.token.clone();
        let client = thread::spawn(move || {
            let mut stream = connect_with_retry(&path);
            let mut wire = serde_json::to_vec(&WireRequest::Roster(RosterRequest {
                token,
                id: "trailing-bytes".to_owned(),
            }))
            .unwrap();
            wire.push(b'\n');
            // No second newline: a reader that frames on the buffer's last
            // byte would wait for the deadline instead of serving the line.
            wire.extend_from_slice(b"{\"kind\":\"roster\"}");
            stream.write_all(&wire).unwrap();
            stream.flush().unwrap();
            serde_json::from_reader::<_, WireResponse>(BufReader::new(stream)).unwrap()
        });
        answer_one_roster(&pipe.events, Instant::now() + Duration::from_secs(2));
        assert_eq!(client.join().unwrap().status, "listed");
    }
}
