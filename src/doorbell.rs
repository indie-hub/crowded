//! Authenticated local envelopes entering through a Unix socket.

use std::{
    collections::{HashSet, VecDeque},
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener, net::UnixStream},
    path::{Path, PathBuf},
    process,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

use crate::pane::GuestEnvironment;

const MAX_WIRE_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024;
const MAX_ID_BYTES: usize = 128;
const MAX_LABEL_BYTES: usize = 64;
const MAX_HOPS: u8 = 8;
const MAX_MESSAGES_PER_SECOND: usize = 5;
const EVENT_QUEUE_CAPACITY: usize = 100;
const DEDUPE_CAPACITY: usize = 256;

#[derive(Deserialize, Serialize)]
struct WireEnvelope {
    token: String,
    id: String,
    to: usize,
    body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(default)]
    hop: u8,
}

#[derive(Serialize, Deserialize)]
struct WireResponse {
    ok: bool,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    envelope_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl WireResponse {
    fn accepted(status: &str, envelope_id: Option<u64>) -> Self {
        Self {
            ok: true,
            status: status.to_owned(),
            envelope_id,
            error: None,
        }
    }

    fn rejected(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            status: "rejected".to_owned(),
            envelope_id: None,
            error: Some(error.into()),
        }
    }
}

pub(crate) struct DoorbellEnvelope {
    pub(crate) from: usize,
    pub(crate) to: usize,
    pub(crate) body: String,
    pub(crate) task: Option<String>,
    pub(crate) role: Option<String>,
    reply: SyncSender<WireResponse>,
}

impl DoorbellEnvelope {
    pub(crate) fn reply_injected(&self, envelope_id: u64) {
        let _ = self
            .reply
            .send(WireResponse::accepted("injected", Some(envelope_id)));
    }

    pub(crate) fn reply_queued(&self, envelope_id: u64) {
        let _ = self
            .reply
            .send(WireResponse::accepted("queued", Some(envelope_id)));
    }

    pub(crate) fn reply_failed(&self, error: impl Into<String>) {
        let _ = self.reply.send(WireResponse::rejected(error));
    }
}

pub(crate) struct Doorbell {
    path: PathBuf,
    tokens: Vec<String>,
    events: Receiver<DoorbellEnvelope>,
    stop: Arc<AtomicBool>,
    listener_thread: Option<JoinHandle<()>>,
}

impl Doorbell {
    pub(crate) fn start(room_count: usize) -> io::Result<Self> {
        let path = env::temp_dir().join(format!("crowded-{}.sock", process::id()));
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

    pub(crate) fn try_recv(&self) -> Result<DoorbellEnvelope, TryRecvError> {
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
    events: SyncSender<DoorbellEnvelope>,
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
                    Ok(request) => {
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
                                    let event = DoorbellEnvelope {
                                        from,
                                        to: request.to - 1,
                                        body: request.body,
                                        task: request.task,
                                        role: request.role,
                                        reply,
                                    };
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
                    Err(error) => Some(WireResponse::rejected(error.to_string())),
                }
                .unwrap_or_else(|| WireResponse::rejected("Mailroom response timed out"));
                let _ = write_response(&mut stream, &response);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn read_request(stream: &UnixStream) -> io::Result<WireEnvelope> {
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

fn validate_request(
    request: &WireEnvelope,
    tokens: &[String],
    room_count: usize,
    recent_by_room: &mut [VecDeque<Instant>],
) -> Result<usize, String> {
    let from = tokens
        .iter()
        .position(|token| token == &request.token)
        .ok_or_else(|| "invalid capability".to_owned())?;
    if !(1..=room_count).contains(&request.to) {
        return Err("target room does not exist".to_owned());
    }
    if request.to - 1 == from {
        return Err("source and target rooms must differ".to_owned());
    }
    if request.body.is_empty() || request.body.len() > MAX_BODY_BYTES {
        return Err("message body must contain 1..=4096 bytes".to_owned());
    }
    if request.id.is_empty() || request.id.len() > MAX_ID_BYTES {
        return Err("message id must contain 1..=128 bytes".to_owned());
    }
    validate_label("task", request.task.as_deref())?;
    validate_label("role", request.role.as_deref())?;
    if request.hop > MAX_HOPS {
        return Err("message exceeded hop limit".to_owned());
    }

    let now = Instant::now();
    let recent = &mut recent_by_room[from];
    while recent
        .front()
        .is_some_and(|sent| now.duration_since(*sent) >= Duration::from_secs(1))
    {
        recent.pop_front();
    }
    if recent.len() >= MAX_MESSAGES_PER_SECOND {
        return Err("source exceeded 5 messages per second".to_owned());
    }
    recent.push_back(now);
    Ok(from)
}

fn validate_label(name: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value
        && (value.is_empty()
            || value.len() > MAX_LABEL_BYTES
            || value.chars().any(|character| character.is_control()))
    {
        return Err(format!(
            "{name} must contain 1..={MAX_LABEL_BYTES} bytes without control characters"
        ));
    }
    Ok(())
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

struct SendArgs {
    target: usize,
    task: Option<String>,
    role: Option<String>,
    body: String,
}

const SEND_USAGE: &str = "usage: crowded send ROOM [--task ID] [--role ROLE] [--] MESSAGE";

fn parse_send_args(args: impl IntoIterator<Item = String>) -> Result<SendArgs, String> {
    let mut args = args.into_iter();
    let target = args
        .next()
        .ok_or_else(|| SEND_USAGE.to_owned())?
        .parse()
        .map_err(|_| SEND_USAGE.to_owned())?;
    let mut task = None;
    let mut role = None;
    let mut body = Vec::new();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--task" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--task requires an ID".to_owned())?;
                if task.replace(value).is_some() {
                    return Err("--task may appear only once".to_owned());
                }
            }
            "--role" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--role requires a role".to_owned())?;
                if role.replace(value).is_some() {
                    return Err("--role may appear only once".to_owned());
                }
            }
            "--" => {
                body.extend(args);
                break;
            }
            _ => {
                body.push(argument);
                body.extend(args);
                break;
            }
        }
    }

    let body = body.join(" ");
    if body.is_empty() {
        return Err(SEND_USAGE.to_owned());
    }
    validate_label("task", task.as_deref())?;
    validate_label("role", role.as_deref())?;
    Ok(SendArgs {
        target,
        task,
        role,
        body,
    })
}

pub(crate) fn send_command() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_send_args(env::args().skip(2))?;

    let path = env::var_os("CROWDED_SOCKET").ok_or("CROWDED_SOCKET is not set")?;
    let token = env::var("CROWDED_TOKEN").map_err(|_| "CROWDED_TOKEN is not set")?;
    let room = env::var("CROWDED_ROOM").unwrap_or_else(|_| "external".to_owned());
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let request = WireEnvelope {
        token,
        id: format!("{room}-{}-{now}", process::id()),
        to: args.target,
        body: args.body,
        task: args.task,
        role: args.role,
        hop: 0,
    };

    let mut stream = UnixStream::connect(path)?;
    serde_json::to_writer(&mut stream, &request)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let response: WireResponse = serde_json::from_reader(BufReader::new(stream))?;
    println!("{}", serde_json::to_string(&response)?);
    if response.ok {
        Ok(())
    } else {
        Err(response
            .error
            .unwrap_or_else(|| "Doorbell rejected envelope".to_owned())
            .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_binds_identity_and_limits_hops() {
        let tokens = vec!["left".to_owned(), "right".to_owned()];
        let mut recent = vec![VecDeque::new(), VecDeque::new()];
        let mut request = WireEnvelope {
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
    fn request_validation_rejects_control_characters_in_hats() {
        let tokens = vec!["left".to_owned(), "right".to_owned()];
        let mut recent = vec![VecDeque::new(), VecDeque::new()];
        let request = WireEnvelope {
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
}
