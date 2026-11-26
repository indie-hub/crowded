//! Launch-side state for Crowded-owned OpenCode rooms: the persisted launch
//! generation, the owned server with its TUI hand-off, and deferred reaping.
#![allow(dead_code)]

use std::{
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    command::ResolvedCommand,
    native_outbox::Binding,
    opencode_native::{Handoff, Server},
    pane::session_state::{create_private_directory, private_file, refuse_symlink},
};

const GENERATION_FILE: &str = "native-generation";
const GENERATION_LOCK: &str = "native-generation.lock";
/// A freshly written executable can take a second or more for its first run
/// on a busy machine, so this is generous; it only bounds a hung binary.
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);
/// The first release the owned-server path was exercised against.
const MINIMUM_VERSION: (u64, u64, u64) = (2, 2, 5);
/// Server start, ownership check and session creation each have their own
/// bound; this caps the whole launch for a caller that has to wait for it.
pub(crate) const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Hands out launch generations that never repeat, even across Crowded
/// restarts, because the next value is durable before it is returned.
pub(crate) struct GenerationAllocator {
    directory: PathBuf,
}

impl GenerationAllocator {
    pub(crate) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    pub(crate) fn next(&self) -> io::Result<u64> {
        create_private_directory(&self.directory)?;
        let lock_path = self.directory.join(GENERATION_LOCK);
        refuse_symlink(&lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.lock()?;

        let path = self.directory.join(GENERATION_FILE);
        refuse_symlink(&path)?;
        let current = match fs::read_to_string(&path) {
            Ok(text) => text.trim().parse::<u64>().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "native generation state is corrupt and was left unchanged",
                )
            })?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error),
        };
        let next = current
            .checked_add(1)
            .ok_or_else(|| io::Error::other("native generation exhausted"))?;

        let temporary = self
            .directory
            .join(format!("{GENERATION_FILE}.{}.tmp", std::process::id()));
        refuse_symlink(&temporary)?;
        let _ = fs::remove_file(&temporary);
        let mut file = private_file(&temporary)?;
        file.write_all(next.to_string().as_bytes())?;
        file.sync_all()?;
        drop(file);
        if let Err(error) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        #[cfg(unix)]
        fs::File::open(&self.directory)?.sync_all()?;
        Ok(next)
    }
}

enum Reap {
    Value(Box<dyn Send>),
    Flush(mpsc::SyncSender<()>),
}

static REAPER: Mutex<Option<mpsc::Sender<Reap>>> = Mutex::new(None);
/// Launch workers that have not finished. Counting them, rather than keeping
/// join handles, lets every caller of [`wait_for_reaps`] wait for all of them.
static ACTIVE_LAUNCHES: (Mutex<usize>, Condvar) = (Mutex::new(0), Condvar::new());
const LAUNCH_JOIN_TIMEOUT: Duration = Duration::from_secs(120);

struct ActiveLaunch;

impl ActiveLaunch {
    fn register() -> Self {
        if let Ok(mut count) = ACTIVE_LAUNCHES.0.lock() {
            *count += 1;
        }
        Self
    }
}

impl Drop for ActiveLaunch {
    fn drop(&mut self) {
        if let Ok(mut count) = ACTIVE_LAUNCHES.0.lock() {
            *count = count.saturating_sub(1);
            ACTIVE_LAUNCHES.1.notify_all();
        }
    }
}
const REAP_FLUSH_TIMEOUT: Duration = Duration::from_secs(60);

/// Starts the one long-lived reaper thread. Native launch calls this before any
/// server exists, so a failure to create a thread is a startup error and never
/// a reason to destroy a server on the terminal thread later.
pub(crate) fn ensure_reaper() -> io::Result<()> {
    let mut reaper = REAPER
        .lock()
        .map_err(|_| io::Error::other("reaper state is poisoned"))?;
    if reaper.is_some() {
        return Ok(());
    }
    let (sender, receiver) = mpsc::channel::<Reap>();
    thread::Builder::new()
        .name("native-reaper".into())
        .spawn(move || {
            for message in receiver {
                match message {
                    Reap::Value(value) => drop(value),
                    Reap::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        })?;
    *reaper = Some(sender);
    Ok(())
}

/// Drops its value on the reaper thread, so a destructor that kills and waits
/// for a child process never runs on the terminal event loop. Only if the
/// reaper cannot be reached at all is the value dropped in place, because
/// leaking a child process would be worse.
pub(crate) struct Reaped<T: Send + 'static> {
    value: Option<T>,
}

impl<T: Send + 'static> Reaped<T> {
    pub(crate) fn new(value: T) -> Self {
        Self { value: Some(value) }
    }

    pub(crate) fn get(&self) -> &T {
        self.value.as_ref().expect("value is present until drop")
    }
}

impl<T: Send + 'static> Drop for Reaped<T> {
    fn drop(&mut self) {
        let Some(value) = self.value.take() else {
            return;
        };
        let mut message = Reap::Value(Box::new(value));
        for _ in 0..2 {
            let sender = REAPER.lock().ok().and_then(|guard| guard.clone());
            match sender {
                Some(sender) => match sender.send(message) {
                    Ok(()) => return,
                    Err(mpsc::SendError(returned)) => message = returned,
                },
                None => {
                    if ensure_reaper().is_err() {
                        break;
                    }
                }
            }
        }
        drop(message);
    }
}

/// Waits for every launch worker and then for the reaper to finish everything
/// queued, for use when Crowded exits. Workers go first: a worker whose caller
/// gave up still owns a child and hands it to the reaper when it finishes.
pub(crate) fn wait_for_reaps() {
    if let Ok(count) = ACTIVE_LAUNCHES.0.lock() {
        let _ = ACTIVE_LAUNCHES
            .1
            .wait_timeout_while(count, LAUNCH_JOIN_TIMEOUT, |active| *active > 0);
    }
    let sender = REAPER.lock().ok().and_then(|guard| guard.clone());
    if let Some(sender) = sender {
        let (done, finished) = mpsc::sync_channel(1);
        if sender.send(Reap::Flush(done)).is_ok() {
            let _ = finished.recv_timeout(REAP_FLUSH_TIMEOUT);
        }
    }
}

/// One room's launch: what to spawn, what to verify, and the private config
/// that must outlive the server.
pub(crate) struct NativeLaunch {
    /// The OpenCode binary whose version and environment Crowded verifies.
    pub(crate) guest: OsString,
    /// The process Crowded spawns: OpenCode itself or its Headroom wrapper.
    pub(crate) program: OsString,
    /// Arguments before the fixed `serve --hostname 127.0.0.1 --port 0`.
    pub(crate) prefix: Vec<OsString>,
    pub(crate) cwd: PathBuf,
    pub(crate) room: String,
    pub(crate) generation: u64,
    pub(crate) variables: Variables,
    /// Headroom replaces the inline OpenCode configuration, so the room's own
    /// document is a file the server keeps referring to.
    pub(crate) config: Option<tempfile::NamedTempFile>,
}

impl NativeLaunch {
    /// The guest itself, with no wrapper; used by tests and direct launches.
    pub(crate) fn direct(
        program: &OsStr,
        cwd: impl Into<PathBuf>,
        room: String,
        generation: u64,
        variables: &[(OsString, OsString)],
    ) -> Self {
        Self {
            guest: program.to_os_string(),
            program: program.to_os_string(),
            prefix: Vec::new(),
            cwd: cwd.into(),
            room,
            generation,
            variables: variables.to_vec(),
            config: None,
        }
    }
}

/// One room's owned server, its exact binding, and the credentials its TUI needs.
pub(crate) struct NativeRoom {
    server: Reaped<Arc<Server>>,
    binding: Binding,
    handoff: Handoff,
    /// Kept alive for as long as the server can read it.
    _config: Option<tempfile::NamedTempFile>,
}

impl NativeRoom {
    pub(crate) fn binding(&self) -> &Binding {
        &self.binding
    }

    /// Puts the room back on a binding it held before a failed clear, so the
    /// server, the room and its terminal arguments name the same session again.
    pub(crate) fn restore_binding(&mut self, binding: &Binding) -> io::Result<()> {
        self.server.get().adopt(binding)?;
        self.binding = binding.clone();
        self.handoff.session = binding.session.clone();
        Ok(())
    }

    /// Starts a new session on this room's owned server and moves the room to
    /// it: the same endpoint and server, a new generation and session, so the
    /// caller can respawn the TUI against [`Self::tui_arguments`].
    pub(crate) fn clear_session(&mut self, generation: u64) -> io::Result<Binding> {
        let binding = self
            .server
            .get()
            .new_session(self.binding.room.clone(), generation)?;
        self.binding = binding.clone();
        self.handoff.session = binding.session.clone();
        Ok(binding)
    }

    /// Sharing the server is for the outbox adapter, which must keep and drop
    /// its handle off the terminal event loop.
    pub(crate) fn server(&self) -> Arc<Server> {
        Arc::clone(self.server.get())
    }

    /// Replaces OpenCode's private `--standalone` server with this one.
    pub(crate) fn tui_arguments(&self) -> Vec<OsString> {
        vec![
            "--server".into(),
            self.handoff.url.clone().into(),
            "--session".into(),
            self.handoff.session.clone().into(),
        ]
    }

    /// For the TUI child's environment only: never argv, logs or saved state.
    pub(crate) fn tui_password(&self) -> &str {
        &self.handoff.password
    }
}

/// The room's effective OpenCode environment: the same variables go to the
/// owned server, which runs the agent, and to the terminal attached to it.
pub(crate) type Variables = Vec<(OsString, OsString)>;

/// Starts the owned server, proves it is the child, creates the session and
/// binds it to this launch generation. Blocks; callers on a thread that must
/// stay responsive use [`begin`].
pub(crate) fn start(launch: NativeLaunch) -> io::Result<NativeRoom> {
    start_cancellable(launch, &AtomicBool::new(false))
}

fn start_cancellable(launch: NativeLaunch, cancelled: &AtomicBool) -> io::Result<NativeRoom> {
    ensure_reaper()?;
    check_version(
        &launch.guest,
        &launch.cwd,
        &launch.variables,
        VERSION_TIMEOUT,
    )?;
    // The version check can be slow; do not start a server nobody waits for.
    if cancelled.load(Ordering::SeqCst) {
        return Err(io::Error::other("native launch was cancelled"));
    }
    // A launch abandoned from here on still ends cleanly: its result has no
    // receiver, so the server it owns is handed to the reaper.
    let mut server = Server::start_with_environment(
        &launch.program,
        &launch.prefix,
        &launch.cwd,
        &launch.variables,
    )?;
    // Binding needs exclusive access, so it happens before the server is shared.
    let binding = server.claim(launch.room, launch.generation)?;
    // A delivered message is meant to be acted on, not just stored.
    server.resume_turns()?;
    let handoff = server.handoff()?;
    Ok(NativeRoom {
        server: Reaped::new(Arc::new(server)),
        binding,
        handoff,
        _config: launch.config,
    })
}

/// A launch running on its own worker. Dropping it before it finishes cancels
/// it: the worker kills and waits for the child it owns.
pub(crate) struct Launch {
    receiver: mpsc::Receiver<io::Result<NativeRoom>>,
    cancelled: Arc<AtomicBool>,
}

impl Launch {
    pub(crate) fn wait(self, timeout: Duration) -> io::Result<NativeRoom> {
        match self.receiver.recv_timeout(timeout) {
            Ok(result) => result,
            // Returning drops the launch, which cancels it.
            Err(_) => Err(io::Error::other(
                "native OpenCode server did not start in time",
            )),
        }
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

/// Starts a launch on a tracked worker, so exit can join it.
pub(crate) fn begin(launch: NativeLaunch) -> Launch {
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancelled);
    let failed = sender.clone();
    let active = ActiveLaunch::register();
    let spawned = thread::Builder::new()
        .name("native-launch".into())
        .spawn(move || {
            // Dropped last, after the result and any server it still owns have
            // been handed on, so waiting for no active launches is enough.
            let _active = active;
            let _ = sender.send(start_cancellable(launch, &flag));
        });
    if let Err(error) = spawned {
        // The closure, and the registration it owned, are gone with the error.
        let _ = failed.send(Err(error));
    }
    Launch {
        receiver,
        cancelled,
    }
}

fn parse_version(output: &str) -> Option<(u64, u64, u64)> {
    let token = output.split_whitespace().find(|token| {
        token
            .trim_start_matches('v')
            .starts_with(|c: char| c.is_ascii_digit())
    })?;
    let mut parts = token.trim_start_matches('v').split('.');
    let mut number = || {
        let part = parts.next()?;
        let digits = part
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .filter(|digits| !digits.is_empty())?;
        digits.parse::<u64>().ok()
    };
    Some((number()?, number()?, number()?))
}

fn supports_native(version: (u64, u64, u64)) -> bool {
    version >= MINIMUM_VERSION
}

fn check_version(
    program: &OsStr,
    cwd: &Path,
    variables: &[(OsString, OsString)],
    timeout: Duration,
) -> io::Result<()> {
    let arguments = [OsString::from("--version")];
    let mut command = ResolvedCommand::resolve(program, &arguments, cwd)?.standard();
    command.current_dir(cwd).env("PWD", cwd);
    // The same environment the server and the terminal get, so the version that
    // is checked is the one that will run.
    for (key, value) in variables {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other("OpenCode version check timed out"));
        }
        thread::sleep(Duration::from_millis(20));
    }
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let _ = stdout.by_ref().take(256).read_to_string(&mut output);
    }
    match parse_version(&output) {
        Some(version) if supports_native(version) => Ok(()),
        Some((major, minor, patch)) => Err(io::Error::other(format!(
            "native delivery needs OpenCode {}.{}.{} or newer; found {major}.{minor}.{patch}",
            MINIMUM_VERSION.0, MINIMUM_VERSION.1, MINIMUM_VERSION.2
        ))),
        None => Err(io::Error::other(
            "native delivery could not read the OpenCode version",
        )),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
        sync::atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn generations_increase_persist_and_never_repeat() {
        let dir = tempfile::tempdir().unwrap();
        let first = GenerationAllocator::new(dir.path().join(".crowded"));
        assert_eq!(first.next().unwrap(), 1);
        assert_eq!(first.next().unwrap(), 2);
        // A new allocator models a Crowded restart.
        let restarted = GenerationAllocator::new(dir.path().join(".crowded"));
        assert_eq!(restarted.next().unwrap(), 3);
    }

    #[test]
    fn concurrent_allocators_never_hand_out_the_same_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".crowded");
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let allocator = GenerationAllocator::new(path.clone());
                thread::spawn(move || {
                    (0..5)
                        .map(|_| allocator.next().unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<u64> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        all.sort_unstable();
        assert_eq!(all, (1..=40).collect::<Vec<u64>>());
    }

    #[test]
    fn corrupt_generation_state_is_reported_and_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().join(".crowded");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(GENERATION_FILE), b"not a number").unwrap();
        let error = GenerationAllocator::new(directory.clone())
            .next()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read(directory.join(GENERATION_FILE)).unwrap(),
            b"not a number"
        );
    }

    #[cfg(unix)]
    #[test]
    fn generation_state_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let directory = dir.path().join(".crowded");
        GenerationAllocator::new(directory.clone()).next().unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&directory), 0o700);
        assert_eq!(mode(&directory.join(GENERATION_FILE)), 0o600);
    }

    struct SlowDrop(Arc<AtomicBool>);

    impl Drop for SlowDrop {
        fn drop(&mut self) {
            thread::sleep(Duration::from_millis(300));
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn dropping_reaped_value_returns_promptly_and_finishes_in_the_background() {
        let finished = Arc::new(AtomicBool::new(false));
        let reaped = Reaped::new(SlowDrop(finished.clone()));
        let start = Instant::now();
        drop(reaped);
        assert!(start.elapsed() < Duration::from_millis(150));
        assert!(!finished.load(Ordering::SeqCst));
        wait_for_reaps();
        assert!(finished.load(Ordering::SeqCst));
    }

    #[test]
    fn versions_parse_and_old_ones_are_refused() {
        assert_eq!(parse_version("v2.2.5\n"), Some((2, 2, 5)));
        assert_eq!(parse_version("opencode 2.10.0"), Some((2, 10, 0)));
        assert_eq!(parse_version("2.3.1-beta.2"), Some((2, 3, 1)));
        assert_eq!(parse_version("no version"), None);
        assert_eq!(parse_version("v2.2"), None);
        assert!(supports_native((2, 2, 5)));
        assert!(supports_native((3, 0, 0)));
        assert!(!supports_native((2, 2, 4)));
        assert!(!supports_native((1, 99, 99)));
    }

    /// A stand-in OpenCode: `--version` prints a version; `serve` records its
    /// pid, password, home and arguments, announces a fake server's address,
    /// and stays alive so the pid can be checked.
    #[cfg(unix)]
    pub(crate) struct FakeRoom {
        pub(crate) program: PathBuf,
        pub(crate) dir: PathBuf,
        pub(crate) state: Arc<FakeState>,
        pub(crate) address: std::net::SocketAddr,
    }

    #[cfg(unix)]
    impl FakeRoom {
        /// Stops listening, so later connections are refused.
        pub(crate) fn shutdown(&self) {
            self.state.stop.store(true, Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(self.address);
        }
    }

    /// How the stand-in server answers prompts, and what it received.
    #[derive(Clone, Copy, Default, PartialEq)]
    pub(crate) enum FakeMode {
        #[default]
        Normal,
        /// Prompt and inbox requests get 401.
        Unauthorized,
        /// A prompt is read and the connection closed without any reply.
        Drop,
        /// The prompt is stored, then the connection is closed without a reply.
        StoreThenDrop,
    }

    #[derive(Default)]
    pub(crate) struct FakeState {
        pub(crate) mode: Mutex<FakeMode>,
        /// Milliseconds a prompt waits before it is answered.
        pub(crate) delay_ms: std::sync::atomic::AtomicU64,
        /// Prompts stored by ID, in arrival order.
        pub(crate) received: Mutex<Vec<(String, String)>>,
        pub(crate) stop: AtomicBool,
        /// The `resume` flag of every prompt the server saw.
        pub(crate) resumes: Mutex<Vec<bool>>,
        /// How many sessions this stand-in has created, and the current one.
        pub(crate) sessions: std::sync::atomic::AtomicUsize,
        pub(crate) session: Mutex<String>,
        /// Session ids whose interrupt was requested, in arrival order.
        pub(crate) interrupts: Mutex<Vec<String>>,
        /// The `interrupted` flag the stand-in returns for an accepted
        /// interrupt: false models an idle session, true a stopped run.
        pub(crate) interrupt_interrupted: AtomicBool,
        /// Answer interrupt requests with HTTP 500 instead of accepting them.
        pub(crate) interrupt_failure: AtomicBool,
        /// Answer interrupt requests with 200 and no `interrupted` flag.
        pub(crate) interrupt_malformed: AtomicBool,
        /// Answer session creation with HTTP 500 instead of creating one.
        pub(crate) session_failure: AtomicBool,
    }

    /// The session the stand-in currently serves. The first one keeps the
    /// historical name; later ones are numbered, as a real server would.
    fn serving_session(state: &FakeState, tag: &str) -> String {
        let held = state.session.lock().unwrap();
        if held.is_empty() {
            format!("ses_{tag}")
        } else {
            held.clone()
        }
    }

    #[cfg(unix)]
    pub(crate) fn fake_room(root: &Path, name: &str, version: &str) -> FakeRoom {
        fake_room_with_delay(root, name, version, 0)
    }

    #[cfg(unix)]
    pub(crate) fn fake_room_with_delay(
        root: &Path,
        name: &str,
        version: &str,
        delay: u64,
    ) -> FakeRoom {
        fake_room_with_delays(root, name, version, 0, delay)
    }

    #[cfg(unix)]
    pub(crate) fn fake_room_with_delays(
        root: &Path,
        name: &str,
        version: &str,
        version_delay: u64,
        delay: u64,
    ) -> FakeRoom {
        use std::os::unix::fs::PermissionsExt;
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let tag = name.to_owned();
        let pid_file = dir.join("pid");
        let password_file = dir.join("password");
        let state = Arc::new(FakeState::default());
        let server_state = Arc::clone(&state);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                if server_state.stop.load(Ordering::SeqCst) {
                    break;
                }
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut authorization = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line.strip_prefix("Authorization: ") {
                        authorization = value.trim().to_owned();
                    }
                    if let Some(value) = line.strip_prefix("Content-Length: ") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let password = fs::read_to_string(&password_file).unwrap_or_default();
                let expected = {
                    use base64::{Engine, engine::general_purpose::STANDARD};
                    format!("Basic {}", STANDARD.encode(format!("opencode:{password}")))
                };
                let is_prompt =
                    request.starts_with("POST /api/session/") && request.contains("/prompt ");
                let is_inbox =
                    request.starts_with("GET /api/session/") && request.contains("/inbox ");
                let is_interrupt =
                    request.starts_with("POST /api/session/") && request.contains("/interrupt ");
                let mode = *server_state.mode.lock().unwrap();
                if is_prompt && mode == FakeMode::Drop {
                    continue;
                }
                if is_prompt && mode == FakeMode::StoreThenDrop {
                    let request_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let id = request_body["id"].as_str().unwrap().to_owned();
                    let text = request_body["text"].as_str().unwrap().to_owned();
                    let mut received = server_state.received.lock().unwrap();
                    if !received.iter().any(|(known, _)| *known == id) {
                        received.push((id, text));
                    }
                    continue;
                }
                let (status, payload) = if authorization != expected
                    || ((is_prompt || is_inbox) && mode == FakeMode::Unauthorized)
                {
                    (401, "{}".to_owned())
                } else if is_prompt {
                    let request_body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let id = request_body["id"].as_str().unwrap().to_owned();
                    let text = request_body["text"].as_str().unwrap().to_owned();
                    server_state
                        .resumes
                        .lock()
                        .unwrap()
                        .push(request_body["resume"].as_bool().unwrap_or(false));
                    let delay = server_state
                        .delay_ms
                        .load(std::sync::atomic::Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(delay));
                    let mut received = server_state.received.lock().unwrap();
                    // A repeated ID keeps its first content, as the real server does.
                    let stored = match received.iter().find(|(known, _)| *known == id) {
                        Some((_, stored)) => stored.clone(),
                        None => {
                            received.push((id.clone(), text.clone()));
                            text
                        }
                    };
                    let item = serde_json::json!({
                        "id": id, "sessionID": serving_session(&server_state, &tag),
                        "payload": {"text": stored}, "delivery": "queue"
                    });
                    (200, serde_json::json!({"data": item}).to_string())
                } else if is_inbox {
                    let session = serving_session(&server_state, &tag);
                    let items: Vec<_> = server_state
                        .received
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|(id, text)| {
                            serde_json::json!({
                                "id": id, "sessionID": session,
                                "payload": {"text": text}, "delivery": "queue"
                            })
                        })
                        .collect();
                    (200, serde_json::json!({"data": items}).to_string())
                } else if is_interrupt {
                    if server_state.interrupt_failure.load(Ordering::SeqCst) {
                        (500, "{}".to_owned())
                    } else {
                        let id = request
                            .trim_start_matches("POST /api/session/")
                            .split('/')
                            .next()
                            .unwrap_or_default()
                            .to_owned();
                        server_state.interrupts.lock().unwrap().push(id);
                        if server_state.interrupt_malformed.load(Ordering::SeqCst) {
                            (200, "{}".to_owned())
                        } else {
                            let interrupted =
                                server_state.interrupt_interrupted.load(Ordering::SeqCst);
                            (200, format!("{{\"interrupted\":{interrupted}}}"))
                        }
                    }
                } else if request.starts_with("GET /api/info") {
                    let pid = fs::read_to_string(&pid_file).unwrap_or_default();
                    (200, format!("{{\"pid\":{}}}", pid.trim()))
                } else if request.starts_with("POST /api/session ") {
                    if server_state.session_failure.load(Ordering::SeqCst) {
                        (500, "{}".to_owned())
                    } else {
                        let created = server_state
                            .sessions
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                            + 1;
                        let id = if created == 1 {
                            format!("ses_{tag}")
                        } else {
                            format!("ses_{tag}_{created}")
                        };
                        *server_state.session.lock().unwrap() = id.clone();
                        (200, format!("{{\"data\":{{\"id\":\"{id}\"}}}}"))
                    }
                } else {
                    (404, "{}".to_owned())
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let program = dir.join("opencode");
        // One stand-in plays both roles. `serve` records the server's view of
        // its environment and announces the fake server; any other invocation
        // is the TUI and records its own view.
        fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n--version) printf '%s' \"$OPENCODE_CONFIG_CONTENT\" > '{dir}/version_config'; printf '%s' \"$PWD\" > '{dir}/version_pwd'; sleep {version_delay}; echo {version}; exit 0;;\nserve) ;;\n*)\nprintf '%s' \"$*\" > '{dir}/tui_argv'\nprintf '%s' \"$OPENCODE_PASSWORD\" > '{dir}/tui_password'\nprintf '%s' \"$OPENCODE_CONFIG_CONTENT\" > '{dir}/tui_config'\nprintf '%s' \"$CROWDED_TEST_TOKEN\" > '{dir}/tui_token'\nIFS= read -r line\nprintf '%s' \"$line\" > '{dir}/tui_stdin'\nexec sleep 30;;\nesac\nprintf '%s' \"$$\" > '{dir}/pid'\nprintf '%s' \"$OPENCODE_PASSWORD\" > '{dir}/password'\nprintf '%s' \"$HOME\" > '{dir}/home'\nprintf '%s' \"$PWD\" > '{dir}/pwd'\nprintf '%s' \"$*\" > '{dir}/argv'\nprintf '%s' \"$OPENCODE_CONFIG_CONTENT\" > '{dir}/config'\nprintf '%s' \"$OPENCODE_CONFIG\" > '{dir}/config_path'\nprintf '%s' \"$CROWDED_TEST_TOKEN\" > '{dir}/token'\nsleep {delay}\nprintf 'server listening on http://{address}\\n'\nexec sleep 30\n",
                dir = dir.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        FakeRoom {
            program,
            dir,
            state,
            address,
        }
    }

    #[cfg(unix)]
    pub(crate) fn process_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[cfg(unix)]
    #[test]
    fn two_native_rooms_in_one_directory_get_distinct_servers_sessions_and_generations() {
        let root = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let a = fake_room(root.path(), "a", "v2.2.5");
        let b = fake_room(root.path(), "b", "v2.2.5");
        let room_a = start(NativeLaunch::direct(
            a.program.as_os_str(),
            shared.path(),
            "1".into(),
            1,
            &[],
        ))
        .unwrap();
        let room_b = start(NativeLaunch::direct(
            b.program.as_os_str(),
            shared.path(),
            "2".into(),
            2,
            &[],
        ))
        .unwrap();

        assert_ne!(room_a.binding().session, room_b.binding().session);
        assert_ne!(room_a.binding().endpoint, room_b.binding().endpoint);
        assert_eq!(room_a.binding().generation, 1);
        assert_eq!(room_b.binding().generation, 2);
        assert_eq!(room_a.binding().room, "1");
        assert_eq!(room_b.binding().room, "2");
        let pid_a = fs::read_to_string(a.dir.join("pid")).unwrap();
        let pid_b = fs::read_to_string(b.dir.join("pid")).unwrap();
        assert_ne!(pid_a, pid_b);

        // Each server accepts only its own binding.
        let message = |room: &NativeRoom, other: &NativeRoom| {
            let own = room.server().queue_async(
                room.binding(),
                format!("msg_{}", "a".repeat(64)),
                "hello".into(),
            );
            let crossed = room.server().queue_async(
                other.binding(),
                format!("msg_{}", "b".repeat(64)),
                "hello".into(),
            );
            (
                own.recv_timeout(Duration::from_secs(5)).unwrap(),
                crossed.recv().unwrap(),
            )
        };
        // A claimed server reaches its own session (the stand-in answers 404,
        // which is a rejection, not a refusal to send) and refuses the other
        // room's binding without sending anything.
        let (own, crossed) = message(&room_a, &room_b);
        assert_ne!(own, crate::opencode_native::Delivery::NotSent);
        assert_eq!(crossed, crate::opencode_native::Delivery::NotSent);
        let (own, crossed) = message(&room_b, &room_a);
        assert_ne!(own, crate::opencode_native::Delivery::NotSent);
        assert_eq!(crossed, crate::opencode_native::Delivery::NotSent);

        drop(room_a);
        drop(room_b);
        wait_for_reaps();
        wait_until_stopped(&pid_a);
        wait_until_stopped(&pid_b);
    }

    #[cfg(unix)]
    #[test]
    fn clear_session_moves_the_room_to_a_new_session_on_the_same_server() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room(root.path(), "clear", "v2.2.5");
        let mut room = start(NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1".into(),
            1,
            &[],
        ))
        .unwrap();
        let launch = room.binding().clone();
        let fresh = room.clear_session(9).unwrap();

        assert_ne!(fresh.session, launch.session);
        assert_eq!(fresh.room, launch.room);
        assert_eq!(fresh.endpoint, launch.endpoint);
        assert_eq!(fresh.generation, 9);
        assert_eq!(room.binding(), &fresh);
        let args: Vec<String> = room
            .tui_arguments()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&fresh.session), "{args:?}");
        assert!(!args.contains(&launch.session), "{args:?}");

        // The server keeps its endpoint and accepts only the new binding.
        let refused = room
            .server()
            .queue_async(&launch, format!("msg_{}", "c".repeat(64)), "hello".into())
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_eq!(refused, crate::opencode_native::Delivery::NotSent);
        let accepted = room
            .server()
            .queue_async(&fresh, format!("msg_{}", "d".repeat(64)), "hello".into())
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert_ne!(accepted, crate::opencode_native::Delivery::NotSent);
    }

    #[cfg(unix)]
    #[test]
    fn tui_gets_the_server_password_through_its_handoff_and_never_through_arguments() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room(root.path(), "a", "v2.2.5");
        let room = start(NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1".into(),
            1,
            &[],
        ))
        .unwrap();

        let password = fs::read_to_string(fake.dir.join("password")).unwrap();
        assert!(!password.is_empty());
        assert_eq!(room.tui_password(), password);

        let arguments: Vec<String> = room
            .tui_arguments()
            .into_iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(arguments[0], "--server");
        assert!(arguments[1].starts_with("http://127.0.0.1:"));
        assert_eq!(arguments[2], "--session");
        assert_eq!(arguments[3], room.binding().session);
        assert!(
            arguments
                .iter()
                .all(|argument| !argument.contains(&password))
        );

        // The server was launched without the secret on its command line.
        let argv = fs::read_to_string(fake.dir.join("argv")).unwrap();
        assert_eq!(argv, "serve --hostname 127.0.0.1 --port 0");
        assert!(!argv.contains(&password));
        // The child keeps the user's environment, so their OpenCode login works.
        assert_eq!(
            fs::read_to_string(fake.dir.join("home")).unwrap(),
            std::env::var("HOME").unwrap()
        );
    }

    /// Environment that keeps a real OpenCode inside a temporary directory, so a
    /// test never reads or writes the user's login, configuration or data.
    pub(crate) fn isolated_variables(root: &Path) -> Variables {
        let configuration = root.join("opencode.json");
        fs::write(&configuration, "{}").unwrap();
        let mut variables: Variables = Vec::new();
        for (key, directory) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let path = root.join(directory);
            fs::create_dir_all(&path).unwrap();
            variables.push((key.into(), path.into_os_string()));
        }
        variables.push(("OPENCODE_CONFIG".into(), configuration.into_os_string()));
        variables.push(("OPENCODE_CONFIG_CONTENT".into(), "{}".into()));
        variables
    }

    /// Runs the real OpenCode in an isolated environment; no model call is made.
    #[test]
    #[ignore]
    fn isolated_real_launch_binds_and_reaps() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let variables = isolated_variables(root.path());
        let room = start(NativeLaunch::direct(
            OsStr::new("opencode"),
            cwd.path(),
            "1".into(),
            1,
            &variables,
        ))
        .unwrap();
        assert_eq!(room.binding().generation, 1);
        assert_eq!(room.tui_arguments().len(), 4);
        let server = room.server();
        drop(room);
        drop(server);
        wait_for_reaps();
    }

    #[test]
    fn the_version_check_waits_long_enough_for_a_busy_machine() {
        assert!(VERSION_TIMEOUT >= Duration::from_secs(20));
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_version_check_is_stopped_at_its_bound() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // This stand-in takes 3 s to print its version.
        let fake = fake_room_with_delays(root.path(), "slow-version", "v2.2.5", 3, 0);
        let started = Instant::now();
        let error = check_version(
            fake.program.as_os_str(),
            cwd.path(),
            &[],
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(2));
        // With room to finish it passes.
        check_version(
            fake_room(root.path(), "fast", "v2.2.5").program.as_os_str(),
            cwd.path(),
            &[],
            VERSION_TIMEOUT,
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_old_or_unreadable_opencode_is_refused_before_any_server_starts() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        for (version, expected) in [
            ("v2.2.4", "OpenCode 2.2.5 or newer"),
            ("banana", "could not read the OpenCode version"),
        ] {
            let fake = fake_room(root.path(), version, version);
            let error = start(NativeLaunch::direct(
                fake.program.as_os_str(),
                cwd.path(),
                "1".into(),
                1,
                &[],
            ))
            .err()
            .expect("must refuse");
            assert!(error.to_string().contains(expected), "{error}");
            // `serve` never ran, so no pid file exists.
            assert!(!fake.dir.join("pid").exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_server_that_is_not_the_child_is_refused_and_cleaned_up() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room(root.path(), "a", "v2.2.5");
        // The fake reports the pid recorded here, so corrupt it after the
        // script writes it: replace the script's recording with a wrong value.
        let script = fs::read_to_string(&fake.program).unwrap();
        fs::write(
            &fake.program,
            script.replace("printf '%s' \"$$\"", "printf '%s' \"1\""),
        )
        .unwrap();
        let error = start(NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1".into(),
            1,
            &[],
        ))
        .err()
        .expect("must refuse");
        assert!(error.to_string().contains("identity"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn the_owned_server_gets_the_room_environment_the_terminal_gets() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = fake_room(root.path(), "a", "v2.2.5");
        let b = fake_room(root.path(), "b", "v2.2.5");
        let variables = |config: &str| -> Variables {
            vec![
                ("OPENCODE_CONFIG_CONTENT".into(), config.into()),
                (
                    "CROWDED_TEST_TOKEN".into(),
                    format!("token-{config}").into(),
                ),
                // A room variable can never replace the generated password.
                ("OPENCODE_PASSWORD".into(), "attacker".into()),
            ]
        };
        let room_a = start(NativeLaunch::direct(
            a.program.as_os_str(),
            cwd.path(),
            "1".into(),
            1,
            &variables("one"),
        ))
        .unwrap();
        let room_b = start(NativeLaunch::direct(
            b.program.as_os_str(),
            cwd.path(),
            "2".into(),
            2,
            &variables("two"),
        ))
        .unwrap();
        assert_eq!(fs::read_to_string(a.dir.join("config")).unwrap(), "one");
        assert_eq!(fs::read_to_string(b.dir.join("config")).unwrap(), "two");
        assert_eq!(
            fs::read_to_string(a.dir.join("token")).unwrap(),
            "token-one"
        );
        assert_eq!(
            fs::read_to_string(b.dir.join("token")).unwrap(),
            "token-two"
        );
        for (fake, room) in [(&a, &room_a), (&b, &room_b)] {
            let password = fs::read_to_string(fake.dir.join("password")).unwrap();
            assert_ne!(password, "attacker");
            assert_eq!(room.tui_password(), password);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_launch_that_times_out_kills_and_waits_for_its_child_before_exit_waits_finish() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        // The stand-in takes 2 s to announce its server.
        let fake = fake_room_with_delay(root.path(), "slow", "v2.2.5", 2);
        let launch = begin(NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1".into(),
            1,
            &[],
        ));
        // Once the stand-in is running but still silent, the caller gives up.
        let pid = wait_for_pid_file(&fake.dir.join("pid"));
        let error = launch.wait(Duration::from_millis(50)).err().unwrap();
        assert!(error.to_string().contains("did not start in time"));
        assert!(
            process_alive(&pid),
            "the child is running when the caller gives up"
        );
        wait_for_reaps();
        wait_until_stopped(&pid);
    }

    #[cfg(unix)]
    #[test]
    fn dropping_a_launch_cancels_it_and_reaps_its_child() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room_with_delay(root.path(), "slow", "v2.2.5", 1);
        let launch = begin(NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1".into(),
            1,
            &[],
        ));
        let pid = wait_for_pid_file(&fake.dir.join("pid"));
        drop(launch);
        wait_for_reaps();
        wait_until_stopped(&pid);
    }

    /// A stand-in `headroom wrap`: runs the wrapped program as its own child,
    /// like the real wrapper, and either forwards SIGTERM or ignores it.
    #[cfg(unix)]
    fn wrapper_binary(root: &Path, name: &str, forward_term: bool) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let program = root.join(name);
        let signal = if forward_term {
            "trap 'kill -TERM $child 2>/dev/null; wait $child; exit 0' TERM"
        } else {
            "trap '' TERM"
        };
        fs::write(
            &program,
            format!(
                "#!/bin/sh\n[ \"$1\" = \"wrap\" ] && shift\nprog=\"$1\"; shift\n\"$prog\" \"$@\" &\nchild=$!\n{signal}\nwait $child\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        program
    }

    #[cfg(unix)]
    fn wrapped_launch(fake: &FakeRoom, wrapper: &Path, cwd: &Path) -> NativeLaunch {
        NativeLaunch {
            guest: fake.program.clone().into_os_string(),
            program: wrapper.as_os_str().to_os_string(),
            prefix: vec![
                OsString::from("wrap"),
                fake.program.clone().into_os_string(),
            ],
            cwd: cwd.to_path_buf(),
            room: "1".into(),
            generation: 1,
            variables: Vec::new(),
            config: None,
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_wrapped_server_is_owned_when_it_is_the_wrappers_direct_child() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room(root.path(), "wrapped", "v2.2.5");
        let wrapper = wrapper_binary(root.path(), "headroom", true);

        let room = start(wrapped_launch(&fake, &wrapper, cwd.path())).unwrap();
        assert_eq!(room.binding().session, "ses_wrapped");
        let server = fs::read_to_string(fake.dir.join("pid")).unwrap();

        drop(room);
        wait_for_reaps();
        wait_until_stopped(&server);
    }

    #[cfg(unix)]
    #[test]
    fn a_wrapper_that_never_forwards_sigterm_still_has_its_server_stopped() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room(root.path(), "stubborn", "v2.2.5");
        let wrapper = wrapper_binary(root.path(), "headroom", false);

        let room = start(wrapped_launch(&fake, &wrapper, cwd.path())).unwrap();
        let server = fs::read_to_string(fake.dir.join("pid")).unwrap();

        drop(room);
        wait_for_reaps();
        wait_until_stopped(&server);
    }

    #[cfg(unix)]
    fn wait_until_stopped(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while process_alive(pid) {
            assert!(
                Instant::now() < deadline,
                "the process {pid} is still alive"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    fn wait_for_pid_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = fs::read_to_string(path) {
                if !text.is_empty() {
                    return text;
                }
            }
            assert!(Instant::now() < deadline, "pid file never appeared");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn reaper_is_started_ahead_of_time_and_survives_repeated_use() {
        ensure_reaper().unwrap();
        ensure_reaper().unwrap();
        let finished = Arc::new(AtomicBool::new(false));
        drop(Reaped::new(SlowDrop(finished.clone())));
        wait_for_reaps();
        assert!(finished.load(Ordering::SeqCst));
    }

    #[cfg(unix)]
    #[test]
    fn the_version_check_and_the_server_run_in_the_room_directory_with_the_room_environment() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room(root.path(), "a", "v2.2.5");
        let variables: Variables = vec![("OPENCODE_CONFIG_CONTENT".into(), "room-config".into())];
        let _room = start(NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1.1".into(),
            1,
            &variables,
        ))
        .unwrap();
        let expected = cwd.path().to_str().unwrap();
        assert_eq!(
            fs::read_to_string(fake.dir.join("version_config")).unwrap(),
            "room-config"
        );
        assert_eq!(
            fs::read_to_string(fake.dir.join("config")).unwrap(),
            "room-config"
        );
        assert_eq!(
            fs::read_to_string(fake.dir.join("version_pwd")).unwrap(),
            expected
        );
        assert_eq!(fs::read_to_string(fake.dir.join("pwd")).unwrap(), expected);
    }

    #[cfg(unix)]
    #[test]
    fn a_launch_dropped_during_the_version_check_never_starts_a_server() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let fake = fake_room_with_delays(root.path(), "slow-version", "v2.2.5", 1, 0);
        let launch = begin(NativeLaunch::direct(
            fake.program.as_os_str(),
            cwd.path(),
            "1".into(),
            1,
            &[],
        ));
        drop(launch);
        wait_for_reaps();
        assert!(!fake.dir.join("pid").exists(), "serve must not have run");
    }
}
