//! Isolated OpenCode server and bounded local HTTP client.
#![allow(dead_code)]

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    ffi::{OsStr, OsString},
    io::{self, BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream},
    path::Path,
    process::{Child, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::command::{ProcessTree, ResolvedCommand};
use crate::native_binding::{LaunchBinding, OwnershipEvidence};
use crate::native_outbox::Binding;

const MAX_RESPONSE: usize = 128 * 1024;
const MAX_REQUEST: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
/// Startup is slower behind a Headroom wrapper (its proxy starts first), so
/// the first announcement of the server's address gets a wider bound than a
/// single request.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Delivery {
    Accepted,
    Conflict,
    Rejected(String),
    NotSent,
    Uncertain,
}

enum RequestError {
    NotSent,
    Uncertain,
}

struct Response {
    status: u16,
    body: Vec<u8>,
}

/// Holds a credential; intentionally has no Debug implementation.
pub(crate) struct Client {
    address: SocketAddrV4,
    authorization: String,
    timeout: Duration,
    /// Ask OpenCode to start a turn for an admitted prompt. Off by default so
    /// tests and probes never trigger a model call.
    resume: bool,
}

fn endpoint(url: &str) -> io::Result<SocketAddrV4> {
    let address: SocketAddrV4 = url
        .strip_prefix("http://")
        .ok_or_else(|| io::Error::other("invalid server URL"))?
        .parse()
        .map_err(|_| io::Error::other("invalid server URL"))?;
    if *address.ip() != Ipv4Addr::LOCALHOST || address.port() == 0 {
        return Err(io::Error::other("server must use loopback"));
    }
    Ok(address)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Client {
    /// The password this client authenticates with, recovered from the stored
    /// header so a single copy of the secret exists.
    fn password(&self) -> Option<String> {
        let encoded = self.authorization.strip_prefix("Basic ")?;
        let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
        decoded.strip_prefix("opencode:").map(str::to_owned)
    }

    fn new(url: &str, password: &str) -> io::Result<Self> {
        Ok(Self {
            address: endpoint(url)?,
            authorization: format!("Basic {}", STANDARD.encode(format!("opencode:{password}"))),
            timeout: TIMEOUT,
            resume: false,
        })
    }

    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Response, RequestError> {
        let payload = body
            .map(|v| serde_json::to_vec(v).unwrap())
            .unwrap_or_default();
        if payload.len() > MAX_REQUEST {
            return Err(RequestError::NotSent);
        }
        let mut stream = TcpStream::connect_timeout(&SocketAddr::V4(self.address), self.timeout)
            .map_err(|_| RequestError::NotSent)?;
        let deadline = Instant::now() + self.timeout;
        let headers = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            self.address,
            self.authorization,
            payload.len()
        );
        write_until(&mut stream, headers.as_bytes(), deadline)
            .map_err(|_| RequestError::Uncertain)?;
        write_until(&mut stream, &payload, deadline).map_err(|_| RequestError::Uncertain)?;
        let mut bytes = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or(RequestError::Uncertain)?;
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|_| RequestError::Uncertain)?;
            let n = stream.read(&mut buf).map_err(|_| RequestError::Uncertain)?;
            if n == 0 {
                break;
            }
            if bytes.len() + n > MAX_RESPONSE {
                return Err(RequestError::Uncertain);
            }
            bytes.extend_from_slice(&buf[..n]);
            if framed(&bytes) {
                if let Some(response) = parse_response(&bytes) {
                    return Ok(response);
                }
            }
        }
        parse_response(&bytes).ok_or(RequestError::Uncertain)
    }

    fn verify_pid(&self, child_pid: u32, job: Option<&ProcessTree>) -> io::Result<u32> {
        let response = self
            .request("GET", "/api/info", None)
            .map_err(|_| io::Error::other("server info unavailable"))?;
        let data: Value = serde_json::from_slice(&response.body)
            .map_err(|_| io::Error::other("invalid server info"))?;
        let reported = data["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .ok_or_else(|| io::Error::other("invalid server identity"))?;
        if response.status != 200 || !owned_by(child_pid, reported, job) {
            return Err(io::Error::other("server identity mismatch"));
        }
        Ok(reported)
    }

    fn create_session(&self) -> io::Result<String> {
        let response = self
            .request("POST", "/api/session", Some(&json!({})))
            .map_err(|_| io::Error::other("session creation uncertain"))?;
        let data: Value = serde_json::from_slice(&response.body)
            .map_err(|_| io::Error::other("invalid session response"))?;
        let id = data["data"]["id"]
            .as_str()
            .filter(|id| valid_id(id))
            .ok_or_else(|| io::Error::other("invalid session identity"))?;
        if response.status != 200 {
            return Err(io::Error::other("session creation rejected"));
        }
        Ok(id.into())
    }

    fn queue(&self, session: &str, message_id: &str, text: &str) -> Delivery {
        if !valid_id(session) || !valid_id(message_id) {
            return Delivery::NotSent;
        }
        let path = format!("/api/session/{session}/prompt");
        let body =
            json!({"id": message_id, "text": text, "delivery": "queue", "resume": self.resume});
        match self.request("POST", &path, Some(&body)) {
            Err(RequestError::NotSent) => Delivery::NotSent,
            Err(RequestError::Uncertain) => Delivery::Uncertain,
            Ok(response) if (400..500).contains(&response.status) => Delivery::Rejected(format!(
                "OpenCode returned HTTP {}; check credentials and session",
                response.status
            )),
            Ok(response) if !(200..300).contains(&response.status) => Delivery::Uncertain,
            Ok(response) => {
                let Ok(data) = serde_json::from_slice::<Value>(&response.body) else {
                    return Delivery::Uncertain;
                };
                let item = &data["data"];
                if item["delivery"].as_str() != Some("queue") {
                    return Delivery::Uncertain;
                }
                let Some(returned) = item["payload"]["text"].as_str() else {
                    return Delivery::Uncertain;
                };
                if item["id"].as_str() != Some(message_id)
                    || item["sessionID"].as_str() != Some(session)
                    || !returned.starts_with(text)
                {
                    Delivery::Conflict
                } else {
                    Delivery::Accepted
                }
            }
        }
    }

    fn queue_async(
        self: Arc<Self>,
        session: String,
        message_id: String,
        text: String,
    ) -> mpsc::Receiver<Delivery> {
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(self.queue(&session, &message_id, &text));
        });
        rx
    }

    fn reconcile(&self, session: &str, message_id: &str, text: &str) -> Delivery {
        if !valid_id(session) || !valid_id(message_id) {
            return Delivery::NotSent;
        }
        let response = match self.request("GET", &format!("/api/session/{session}/inbox"), None) {
            Ok(response) => response,
            Err(_) => return Delivery::Uncertain,
        };
        if (400..500).contains(&response.status) {
            return Delivery::Rejected(format!(
                "OpenCode returned HTTP {}; check credentials and session",
                response.status
            ));
        }
        if !(200..300).contains(&response.status) {
            return Delivery::Uncertain;
        }
        let Ok(data) = serde_json::from_slice::<Value>(&response.body) else {
            return Delivery::Uncertain;
        };
        let Some(items) = data["data"].as_array() else {
            return Delivery::Uncertain;
        };
        let Some(item) = items
            .iter()
            .find(|item| item["id"].as_str() == Some(message_id))
        else {
            return Delivery::Uncertain;
        };
        if item["delivery"].as_str() != Some("queue") {
            return Delivery::Uncertain;
        }
        let Some(returned) = item["payload"]["text"].as_str() else {
            return Delivery::Uncertain;
        };
        if item["sessionID"].as_str() != Some(session) || !returned.starts_with(text) {
            Delivery::Conflict
        } else {
            Delivery::Accepted
        }
    }

    fn reconcile_async(
        self: Arc<Self>,
        session: String,
        message_id: String,
        text: String,
    ) -> mpsc::Receiver<Delivery> {
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(self.reconcile(&session, &message_id, &text));
        });
        rx
    }
}

fn write_until(stream: &mut TcpStream, mut data: &[u8], deadline: Instant) -> io::Result<()> {
    while !data.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "request timed out"))?;
        stream.set_write_timeout(Some(remaining))?;
        let written = stream.write(data)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "request not written",
            ));
        }
        data = &data[written..];
    }
    Ok(())
}

fn framed(raw: &[u8]) -> bool {
    let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let Ok(header) = std::str::from_utf8(&raw[..end]) else {
        return false;
    };
    header.split("\r\n").skip(1).any(|line| {
        line.split_once(':').is_some_and(|(name, _)| {
            name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
        })
    })
}

fn parse_response(raw: &[u8]) -> Option<Response> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header = std::str::from_utf8(&raw[..split]).ok()?;
    let mut lines = header.split("\r\n");
    let mut status = lines.next()?.split_whitespace();
    if !matches!(status.next()?, "HTTP/1.0" | "HTTP/1.1") {
        return None;
    }
    let code = status.next()?.parse().ok()?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return None;
            }
            length = Some(value.trim().parse::<usize>().ok()?);
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if !value.trim().eq_ignore_ascii_case("chunked") {
                return None;
            }
            chunked = true;
        }
    }
    if chunked && length.is_some() {
        return None;
    }
    let bytes = &raw[split + 4..];
    let body = if chunked {
        decode_chunks(bytes)?
    } else {
        if length.is_some_and(|n| n != bytes.len()) {
            return None;
        }
        bytes.to_vec()
    };
    Some(Response { status: code, body })
}

fn decode_chunks(mut raw: &[u8]) -> Option<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let end = raw.windows(2).position(|w| w == b"\r\n")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&raw[..end]).ok()?.split(';').next()?,
            16,
        )
        .ok()?;
        raw = raw.get(end + 2..)?;
        if size == 0 {
            return if raw == b"\r\n" { Some(body) } else { None };
        }
        if body.len().checked_add(size)? > MAX_RESPONSE {
            return None;
        }
        body.extend_from_slice(raw.get(..size)?);
        if raw.get(size..size + 2)? != b"\r\n" {
            return None;
        }
        raw = raw.get(size + 2..)?;
    }
}

/// What the TUI needs to attach to this server. It carries the password, so it
/// has no Debug implementation and must go straight into the TUI's environment.
pub(crate) struct Handoff {
    pub url: String,
    pub session: String,
    pub password: String,
}

/// How long a spawned wrapper gets to forward SIGTERM to its child before it
/// is killed outright.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(2);

/// The server is ours on Unix when it is the process Crowded spawned, or a
/// direct child of it: `headroom wrap` starts OpenCode as its own child.
/// On Windows the spawned pid is the launcher, so ownership is membership
/// of the launch's job instead; `job` is always `Some` there.
fn owned_by(spawned: u32, reported: u32, job: Option<&ProcessTree>) -> bool {
    #[cfg(unix)]
    {
        let _ = job;
        reported == spawned || process_parent(reported) == Some(spawned)
    }
    #[cfg(not(unix))]
    {
        let _ = spawned;
        job.is_some_and(|job| job.contains(reported))
    }
}

#[cfg(unix)]
fn process_parent(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

#[cfg(not(unix))]
fn process_parent(_pid: u32) -> Option<u32> {
    None
}

/// Stops the spawned process and, on Unix, gives it a chance to stop its own
/// children first: a wrapper only forwards the signal while it is alive.
fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + TERMINATE_TIMEOUT;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Kills a server the wrapper did not reap. It is only ever the pid the
/// authenticated server reported and Crowded proved it owns.
fn stop_process(pid: u32) {
    #[cfg(unix)]
    {
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if alive {
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Own and drop the server on a background thread because reaping may block.
pub(crate) struct Server {
    child: Option<Child>,
    /// The Windows job every server descendant joins at birth. It outlives
    /// all of them: dropping it first would orphan the processes it owns.
    #[cfg(windows)]
    tree: Option<ProcessTree>,
    client: Arc<Client>,
    session: Mutex<String>,
    url: String,
    binding: Mutex<Option<LaunchBinding>>,
    /// The authenticated server's own pid; when it differs from the spawned
    /// child (a wrapper sits in between), it is stopped explicitly on drop.
    server_pid: Option<u32>,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let spawned = child.id();
            #[cfg(windows)]
            {
                if let Some(tree) = self.tree.take() {
                    stop_owned(&mut child, Some(tree), self.server_pid);
                }
            }
            #[cfg(not(windows))]
            {
                terminate(&mut child);
                if let Some(server) = self.server_pid.filter(|server| *server != spawned) {
                    stop_process(server);
                }
            }
        }
    }
}

/// Stops an owned server's processes at once through its job. A failed
/// terminate is reported; the closed job handle stays the backstop.
#[cfg(windows)]
fn stop_owned(child: &mut Child, tree: Option<ProcessTree>, server_pid: Option<u32>) {
    let spawned = child.id();
    // TEMP-MUTATION(T2b-repair): no terminate; the drop test must time out.
    // Reverted before the final push.
    let _ = tree;
    let _ = child.wait();
    if let Some(server) = server_pid.filter(|server| *server != spawned) {
        stop_process(server);
    }
}

impl Server {
    /// Makes every admitted prompt start an agent turn when the session is
    /// idle; a prompt that arrives mid-turn still waits its turn. Only possible
    /// before the server is shared.
    pub(crate) fn resume_turns(&mut self) -> io::Result<()> {
        let client = Arc::get_mut(&mut self.client)
            .ok_or_else(|| io::Error::other("server is already shared"))?;
        client.resume = true;
        Ok(())
    }

    /// The session this server's room currently uses. A clear moves it, so it
    /// is read under its own lock, separate from the binding's.
    fn launch_session(&self) -> io::Result<String> {
        Ok(self
            .session
            .lock()
            .map_err(|_| io::Error::other("server session state is poisoned"))?
            .clone())
    }

    pub(crate) fn handoff(&self) -> io::Result<Handoff> {
        Ok(Handoff {
            url: self.url.clone(),
            session: self.launch_session()?,
            password: self
                .client
                .password()
                .ok_or_else(|| io::Error::other("server credential unavailable"))?,
        })
    }

    pub(crate) fn claim(&mut self, room: String, generation: u64) -> io::Result<Binding> {
        if self
            .binding
            .lock()
            .map_err(|_| io::Error::other("server binding state is poisoned"))?
            .is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "server already bound",
            ));
        }
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("server missing"))?;
        if child.try_wait()?.is_some() {
            return Err(io::Error::other("server has exited"));
        }
        #[cfg(windows)]
        let reported_pid = self.client.verify_pid(child.id(), self.tree.as_ref())?;
        #[cfg(not(windows))]
        let reported_pid = self.client.verify_pid(child.id(), None)?;
        self.server_pid = Some(reported_pid);
        // The binding identifies the verified server process, which is a
        // wrapper's child when Headroom launches OpenCode for us.
        let mut binding = LaunchBinding::new(room, generation, reported_pid)?;
        let result = binding
            .bind(OwnershipEvidence {
                child_pid: reported_pid,
                reported_pid,
                endpoint: self.client.address,
                session: self.launch_session()?,
                generation,
            })?
            .clone();
        *self
            .binding
            .lock()
            .map_err(|_| io::Error::other("server binding state is poisoned"))? = Some(binding);
        Ok(result)
    }

    /// Starts a new session on this owned server and moves the stored binding
    /// to it. The session is created before the room and generation are
    /// checked, so a rejected adoption leaves one unused session on the server
    /// and the old binding intact. `adopt_report` accepts equal or older
    /// nonzero generations, so the caller must pass a new one.
    pub(crate) fn new_session(&self, room: String, generation: u64) -> io::Result<Binding> {
        let session = self.client.create_session()?;
        let mut owned = self
            .binding
            .lock()
            .map_err(|_| io::Error::other("server binding state is poisoned"))?;
        let launch = owned
            .as_mut()
            .ok_or_else(|| io::Error::other("server is not bound"))?;
        let binding = launch.adopt_report(room, generation, session.clone())?;
        *self
            .session
            .lock()
            .map_err(|_| io::Error::other("server session state is poisoned"))? = session;
        Ok(binding)
    }

    pub fn claim_async(
        mut self,
        room: String,
        generation: u64,
    ) -> mpsc::Receiver<io::Result<(Self, Binding)>> {
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = self.claim(room, generation).map(|binding| (self, binding));
            let _ = tx.send(result);
        });
        rx
    }

    /// Moves this owned server to a session the room reported, without
    /// relaunching it or copying credentials: the launch binding already
    /// proved ownership of the server process, and the report only changes
    /// which session on that same server the room uses. The room and endpoint
    /// must still match the bound ones, so a report can never aim this server
    /// at another room or server.
    pub(crate) fn adopt(&self, binding: &Binding) -> io::Result<()> {
        let mut owned = self
            .binding
            .lock()
            .map_err(|_| io::Error::other("server binding state is poisoned"))?;
        let Some(launch) = owned.as_mut() else {
            return Err(io::Error::other("server is not bound"));
        };
        if launch
            .bound()
            .is_some_and(|current| current.endpoint != binding.endpoint)
        {
            return Err(io::Error::other(
                "a session report cannot move a server to another endpoint",
            ));
        }
        launch.adopt_report(
            binding.room.clone(),
            binding.generation,
            binding.session.clone(),
        )?;
        *self
            .session
            .lock()
            .map_err(|_| io::Error::other("server session state is poisoned"))? =
            binding.session.clone();
        Ok(())
    }

    pub fn queue_async(
        &self,
        binding: &Binding,
        message_id: String,
        text: String,
    ) -> mpsc::Receiver<Delivery> {
        let owned = self
            .binding
            .lock()
            .map(|guard| guard.as_ref().is_some_and(|owned| owned.accepts(binding)))
            .unwrap_or(false);
        if owned {
            return self
                .client
                .clone()
                .queue_async(binding.session.clone(), message_id, text);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let _ = tx.send(Delivery::NotSent);
        rx
    }

    pub fn reconcile_async(
        &self,
        binding: &Binding,
        message_id: String,
        text: String,
    ) -> mpsc::Receiver<Delivery> {
        let owned = self
            .binding
            .lock()
            .map(|guard| guard.as_ref().is_some_and(|owned| owned.accepts(binding)))
            .unwrap_or(false);
        if owned {
            return self
                .client
                .clone()
                .reconcile_async(binding.session.clone(), message_id, text);
        }
        let (tx, rx) = mpsc::sync_channel(1);
        let _ = tx.send(Delivery::NotSent);
        rx
    }

    pub fn start_async(
        program: OsString,
        cwd: std::path::PathBuf,
    ) -> mpsc::Receiver<io::Result<Self>> {
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = tx.send(Self::start(&program, &cwd));
        });
        rx
    }

    pub(crate) fn start(program: &OsStr, cwd: &Path) -> io::Result<Self> {
        Self::start_with_environment(program, &[], cwd, &[])
    }

    /// `variables` is the room's effective OpenCode environment (configuration,
    /// model override, plugins, hook tokens). It is applied before the password,
    /// so it can never replace it.
    pub(crate) fn start_with_environment(
        program: &OsStr,
        prefix: &[OsString],
        cwd: &Path,
        variables: &[(OsString, OsString)],
    ) -> io::Result<Self> {
        let mut random = [0u8; 32];
        getrandom::fill(&mut random).map_err(io::Error::other)?;
        let password = STANDARD.encode(random);
        let mut args: Vec<OsString> = prefix.to_vec();
        args.extend(
            ["serve", "--hostname", "127.0.0.1", "--port", "0"]
                .into_iter()
                .map(Into::into),
        );
        let resolved = ResolvedCommand::resolve(program, &args, cwd)?;
        // Inherit OpenCode login settings; its descendants can also inherit this password.
        // On Windows the server starts inside a job through the launcher, so
        // the wrapper and its OpenCode child are members from birth.
        #[cfg(windows)]
        let (mut command, tree) = resolved.in_job()?;
        #[cfg(not(windows))]
        let mut command = resolved.standard();
        command.current_dir(cwd);
        command.env("PWD", cwd);
        for (key, value) in variables {
            command.env(key, value);
        }
        let mut child = command
            .env("OPENCODE_PASSWORD", &password)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("server output unavailable"));
            }
        };
        let (tx, rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let _ = tx.send(startup_url(&mut reader));
            let _ = io::copy(&mut reader, &mut io::sink());
        });
        let mut verified = None;
        let result = (|| {
            let url = rx
                .recv_timeout(STARTUP_TIMEOUT)
                .map_err(|_| io::Error::other("server startup timed out"))??;
            let client = Arc::new(Client::new(&url, &password)?);
            #[cfg(windows)]
            let server_pid = client.verify_pid(child.id(), Some(&tree))?;
            #[cfg(not(windows))]
            let server_pid = client.verify_pid(child.id(), None)?;
            verified = Some(server_pid);
            let session = client.create_session()?;
            Ok((client, session, url, server_pid))
        })();
        match result {
            Ok((client, session, url, server_pid)) => Ok(Self {
                child: Some(child),
                #[cfg(windows)]
                tree: Some(tree),
                client,
                session: Mutex::new(session),
                url,
                binding: Mutex::new(None),
                server_pid: Some(server_pid),
            }),
            Err(error) => {
                #[cfg(windows)]
                {
                    stop_owned(&mut child, Some(tree), verified);
                    Err(error)
                }
                #[cfg(not(windows))]
                {
                    let spawned = child.id();
                    terminate(&mut child);
                    if let Some(server) = verified.filter(|server| *server != spawned) {
                        stop_process(server);
                    }
                    Err(error)
                }
            }
        }
    }
}

fn startup_url(reader: &mut impl BufRead) -> io::Result<String> {
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let count = reader.by_ref().take(1025).read_until(b'\n', &mut line)?;
        total += count;
        if count == 0 || count > 1024 || total > 8192 {
            return Err(io::Error::other("invalid server startup output"));
        }
        let line = std::str::from_utf8(&line)
            .map_err(|_| io::Error::other("invalid server startup output"))?;
        if let Some(url) = line.trim().strip_prefix("server listening on ") {
            endpoint(url)?;
            return Ok(url.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{
        net::TcpListener,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn fake<F>(count: usize, handler: F) -> Arc<Client>
    where
        F: Fn(usize, &str, &[u8]) -> Option<Vec<u8>> + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            for i in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut input = Vec::new();
                let mut buf = [0u8; 4096];
                let (header_end, length) = loop {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    input.extend_from_slice(&buf[..n]);
                    if let Some(pos) = input.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&input[..pos]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Length: "))
                            .unwrap()
                            .parse::<usize>()
                            .unwrap();
                        break (pos + 4, length);
                    }
                };
                while input.len() < header_end + length {
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    input.extend_from_slice(&buf[..n]);
                }
                let headers = std::str::from_utf8(&input[..header_end]).unwrap();
                if let Some(response) = handler(i, headers, &input[header_end..header_end + length])
                {
                    let _ = stream.write_all(&response);
                }
            }
        });
        Arc::new(Client::new(&format!("http://{address}"), "fake-password").unwrap())
    }

    fn response(status: u16, value: &Value) -> Vec<u8> {
        let body = serde_json::to_vec(value).unwrap();
        let mut result = format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        result.extend_from_slice(&body);
        result
    }

    fn item(id: &str, text: &str) -> Value {
        json!({"id": id, "sessionID": "ses_a", "payload": {"text": text}, "delivery": "queue"})
    }

    #[test]
    fn queue_busy_duplicate_and_changed_body() {
        let stored = Arc::new(Mutex::new(None::<Value>));
        let state = stored.clone();
        let busy = Arc::new(AtomicBool::new(true));
        let server_busy = busy.clone();
        let client = fake(3, move |_, headers, body| {
            assert!(server_busy.load(Ordering::SeqCst));
            assert!(headers.starts_with("POST /api/session/ses_a/prompt "));
            assert!(headers.contains("Authorization: Basic "));
            let request: Value = serde_json::from_slice(body).unwrap();
            assert_eq!(request["delivery"], "queue");
            assert_eq!(request["resume"], false);
            let mut existing = state.lock().unwrap();
            let entry = existing.get_or_insert_with(|| {
                item(
                    request["id"].as_str().unwrap(),
                    request["text"].as_str().unwrap(),
                )
            });
            Some(response(200, &json!({"data": entry})))
        });
        let first = client
            .clone()
            .queue_async("ses_a".into(), "msg_a".into(), "hello\n✓".into());
        assert_eq!(
            first.recv_timeout(Duration::from_secs(3)).unwrap(),
            Delivery::Accepted
        );
        assert_eq!(
            client.queue("ses_a", "msg_a", "hello\n✓"),
            Delivery::Accepted
        );
        assert_eq!(
            client.queue("ses_a", "msg_a", "changed"),
            Delivery::Conflict
        );
        assert!(busy.load(Ordering::SeqCst));
    }

    #[test]
    fn stored_appendix_accepted_only_as_suffix() {
        let client = fake(1, |_, _, _| {
            Some(response(
                200,
                &json!({"data": item("msg_a", "hello\n\nextra")}),
            ))
        });
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Accepted);
        let client = fake(1, |_, _, _| {
            Some(response(
                200,
                &json!({"data": [item("msg_a", "hello\n\nextra")]}),
            ))
        });
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Accepted
        );
        let client = fake(1, |_, _, _| {
            Some(response(
                200,
                &json!({"data": item("msg_a", "hxllo\n\nextra")}),
            ))
        });
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Conflict);
        let client = fake(1, |_, _, _| {
            Some(response(
                200,
                &json!({"data": [item("msg_a", "hxllo\n\nextra")]}),
            ))
        });
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Conflict
        );
        let client = fake(1, |_, _, _| {
            Some(response(200, &json!({"data": item("msg_a", "hell")})))
        });
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Conflict);
        let client = fake(1, |_, _, _| {
            Some(response(200, &json!({"data": [item("msg_a", "hell")]})))
        });
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Conflict
        );
        let client = fake(1, |_, _, _| {
            Some(response(
                200,
                &json!({"data": item("msg_a", "extra hello")}),
            ))
        });
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Conflict);
    }

    #[test]
    fn lost_response_then_reconcile_and_mid_request_crash() {
        let client = fake(3, |i, headers, _| match i {
            0 | 2 => {
                assert!(headers.starts_with("POST /api/session/ses_a/prompt "));
                None
            }
            _ => {
                assert!(headers.starts_with("GET /api/session/ses_a/inbox "));
                Some(response(200, &json!({"data":[item("msg_a", "hello")]})))
            }
        });
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Accepted
        );
        assert_eq!(client.queue("ses_a", "msg_b", "hello"), Delivery::Uncertain);
    }

    #[test]
    fn authorization_bad_pid_and_oversized_response() {
        let client = fake(3, |i, _, _| match i {
            0 => Some(response(401, &json!({}))),
            1 => Some(response(200, &json!({"pid": 6}))),
            _ => Some(vec![b'x'; MAX_RESPONSE + 1]),
        });
        assert!(matches!(
            client.queue("ses_a", "msg_a", "hello"),
            Delivery::Rejected(_)
        ));
        assert!(client.verify_pid(5, None).is_err());
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
    }

    #[test]
    fn malformed_urls_connection_failure_and_chunked_body() {
        for url in [
            "http://127.0.0.2:80",
            "http://localhost:80",
            "https://127.0.0.1:80",
            "http://127.0.0.1:0",
            "http://127.0.0.1:80/path",
        ] {
            assert!(endpoint(url).is_err(), "{url}");
        }
        let client = Client {
            address: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            authorization: String::new(),
            timeout: Duration::from_millis(100),
            resume: false,
        };
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::NotSent);
        let parsed = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(parsed.body, b"hello");
    }

    #[test]
    fn invalid_ids_never_reach_server_and_info_requires_success() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = Client::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            "fake",
        )
        .unwrap();
        for id in ["", "ses/a", "ses\r\nHost:evil", "ses?x", "ses a"] {
            assert_eq!(client.queue(id, "msg_a", "hello"), Delivery::NotSent);
            assert_eq!(client.queue("ses_a", id, "hello"), Delivery::NotSent);
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let client = fake(1, |_, _, _| Some(response(401, &json!({"pid": 42}))));
        assert!(client.verify_pid(42, None).is_err());
    }

    #[test]
    fn startup_parser_bounds_noise_and_rejects_nonloopback() {
        let mut input = &b"notice\nserver listening on http://127.0.0.1:1234\n"[..];
        assert_eq!(startup_url(&mut input).unwrap(), "http://127.0.0.1:1234");
        let mut oversized = vec![b'x'; 1025];
        oversized.extend(b"\nserver listening on http://127.0.0.1:1234\n");
        assert!(startup_url(&mut oversized.as_slice()).is_err());
        let mut input = &b"server listening on http://192.0.2.1:1234\n"[..];
        assert!(startup_url(&mut input).is_err());
    }

    #[test]
    fn framed_needs_complete_headers_and_a_framing_header() {
        let with_length = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhel";
        let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n";
        let unframed = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nbody";
        let partial = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n";
        assert!(framed(with_length));
        assert!(framed(chunked));
        // Without a framing header the body only ends at EOF.
        assert!(!framed(unframed));
        assert!(!framed(partial));
        assert!(!framed(b"HTTP/1.1 200 OK\r\nContent-Length: \xff\r\n\r\n"));
    }

    #[test]
    fn a_body_without_framing_is_read_to_the_end_of_the_connection() {
        // The reply looks like an accepted message after its first write, but
        // more bytes follow before the server closes, so it is not that reply.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf).unwrap();
            let body = serde_json::to_vec(&json!({"data": item("msg_a", "hello")})).unwrap();
            // One write, so the client sees a complete-looking reply at once.
            let mut reply = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            reply.extend(&body);
            stream.write_all(&reply).unwrap();
            thread::sleep(Duration::from_millis(150));
            stream.write_all(b" trailing garbage").unwrap();
        });
        let mut client = Client::new(&format!("http://{address}"), "fake").unwrap();
        client.timeout = Duration::from_secs(2);
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
    }

    #[test]
    fn ambiguous_message_framing_is_rejected() {
        let single = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok";
        assert_eq!(parse_response(single).unwrap().body, b"ok");
        // Two lengths, even equal ones, leave the body boundary open to dispute.
        let duplicate = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\nok";
        assert!(parse_response(duplicate).is_none());
        // A length together with chunking is the classic framing conflict.
        let both = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert!(parse_response(both).is_none());
        let chunked_only =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked_only).unwrap().body, b"hello");
    }

    #[test]
    fn startup_output_is_bounded_in_total_and_must_end_in_a_url() {
        // Each line is within its own cap, but together they exceed the total.
        let mut noise = Vec::new();
        for _ in 0..10 {
            noise.extend(vec![b'x'; 1000]);
            noise.push(b'\n');
        }
        noise.extend(b"server listening on http://127.0.0.1:1234\n");
        assert!(startup_url(&mut noise.as_slice()).is_err());

        // Output that ends without the announcement is an error, not a hang.
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut quiet = &b"starting up\nstill starting\n"[..];
            let _ = sender.send(startup_url(&mut quiet).is_err());
        });
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)), Ok(true));
    }

    #[test]
    fn verified_pid_and_explicit_session_create() {
        // On Windows the fake server must report a live member of a job
        // this test holds; on Unix any named pid passes the direct check.
        #[cfg(windows)]
        let (reported, mut member, tree) = live_job_member();
        #[cfg(not(windows))]
        let reported: u32 = 42;
        let client = fake(2, move |i, headers, _| match i {
            0 => {
                assert!(headers.starts_with("GET /api/info "));
                Some(response(200, &json!({"pid": reported})))
            }
            _ => {
                assert!(headers.starts_with("POST /api/session "));
                Some(response(200, &json!({"data": {"id": "ses_explicit"}})))
            }
        });
        #[cfg(windows)]
        client.verify_pid(0, Some(&tree)).unwrap();
        #[cfg(not(windows))]
        client.verify_pid(42, None).unwrap();
        assert_eq!(client.create_session().unwrap(), "ses_explicit");
        #[cfg(windows)]
        {
            tree.terminate().unwrap();
            let _ = member.wait();
        }
    }

    /// A live member of a job this test holds: a child test process that
    /// joins the job on entry and sleeps until reaped. The child and the
    /// tree must stay alive while the pid is checked; dropping the tree
    /// kills the job.
    #[cfg(windows)]
    fn live_job_member() -> (u32, std::process::Child, ProcessTree) {
        // in_job is the tree factory here; its command is unused because a
        // test binary cannot run the launcher path (libtest owns main).
        let resolved = ResolvedCommand::resolve(
            OsStr::new("timeout.exe"),
            &["/t".into(), "60".into()],
            &std::env::temp_dir(),
        )
        .expect("timeout.exe must resolve on PATH");
        let (_command, tree) = resolved.in_job().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "opencode_native::tests::windows_join_self_entry",
                "--nocapture",
            ])
            .env("CROWDED_TEST_JOB_NAME", tree.name())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = child.spawn().unwrap();
        let pid = child.id();
        // Joining happens after the re-exec starts, so the fresh process
        // is not a member the instant spawn returns; poll briefly instead.
        let mut joined = false;
        for _ in 0..200 {
            if tree.contains(pid) {
                joined = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(joined, "child never joined its job");
        (pid, child, tree)
    }

    /// Child entry for the job tests below: join the job named in the
    /// environment, then sleep until reaped.
    #[cfg(windows)]
    #[test]
    fn windows_join_self_entry() {
        if let Some(name) = std::env::var_os("CROWDED_TEST_JOB_NAME") {
            ProcessTree::join_current(name.as_os_str()).unwrap();
            thread::sleep(Duration::from_secs(60));
            std::process::exit(0);
        }
    }

    /// Independent liveness probe: OpenProcess plus the exit code.
    /// (`!contains` alone also passes for a live outsider, so it cannot
    /// prove a process died.)
    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::STILL_ACTIVE;
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
        };

        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if process.is_null() {
            return false;
        }
        let mut code = 0;
        let running =
            unsafe { GetExitCodeProcess(process, &mut code) } != 0 && code == STILL_ACTIVE as u32;
        unsafe { CloseHandle(process) };
        running
    }

    /// Dropping a Server with a live joined member terminates the job and
    /// reaps the child: the same stop_owned path production uses. Without
    /// the terminate call the drop would hang in `wait` on the 60 s
    /// sleeper, so the drop itself is time-bounded; the member's death is
    /// then checked independently of job membership.
    #[cfg(windows)]
    #[test]
    fn dropping_server_with_a_joined_member_reaps_the_tree() {
        let (member_pid, member, tree) = live_job_member();
        assert!(tree.contains(member_pid));
        let client = fake(0, |_, _, _| None);
        let server = Server {
            child: Some(member),
            #[cfg(windows)]
            tree: Some(tree),
            client,
            session: Mutex::new("ses_a".into()),
            url: "http://127.0.0.1:1234".into(),
            binding: Mutex::new(None),
            server_pid: Some(member_pid),
        };
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(server);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(20)).is_ok(),
            "Server drop hung: the job terminate must reap the tree promptly"
        );
        for _ in 0..200 {
            if !process_is_running(member_pid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !process_is_running(member_pid),
            "joined member survived the Server drop"
        );
    }

    fn bound_server(client: &Arc<Client>, room: &str, generation: u64, session: &str) -> Server {
        let mut launch = LaunchBinding::new(room.into(), generation, 42).unwrap();
        launch
            .bind(OwnershipEvidence {
                child_pid: 42,
                reported_pid: 42,
                endpoint: client.address,
                session: session.into(),
                generation,
            })
            .unwrap();
        Server {
            child: None,
            #[cfg(windows)]
            tree: None,
            client: Arc::clone(client),
            session: Mutex::new(session.into()),
            url: format!("http://{}", client.address),
            binding: Mutex::new(Some(launch)),
            server_pid: Some(42),
        }
    }

    #[test]
    fn a_new_session_replaces_the_binding_and_keeps_the_server() {
        let client = fake(2, |i, headers, _| {
            assert!(headers.starts_with("POST /api/session "));
            match i {
                0 => Some(response(200, &json!({"data": {"id": "ses_fresh"}}))),
                _ => Some(response(500, &json!({}))),
            }
        });
        let server = bound_server(&client, "1", 3, "ses_old");
        let old = server
            .binding
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .bound()
            .unwrap()
            .clone();
        let fresh = server.new_session("1".into(), 9).unwrap();
        assert_eq!(fresh.session, "ses_fresh");
        assert_eq!(fresh.endpoint, old.endpoint);
        assert_eq!(fresh.generation, 9);
        assert_eq!(*server.session.lock().unwrap(), "ses_fresh");
        {
            let owned = server.binding.lock().unwrap();
            let launch = owned.as_ref().unwrap();
            assert!(!launch.accepts(&old));
            assert!(launch.accepts(&fresh));
        }
        // A failed creation leaves the binding and the session in place.
        assert!(server.new_session("1".into(), 10).is_err());
        assert_eq!(*server.session.lock().unwrap(), "ses_fresh");
        let owned = server.binding.lock().unwrap();
        let launch = owned.as_ref().unwrap();
        assert!(launch.accepts(&fresh));
        assert!(!launch.accepts(&old));
        assert_eq!(launch.bound().unwrap().generation, 9);
    }

    #[test]
    fn adopt_keeps_the_handoff_session_in_step_with_the_binding() {
        let client = fake(0, |_, _, _| None);
        let server = bound_server(&client, "1", 3, "ses_old");
        let adopted = Binding {
            room: "1".into(),
            session: "ses_reported".into(),
            endpoint: format!("http://{}", client.address),
            generation: 3,
        };
        server.adopt(&adopted).unwrap();
        assert_eq!(server.launch_session().unwrap(), "ses_reported");
        assert_eq!(server.handoff().unwrap().session, "ses_reported");
        let owned = server.binding.lock().unwrap();
        assert_eq!(owned.as_ref().unwrap().bound(), Some(&adopted));
    }

    #[test]
    fn a_new_session_for_another_room_is_refused_and_keeps_the_old_binding() {
        let client = fake(1, |_, headers, _| {
            assert!(headers.starts_with("POST /api/session "));
            Some(response(200, &json!({"data": {"id": "ses_fresh"}})))
        });
        let server = bound_server(&client, "1", 3, "ses_old");
        let old = server
            .binding
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .bound()
            .unwrap()
            .clone();
        assert!(server.new_session("2".into(), 9).is_err());
        let owned = server.binding.lock().unwrap();
        let launch = owned.as_ref().unwrap();
        assert!(launch.accepts(&old));
        assert_eq!(launch.bound().unwrap().session, "ses_old");
        assert_eq!(*server.session.lock().unwrap(), "ses_old");
    }

    #[test]
    fn server_crash_during_request_is_uncertain() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let _ = stream.shutdown(std::net::Shutdown::Both);
        });
        let client = Client::new(&format!("http://{address}"), "fake").unwrap();
        assert_eq!(
            client.queue("ses_a", "msg_a", &"x".repeat(100_000)),
            Delivery::Uncertain
        );
    }

    #[test]
    fn rejects_mismatched_response_identity_and_reconciliation() {
        let client = fake(7, |i, _, _| {
            let value = match i {
                0 => json!({"data":item("msg_other", "hello")}),
                1 => {
                    json!({"data":{"id":"msg_a","sessionID":"ses_other","payload":{"text":"hello"},"delivery":"queue"}})
                }
                2 => {
                    json!({"data":{"id":"msg_a","sessionID":"ses_a","payload":{"text":"hello"},"delivery":"steer"}})
                }
                3 => json!({"data":item("msg_a", "hello")}),
                4 => json!({"data":[item("msg_a", "different")]}),
                5 => {
                    json!({"data":[{"id":"msg_a","sessionID":"ses_other","payload":{"text":"hello"},"delivery":"queue"}]})
                }
                _ => json!({"data":[]}),
            };
            Some(response(if i == 3 { 500 } else { 200 }, &value))
        });
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Conflict);
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Conflict);
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Conflict
        );
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Conflict
        );
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Uncertain
        );
    }

    #[test]
    fn reconcile_does_not_accept_error_status_with_matching_body() {
        let client = fake(1, |_, _, _| {
            Some(response(500, &json!({"data":[item("msg_a", "hello")]})))
        });
        assert_eq!(
            client.reconcile("ses_a", "msg_a", "hello"),
            Delivery::Uncertain
        );
    }

    #[test]
    fn size_limits_and_truncated_content_length() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = Client::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            "fake",
        )
        .unwrap();
        assert_eq!(
            client.queue("ses_a", "msg_a", &"x".repeat(MAX_REQUEST)),
            Delivery::NotSent
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let client = fake(3, |i, _, _| match i {
            0 => Some(response(
                200,
                &json!({"data": item("msg_a", "hello"), "padding": "x".repeat(MAX_RESPONSE)}),
            )),
            1 => {
                let body = serde_json::to_vec(&json!({"data": item("msg_a", "hello")})).unwrap();
                let mut raw = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    body.len() + 1
                )
                .into_bytes();
                raw.extend(body);
                Some(raw)
            }
            _ => {
                let mut result = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
                result.extend(format!("{:x}\r\n", MAX_RESPONSE + 1).as_bytes());
                result.extend(vec![b'a'; MAX_RESPONSE + 1]);
                result.extend(b"\r\n0\r\n\r\n");
                Some(result)
            }
        });
        for _ in 0..3 {
            assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
        }
        let mut chunks = format!("{:x}\r\n", MAX_RESPONSE + 1).into_bytes();
        chunks.extend(vec![b'a'; MAX_RESPONSE + 1]);
        chunks.extend(b"\r\n0\r\n\r\n");
        assert!(decode_chunks(&chunks).is_none());
    }

    #[test]
    fn silent_server_times_out_and_keepalive_reply_completes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            thread::sleep(Duration::from_millis(700));
        });
        let mut client = Client::new(&format!("http://{address}"), "fake").unwrap();
        client.timeout = Duration::from_millis(150);
        let start = Instant::now();
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
        assert!(start.elapsed() < Duration::from_millis(600));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.strip_prefix("Content-Length: ") {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            for _ in 0..14 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        let mut client = Client::new(&format!("http://{address}"), "fake").unwrap();
        client.timeout = Duration::from_millis(150);
        let start = Instant::now();
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Uncertain);
        assert!(start.elapsed() < Duration::from_millis(600));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0; 4096];
            let _ = stream.read(&mut buf).unwrap();
            stream
                .write_all(&response(200, &json!({"data": item("msg_a", "hello")})))
                .unwrap();
            thread::sleep(Duration::from_millis(700));
        });
        let mut client = Client::new(&format!("http://{address}"), "fake").unwrap();
        client.timeout = Duration::from_millis(400);
        assert_eq!(client.queue("ses_a", "msg_a", "hello"), Delivery::Accepted);
    }

    #[cfg(unix)]
    #[test]
    fn server_binding_gates_and_claim_verifies_pid() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let client = fake(3, move |i, headers, _| match i {
            0 => {
                assert!(headers.starts_with("GET /api/info "));
                Some(response(200, &json!({"pid": pid})))
            }
            1 => {
                assert!(headers.starts_with("POST /api/session/ses_a/prompt "));
                Some(response(200, &json!({"data": item("msg_a", "hello")})))
            }
            _ => {
                assert!(headers.starts_with("GET /api/session/ses_a/inbox "));
                Some(response(200, &json!({"data": [item("msg_a", "hello")]})))
            }
        });
        let mut server = Server {
            child: Some(child),
            #[cfg(windows)]
            tree: None,
            client,
            session: Mutex::new("ses_a".into()),
            url: "http://127.0.0.1:1234".into(),
            binding: Mutex::new(None),
            server_pid: None,
        };
        let fake_binding = Binding {
            room: "room".into(),
            session: "ses_a".into(),
            endpoint: "http://127.0.0.1:1234".into(),
            generation: 1,
        };
        assert_eq!(
            server
                .queue_async(&fake_binding, "msg_a".into(), "hello".into())
                .recv()
                .unwrap(),
            Delivery::NotSent
        );
        assert_eq!(
            server
                .reconcile_async(&fake_binding, "msg_a".into(), "hello".into())
                .recv()
                .unwrap(),
            Delivery::NotSent
        );
        let binding = server.claim("room".into(), 1).unwrap();
        assert!(server.claim("other".into(), 1).is_err());
        let mut other = binding.clone();
        other.generation += 1;
        assert_eq!(
            server
                .queue_async(&other, "msg_a".into(), "hello".into())
                .recv()
                .unwrap(),
            Delivery::NotSent
        );
        other = binding.clone();
        other.room = "other".into();
        assert_eq!(
            server
                .reconcile_async(&other, "msg_a".into(), "hello".into())
                .recv()
                .unwrap(),
            Delivery::NotSent
        );
        assert_eq!(
            server
                .queue_async(&binding, "msg_a".into(), "hello".into())
                .recv_timeout(Duration::from_secs(3))
                .unwrap(),
            Delivery::Accepted
        );
        assert_eq!(
            server
                .reconcile_async(&binding, "msg_a".into(), "hello".into())
                .recv_timeout(Duration::from_secs(3))
                .unwrap(),
            Delivery::Accepted
        );
    }

    #[cfg(unix)]
    #[test]
    fn outbox_adapter_binds_and_rejects_conflicting_response() {
        use crate::native_outbox::{Adapter, AdapterResult, Message, Status};
        use crate::opencode_outbox::OpenCodeAdapter;
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let message_id = "a".repeat(64);
        let expected = format!("msg_{message_id}");
        let client = fake(3, move |i, headers, _| {
            if i == 0 {
                assert!(headers.starts_with("GET /api/info "));
                Some(response(200, &json!({"pid":pid})))
            } else {
                assert!(headers.starts_with("POST /api/session/ses_a/prompt "));
                Some(response(200, &json!({"data": item(&expected, "hello")})))
            }
        });
        let mut server = Server {
            child: Some(child),
            #[cfg(windows)]
            tree: None,
            client,
            session: Mutex::new("ses_a".into()),
            url: "http://127.0.0.1:1234".into(),
            binding: Mutex::new(None),
            server_pid: None,
        };
        let binding = server.claim("room-a".into(), 1).unwrap();
        let server = Arc::new(server);
        assert!(
            OpenCodeAdapter::new(vec![
                (binding.clone(), server.clone()),
                (binding.clone(), server.clone())
            ])
            .is_err()
        );
        let adapter = OpenCodeAdapter::new(vec![(binding.clone(), server)]).unwrap();
        let mut message = Message {
            id: message_id,
            source: "source".into(),
            destination: "room-a".into(),
            binding: binding.clone(),
            text: "hello".into(),
            hash: format!("{:x}", Sha256::digest(b"hello")),
            created_ms: 0,
            sequence: 1,
            attempts: Vec::new(),
            receipt: None,
            status: Status::Sending,
        };
        let mut stale = message.clone();
        stale.binding.generation += 1;
        assert!(matches!(adapter.send(stale), AdapterResult::Dropped(_)));
        let mut bad_id = message.clone();
        bad_id.id.pop();
        assert!(matches!(adapter.send(bad_id), AdapterResult::Failed(_)));
        let mut bad_hash = message.clone();
        bad_hash.hash = "wrong".into();
        assert!(matches!(adapter.send(bad_hash), AdapterResult::Failed(_)));
        assert!(matches!(
            adapter.send(message.clone()),
            AdapterResult::Accepted(Some(_))
        ));
        message.text = "changed".into();
        message.hash = format!("{:x}", Sha256::digest(b"changed"));
        assert!(matches!(adapter.send(message), AdapterResult::Failed(_)));
    }

    #[cfg(unix)]
    #[test]
    fn adapter_timeout_and_disconnect_remain_uncertain_in_outbox() {
        use crate::native_outbox::{
            Adapter, AdapterResult, BackgroundOutbox, Command, Event, Message, Status,
        };
        use crate::opencode_outbox::OpenCodeAdapter;
        use std::collections::HashMap;
        fn disconnected() -> mpsc::Receiver<Delivery> {
            let (tx, rx) = mpsc::channel();
            drop(tx);
            rx
        }
        for is_timeout in [true, false] {
            let child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap();
            let pid = child.id();
            let client = fake(if is_timeout { 3 } else { 1 }, move |i, _, _| {
                if i == 0 {
                    Some(response(200, &json!({"pid": pid})))
                } else {
                    thread::sleep(Duration::from_millis(200));
                    None
                }
            });
            let mut server = Server {
                child: Some(child),
                #[cfg(windows)]
                tree: None,
                client,
                session: Mutex::new("ses_a".into()),
                url: "http://127.0.0.1:1234".into(),
                binding: Mutex::new(None),
                server_pid: None,
            };
            let binding = server.claim("room-a".into(), 1).unwrap();
            let mut adapter =
                OpenCodeAdapter::new(vec![(binding.clone(), Arc::new(server))]).unwrap();
            if is_timeout {
                adapter.set_test_timeout(Duration::from_millis(20));
            } else {
                adapter.set_test_reply(Duration::from_millis(20), disconnected);
            }
            let message = Message {
                id: "a".repeat(64),
                source: "s".into(),
                destination: binding.room.clone(),
                binding: binding.clone(),
                text: "hello".into(),
                hash: format!("{:x}", Sha256::digest(b"hello")),
                created_ms: 0,
                sequence: 1,
                attempts: Vec::new(),
                receipt: None,
                status: Status::Sending,
            };
            assert!(matches!(adapter.send(message), AdapterResult::Uncertain));
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("outbox.json");
            let outbox = BackgroundOutbox::start(path.clone(), 4, 4, 1, Arc::new(adapter));
            outbox
                .try_send(
                    1,
                    Command::Enqueue {
                        source: "s".into(),
                        binding: binding.clone(),
                        text: "hello".into(),
                    },
                )
                .ok()
                .unwrap();
            assert!(matches!(
                outbox
                    .events
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .event,
                Event::Queued(_)
            ));
            let bindings = HashMap::from([(binding.room.clone(), binding)]);
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                outbox
                    .try_send(
                        2,
                        Command::Schedule {
                            bindings: bindings.clone(),
                            paused: false,
                            fused: false,
                            pending: Vec::new(),
                        },
                    )
                    .ok()
                    .unwrap();
                let event = outbox.events.recv_timeout(Duration::from_secs(3)).unwrap();
                if let Event::Updated(messages) = event.event {
                    if messages[0].status == Status::Uncertain {
                        break;
                    }
                    assert_eq!(messages[0].status, Status::Sending);
                } else {
                    panic!("unexpected outbox event");
                }
                assert!(Instant::now() < deadline);
                thread::yield_now();
            }
            let disk: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(disk["messages"][0]["status"], "Uncertain");
        }
    }

    #[cfg(unix)]
    #[test]
    fn claim_rejects_wrong_pid() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let client = fake(1, move |_, _, _| {
            Some(response(200, &json!({"pid":pid + 1})))
        });
        let mut server = Server {
            child: Some(child),
            #[cfg(windows)]
            tree: None,
            client,
            session: Mutex::new("ses_a".into()),
            url: "http://127.0.0.1:1234".into(),
            binding: Mutex::new(None),
            server_pid: None,
        };
        assert!(server.claim("room".into(), 1).is_err());
        assert!(server.binding.lock().unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn ownership_accepts_the_spawned_process_or_its_direct_child_only() {
        let mut wrapper = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(wrapper.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().unwrap();

        assert!(owned_by(wrapper.id(), grandchild, None));
        assert!(!owned_by(std::process::id(), grandchild, None));
        assert!(!owned_by(grandchild, wrapper.id(), None));

        let _ = wrapper.kill();
        let _ = wrapper.wait();
        let _ = std::process::Command::new("kill")
            .args(["-9", &grandchild.to_string()])
            .status();
    }

    /// Windows ownership is job membership, not parentage: the spawned pid
    /// is a launcher, so `reported == spawned` must not apply. Only the
    /// live member passes; an outsider, an exited pid, and a missing job
    /// all fail closed. Terminating the job reaps the tree.
    #[cfg(windows)]
    #[test]
    fn windows_job_membership_proves_only_the_live_server_tree() {
        let (member_pid, mut member, tree) = live_job_member();

        assert!(tree.contains(member_pid));
        assert!(owned_by(0, member_pid, Some(&tree)));
        assert!(!tree.contains(std::process::id()));
        assert!(!owned_by(0, std::process::id(), Some(&tree)));
        assert!(!owned_by(member_pid, member_pid, None));

        let mut quick = std::process::Command::new("cmd")
            .args(["/c", "exit", "0"])
            .spawn()
            .unwrap();
        let quick_pid = quick.id();
        quick.wait().unwrap();
        assert!(!tree.contains(quick_pid));
        assert!(!owned_by(quick_pid, quick_pid, Some(&tree)));

        tree.terminate().unwrap();
        member.wait().unwrap();
        for _ in 0..200 {
            if !tree.contains(member_pid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(!tree.contains(member_pid));
        assert!(!owned_by(member_pid, member_pid, Some(&tree)));
        // Independent of job membership: the member itself is dead.
        for _ in 0..200 {
            if !process_is_running(member_pid) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            !process_is_running(member_pid),
            "joined member survived the job terminate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dropping_server_kills_and_reaps_child() {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let client = fake(0, |_, _, _| None);
        let server = Server {
            child: Some(child),
            #[cfg(windows)]
            tree: None,
            client,
            session: Mutex::new("ses_a".into()),
            url: "http://127.0.0.1:1234".into(),
            binding: Mutex::new(None),
            server_pid: None,
        };
        drop(server);
        let status = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[test]
    fn startup_rejects_wrong_pid_and_reaps_child() {
        use std::os::unix::fs::PermissionsExt;
        let client = fake(1, |_, headers, _| {
            if headers.starts_with("GET /api/info ") {
                Some(response(200, &json!({"pid": 0})))
            } else {
                Some(response(200, &json!({"data":{"id":"ses_a"}})))
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("fake-server");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > child.pid\nif [ -z \"${{OPENCODE_PASSWORD:-}}\" ]; then printf missing > audit; else case \"$*\" in *\"$OPENCODE_PASSWORD\"*) printf leaked > audit;; *) printf safe > audit;; esac; fi\nprintf 'server listening on http://{}\\n'\nexec sleep 30\n",
                client.address
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(Server::start(program.as_os_str(), dir.path()).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("audit")).unwrap(),
            "safe"
        );
        let pid = std::fs::read_to_string(dir.path().join("child.pid")).unwrap();
        let status = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        if status.success() {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid])
                .status();
        }
        assert!(!status.success());
    }

    #[test]
    #[ignore]
    fn isolated_real_server_smoke() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let variables = crate::native_launch::tests::isolated_variables(root.path());
        let mut server =
            Server::start_with_environment(OsStr::new("opencode"), &[], cwd.path(), &variables)
                .unwrap();
        let binding = server.claim("isolated-room".into(), 1).unwrap();
        assert_eq!(
            server
                .queue_async(
                    &binding,
                    "msg_1234567890abcdef12345678".into(),
                    "isolated\n✓".into()
                )
                .recv_timeout(Duration::from_secs(10))
                .unwrap(),
            Delivery::Accepted
        );
        assert!(server.claim("other-room".into(), 1).is_err());
    }
}
