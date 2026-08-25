use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use serde::Deserialize;

use crate::{
    config::{RoomSpec, Transport},
    doorbell::{Effort, ModelCatalogue, RoomCapabilities, SupportedControl},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum CliVendor {
    Claude,
    Codex,
    OpenCode,
}

impl CliVendor {
    /// Stable key used in the session-state file (`.crowded/session-state.json`).
    pub(super) fn key(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }
}

pub(super) fn uses_bracketed_paste(spec: &RoomSpec) -> bool {
    matches!(cli_vendor(spec), Ok(CliVendor::Claude | CliVendor::Codex))
}

pub(super) fn clear_resume_args(spec: &mut RoomSpec) -> io::Result<()> {
    match cli_vendor(spec)? {
        CliVendor::Claude => {
            strip_flags(&mut spec.args, &["--continue", "-c", "--fork-session"]);
            strip_options(&mut spec.args, &["--resume", "-r", "--session-id"]);
        }
        CliVendor::Codex => {
            if let Some(resume) = spec.args.iter().position(|argument| {
                matches!(argument.to_string_lossy().as_ref(), "resume" | "fork")
            }) {
                spec.args.truncate(resume);
            }
        }
        CliVendor::OpenCode => {
            strip_flags(&mut spec.args, &["--continue", "-c", "--fork"]);
            strip_options(&mut spec.args, &["--session", "-s"]);
        }
    }
    Ok(())
}

/// Relaunch the guest with each vendor's "resume" flag. When an exact session
/// id was captured for this room's `(vendor, cwd)` after its own intro was
/// delivered, target that exact session; otherwise fall back to the ambiguous
/// "most recent" form. Claude and OpenCode take `--continue` / Codex takes a
/// trailing `resume --last` for the fallback; the exact-id forms are
/// `--resume <id>` / `resume <id>` / `--session <id>`. Every form appends at
/// the end of the args, matching the pairing convention
/// `clear_resume_args` already expects.
/// `refused` carries the rooms `repair_opencode_session_mappings` rejected and
/// could not re-home; pass an empty slice when resuming a single room, which has
/// no slate to be judged against. Returns whether resume args were actually
/// applied, which is not the same as succeeding: a refused room succeeds at
/// starting fresh.
pub(super) fn add_resume_args(spec: &mut RoomSpec, refused: &[RoomKey]) -> io::Result<bool> {
    let vendor = cli_vendor(spec)?;
    match vendor {
        CliVendor::Claude => {
            strip_flags(&mut spec.args, &["--continue", "-c", "--fork-session"]);
            strip_options(&mut spec.args, &["--resume", "-r", "--session-id"]);
        }
        CliVendor::Codex => {
            if let Some(resume) = spec.args.iter().position(|argument| {
                matches!(argument.to_string_lossy().as_ref(), "resume" | "fork")
            }) {
                spec.args.truncate(resume);
            }
        }
        CliVendor::OpenCode => {
            strip_flags(&mut spec.args, &["--continue", "-c", "--fork"]);
            strip_options(&mut spec.args, &["--session", "-s"]);
        }
    }
    let args = match captured_resume(vendor, spec, refused) {
        Resume::Session(id) => session_resume_args(vendor, &id),
        Resume::MostRecent => recent_resume_args(vendor),
        Resume::Fresh => Vec::new(),
    };
    let resumed = !args.is_empty();
    spec.args.extend(args);
    Ok(resumed)
}

/// Identifies a room the way its persisted session state does, by working
/// directory and title.
pub(crate) type RoomKey = (PathBuf, String);

/// How a room should relaunch. `Fresh` exists because "resume nothing" and
/// "resume whatever is newest" are different answers: once an exact id has been
/// rejected as belonging to another model, the newest conversation in the
/// directory is the very thing that must not be restored.
enum Resume {
    Session(String),
    MostRecent,
    Fresh,
}

/// The ambiguous "resume most recent conversation" args for each vendor.
fn recent_resume_args(vendor: CliVendor) -> Vec<OsString> {
    match vendor {
        CliVendor::Claude => vec!["--continue".into()],
        CliVendor::Codex => vec!["resume".into(), "--last".into()],
        CliVendor::OpenCode => vec!["--continue".into()],
    }
}

/// The exact-session args for each vendor, given a captured session id.
fn session_resume_args(vendor: CliVendor, session_id: &str) -> Vec<OsString> {
    match vendor {
        CliVendor::Claude => vec!["--resume".into(), session_id.into()],
        CliVendor::Codex => vec!["resume".into(), session_id.into()],
        CliVendor::OpenCode => vec!["--session".into(), session_id.into()],
    }
}

fn captured_resume(vendor: CliVendor, spec: &RoomSpec, refused: &[RoomKey]) -> Resume {
    let Ok(cwd) = super::working_directory(spec.cwd.as_deref()) else {
        return Resume::MostRecent;
    };
    // A refusal is the slate's decision and is final here. This room's recorded
    // id is still on disk and still looks plausible from inside the room, so
    // re-deciding from what is visible locally would only reverse the refusal:
    // the whole reason the slate had to judge it is that a single room cannot
    // see whether a sibling holds the same session.
    if refused
        .iter()
        .any(|(refused_cwd, refused_title)| *refused_cwd == cwd && *refused_title == spec.title)
    {
        // "Most recent" is filtered by neither model nor by what other rooms
        // hold, so it would restore the very session just refused.
        return Resume::Fresh;
    }
    let Some(id) = super::session_state::lookup(vendor.key(), &cwd, &spec.title) else {
        return Resume::MostRecent;
    };
    // A clear recorded durable fresh-state intent. The room must not resume
    // the pre-clear session; a genuine capture will supersede the marker.
    if id == super::session_state::FRESH_STATE_MARKER {
        return Resume::Fresh;
    }
    if vendor == CliVendor::OpenCode && !opencode_claim_is_verifiable(spec, &id) {
        return Resume::Fresh;
    }
    Resume::Session(id)
}

/// Whether this room's recorded OpenCode session can be shown to be on the
/// model the room is configured for.
///
/// Which room *owns* a session needs the whole slate, and that verdict arrives
/// as a refusal. Whether a session is objectively on this room's model needs
/// nothing but the row, so it is the one check an isolated resume can still
/// make, and its only protection. On the slate path this can only ever agree
/// with the repair pass, which reserved or replaced every claim on exactly this
/// basis; a room resuming alone has no such pass behind it.
///
/// An unreadable or absent model column is not evidence of a match, so it is
/// treated as failure. A room with no configured model has nothing to compare
/// against and keeps its claim.
fn opencode_claim_is_verifiable(spec: &RoomSpec, session_id: &str) -> bool {
    let Some(configured) = current_model(spec) else {
        return true;
    };
    let Some(home) = home_dir() else {
        return true;
    };
    let database = home.join(OPENCODE_DATABASE_PATH);
    if !database.is_file() {
        return true;
    }
    opencode_session_model(&database, session_id)
        .is_some_and(|stored| opencode_model_matches(&stored, &configured))
}

const OPENCODE_DATABASE_PATH: &str = ".local/share/opencode/opencode.db";

/// Reassign persisted OpenCode session ids across every room before any of them
/// resumes, so no two rooms sharing a working directory resume the same session
/// and none resumes another model's conversation.
///
/// This cannot be decided one room at a time. From inside a single room, "the
/// sibling is holding the session that belongs to me" and "the sibling legitimately
/// owns that session" look identical: both are an id recorded against another
/// room. Only the full slate separates them, because a claim is trustworthy
/// exactly when its session's recorded model matches that room's configured
/// model. Trustworthy claims are therefore reserved first and left untouched;
/// every remaining room then draws from what is left, newest first, and each id
/// drawn is reserved in turn so a later room cannot take it as well.
///
/// Session ids are unique across directories, so one reserved set is safe to
/// share between rooms with different working directories: an id from elsewhere
/// simply never matches the query, which is already filtered by directory.
///
/// Returns the rooms whose recorded id was rejected and could not be replaced.
/// That verdict has to travel out with them: their id stays on disk, and any
/// later per-room check would see a plausible-looking claim and trust it again.
pub(crate) fn repair_opencode_session_mappings(specs: &[RoomSpec]) -> Vec<RoomKey> {
    let mut refused: Vec<RoomKey> = Vec::new();
    let Some(home) = home_dir() else {
        return refused;
    };
    let database = home.join(OPENCODE_DATABASE_PATH);
    if !database.is_file() {
        return refused;
    }

    struct Claim {
        cwd: PathBuf,
        title: String,
        model: Option<String>,
        session_id: Option<String>,
    }

    let key = CliVendor::OpenCode.key();
    let claims: Vec<Claim> = specs
        .iter()
        .filter(|spec| matches!(cli_vendor(spec), Ok(CliVendor::OpenCode)))
        .filter_map(|spec| {
            let cwd = super::working_directory(spec.cwd.as_deref()).ok()?;
            let session_id = super::session_state::lookup(key, &cwd, &spec.title);
            Some(Claim {
                cwd,
                title: spec.title.clone(),
                model: current_model(spec),
                session_id,
            })
        })
        .collect();

    let mut reserved: Vec<String> = Vec::new();
    let mut unresolved: Vec<&Claim> = Vec::new();
    for claim in &claims {
        let Some(id) = claim.session_id.as_deref() else {
            unresolved.push(claim);
            continue;
        };
        // Only one room can resume a session, so a claim on an id already
        // reserved is not trustworthy however well its model matches: the
        // earlier claim holds it and this room needs one of its own.
        if reserved.iter().any(|held| held == id) {
            unresolved.push(claim);
            continue;
        }
        let trustworthy = match &claim.model {
            Some(model) => opencode_session_model(&database, id)
                .is_some_and(|stored| opencode_model_matches(&stored, model)),
            // A room with no configured model cannot be model-checked, and it
            // resumes this id regardless. Reserve it anyway, or a room that can
            // be checked will draw the session out from under it.
            None => true,
        };
        if trustworthy {
            reserved.push(id.to_owned());
        } else {
            unresolved.push(claim);
        }
    }

    for claim in unresolved {
        if let Some(correct) = opencode_session_id(
            &database,
            &claim.cwd,
            SystemTime::UNIX_EPOCH,
            &reserved,
            claim.model.as_deref(),
        ) {
            super::session_state::upsert(key, &claim.cwd, &claim.title, &correct);
            reserved.push(correct);
        } else if claim.session_id.is_some() {
            // Nothing left to hand this room, and what it recorded was rejected.
            // A room that never had a mapping is not refused: it has claimed
            // nothing, so the ambiguous "most recent" form is still its best
            // available answer.
            refused.push((claim.cwd.clone(), claim.title.clone()));
        }
    }
    refused
}

/// Discover the exact underlying session id for a fresh spawn's vendor
/// artifact, restricted to artifacts touched after `since` (the instant that
/// spawn's own process was created -- not the later intro-sent event, because
/// Codex creates its session artifact at spawn and OpenCode's row, though
/// committed much later, must still postdate this spawn rather than an
/// earlier one in the same directory), and skipping any id already present in
/// `exclude` (the persisted baseline of claimed ids for this `(vendor, cwd)`).
/// Returns `None` when nothing matches.
pub(super) fn discover_session_id(
    vendor: CliVendor,
    cwd: &Path,
    since: SystemTime,
    exclude: &[String],
    opencode_model: Option<&str>,
) -> Option<String> {
    let home = home_dir()?;
    match vendor {
        CliVendor::Claude => {
            let projects = home.join(".claude").join("projects");
            claude_session_id(&projects, cwd, since, exclude)
        }
        CliVendor::Codex => {
            let sessions = home.join(".codex").join("sessions");
            codex_session_id(&sessions, cwd, since, exclude)
        }
        CliVendor::OpenCode => {
            let database = home.join(OPENCODE_DATABASE_PATH);
            opencode_session_id(&database, cwd, since, exclude, opencode_model)
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(home) = test_home() {
            return Some(home);
        }
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Test-only override for the home directory, so `discover_session_id` (which
/// resolves vendor artifacts under `~/.claude`, `~/.codex`, `~/.local/share`)
/// can point at a temp tree in tests instead of the real home.
#[cfg(test)]
static TEST_HOME: std::sync::OnceLock<std::sync::RwLock<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn test_home() -> Option<PathBuf> {
    let lock = TEST_HOME.get_or_init(|| std::sync::RwLock::new(None));
    lock.read().ok().and_then(|guard| guard.clone())
}

/// Points `discover_session_id`'s home at a fresh temp tree while held, and
/// restores the real home on drop.
#[cfg(test)]
pub(super) struct HomeDirGuard {
    home: PathBuf,
}

#[cfg(test)]
impl HomeDirGuard {
    pub(super) fn isolated() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let home = std::env::temp_dir().join(format!(
            "crowded-fake-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let lock = TEST_HOME.get_or_init(|| std::sync::RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(home.clone());
        }
        Self { home }
    }

    pub(super) fn path(&self) -> &Path {
        &self.home
    }
}

#[cfg(test)]
impl Drop for HomeDirGuard {
    fn drop(&mut self) {
        let lock = TEST_HOME.get_or_init(|| std::sync::RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}

/// Claude stores transcripts at `~/.claude/projects/<sanitized-cwd>/<session
/// id>.jsonl`; the filename stem is the session id. `sanitized-cwd` replaces
/// every character that is not an ASCII letter or digit with `-`, including
/// the leading separator (confirmed against real directories on this
/// machine: `/Users/me/project` -> `-Users-me-project`).
///
/// Extracted straight from Claude CLI's own bundle on a Windows box
/// (`e.replace(/[^a-zA-Z0-9]/g,"-")`) and cross-checked byte-for-byte against
/// every entry under `~/.claude/projects` and `~/.claude.json`'s `projects`
/// map on that machine. A prior fix here only added `:` to a hand-picked
/// separator list, which still missed `.` -- so a cwd with a dot anywhere in
/// it (e.g. a Windows username like `Bruno.O`) still failed to match, and
/// session capture silently found nothing.
///
/// That JS regex has no `u` flag, so it matches UTF-16 *code units*, not
/// Unicode scalar values: a cwd containing an astral character (e.g. an
/// emoji, encoded as a surrogate pair) gets two hyphens from Claude CLI, one
/// per surrogate half. Iterating `char`s would collapse that to one hyphen,
/// so this walks UTF-16 code units instead to stay byte-for-byte identical
/// for every cwd, not just the common BMP case.
fn claude_project_directory(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .encode_utf16()
        .map(|unit| match unit {
            0x30..=0x39 | 0x41..=0x5a | 0x61..=0x7a => {
                char::from_u32(unit as u32).expect("ASCII alphanumeric code unit is a valid char")
            }
            _ => '-',
        })
        .collect()
}

fn claude_session_id(
    projects_dir: &Path,
    cwd: &Path,
    since: SystemTime,
    exclude: &[String],
) -> Option<String> {
    let project_dir = projects_dir.join(claude_project_directory(cwd));
    let mut newest: Option<(SystemTime, String)> = None;
    for entry in fs::read_dir(project_dir).ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(OsStr::to_str) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified < since {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(OsStr::to_str) else {
            continue;
        };
        // Skip ids another room (or this room's own prior spawn) already owns.
        if exclude.iter().any(|id| id == stem) {
            continue;
        }
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, stem.to_owned()));
        }
    }
    newest.map(|(_, stem)| stem)
}

fn codex_session_id(
    sessions_dir: &Path,
    cwd: &Path,
    since: SystemTime,
    exclude: &[String],
) -> Option<String> {
    let mut candidates: Vec<(SystemTime, String)> = Vec::new();
    for year in fs::read_dir(sessions_dir).ok()?.flatten() {
        if !year.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            continue;
        }
        for month in fs::read_dir(year.path()).ok()?.flatten() {
            if !month.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            for day in fs::read_dir(month.path()).ok()?.flatten() {
                if !day.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    continue;
                }
                for entry in fs::read_dir(day.path()).ok()?.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(OsStr::to_str) != Some("jsonl") {
                        continue;
                    }
                    let Ok(metadata) = entry.metadata() else {
                        continue;
                    };
                    let Ok(modified) = metadata.modified() else {
                        continue;
                    };
                    if modified < since {
                        continue;
                    }
                    if let Some(id) = codex_session_meta_id(&path, cwd) {
                        // Skip ids another room (or this room's own prior
                        // spawn) already owns.
                        if exclude.iter().any(|claimed| claimed == &id) {
                            continue;
                        }
                        candidates.push((modified, id));
                    }
                }
            }
        }
    }
    candidates.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    candidates.first().map(|(_, id)| id.clone())
}

/// Parse a Codex rollout file's first line: `{"type":"session_meta","payload":
/// {"id":"<uuid>","cwd":"<path>",...}}`. The ground-truth field is `payload.id`
/// (the session id); `session_id` is a fallback for payloads that omit `id`.
/// Live Codex 0.147.0 payloads carry *both* fields, so they must be
/// deserialized independently rather than via `#[serde(alias)]` -- an alias
/// on `id` makes serde treat a payload containing both keys as a duplicate
/// field and reject it outright.
fn codex_session_meta_id(path: &Path, cwd: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct SessionPayload {
        id: Option<String>,
        session_id: Option<String>,
        cwd: String,
    }

    #[derive(Deserialize)]
    struct SessionMetaRecord {
        #[serde(rename = "type")]
        kind: Option<String>,
        payload: Option<SessionPayload>,
    }

    let first_line = fs::read_to_string(path).ok()?.lines().next()?.to_owned();
    let record: SessionMetaRecord = serde_json::from_str(&first_line).ok()?;
    if record.kind.as_deref() != Some("session_meta") {
        return None;
    }
    let payload = record.payload?;
    if payload.cwd != cwd.to_string_lossy() {
        return None;
    }
    payload.id.or(payload.session_id)
}

/// Read the newest session row for `cwd` from OpenCode's sqlite database via
/// the system `sqlite3` CLI. `time_created` is milliseconds since the Unix
/// epoch. Live evidence (OpenCode 1.18.15 under Headroom, this repo, 2026-08-08)
/// shows the row is committed at first-message time, not at process spawn --
/// measured 47-55s after the guest process started, confirmed independently
/// against `ps` start times and the row's own `time_created`. So, unlike
/// Claude/Codex, the row for this spawn will not exist yet for most of the
/// capture window; `since` still matters once it does appear, to reject a
/// stale unclaimed row left over from an earlier spawn in the same directory
/// that never got captured. `exclude` (the persisted baseline of
/// already-claimed ids for this `(vendor, cwd)`) additionally skips rows that
/// belong to a sibling room or this room's own stale pre-clear id.
fn opencode_session_id(
    database: &Path,
    cwd: &Path,
    since: SystemTime,
    exclude: &[String],
    model: Option<&str>,
) -> Option<String> {
    if !database.is_file() {
        return None;
    }
    let since_millis = since
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let output = Command::new("sqlite3")
        .arg(database)
        .arg(opencode_session_query(cwd, since_millis, exclude, model))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() { None } else { Some(id) }
}

fn opencode_session_model(database: &Path, session_id: &str) -> Option<String> {
    if !database.is_file() {
        return None;
    }
    let esc_id = session_id.replace('\'', "''");
    let query = format!("SELECT model FROM session WHERE id = '{esc_id}';");
    let output = Command::new("sqlite3")
        .arg(database)
        .arg(query)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let model = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if model.is_empty() { None } else { Some(model) }
}

fn opencode_model_matches(stored: &str, configured: &str) -> bool {
    let Ok(stored_json) = serde_json::from_str::<serde_json::Value>(stored) else {
        return false;
    };
    let (cfg_provider, cfg_id) = normalize_opencode_model(configured);
    let stored_provider = stored_json
        .get("providerID")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stored_id = stored_json.get("id").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(p) = cfg_provider
        && stored_provider != p
    {
        return false;
    }
    if let Some(i) = cfg_id
        && stored_id != i
    {
        return false;
    }
    true
}

/// Build the safe lookup query. `cwd` and each excluded id are local trusted
/// values; single quotes are the only SQL metacharacter that needs escaping
/// here. `since_millis` is a locally-computed integer, safe to interpolate
/// directly. Passed as a single argv element to `Command`, never through a
/// shell.
fn normalize_opencode_model(model: &str) -> (Option<String>, Option<String>) {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return (None, None);
    }
    if let Some((provider, id)) = trimmed.split_once('/') {
        let provider = provider.trim();
        let id = id.trim();
        if provider.is_empty() || id.is_empty() {
            return (None, None);
        }
        (Some(provider.to_owned()), Some(id.to_owned()))
    } else {
        (None, Some(trimmed.to_owned()))
    }
}

fn opencode_session_query(
    cwd: &Path,
    since_millis: u128,
    exclude: &[String],
    model: Option<&str>,
) -> String {
    let escaped_cwd = cwd.to_string_lossy().replace('\'', "''");
    let mut query = format!(
        "SELECT id FROM session WHERE directory = '{escaped_cwd}' AND time_created > {since_millis}"
    );
    if let Some(m) = model {
        let (provider, id) = normalize_opencode_model(m);
        if let Some(p) = provider {
            let esc_p = p.replace('\'', "''");
            query.push_str(&format!(
                " AND json_extract(model, '$.providerID') = '{esc_p}'"
            ));
        }
        if let Some(i) = id {
            let esc_i = i.replace('\'', "''");
            query.push_str(&format!(" AND json_extract(model, '$.id') = '{esc_i}'"));
        }
    }
    if !exclude.is_empty() {
        let ids = exclude
            .iter()
            .map(|id| id.replace('\'', "''"))
            .collect::<Vec<_>>()
            .join("', '");
        query.push_str(&format!(" AND id NOT IN ('{ids}')"));
    }
    query.push_str(" ORDER BY time_created DESC LIMIT 1;");
    query
}

pub(super) fn set_model(spec: &mut RoomSpec, model: &str) -> io::Result<()> {
    cli_vendor(spec)?;
    replace_option(&mut spec.args, &["--model", "-m"], "--model", model);
    Ok(())
}

pub(super) fn set_effort(spec: &mut RoomSpec, effort: &str) -> io::Result<()> {
    match cli_vendor(spec)? {
        CliVendor::Claude => replace_option(&mut spec.args, &["--effort"], "--effort", effort),
        CliVendor::Codex => replace_codex_effort(&mut spec.args, effort),
        CliVendor::OpenCode => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "OpenCode does not expose a stable effort launch option",
            ));
        }
    }
    Ok(())
}

pub(super) fn current_model(spec: &RoomSpec) -> Option<String> {
    let _ = cli_vendor(spec).ok()?;
    scan_option(&spec.args, &["--model", "-m"])
}

pub(super) fn current_effort(spec: &RoomSpec) -> Option<String> {
    match cli_vendor(spec).ok()? {
        CliVendor::Claude => scan_option(&spec.args, &["--effort"]),
        CliVendor::Codex => scan_codex_effort(&spec.args),
        CliVendor::OpenCode => None,
    }
}

pub(super) fn cli_vendor(spec: &RoomSpec) -> io::Result<CliVendor> {
    if spec.transport != Transport::Raw {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "terminal rooms do not support agent controls",
        ));
    }
    let guest = Path::new(spec.program.as_os_str())
        .file_name()
        .unwrap_or(spec.program.as_os_str())
        .to_string_lossy();
    match guest.to_ascii_lowercase().as_str() {
        "claude" => Ok(CliVendor::Claude),
        "codex" => Ok(CliVendor::Codex),
        "opencode" => Ok(CliVendor::OpenCode),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{guest} has no Conductor adapter"),
        )),
    }
}

/// The adapter-derived capability matrix for one room, read off the same
/// adapter `set_model`/`set_effort`/`current_*` use. No vendor probes: the
/// model catalogue is always `Unknown`, and only the configured `model`
/// field makes any model claim. Effort levels come from the doorbell's
/// `Effort` set for adapters that accept an effort launch option; OpenCode
/// has no stable one, so its list stays empty rather than claiming
/// unsupported effort control.
pub(super) fn capabilities(spec: &RoomSpec) -> RoomCapabilities {
    let (supported_controls, effort_levels) = match cli_vendor(spec) {
        Ok(CliVendor::Claude | CliVendor::Codex) => (
            vec![
                SupportedControl::Clear,
                SupportedControl::Resume,
                SupportedControl::Model,
                SupportedControl::Effort,
            ],
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
                Effort::Max,
            ],
        ),
        Ok(CliVendor::OpenCode) => (
            vec![
                SupportedControl::Clear,
                SupportedControl::Resume,
                SupportedControl::Model,
            ],
            Vec::new(),
        ),
        Err(_) => (Vec::new(), Vec::new()),
    };
    RoomCapabilities {
        controls: !supported_controls.is_empty(),
        supported_controls,
        effort_levels,
        model_catalogue: ModelCatalogue::Unknown,
    }
}

fn replace_option(args: &mut Vec<OsString>, aliases: &[&str], option: &str, value: &str) {
    let mut kept = Vec::with_capacity(args.len() + 2);
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if aliases.iter().any(|alias| argument == *alias) {
            index += 1;
            if index < args.len() {
                index += 1;
            }
            continue;
        }
        if aliases
            .iter()
            .any(|alias| argument.starts_with(&format!("{alias}=")))
        {
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    args.clear();
    args.push(option.into());
    args.push(value.into());
    args.append(&mut kept);
}

fn replace_codex_effort(args: &mut Vec<OsString>, effort: &str) {
    let mut kept = Vec::with_capacity(args.len() + 2);
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if matches!(argument.as_ref(), "-c" | "--config")
            && args.get(index + 1).is_some_and(|value| {
                value
                    .to_string_lossy()
                    .starts_with("model_reasoning_effort=")
            })
        {
            index += 2;
            continue;
        }
        if argument.starts_with("--config=model_reasoning_effort=") {
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    args.clear();
    args.push("-c".into());
    args.push(format!("model_reasoning_effort=\"{effort}\"").into());
    args.append(&mut kept);
}

fn strip_flags(args: &mut Vec<OsString>, flags: &[&str]) {
    args.retain(|argument| {
        let argument = argument.to_string_lossy();
        !flags
            .iter()
            .any(|flag| argument == *flag || argument.starts_with(&format!("{flag}=")))
    });
}

fn strip_options(args: &mut Vec<OsString>, options: &[&str]) {
    let mut kept = Vec::with_capacity(args.len());
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if options.iter().any(|option| argument == *option) {
            index += 1;
            if args
                .get(index)
                .is_some_and(|value| !value.to_string_lossy().starts_with('-'))
            {
                index += 1;
            }
            continue;
        }
        if options
            .iter()
            .any(|option| argument.starts_with(&format!("{option}=")))
        {
            index += 1;
            continue;
        }
        kept.push(args[index].clone());
        index += 1;
    }
    *args = kept;
}

fn scan_option(args: &[OsString], aliases: &[&str]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        for alias in aliases {
            if argument == *alias {
                return args
                    .get(index + 1)
                    .map(|v| v.to_string_lossy().into_owned());
            }
            if let Some(value) = argument.strip_prefix(&format!("{alias}=")) {
                return Some(value.to_owned());
            }
        }
        index += 1;
    }
    None
}

fn scan_codex_effort(args: &[OsString]) -> Option<String> {
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].to_string_lossy();
        if matches!(argument.as_ref(), "-c" | "--config")
            && let Some(value) = args.get(index + 1)
        {
            let value = value.to_string_lossy();
            if let Some(effort) = value.strip_prefix("model_reasoning_effort=") {
                return Some(effort.trim_matches('"').to_owned());
            }
        }
        if let Some(effort) = argument.strip_prefix("--config=model_reasoning_effort=") {
            return Some(effort.trim_matches('"').to_owned());
        }
        index += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn raw_room(program: &str) -> RoomSpec {
        RoomSpec {
            name: program.to_owned(),
            vendor: String::new(),
            title: program.to_owned(),
            program: program.into(),
            args: Vec::new(),
            transport: Transport::Raw,
            cwd: None,
            variables: Vec::new(),
            allow_control: false,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
        }
    }

    #[test]
    fn claude_and_codex_use_bracketed_paste() {
        assert!(uses_bracketed_paste(&raw_room("claude")));
        assert!(uses_bracketed_paste(&raw_room("codex")));
        assert!(!uses_bracketed_paste(&raw_room("opencode")));
    }

    #[test]
    fn add_resume_args_sets_the_vendor_continue_flag() {
        // Point the session-state lookup at an empty temp dir so this test is
        // independent of any real .crowded/session-state.json in the repo.
        let _state = super::super::session_state::StateRootGuard::isolated();

        let mut claude = raw_room("claude");
        claude.args = vec!["--model".into(), "sonnet".into()];
        add_resume_args(&mut claude, &[]).unwrap();
        assert_eq!(
            claude.args,
            vec![
                OsString::from("--model"),
                "sonnet".into(),
                "--continue".into()
            ]
        );

        let mut codex = raw_room("codex");
        codex.args = vec!["--dangerously-bypass-approvals-and-sandbox".into()];
        add_resume_args(&mut codex, &[]).unwrap();
        assert_eq!(
            codex.args,
            vec![
                OsString::from("--dangerously-bypass-approvals-and-sandbox"),
                "resume".into(),
                "--last".into(),
            ]
        );
        // Resuming again must not stack duplicate "resume --last" pairs.
        add_resume_args(&mut codex, &[]).unwrap();
        assert_eq!(
            codex.args,
            vec![
                OsString::from("--dangerously-bypass-approvals-and-sandbox"),
                "resume".into(),
                "--last".into(),
            ]
        );

        let mut opencode = raw_room("opencode");
        add_resume_args(&mut opencode, &[]).unwrap();
        assert_eq!(opencode.args, vec![OsString::from("--continue")]);
    }

    #[test]
    fn add_resume_args_prefers_a_captured_exact_session_id() {
        let _state = super::super::session_state::StateRootGuard::isolated();
        let cwd = std::env::current_dir().unwrap();
        // Pre-seed a captured id for each vendor under the isolated root,
        // keyed by the room title each raw_room below uses.
        for (vendor, id) in [
            ("claude", "claude-ses-1"),
            ("codex", "codex-ses-1"),
            ("opencode", "opencode-ses-1"),
        ] {
            super::super::session_state::upsert(vendor, &cwd, vendor, id);
        }

        let mut claude = raw_room("claude");
        claude.cwd = Some(cwd.clone());
        claude.args = vec!["--model".into(), "sonnet".into()];
        add_resume_args(&mut claude, &[]).unwrap();
        assert_eq!(
            claude.args,
            vec![
                OsString::from("--model"),
                "sonnet".into(),
                "--resume".into(),
                "claude-ses-1".into(),
            ]
        );

        let mut codex = raw_room("codex");
        codex.cwd = Some(cwd.clone());
        codex.args = vec!["--dangerously-bypass-approvals-and-sandbox".into()];
        add_resume_args(&mut codex, &[]).unwrap();
        assert_eq!(
            codex.args,
            vec![
                OsString::from("--dangerously-bypass-approvals-and-sandbox"),
                "resume".into(),
                "codex-ses-1".into(),
            ]
        );

        let mut opencode = raw_room("opencode");
        opencode.cwd = Some(cwd);
        add_resume_args(&mut opencode, &[]).unwrap();
        assert_eq!(
            opencode.args,
            vec![OsString::from("--session"), "opencode-ses-1".into()]
        );
    }

    #[test]
    fn claude_project_directory_replaces_every_separator_with_a_hyphen() {
        assert_eq!(
            claude_project_directory(Path::new("/Users/bruno/project")),
            "-Users-bruno-project"
        );
        assert_eq!(
            claude_project_directory(Path::new("/private/tmp/a")),
            "-private-tmp-a"
        );
        assert_eq!(
            claude_project_directory(Path::new(r"C:\Users\bruno\project")),
            "C--Users-bruno-project"
        );
    }

    #[test]
    fn claude_project_directory_also_sanitizes_dots_in_a_windows_username() {
        // Regression: a real Windows machine with a dotted username
        // (`C:\Users\Bruno.O\...`) produced an on-disk project directory of
        // `C--Users-Bruno-O-...` (the dot became a hyphen too) -- confirmed
        // against `~/.claude/projects` and `~/.claude.json` on that machine.
        // The earlier fix only added `:` to the replacement set and still
        // missed `.`, so this case kept failing to match.
        assert_eq!(
            claude_project_directory(Path::new(r"C:\Users\Bruno.O\Development\crowded")),
            "C--Users-Bruno-O-Development-crowded"
        );
        assert_eq!(
            claude_project_directory(Path::new(r"C:\Users\Bruno.O\Development\FVR\test-crowded")),
            "C--Users-Bruno-O-Development-FVR-test-crowded"
        );
    }

    #[test]
    fn claude_project_directory_matches_claude_cli_on_astral_characters() {
        // Claude CLI's JS regex (`/[^a-zA-Z0-9]/g`) has no `u` flag, so it
        // matches UTF-16 *code units*: an astral character like an emoji is
        // stored as a surrogate pair and gets sanitized to two hyphens, one
        // per surrogate half. Iterating Rust `char`s (Unicode scalar values)
        // would collapse that to a single hyphen and mismatch Claude CLI's
        // real directory name.
        assert_eq!(
            claude_project_directory(Path::new("/Users/bruno/\u{1F600}project")),
            "-Users-bruno---project"
        );
    }

    #[test]
    fn claude_session_id_finds_newest_transcript_after_since() {
        let root = std::env::temp_dir().join(format!("crowded-claude-test-{}", std::process::id()));
        let project = root.join("-Users-me-project");
        fs::create_dir_all(&project).unwrap();

        let old_path = project.join("old-session.jsonl");
        let new_path = project.join("new-session.jsonl");
        fs::write(&old_path, "{}").unwrap();
        fs::write(&new_path, "{}").unwrap();

        let now = SystemTime::now();
        // Both transcripts are newer than `since` so either is a valid
        // candidate; only relative recency orders them.
        let old_time = now - Duration::from_secs(30);
        let recent = now - Duration::from_secs(1);
        set_modified(&old_path, old_time);
        set_modified(&new_path, recent);

        // The newest transcript after `since` wins; the older one is excluded.
        let since = now - Duration::from_secs(60);
        assert_eq!(
            claude_session_id(&root, Path::new("/Users/me/project"), since, &[]).as_deref(),
            Some("new-session")
        );
        // An already-claimed id is skipped, even when it is the newest.
        assert_eq!(
            claude_session_id(
                &root,
                Path::new("/Users/me/project"),
                since,
                &["new-session".to_owned()]
            )
            .as_deref(),
            Some("old-session")
        );

        // A `since` after both files are touched finds nothing.
        assert_eq!(
            claude_session_id(&root, Path::new("/Users/me/project"), now, &[]),
            None
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_session_id_matches_cwd_and_ignores_older_or_foreign_files() {
        let root = std::env::temp_dir().join(format!("crowded-codex-test-{}", std::process::id()));
        let day = root.join("2026/08/07");
        fs::create_dir_all(&day).unwrap();
        let now = SystemTime::now();
        let since = now - Duration::from_secs(60);

        let ours = day.join("rollout-2026-08-07T10-00-00-deadbeef.jsonl");
        fs::write(
            &ours,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-abc\",\"cwd\":\"/repo\"}}\n",
        )
        .unwrap();
        set_modified(&ours, now - Duration::from_secs(10));

        let stale = day.join("rollout-2026-08-07T09-00-00-cafebabe.jsonl");
        fs::write(
            &stale,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-old\",\"cwd\":\"/repo\"}}\n",
        )
        .unwrap();
        // Newer than `since` so it stays a valid candidate for the exclude
        // check, but older than `ours` so `ses-abc` still wins normally.
        set_modified(&stale, now - Duration::from_secs(30));

        let foreign = day.join("rollout-2026-08-07T10-05-00-0123abcd.jsonl");
        fs::write(
            &foreign,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-other\",\"cwd\":\"/elsewhere\"}}\n",
        )
        .unwrap();
        set_modified(&foreign, now - Duration::from_secs(5));

        assert_eq!(
            codex_session_id(&root, Path::new("/repo"), since, &[]).as_deref(),
            Some("ses-abc")
        );
        // An already-claimed id is skipped even though it is the newest for
        // this cwd.
        assert_eq!(
            codex_session_id(&root, Path::new("/repo"), since, &["ses-abc".to_owned()]).as_deref(),
            Some("ses-old")
        );
        // Nothing newer than `since` for this cwd.
        assert_eq!(
            codex_session_id(&root, Path::new("/repo"), now, &[]).as_deref(),
            None
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_session_meta_id_prefers_id_over_session_id_when_both_present() {
        // Live Codex 0.147.0 shape: the first line carries both `id` and
        // `session_id`. Before the fix, `#[serde(alias = "session_id")]` on
        // `id` made serde reject this as a duplicate field; now the two are
        // independent fields and `id` wins.
        let root = std::env::temp_dir().join(format!(
            "crowded-codex-meta-test-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout-both-fields.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-new\",\"session_id\":\"ses-legacy\",\"cwd\":\"/repo\"}}\n",
        )
        .unwrap();

        assert_eq!(
            codex_session_meta_id(&path, Path::new("/repo")).as_deref(),
            Some("ses-new")
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_session_meta_id_falls_back_to_session_id_when_id_is_absent() {
        let root = std::env::temp_dir().join(format!(
            "crowded-codex-meta-test-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("rollout-session-id-only.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"ses-legacy\",\"cwd\":\"/repo\"}}\n",
        )
        .unwrap();

        assert_eq!(
            codex_session_meta_id(&path, Path::new("/repo")).as_deref(),
            Some("ses-legacy")
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn codex_session_meta_id_rejects_wrong_cwd_and_wrong_record_type() {
        let root = std::env::temp_dir().join(format!(
            "crowded-codex-meta-test-{}-{}",
            std::process::id(),
            line!()
        ));
        fs::create_dir_all(&root).unwrap();

        let wrong_cwd = root.join("rollout-wrong-cwd.jsonl");
        fs::write(
            &wrong_cwd,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-abc\",\"cwd\":\"/elsewhere\"}}\n",
        )
        .unwrap();
        assert_eq!(codex_session_meta_id(&wrong_cwd, Path::new("/repo")), None);

        let wrong_type = root.join("rollout-wrong-type.jsonl");
        fs::write(
            &wrong_type,
            "{\"type\":\"turn_context\",\"payload\":{\"id\":\"ses-abc\",\"cwd\":\"/repo\"}}\n",
        )
        .unwrap();
        assert_eq!(codex_session_meta_id(&wrong_type, Path::new("/repo")), None);

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn opencode_session_query_escapes_single_quotes() {
        let query = opencode_session_query(Path::new("/some/it's-path"), 0, &[], None);
        assert!(query.contains("directory = '/some/it''s-path'"));
        assert!(query.ends_with(" ORDER BY time_created DESC LIMIT 1;"));
        // No exclude baseline: no NOT IN clause.
        assert!(!query.contains("NOT IN"));
    }

    #[test]
    fn opencode_session_query_filters_by_since_millis() {
        let query = opencode_session_query(Path::new("/repo"), 1_786_195_000_000, &[], None);
        assert!(query.contains("AND time_created > 1786195000000"));
    }

    #[test]
    fn opencode_session_query_excludes_already_claimed_ids() {
        let query = opencode_session_query(
            Path::new("/repo"),
            0,
            &["sibling-id".to_owned(), "stale-own-id".to_owned()],
            None,
        );
        assert!(query.contains("directory = '/repo'"));
        assert!(query.contains("AND id NOT IN ('sibling-id', 'stale-own-id')"));
        assert!(query.ends_with(" ORDER BY time_created DESC LIMIT 1;"));
        // Excluded ids are single-quote escaped the same way as the cwd.
        let escaped = opencode_session_query(Path::new("/repo"), 0, &["it's-id".to_owned()], None);
        assert!(escaped.contains("AND id NOT IN ('it''s-id')"));
    }

    #[test]
    fn opencode_session_query_filters_by_model() {
        let query = opencode_session_query(Path::new("/repo"), 0, &[], Some("meta/muse-spark-1.2"));
        assert!(query.contains("json_extract(model, '$.providerID') = 'meta'"));
        assert!(query.contains("json_extract(model, '$.id') = 'muse-spark-1.2'"));
        assert!(query.contains("directory = '/repo'"));

        let query2 = opencode_session_query(Path::new("/repo"), 0, &[], Some("muse-spark-1.2"));
        assert!(query2.contains("json_extract(model, '$.id') = 'muse-spark-1.2'"));
        assert!(!query2.contains("providerID"));

        let query3 = opencode_session_query(
            Path::new("/repo"),
            0,
            &[],
            Some("deepseek/deepseek-v4-flash"),
        );
        assert!(query3.contains("providerID') = 'deepseek'"));
        assert!(query3.contains("id') = 'deepseek-v4-flash'"));

        let query4 = opencode_session_query(Path::new("/repo"), 0, &[], Some("a'b/c'd"));
        assert!(query4.contains("a''b"));
        assert!(query4.contains("c''d"));
    }

    #[test]
    fn normalize_opencode_model_splits_provider_and_id() {
        assert_eq!(
            normalize_opencode_model("meta/muse-spark-1.2"),
            (Some("meta".to_owned()), Some("muse-spark-1.2".to_owned()))
        );
        assert_eq!(
            normalize_opencode_model("deepseek/deepseek-v4-flash"),
            (
                Some("deepseek".to_owned()),
                Some("deepseek-v4-flash".to_owned())
            )
        );
        assert_eq!(
            normalize_opencode_model("muse-spark-1.2"),
            (None, Some("muse-spark-1.2".to_owned()))
        );
        assert_eq!(normalize_opencode_model(""), (None, None));
        assert_eq!(normalize_opencode_model("  "), (None, None));
        assert_eq!(normalize_opencode_model("a/"), (None, None));
        assert_eq!(normalize_opencode_model("/b"), (None, None));
    }

    #[test]
    fn opencode_session_id_filters_by_model_and_handles_74ms_gap() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let database = std::env::temp_dir().join(format!(
            "crowded-opencode-model-test-{}.db",
            std::process::id()
        ));
        let _ = fs::remove_file(&database);
        let create = r#"CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_created INTEGER NOT NULL, model TEXT);
              INSERT INTO session VALUES ('ses-spark','/repo',1000, '{"providerID":"meta","id":"muse-spark-1.2"}');
              INSERT INTO session VALUES ('ses-deepseek','/repo',1074, '{"providerID":"deepseek","id":"deepseek-v4-flash"}');"#;
        let setup = Command::new("sqlite3")
            .arg(&database)
            .arg(create)
            .output()
            .unwrap();
        assert!(setup.status.success());
        let since = SystemTime::UNIX_EPOCH;
        assert_eq!(
            opencode_session_id(
                &database,
                Path::new("/repo"),
                since,
                &[],
                Some("meta/muse-spark-1.2")
            )
            .as_deref(),
            Some("ses-spark")
        );
        assert_eq!(
            opencode_session_id(
                &database,
                Path::new("/repo"),
                since,
                &[],
                Some("deepseek/deepseek-v4-flash")
            )
            .as_deref(),
            Some("ses-deepseek")
        );
        assert_eq!(
            opencode_session_id(&database, Path::new("/repo"), since, &[], None).as_deref(),
            Some("ses-deepseek")
        );
        fs::remove_file(&database).ok();
    }

    #[test]
    fn crossed_opencode_mapping_is_self_healed_on_resume() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let _state = super::super::session_state::StateRootGuard::isolated();
        let home_guard = crate::pane::controls::HomeDirGuard::isolated();
        let cwd = std::env::current_dir().unwrap();
        let db = home_guard.path().join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&db);
        let cwd_str = cwd.to_string_lossy().to_string();
        let create = format!(
            r#"CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_created INTEGER NOT NULL, model TEXT);
              INSERT INTO session VALUES ('ses-spark','{cwd_str}',1000, '{{"providerID":"meta","id":"muse-spark-1.2"}}');
              INSERT INTO session VALUES ('ses-deepseek','{cwd_str}',1074, '{{"providerID":"deepseek","id":"deepseek-v4-flash"}}');"#,
        );
        let setup = Command::new("sqlite3")
            .arg(&db)
            .arg(create)
            .output()
            .unwrap();
        assert!(setup.status.success());

        super::super::session_state::upsert(
            "opencode",
            &cwd,
            "OpenCode \u{00b7} 3",
            "ses-deepseek",
        );
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-spark");

        let room3 = RoomSpec {
            name: "OpenCode".to_owned(),
            vendor: "opencode".to_owned(),
            title: "OpenCode \u{00b7} 3".to_owned(),
            program: "opencode".into(),
            args: vec!["--model".into(), "meta/muse-spark-1.2".into()],
            transport: crate::config::Transport::Raw,
            cwd: Some(cwd.clone()),
            variables: Vec::new(),
            allow_control: true,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
        };
        let room4 = RoomSpec {
            name: "OpenCode".to_owned(),
            vendor: "opencode".to_owned(),
            title: "OpenCode \u{00b7} 4".to_owned(),
            program: "opencode".into(),
            args: vec!["--model".into(), "deepseek/deepseek-v4-flash".into()],
            transport: crate::config::Transport::Raw,
            cwd: Some(cwd.clone()),
            variables: Vec::new(),
            allow_control: true,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
        };

        // Repair is a slate-level decision, so drive the same entry point
        // `crowded resume` uses rather than one room at a time.
        let mut specs = [room3, room4];
        crate::pane::resume_supported_specs(&mut specs);
        assert!(specs[0].args.contains(&OsString::from("ses-spark")));
        assert!(!specs[0].args.contains(&OsString::from("ses-deepseek")));
        assert!(specs[1].args.contains(&OsString::from("ses-deepseek")));
        assert!(!specs[1].args.contains(&OsString::from("ses-spark")));

        assert_eq!(
            super::super::session_state::lookup("opencode", &cwd, "OpenCode \u{00b7} 3").as_deref(),
            Some("ses-spark")
        );
        assert_eq!(
            super::super::session_state::lookup("opencode", &cwd, "OpenCode \u{00b7} 4").as_deref(),
            Some("ses-deepseek")
        );

        // Re-cross the mapping and repair with the rooms in the opposite order.
        // Whichever room is considered first, neither may end up on the other's
        // session.
        super::super::session_state::upsert(
            "opencode",
            &cwd,
            "OpenCode \u{00b7} 3",
            "ses-deepseek",
        );
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-spark");
        let mut reversed = [specs[1].clone(), specs[0].clone()];
        for spec in &mut reversed {
            clear_resume_args(spec).unwrap();
        }
        crate::pane::resume_supported_specs(&mut reversed);
        assert!(reversed[0].args.contains(&OsString::from("ses-deepseek")));
        assert!(reversed[1].args.contains(&OsString::from("ses-spark")));
    }

    /// Two rooms on the same model cannot be told apart by the model filter, so
    /// only the reserved set stops the room being repaired from taking the
    /// session its sibling already legitimately holds.
    #[test]
    fn repair_does_not_take_a_session_a_sibling_already_holds() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let _state = super::super::session_state::StateRootGuard::isolated();
        let home_guard = crate::pane::controls::HomeDirGuard::isolated();
        let cwd = std::env::current_dir().unwrap();
        let db = home_guard.path().join(OPENCODE_DATABASE_PATH);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&db);
        let cwd_str = cwd.to_string_lossy().to_string();
        let model = r#"{"providerID":"deepseek","id":"deepseek-v4-flash"}"#;
        let create = format!(
            r#"CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_created INTEGER NOT NULL, model TEXT);
              INSERT INTO session VALUES ('ses-older','{cwd_str}',1000, '{model}');
              INSERT INTO session VALUES ('ses-newer','{cwd_str}',2000, '{model}');
              INSERT INTO session VALUES ('ses-stale','{cwd_str}',500, '{{"providerID":"meta","id":"muse-spark-1.2"}}');"#,
        );
        assert!(
            Command::new("sqlite3")
                .arg(&db)
                .arg(create)
                .output()
                .unwrap()
                .status
                .success()
        );

        // Room 3 legitimately holds the newest session. Room 4 is on the same
        // model but points at a session belonging to a different one.
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 3", "ses-newer");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-stale");

        let spec = |title: &str| RoomSpec {
            name: "OpenCode".to_owned(),
            vendor: "opencode".to_owned(),
            title: title.to_owned(),
            program: "opencode".into(),
            args: vec!["--model".into(), "deepseek/deepseek-v4-flash".into()],
            transport: crate::config::Transport::Raw,
            cwd: Some(cwd.clone()),
            variables: Vec::new(),
            allow_control: true,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
        };
        let mut specs = [spec("OpenCode \u{00b7} 3"), spec("OpenCode \u{00b7} 4")];
        crate::pane::resume_supported_specs(&mut specs);

        // Without the reserved set both rooms resume ses-newer.
        assert!(specs[0].args.contains(&OsString::from("ses-newer")));
        assert!(specs[1].args.contains(&OsString::from("ses-older")));
        assert!(!specs[1].args.contains(&OsString::from("ses-newer")));
        assert_eq!(
            super::super::session_state::lookup("opencode", &cwd, "OpenCode \u{00b7} 4").as_deref(),
            Some("ses-older")
        );
    }

    /// Builds a database holding two sessions on one model plus a third on
    /// another, all in `cwd`, and returns the guards that keep the home
    /// directory and the persisted state isolated for the test.
    fn opencode_slate_fixture(
        cwd: &Path,
    ) -> Option<(
        super::super::session_state::StateRootGuard,
        crate::pane::controls::HomeDirGuard,
    )> {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return None;
        }
        let state = super::super::session_state::StateRootGuard::isolated();
        let home_guard = crate::pane::controls::HomeDirGuard::isolated();
        let db = home_guard.path().join(OPENCODE_DATABASE_PATH);
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let _ = std::fs::remove_file(&db);
        let cwd_str = cwd.to_string_lossy().to_string();
        let model = r#"{"providerID":"deepseek","id":"deepseek-v4-flash"}"#;
        let create = format!(
            r#"CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, time_created INTEGER NOT NULL, model TEXT);
              INSERT INTO session VALUES ('ses-older','{cwd_str}',1000, '{model}');
              INSERT INTO session VALUES ('ses-newer','{cwd_str}',2000, '{model}');
              INSERT INTO session VALUES ('ses-stale','{cwd_str}',500, '{{"providerID":"meta","id":"muse-spark-1.2"}}');"#,
        );
        assert!(
            Command::new("sqlite3")
                .arg(&db)
                .arg(create)
                .output()
                .unwrap()
                .status
                .success()
        );
        Some((state, home_guard))
    }

    fn opencode_spec(cwd: &Path, title: &str, model: Option<&str>) -> RoomSpec {
        RoomSpec {
            name: "OpenCode".to_owned(),
            vendor: "opencode".to_owned(),
            title: title.to_owned(),
            program: "opencode".into(),
            args: match model {
                Some(model) => vec!["--model".into(), model.into()],
                None => Vec::new(),
            },
            transport: crate::config::Transport::Raw,
            cwd: Some(cwd.to_path_buf()),
            variables: Vec::new(),
            allow_control: true,
            use_headroom: false,
            scheduling: None,
            headroom_args: Vec::new(),
        }
    }

    /// A matching model is not enough to make a claim trustworthy. When two
    /// rooms have both persisted the same id, only one can hold it; the other
    /// must be treated as unresolved and drawn a session of its own.
    #[test]
    fn duplicate_claims_on_one_session_do_not_both_resume_it() {
        let cwd = std::env::current_dir().unwrap();
        let Some(_guards) = opencode_slate_fixture(&cwd) else {
            return;
        };
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 3", "ses-newer");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-newer");

        let model = Some("deepseek/deepseek-v4-flash");
        let mut specs = [
            opencode_spec(&cwd, "OpenCode \u{00b7} 3", model),
            opencode_spec(&cwd, "OpenCode \u{00b7} 4", model),
        ];
        crate::pane::resume_supported_specs(&mut specs);

        assert!(specs[0].args.contains(&OsString::from("ses-newer")));
        assert!(specs[1].args.contains(&OsString::from("ses-older")));
        assert!(!specs[1].args.contains(&OsString::from("ses-newer")));
    }

    /// A room with no `--model` cannot be model-checked, so it resumes its
    /// persisted id as-is. Its claim must still be reserved, or the sibling that
    /// can be checked draws that session out from under it.
    #[test]
    fn a_modelless_claim_is_reserved_against_a_checkable_sibling() {
        let cwd = std::env::current_dir().unwrap();
        let Some(_guards) = opencode_slate_fixture(&cwd) else {
            return;
        };
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 3", "ses-newer");

        let mut specs = [
            opencode_spec(&cwd, "OpenCode \u{00b7} 3", None),
            opencode_spec(
                &cwd,
                "OpenCode \u{00b7} 4",
                Some("deepseek/deepseek-v4-flash"),
            ),
        ];
        crate::pane::resume_supported_specs(&mut specs);

        assert!(specs[0].args.contains(&OsString::from("ses-newer")));
        assert!(specs[1].args.contains(&OsString::from("ses-older")));
        assert!(!specs[1].args.contains(&OsString::from("ses-newer")));
    }

    /// When no session for this room's model exists, the rejected id must not
    /// degrade into `--continue`: that form is filtered by neither model nor
    /// what other rooms hold, so it would restore the very session just refused.
    #[test]
    fn a_surviving_model_mismatch_starts_fresh_instead_of_continuing() {
        let cwd = std::env::current_dir().unwrap();
        let Some(_guards) = opencode_slate_fixture(&cwd) else {
            return;
        };
        // The only sessions on this room's model belong to its siblings, so the
        // repair pass has nothing left to hand it.
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 3", "ses-newer");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-older");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 5", "ses-stale");

        let model = Some("deepseek/deepseek-v4-flash");
        let mut specs = [
            opencode_spec(&cwd, "OpenCode \u{00b7} 3", model),
            opencode_spec(&cwd, "OpenCode \u{00b7} 4", model),
            opencode_spec(&cwd, "OpenCode \u{00b7} 5", model),
        ];
        crate::pane::resume_supported_specs(&mut specs);

        assert_eq!(
            specs[2].args,
            vec![
                OsString::from("--model"),
                OsString::from("deepseek/deepseek-v4-flash")
            ]
        );
    }

    /// The duplicate guard only moves a room to unresolved. When every matching
    /// session is already reserved there is nothing to move it to, and the
    /// rejected id is still on disk: without the refusal travelling out of the
    /// repair pass, the room resumes the duplicate it was just denied.
    #[test]
    fn a_refused_duplicate_with_no_replacement_starts_fresh() {
        let cwd = std::env::current_dir().unwrap();
        let Some(_guards) = opencode_slate_fixture(&cwd) else {
            return;
        };
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 3", "ses-newer");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-newer");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 5", "ses-older");

        let model = Some("deepseek/deepseek-v4-flash");
        let mut specs = [
            opencode_spec(&cwd, "OpenCode \u{00b7} 3", model),
            opencode_spec(&cwd, "OpenCode \u{00b7} 4", model),
            opencode_spec(&cwd, "OpenCode \u{00b7} 5", model),
        ];
        let resumed = crate::pane::resume_supported_specs(&mut specs);

        assert!(specs[0].args.contains(&OsString::from("ses-newer")));
        assert!(specs[2].args.contains(&OsString::from("ses-older")));
        // The denied duplicate resumes nothing at all.
        assert_eq!(
            specs[1].args,
            vec![
                OsString::from("--model"),
                OsString::from("deepseek/deepseek-v4-flash")
            ]
        );
        // A room that starts fresh has not resumed, so it must still receive
        // its intro and have its new session captured.
        assert_eq!(resumed, vec![true, false, true]);
    }

    /// A session whose model column is unreadable cannot be verified, so it
    /// cannot be trusted either. Repair refuses the claim; the refusal is what
    /// stops the room resuming a sibling's conversation on legacy state.
    #[test]
    fn unverifiable_model_metadata_does_not_resume_the_claim() {
        let cwd = std::env::current_dir().unwrap();
        let Some((_state, home_guard)) = opencode_slate_fixture(&cwd) else {
            return;
        };
        let db = home_guard.path().join(OPENCODE_DATABASE_PATH);
        let cwd_str = cwd.to_string_lossy().to_string();
        assert!(
            Command::new("sqlite3")
                .arg(&db)
                .arg(format!(
                    "INSERT INTO session VALUES ('ses-nometa','{cwd_str}',3000, NULL);"
                ))
                .output()
                .unwrap()
                .status
                .success()
        );
        // Both readable sessions on this model are legitimately held, so no
        // replacement exists for the room holding the unreadable one.
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 3", "ses-newer");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-older");
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 5", "ses-nometa");

        let model = Some("deepseek/deepseek-v4-flash");
        let mut specs = [
            opencode_spec(&cwd, "OpenCode \u{00b7} 3", model),
            opencode_spec(&cwd, "OpenCode \u{00b7} 4", model),
            opencode_spec(&cwd, "OpenCode \u{00b7} 5", model),
        ];
        let resumed = crate::pane::resume_supported_specs(&mut specs);

        assert!(!specs[2].args.contains(&OsString::from("ses-nometa")));
        assert!(!specs[2].args.contains(&OsString::from("--continue")));
        assert_eq!(
            specs[2].args,
            vec![
                OsString::from("--model"),
                OsString::from("deepseek/deepseek-v4-flash")
            ]
        );
        assert_eq!(resumed, vec![true, true, false]);
    }

    /// A room that never recorded a session has claimed nothing, so refusal
    /// must not reach it: it is free to be given whichever session is still
    /// unheld, and it counts as resumed. The `--continue` form remains the
    /// answer only when repair has nothing to give it.
    #[test]
    fn a_room_without_a_mapping_is_not_refused() {
        let cwd = std::env::current_dir().unwrap();
        let Some(_guards) = opencode_slate_fixture(&cwd) else {
            return;
        };
        let mut specs = [opencode_spec(&cwd, "OpenCode \u{00b7} 9", None)];
        let resumed = crate::pane::resume_supported_specs(&mut specs);

        assert!(specs[0].args.contains(&OsString::from("--session")));
        assert_eq!(resumed, vec![true]);
    }

    /// One room resuming on request has no slate and so receives no refusal.
    /// It can still check the one thing that needs no slate: whether the
    /// session it recorded is objectively on the model it is configured for.
    #[test]
    fn an_isolated_resume_rejects_a_claim_on_another_model() {
        let cwd = std::env::current_dir().unwrap();
        let Some((_state, home_guard)) = opencode_slate_fixture(&cwd) else {
            return;
        };
        let db = home_guard.path().join(OPENCODE_DATABASE_PATH);
        let cwd_str = cwd.to_string_lossy().to_string();
        assert!(
            Command::new("sqlite3")
                .arg(&db)
                .arg(format!(
                    "INSERT INTO session VALUES ('ses-nometa','{cwd_str}',3000, NULL);"
                ))
                .output()
                .unwrap()
                .status
                .success()
        );

        // Recorded against a session belonging to a different model.
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 3", "ses-stale");
        let mut mismatched = opencode_spec(
            &cwd,
            "OpenCode \u{00b7} 3",
            Some("deepseek/deepseek-v4-flash"),
        );
        assert!(!add_resume_args(&mut mismatched, &[]).unwrap());
        assert!(!mismatched.args.contains(&OsString::from("--session")));
        assert!(!mismatched.args.contains(&OsString::from("--continue")));

        // Recorded against a session whose model cannot be read at all, which
        // is not evidence of a match.
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 4", "ses-nometa");
        let mut unreadable = opencode_spec(
            &cwd,
            "OpenCode \u{00b7} 4",
            Some("deepseek/deepseek-v4-flash"),
        );
        assert!(!add_resume_args(&mut unreadable, &[]).unwrap());
        assert!(!unreadable.args.contains(&OsString::from("--session")));

        // A claim that does check out is still resumed exactly.
        super::super::session_state::upsert("opencode", &cwd, "OpenCode \u{00b7} 5", "ses-newer");
        let mut verified = opencode_spec(
            &cwd,
            "OpenCode \u{00b7} 5",
            Some("deepseek/deepseek-v4-flash"),
        );
        assert!(add_resume_args(&mut verified, &[]).unwrap());
        assert!(verified.args.contains(&OsString::from("ses-newer")));
    }

    #[test]
    fn opencode_session_id_reads_a_real_sqlite_database_when_available() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let database =
            std::env::temp_dir().join(format!("crowded-opencode-test-{}.db", std::process::id()));
        let _ = fs::remove_file(&database);
        // time_created values are Unix-epoch milliseconds. `ses-pre-spawn`
        // predates `since` (a stale row from an earlier spawn in the same
        // directory) and must never be returned regardless of exclude state.
        let create = "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, \
             time_created INTEGER NOT NULL); \
             INSERT INTO session VALUES ('ses-new','/repo',2000); \
             INSERT INTO session VALUES ('ses-old','/repo',1500); \
             INSERT INTO session VALUES ('ses-pre-spawn','/repo',500); \
             INSERT INTO session VALUES ('ses-other','/elsewhere',2500);";
        let setup = Command::new("sqlite3")
            .arg(&database)
            .arg(create)
            .output()
            .unwrap();
        assert!(setup.status.success());

        let since = std::time::UNIX_EPOCH + Duration::from_millis(1000);

        // Newest row for the cwd at or after `since` wins; other cwds don't
        // leak in, and the pre-spawn row is never a candidate.
        assert_eq!(
            opencode_session_id(&database, Path::new("/repo"), since, &[], None).as_deref(),
            Some("ses-new")
        );
        // An already-claimed id is skipped, so the next-newest row for the cwd
        // wins instead (the sibling-collision / stale-recapture filter).
        assert_eq!(
            opencode_session_id(
                &database,
                Path::new("/repo"),
                since,
                &["ses-new".to_owned()],
                None
            )
            .as_deref(),
            Some("ses-old")
        );
        // When every post-`since` candidate for the cwd is claimed, nothing
        // matches -- the pre-spawn row is not a fallback.
        assert_eq!(
            opencode_session_id(
                &database,
                Path::new("/repo"),
                since,
                &["ses-new".to_owned(), "ses-old".to_owned()],
                None
            )
            .as_deref(),
            None
        );
        assert_eq!(
            opencode_session_id(&database, Path::new("/missing"), since, &[], None).as_deref(),
            None
        );
        // A missing database resolves to None, never an error.
        assert_eq!(
            opencode_session_id(
                &database.join("does-not-exist.db"),
                Path::new("/repo"),
                since,
                &[],
                None
            )
            .as_deref(),
            None
        );

        fs::remove_file(&database).ok();
    }

    fn set_modified(path: &Path, time: SystemTime) {
        let file = fs::File::options().write(true).open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    #[test]
    fn current_model_effort_roundtrips_set_for_claude_and_codex() {
        let mut claude = raw_room("claude");
        set_model(&mut claude, "sonnet").unwrap();
        set_effort(&mut claude, "high").unwrap();
        assert_eq!(current_model(&claude).as_deref(), Some("sonnet"));
        assert_eq!(current_effort(&claude).as_deref(), Some("high"));

        let mut codex = raw_room("codex");
        set_model(&mut codex, "gpt-5").unwrap();
        set_effort(&mut codex, "xhigh").unwrap();
        assert_eq!(current_model(&codex).as_deref(), Some("gpt-5"));
        assert_eq!(current_effort(&codex).as_deref(), Some("xhigh"));

        let opencode = raw_room("opencode");
        assert_eq!(current_effort(&opencode).as_deref(), None);

        let mut opencode = raw_room("opencode");
        set_model(&mut opencode, "gpt-5").unwrap();
        assert_eq!(current_model(&opencode).as_deref(), Some("gpt-5"));
    }

    #[test]
    fn capabilities_derive_from_the_adapter_without_claiming_opencode_effort() {
        let claude = raw_room("claude");
        let claude_caps = capabilities(&claude);
        assert!(claude_caps.controls);
        assert_eq!(
            claude_caps.supported_controls,
            vec![
                SupportedControl::Clear,
                SupportedControl::Resume,
                SupportedControl::Model,
                SupportedControl::Effort,
            ]
        );
        assert_eq!(
            claude_caps.effort_levels,
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
                Effort::Max,
            ]
        );
        assert_eq!(claude_caps.model_catalogue, ModelCatalogue::Unknown);

        let codex = raw_room("codex");
        let codex_caps = capabilities(&codex);
        assert!(codex_caps.controls);
        assert_eq!(codex_caps.effort_levels, claude_caps.effort_levels);

        // OpenCode has a model control but no stable effort launch option, so
        // the capability matrix must not claim effort support for it.
        let opencode = raw_room("opencode");
        let opencode_caps = capabilities(&opencode);
        assert!(opencode_caps.controls);
        assert_eq!(
            opencode_caps.supported_controls,
            vec![
                SupportedControl::Clear,
                SupportedControl::Resume,
                SupportedControl::Model,
            ]
        );
        assert!(opencode_caps.effort_levels.is_empty());
        assert_eq!(opencode_caps.model_catalogue, ModelCatalogue::Unknown);

        // Terminal rooms have no adapter at all: no controls, no effort.
        let mut terminal = raw_room("claude");
        terminal.transport = Transport::Shell;
        let terminal_caps = capabilities(&terminal);
        assert!(!terminal_caps.controls);
        assert!(terminal_caps.supported_controls.is_empty());
        assert!(terminal_caps.effort_levels.is_empty());
    }

    /// Regression: after clear_room records the fresh-state marker, a later
    /// resume must produce no resume args (Resume::Fresh), not resume the
    /// stale pre-clear session or fall back to the most-recent form.
    #[test]
    fn add_resume_args_starts_fresh_after_clear_marker() {
        let _state = super::super::session_state::StateRootGuard::isolated();
        let cwd = std::env::current_dir().unwrap();
        let room = "claude";
        // Pre-seed a stale session id.
        super::super::session_state::upsert("claude", &cwd, room, "stale-pre-clear-id");
        // Clear records the marker.
        super::super::session_state::clear_room("claude", &cwd, room);

        let mut spec = raw_room("claude");
        spec.cwd = Some(cwd);
        let resumed = add_resume_args(&mut spec, &[]).unwrap();
        // Fresh: no resume args were applied, no --continue.
        assert!(!resumed);
        assert!(!spec.args.contains(&OsString::from("--continue")));
        assert!(!spec.args.contains(&OsString::from("--resume")));
        assert!(!spec.args.contains(&OsString::from("--session")));
    }

    /// Regression: a successful capture supersedes the clear marker, so a
    /// later resume targets the exact fresh id rather than starting fresh.
    #[test]
    fn add_resume_args_prefers_fresh_capture_over_clear_marker() {
        let _state = super::super::session_state::StateRootGuard::isolated();
        let cwd = std::env::current_dir().unwrap();
        let room = "codex";
        // Clear first, then a capture supersedes the marker.
        super::super::session_state::clear_room("codex", &cwd, room);
        super::super::session_state::upsert("codex", &cwd, room, "fresh-capture-id");

        let mut spec = raw_room("codex");
        spec.cwd = Some(cwd);
        add_resume_args(&mut spec, &[]).unwrap();
        assert!(spec.args.contains(&OsString::from("fresh-capture-id")));
        assert!(!spec.args.contains(&OsString::from("--last")));
    }
}
