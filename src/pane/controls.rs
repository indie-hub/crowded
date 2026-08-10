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
    doorbell::{Effort, ModelCatalogue, RoomCapabilities},
};

#[derive(Clone, Copy)]
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
pub(super) fn add_resume_args(spec: &mut RoomSpec) -> io::Result<()> {
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
    spec.args.extend(match captured_session_id(vendor, spec) {
        Some(id) => session_resume_args(vendor, &id),
        None => recent_resume_args(vendor),
    });
    Ok(())
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

fn captured_session_id(vendor: CliVendor, spec: &RoomSpec) -> Option<String> {
    let cwd = super::working_directory(spec.cwd.as_deref()).ok()?;
    // Keyed by this room's own identity (RoomSpec.title), so a room only ever
    // resumes the id it (or its own prior spawn under the same title)
    // captured -- never a sibling room's id for the same (vendor, cwd).
    super::session_state::lookup(vendor.key(), &cwd, &spec.title)
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
            let database = home.join(".local/share/opencode/opencode.db");
            opencode_session_id(&database, cwd, since, exclude)
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
/// every path separator with `-`, including the leading one (confirmed
/// against real directories on this machine: `/Users/me/project` ->
/// `-Users-me-project`).
fn claude_project_directory(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '\\'], "-")
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
        .arg(opencode_session_query(cwd, since_millis, exclude))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() { None } else { Some(id) }
}

/// Build the safe lookup query. `cwd` and each excluded id are local trusted
/// values; single quotes are the only SQL metacharacter that needs escaping
/// here. `since_millis` is a locally-computed integer, safe to interpolate
/// directly. Passed as a single argv element to `Command`, never through a
/// shell.
fn opencode_session_query(cwd: &Path, since_millis: u128, exclude: &[String]) -> String {
    let escaped = cwd.to_string_lossy().replace('\'', "''");
    let mut query = format!(
        "SELECT id FROM session WHERE directory = '{escaped}' AND time_created > {since_millis}"
    );
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
    let effort_levels = match cli_vendor(spec) {
        Ok(CliVendor::Claude | CliVendor::Codex) => vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ],
        Ok(CliVendor::OpenCode) | Err(_) => Vec::new(),
    };
    RoomCapabilities {
        controls: cli_vendor(spec).is_ok(),
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
        add_resume_args(&mut claude).unwrap();
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
        add_resume_args(&mut codex).unwrap();
        assert_eq!(
            codex.args,
            vec![
                OsString::from("--dangerously-bypass-approvals-and-sandbox"),
                "resume".into(),
                "--last".into(),
            ]
        );
        // Resuming again must not stack duplicate "resume --last" pairs.
        add_resume_args(&mut codex).unwrap();
        assert_eq!(
            codex.args,
            vec![
                OsString::from("--dangerously-bypass-approvals-and-sandbox"),
                "resume".into(),
                "--last".into(),
            ]
        );

        let mut opencode = raw_room("opencode");
        add_resume_args(&mut opencode).unwrap();
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
        add_resume_args(&mut claude).unwrap();
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
        add_resume_args(&mut codex).unwrap();
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
        add_resume_args(&mut opencode).unwrap();
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
        let query = opencode_session_query(Path::new("/some/it's-path"), 0, &[]);
        assert!(query.contains("directory = '/some/it''s-path'"));
        assert!(query.ends_with(" ORDER BY time_created DESC LIMIT 1;"));
        // No exclude baseline: no NOT IN clause.
        assert!(!query.contains("NOT IN"));
    }

    #[test]
    fn opencode_session_query_filters_by_since_millis() {
        let query = opencode_session_query(Path::new("/repo"), 1_786_195_000_000, &[]);
        assert!(query.contains("AND time_created > 1786195000000"));
    }

    #[test]
    fn opencode_session_query_excludes_already_claimed_ids() {
        let query = opencode_session_query(
            Path::new("/repo"),
            0,
            &["sibling-id".to_owned(), "stale-own-id".to_owned()],
        );
        assert!(query.contains("directory = '/repo'"));
        assert!(query.contains("AND id NOT IN ('sibling-id', 'stale-own-id')"));
        assert!(query.ends_with(" ORDER BY time_created DESC LIMIT 1;"));
        // Excluded ids are single-quote escaped the same way as the cwd.
        let escaped = opencode_session_query(Path::new("/repo"), 0, &["it's-id".to_owned()]);
        assert!(escaped.contains("AND id NOT IN ('it''s-id')"));
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
            opencode_session_id(&database, Path::new("/repo"), since, &[]).as_deref(),
            Some("ses-new")
        );
        // An already-claimed id is skipped, so the next-newest row for the cwd
        // wins instead (the sibling-collision / stale-recapture filter).
        assert_eq!(
            opencode_session_id(
                &database,
                Path::new("/repo"),
                since,
                &["ses-new".to_owned()]
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
                &["ses-new".to_owned(), "ses-old".to_owned()]
            )
            .as_deref(),
            None
        );
        assert_eq!(
            opencode_session_id(&database, Path::new("/missing"), since, &[]).as_deref(),
            None
        );
        // A missing database resolves to None, never an error.
        assert_eq!(
            opencode_session_id(
                &database.join("does-not-exist.db"),
                Path::new("/repo"),
                since,
                &[]
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
        assert!(opencode_caps.effort_levels.is_empty());
        assert_eq!(opencode_caps.model_catalogue, ModelCatalogue::Unknown);

        // Terminal rooms have no adapter at all: no controls, no effort.
        let mut terminal = raw_room("claude");
        terminal.transport = Transport::Shell;
        let terminal_caps = capabilities(&terminal);
        assert!(!terminal_caps.controls);
        assert!(terminal_caps.effort_levels.is_empty());
    }
}
