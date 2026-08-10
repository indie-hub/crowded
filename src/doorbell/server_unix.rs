//! Authenticated Unix-socket transport for Doorbell events.

use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener, net::UnixStream},
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

use super::protocol::*;
use crate::pane::GuestEnvironment;

static SOCKET_NONCE: AtomicU64 = AtomicU64::new(1);

pub(crate) struct Doorbell {
    path: PathBuf,
    tokens: Vec<String>,
    events: Receiver<DoorbellEvent>,
    stop: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
}

impl Doorbell {
    pub(crate) fn start(room_count: usize) -> io::Result<Self> {
        let nonce = SOCKET_NONCE.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!("crowded-{}-{nonce}.sock", process::id()));
        if path.exists() {
            fs::remove_file(&path)?;
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let tokens = (0..room_count)
            .map(|_| capability_token())
            .collect::<io::Result<Vec<_>>>()?;
        let thread_tokens = tokens.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (event_tx, events) = mpsc::sync_channel(EVENT_QUEUE_CAPACITY);
        let listener_thread = thread::spawn(move || {
            listener_loop(listener, thread_tokens, room_count, event_tx, thread_stop);
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
        if let Some(handle) = self.listener_thread.take() {
            let _ = handle.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

fn capability_token() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    let mut random = fs::File::open("/dev/urandom")?;
    random.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn listener_loop(
    listener: UnixListener,
    tokens: Vec<String>,
    room_count: usize,
    events: SyncSender<DoorbellEvent>,
    stop: Arc<AtomicBool>,
) {
    let mut recent_by_room = vec![VecDeque::<Instant>::new(); room_count];
    let mut seen = HashSet::<String>::new();
    let mut seen_order = VecDeque::<String>::new();

    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                let request = read_request(&stream);
                let response = match request {
                    Ok(WireRequest::Message(request)) => {
                        if seen.contains(&request.id) {
                            Some(WireResponse::accepted("duplicate", None))
                        } else {
                            match validate_request(
                                &request,
                                &tokens,
                                room_count,
                                &mut recent_by_room,
                            ) {
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
                            match validate_control(
                                &request,
                                &tokens,
                                room_count,
                                &mut recent_by_room,
                            ) {
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
                                Ok(from) => {
                                    match events.try_send(DoorbellEvent::Pulse(DoorbellPulse {
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
                                match events
                                    .try_send(DoorbellEvent::Roster(DoorbellRoster { reply }))
                                {
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
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn read_request(stream: &UnixStream) -> io::Result<WireRequest> {
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

fn write_response(stream: &mut UnixStream, response: &WireResponse) -> io::Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doorbell::commands::{parse_control_args, parse_send_args};

    #[test]
    fn request_validation_binds_identity_and_limits_hops() {
        let tokens = vec!["left".to_owned(), "right".to_owned()];
        let mut recent = vec![VecDeque::new(), VecDeque::new()];
        let mut request = MessageRequest {
            token: "left".to_owned(),
            id: "message-1".to_owned(),
            to: 2,
            body: "hello".to_owned(),
            task: None,
            role: None,
            hop: 0,
        };
        assert_eq!(validate_request(&request, &tokens, 2, &mut recent), Ok(0));
        request.hop = MAX_HOPS + 1;
        assert_eq!(
            validate_request(&request, &tokens, 2, &mut recent),
            Err("message exceeded hop limit".to_owned())
        );
    }

    #[test]
    fn send_arguments_support_temporary_task_roles_and_plain_messages() {
        let hatted = parse_send_args(
            [
                "2",
                "--task",
                "parser-fix",
                "--role",
                "reviewer",
                "inspect",
                "this",
            ]
            .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(hatted.target, 2);
        assert_eq!(hatted.task.as_deref(), Some("parser-fix"));
        assert_eq!(hatted.role.as_deref(), Some("reviewer"));
        assert_eq!(hatted.body, "inspect this");

        let plain =
            parse_send_args(["1", "hello", "--role", "is text"].map(str::to_owned)).unwrap();
        assert_eq!(plain.role, None);
        assert_eq!(plain.body, "hello --role is text");
    }

    #[test]
    fn control_arguments_are_structured_and_validated() {
        assert_eq!(
            parse_control_args(["2", "clear"].map(str::to_owned)).unwrap(),
            (2, ControlAction::ClearContext)
        );
        assert_eq!(
            parse_control_args(["2", "resume"].map(str::to_owned)).unwrap(),
            (2, ControlAction::Resume)
        );
        assert!(parse_control_args(["2", "resume", "extra"].map(str::to_owned)).is_err());
        assert!(parse_control_args(["2", "model", "a", "resume"].map(str::to_owned)).is_err());
        assert_eq!(
            parse_control_args(["3", "model", "openai/gpt-5"].map(str::to_owned)).unwrap(),
            (
                3,
                ControlAction::Configure {
                    model: Some("openai/gpt-5".to_owned()),
                    effort: None
                }
            )
        );
        assert_eq!(
            parse_control_args(["1", "effort", "xhigh"].map(str::to_owned)).unwrap(),
            (
                1,
                ControlAction::Configure {
                    model: None,
                    effort: Some(Effort::Xhigh)
                }
            )
        );
        assert_eq!(
            parse_control_args(["3", "model", "gpt-5", "effort", "high"].map(str::to_owned))
                .unwrap(),
            (
                3,
                ControlAction::Configure {
                    model: Some("gpt-5".to_owned()),
                    effort: Some(Effort::High)
                }
            )
        );
        assert_eq!(
            parse_control_args(["1", "effort", "low", "model", "sonnet"].map(str::to_owned))
                .unwrap(),
            (
                1,
                ControlAction::Configure {
                    model: Some("sonnet".to_owned()),
                    effort: Some(Effort::Low)
                }
            )
        );
        assert!(parse_control_args(["2", "effort", "wild"].map(str::to_owned)).is_err());
        assert!(parse_control_args(["2", "model", "a", "model", "b"].map(str::to_owned)).is_err());
        assert!(
            parse_control_args(["2", "effort", "high", "effort", "low"].map(str::to_owned))
                .is_err()
        );
        assert!(parse_control_args(["2", "model", "a", "clear"].map(str::to_owned)).is_err());

        let request = ControlRequest {
            token: "left".to_owned(),
            id: "control-1".to_owned(),
            to: 2,
            action: ControlAction::Configure {
                model: Some("-unsafe".to_owned()),
                effort: None,
            },
        };
        let wire = serde_json::to_value(WireRequest::Control(ControlRequest {
            token: "left".to_owned(),
            id: "control-wire".to_owned(),
            to: 2,
            action: ControlAction::Configure {
                model: Some("sonnet".to_owned()),
                effort: Some(Effort::High),
            },
        }))
        .unwrap();
        assert_eq!(wire["kind"], "control");
        assert_eq!(wire["action"], "configure");
        assert_eq!(wire["value"]["model"], "sonnet");
        assert_eq!(wire["value"]["effort"], "high");

        let mut recent = vec![VecDeque::new(), VecDeque::new()];
        assert_eq!(
            validate_control(
                &request,
                &["left".to_owned(), "right".to_owned()],
                2,
                &mut recent,
            ),
            Err(
                "model must contain 1..=128 bytes, must not start with `-`, and must not contain controls"
                    .to_owned()
            )
        );
    }

    #[test]
    fn request_validation_rejects_control_characters_in_hats() {
        let tokens = vec!["left".to_owned(), "right".to_owned()];
        let mut recent = vec![VecDeque::new(), VecDeque::new()];
        let request = MessageRequest {
            token: "left".to_owned(),
            id: "message-1".to_owned(),
            to: 2,
            body: "hello".to_owned(),
            task: Some("bad\ntask".to_owned()),
            role: None,
            hop: 0,
        };
        assert_eq!(
            validate_request(&request, &tokens, 2, &mut recent),
            Err("task must contain 1..=64 bytes without control characters".to_owned())
        );
    }

    #[test]
    fn capability_tokens_are_random_and_socket_is_private() {
        let doorbell = Doorbell::start(2).unwrap();
        assert_ne!(doorbell.tokens[0], doorbell.tokens[1]);
        let mode = fs::metadata(doorbell.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let path = doorbell.path().to_owned();
        drop(doorbell);
        assert!(!path.exists());
    }

    #[test]
    fn control_crosses_the_authenticated_doorbell() {
        let doorbell = Doorbell::start(2).unwrap();
        let path = doorbell.path().to_owned();
        let token = doorbell.tokens[0].clone();
        let client = thread::spawn(move || {
            let mut stream = UnixStream::connect(path).unwrap();
            serde_json::to_writer(
                &mut stream,
                &WireRequest::Control(ControlRequest {
                    token,
                    id: "control-roundtrip".to_owned(),
                    to: 2,
                    action: ControlAction::Configure {
                        model: None,
                        effort: Some(Effort::High),
                    },
                }),
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
            serde_json::from_reader::<_, WireResponse>(BufReader::new(stream)).unwrap()
        });

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match doorbell.try_recv() {
                Ok(DoorbellEvent::Control(control)) => {
                    assert_eq!(control.from, 0);
                    assert_eq!(control.to, 1);
                    assert_eq!(
                        control.action,
                        ControlAction::Configure {
                            model: None,
                            effort: Some(Effort::High)
                        }
                    );
                    control.reply_applied();
                    break;
                }
                Ok(_) => panic!("unexpected Doorbell event"),
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("control did not arrive: {error}"),
            }
        }
        let response = client.join().unwrap();
        assert!(response.ok);
        assert_eq!(response.status, "applied");
    }

    #[test]
    fn pulse_validation_binds_room_identity() {
        let request = PulseRequest {
            token: "left".to_owned(),
            id: "pulse-1".to_owned(),
            state: PulseState::Working,
        };
        assert_eq!(
            validate_pulse(&request, &["left".to_owned(), "right".to_owned()]),
            Ok(0)
        );
    }

    #[test]
    fn roster_is_authenticated_and_machine_readable() {
        let doorbell = Doorbell::start(2).unwrap();
        let path = doorbell.path().to_owned();
        let token = doorbell.tokens[0].clone();
        let client = thread::spawn(move || {
            let mut stream = UnixStream::connect(path).unwrap();
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

        let deadline = Instant::now() + Duration::from_secs(1);
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
                        allow_control: true,
                        model: None,
                        effort: None,
                        headroom: true,
                        pulse_age_ms: None,
                        capabilities: Default::default(),
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
        assert_eq!(response["rooms"][0]["name"], "Builder");
        assert_eq!(response["rooms"][0]["vendor"], "openai");
        assert_eq!(response["rooms"][0]["state"], "ready");
        assert_eq!(response["rooms"][0]["headroom"], true);
    }
}
