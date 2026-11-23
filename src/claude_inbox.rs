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
    native_outbox::{AdapterResult, Binding, Message},
    native_session::{
        CLAUDE_HOLD, DISCONNECTED_HOLD, MOD_HOLD_PREFIX, STALE_HOLD, UNCOLLECTED_HOLD,
    },
    pane::session_state::create_private_directory,
};

/// The mod polls every 500 ms; four missed polls mean no live mod is bound.
const RECEIVER_TIMEOUT: Duration = Duration::from_millis(2000);
/// How long a send waits for the mod's receipt before calling it uncertain.
const RECEIPT_WAIT: Duration = Duration::from_secs(2);
const BODY_LIMIT: usize = 64 * 1024;
const HEAD_LIMIT: usize = 8 * 1024;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(2);
/// The hold for mail the mod could not receive inside its response limit.
pub(crate) const TOO_LARGE_HOLD: &str = "the message is too large for the Claude mod";
/// A whole request must arrive within this, so no client can hold the listener.
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
/// The mod refuses a response over 64 KiB; stay below it with room to spare.
const RESPONSE_LIMIT: usize = 60 * 1024;
/// What a started receipt says: submitted is not proof the agent acted on it.
const STARTED: &str = "submitted";
/// Bound on in-memory claim and late-receipt state so a flood of timeouts
/// cannot grow either without limit.
const MAX_CLAIMED: usize = 64;

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// One room's inbox, alive for as long as its room is bound.
pub(crate) struct ClaudeInbox {
    url: String,
    token: String,
    state: Arc<Mutex<State>>,
    /// The connection file the room's mod reads, if one is written.
    file: Mutex<Option<PathBuf>>,
    /// Bootstrap connection file written before the room's session is known.
    bootstrap: Mutex<Option<PathBuf>>,
    now: Clock,
    wait: Mutex<Duration>,
}

#[derive(Default)]
struct State {
    generation: u64,
    receiver: Option<Receiver>,
    queue: VecDeque<Mail>,
    /// Claimed message ids with their content hash and binding, until their
    /// receipt arrives. The binding lets a late receipt settle the outbox row.
    claimed: HashMap<String, Claimed>,
    waiters: HashMap<String, mpsc::Sender<Outcome>>,
    /// Authenticated receipts that arrived after deliver() timed out, keyed
    /// by message id. The inbox owns each entry until the router acks it
    /// after observing the row durably settled.
    late: HashMap<String, LateReceipt>,
}

struct Claimed {
    sha256: String,
    binding: Binding,
}

/// An authenticated receipt that arrived after deliver() timed out. `held`
/// carries the mod's reason; `None` means the prompt was submitted.
#[derive(Clone)]
pub(crate) struct LateReceipt {
    pub(crate) id: String,
    pub(crate) sha256: String,
    pub(crate) binding: Binding,
    pub(crate) held: Option<String>,
}

struct Receiver {
    id: String,
    last_poll: Instant,
}

struct Mail {
    id: String,
    text: String,
    sha256: String,
    binding: Binding,
}

/// What the mod reported about a claimed message.
enum Outcome {
    Started,
    Held(String),
}

impl State {
    /// Moves the room to `generation`. Mail and claims of the old generation
    /// are dropped and their senders told the mail is stale. A report rebind
    /// keeps the receiver: the mod registers once per process and gets no
    /// session start after the user clears the session. Only a relaunched
    /// process replaces it.
    fn begin(&mut self, generation: u64, keep_receiver: bool) {
        self.generation = generation;
        if !keep_receiver {
            self.receiver = None;
        }
        self.queue.clear();
        self.claimed.clear();
        // Retained late evidence survives a generation change: it settles its
        // own old row, and resolve checks the row's own binding. Only a
        // router or Crowded restart drops it, leaving the row Uncertain.
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
            bootstrap: Mutex::new(None),
            now,
            wait: Mutex::new(wait),
        })
    }

    /// Names the launch generation and writes the file the room's mod reads.
    /// A room whose file cannot be written must not start.
    pub(crate) fn attach(
        &self,
        cwd: &Path,
        room: usize,
        session: &str,
        generation: u64,
    ) -> io::Result<()> {
        self.relaunch(generation);
        let path = connection_file(cwd, session)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("the Claude inbox file has no directory"))?;
        create_private_directory(parent)?;
        self.write_connection(&path, room)?;
        let bootstrap = bootstrap_file(cwd, room)?;
        if let Err(error) = self.write_connection(&bootstrap, room) {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        if let Ok(mut file) = self.file.lock() {
            *file = Some(path);
        }
        if let Ok(mut stored) = self.bootstrap.lock() {
            *stored = Some(bootstrap);
        }
        Ok(())
    }

    /// Prepares the inbox for a fresh launch whose session will be learned from
    /// the room's hook. Writes a per-room bootstrap connection file the mod can
    /// find via `CROWDED_ROOM`.
    pub(crate) fn prepare(&self, cwd: &Path, room: usize, generation: u64) -> io::Result<()> {
        self.relaunch(generation);
        let path = bootstrap_file(cwd, room)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("the Claude inbox file has no directory"))?;
        create_private_directory(parent)?;
        self.write_connection(&path, room)?;
        if let Ok(mut bootstrap) = self.bootstrap.lock() {
            *bootstrap = Some(path);
        }
        Ok(())
    }

    /// Binds a reported session to an inbox already connected via its per-room
    /// bootstrap file. The mod's receiver stays bound and the bootstrap remains
    /// the room's fallback until the next relaunch.
    pub(crate) fn adopt_reported_session(
        &self,
        cwd: &Path,
        room: usize,
        session: &str,
        generation: u64,
    ) -> io::Result<()> {
        self.retarget(generation);
        let path = connection_file(cwd, session)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("the Claude inbox file has no directory"))?;
        create_private_directory(parent)?;
        self.write_connection(&path, room)?;
        if let Ok(mut file) = self.file.lock() {
            if let Some(previous) = file.replace(path.clone())
                && previous != path
            {
                let _ = fs::remove_file(previous);
            }
        }
        Ok(())
    }

    fn write_connection(&self, path: &Path, room: usize) -> io::Result<()> {
        let contents = format!(
            "{{\"url\":\"{}\",\"token\":\"{}\",\"room\":{room}}}",
            self.url, self.token
        );
        write_private(path, &contents)
    }

    /// Moves the room to a session its own hook reported. The mod's process
    /// keeps running and cannot register again, so its receiver stays bound
    /// while only the new generation's mail is offered.
    pub(crate) fn retarget(&self, generation: u64) {
        if let Ok(mut state) = self.state.lock()
            && state.generation != generation
        {
            state.begin(generation, true);
        }
    }

    /// Starts a new process of the room: no receiver survives it, no mail from
    /// an earlier launch, and no connection file a stale mod could read.
    fn relaunch(&self, generation: u64) {
        if let Ok(mut state) = self.state.lock() {
            state.begin(generation, false);
        }
        if let Ok(mut file) = self.file.lock()
            && let Some(path) = file.take()
        {
            let _ = fs::remove_file(path);
        }
        if let Ok(mut bootstrap) = self.bootstrap.lock()
            && let Some(path) = bootstrap.take()
        {
            let _ = fs::remove_file(path);
        }
    }

    /// Queues a message for the room's mod and waits for its receipt. A mod
    /// that never collects the mail leaves it held; one that collects it and
    /// vanishes leaves the message uncertain, never resent. The waiter's
    /// channel is re-checked inside abandon(): an outcome the mod's receipt
    /// already sent wins over the timeout, so neither path drops evidence.
    pub(crate) fn deliver(&self, message: &Message) -> AdapterResult {
        let (sender, receiver) = mpsc::channel();
        {
            if escaped_size(&message.text) > RESPONSE_LIMIT {
                return AdapterResult::Dropped(TOO_LARGE_HOLD.to_owned());
            }
            let Ok(mut state) = self.state.lock() else {
                return AdapterResult::Uncertain;
            };
            if state.generation != message.binding.generation {
                return AdapterResult::Dropped(STALE_HOLD.to_owned());
            }
            let Some(registered) = &state.receiver else {
                return AdapterResult::Dropped(CLAUDE_HOLD.to_owned());
            };
            if !self.receiver_is_live(registered) {
                return AdapterResult::Dropped(DISCONNECTED_HOLD.to_owned());
            }
            state.queue.push_back(Mail {
                id: message.id.clone(),
                text: message.text.clone(),
                sha256: message.hash.clone(),
                binding: message.binding.clone(),
            });
            state.waiters.insert(message.id.clone(), sender);
        }
        match receiver.recv_timeout(*self.wait.lock().unwrap_or_else(|state| state.into_inner())) {
            Ok(Outcome::Started) => AdapterResult::Accepted(Some(STARTED.to_owned())),
            Ok(Outcome::Held(reason)) => AdapterResult::Dropped(reason),
            Err(_) => self.abandon(&message.id, &receiver),
        }
    }

    /// Whether a registered mod is polling, so mail held for a missing mod can
    /// be offered again once it connects.
    pub(crate) fn open_for_delivery(&self) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        state
            .receiver
            .as_ref()
            .is_some_and(|registered| self.receiver_is_live(registered))
    }

    /// Peeks at the retained authenticated receipts without removing them.
    /// The router classifies each peeked event against its cached snapshot
    /// and acks only an observed durable settle, so a lagging snapshot or a
    /// failed resolve never loses evidence. No generation filter: an event
    /// settles its own old row, and resolve checks the row's own binding.
    pub(crate) fn peek_late(&self) -> Vec<LateReceipt> {
        let Ok(state) = self.state.lock() else {
            return Vec::new();
        };
        state.late.values().cloned().collect()
    }

    /// Drops retained evidence after the router observed its row durably
    /// settled (or only after a newer snapshot still lacks the row).
    pub(crate) fn ack_late(&self, id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.late.remove(id);
        }
    }

    /// Shortens the receipt wait in tests so a busy mod's late receipt is
    /// fast to model without changing production timing.
    #[cfg(test)]
    pub(crate) fn set_test_timeout(&self, wait: Duration) {
        if let Ok(mut current) = self.wait.lock() {
            *current = wait;
        }
    }

    fn receiver_is_live(&self, registered: &Receiver) -> bool {
        (self.now)()
            .checked_duration_since(registered.last_poll)
            .is_none_or(|idle| idle <= RECEIVER_TIMEOUT)
    }

    /// Gives up on a receipt: mail no mod collected is removed, so it is never
    /// offered later; mail a mod already claimed stays uncertain. One
    /// ownership protocol with receipt(): receipt() sends the outcome to the
    /// waiter while holding the state lock, and abandon() re-checks the
    /// waiter's channel while holding that same lock, so an outcome that
    /// arrived first is honored instead of going uncertain.
    fn abandon(&self, id: &str, receiver: &mpsc::Receiver<Outcome>) -> AdapterResult {
        let Ok(mut state) = self.state.lock() else {
            return AdapterResult::Uncertain;
        };
        match receiver.try_recv() {
            Ok(Outcome::Started) => return AdapterResult::Accepted(Some(STARTED.to_owned())),
            Ok(Outcome::Held(reason)) => return AdapterResult::Dropped(reason),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {}
        }
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
        if let Ok(mut bootstrap) = self.bootstrap.lock()
            && let Some(path) = bootstrap.take()
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

/// The per-room bootstrap connection file used before the room's session is
/// known. Named by `CROWDED_ROOM` so fresh rooms in the same cwd never share a
/// connection file.
pub(crate) fn bootstrap_file(cwd: &Path, room: usize) -> io::Result<PathBuf> {
    Ok(cwd
        .join(".crowded")
        .join("claude-inbox")
        .join(format!("_bootstrap.{room}.json")))
}

pub(crate) fn write_private(path: &Path, contents: &str) -> io::Result<()> {
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

/// The bytes the text takes in the JSON body /next returns.
fn escaped_size(text: &str) -> usize {
    serde_json::to_string(text).map_or(usize::MAX, |escaped| escaped.len())
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
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let (status, body) = match read_request(&mut stream, Instant::now() + REQUEST_DEADLINE) {
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

/// Reads one chunk with only what is left of the request's deadline.
fn read_some(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Instant,
) -> Result<usize, (u16, String)> {
    let Some(left) = deadline.checked_duration_since(Instant::now()) else {
        return Err((408, "the request did not arrive in time".to_owned()));
    };
    stream
        .set_read_timeout(Some(left.max(Duration::from_millis(1))))
        .map_err(|error| (408, error.to_string()))?;
    stream.read(buffer).map_err(|error| match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            (408, "the request did not arrive in time".to_owned())
        }
        _ => (400, error.to_string()),
    })
}

/// Reads the head into bounded memory, then the body its Content-Length names.
/// Every read gets only what is left of the whole-request deadline, so a client
/// that trickles cannot stretch one request for ever.
fn read_request(stream: &mut TcpStream, deadline: Instant) -> Result<Request, (u16, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(end) = buffer.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end;
        }
        let count = read_some(stream, &mut chunk, deadline)?;
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
        let count = read_some(stream, &mut chunk, deadline)?;
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
    // Backpressure: unsettled claims and retained evidence are never
    // evicted, so while their combined count is at the cap no new mail is
    // handed out. The queued mail waits for its own deliver() timeout and is
    // held as uncollected, then retried later.
    if state.claimed.len() + state.late.len() >= MAX_CLAIMED {
        return (200, "null".to_owned());
    }
    match state.queue.pop_front() {
        Some(mail) => {
            state.claimed.insert(
                mail.id.clone(),
                Claimed {
                    sha256: mail.sha256.clone(),
                    binding: mail.binding.clone(),
                },
            );
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
    let Some(known) = state.claimed.get(id) else {
        return (
            409,
            error_body("the receipt does not match a claimed message"),
        );
    };
    if known.sha256 != sha256 {
        return (
            409,
            error_body("the receipt does not match a claimed message"),
        );
    }
    // The receipt's state is validated before the claim is touched: a
    // malformed state leaves the claim intact so the mod's valid retry for
    // the same id can still settle it.
    let held = match state_name {
        "started" => None,
        // A mod-supplied reason is namespaced: it must never equal an internal
        // hold string, or a claimed message would look retryable and be sent
        // twice.
        "held" => Some(match field("reason") {
            Some(reason) if !reason.is_empty() => format!("{MOD_HOLD_PREFIX}{reason}"),
            _ => "the Claude mod held the prompt".to_owned(),
        }),
        _ => {
            return (
                409,
                error_body("the receipt does not match a claimed message"),
            );
        }
    };
    // Evidence is retained only when no waiter can take the outcome: an
    // on-time receipt (started or held) is delivered through the waiter and
    // leaves no state behind. A waiter whose channel already closed loses the
    // outcome, so that receipt is kept like a late one.
    let Some(claimed) = state.claimed.remove(id) else {
        return (
            409,
            error_body("the receipt does not match a claimed message"),
        );
    };
    let late = LateReceipt {
        id: id.to_owned(),
        sha256: sha256.to_owned(),
        binding: claimed.binding,
        held,
    };
    if let Some(waiter) = state.waiters.remove(id) {
        let outcome = match &late.held {
            None => Outcome::Started,
            Some(reason) => Outcome::Held(reason.clone()),
        };
        if waiter.send(outcome).is_ok() {
            return (200, "{}".to_owned());
        }
    }
    state.late.insert(id.to_owned(), late);
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
        408 => "Request Timeout",
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

/// How long a test client waits for an answer before calling it missing. It is
/// a liveness bound, not a deadline assertion, so a loaded machine cannot break
/// a test that would pass on an idle one.
#[cfg(test)]
const GENEROUS: Duration = Duration::from_secs(30);

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
    stream.set_read_timeout(Some(GENEROUS)).unwrap();
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

    /// The same receiver, as the mod keeps it across a session-report rebind.
    pub(crate) fn resumed(&self) -> Self {
        Self {
            url: self.url.clone(),
            token: self.token.clone(),
            receiver: self.receiver.clone(),
        }
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
#[derive(Clone)]
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
    use crate::native_session::is_retryable_hold;
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    fn inbox(wait: Duration) -> Arc<ClaudeInbox> {
        let inbox = ClaudeInbox::open(Arc::new(Instant::now), wait).unwrap();
        inbox.retarget(1);
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

    /// Waits until a send has queued its message, so a transition under test
    /// always starts from a known state.
    fn wait_queued(inbox: &ClaudeInbox, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let queued = inbox
                .state
                .lock()
                .map(|state| state.queue.iter().any(|mail| mail.id == id))
                .unwrap_or(false);
            if queued {
                return;
            }
            assert!(Instant::now() < deadline, "{id} was never queued");
            thread::sleep(Duration::from_millis(5));
        }
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
            AdapterResult::Dropped(reason) if reason == "Claude mod: prompt is open"
        ));
    }

    /// A mod-supplied held reason that spells an internal hold is namespaced,
    /// so the row it settles is not retryable and the claimed message is never
    /// offered again.
    #[test]
    fn a_mod_reason_equal_to_an_internal_hold_is_namespaced_and_not_retryable() {
        let inbox = inbox(Duration::from_secs(2));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let held = delivered(Arc::clone(&inbox), message(1, "id-collide", "held"));
        let claim = mod_.claim();
        assert_eq!(mod_.receipt(&claim, "held", CLAUDE_HOLD), 200);
        let reason = match held.join().unwrap() {
            AdapterResult::Dropped(reason) => reason,
            _ => panic!("a held receipt must drop the message"),
        };
        assert_ne!(reason, CLAUDE_HOLD);
        assert_eq!(reason, format!("{MOD_HOLD_PREFIX}{CLAUDE_HOLD}"));
        assert!(!is_retryable_hold(&reason));
    }

    /// Held mail waits for a registered mod: readiness follows registration
    /// and ends once the mod stops polling.
    #[test]
    fn a_missing_or_stalled_mod_is_not_open_for_delivery() {
        let now = Arc::new(Mutex::new(Instant::now()));
        let clock = {
            let now = Arc::clone(&now);
            Arc::new(move || *now.lock().unwrap())
        };
        let inbox = Arc::new(ClaudeInbox::open(clock, Duration::from_millis(50)).unwrap());
        inbox.retarget(1);
        assert!(!inbox.open_for_delivery());
        let _mod = FakeMod::register(&inbox.url, &inbox.token);
        assert!(inbox.open_for_delivery());
        *now.lock().unwrap() += RECEIVER_TIMEOUT + Duration::from_millis(1);
        assert!(!inbox.open_for_delivery());
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

    /// A receipt that arrives after deliver() timed out is kept for late
    /// settlement with its binding; a repeat or a wrong hash settles nothing.
    #[test]
    fn a_late_receipt_is_kept_for_settlement_with_its_binding() {
        let inbox = inbox(Duration::from_millis(50));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let late = delivered(Arc::clone(&inbox), message(1, "id-late", "slow"));
        let claim = mod_.claim();
        assert_eq!(claim.id, "id-late");
        assert!(matches!(late.join().unwrap(), AdapterResult::Uncertain));
        assert_eq!(mod_.receipt(&claim, "started", ""), 200);
        let settled = inbox.peek_late();
        assert_eq!(settled.len(), 1, "one late receipt expected");
        assert_eq!(settled[0].id, "id-late");
        assert_eq!(settled[0].sha256, claim.sha256);
        assert_eq!(settled[0].binding.generation, 1);
        assert!(settled[0].held.is_none());
        inbox.ack_late("id-late");
        assert!(inbox.peek_late().is_empty(), "acked evidence is gone");
        assert_eq!(
            mod_.receipt(&claim, "started", ""),
            409,
            "a repeat receipt must not settle again"
        );
        assert!(inbox.peek_late().is_empty());
    }

    /// A late receipt with the wrong hash is refused. An on-time receipt,
    /// started or held, is delivered through its waiter and leaves no retained
    /// evidence: the waiter is the single delivery path, and only a receipt
    /// with no waiter left is kept for the router.
    #[test]
    fn on_time_held_receipt_creates_no_evidence() {
        let slow = inbox(Duration::from_millis(50));
        let mod_ = FakeMod::register(&slow.url, &slow.token);
        let late = delivered(Arc::clone(&slow), message(1, "id-bad", "slow"));
        let claim = mod_.claim();
        assert_eq!(claim.id, "id-bad");
        assert!(matches!(late.join().unwrap(), AdapterResult::Uncertain));
        let mut forged = claim.clone();
        forged.sha256 = "0".repeat(64);
        assert_eq!(mod_.receipt(&forged, "started", ""), 409);
        assert!(
            slow.peek_late().is_empty(),
            "wrong hash must settle nothing"
        );

        let quick = inbox(Duration::from_secs(2));
        let mod_ = FakeMod::register(&quick.url, &quick.token);
        let fast = delivered(Arc::clone(&quick), message(1, "id-fast", "quick"));
        let claim = mod_.claim();
        assert_eq!(mod_.receipt(&claim, "started", ""), 200);
        assert!(matches!(
            fast.join().unwrap(),
            AdapterResult::Accepted(Some(_))
        ));
        assert!(
            quick.peek_late().is_empty(),
            "an on-time started receipt must not be retained as evidence"
        );

        let held = delivered(Arc::clone(&quick), message(1, "id-held-on-time", "held"));
        let claim = mod_.claim();
        assert_eq!(mod_.receipt(&claim, "held", "prompt is open"), 200);
        assert!(matches!(
            held.join().unwrap(),
            AdapterResult::Dropped(reason) if reason == "Claude mod: prompt is open"
        ));
        assert!(
            quick.peek_late().is_empty(),
            "an on-time held receipt must not be retained as evidence"
        );
    }

    /// A late 'held' receipt keeps the mod's reason so the row can fail with it.
    #[test]
    fn a_late_held_receipt_keeps_its_reason() {
        let inbox = inbox(Duration::from_millis(50));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let late = delivered(Arc::clone(&inbox), message(1, "id-held", "slow"));
        let claim = mod_.claim();
        assert!(matches!(late.join().unwrap(), AdapterResult::Uncertain));
        assert_eq!(mod_.receipt(&claim, "held", "prompt is open"), 200);
        let settled = inbox.peek_late();
        assert_eq!(settled.len(), 1);
        assert_eq!(
            settled[0].held.as_deref(),
            Some("Claude mod: prompt is open")
        );
    }

    /// The timeout does not win when the outcome already arrived: abandon()
    /// re-checks the waiter's channel under the state lock, so an outcome the
    /// receipt sent first is honored instead of going uncertain.
    #[test]
    fn an_outcome_sent_before_abandon_is_honored_not_uncertain() {
        let raced = inbox(Duration::from_millis(50));
        let (sender, receiver) = mpsc::channel();
        sender.send(Outcome::Started).unwrap();
        raced
            .state
            .lock()
            .unwrap()
            .waiters
            .insert("id-race".to_owned(), sender);
        assert!(matches!(
            raced.abandon("id-race", &receiver),
            AdapterResult::Accepted(_)
        ));

        let (sender, receiver) = mpsc::channel();
        sender
            .send(Outcome::Held("prompt is open".to_owned()))
            .unwrap();
        raced
            .state
            .lock()
            .unwrap()
            .waiters
            .insert("id-race-held".to_owned(), sender);
        assert!(matches!(
            raced.abandon("id-race-held", &receiver),
            AdapterResult::Dropped(reason) if reason == "prompt is open"
        ));
        assert!(
            raced.peek_late().is_empty(),
            "abandon itself records no evidence"
        );

        // Receipt first, then the timeout: the waiter takes the outcome, so
        // the delivery is accepted and no evidence is retained.
        let raced = inbox(Duration::from_millis(50));
        let mod_ = FakeMod::register(&raced.url, &raced.token);
        let pending = delivered(Arc::clone(&raced), message(1, "id-first", "text"));
        let claim = mod_.claim();
        assert_eq!(mod_.receipt(&claim, "started", ""), 200);
        assert!(matches!(
            pending.join().unwrap(),
            AdapterResult::Accepted(Some(_))
        ));
        assert!(
            raced.peek_late().is_empty(),
            "an outcome a waiter took must leave no evidence"
        );
    }

    /// A malformed receipt state leaves the claim intact, so the mod's valid
    /// retry for the same id still settles the delivery.
    #[test]
    fn a_malformed_receipt_state_keeps_the_claim_for_a_retry() {
        let inbox = inbox(Duration::from_secs(2));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let pending = delivered(Arc::clone(&inbox), message(1, "id-malformed", "text"));
        let claim = mod_.claim();
        assert_eq!(
            mod_.receipt(&claim, "bogus", ""),
            409,
            "a malformed state must not settle"
        );
        assert!(inbox.peek_late().is_empty());
        assert_eq!(mod_.receipt(&claim, "started", ""), 200);
        assert!(matches!(
            pending.join().unwrap(),
            AdapterResult::Accepted(Some(_))
        ));
        assert!(
            inbox.peek_late().is_empty(),
            "an on-time receipt leaves no evidence behind"
        );
    }

    /// A receipt after a generation change matches nothing: the old claim is
    /// gone, so the stale evidence is refused instead of settling anything.
    #[test]
    fn a_receipt_after_a_generation_change_settles_nothing() {
        let inbox = inbox(Duration::from_millis(50));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let late = delivered(Arc::clone(&inbox), message(1, "id-stale", "slow"));
        let claim = mod_.claim();
        assert!(matches!(late.join().unwrap(), AdapterResult::Uncertain));
        inbox.retarget(2);
        assert_eq!(
            mod_.receipt(&claim, "started", ""),
            409,
            "a stale generation must not settle"
        );
        assert!(inbox.peek_late().is_empty());
    }

    /// Backpressure instead of eviction: with 64 unsettled claims /next hands
    /// out nothing, and settling one claim frees exactly one slot.
    /// Backpressure instead of eviction: with 64 unsettled claims /next hands
    /// out nothing and keeps the mail queued; settling one claim frees
    /// exactly one slot. Driven directly against the inbox state so no thread
    /// timing is involved.
    #[test]
    fn a_full_claim_table_hands_out_nothing_until_a_claim_settles() {
        let now: Clock = Arc::new(Instant::now);
        let mut state = State {
            generation: 1,
            receiver: Some(Receiver {
                id: "r".to_owned(),
                last_poll: now(),
            }),
            ..Default::default()
        };
        let slow_hash = format!("{:x}", Sha256::digest(b"slow"));
        let queue_mail = |state: &mut State, id: &str| {
            state.queue.push_back(Mail {
                id: id.to_owned(),
                text: "slow".to_owned(),
                sha256: slow_hash.clone(),
                binding: Binding {
                    room: "1".to_owned(),
                    session: "session".to_owned(),
                    endpoint: "claude".to_owned(),
                    generation: 1,
                },
            });
        };
        let poll = |state: &mut State| {
            let body = serde_json::json!({"receiver": "r"});
            next(state, &now, &body)
        };
        for n in 0..64 {
            queue_mail(&mut state, &format!("id-cap-{n:02}"));
        }
        for n in 0..64 {
            let (status, raw) = poll(&mut state);
            assert_eq!(status, 200);
            let mail: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(mail["id"], format!("id-cap-{n:02}"));
        }
        assert_eq!(state.claimed.len(), 64);
        queue_mail(&mut state, "id-cap-64");
        let (status, raw) = poll(&mut state);
        assert_eq!(status, 200);
        assert_eq!(raw, "null", "a full claim table must hand out nothing");
        assert_eq!(state.queue.len(), 1, "the refused mail stays queued");
        let receipt_body = serde_json::json!({
            "receiver": "r",
            "id": "id-cap-00",
            "sha256": slow_hash,
            "state": "started",
        });
        assert_eq!(receipt(&mut state, &receipt_body).0, 200);
        assert_eq!(state.claimed.len(), 63);
        assert_eq!(state.late.len(), 1);
        // The cap counts claims and retained evidence together, so turning a
        // claim into evidence does not free a slot: only the router's ack
        // does, once the row is durably settled.
        let (status, raw) = poll(&mut state);
        assert_eq!(status, 200);
        assert_eq!(raw, "null", "evidence still counts against the cap");
        state.late.remove("id-cap-00");
        let (status, raw) = poll(&mut state);
        assert_eq!(status, 200);
        let mail: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(mail["id"], "id-cap-64");
    }

    /// Liveness is judged by missed polls on the inbox's own clock.
    #[test]
    fn a_silent_receiver_is_gone_after_missed_polls() {
        let ms = Arc::new(AtomicU64::new(0));
        let base = Instant::now();
        let hand = Arc::clone(&ms);
        let clock: Clock =
            Arc::new(move || base + Duration::from_millis(hand.load(Ordering::SeqCst)));
        let inbox = Arc::new(ClaudeInbox::open(clock, Duration::from_millis(200)).unwrap());
        inbox.retarget(1);
        let _mod = FakeMod::register(&inbox.url, &inbox.token);
        ms.store(
            (RECEIVER_TIMEOUT.as_millis() as u64) + 1000,
            Ordering::SeqCst,
        );
        assert!(matches!(
            inbox.deliver(&message(1, "id-6", "late")),
            AdapterResult::Dropped(reason) if reason == DISCONNECTED_HOLD
        ));
        assert_eq!(
            FakeMod::open(&inbox.url, &inbox.token).register_again(),
            200,
            "a silent receiver is replaced"
        );
    }

    /// A session-report rebind keeps the room's receiver, because the mod's
    /// process did not restart and it cannot register again; only the mail of
    /// the generation it now serves is offered.
    #[test]
    fn a_report_rebind_keeps_the_receiver_and_offers_only_new_mail() {
        let inbox = inbox(Duration::from_millis(600));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let queued = delivered(Arc::clone(&inbox), message(1, "id-7", "old"));
        wait_queued(&inbox, "id-7");
        inbox.retarget(2);
        assert!(matches!(
            queued.join().unwrap(),
            AdapterResult::Dropped(reason) if reason == STALE_HOLD
        ));
        assert!(
            mod_.next_claim().is_none(),
            "the receiver survives without the old mail"
        );

        let accepted = delivered(Arc::clone(&inbox), message(2, "id-8", "new"));
        let claim = mod_.claim();
        assert_eq!(claim.id, "id-8");
        assert_eq!(mod_.receipt(&claim, "started", ""), 200);
        assert!(matches!(
            accepted.join().unwrap(),
            AdapterResult::Accepted(Some(_))
        ));
    }

    /// A message carrying the generation a rebind replaced is held before it
    /// can be queued, even while the receiver is still registered.
    #[test]
    fn a_message_from_the_replaced_generation_is_held() {
        let inbox = inbox(Duration::from_millis(200));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        inbox.retarget(2);
        assert!(matches!(
            inbox.deliver(&message(1, "id-9", "stale")),
            AdapterResult::Dropped(reason) if reason == STALE_HOLD
        ));
        assert!(
            mod_.next_claim().is_none(),
            "the replaced generation's mail was offered"
        );
    }

    /// Mail whose escaped text would not fit the mod's response limit is
    /// refused before it is queued.
    #[test]
    fn mail_too_large_for_the_mod_is_refused_before_it_is_queued() {
        let inbox = inbox(Duration::from_millis(200));
        let mod_ = FakeMod::register(&inbox.url, &inbox.token);
        let huge = "\"".repeat(40_000);
        assert!(matches!(
            inbox.deliver(&message(1, "id-10", &huge)),
            AdapterResult::Dropped(reason) if reason == TOO_LARGE_HOLD
        ));
        assert!(
            mod_.next_claim().is_none(),
            "an oversized message was offered"
        );

        let accepted = delivered(Arc::clone(&inbox), message(1, "id-11", "a normal message"));
        let claim = mod_.claim();
        assert_eq!(claim.text, "a normal message");
        assert_eq!(mod_.receipt(&claim, "started", ""), 200);
        assert!(matches!(
            accepted.join().unwrap(),
            AdapterResult::Accepted(Some(_))
        ));
    }

    /// A client that trickles its headers cannot hold the listener: its own
    /// connection is cut off by the request deadline, and a legitimate request
    /// on the same inbox is answered. The proofs are the trickle's 408 and the
    /// answer that follows it; the only clock involved is a liveness bound far
    /// beyond the deadline, so a loaded machine cannot make this flaky.
    #[test]
    fn a_trickling_client_cannot_hold_the_listener() {
        let inbox = inbox(Duration::from_millis(200));
        let address = inbox.url.trim_start_matches("http://").to_owned();
        let mut trickle = TcpStream::connect(&address).unwrap();
        trickle
            .set_read_timeout(Some(GENEROUS))
            .expect("a readable trickle");
        trickle.write_all(b"POST /next HTTP/1.1\r\n").unwrap();
        // One byte at a time, faster than a per-read timeout would fire.
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let mut piped = trickle.try_clone().unwrap();
        let writer = thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                if piped.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        // A legitimate request while the trickle is still open.
        let (url, token) = (inbox.url.clone(), inbox.token.clone());
        let legitimate = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            request(&url, Some(&token), "/register", "{}", None).0
        });

        let mut response = String::new();
        let _ = trickle.read_to_string(&mut response);
        assert!(
            response.starts_with("HTTP/1.1 408"),
            "the trickle was not cut off by the deadline: {response:?}"
        );
        stop.store(true, Ordering::SeqCst);
        writer.join().unwrap();
        assert_eq!(
            legitimate.join().unwrap(),
            200,
            "a legitimate request must still be answered"
        );
    }

    /// A second session report replaces the previous session's connection file;
    /// the old one must not remain readable in the cwd.
    #[test]
    fn adopt_reported_session_removes_the_previous_session_file() {
        let inbox = inbox(Duration::from_millis(200));
        let cwd = tempfile::tempdir().unwrap();
        let first = "11111111-1111-4111-8111-111111111111";
        let second = "22222222-2222-4222-8222-222222222222";
        inbox.prepare(cwd.path(), 1, 1).unwrap();
        let bootstrap = bootstrap_file(cwd.path(), 1).unwrap();
        assert!(
            bootstrap.exists(),
            "bootstrap must exist before first report"
        );

        inbox
            .adopt_reported_session(cwd.path(), 1, first, 2)
            .unwrap();
        let first_file = connection_file(cwd.path(), first).unwrap();
        assert!(first_file.exists(), "first session file must exist");
        assert!(bootstrap.exists(), "bootstrap must remain after a report");

        inbox
            .adopt_reported_session(cwd.path(), 1, second, 3)
            .unwrap();
        let second_file = connection_file(cwd.path(), second).unwrap();
        assert!(second_file.exists(), "second session file must exist");
        assert!(
            !first_file.exists(),
            "previous session file must be removed on second report"
        );
    }

    /// A resumed launch leaves both its session file and its per-room fallback;
    /// a bootstrap that cannot be written leaves neither and fails the launch.
    #[test]
    fn attach_writes_a_bootstrap_and_cleans_up_on_failure() {
        let inbox = inbox(Duration::from_millis(200));
        let cwd = tempfile::tempdir().unwrap();
        let session = "44444444-4444-4444-8444-444444444444";
        let session_file = connection_file(cwd.path(), session).unwrap();
        let boot = bootstrap_file(cwd.path(), 1).unwrap();

        inbox.attach(cwd.path(), 1, session, 1).unwrap();
        assert!(session_file.exists(), "session file must exist");
        assert!(boot.exists(), "bootstrap must exist for a resumed launch");

        fs::remove_file(&session_file).unwrap();
        fs::remove_file(&boot).unwrap();
        fs::create_dir_all(&boot).unwrap();
        assert!(
            inbox.attach(cwd.path(), 1, session, 2).is_err(),
            "a bootstrap that cannot be written must fail the launch"
        );
        assert!(
            !session_file.exists(),
            "session file must be removed on failure"
        );
    }

    /// Dropping the inbox removes its session and bootstrap connection files.
    #[test]
    fn drop_removes_session_and_bootstrap_files() {
        let cwd = tempfile::tempdir().unwrap();
        let session = "33333333-3333-4333-8333-333333333333";
        let session_file = connection_file(cwd.path(), session).unwrap();
        let boot = bootstrap_file(cwd.path(), 1).unwrap();
        {
            let inbox = inbox(Duration::from_millis(200));
            inbox.prepare(cwd.path(), 1, 1).unwrap();
            assert!(boot.exists());
        }
        assert!(!boot.exists(), "bootstrap must be removed on drop");
        {
            let inbox = inbox(Duration::from_millis(200));
            inbox.attach(cwd.path(), 1, session, 1).unwrap();
            assert!(session_file.exists());
        }
        assert!(
            !session_file.exists(),
            "session file must be removed on drop"
        );
    }
}
