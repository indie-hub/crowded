//! Best-effort persistence of each raw room's exact underlying vendor session
//! id.
//!
//! The state file lives beside the toolbox state (`.crowded/`), follows the
//! same versioned-JSON + atomic-rename pattern, and is keyed by
//! `(vendor, absolute cwd)` so a later `crowded resume` in the same directory
//! can target the exact session instead of the ambiguous "most recent" flag.
//!
//! Discovery is kicked off from the pane's own intro-sent event and runs on a
//! background thread (never the UI loop), bounded by [`SESSION_CAPTURE_GRACE`].
//! Nothing found within the bound simply persists nothing -- resume falls back
//! to the vendor's most-recent form.

use std::{
    env, fs, io,
    io::Write,
    path::{Path, PathBuf},
    process,
    sync::{Mutex, OnceLock, RwLock},
    thread,
    time::{Duration, Instant, SystemTime},
};

use serde::{Deserialize, Serialize};

use super::controls::{self, CliVendor};

const SESSION_STATE_DIRECTORY: &str = ".crowded";
const SESSION_STATE_FILE: &str = "session-state.json";
const SESSION_STATE_VERSION: u8 = 1;

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

    fn upsert(&mut self, vendor: &str, cwd: &str, session_id: &str) {
        match self
            .sessions
            .iter_mut()
            .find(|entry| entry.vendor == vendor && entry.cwd == cwd)
        {
            Some(entry) => entry.session_id = session_id.to_owned(),
            None => self.sessions.push(SessionEntry {
                vendor: vendor.to_owned(),
                cwd: cwd.to_owned(),
                session_id: session_id.to_owned(),
            }),
        }
    }

    fn lookup(&self, vendor: &str, cwd: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|entry| entry.vendor == vendor && entry.cwd == cwd)
            .map(|entry| entry.session_id.as_str())
    }
}

/// Look up the exact session id captured for `(vendor, cwd)`. Best-effort:
/// a missing file, a parse/version problem, or a missing entry all mean
/// `None`, and `crowded resume` falls back to today's most-recent form.
pub(super) fn lookup(vendor: &str, cwd: &Path) -> Option<String> {
    let state = load_state().ok()?;
    state
        .lookup(vendor, &cwd.to_string_lossy())
        .map(str::to_owned)
}

/// Record or supersede the captured session id for `(vendor, cwd)`. Called
/// from background capture threads, so writes are serialized and saved
/// atomically (temp file + rename).
pub(super) fn upsert(vendor: &str, cwd: &Path, session_id: &str) {
    let Ok(_guard) = STATE_LOCK.lock() else {
        return;
    };
    let mut state = load_state().unwrap_or_else(|_| SessionState::empty());
    state.upsert(vendor, &cwd.to_string_lossy(), session_id);
    let _ = save_state(&state);
}

/// Kick off best-effort discovery of one fresh spawn's exact session id,
/// keyed off that spawn's own delivered intro. Runs on a background thread so
/// the UI loop is never blocked, and is bounded to [`SESSION_CAPTURE_GRACE`].
pub(super) fn capture_async(vendor: CliVendor, cwd: PathBuf) {
    let since = SystemTime::now();
    thread::spawn(move || {
        let deadline = Instant::now() + SESSION_CAPTURE_GRACE;
        loop {
            if let Some(id) = controls::discover_session_id(vendor, &cwd, since) {
                upsert(vendor.key(), &cwd, &id);
                return;
            }
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(DISCOVERY_POLL);
        }
    });
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
}

#[cfg(test)]
impl StateRootGuard {
    pub(super) fn isolated() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let lock = TEST_ROOT_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let directory = env::temp_dir().join(format!(
            "crowded-session-state-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).expect("create temp state root");
        override_state_root(Some(directory));
        StateRootGuard { _lock: lock }
    }
}

#[cfg(test)]
impl Drop for StateRootGuard {
    fn drop(&mut self) {
        override_state_root(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_merges_by_vendor_and_cwd_and_lookup_roundtrips() {
        let mut state = SessionState::empty();
        state.upsert("claude", "/a", "id-a");
        state.upsert("opencode", "/b", "id-b");
        // Same (vendor, cwd) supersedes in place rather than appending.
        state.upsert("claude", "/a", "id-a2");

        assert_eq!(state.sessions.len(), 2);
        assert_eq!(state.lookup("claude", "/a"), Some("id-a2"));
        assert_eq!(state.lookup("opencode", "/b"), Some("id-b"));
        assert_eq!(state.lookup("codex", "/a"), None);
        assert_eq!(state.lookup("claude", "/nope"), None);
    }

    #[test]
    fn save_and_load_roundtrip_through_a_temp_root() {
        let _guard = StateRootGuard::isolated();

        let mut state = SessionState::empty();
        state.upsert("codex", "/repo", "ses-codex");
        save_state(&state).unwrap();

        let loaded = load_state().unwrap();
        assert_eq!(loaded.version, SESSION_STATE_VERSION);
        assert_eq!(loaded.lookup("codex", "/repo"), Some("ses-codex"));

        let looked_up = lookup("codex", Path::new("/repo"));
        assert_eq!(looked_up.as_deref(), Some("ses-codex"));
        // A missing (vendor, cwd) still resolves to None.
        assert_eq!(lookup("claude", Path::new("/repo")), None);
    }

    #[test]
    fn missing_or_mismatched_state_resolves_to_none() {
        let _guard = StateRootGuard::isolated();
        assert_eq!(lookup("claude", Path::new("/repo")), None);
    }

    #[test]
    fn production_root_is_the_current_directory() {
        // No guard: the real root (repo dir) is active; a missing state file
        // simply resolves to None.
        assert_eq!(lookup("claude", Path::new("/whatever")), None);
    }
}
