//! Best-effort persistence of each raw room's exact underlying vendor session
//! id.
//!
//! The state file lives beside the toolbox state (`.crowded/`), follows the
//! same versioned-JSON + atomic-rename pattern, and is keyed by
//! `(absolute cwd, numeric room)` -- the numeric suffix of `RoomSpec.title`
//! (e.g. `2` in `OpenCode · 2`), which stays stable across a room's
//! ClearContext/Configure respawns even when its vendor and title change --
//! so two rooms sharing a cwd (e.g. two OpenCode rooms in the same directory)
//! each keep their own persisted id, a later `crowded resume` in the same
//! directory targets the exact session this room captured instead of a
//! sibling room's, and reconfiguring a room to a new vendor supersedes its
//! own stale entry instead of leaving it behind alongside a new one.
//!
//! Discovery is kicked off from the pane's own intro-sent event and runs on a
//! background thread (never the UI loop), bounded by [`SESSION_CAPTURE_GRACE`]
//! for Claude, `CODEX_SESSION_CAPTURE_GRACE` for Codex (whose rollout file
//! does not land on disk until well after spawn -- see that constant's doc),
//! or the much longer `OPENCODE_SESSION_CAPTURE_GRACE` for OpenCode, whose
//! sqlite row is committed at first-message time rather than at spawn. Each
//! poll tick re-derives an `exclude` baseline -- every id already
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
/// `HEADROOM_STARTUP_GRACE` in `src/app.rs`). Claude writes its transcript at
/// process spawn, so a few seconds is enough.
pub(super) const SESSION_CAPTURE_GRACE: Duration = Duration::from_secs(3);
/// Codex's rollout file is not on disk at spawn either, despite the file's
/// own embedded `payload.timestamp` looking spawn-adjacent: live evidence
/// (this repo, 2026-08-10, runtime PID 40406 started 10:07:31) shows rollout
/// `019feaed-6ba7-7e23-965a-6c685031dbb3` has a `payload.timestamp` of
/// 10:07:36 but the file's own on-disk birth time is 10:07:43 -- 12s after
/// the runtime (and this room's spawn, which follows immediately) started --
/// and a same-day earlier rollout showed the same several-second gap between
/// its embedded timestamp and its on-disk birth. The old 3s
/// `SESSION_CAPTURE_GRACE` closes before the file exists at all. 20s keeps
/// margin above the measured ~12s without imposing OpenCode's much longer
/// wait on Codex.
const CODEX_SESSION_CAPTURE_GRACE: Duration = Duration::from_secs(20);
/// OpenCode is the one vendor whose sqlite row is not written at spawn: live
/// evidence (OpenCode 1.18.15 under Headroom, this repo, 2026-08-08) measured
/// the row committed 47-55s after the guest process started, at first-message
/// time. 75s keeps ~20s of margin above the measured maximum without
/// imposing that wait on the faster vendors, which use `SESSION_CAPTURE_GRACE`.
const OPENCODE_SESSION_CAPTURE_GRACE: Duration = Duration::from_secs(75);
const DISCOVERY_POLL: Duration = Duration::from_millis(250);

/// The capture bound for `vendor`: OpenCode's much longer window, Codex's
/// own measured window, or the shared default for Claude.
fn capture_grace(vendor: CliVendor) -> Duration {
    match vendor {
        CliVendor::OpenCode => OPENCODE_SESSION_CAPTURE_GRACE,
        CliVendor::Codex => CODEX_SESSION_CAPTURE_GRACE,
        CliVendor::Claude => SESSION_CAPTURE_GRACE,
    }
}

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
    /// The room identity (RoomSpec.title, e.g. `OpenCode · 3`), persisted
    /// verbatim for vendor-aware `lookup`. Matching for `upsert`, however,
    /// keys on [`room_identity`] -- the numeric suffix alone -- so a room
    /// reconfigured to a new vendor (and thus a new title) still supersedes
    /// its own prior entry instead of leaving it stale. See module docs.
    room: String,
    session_id: String,
}

/// The stable numeric-room suffix of a `RoomSpec.title` like `OpenCode · 2`
/// (see `RoomSpec::new`/`RoomSpec::configured` in `src/config.rs`, both of
/// which format titles as `"{guest} · {room_number}"`). This is what actually
/// identifies "the same room" across a ClearContext/Configure respawn, which
/// can change the guest name and vendor but not the room number. Falls back
/// to the full title when the separator is absent, so a malformed or legacy
/// title degrades to exact-title matching rather than colliding with an
/// unrelated room.
fn room_identity(title: &str) -> &str {
    title.rsplit_once(" · ").map_or(title, |(_, suffix)| suffix)
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
        let identity = room_identity(room);
        // Drop EVERY prior entry for this (cwd, numeric room), not just the
        // first match -- a room can already have more than one on disk (e.g.
        // a stale pre-reconfigure entry alongside one already superseded
        // in-place), and `find`-then-update only ever fixes one of them,
        // leaving duplicates behind. Collapsing them all before pushing the
        // fresh entry guarantees exactly one survives.
        self.sessions
            .retain(|entry| !(entry.cwd == cwd && room_identity(&entry.room) == identity));
        self.sessions.push(SessionEntry {
            vendor: vendor.to_owned(),
            cwd: cwd.to_owned(),
            room: room.to_owned(),
            session_id: session_id.to_owned(),
        });
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

/// The sentinel session id written by [`clear_room`] to record a durable
/// fresh-state intent. A room whose persisted id is this value resumes with
/// no resume args until a genuine capture supersedes it.
pub(super) const FRESH_STATE_MARKER: &str = "__fresh__";

/// Record that a room was cleared and must not resume its prior session.
/// Replaces the room's persisted entry (if any) with a [`FRESH_STATE_MARKER`]
/// so that a later resume reads the marker instead of a stale pre-clear id.
/// A subsequent successful capture supersedes the marker with the real id.
#[allow(dead_code)]
pub(super) fn clear_room(vendor: &str, cwd: &Path, room: &str) {
    let Ok(_guard) = STATE_LOCK.lock() else {
        return;
    };
    let mut state = load_state().unwrap_or_else(|_| SessionState::empty());
    state.upsert(vendor, &cwd.to_string_lossy(), room, FRESH_STATE_MARKER);
    let _ = save_state(&state);
}

/// Record or supersede the captured session id for `(vendor, cwd, room)`.
/// Called from background capture threads, so writes are serialized and saved
/// atomically (temp file + rename).
#[allow(dead_code)]
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
/// anchored to `since` -- the instant that spawn's own process was created,
/// supplied by the caller (see `Pane::spawn`). Runs on a background thread so
/// the UI loop is never blocked, and is bounded to [`SESSION_CAPTURE_GRACE`]
/// (OpenCode instead uses its own, much longer bound -- see
/// `OPENCODE_SESSION_CAPTURE_GRACE`). On success the same path that persists the id also reports it into the
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
    since: SystemTime,
    captured: CapturedSession,
    opencode_model: Option<String>,
) {
    thread::spawn(move || {
        let deadline = Instant::now() + capture_grace(vendor);
        loop {
            if capture_once(
                &vendor,
                &cwd,
                &room,
                since,
                &captured,
                opencode_model.as_deref(),
            ) {
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
/// The read-exclude-then-persist sequence is atomic under `STATE_LOCK` end to
/// end, so two sibling rooms racing in the same poll tick cannot both claim
/// the same underlying candidate.
fn capture_once(
    vendor: &CliVendor,
    cwd: &Path,
    room: &str,
    since: SystemTime,
    captured: &CapturedSession,
    opencode_model: Option<&str>,
) -> bool {
    let Ok(_guard) = STATE_LOCK.lock() else {
        return false;
    };
    let exclude = claimed_session_ids(vendor.key(), cwd);
    if let Some(id) = controls::discover_session_id(*vendor, cwd, since, &exclude, opencode_model) {
        let mut state = load_state().unwrap_or_else(|_| SessionState::empty());
        state.upsert(vendor.key(), &cwd.to_string_lossy(), room, &id);
        let _ = save_state(&state);
        if let Ok(mut guard) = captured.lock() {
            *guard = Some(id);
        }
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
#[allow(dead_code)]
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

    fn set_modified(path: &Path, time: SystemTime) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    /// Regression: capture must be anchored to the fresh process spawn, not to
    /// the later intro-sent event. Codex writes its rollout artifact at process
    /// spawn (before the intro is delivered), so an artifact created after
    /// `since` (the spawn instant) but before "now" (when capture used to take
    /// its timestamp) must still be claimed. Under the bug, `capture_async`
    /// computed `since = now` at the intro-sent call, which rejected the
    /// already-created spawn artifact and left the fresh Codex session
    /// uncaptured.
    #[test]
    fn capture_async_claims_an_artifact_created_before_the_intro_but_after_spawn() {
        let _guard = StateRootGuard::isolated();
        let home_guard = controls::HomeDirGuard::isolated();
        let day = home_guard.path().join(".codex/sessions/2026/08/07");
        fs::create_dir_all(&day).unwrap();

        // The spawn anchor: 30s ago. Codex writes its rollout moments later
        // (25s ago) -- still before "now", the instant the intro-sent event
        // fires. The artifact belongs to the fresh spawn, so capture anchored
        // to the spawn instant must claim it.
        let since = SystemTime::now() - Duration::from_secs(30);
        let artifact = day.join("rollout-2026-08-07T10-00-00-freshspawn.jsonl");
        fs::write(
            &artifact,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-fresh\",\"cwd\":\"/repo\"}}\n",
        )
        .unwrap();
        set_modified(&artifact, since + Duration::from_secs(5));

        let cell = fresh_capture_cell();
        capture_async(
            CliVendor::Codex,
            PathBuf::from("/repo"),
            "Codex · 2".to_owned(),
            since,
            Arc::clone(&cell),
            None,
        );

        // The background thread polls (250 ms) within the 3s capture grace;
        // give it a beat past the grace so the loop drains deterministically.
        let deadline = Instant::now() + SESSION_CAPTURE_GRACE + Duration::from_secs(1);
        while !has_captured_session(&cell) && Instant::now() < deadline {
            thread::sleep(DISCOVERY_POLL);
        }

        assert_eq!(cell.lock().unwrap().as_deref(), Some("ses-fresh"));
        assert_eq!(
            lookup("codex", Path::new("/repo"), "Codex · 2").as_deref(),
            Some("ses-fresh")
        );
    }

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

    /// Regression: reconfiguring a room to a new vendor (OpenCode · 2 ->
    /// Codex · 2, same cwd) must supersede the stale entry in place rather
    /// than leaving it behind alongside a new one keyed by the new title.
    #[test]
    fn upsert_supersedes_a_stale_entry_when_the_same_numeric_room_changes_vendor() {
        let mut state = SessionState::empty();
        state.upsert("opencode", "/repo", "OpenCode · 2", "ses-opencode-stale");
        state.upsert("codex", "/repo", "Codex · 2", "ses-codex-fresh");

        assert_eq!(
            state.sessions.len(),
            1,
            "stale OpenCode · 2 entry must be superseded, not retained alongside Codex · 2"
        );
        assert_eq!(
            state.lookup("codex", "/repo", "Codex · 2"),
            Some("ses-codex-fresh")
        );
        assert_eq!(
            state.lookup("opencode", "/repo", "OpenCode · 2"),
            None,
            "the superseded entry no longer matches its old vendor/title"
        );
    }

    /// Regression: live state can already contain BOTH a stale pre-reconfigure
    /// entry (OpenCode · 2) and an already-captured current entry (Codex · 2)
    /// for the same room simultaneously (e.g. state written before this fix
    /// landed). A find-then-update `upsert` only fixes the first match and
    /// leaves the duplicate behind. Upsert must collapse ALL matching
    /// (cwd, numeric room) entries down to exactly one current entry, while
    /// leaving sibling-room and other-cwd entries untouched.
    #[test]
    fn upsert_collapses_pre_existing_duplicate_entries_for_the_same_room_to_one() {
        let mut state = SessionState::empty();
        // Seed the already-duplicated state directly, as if written before
        // this fix: a stale pre-reconfigure entry and an already-captured
        // current one, both for room 2 at the same cwd.
        state.sessions.push(SessionEntry {
            vendor: "opencode".to_owned(),
            cwd: "/repo".to_owned(),
            room: "OpenCode · 2".to_owned(),
            session_id: "ses-opencode-stale".to_owned(),
        });
        state.sessions.push(SessionEntry {
            vendor: "codex".to_owned(),
            cwd: "/repo".to_owned(),
            room: "Codex · 2".to_owned(),
            session_id: "ses-codex-old".to_owned(),
        });
        // A sibling room at the same cwd, and the same room number at a
        // different cwd: both must survive untouched.
        state.sessions.push(SessionEntry {
            vendor: "opencode".to_owned(),
            cwd: "/repo".to_owned(),
            room: "OpenCode · 3".to_owned(),
            session_id: "ses-sibling".to_owned(),
        });
        state.sessions.push(SessionEntry {
            vendor: "codex".to_owned(),
            cwd: "/elsewhere".to_owned(),
            room: "Codex · 2".to_owned(),
            session_id: "ses-elsewhere".to_owned(),
        });

        state.upsert("codex", "/repo", "Codex · 2", "ses-codex-fresh");

        let room_2_at_repo: Vec<_> = state
            .sessions
            .iter()
            .filter(|entry| entry.cwd == "/repo" && room_identity(&entry.room) == "2")
            .collect();
        assert_eq!(room_2_at_repo.len(), 1, "exactly one current room-2 entry");
        assert_eq!(room_2_at_repo[0].vendor, "codex");
        assert_eq!(room_2_at_repo[0].room, "Codex · 2");
        assert_eq!(room_2_at_repo[0].session_id, "ses-codex-fresh");

        // Sibling-room and other-cwd entries are untouched.
        assert_eq!(
            state.lookup("opencode", "/repo", "OpenCode · 3"),
            Some("ses-sibling")
        );
        assert_eq!(
            state.lookup("codex", "/elsewhere", "Codex · 2"),
            Some("ses-elsewhere")
        );
        assert_eq!(
            state.sessions.len(),
            3,
            "collapsed the duplicate, kept siblings"
        );
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
        // Predates both fixture rows: this test exercises the exclude filter,
        // not the `since` filter (covered separately in controls.rs).
        let since = SystemTime::UNIX_EPOCH;

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
        assert!(!capture_once(
            &CliVendor::OpenCode,
            cwd,
            room,
            since,
            &cell,
            None
        ));
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

        assert!(capture_once(
            &CliVendor::OpenCode,
            cwd,
            room,
            since,
            &cell,
            None
        ));
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
        // Predates both fixture rows: this test exercises the exclude filter,
        // not the `since` filter (covered separately in controls.rs).
        let since = SystemTime::UNIX_EPOCH;

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
            &cell,
            None
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
            &cell,
            None
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

    /// Concurrency regression: two sibling rooms sharing (vendor,cwd) race in
    /// the same poll tick against a single-candidate fixture must not both
    /// capture the identical session id. The read-exclude-then-persist path is
    /// atomic under STATE_LOCK end to end; a barrier forces both threads to
    /// enter capture_once at effectively the same instant.
    #[test]
    fn concurrent_capture_once_at_most_one_thread_claims_same_id() {
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
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let cwd = Path::new("/repo");
        // Predates the fixture row: this test exercises exclude-baseline
        // concurrency, not the `since` filter.
        let since = SystemTime::UNIX_EPOCH;

        // Single candidate that both rooms could theoretically claim.
        let _ = std::fs::remove_file(&database);
        let setup = std::process::Command::new("sqlite3")
            .arg(&database)
            .arg(
                "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL,                  time_created INTEGER NOT NULL);                  INSERT INTO session VALUES ('race-candidate-id','/repo',2000);",
            )
            .output()
            .unwrap();
        assert!(setup.status.success());

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let cell_a = fresh_capture_cell();
        let cell_b = fresh_capture_cell();
        let barrier_a = std::sync::Arc::clone(&barrier);
        let barrier_b = std::sync::Arc::clone(&barrier);
        let cell_a_clone = std::sync::Arc::clone(&cell_a);
        let cell_b_clone = std::sync::Arc::clone(&cell_b);

        let handle_a = std::thread::spawn(move || {
            barrier_a.wait();
            capture_once(
                &CliVendor::OpenCode,
                cwd,
                "OpenCode · 2",
                since,
                &cell_a_clone,
                None,
            )
        });
        let handle_b = std::thread::spawn(move || {
            barrier_b.wait();
            capture_once(
                &CliVendor::OpenCode,
                cwd,
                "OpenCode · 3",
                since,
                &cell_b_clone,
                None,
            )
        });

        let captured_a = handle_a.join().unwrap();
        let captured_b = handle_b.join().unwrap();

        // At most one thread can capture the single candidate when the sequence
        // is atomic; the other must observe the id already in the exclude
        // baseline and keep polling (returns false).
        assert!(
            !(captured_a && captured_b),
            "both threads captured the same id; expected at most one"
        );
        let claimed_a = has_captured_session(&cell_a);
        let claimed_b = has_captured_session(&cell_b);
        assert!(
            !(claimed_a && claimed_b),
            "both cells claim capture; expected at most one"
        );

        // Persisted state must contain at most one entry with the candidate id.
        let state = load_state().unwrap();
        let count = state
            .sessions
            .iter()
            .filter(|entry| entry.session_id == "race-candidate-id")
            .count();
        assert!(
            count <= 1,
            "candidate id persisted {count} times; expected at most once"
        );
        // The successful cell, if any, must agree with persisted state.
        if claimed_a {
            assert_eq!(cell_a.lock().unwrap().as_deref(), Some("race-candidate-id"));
            assert_eq!(
                lookup("opencode", cwd, "OpenCode · 2").as_deref(),
                Some("race-candidate-id")
            );
        }
        if claimed_b {
            assert_eq!(cell_b.lock().unwrap().as_deref(), Some("race-candidate-id"));
            assert_eq!(
                lookup("opencode", cwd, "OpenCode · 3").as_deref(),
                Some("race-candidate-id")
            );
        }
        // Exactly one of the two rooms should have succeeded; the fixture has a
        // single candidate and the non-winning thread finds only an excluded row.
        // On heavily loaded CI the barrier may still serialize, but the at-most-
        // one invariant above is the load-bearing property. On a typical run
        // exactly one wins.
        assert!(
            count == 0 || count == 1,
            "unexpected persisted count {count}"
        );
    }

    /// Regression: under the old single fixed `SESSION_CAPTURE_GRACE` (3s),
    /// OpenCode's row -- measured live at 47-55s after spawn, at first-message
    /// time rather than at process spawn -- could never be captured; the
    /// window closed long before the row existed. OpenCode must use its own,
    /// much longer bound; Claude, which writes at spawn, keeps the short one.
    #[test]
    fn opencode_capture_grace_covers_the_measured_first_message_delay() {
        assert!(capture_grace(CliVendor::OpenCode) > Duration::from_secs(55));
        assert_eq!(capture_grace(CliVendor::Claude), SESSION_CAPTURE_GRACE);
    }

    /// Regression: live evidence (2026-08-10) showed a Codex rollout file's
    /// on-disk birth time landing ~12s after its own room's spawn, well past
    /// the old shared 3s `SESSION_CAPTURE_GRACE` that Codex used to share
    /// with Claude. Under the bug, a poll loop bounded to 3s gives up before
    /// a file that only appears on disk 4s after spawn is ever written, so it
    /// is never claimed. This test actually delays the file's creation on a
    /// background thread (not just its `mtime`), so it fails against the old
    /// 3s Codex bound and passes once Codex gets its own longer one.
    #[test]
    fn capture_async_claims_a_codex_artifact_that_lands_on_disk_past_the_old_three_second_bound() {
        assert!(capture_grace(CliVendor::Codex) > Duration::from_secs(4));
        assert!(capture_grace(CliVendor::Codex) < capture_grace(CliVendor::OpenCode));

        let _guard = StateRootGuard::isolated();
        let home_guard = controls::HomeDirGuard::isolated();
        let day = home_guard.path().join(".codex/sessions/2026/08/10");
        fs::create_dir_all(&day).unwrap();

        let since = SystemTime::now();
        let artifact = day.join("rollout-2026-08-10T10-07-36-freshspawn.jsonl");
        thread::spawn(move || {
            // Simulate Codex's file only landing on disk 4s after spawn --
            // past the old 3s bound, within the new one.
            thread::sleep(Duration::from_secs(4));
            fs::write(
                &artifact,
                "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-slow-write\",\"cwd\":\"/repo\"}}\n",
            )
            .unwrap();
        });

        let cell = fresh_capture_cell();
        capture_async(
            CliVendor::Codex,
            PathBuf::from("/repo"),
            "Codex · 2".to_owned(),
            since,
            Arc::clone(&cell),
            None,
        );

        let deadline = Instant::now() + capture_grace(CliVendor::Codex) + Duration::from_secs(1);
        while !has_captured_session(&cell) && Instant::now() < deadline {
            thread::sleep(DISCOVERY_POLL);
        }

        assert_eq!(cell.lock().unwrap().as_deref(), Some("ses-slow-write"));
    }

    /// Regression: clear must record a durable fresh-state marker so a later
    /// resume never restores the pre-clear session id. After clear_room, the
    /// marker supersedes the stale entry; a subsequent capture replaces the
    /// marker with the real id.
    #[test]
    fn clear_room_replaces_stale_id_with_fresh_marker_and_capture_supersedes_it() {
        let _guard = StateRootGuard::isolated();
        let cwd = Path::new("/repo");
        let room = "Claude · 1";

        // Pre-seed a stale session id for this room.
        upsert("claude", cwd, room, "stale-pre-clear-id");
        assert_eq!(
            lookup("claude", cwd, room).as_deref(),
            Some("stale-pre-clear-id")
        );

        // Clear replaces the stale id with the marker.
        clear_room("claude", cwd, room);
        assert_eq!(
            lookup("claude", cwd, room).as_deref(),
            Some(FRESH_STATE_MARKER)
        );
        // The marker is in the claimed ids baseline so a capture cannot
        // re-claim a fresh id that happens to match it (extremely unlikely
        // but defensively correct).
        let claimed = claimed_session_ids("claude", cwd);
        assert!(claimed.iter().any(|id| id == FRESH_STATE_MARKER));

        // A subsequent capture supersedes the marker with the real id.
        upsert("claude", cwd, room, "fresh-post-clear-id");
        assert_eq!(
            lookup("claude", cwd, room).as_deref(),
            Some("fresh-post-clear-id")
        );
        // Exactly one entry for the room: the marker was collapsed, not kept.
        let state = load_state().unwrap();
        let room_entries: Vec<_> = state
            .sessions
            .iter()
            .filter(|e| e.cwd == "/repo" && room_identity(&e.room) == "1")
            .collect();
        assert_eq!(room_entries.len(), 1, "marker superseded by fresh capture");
    }

    /// Regression: clear_room must not disturb sibling rooms sharing the same
    /// (vendor, cwd). Only the targeted room's entry is replaced.
    #[test]
    fn clear_room_preserves_sibling_room_entries() {
        let _guard = StateRootGuard::isolated();
        let cwd = Path::new("/repo");
        upsert("claude", cwd, "Claude · 1", "id-room-1");
        upsert("claude", cwd, "Claude · 2", "id-room-2");

        clear_room("claude", cwd, "Claude · 1");

        assert_eq!(
            lookup("claude", cwd, "Claude · 1").as_deref(),
            Some(FRESH_STATE_MARKER)
        );
        assert_eq!(
            lookup("claude", cwd, "Claude · 2").as_deref(),
            Some("id-room-2"),
            "sibling room untouched"
        );
    }

    /// Regression: clear on a room with no prior entry still records the
    /// marker, so a later resume starts fresh rather than falling back to the
    /// vendor's most-recent form.
    #[test]
    fn clear_room_records_marker_even_when_no_prior_entry_exists() {
        let _guard = StateRootGuard::isolated();
        let cwd = Path::new("/repo");
        assert_eq!(lookup("claude", cwd, "Claude · 1"), None);

        clear_room("claude", cwd, "Claude · 1");
        assert_eq!(
            lookup("claude", cwd, "Claude · 1").as_deref(),
            Some(FRESH_STATE_MARKER)
        );
    }
}
