//! Materialize the shared toolbox into each guest's native project config.

use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{
    McpClient, McpConfig, OpenCodePluginConfig, RoomConfig, Transport, claude_mcp_config,
    load_room_file, opencode_mcp_config, validate_mcp_servers, validate_opencode_plugins,
};

const STATE_DIRECTORY: &str = ".crowded";
const STATE_FILE: &str = ".crowded/toolbox-state.json";
const STATE_VERSION: u8 = 1;

#[derive(Deserialize, Serialize)]
struct ToolboxState {
    version: u8,
    files: Vec<ManagedFile>,
}

// JSON uses per-entry ownership because OpenCode rewrites its project file.
// ponytail: TOML keeps a whole-file snapshot until Codex rewrites it in practice.
#[derive(Deserialize, Serialize)]
struct ManagedFile {
    path: PathBuf,
    original: Option<String>,
    generated: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Vendor {
    Claude,
    Codex,
    OpenCode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeTarget {
    Mcp(Vendor),
    Hooks(Vendor),
}

enum Removal {
    AlreadyRestored,
    RestoreSnapshot,
    RewriteJson(String),
    Delete,
}

pub(crate) fn command() -> Result<(), Box<dyn std::error::Error>> {
    let action = env::args().nth(2).ok_or_else(|| {
        invalid_input("usage: crowded toolbox preview|sync|remove (no additional arguments)")
    })?;
    if env::args().nth(3).is_some() {
        return Err(invalid_input(
            "usage: crowded toolbox preview|sync|remove (no additional arguments)",
        )
        .into());
    }

    let root = env::current_dir()?;
    match action.as_str() {
        "preview" => preview(&root)?,
        "sync" => {
            let files = sync(&root)?;
            for path in &files {
                println!("synced {}", path.display());
            }
        }
        "remove" => {
            let restored = remove(&root)?;
            println!("removed the native toolbox from {restored} file(s)");
        }
        _ => {
            return Err(invalid_input(
                "usage: crowded toolbox preview|sync|remove (no additional arguments)",
            )
            .into());
        }
    }
    Ok(())
}

pub(crate) fn native_files_are_active() -> io::Result<bool> {
    native_files_are_active_at(&env::current_dir()?)
}

pub(crate) fn native_files_are_active_at(root: &Path) -> io::Result<bool> {
    let state_path = root.join(STATE_FILE);
    if !state_path.try_exists()? {
        return Ok(false);
    }

    let state = load_state(&state_path)?;
    // Stale state: crowded.toml now requires different native files (e.g. a new
    // room was added after the last sync). Treat as not active so the caller
    // falls back to env injection and `crowded init` can re-sync. `src/toolbox.rs:95`
    if let Ok(config) = load_room_file(&root.join("crowded.toml"))
        && let Ok(expected) =
            native_targets(root, &config.rooms, &config.mcp_servers, &config.opencode_plugins)
    {
        if state.files.len() != expected.len()
            || !state
                .files
                .iter()
                .all(|file| expected.contains_key(&file.path))
        {
            return Ok(false);
        }
    }
    for file in &state.files {
        let current = read_optional(&file.path)?;
        let exact = current.as_deref() == Some(&file.generated);
        let managed_json_is_intact = match current.as_deref() {
            Some(current) if is_opencode_config(&file.path) => {
                managed_opencode_matches(file, current)?
            }
            Some(current) => match json_section(&file.path) {
                Some(section) => managed_json_matches(file, current, section)?,
                None => false,
            },
            None => false,
        };
        if !exact && !managed_json_is_intact {
            return Err(invalid_data(format!(
                "{} changed Crowded-managed configuration after toolbox sync",
                file.path.display()
            )));
        }
    }
    Ok(true)
}

fn preview(root: &Path) -> io::Result<()> {
    let state = build_plan(root)?;
    for file in state.files {
        let action = if file.original.is_some() {
            "update"
        } else {
            "create"
        };
        println!("\n--- {action} {}", file.path.display());
        print!("{}", file.generated);
    }
    Ok(())
}

pub(crate) fn sync(root: &Path) -> io::Result<Vec<PathBuf>> {
    let state = build_plan(root)?;
    let staged = stage_files(&state.files)?;
    if let Err(error) = save_state(root, &state) {
        for (temporary, _) in &staged {
            let _ = fs::remove_file(temporary);
        }
        return Err(error);
    }

    for (index, (temporary, target)) in staged.iter().enumerate() {
        if let Err(error) = fs::rename(temporary, target) {
            for (remaining, _) in &staged[index..] {
                let _ = fs::remove_file(remaining);
            }
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "could not replace {}; toolbox state was preserved so `crowded toolbox remove` can recover: {error}",
                    target.display()
                ),
            ));
        }
    }

    Ok(state.files.into_iter().map(|file| file.path).collect())
}

fn stage_files(files: &[ManagedFile]) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut staged = Vec::with_capacity(files.len());
    for (index, file) in files.iter().enumerate() {
        let parent = file
            .path
            .parent()
            .ok_or_else(|| invalid_input(format!("{} has no parent", file.path.display())))?;
        fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(
            ".crowded-toolbox-{}-{index}.tmp",
            std::process::id()
        ));
        let result = private_file(&temporary)
            .and_then(|mut output| output.write_all(file.generated.as_bytes()))
            .and_then(|()| {
                if file.path.try_exists()? {
                    fs::set_permissions(&temporary, fs::metadata(&file.path)?.permissions())?;
                }
                Ok(())
            });
        if let Err(error) = result {
            for (temporary, _) in &staged {
                let _ = fs::remove_file(temporary);
            }
            return Err(error);
        }
        staged.push((temporary, file.path.clone()));
    }
    Ok(staged)
}

fn remove(root: &Path) -> io::Result<usize> {
    let state_path = root.join(STATE_FILE);
    if !state_path.try_exists()? {
        return Err(invalid_input("the native toolbox is not synced"));
    }
    let state = load_state(&state_path)?;

    let mut removals = Vec::with_capacity(state.files.len());
    for file in &state.files {
        let current = read_optional(&file.path)?;
        if current.as_deref() == Some(&file.generated) {
            removals.push(Removal::RestoreSnapshot);
        } else if current.as_deref() == file.original.as_deref() {
            // A failed sync may leave a target untouched; it is already restored.
            removals.push(Removal::AlreadyRestored);
        } else if let Some(current) = current.as_deref()
            && is_opencode_config(&file.path)
            && managed_opencode_matches(file, current)?
        {
            match remove_managed_opencode(file, current)? {
                Some(contents) => removals.push(Removal::RewriteJson(contents)),
                None => removals.push(Removal::Delete),
            }
        } else if let (Some(current), Some(section)) =
            (current.as_deref(), json_section(&file.path))
            && managed_json_matches(file, current, section)?
        {
            match remove_managed_json(file, current, section)? {
                Some(contents) => removals.push(Removal::RewriteJson(contents)),
                None => removals.push(Removal::Delete),
            }
        } else {
            return Err(invalid_data(format!(
                "{} changed Crowded-managed configuration; refusing to overwrite it",
                file.path.display()
            )));
        }
    }

    for (file, removal) in state.files.iter().zip(removals) {
        match removal {
            Removal::AlreadyRestored => {}
            Removal::RestoreSnapshot => {
                if let Some(original) = &file.original {
                    fs::write(&file.path, original)?;
                } else {
                    fs::remove_file(&file.path)?;
                    remove_empty_codex_directory(&file.path)?;
                }
            }
            Removal::RewriteJson(contents) => fs::write(&file.path, contents)?,
            Removal::Delete => fs::remove_file(&file.path)?,
        }
    }

    fs::remove_file(state_path)?;
    remove_empty_directory(&root.join(STATE_DIRECTORY))?;
    Ok(state.files.len())
}

fn json_section(path: &Path) -> Option<&'static str> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(".mcp.json") => Some("mcpServers"),
        Some("hooks.json" | "settings.local.json") => Some("hooks"),
        _ => None,
    }
}

fn is_opencode_config(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("opencode.json")
}

fn managed_opencode_matches(file: &ManagedFile, current: &str) -> io::Result<bool> {
    let current: Value = serde_json::from_str(current).map_err(invalid_json(&file.path))?;
    let current_mcp = current.get("mcp").and_then(Value::as_object);
    for (name, expected) in owned_json_entries(file, "mcp")? {
        if current_mcp.and_then(|entries| entries.get(&name)) != Some(&expected) {
            return Ok(false);
        }
    }
    let current_plugins = current.get("plugin").and_then(Value::as_array);
    for expected in owned_array_entries(file, "plugin")? {
        if !current_plugins.is_some_and(|plugins| plugins.contains(&expected)) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn remove_managed_opencode(file: &ManagedFile, current: &str) -> io::Result<Option<String>> {
    let mut current: Value = serde_json::from_str(current).map_err(invalid_json(&file.path))?;
    let original: Value = match file.original.as_deref() {
        Some(original) => serde_json::from_str(original).map_err(invalid_json(&file.path))?,
        None => serde_json::json!({}),
    };
    let root = current.as_object_mut().ok_or_else(|| {
        invalid_data(format!(
            "{} must contain a JSON object",
            file.path.display()
        ))
    })?;

    let owned_mcp = owned_json_entries(file, "mcp")?;
    if !owned_mcp.is_empty() {
        let entries = root
            .get_mut("mcp")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                invalid_data(format!("{} `mcp` must be an object", file.path.display()))
            })?;
        for (name, _) in owned_mcp {
            entries.remove(&name);
        }
        if entries.is_empty() && original.get("mcp").is_none() {
            root.remove("mcp");
        }
    }

    let owned_plugins = owned_array_entries(file, "plugin")?;
    if !owned_plugins.is_empty() {
        let plugins = root
            .get_mut("plugin")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| {
                invalid_data(format!("{} `plugin` must be an array", file.path.display()))
            })?;
        for owned in owned_plugins {
            let position = plugins
                .iter()
                .position(|plugin| plugin == &owned)
                .ok_or_else(|| {
                    invalid_data(format!(
                        "{} no longer contains a Crowded-managed OpenCode plugin",
                        file.path.display()
                    ))
                })?;
            plugins.remove(position);
        }
        if plugins.is_empty() && original.get("plugin").is_none() {
            root.remove("plugin");
        }
    }

    if root.is_empty() && file.original.is_none() {
        return Ok(None);
    }
    let mut output = serde_json::to_string_pretty(&current).map_err(invalid_json(&file.path))?;
    output.push('\n');
    Ok(Some(output))
}

fn managed_json_matches(file: &ManagedFile, current: &str, section: &str) -> io::Result<bool> {
    let current: Value = serde_json::from_str(current).map_err(invalid_json(&file.path))?;
    let current_entries = current.get(section).and_then(Value::as_object);
    if section == "hooks" {
        for (event, expected) in owned_hook_entries(file)? {
            let contains = current_entries
                .and_then(|entries| entries.get(&event))
                .and_then(Value::as_array)
                .is_some_and(|handlers| handlers.contains(&expected));
            if !contains {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    for (name, expected) in owned_json_entries(file, section)? {
        if current_entries.and_then(|entries| entries.get(&name)) != Some(&expected) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn remove_managed_json(
    file: &ManagedFile,
    current: &str,
    section: &str,
) -> io::Result<Option<String>> {
    let mut current: Value = serde_json::from_str(current).map_err(invalid_json(&file.path))?;
    let original_had_section = match file.original.as_deref() {
        Some(original) => serde_json::from_str::<Value>(original)
            .map_err(invalid_json(&file.path))?
            .get(section)
            .is_some(),
        None => false,
    };
    let root = current.as_object_mut().ok_or_else(|| {
        invalid_data(format!(
            "{} must contain a JSON object",
            file.path.display()
        ))
    })?;
    let entries = root
        .get_mut(section)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            invalid_data(format!(
                "{} `{section}` must contain a JSON object",
                file.path.display()
            ))
        })?;
    if section == "hooks" {
        let original: Value = match file.original.as_deref() {
            Some(original) => serde_json::from_str(original).map_err(invalid_json(&file.path))?,
            None => serde_json::json!({}),
        };
        let original_hooks = original.get(section).and_then(Value::as_object);
        for (event, owned) in owned_hook_entries(file)? {
            let handlers = entries
                .get_mut(&event)
                .and_then(Value::as_array_mut)
                .ok_or_else(|| {
                    invalid_data(format!(
                        "{} `hooks.{event}` must contain an array",
                        file.path.display()
                    ))
                })?;
            let position = handlers
                .iter()
                .position(|handler| handler == &owned)
                .ok_or_else(|| {
                    invalid_data(format!(
                        "{} no longer contains a Crowded-managed `{event}` hook",
                        file.path.display()
                    ))
                })?;
            handlers.remove(position);
            let original_had_event = original_hooks.is_some_and(|hooks| hooks.contains_key(&event));
            if handlers.is_empty() && !original_had_event {
                entries.remove(&event);
            }
        }
    } else {
        for (name, _) in owned_json_entries(file, section)? {
            entries.remove(&name);
        }
    }
    if entries.is_empty() && !original_had_section {
        root.remove(section);
    }
    if root.is_empty() && file.original.is_none() {
        return Ok(None);
    }

    let mut output = serde_json::to_string_pretty(&current).map_err(invalid_json(&file.path))?;
    output.push('\n');
    Ok(Some(output))
}

fn owned_hook_entries(file: &ManagedFile) -> io::Result<Vec<(String, Value)>> {
    let generated: Value =
        serde_json::from_str(&file.generated).map_err(invalid_json(&file.path))?;
    let generated = generated
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            invalid_data(format!(
                "{} generated `hooks` is not a JSON object",
                file.path.display()
            ))
        })?;
    let original: Value = match file.original.as_deref() {
        Some(original) => serde_json::from_str(original).map_err(invalid_json(&file.path))?,
        None => serde_json::json!({}),
    };
    let original = original.get("hooks").and_then(Value::as_object);
    let mut owned = Vec::new();
    for (event, handlers) in generated {
        let handlers = handlers.as_array().ok_or_else(|| {
            invalid_data(format!(
                "{} generated `hooks.{event}` is not an array",
                file.path.display()
            ))
        })?;
        let original_handlers = original
            .and_then(|hooks| hooks.get(event))
            .and_then(Value::as_array);
        for handler in handlers {
            if original_handlers.is_none_or(|original| !original.contains(handler)) {
                owned.push((event.clone(), handler.clone()));
            }
        }
    }
    Ok(owned)
}

fn owned_json_entries(file: &ManagedFile, section: &str) -> io::Result<Vec<(String, Value)>> {
    let generated: Value =
        serde_json::from_str(&file.generated).map_err(invalid_json(&file.path))?;
    let generated = match generated.get(section) {
        None => return Ok(Vec::new()),
        Some(generated) => generated.as_object().ok_or_else(|| {
            invalid_data(format!(
                "{} generated `{section}` is not a JSON object",
                file.path.display()
            ))
        })?,
    };
    let original: Value = match file.original.as_deref() {
        Some(original) => serde_json::from_str(original).map_err(invalid_json(&file.path))?,
        None => serde_json::json!({}),
    };
    let original = original.get(section).and_then(Value::as_object);

    Ok(generated
        .iter()
        .filter(|(name, _)| original.is_none_or(|entries| !entries.contains_key(*name)))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect())
}

fn owned_array_entries(file: &ManagedFile, section: &str) -> io::Result<Vec<Value>> {
    let generated: Value =
        serde_json::from_str(&file.generated).map_err(invalid_json(&file.path))?;
    let generated = match generated.get(section) {
        None => return Ok(Vec::new()),
        Some(generated) => generated.as_array().ok_or_else(|| {
            invalid_data(format!(
                "{} generated `{section}` is not an array",
                file.path.display()
            ))
        })?,
    };
    let original: Value = match file.original.as_deref() {
        Some(original) => serde_json::from_str(original).map_err(invalid_json(&file.path))?,
        None => serde_json::json!({}),
    };
    let original = original.get(section).and_then(Value::as_array);
    Ok(generated
        .iter()
        .filter(|entry| original.is_none_or(|entries| !entries.contains(entry)))
        .cloned()
        .collect())
}

fn build_plan(root: &Path) -> io::Result<ToolboxState> {
    let state_path = root.join(STATE_FILE);
    let old_state = if state_path.try_exists()? {
        // Allow re-sync when the state is stale (e.g. rooms added after last sync).
        // Otherwise require explicit `toolbox remove` to avoid clobbering.
        let is_stale = (|| -> io::Result<bool> {
            let state = load_state(&state_path)?;
            let config = load_room_file(&root.join("crowded.toml"))?;
            let expected = native_targets(
                root,
                &config.rooms,
                &config.mcp_servers,
                &config.opencode_plugins,
            )?;
            Ok(state.files.len() != expected.len()
                || !state
                    .files
                    .iter()
                    .all(|file| expected.contains_key(&file.path)))
        })()
        .unwrap_or(false);
        if !is_stale {
            return Err(invalid_input(
                "the native toolbox is already synced; remove it before syncing again",
            ));
        }
        let old_state = load_state(&state_path)?;
        let config = load_room_file(&root.join("crowded.toml"))?;
        let expected = native_targets(
            root,
            &config.rooms,
            &config.mcp_servers,
            &config.opencode_plugins,
        )?;
        // Restore orphaned targets that fell out of the new expected set before
        // dropping the state file (same logic as remove()).
        for file in &old_state.files {
            if expected.contains_key(&file.path) {
                continue;
            }
            let current = read_optional(&file.path)?;
            let removal = if current.as_deref() == Some(&file.generated) {
                Removal::RestoreSnapshot
            } else if current.as_deref() == file.original.as_deref() {
                Removal::AlreadyRestored
            } else if let Some(current_str) = current.as_deref()
                && is_opencode_config(&file.path)
                && managed_opencode_matches(file, current_str)?
            {
                match remove_managed_opencode(file, current_str)? {
                    Some(contents) => Removal::RewriteJson(contents),
                    None => Removal::Delete,
                }
            } else if let (Some(current_str), Some(section)) =
                (current.as_deref(), json_section(&file.path))
                && managed_json_matches(file, current_str, section)?
            {
                match remove_managed_json(file, current_str, section)? {
                    Some(contents) => Removal::RewriteJson(contents),
                    None => Removal::Delete,
                }
            } else {
                return Err(invalid_data(format!(
                    "{} changed Crowded-managed configuration; refusing to overwrite it",
                    file.path.display()
                )));
            };
            match removal {
                Removal::AlreadyRestored => {}
                Removal::RestoreSnapshot => {
                    if let Some(original) = &file.original {
                        fs::write(&file.path, original)?;
                    } else {
                        fs::remove_file(&file.path)?;
                        remove_empty_codex_directory(&file.path)?;
                    }
                }
                Removal::RewriteJson(contents) => fs::write(&file.path, contents)?,
                Removal::Delete => {
                    fs::remove_file(&file.path)?;
                    remove_empty_codex_directory(&file.path)?;
                }
            }
        }
        // Stale: drop the old state so a fresh plan can be built.
        fs::remove_file(&state_path)?;
        let _ = remove_empty_directory(&root.join(STATE_DIRECTORY));
        Some(old_state)
    } else {
        None
    };

    let config = load_room_file(&root.join("crowded.toml"))?;
    if config.rooms.len() < 2 {
        return Err(invalid_input("crowded.toml needs at least two rooms"));
    }
    validate_mcp_servers(&config.mcp_servers)?;
    validate_opencode_plugins(&config.opencode_plugins)?;

    let targets = native_targets(
        root,
        &config.rooms,
        &config.mcp_servers,
        &config.opencode_plugins,
    )?;
    let mut files = Vec::with_capacity(targets.len());
    for (path, target) in targets {
        refuse_symlink(&path)?;
        if target == NativeTarget::Mcp(Vendor::OpenCode)
            && path.with_extension("jsonc").try_exists()?
        {
            return Err(invalid_input(format!(
                "{} exists; Crowded does not rewrite JSONC yet",
                path.with_extension("jsonc").display()
            )));
        }
        let original = if let Some(old) = &old_state {
            if let Some(prev) = old.files.iter().find(|f| f.path == path) {
                prev.original.clone()
            } else {
                read_optional(&path)?
            }
        } else {
            read_optional(&path)?
        };
        let generated = generate(
            target,
            original.as_deref(),
            &config.mcp_servers,
            &config.opencode_plugins,
            &path,
        )?;
        files.push(ManagedFile {
            path,
            original,
            generated,
        });
    }

    Ok(ToolboxState {
        version: STATE_VERSION,
        files,
    })
}

fn native_targets(
    root: &Path,
    rooms: &[RoomConfig],
    servers: &[McpConfig],
    opencode_plugins: &[OpenCodePluginConfig],
) -> io::Result<BTreeMap<PathBuf, NativeTarget>> {
    let mut targets = BTreeMap::new();
    for room in rooms {
        if room.transport == Transport::Shell {
            continue;
        }
        let directory = match &room.cwd {
            Some(cwd) if cwd.is_absolute() => cwd.clone(),
            Some(cwd) => root.join(cwd),
            None => root.to_path_buf(),
        };
        if !directory.is_dir() {
            return Err(invalid_input(format!(
                "room working directory does not exist: {}",
                directory.display()
            )));
        }

        let command = Path::new(&room.command)
            .file_name()
            .unwrap_or(room.command.as_ref())
            .to_string_lossy()
            .to_ascii_lowercase();
        let (vendor, mcp_path, hook_path) = match command.as_str() {
            "claude" => (
                Vendor::Claude,
                directory.join(".mcp.json"),
                directory.join(".claude").join("settings.local.json"),
            ),
            "codex" => (
                Vendor::Codex,
                directory.join(".codex").join("config.toml"),
                directory.join(".codex").join("hooks.json"),
            ),
            "opencode" => (
                Vendor::OpenCode,
                directory.join("opencode.json"),
                directory
                    .join(".opencode")
                    .join("plugins")
                    .join("crowded-pulse.js"),
            ),
            _ => {
                return Err(invalid_input(format!(
                    "{command} cannot receive native toolbox files yet; supported commands are claude, codex, and opencode"
                )));
            }
        };
        let client = match vendor {
            Vendor::Claude => McpClient::Claude,
            Vendor::Codex => McpClient::Codex,
            Vendor::OpenCode => McpClient::Opencode,
        };
        if servers.iter().any(|server| server.supports(client))
            || vendor == Vendor::OpenCode && !opencode_plugins.is_empty()
        {
            targets.insert(mcp_path, NativeTarget::Mcp(vendor));
        }
        targets.insert(hook_path, NativeTarget::Hooks(vendor));
    }
    Ok(targets)
}

fn generate(
    target: NativeTarget,
    original: Option<&str>,
    servers: &[McpConfig],
    opencode_plugins: &[OpenCodePluginConfig],
    path: &Path,
) -> io::Result<String> {
    match target {
        NativeTarget::Mcp(Vendor::Claude) => {
            merge_json(original, &claude_mcp_config(servers)?, "mcpServers", path)
        }
        NativeTarget::Mcp(Vendor::Codex) => merge_codex(original, servers, path),
        NativeTarget::Mcp(Vendor::OpenCode) => merge_opencode(
            original,
            &opencode_mcp_config(None, servers, opencode_plugins)?,
            path,
        ),
        NativeTarget::Hooks(Vendor::Claude) => merge_hooks(original, path, false),
        NativeTarget::Hooks(Vendor::Codex) => merge_hooks(original, path, true),
        NativeTarget::Hooks(Vendor::OpenCode) => {
            if original.is_some() {
                return Err(invalid_input(format!(
                    "{} already exists; Crowded will not replace a local plugin",
                    path.display()
                )));
            }
            Ok(OPENCODE_PULSE_PLUGIN.to_owned())
        }
    }
}

fn merge_hooks(original: Option<&str>, path: &Path, windows_command: bool) -> io::Result<String> {
    let mut document = parse_json_document(original, path)?;
    let root = document
        .as_object_mut()
        .ok_or_else(|| invalid_data(format!("{} must contain a JSON object", path.display())))?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            invalid_data(format!(
                "{} `hooks` must contain a JSON object",
                path.display()
            ))
        })?;

    for (event, state) in [
        ("SessionStart", "starting"),
        ("UserPromptSubmit", "thinking"),
        ("PreToolUse", "working"),
        ("Stop", "ready"),
        ("SessionEnd", "offline"),
    ] {
        let mut hook = serde_json::json!({
            "type": "command",
            "command": format!("\"$CROWDED_BIN\" pulse {state}"),
            "timeout": 3
        });
        if windows_command {
            hook["commandWindows"] = Value::String(format!("& \"$env:CROWDED_BIN\" pulse {state}"));
        }
        let entry = serde_json::json!({ "hooks": [hook] });
        let handlers = hooks
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                invalid_data(format!(
                    "{} `hooks.{event}` must contain an array",
                    path.display()
                ))
            })?;
        if handlers.contains(&entry) {
            return Err(invalid_input(format!(
                "{} already configures the Crowded `{event}` pulse",
                path.display()
            )));
        }
        handlers.push(entry);
    }

    let mut output = serde_json::to_string_pretty(&document).map_err(invalid_json(path))?;
    output.push('\n');
    Ok(output)
}

const OPENCODE_PULSE_PLUGIN: &str = r#"const pulse = async (state) => {
  const crowded = process.env.CROWDED_BIN
  if (!crowded) return
  await Bun.spawn([crowded, "pulse", state], {
    stdout: "ignore",
    stderr: "ignore",
  }).exited
}

export const CrowdedPulse = async () => {
  await pulse("starting")
  return {
    "chat.message": async () => pulse("thinking"),
    "tool.execute.before": async () => pulse("working"),
    event: async ({ event }) => {
      if (event.type === "session.idle") await pulse("ready")
      if (event.type === "session.error") await pulse("error")
      if (event.type === "session.deleted") await pulse("offline")
    },
  }
}
"#;

fn merge_json(
    original: Option<&str>,
    additions: &str,
    section: &str,
    path: &Path,
) -> io::Result<String> {
    let mut document = parse_json_document(original, path)?;
    let additions: Value = serde_json::from_str(additions).map_err(invalid_json(path))?;
    let new_entries = additions[section]
        .as_object()
        .expect("Crowded generates object-valued MCP sections");
    let root = document
        .as_object_mut()
        .ok_or_else(|| invalid_data(format!("{} must contain a JSON object", path.display())))?;
    let entries = root
        .entry(section)
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| {
            invalid_data(format!(
                "{} `{section}` must contain a JSON object",
                path.display()
            ))
        })?;

    for (name, value) in new_entries {
        if entries.contains_key(name) {
            return Err(invalid_input(format!(
                "{} already configures MCP `{name}`",
                path.display()
            )));
        }
        entries.insert(name.clone(), value.clone());
    }

    let mut output = serde_json::to_string_pretty(&document).map_err(invalid_json(path))?;
    output.push('\n');
    Ok(output)
}

fn merge_opencode(original: Option<&str>, additions: &str, path: &Path) -> io::Result<String> {
    let mut document = parse_json_document(original, path)?;
    let additions: Value = serde_json::from_str(additions).map_err(invalid_json(path))?;
    let root = document
        .as_object_mut()
        .ok_or_else(|| invalid_data(format!("{} must contain a JSON object", path.display())))?;

    if let Some(new_mcp) = additions.get("mcp").and_then(Value::as_object) {
        let mcp = root
            .entry("mcp")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                invalid_data(format!(
                    "{} `mcp` must contain a JSON object",
                    path.display()
                ))
            })?;
        for (name, value) in new_mcp {
            if mcp.contains_key(name) {
                return Err(invalid_input(format!(
                    "{} already configures MCP `{name}`",
                    path.display()
                )));
            }
            mcp.insert(name.clone(), value.clone());
        }
    }

    if let Some(new_plugins) = additions.get("plugin").and_then(Value::as_array) {
        let plugins = root
            .entry("plugin")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                invalid_data(format!("{} `plugin` must contain an array", path.display()))
            })?;
        for plugin in new_plugins {
            if !plugins.contains(plugin) {
                plugins.push(plugin.clone());
            }
        }
    }

    let mut output = serde_json::to_string_pretty(&document).map_err(invalid_json(path))?;
    output.push('\n');
    Ok(output)
}

fn parse_json_document(original: Option<&str>, path: &Path) -> io::Result<Value> {
    match original.filter(|text| !text.trim().is_empty()) {
        Some(text) => serde_json::from_str(text).map_err(invalid_json(path)),
        None => Ok(serde_json::json!({})),
    }
}

fn merge_codex(original: Option<&str>, servers: &[McpConfig], path: &Path) -> io::Result<String> {
    let original = original.unwrap_or_default();
    let document: toml::Value = if original.trim().is_empty() {
        toml::Value::Table(Default::default())
    } else {
        toml::from_str(original).map_err(invalid_toml(path))?
    };
    if let Some(existing) = document.get("mcp_servers") {
        let existing = existing.as_table().ok_or_else(|| {
            invalid_data(format!(
                "{} `mcp_servers` must contain a TOML table",
                path.display()
            ))
        })?;
        for server in servers
            .iter()
            .filter(|server| server.supports(McpClient::Codex))
        {
            if existing.contains_key(&server.name) {
                return Err(invalid_input(format!(
                    "{} already configures MCP `{}`",
                    path.display(),
                    server.name
                )));
            }
        }
    }

    let mut output = original.to_owned();
    if !output.is_empty() {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push('\n');
    }
    output.push_str("# Crowded Room Shared Toolbox\n");
    for server in servers
        .iter()
        .filter(|server| server.supports(McpClient::Codex))
    {
        output.push_str(&format!("[mcp_servers.{}]\n", server.name));
        if let Some(url) = server.url() {
            output.push_str(&format!("url = {}\n", toml::Value::String(url.to_owned())));
        } else {
            output.push_str(&format!(
                "command = {}\n",
                toml::Value::String(server.command().unwrap_or_default().to_owned())
            ));
            output.push_str(&format!(
                "args = {}\n",
                toml::Value::Array(
                    server
                        .args
                        .iter()
                        .cloned()
                        .map(toml::Value::String)
                        .collect()
                )
            ));
            if let Some(cwd) = &server.cwd {
                output.push_str(&format!(
                    "cwd = {}\n",
                    toml::Value::String(cwd.to_string_lossy().into_owned())
                ));
            }
        }
        output.push('\n');
    }
    toml::from_str::<toml::Value>(&output).map_err(invalid_toml(path))?;
    Ok(output)
}

fn load_state(path: &Path) -> io::Result<ToolboxState> {
    refuse_symlink(path)?;
    let state: ToolboxState =
        serde_json::from_str(&fs::read_to_string(path)?).map_err(invalid_json(path))?;
    if state.version != STATE_VERSION {
        return Err(invalid_data(format!(
            "{} uses unsupported toolbox state version {}",
            path.display(),
            state.version
        )));
    }
    Ok(state)
}

fn save_state(root: &Path, state: &ToolboxState) -> io::Result<()> {
    let directory = root.join(STATE_DIRECTORY);
    create_private_directory(&directory)?;
    let state_path = root.join(STATE_FILE);
    refuse_symlink(&state_path)?;
    let temporary = directory.join(format!("toolbox-state.{}.tmp", std::process::id()));
    let mut contents = serde_json::to_vec_pretty(state).map_err(invalid_json(&state_path))?;
    contents.push(b'\n');

    let mut file = private_file(&temporary)?;
    file.write_all(&contents)?;
    file.sync_all()?;
    if let Err(error) = fs::rename(&temporary, state_path) {
        let _ = fs::remove_file(temporary);
        return Err(error);
    }
    Ok(())
}

fn read_optional(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid_input(format!(
            "refusing to manage symbolic link {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_empty_codex_directory(path: &Path) -> io::Result<()> {
    if path.file_name().and_then(|name| name.to_str()) == Some("config.toml")
        && path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some(".codex")
        && let Some(parent) = path.parent()
    {
        remove_empty_directory(parent)?;
    }
    Ok(())
}

pub(crate) fn remove_empty_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
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

fn invalid_json(path: &Path) -> impl FnOnce(serde_json::Error) -> io::Error + '_ {
    move |error| invalid_data(format!("invalid JSON in {}: {error}", path.display()))
}

fn invalid_toml(path: &Path) -> impl FnOnce(toml::de::Error) -> io::Error + '_ {
    move |error| invalid_data(format!("invalid TOML in {}: {error}", path.display()))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn codex_native_config_serializes_streamable_http_url() {
        let config: crate::config::RoomFile = toml::from_str(
            r#"
                [[mcp]]
                name = "streamable"
                url = "https://example.com/mcp"
                transport = "http"

                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"
            "#,
        )
        .unwrap();
        validate_mcp_servers(&config.mcp_servers).unwrap();

        let generated = merge_codex(
            Some("model = \"gpt-5\"\n"),
            &config.mcp_servers,
            Path::new(".codex/config.toml"),
        )
        .unwrap();

        assert!(generated.contains("[mcp_servers.streamable]\n"));
        assert!(generated.contains("url = \"https://example.com/mcp\"\n"));
        assert!(!generated.contains("command ="));
        assert!(!generated.contains("args ="));
    }

    #[test]
    fn native_files_merge_and_restore_without_losing_existing_config() {
        let root = test_directory();
        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".mcp.json"), "{\n  \"keep\": true\n}\n").unwrap();
        fs::write(
            root.join(".claude/settings.local.json"),
            "{\n  \"keep\": true\n}\n",
        )
        .unwrap();
        fs::write(
            root.join(".codex/config.toml"),
            "# keep this comment\nmodel = \"gpt-5\"\n",
        )
        .unwrap();
        fs::write(
            root.join("opencode.json"),
            "{\n  \"model\": \"openai/gpt-5\"\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"

                [[rooms]]
                command = "/bin/zsh"
                transport = "shell"

                [[mcp]]
                name = "everything"
                command = "npx"
                args = ["-y", "@modelcontextprotocol/server-everything"]

                [[opencode_plugin]]
                package = "context-mode@1.0.169"
            "#,
        )
        .unwrap();

        let paths = sync(&root).unwrap();
        assert_eq!(paths.len(), 6);
        assert!(native_files_are_active_at(&root).unwrap());
        assert!(
            fs::read_to_string(root.join(".mcp.json"))
                .unwrap()
                .contains("\"keep\": true")
        );
        assert!(
            fs::read_to_string(root.join(".codex/config.toml"))
                .unwrap()
                .starts_with("# keep this comment")
        );
        assert!(
            fs::read_to_string(root.join("opencode.json"))
                .unwrap()
                .contains("\"model\": \"openai/gpt-5\"")
        );
        let opencode: Value =
            serde_json::from_str(&fs::read_to_string(root.join("opencode.json")).unwrap()).unwrap();
        assert_eq!(opencode["plugin"][0], "context-mode@1.0.169");
        let claude_hooks: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude_hooks["keep"], true);
        assert_eq!(
            claude_hooks["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "\"$CROWDED_BIN\" pulse working"
        );
        assert!(
            claude_hooks["hooks"]["PreToolUse"][0]["hooks"][0]
                .get("commandWindows")
                .is_none()
        );
        let mut rewritten_claude = claude_hooks;
        rewritten_claude["newSetting"] = Value::Bool(true);
        rewritten_claude["hooks"]["PreToolUse"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "hooks": [{"type": "command", "command": "external-hook"}]
            }));
        fs::write(
            root.join(".claude/settings.local.json"),
            serde_json::to_string(&rewritten_claude).unwrap(),
        )
        .unwrap();
        let codex_hooks: Value =
            serde_json::from_str(&fs::read_to_string(root.join(".codex/hooks.json")).unwrap())
                .unwrap();
        assert_eq!(
            codex_hooks["hooks"]["PreToolUse"][0]["hooks"][0]["commandWindows"],
            "& \"$env:CROWDED_BIN\" pulse working"
        );
        assert!(
            fs::read_to_string(root.join(".opencode/plugins/crowded-pulse.js"))
                .unwrap()
                .contains("session.idle")
        );
        let mut rewritten: Value =
            serde_json::from_str(&fs::read_to_string(root.join("opencode.json")).unwrap()).unwrap();
        rewritten["$schema"] = Value::String("https://opencode.ai/config.json".into());
        fs::write(
            root.join("opencode.json"),
            serde_json::to_string(&rewritten).unwrap(),
        )
        .unwrap();
        assert!(native_files_are_active_at(&root).unwrap());

        remove(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".mcp.json")).unwrap(),
            "{\n  \"keep\": true\n}\n"
        );
        assert_eq!(
            fs::read_to_string(root.join(".codex/config.toml")).unwrap(),
            "# keep this comment\nmodel = \"gpt-5\"\n"
        );
        let claude: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["keep"], true);
        assert_eq!(claude["newSetting"], true);
        assert_eq!(
            claude["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "external-hook"
        );
        assert!(!root.join(".codex/hooks.json").exists());
        assert!(!root.join(".opencode/plugins/crowded-pulse.js").exists());
        let opencode: Value =
            serde_json::from_str(&fs::read_to_string(root.join("opencode.json")).unwrap()).unwrap();
        assert_eq!(opencode["model"], "openai/gpt-5");
        assert_eq!(opencode["$schema"], "https://opencode.ai/config.json");
        assert!(opencode.get("mcp").is_none());
        assert!(opencode.get("plugin").is_none());
        assert!(!root.join(STATE_FILE).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_toolbox_can_sync_hooks_without_mcps() {
        let root = test_directory();
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "/bin/zsh"
                transport = "shell"
            "#,
        )
        .unwrap();

        assert_eq!(
            sync(&root).unwrap(),
            [root.join(".claude/settings.local.json")]
        );
        remove(&root).unwrap();
        assert!(!root.join(".claude/settings.local.json").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn removal_refuses_to_overwrite_a_changed_generated_file() {
        let root = test_directory();
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"

                [[mcp]]
                name = "everything"
                command = "npx"
            "#,
        )
        .unwrap();

        sync(&root).unwrap();
        fs::write(root.join(".mcp.json"), "{}\n").unwrap();
        assert!(remove(&root).is_err());
        assert!(root.join(STATE_FILE).exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_resync_preserves_original_and_remove_still_cleans() {
        let root = test_directory();
        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".mcp.json"), "{\n  \"keep\": true\n}\n").unwrap();
        fs::write(root.join("opencode.json"), "{\n  \"model\": \"openai/gpt-5\"\n}\n").unwrap();
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"

                [[mcp]]
                name = "everything"
                command = "npx"
                args = ["-y", "@modelcontextprotocol/server-everything"]

                [[opencode_plugin]]
                package = "context-mode@1.0.169"
            "#,
        )
        .unwrap();

        // First sync with 2 rooms -> 4 files
        let first = sync(&root).unwrap();
        assert_eq!(first.len(), 4);
        assert!(native_files_are_active_at(&root).unwrap());

        // Add a new room -> stale state (4 vs 6 files)
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"

                [[mcp]]
                name = "everything"
                command = "npx"
                args = ["-y", "@modelcontextprotocol/server-everything"]

                [[opencode_plugin]]
                package = "context-mode@1.0.169"
            "#,
        )
        .unwrap();
        // stale sync must succeed and preserve original for surviving paths
        let second = sync(&root).unwrap();
        assert_eq!(second.len(), 6);
        let state: ToolboxState =
            serde_json::from_str(&fs::read_to_string(root.join(STATE_FILE)).unwrap()).unwrap();
        let mcp_file = state.files.iter().find(|f| f.path.ends_with(".mcp.json")).unwrap();
        assert_eq!(mcp_file.original.as_deref(), Some("{\n  \"keep\": true\n}\n"));
        let opencode_file = state.files.iter().find(|f| f.path.ends_with("opencode.json")).unwrap();
        assert_eq!(
            opencode_file.original.as_deref(),
            Some("{\n  \"model\": \"openai/gpt-5\"\n}\n")
        );

        // remove after stale resync must strip managed entries
        remove(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".mcp.json")).unwrap(),
            "{\n  \"keep\": true\n}\n"
        );
        let opencode_restored: Value =
            serde_json::from_str(&fs::read_to_string(root.join("opencode.json")).unwrap()).unwrap();
        assert_eq!(opencode_restored["model"], "openai/gpt-5");
        assert!(opencode_restored.get("mcp").is_none());
        assert!(opencode_restored.get("plugin").is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_resync_restores_orphaned_files_when_room_removed() {
        let root = test_directory();
        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::write(root.join(".mcp.json"), "{\n  \"keep\": true\n}\n").unwrap();
        fs::write(root.join(".codex/config.toml"), "model = \"gpt-5\"\n").unwrap();
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"

                [[mcp]]
                name = "everything"
                command = "npx"

                [[opencode_plugin]]
                package = "context-mode@1.0.169"
            "#,
        )
        .unwrap();

        let first = sync(&root).unwrap();
        assert_eq!(first.len(), 6);
        assert!(root.join(".codex/config.toml").exists());
        assert!(root.join(".codex/hooks.json").exists());

        // Remove codex room -> expected shrinks, orphans should be restored
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"

                [[mcp]]
                name = "everything"
                command = "npx"

                [[opencode_plugin]]
                package = "context-mode@1.0.169"
            "#,
        )
        .unwrap();

        let second = sync(&root).unwrap();
        assert_eq!(second.len(), 4);
        // orphaned files must have been restored/deleted during stale handling
        assert_eq!(fs::read_to_string(root.join(".codex/config.toml")).unwrap(), "model = \"gpt-5\"\n");
        assert!(!root.join(".codex/hooks.json").exists());
        // surviving files still active
        assert!(native_files_are_active_at(&root).unwrap());

        remove(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    fn test_directory() -> PathBuf {
        static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "crowded-toolbox-{}-{nonce}-{id}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}
