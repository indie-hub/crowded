use std::{ffi::OsString, io, path::Path};

use crate::config::{RoomSpec, Transport};

#[derive(Clone, Copy)]
enum CliVendor {
    Claude,
    Codex,
    OpenCode,
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

fn cli_vendor(spec: &RoomSpec) -> io::Result<CliVendor> {
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
        }
    }

    #[test]
    fn claude_and_codex_use_bracketed_paste() {
        assert!(uses_bracketed_paste(&raw_room("claude")));
        assert!(uses_bracketed_paste(&raw_room("codex")));
        assert!(!uses_bracketed_paste(&raw_room("opencode")));
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
