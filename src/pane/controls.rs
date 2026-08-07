use std::{
    env,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

use serde::Deserialize;

use crate::config::{RoomSpec, Transport};

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
    super::session_state::lookup(vendor.key(), &cwd)
}

/// Discover the exact underlying session id for a fresh spawn's vendor
/// artifact, restricted to artifacts touched after `since` (the instant that
/// spawn's own intro was delivered). Returns `None` when nothing matches.
pub(super) fn discover_session_id(
    vendor: CliVendor,
    cwd: &Path,
    since: SystemTime,
) -> Option<String> {
    let home = home_dir()?;
    match vendor {
        CliVendor::Claude => {
            let projects = home.join(".claude").join("projects");
            claude_session_id(&projects, cwd, since)
        }
        CliVendor::Codex => {
            let sessions = home.join(".codex").join("sessions");
            codex_session_id(&sessions, cwd, since)
        }
        CliVendor::OpenCode => {
            let database = home.join(".local/share/opencode/opencode.db");
            opencode_session_id(&database, cwd)
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
}

/// Claude stores transcripts at `~/.claude/projects/<sanitized-cwd>/<session
/// id>.jsonl`; the filename stem is the session id. `sanitized-cwd` replaces
/// every path separator with `-`, including the leading one (confirmed
/// against real directories on this machine: `/Users/me/project` ->
/// `-Users-me-project`).
fn claude_project_directory(cwd: &Path) -> String {
    cwd.to_string_lossy().replace(['/', '\\'], "-")
}

fn claude_session_id(projects_dir: &Path, cwd: &Path, since: SystemTime) -> Option<String> {
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
        if newest.as_ref().is_none_or(|(time, _)| modified > *time) {
            newest = Some((modified, stem.to_owned()));
        }
    }
    newest.map(|(_, stem)| stem)
}

fn codex_session_id(sessions_dir: &Path, cwd: &Path, since: SystemTime) -> Option<String> {
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
/// (the session id); `session_id` is accepted as an alias.
fn codex_session_meta_id(path: &Path, cwd: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct SessionPayload {
        #[serde(alias = "session_id")]
        id: String,
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
    Some(payload.id)
}

/// Read the newest session row for `cwd` from OpenCode's sqlite database via
/// the system `sqlite3` CLI. `time_created` is milliseconds; OpenCode creates
/// the row at spawn (before the intro), so no `since` filter applies here.
fn opencode_session_id(database: &Path, cwd: &Path) -> Option<String> {
    if !database.is_file() {
        return None;
    }
    let output = Command::new("sqlite3")
        .arg(database)
        .arg(opencode_session_query(cwd))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() { None } else { Some(id) }
}

/// Build the safe lookup query. `cwd` is a local trusted path; single quotes
/// are the only SQL metacharacter that needs escaping here.
fn opencode_session_query(cwd: &Path) -> String {
    let escaped = cwd.to_string_lossy().replace('\'', "''");
    format!(
        "SELECT id FROM session WHERE directory = '{escaped}' ORDER BY time_created DESC LIMIT 1;"
    )
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
        // Pre-seed a captured id for each vendor under the isolated root.
        for (vendor, id) in [
            ("claude", "claude-ses-1"),
            ("codex", "codex-ses-1"),
            ("opencode", "opencode-ses-1"),
        ] {
            super::super::session_state::upsert(vendor, &cwd, id);
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
        let old_time = now - Duration::from_secs(120);
        let recent = now - Duration::from_secs(1);
        set_modified(&old_path, old_time);
        set_modified(&new_path, recent);

        // The newest transcript after `since` wins; the older one is excluded.
        let since = now - Duration::from_secs(60);
        assert_eq!(
            claude_session_id(&root, Path::new("/Users/me/project"), since).as_deref(),
            Some("new-session")
        );

        // A `since` after both files are touched finds nothing.
        assert_eq!(
            claude_session_id(&root, Path::new("/Users/me/project"), now),
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
        set_modified(&stale, now - Duration::from_secs(300));

        let foreign = day.join("rollout-2026-08-07T10-05-00-0123abcd.jsonl");
        fs::write(
            &foreign,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"ses-other\",\"cwd\":\"/elsewhere\"}}\n",
        )
        .unwrap();
        set_modified(&foreign, now - Duration::from_secs(5));

        assert_eq!(
            codex_session_id(&root, Path::new("/repo"), since).as_deref(),
            Some("ses-abc")
        );
        // Nothing newer than `since` for this cwd.
        assert_eq!(
            codex_session_id(&root, Path::new("/repo"), now).as_deref(),
            None
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn opencode_session_query_escapes_single_quotes() {
        let query = opencode_session_query(Path::new("/some/it's-path"));
        assert!(query.contains("directory = '/some/it''s-path'"));
        assert!(query.ends_with(" ORDER BY time_created DESC LIMIT 1;"));
    }

    #[test]
    fn opencode_session_id_reads_a_real_sqlite_database_when_available() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let database =
            std::env::temp_dir().join(format!("crowded-opencode-test-{}.db", std::process::id()));
        let _ = fs::remove_file(&database);
        let create = "CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL, \
             time_created INTEGER NOT NULL); \
             INSERT INTO session VALUES ('ses-new','/repo',1000); \
             INSERT INTO session VALUES ('ses-old','/repo',500); \
             INSERT INTO session VALUES ('ses-other','/elsewhere',2000);";
        let setup = Command::new("sqlite3")
            .arg(&database)
            .arg(create)
            .output()
            .unwrap();
        assert!(setup.status.success());

        // Newest row for the cwd wins; other cwds don't leak in.
        assert_eq!(
            opencode_session_id(&database, Path::new("/repo")).as_deref(),
            Some("ses-new")
        );
        assert_eq!(
            opencode_session_id(&database, Path::new("/missing")).as_deref(),
            None
        );
        // A missing database resolves to None, never an error.
        assert_eq!(
            opencode_session_id(&database.join("does-not-exist.db"), Path::new("/repo")).as_deref(),
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
}
