//! Best-effort persistence of each raw room's exact underlying vendor session
//! id.
//!
//! The state file lives beside the toolbox state (`.crowded/`), follows the
//! same versioned-JSON + atomic-rename pattern, and is keyed by
//! `(vendor, absolute cwd, room identity)` so two rooms sharing a vendor+cwd
//! (e.g. two OpenCode rooms in the same directory) each keep their own
//! persisted id, and a later `crowded resume` in the same directory targets
//! the exact session this room captured instead of a sibling room's.
//!
//! Discovery is kicked off from the pane's own intro-sent event and runs on a
//! background thread (never the UI loop), bounded by [`SESSION_CAPTURE_GRACE`].
//! Each poll tick re-derives an `exclude` baseline -- every id already
//! persisted for this `(vendor, cwd)` across all room identities -- and refuses
//! to (re)claim one, so a fresh capture can never steal a sibling room's id or
//! re-capture its own stale pre-clear id. Nothing found within the bound
//! simply persists nothing -- resume falls back to the vendor's most-recent
//! form.

use std::{
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
    process,
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};

use super::controls::{self, CliVendor};

const SESSION_STATE_DIRECTORY: &str = ".crowded";
const SESSION_STATE_FILE: &str = "session-state.json";
// v2 added the per-room `room` key (RoomSpec.title) to SessionEntry; a v1
// state file written without it is rejected as an unsupported version and
// resume degrades to the ambiguous most-recent form until recaptured.
const SESSION_STATE_VERSION: u8 = 2;

/// How long a fresh spawn's capture waits for the guest to write its session
/// artifact after the intro is delivered (the same grace-window idea as
/// `HEADROOM_STARTUP_GRACE` in `src/app.rs`).
pub(super) const SESSION_CAPTURE_GRACE: Duration = Duration::from_secs(3);
const DISCOVERY_POLL: Duration = Duration::from_millis(250);

/// Serializes concurrent background capture threads writing the shared file.
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// Test-only override for the state root directory. Production reads
/// `env::current_dir()`. `OnceLock` + `RwLock` because the value may be set
/// and unset around individual tests.
static STATE_ROOT: OnceLock<RwLock<Option<PathBuf>>> = OnceLock::new();

#[derive(Deserialize, Serialize)]
struct SessionEntry {
    vendor: String,
    cwd: String,
    /// The room identity (RoomSpec.title, e.g. `OpenCode · 3`): unique per
    /// room, stable across ClearContext/Configure/restart respawns (which clone
    /// the spec), and disambiguates rooms sharing a `(vendor, cwd)`.
    room: String,
    session_id: String,
}

#[derive(Deserialize, Serialize)]
struct SessionState {
    version: u8,
    sessions: Vec<SessionEntry>,
}

impl SessionState {
    fn empty() -> Self {
        Self {
            version: SESSION_STATE_VERSION,
            sessions: Vec::new(),
        }
    }

    fn upsert(&mut self, vendor: &str, cwd: &str, room: &str, session_id: &str) {
        match self
            .sessions
            .iter_mut()
            .find(|entry| entry.vendor == vendor && entry.cwd == cwd && entry.room == room)
        {
            Some(entry) => entry.session_id = session_id.to_owned(),
            None => self.sessions.push(SessionEntry {
                vendor: vendor.to_owned(),
                cwd: cwd.to_owned(),
                room: room.to_owned(),
                session_id: session_id.to_owned(),
            }),
        }
    }

    fn lookup(&self, vendor: &str, cwd: &str, room: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|entry| entry.vendor == vendor && entry.cwd == cwd && entry.room == room)
            .map(|entry| entry.session_id.as_str())
    }

    /// Every session id currently persisted for this `(vendor, cwd)`, across
    /// ALL room identities. Used as the discovery `exclude` baseline so a
    /// fresh capture never (re)claims an id another room (or this room's own
    /// prior spawn) already owns.
    fn claimed_session_ids(&self, vendor: &str, cwd: &str) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|entry| entry.vendor == vendor && entry.cwd == cwd)
            .map(|entry| entry.session_id.clone())
            .collect()
    }
}

/// Look up the exact session id captured for `(vendor, cwd, room)`. Best-effort:
/// a missing file, a parse/version problem, or a missing entry all mean
/// `None`, and `crowded resume` falls back to today's most-recent form.
pub(super) fn lookup(vendor: &str, cwd: &Path, room: &str) -> Option<String> {
    let state = load_state().ok()?;
    state
        .lookup(vendor, &cwd.to_string_lossy(), room)
        .map(str::to_owned)
}

/// Record or supersede the captured session id for `(vendor, cwd, room)`.
/// Called from background capture threads, so writes are serialized and saved
/// atomically (temp file + rename).
pub(super) fn upsert(vendor: &str, cwd: &Path, room: &str, session_id: &str) {
    let Ok(_guard) = STATE_LOCK.lock() else {
        return;
    };
    let mut state = load_state().unwrap_or_else(|_| SessionState::empty());
    state.upsert(vendor, &cwd.to_string_lossy(), room, session_id);
    let _ = save_state(&state);
}

/// The exclude baseline for a fresh capture of `(vendor, cwd)`: every session
/// id already persisted across all room identities, re-read fresh from the
/// state file on each poll tick (not cached at loop start).
pub(super) fn claimed_session_ids(vendor: &str, cwd: &Path) -> Vec<String> {
    load_state()
        .map(|state| state.claimed_session_ids(vendor, &cwd.to_string_lossy()))
        .unwrap_or_default()
}

/// Kick off best-effort discovery of one fresh spawn's exact session id,
/// keyed off that spawn's own delivered intro. Runs on a background thread so
/// the UI loop is never blocked, and is bounded to [`SESSION_CAPTURE_GRACE`].
/// On success the same path that persists the id also reports it into the
/// shared `captured` cell for the Room Pulse tag.
///
/// `room` is this room's identity (`RoomSpec.title`): it both keys the
/// persisted entry (so a room only ever resumes its own id) and scopes nothing
/// about discovery itself -- the `exclude` baseline below is per
/// `(vendor, cwd)`, across ALL rooms, so a sibling's already-claimed id is
/// never re-discovered.
pub(super) fn capture_async(
    vendor: CliVendor,
    cwd: PathBuf,
    room: String,
    captured: CapturedSession,
) {
    let since = SystemTime::now();
    thread::spawn(move || {
        let deadline = Instant::now() + SESSION_CAPTURE_GRACE;
        loop {
            if capture_once(&vendor, &cwd, &room, since, &captured) {
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(DISCOVERY_POLL);
        }
    });
}

/// One poll tick: re-read the exclude baseline fresh (so a sibling that
/// finished capturing during the previous tick is skipped now), then discover.
/// Returns `true` when a genuinely-new id was captured and persisted.
///
/// ponytail: the exclude baseline closes every race whose artifact appears in
/// a DIFFERENT poll tick from our own. The one remaining window is two sibling
/// rooms whose underlying vendor artifacts both materialize within the same
/// ~250 ms tick, before either has persisted a claim -- the newest one then
/// wins for both. That is a documented ceiling: do not add a cross-process
/// lock or a second confirmation poll to close it. An upgrade path would be a
/// per-vendor ownership column persisted with the claim, so a discovered row
/// could be verified against its OWNER rather than against a global baseline.
fn capture_once(
    vendor: &CliVendor,
    cwd: &Path,
    room: &str,
    since: SystemTime,
    captured: &CapturedSession,
) -> bool {
    let exclude = claimed_session_ids(vendor.key(), cwd);
    if let Some(id) = controls::discover_session_id(*vendor, cwd, since, &exclude) {
        record_capture(*vendor, cwd, room, &id, captured);
        return true;
    }
    false
}

/// Shared in-memory confirmation that a fresh spawn's exact session id was
/// captured. The background capture thread writes it on success; the render
/// loop reads it per frame with a plain in-memory read, never a file read.
pub(super) type CapturedSession = Arc<Mutex<Option<String>>>;

/// A fresh, unset capture cell for a new pane.
pub(super) fn fresh_capture_cell() -> CapturedSession {
    Arc::new(Mutex::new(None))
}

/// The single success path: persist to disk and report to the shared cell
/// together, so the pulse tag can never disagree with what resume will use.
fn record_capture(
    vendor: CliVendor,
    cwd: &Path,
    room: &str,
    session_id: &str,
    captured: &CapturedSession,
) {
    upsert(vendor.key(), cwd, room, session_id);
    if let Ok(mut guard) = captured.lock() {
        *guard = Some(session_id.to_owned());
    }
}

/// Whether the shared cell reports a successful capture. Plain in-memory.
pub(super) fn has_captured_session(captured: &CapturedSession) -> bool {
    captured
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

fn load_state() -> io::Result<SessionState> {
    let path = state_path();
    refuse_symlink(&path)?;
    let state: SessionState = serde_json::from_str(&fs::read_to_string(&path)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if state.version != SESSION_STATE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported session state version {}", state.version),
        ));
    }
    Ok(state)
}

fn save_state(state: &SessionState) -> io::Result<()> {
    let root = state_root();
    let directory = root.join(SESSION_STATE_DIRECTORY);
    create_private_directory(&directory)?;
    let state_path = directory.join(SESSION_STATE_FILE);
    refuse_symlink(&state_path)?;
    let temporary = directory.join(format!("session-state.{}.tmp", process::id()));
    let mut contents = serde_json::to_vec_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    contents.push(b'\n');

    let mut file = private_file(&temporary)?;
    file.write_all(&contents)?;
    file.sync_all()?;
    if let Err(error) = fs::rename(&temporary, &state_path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

fn state_root() -> PathBuf {
    let lock = STATE_ROOT.get_or_init(|| RwLock::new(None));
    if let Ok(Some(root)) = lock.read().map(|guard| guard.clone()) {
        return root;
    }
    env::current_dir().unwrap_or_default()
}

fn state_path() -> PathBuf {
    state_root()
        .join(SESSION_STATE_DIRECTORY)
        .join(SESSION_STATE_FILE)
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to manage symbolic link {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::PermissionsExt;

    refuse_symlink(path)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    refuse_symlink(path)?;
    fs::create_dir_all(path)
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Test-only override for where the state file lives, so tests can point at a
/// temp directory instead of the real `.crowded/` under the repo.
#[cfg(test)]
fn override_state_root(root: Option<PathBuf>) {
    let lock = STATE_ROOT.get_or_init(|| RwLock::new(None));
    let Ok(mut guard) = lock.write() else {
        return;
    };
    *guard = root;
}

/// Serializes tests that swap the shared state root. Distinct from
/// [`STATE_LOCK`] (which guards production `upsert` writes) so a test can hold
/// the guard and still call `upsert` without deadlocking.
#[cfg(test)]
static TEST_ROOT_LOCK: Mutex<()> = Mutex::new(());

/// Points the state root at a fresh temp directory while held.
#[cfg(test)]
pub(super) struct StateRootGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    directory: PathBuf,
}

#[cfg(test)]
impl StateRootGuard {
    pub(super) fn isolated() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let lock = TEST_ROOT_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Include the process id so a fresh test process never reuses a temp
        // directory a previous process seeded and left behind.
        let directory = env::temp_dir().join(format!(
            "crowded-session-state-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create temp state root");
        override_state_root(Some(directory.clone()));
        StateRootGuard {
            _lock: lock,
            directory,
        }
    }
}

#[cfg(test)]
impl Drop for StateRootGuard {
    fn drop(&mut self) {
        override_state_root(None);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_merges_by_vendor_cwd_and_room_and_lookup_roundtrips() {
        let mut state = SessionState::empty();
        state.upsert("claude", "/a", "Claude · 1", "id-a");
        state.upsert("opencode", "/b", "OpenCode · 3", "id-b");
        // Same (vendor, cwd, room) supersedes in place rather than appending.
        state.upsert("claude", "/a", "Claude · 1", "id-a2");

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.lookup("claude", "/a", "Claude · 1"), Some("id-a2"));
        assert_eq!(state.lookup("opencode", "/b", "OpenCode · 3"), Some("id-b"));
        assert_eq!(state.lookup("codex", "/a", "Codex · 2"), None);
        assert_eq!(state.lookup("claude", "/nope", "Claude · 1"), None);
        // Same room, wrong vendor, resolves to None.
        assert_eq!(state.lookup("opencode", "/a", "Claude · 1"), None);
    }

    #[test]
    fn two_rooms_sharing_vendor_and_cwd_do_not_collide() {
        let mut state = SessionState::empty();
        state.upsert("opencode", "/shared", "OpenCode · 2", "id-room-2");
        state.upsert("opencode", "/shared", "OpenCode · 3", "id-room-3");

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(
            state.lookup("opencode", "/shared", "OpenCode · 2"),
            Some("id-room-2")
        );
        assert_eq!(
            state.lookup("opencode", "/shared", "OpenCode · 3"),
            Some("id-room-3")
        );

        // The per-(vendor, cwd) exclude baseline contains BOTH ids, so neither
        // room's capture can re-claim the other's.
        let claimed = state.claimed_session_ids("opencode", "/shared");
        assert_eq!(claimed.len(), 2);
        assert!(claimed.iter().any(|id| id == "id-room-2"));
        assert!(claimed.iter().any(|id| id == "id-room-3"));

        // A different (vendor, cwd) yields an empty baseline.
        assert!(
            state
                .claimed_session_ids("opencode", "/elsewhere")
                .is_empty()
        );
    }

    #[test]
    fn save_and_load_roundtrip_through_a_temp_root() {
        let _guard = StateRootGuard::isolated();

        let mut state = SessionState::empty();
        state.upsert("codex", "/repo", "Codex · 2", "ses-codex");
        save_state(&state).unwrap();

        let loaded = load_state().unwrap();
        assert_eq!(loaded.version, SESSION_STATE_VERSION);
        assert_eq!(
            loaded.lookup("codex", "/repo", "Codex · 2"),
            Some("ses-codex")
        );

        let looked_up = lookup("codex", Path::new("/repo"), "Codex · 2");
        assert_eq!(looked_up.as_deref(), Some("ses-codex"));
        // A missing (vendor, cwd, room) still resolves to None.
        assert_eq!(lookup("claude", Path::new("/repo"), "Claude · 1"), None);
        // The same cwd under a different room resolves to None too.
        assert_eq!(lookup("codex", Path::new("/repo"), "Codex · 3"), None);
    }

    #[test]
    fn missing_or_mismatched_state_resolves_to_none() {
        let _guard = StateRootGuard::isolated();
        assert_eq!(lookup("claude", Path::new("/repo"), "Claude · 1"), None);
    }

    #[test]
    fn production_root_is_the_current_directory() {
        // No guard: the real root (repo dir) is active; a missing state file
        // simply resolves to None.
        assert_eq!(lookup("claude", Path::new("/whatever"), "Claude · 1"), None);
    }

    #[test]
    fn record_capture_sets_the_shared_cell_and_persists_together() {
        let _guard = StateRootGuard::isolated();
        let cell = fresh_capture_cell();
        assert!(!has_captured_session(&cell));

        record_capture(
            CliVendor::Claude,
            Path::new("/repo"),
            "Claude · 1",
            "ses-captured",
            &cell,
        );

        // The cell reports the capture, and the same success path also wrote
        // the persisted state resume will read.
        assert!(has_captured_session(&cell));
        assert_eq!(cell.lock().unwrap().as_deref(), Some("ses-captured"));
        assert_eq!(
            lookup("claude", Path::new("/repo"), "Claude · 1").as_deref(),
            Some("ses-captured")
        );
        // Another room sharing (vendor, cwd) must NOT see this room's id.
        assert_eq!(
            lookup("claude", Path::new("/repo"), "Claude · 2").as_deref(),
            None
        );
    }

    #[test]
    fn capture_cell_reads_back_without_touching_disk() {
        // No StateRootGuard: the accessor is a pure in-memory read of the cell,
        // independent of any state file.
        let cell = fresh_capture_cell();
        assert!(!has_captured_session(&cell));
        assert!(cell.lock().unwrap().is_none());

        {
            let mut guard = cell.lock().unwrap();
            *guard = Some("ses-mem".to_owned());
        }
        assert!(has_captured_session(&cell));
        assert_eq!(cell.lock().unwrap().as_deref(), Some("ses-mem"));
    }

    /// Regression test for the ClearContext/restart stale-recapture bug: a
    /// fresh capture whose only candidate id is already in the persisted
    /// `exclude` baseline must NOT record_capture and keeps polling (returns
    /// false) instead of re-capturing the stale pre-clear id. Once a genuinely
    /// new row appears, the next tick captures it.
    #[test]
    fn capture_once_skips_an_already_claimed_id_and_polls_for_a_fresh_one() {
        if std::process::Command::new("sqlite3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let _guard = StateRootGuard::isolated();
        let home_guard = controls::HomeDirGuard::isolated();
        let database = home_guard.path().join(".local/share/opencode/opencode.db");
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        let cell = fresh_capture_cell();
        let cwd = Path::new("/repo");
        let room = "OpenCode · 3";
        let since = SystemTime::now();

        // Bug-2 setup: the room's own STALE pre-clear id is already persisted,
        // and the fresh CLI process hasn't written its new row yet.
        upsert("opencode", cwd, room, "stale-pre-clear-id");

        let _ = fs::remove_file(&database);
        let setup = std::process::Command::new("sqlite3")
            .arg(&database)
            .arg(
                "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, \
                 time_created INTEGER NOT NULL); \
                 INSERT INTO session VALUES ('stale-pre-clear-id','/repo',1000);",
            )
            .output()
            .unwrap();
        assert!(setup.status.success());

        // The only candidate is the excluded stale id: the tick must not
        // capture it, and must keep polling (returns false).
        assert!(!capture_once(&CliVendor::OpenCode, cwd, room, since, &cell));
        assert!(!has_captured_session(&cell));
        assert_eq!(
            lookup("opencode", cwd, room).as_deref(),
            Some("stale-pre-clear-id")
        );

        // The fresh post-clear row appears moments later: the next tick
        // captures it and supersedes the stale entry in place.
        let advance = std::process::Command::new("sqlite3")
            .arg(&database)
            .arg("INSERT INTO session VALUES ('fresh-post-clear-id','/repo',2000);")
            .output()
            .unwrap();
        assert!(advance.status.success());

        assert!(capture_once(&CliVendor::OpenCode, cwd, room, since, &cell));
        assert_eq!(cell.lock().unwrap().as_deref(), Some("fresh-post-clear-id"));
        assert_eq!(
            lookup("opencode", cwd, room).as_deref(),
            Some("fresh-post-clear-id")
        );
        // Exactly one entry for the room: the stale id was superseded, not
        // appended.
        assert_eq!(load_state().unwrap().sessions.len(), 1, "room entry count");
    }

    /// The multi-room mirror of the stale-recapture regression: a sibling room
    /// that finished capturing first is in the exclude baseline, so this
    /// room's tick refuses to claim the sibling's id and waits for its own.
    #[test]
    fn capture_once_does_not_claim_a_sibling_rooms_id() {
        if std::process::Command::new("sqlite3")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let _guard = StateRootGuard::isolated();
        let home_guard = controls::HomeDirGuard::isolated();
        let database = home_guard.path().join(".local/share/opencode/opencode.db");
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        let cell = fresh_capture_cell();
        let cwd = Path::new("/repo");
        let since = SystemTime::now();

        // A sibling room already claimed its id for the same (vendor, cwd).
        upsert("opencode", cwd, "OpenCode · 2", "sibling-claimed-id");

        let _ = fs::remove_file(&database);
        let setup = std::process::Command::new("sqlite3")
            .arg(&database)
            .arg(
                "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, \
                 time_created INTEGER NOT NULL); \
                 INSERT INTO session VALUES ('sibling-claimed-id','/repo',1000);",
            )
            .output()
            .unwrap();
        assert!(setup.status.success());

        // Only the sibling's (excluded) row exists: no capture, keep polling.
        assert!(!capture_once(
            &CliVendor::OpenCode,
            cwd,
            "OpenCode · 3",
            since,
            &cell
        ));
        assert!(!has_captured_session(&cell));

        // This room's own row appears: captured under ITS identity, leaving the
        // sibling's entry untouched.
        let advance = std::process::Command::new("sqlite3")
            .arg(&database)
            .arg("INSERT INTO session VALUES ('room-3-own-id','/repo',2000);")
            .output()
            .unwrap();
        assert!(advance.status.success());

        assert!(capture_once(
            &CliVendor::OpenCode,
            cwd,
            "OpenCode · 3",
            since,
            &cell
        ));
        assert_eq!(
            lookup("opencode", cwd, "OpenCode · 3").as_deref(),
            Some("room-3-own-id")
        );
        assert_eq!(
            lookup("opencode", cwd, "OpenCode · 2").as_deref(),
            Some("sibling-claimed-id")
        );
    }
}
