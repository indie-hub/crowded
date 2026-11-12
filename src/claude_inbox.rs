//! The loopback HTTP inbox a Claude native room's own mod polls.
//!
//! Crowded writes a connection file holding this inbox's URL and a token per
//! ROOM before the room's terminal starts. The mod registers, polls for mail
//! and receipts what it did with it. The token names the room; the session id
//! in the file name is only a lookup key and never an identity.

use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::{
    native_outbox::{AdapterResult, CLAUDE_HOLD, Message, STALE_HOLD},
    pane::session_state::create_private_directory,
};

/// The mod polls every 500 ms; four missed polls mean no live mod is bound.
const RECEIVER_TIMEOUT: Duration = Duration::from_millis(2000);
/// How long a send waits for the mod's receipt before calling it uncertain.
const RECEIPT_WAIT: Duration = Duration::from_secs(2);
const BODY_LIMIT: usize = 64 * 1024;
const HEAD_LIMIT: usize = 8 * 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
/// The hold for a room whose registered mod stopped polling.
pub(crate) const DISCONNECTED_HOLD: &str = "Claude mod disconnected";
/// The hold for mail no mod collected in time.
const UNCOLLECTED_HOLD: &str = "Claude mod did not collect the message";
/// What a started receipt says: submitted is not proof the agent acted on it.
const STARTED: &str = "submitted";

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// One room's inbox, alive for as long as its room is bound.
pub(crate) struct ClaudeInbox {
    url: String,
    token: String,
    state: Arc<Mutex<State>>,
    /// The connection file the room's mod reads, if one is written.
    file: Mutex<Option<PathBuf>>,
    now: Clock,
    wait: Duration,
}

#[derive(Default)]
struct State {
    generation: u64,
    receiver: Option<Receiver>,
    queue: VecDeque<Mail>,
    /// Claimed message ids and their content hash, until their receipt arrives.
    claimed: HashMap<String, String>,
    waiters: HashMap<String, mpsc::Sender<Outcome>>,
}

struct Receiver {
    id: String,
    last_poll: Instant,
}

struct Mail {
    id: String,
    text: String,
    sha256: String,
}

/// What the mod reported about a claimed message.
enum Outcome {
    Started,
    Held(String),
}

impl State {
    /// Drops everything that belongs to an earlier launch of the room.
    fn reset(&mut self, generation: u64) {
        self.generation = generation;
        self.receiver = None;
        self.queue.clear();
        self.claimed.clear();
        for (_, waiter) in self.waiters.drain() {
            let _ = waiter.send(Outcome::Held(STALE_HOLD.to_owned()));
        }
    }
}

impl ClaudeInbox {
    pub(crate) fn start() -> io::Result<Self> {
        Self::open(Arc::new(Instant::now), RECEIPT_WAIT)
    }

    fn open(now: Clock, wait: Duration) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let token = random_hex()?;
        let state = Arc::new(Mutex::new(State::default()));
        let (shared, served, clock) = (Arc::clone(&state), token.clone(), Arc::clone(&now));
        // shortcut: the listener thread ends with the process, not with the
        // room's binding; one inbox per room is reused by every relaunch, so a
        // stop signal is only needed if rooms are ever recycled without one.
        thread::spawn(move || serve(listener, shared, served, clock));
        Ok(Self {
            url,
            token,
            state,
            file: Mutex::new(None),
            now,
            wait,
        })
    }

    /// Names the launch generation and writes the file the room's mod reads.
    /// A room whose file cannot be written must not start.
    pub(crate) fn attach(&self, cwd: &Path, session: &str, generation: u64) -> io::Result<()> {
        self.begin_generation(generation);
        let path = connection_file(cwd, session)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("the Claude inbox file has no directory"))?;
        create_private_directory(parent)?;
        let contents = format!("{{\"url\":\"{}\",\"token\":\"{}\"}}", self.url, self.token);
        write_private(&path, &contents)?;
        if let Ok(mut file) = self.file.lock() {
            *file = Some(path);
        }
        Ok(())
    }

    /// Starts a new launch generation for the room: no receiver, no mail from
    /// an earlier launch, and no connection file a stale mod could read.
    pub(crate) fn begin_generation(&self, generation: u64) {
        let changed = self
            .state
            .lock()
            .map(|mut state| {
                if state.generation == generation {
                    return false;
                }
                state.reset(generation);
                true
            })
            .unwrap_or(false);
        if !changed {
            return;
        }
        if let Ok(mut file) = self.file.lock()
            && let Some(path) = file.take()
        {
            let _ = fs::remove_file(path);
        }
    }

    /// Queues a message for the room's mod and waits for its receipt. A mod
    /// that never collects the mail leaves it held; one that collects it and
    /// vanishes leaves the message uncertain, never resent.
    pub(crate) fn deliver(&self, message: &Message) -> AdapterResult {
        let (sender, receiver) = mpsc::channel();
        {
            let Ok(mut state) = self.state.lock() else {
                return AdapterResult::Uncertain;
            };
            if state.generation != message.binding.generation {
                return AdapterResult::Dropped(STALE_HOLD.to_owned());
            }
            let Some(registered) = &state.receiver else {
                return AdapterResult::Dropped(CLAUDE_HOLD.to_owned());
            };
            let live = (self.now)()
                .checked_duration_since(registered.last_poll)
                .is_none_or(|idle| idle <= RECEIVER_TIMEOUT);
            if !live {
                return AdapterResult::Dropped(DISCONNECTED_HOLD.to_owned());
            }
            state.queue.push_back(Mail {
                id: message.id.clone(),
                text: message.text.clone(),
                sha256: message.hash.clone(),
            });
            state.waiters.insert(message.id.clone(), sender);
        }
        match receiver.recv_timeout(self.wait) {
            Ok(Outcome::Started) => AdapterResult::Accepted(Some(STARTED.to_owned())),
            Ok(Outcome::Held(reason)) => AdapterResult::Dropped(reason),
            Err(_) => self.abandon(&message.id),
        }
    }

    /// Gives up on a receipt: mail no mod collected is removed, so it is never
    /// offered later; mail a mod already claimed stays uncertain.
    fn abandon(&self, id: &str) -> AdapterResult {
        let Ok(mut state) = self.state.lock() else {
            return AdapterResult::Uncertain;
        };
        state.waiters.remove(id);
        match state.queue.iter().position(|mail| mail.id == id) {
            Some(position) => {
                state.queue.remove(position);
                AdapterResult::Dropped(UNCOLLECTED_HOLD.to_owned())
            }
            None => AdapterResult::Uncertain,
        }
    }
}

impl Drop for ClaudeInbox {
    fn drop(&mut self) {
        if let Ok(mut file) = self.file.lock()
            && let Some(path) = file.take()
        {
            let _ = fs::remove_file(path);
        }
    }
}

/// The connection file of one launch of a room.
pub(crate) fn connection_file(cwd: &Path, session: &str) -> io::Result<PathBuf> {
    if !crate::codex_native::valid_uuid(session) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a Claude inbox file is named by a session id",
        ));
    }
    Ok(cwd
        .join(".crowded")
        .join("claude-inbox")
        .join(format!("{session}.json")))
}

fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("the Claude inbox file has no directory"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".claude-inbox-")
        .tempfile_in(parent)?;
    temp.write_all(contents.as_bytes())?;
    temp.flush()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o600))?;
    }
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn random_hex() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    #[cfg(unix)]
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    #[cfg(not(unix))]
    getrandom::fill(&mut bytes).map_err(io::Error::other)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn serve(listener: TcpListener, state: Arc<Mutex<State>>, token: String, now: Clock) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            break;
        };
        let _ = answer(stream, &state, &token, &now);
    }
}

/// Reads one request and writes one small JSON response. Nothing about the
/// request is logged: the token is never written anywhere but its file.
fn answer(
    mut stream: TcpStream,
    state: &Arc<Mutex<State>>,
    token: &str,
    now: &Clock,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let (status, body) = match read_request(&mut stream) {
        Ok(request) => respond(request, state, token, now),
        Err((status, message)) => (status, error_body(&message)),
    };
    let head = format!(
        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

struct Request {
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// Reads the head into bounded memory, then the body its Content-Length names.
fn read_request(stream: &mut TcpStream) -> Result<Request, (u16, String)> {
    let rejected = |message: io::Error| (400, message.to_string());
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(end) = buffer.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end;
        }
        let count = stream.read(&mut chunk).map_err(rejected)?;
        if count == 0 {
            return Err((400, "the request ended before its headers".to_owned()));
        }
        buffer.extend_from_slice(&chunk[..count]);
        if buffer.len() > HEAD_LIMIT {
            return Err((400, "the request headers are too large".to_owned()));
        }
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let mut body = buffer[end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let mut start = lines
        .next()
        .ok_or_else(|| (400, "the request has no request line".to_owned()))?
        .split(' ');
    let (method, path, version) = (
        start.next().unwrap_or_default(),
        start.next().unwrap_or_default().to_owned(),
        start.next().unwrap_or_default(),
    );
    if method != "POST" || !version.starts_with("HTTP/1.") {
        return Err((400, "the inbox accepts POST over HTTP/1".to_owned()));
    }
    let mut headers = HashMap::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| (400, "the request has a malformed header".to_owned()))?;
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| (400, "the request has no content length".to_owned()))?;
    if length > BODY_LIMIT {
        return Err((413, "the request body is too large".to_owned()));
    }
    while body.len() < length {
        let count = stream.read(&mut chunk).map_err(rejected)?;
        if count == 0 {
            return Err((400, "the request body ended early".to_owned()));
        }
        body.extend_from_slice(&chunk[..count]);
    }
    body.truncate(length);
    Ok(Request {
        path,
        headers,
        body,
    })
}

fn respond(request: Request, state: &Arc<Mutex<State>>, token: &str, now: &Clock) -> (u16, String) {
    if request.headers.contains_key("origin") {
        return (403, error_body("an Origin header is not accepted"));
    }
    let authorized = request
        .headers
        .get("authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|value| value == token);
    if !authorized {
        return (401, error_body("the token does not name this room"));
    }
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
        return (400, error_body("the request body is not JSON"));
    };
    let Ok(mut state) = state.lock() else {
        return (500, error_body("the inbox is unavailable"));
    };
    match request.path.as_str() {
        "/register" => register(&mut state, now),
        "/next" => next(&mut state, now, &body),
        "/receipt" => receipt(&mut state, &body),
        _ => (404, error_body("unknown inbox path")),
    }
}

fn register(state: &mut State, now: &Clock) -> (u16, String) {
    let live = state.receiver.as_ref().is_some_and(|receiver| {
        now()
            .checked_duration_since(receiver.last_poll)
            .is_none_or(|idle| idle <= RECEIVER_TIMEOUT)
    });
    if live {
        return (409, error_body("receiver exists"));
    }
    let id = match random_hex() {
        Ok(id) => id,
        Err(error) => return (500, error_body(&error.to_string())),
    };
    state.receiver = Some(Receiver {
        id: id.clone(),
        last_poll: now(),
    });
    (200, serde_json::json!({ "receiver": id }).to_string())
}

fn next(state: &mut State, now: &Clock, body: &serde_json::Value) -> (u16, String) {
    let Some(registered) = current_receiver(state, body) else {
        return (401, error_body("the receiver is not bound to this room"));
    };
    registered.last_poll = now();
    match state.queue.pop_front() {
        Some(mail) => {
            state.claimed.insert(mail.id.clone(), mail.sha256.clone());
            (
                200,
                serde_json::json!({
                    "id": mail.id,
                    "text": mail.text,
                    "sha256": mail.sha256,
                })
                .to_string(),
            )
        }
        None => (200, "null".to_owned()),
    }
}

fn receipt(state: &mut State, body: &serde_json::Value) -> (u16, String) {
    if current_receiver(state, body).is_none() {
        return (401, error_body("the receiver is not bound to this room"));
    }
    let field = |name: &str| body.get(name).and_then(|value| value.as_str());
    let (Some(id), Some(sha256), Some(state_name)) = (field("id"), field("sha256"), field("state"))
    else {
        return (
            409,
            error_body("the receipt does not match a claimed message"),
        );
    };
    if !state.claimed.get(id).is_some_and(|known| known == sha256) {
        return (
            409,
            error_body("the receipt does not match a claimed message"),
        );
    }
    state.claimed.remove(id);
    let outcome = match state_name {
        "started" => Outcome::Started,
        "held" => Outcome::Held(match field("reason") {
            Some(reason) if !reason.is_empty() => reason.to_owned(),
            _ => "the Claude mod held the prompt".to_owned(),
        }),
        _ => {
            return (
                409,
                error_body("the receipt does not match a claimed message"),
            );
        }
    };
    if let Some(waiter) = state.waiters.remove(id) {
        let _ = waiter.send(outcome);
    }
    (200, "{}".to_owned())
}

fn current_receiver<'a>(
    state: &'a mut State,
    body: &serde_json::Value,
) -> Option<&'a mut Receiver> {
    let id = body.get("receiver").and_then(|value| value.as_str());
    let registered = state.receiver.as_mut()?;
    (id == Some(registered.id.as_str())).then_some(registered)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        401 => "Unauthorized",
        _ => "Bad Request",
    }
}

fn error_body(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

/// One raw HTTP request, as the room's mod makes it.
/// One raw HTTP request, as the room's mod makes it.
#[cfg(test)]
pub(crate) fn request(
    url: &str,
    token: Option<&str>,
    path: &str,
    body: &str,
    origin: Option<&str>,
) -> (u16, String) {
    let mut head = format!(
        "POST {path} HTTP/1.1\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    if let Some(token) = token {
        head += &format!("authorization: Bearer {token}\r\n");
    }
    if let Some(origin) = origin {
        head += &format!("origin: {origin}\r\n");
    }
    head += &format!("\r\n{body}");
    let mut stream = TcpStream::connect(url.trim_start_matches("http://")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(head.as_bytes()).unwrap();
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    let (status, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = status
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_owned())
}

/// A stand-in for the room's mod, speaking the inbox's HTTP protocol.
#[cfg(all(test, unix))]
pub(crate) struct FakeMod {
    pub(crate) url: String,
    pub(crate) token: String,
    pub(crate) receiver: String,
}

#[cfg(all(test, unix))]
impl FakeMod {
    pub(crate) fn open(url: &str, token: &str) -> Self {
        Self {
            url: url.to_owned(),
            token: token.to_owned(),
            receiver: String::new(),
        }
    }

    pub(crate) fn register(url: &str, token: &str) -> Self {
        let mut mod_ = Self::open(url, token);
        let (status, body) = request(url, Some(token), "/register", "{\"session\":\"s\"}", None);
        assert_eq!(status, 200, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        mod_.receiver = body["receiver"].as_str().unwrap().to_owned();
        mod_
    }

    fn post(&self, path: &str, body: String) -> (u16, String) {
        request(&self.url, Some(&self.token), path, &body, None)
    }

    pub(crate) fn register_again(&self) -> u16 {
        request(
            &self.url,
            Some(&self.token),
            "/register",
            "{\"session\":\"s\"}",
            None,
        )
        .0
    }

    /// The oldest offered message, or `None` when the mod has no mail.
    pub(crate) fn next_claim(&self) -> Option<Claim> {
        let (status, body) = self.post("/next", format!("{{\"receiver\":\"{}\"}}", self.receiver));
        assert_eq!(status, 200, "{body}");
        let mail: serde_json::Value = serde_json::from_str(&body).unwrap();
        mail.as_object().map(|mail| Claim {
            id: mail["id"].as_str().unwrap().to_owned(),
            text: mail["text"].as_str().unwrap().to_owned(),
            sha256: mail["sha256"].as_str().unwrap().to_owned(),
        })
    }

    pub(crate) fn next_status(&self) -> u16 {
        self.post("/next", format!("{{\"receiver\":\"{}\"}}", self.receiver))
            .0
    }

    /// Claims and returns the next message, waiting for the sender to queue it.
    pub(crate) fn claim(&self) -> Claim {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(claim) = self.next_claim() {
                return claim;
            }
            assert!(
                Instant::now() < deadline,
                "the inbox never offered the message"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn receipt(&self, claim: &Claim, state: &str, reason: &str) -> u16 {
        self.post(
            "/receipt",
            format!(
                "{{\"receiver\":\"{}\",\"id\":\"{}\",\"sha256\":\"{}\",\"state\":\"{state}\",\"reason\":\"{reason}\"}}",
                self.receiver, claim.id, claim.sha256
            ),
        )
        .0
    }
}

#[cfg(all(test, unix))]
pub(crate) struct Claim {
    pub(crate) id: String,
    pub(crate) text: String,
    pub(crate) sha256: String,
}

/// The mod that reads a room's written connection file, as the room does.
#[cfg(all(test, unix))]
pub(crate) fn mod_of(cwd: &Path, session: &str) -> FakeMod {
    let path = connection_file(cwd, session).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    FakeMod::open(
        value["url"].as_str().unwrap(),
        value["token"].as_str().unwrap(),
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::native_outbox::Binding;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn inbox(wait: Duration) -> Arc<ClaudeInbox> {
        let inbox = ClaudeInbox::open(Arc::new(Instant::now), wait).unwrap();
        inbox.begin_generation(1);
        Arc::new(inbox)
    }

    fn message(generation: u64, id: &str, text: &str) -> Message {
        Message {
            id: id.to_owned(),
            source: "Sender".to_owned(),
            destination: "1".to_owned(),
            binding: Binding {
                room: "1".to_owned(),
                session: "session".to_owned(),
                endpoint: "claude".to_owned(),
                generation,
            },
            text: text.to_owned(),
            hash: format!("{:x}", Sha256::digest(text.as_bytes())),
            created_ms: 0,
            sequence: 1,
            attempts: Vec::new(),
            receipt: None,
            status: crate::native_outbox::Status::Sending,
        }
    }

    fn delivered(inbox: Arc<ClaudeInbox>, message: Message) -> thread::JoinHandle<AdapterResult> {
        thread::spawn(move || inbox.deliver(&message))
    }

    /// The token names the room, an Origin header is refused, and a body beyond
    /// the limit is rejected before it is parsed.
    #[test]
    fn the_inbox_refuses_a_missing_token_an_origin_and_an_oversized_body() {
        let inbox = inbox(Duration::from_millis(200));
        let body = "{\"session\":\"s\"}";
        assert_eq!(request(&inbox.url, None, "/register", body, None).0, 401);
        assert_eq!(
            request(&inbox.url, Some("00"), "/register", body, None).0,
            401
        );
        assert_eq!(
            request(
                &inbox.url,
                Some(&inbox.token),
                "/register",
                body,
                Some("http://x")
            )
            .0,
            403
        );
        let oversized = format!("{{\"session\":\"{}\"}}", "s".repeat(BODY_LIMIT));
        assert_eq!(
            request(
                &inbox.url,
                Some(&inbox.token),
                "/register",
                &oversized,
                None
            )
            .0,
            413
        );
        assert_eq!(
            request(&inbox.url, Some(&inbox.token), "/register", body, None).0,
            200
        );
    }

    /// One live receiver per room, one delivery per message, and the mod's
    /// receipt decides whether the message was accepted or held.
    #[test]
    fn a_room_offers_mail_once_and_receipts_settle_it() {
        let inbox = inbox(Duration::from_secs(2));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        assert_eq!(mod_.register_again(), 409, "a live receiver is kept");

        let accepted = delivered(Arc::clone(&inbox), message(1, "id-1", "hello\r\nworld"));
        let claim = mod_.claim();
        assert_eq!(claim.text, "hello\r\nworld");
        assert!(
            mod_.next_claim().is_none(),
            "a claimed message is not offered again"
        );
        assert_eq!(mod_.receipt(&claim, "started", ""), 200);
        assert_eq!(
            mod_.receipt(&claim, "started", ""),
            409,
            "one receipt per id"
        );
        assert!(matches!(
            accepted.join().unwrap(),
            AdapterResult::Accepted(Some(_))
        ));

        let held = delivered(Arc::clone(&inbox), message(1, "id-2", "hold me"));
        let claim = mod_.claim();
        assert_eq!(mod_.receipt(&claim, "held", "prompt is open"), 200);
        assert!(matches!(
            held.join().unwrap(),
            AdapterResult::Dropped(reason) if reason == "prompt is open"
        ));
    }

    /// A mod that claims a message and vanishes leaves it uncertain and it is
    /// never offered again; mail no mod collects is held and withdrawn.
    #[test]
    fn a_vanished_receiver_leaves_mail_uncertain_and_uncollected_mail_is_held() {
        let inbox = inbox(Duration::from_millis(200));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let vanished = delivered(Arc::clone(&inbox), message(1, "id-3", "gone"));
        assert_eq!(mod_.claim().id, "id-3");
        assert!(matches!(vanished.join().unwrap(), AdapterResult::Uncertain));
        assert!(mod_.next_claim().is_none(), "id-3 was offered again");
        assert!(matches!(
            inbox.deliver(&message(1, "id-4", "stuck")),
            AdapterResult::Dropped(reason) if reason == UNCOLLECTED_HOLD
        ));
        assert!(
            mod_.next_claim().is_none(),
            "id-4 was offered after its hold"
        );
    }

    /// Liveness is judged by missed polls on the inbox's own clock, and mail
    /// from an earlier launch is never offered to the next launch's receiver.
    #[test]
    fn mail_from_an_earlier_generation_is_held_and_never_offered() {
        let ms = Arc::new(AtomicU64::new(0));
        let base = Instant::now();
        let hand = Arc::clone(&ms);
        let clock: Clock =
            Arc::new(move || base + Duration::from_millis(hand.load(Ordering::SeqCst)));
        let inbox = Arc::new(ClaudeInbox::open(clock, Duration::from_millis(600)).unwrap());
        inbox.begin_generation(1);
        let _mod = FakeMod::register(&inbox.url, &inbox.token);

        let queued = delivered(Arc::clone(&inbox), message(1, "id-5", "old"));
        thread::sleep(Duration::from_millis(100));
        inbox.begin_generation(2);
        assert!(matches!(
            queued.join().unwrap(),
            AdapterResult::Dropped(reason) if reason == STALE_HOLD
        ));
        let next_launch = FakeMod::register(&inbox.url, &inbox.token);
        assert!(
            next_launch.next_claim().is_none(),
            "older-generation mail reached a receiver"
        );

        ms.store(
            (RECEIVER_TIMEOUT.as_millis() as u64) + 1000,
            Ordering::SeqCst,
        );
        assert!(matches!(
            inbox.deliver(&message(2, "id-6", "late")),
            AdapterResult::Dropped(reason) if reason == DISCONNECTED_HOLD
        ));
        assert_eq!(
            FakeMod::open(&inbox.url, &inbox.token).register_again(),
            200,
            "a silent receiver is replaced"
        );
    }
}
