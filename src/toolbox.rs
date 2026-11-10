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
    /// Crowded's own OpenCode TUI plugin directory entry. Tracked per file so
    /// the existing refuse-to-overwrite, remove and resync handling applies.
    OpenCodeTui,
}

enum Removal {
    AlreadyRestored,
    RestoreSnapshot,
    RewriteJson(String),
    Delete,
}

pub(crate) fn command() -> Result<(), Box<dyn std::error::Error>> {
    let action = env::args().nth(2).ok_or_else(|| {
        invalid_input("usage: crowded toolbox preview|sync|resync|remove (no additional arguments)")
    })?;
    if env::args().nth(3).is_some() {
        return Err(invalid_input(
            "usage: crowded toolbox preview|sync|resync|remove (no additional arguments)",
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
        "resync" => {
            let files = resync(&root)?;
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
                "usage: crowded toolbox preview|sync|resync|remove (no additional arguments)",
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
        && let Ok(expected) = native_targets(
            root,
            &config.rooms,
            &config.mcp_servers,
            &config.opencode_plugins,
        )
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
        if exact && is_legacy_opencode_plugin(&file.path, &file.generated) {
            return Ok(false);
        }
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

fn is_legacy_opencode_plugin(path: &Path, contents: &str) -> bool {
    path.ends_with(Path::new(".opencode/plugins/crowded-pulse.js"))
        && contents.contains("export const CrowdedPulse")
        && contents.contains("\"chat.message\"")
}

fn preview(root: &Path) -> io::Result<()> {
    if let Some((updates, removals)) = stale_preview_targets(root)? {
        println!(
            "saved toolbox state is out of date; `crowded toolbox sync` would reconcile these target files:"
        );
        for path in updates {
            println!("update {}", path.display());
        }
        for path in removals {
            println!("restore/remove {}", path.display());
        }
        return Ok(());
    }
    let state = build_plan(root, false)?;
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

fn stale_preview_targets(root: &Path) -> io::Result<Option<(Vec<PathBuf>, Vec<PathBuf>)>> {
    let state_path = root.join(STATE_FILE);
    if !state_path.try_exists()? {
        return Ok(None);
    }
    let state = load_state(&state_path)?;
    let config = load_room_file(&root.join("crowded.toml"))?;
    let expected = native_targets(
        root,
        &config.rooms,
        &config.mcp_servers,
        &config.opencode_plugins,
    )?;
    let mut stale = state.files.len() != expected.len()
        || !state
            .files
            .iter()
            .all(|file| expected.contains_key(&file.path));
    if !stale {
        for file in &state.files {
            if expected.contains_key(&file.path)
                && is_legacy_opencode_plugin(&file.path, &file.generated)
                && read_optional(&file.path)?.as_deref() == Some(&file.generated)
            {
                stale = true;
                break;
            }
        }
    }
    if !stale {
        return Ok(None);
    }

    let updates = expected.keys().cloned().collect();
    let removals = state
        .files
        .iter()
        .filter(|file| !expected.contains_key(&file.path))
        .map(|file| file.path.clone())
        .collect();
    Ok(Some((updates, removals)))
}

pub(crate) fn sync(root: &Path) -> io::Result<Vec<PathBuf>> {
    sync_inner(root, false)
}

/// Force a re-sync of native toolbox files, bypassing the already-synced guard.
/// Used by mcp_cli after editing crowded.toml to ensure managed files reflect
/// the current MCP configuration even when the file *set* is unchanged.
pub(crate) fn resync(root: &Path) -> io::Result<Vec<PathBuf>> {
    sync_inner(root, true)
}

fn sync_inner(root: &Path, force: bool) -> io::Result<Vec<PathBuf>> {
    let state = build_plan(root, force)?;
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
                    remove_empty_opencode_plugin_directory(&file.path)?;
                }
            }
            Removal::RewriteJson(contents) => fs::write(&file.path, contents)?,
            Removal::Delete => {
                fs::remove_file(&file.path)?;
                remove_empty_opencode_plugin_directory(&file.path)?;
            }
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

fn build_plan(root: &Path, force: bool) -> io::Result<ToolboxState> {
    let state_path = root.join(STATE_FILE);
    let mut orphan_removals: Vec<(usize, Removal)> = Vec::new();
    let old_state = if state_path.try_exists()? {
        // Allow re-sync when the state is stale (e.g. rooms added after last sync).
        // Otherwise require explicit `toolbox remove` to avoid clobbering,
        // unless the caller explicitly forces a re-sync.
        let is_stale = (|| -> io::Result<bool> {
            let state = load_state(&state_path)?;
            let config = load_room_file(&root.join("crowded.toml"))?;
            let expected = native_targets(
                root,
                &config.rooms,
                &config.mcp_servers,
                &config.opencode_plugins,
            )?;
            if state.files.len() != expected.len()
                || !state
                    .files
                    .iter()
                    .all(|file| expected.contains_key(&file.path))
            {
                return Ok(true);
            }
            for file in &state.files {
                if expected.contains_key(&file.path)
                    && is_legacy_opencode_plugin(&file.path, &file.generated)
                    && read_optional(&file.path)?.as_deref() == Some(&file.generated)
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })()
        .unwrap_or(false);
        if !is_stale && !force {
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
        // Orphaned targets that fell out of the new expected set are restored
        // like remove() does, but only after the whole new plan is known to be
        // acceptable: a refusal must leave every file and the state untouched.
        for (index, file) in old_state.files.iter().enumerate() {
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
            orphan_removals.push((index, removal));
        }
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
        let current = read_optional(&path)?;
        let previous = old_state
            .as_ref()
            .and_then(|state| state.files.iter().find(|file| file.path == path));
        let original = match previous {
            Some(previous) => previous.original.clone(),
            None => current.clone(),
        };
        // Replacing a file Crowded already wrote is only safe when it still
        // holds what Crowded wrote. Hook files are the exception: they keep
        // merge semantics, so Crowded rewrites only its own entries and the
        // user's hooks stay.
        let generated = match (previous, current.as_deref()) {
            (Some(previous), Some(current)) if current != previous.generated => {
                if json_section(&path) != Some("hooks") {
                    return Err(invalid_data(format!(
                        "{} changed Crowded-managed configuration; refusing to overwrite it",
                        path.display()
                    )));
                }
                let base = match remove_managed_json(previous, current, "hooks") {
                    Ok(base) => base,
                    // The user removed one of Crowded's entries, or the file
                    // is not the JSON Crowded wrote; merge into what is there
                    // and let the generator reject anything unusable.
                    Err(_) => Some(current.to_owned()),
                };
                generate(
                    target,
                    base.as_deref(),
                    &config.mcp_servers,
                    &config.opencode_plugins,
                    &path,
                )?
            }
            (Some(previous), _) => generate(
                target,
                previous.original.as_deref(),
                &config.mcp_servers,
                &config.opencode_plugins,
                &path,
            )?,
            (None, _) => generate(
                target,
                original.as_deref(),
                &config.mcp_servers,
                &config.opencode_plugins,
                &path,
            )?,
        };
        files.push(ManagedFile {
            path,
            original,
            generated,
        });
    }

    if let Some(old_state) = &old_state {
        for (index, removal) in orphan_removals {
            let file = &old_state.files[index];
            match removal {
                Removal::AlreadyRestored => {}
                Removal::RestoreSnapshot => {
                    if let Some(original) = &file.original {
                        fs::write(&file.path, original)?;
                    } else {
                        fs::remove_file(&file.path)?;
                        remove_empty_codex_directory(&file.path)?;
                        remove_empty_opencode_plugin_directory(&file.path)?;
                    }
                }
                Removal::RewriteJson(contents) => fs::write(&file.path, contents)?,
                Removal::Delete => {
                    fs::remove_file(&file.path)?;
                    remove_empty_codex_directory(&file.path)?;
                    remove_empty_opencode_plugin_directory(&file.path)?;
                }
            }
        }
        fs::remove_file(&state_path)?;
        let _ = remove_empty_directory(&root.join(STATE_DIRECTORY));
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
        if vendor == Vendor::OpenCode {
            targets.insert(
                directory
                    .join(".opencode")
                    .join("plugins")
                    .join("crowded-tui")
                    .join("tui.js"),
                NativeTarget::OpenCodeTui,
            );
        }
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
            &opencode_mcp_config(None, servers, opencode_plugins, None)?,
            path,
        ),
        NativeTarget::Hooks(Vendor::Claude) => merge_hooks(original, path, Vendor::Claude),
        NativeTarget::Hooks(Vendor::Codex) => merge_hooks(original, path, Vendor::Codex),
        NativeTarget::Hooks(Vendor::OpenCode) => {
            if original.is_some() {
                return Err(invalid_input(format!(
                    "{} already exists; Crowded will not replace a local plugin",
                    path.display()
                )));
            }
            Ok(OPENCODE_PULSE_PLUGIN.to_owned())
        }
        NativeTarget::OpenCodeTui => {
            if original.is_some() {
                return Err(invalid_input(format!(
                    "{} already exists; Crowded will not replace a local TUI plugin",
                    path.display()
                )));
            }
            Ok(OPENCODE_TUI_PLUGIN.to_owned())
        }
    }
}

fn merge_hooks(original: Option<&str>, path: &Path, vendor: Vendor) -> io::Result<String> {
    let windows_command = vendor == Vendor::Codex;
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
        ("SubagentStart", "working"),
        ("SubagentStop", "thinking"),
        // TaskCreate/TaskUpdate/TodoWrite are tool names, not Claude Code
        // hook lifecycle events, so they can't be installed as top-level
        // `hooks.<name>` keys (Claude Code silently ignores those with a
        // startup warning). Route them through PreToolUse matchers below.
        ("TaskCreate", "working"),
        ("TaskUpdate", "thinking"),
        ("TodoWrite", "thinking"),
    ] {
        // `--hook-stdin` lets the pulse read the active model straight off the
        // hook JSON payload when the vendor runtime supplies one (Codex always
        // does; Claude only on SessionStart), while degrading to a plain pulse
        // when the payload carries no model field.
        let mut hook = serde_json::json!({
            "type": "command",
            "command": format!("\"$CROWDED_BIN\" pulse {state} --hook-stdin"),
            "timeout": 3
        });
        if windows_command {
            hook["commandWindows"] =
                Value::String(format!("& \"$env:CROWDED_BIN\" pulse {state} --hook-stdin"));
        }
        let mut entry = serde_json::json!({ "hooks": [hook] });
        let target_event = match event {
            "TaskCreate" | "TaskUpdate" | "TodoWrite" => {
                entry["matcher"] = Value::String(event.to_owned());
                "PreToolUse"
            }
            _ => event,
        };
        let handlers = hooks
            .entry(target_event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                invalid_data(format!(
                    "{} `hooks.{target_event}` must contain an array",
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
        if vendor == Vendor::Codex && event == "SessionStart" {
            // A second SessionStart hook reports the new thread id to Crowded
            // (`crowded session-report` reads the session id from stdin). It
            // is a separate entry from the pulse so each can evolve alone;
            // like the pulse, it carries no matcher.
            let mut report = serde_json::json!({
                "type": "command",
                "command": "\"$CROWDED_BIN\" session-report",
                "timeout": 3
            });
            if windows_command {
                report["commandWindows"] =
                    Value::String("& \"$env:CROWDED_BIN\" session-report".to_owned());
            }
            let report = serde_json::json!({ "hooks": [report] });
            let handlers = hooks
                .entry("SessionStart")
                .or_insert_with(|| serde_json::json!([]))
                .as_array_mut()
                .ok_or_else(|| {
                    invalid_data(format!(
                        "{} `hooks.SessionStart` must contain an array",
                        path.display()
                    ))
                })?;
            if handlers.contains(&report) {
                return Err(invalid_input(format!(
                    "{} already configures the Crowded `{event}` session report",
                    path.display()
                )));
            }
            handlers.push(report);
        }
    }

    let mut output = serde_json::to_string_pretty(&document).map_err(invalid_json(path))?;
    output.push('\n');
    Ok(output)
}

const OPENCODE_PULSE_PLUGIN: &str = r#"export default {
  id: "crowded.pulse",
  async setup(ctx) {
    let sessionID
    let model
    const pulse = async (state, selected = model) => {
      if (selected) model = selected
      const command = process.env.CROWDED_BIN
      if (!command) return
      const args = [command, "pulse", state]
      if (model) args.push("--model", `${model.providerID}/${model.id}${model.variant ? `#${model.variant}` : ""}`)
      await Bun.spawn(args, { stdout: "ignore", stderr: "ignore" }).exited
    }
    await pulse("starting")
    await ctx.session.hook("prompt", async (event) => {
      sessionID = event.sessionID
      await pulse("thinking")
    })
    await ctx.session.hook("context", async (event) => {
      sessionID = event.sessionID
      await pulse("thinking", event.model)
    })
    await ctx.tool.hook("execute.before", (event) => {
      if (event.sessionID === sessionID) return pulse("working")
    })
    const controller = new AbortController()
    const events = (async () => {
      for await (const event of ctx.event.subscribe({ signal: controller.signal })) {
        if (event.data?.sessionID && !sessionID) {
          sessionID = event.data.sessionID
          model = event.data.model
        }
        if (event.data?.sessionID !== sessionID) continue
        if (event.type === "session.model.selected") await pulse("ready", event.data.model)
        if (event.type === "session.status" && event.data.status.type === "idle") await pulse("ready")
        if (event.type === "session.execution.succeeded") await pulse("ready")
        if (event.type === "session.execution.interrupted") await pulse(event.data.reason === "shutdown" ? "offline" : "ready")
        if (event.type === "session.execution.failed") await pulse("error")
        if (event.type === "session.deleted") await pulse("offline")
      }
    })()
    events.catch(() => { if (!controller.signal.aborted) return pulse("error") })
    return async () => {
      controller.abort()
      await events.catch(() => {})
      await pulse("offline")
    }
  },
}
"#;

/// Crowded's OpenCode TUI plugin. Auto-discovered from the
/// `.opencode/plugins/crowded-tui/` directory: the TUI loads the directory's
/// `tui.js` entry, no package.json or config key needed. It reports the room's
/// sessions to Crowded (`crowded session-report`), creating one after /clear
/// when auto-create is on. Never logs the CROWDED_TOKEN and never passes it
/// in an argument; every host call is guarded so the plugin cannot throw
/// into the TUI.
const OPENCODE_TUI_PLUGIN: &str = r#"export default {
  id: "crowded-tui",
  async setup(ctx) {
    const ui = ctx && ctx.ui;
    if (!ui || typeof ui.router?.current !== "function") {
      return;
    }
    const AUTOCREATE = process.env.CROWDED_TUI_AUTOCREATE !== "0";
    const POLL_MS = 300;
    // A `crowded session-report` child answers as soon as it has its argument.
    // One that outlives this bound is sent SIGTERM, then SIGKILL after the
    // grace period; the next report starts only once the child has exited, so
    // a late report can never overtake a newer one.
    const REPORT_TIMEOUT_MS = 5000;
    const KILL_GRACE_MS = 1000;
    // The id rule `crowded session-report` applies to its own input: a
    // non-empty string of at most 128 characters drawn from letters, digits,
    // `-`, `_` and `.`. Anything else is left for the human.
    const validId = (id) =>
      typeof id === "string" && id.length > 0 && id.length <= 128 && /^[A-Za-z0-9._-]+$/.test(id);
    const PENDING = { kind: "pending" };
    const sessionItem = (id) => ({ kind: "session", id });
    // `--ack` makes the exit status say whether Crowded accepted the report;
    // without it the command exits 0 whatever happened.
    const argsOf = (item) => (item.kind === "pending" ? ["--pending", "--ack"] : ["--session", item.id, "--ack"]);
    const last = (list) => list[list.length - 1];

    // ---- Reports: one child in flight, the rest coalesced in `need`. ----
    // `need` is what still has to reach Crowded, in the one order that is
    // always safe: the pending marker first, then the session. A pending
    // boundary is information a later session report does not replace, so only
    // an older session id is superseded by a newer one. A child that timed
    // out or exited non-zero has delivered nothing: its report is queued
    // again, after a wait that grows with the failures in a row. A pending
    // marker is never given up on and nothing that follows it is sent before it
    // is delivered, so Crowded can never rebind the room without having held
    // the mail of the interval in between.
    const RETRY_MS = [0, 0, 5000, 15000, 60000];
    const FAILED_AFTER = 3;
    const STUCK_AFTER_KILL_MS = 2000;
    const NOTHING = { pending: false, session: null };
    const IDLE_REPORT = { flight: null, need: NOTHING, failures: 0, waiting: false, noted: false, stuck: false };

    const startNext = (report) => {
      if (report.flight || report.waiting) {
        return { report, fx: [] };
      }
      const { pending, session } = report.need;
      if (!pending && !session) {
        return { report, fx: [] };
      }
      const item = pending ? PENDING : sessionItem(session);
      return {
        report: { ...report, flight: { item, stage: "run" }, need: pending ? { pending: false, session } : NOTHING },
        fx: [{ t: "spawn", item }, { t: "timer", name: "report", ms: REPORT_TIMEOUT_MS }],
      };
    };

    // A report equal to the one running needs no second child, but only while
    // that child has not timed out.
    const deliver = (report, item) => {
      const running = report.flight && report.flight.stage === "run" ? report.flight.item : null;
      let need;
      if (item.kind === "pending") {
        need = running && running.kind === "pending" ? NOTHING : { pending: true, session: null };
      } else if (!report.need.pending && running && running.kind === "session" && running.id === item.id) {
        need = NOTHING;
      } else {
        need = { pending: report.need.pending, session: item.id };
      }
      return startNext({ ...report, need });
    };

    const requeue = (need, item) => {
      if (item.kind === "pending") {
        return { pending: true, session: need.session };
      }
      return need.pending || need.session ? need : { pending: false, session: item.id };
    };

    // ---- The state machine. ----
    // phase: idle (no session seen yet), bound (session known to Crowded),
    // pending (between sessions), creating (clear seen, auto-create running),
    // disposed. Fields besides phase:
    //   session   the id Crowded was told last
    //   wantCreate  creating, still waiting for the pending report to finish
    //   call      true while a session.create call has not resolved (it can
    //             outlive the attempt)
    //   nav       creating, navigating to the session the call returned
    //   unused    a session this plugin created but did not show
    //   report    { flight, need, failures, waiting, noted, stuck }
    const INITIAL = {
      phase: "idle",
      session: null,
      wantCreate: null,
      call: null,
      nav: null,
      unused: null,
      report: IDLE_REPORT,
    };

    const adopt = (id) => ({ phase: "bound", session: id, wantCreate: null, nav: null, tell: sessionItem(id) });

    const leave = (s, e) => {
      const create = AUTOCREATE && !s.call;
      return { phase: create ? "creating" : "pending", wantCreate: create ? { dir: e.dir } : null, tell: PENDING };
    };

    const adoptRoute = (s, e) => adopt(e.id);
    const keepOrAdoptRoute = (s, e) => (e.id === s.session ? {} : adopt(e.id));

    // Only a session on screen is the human's choice. One created in the
    // background (a subagent, another client) waits until the route shows it.
    const onCreated = (s, e) => {
      if (!e.displayed || s.call) {
        return {};
      }
      if (e.id === s.unused || e.id === s.nav || (s.phase === "bound" && e.id === s.session)) {
        return {};
      }
      return adopt(e.id);
    };

    const onResolvedWhileCreating = (s, e) => {
      if (!s.call) {
        return {};
      }
      if (e.routeId) {
        // The human reached a session while the create ran: it wins.
        return { call: null, unused: e.id && e.routeId !== e.id ? e.id : s.unused, ...adopt(e.routeId) };
      }
      if (!e.id) {
        return { call: null, phase: "pending" };
      }
      return { call: null, nav: e.id, fx: [{ t: "navigate", id: e.id }] };
    };

    const onResolvedLate = (s, e) => (s.call ? { call: null, unused: e.id || s.unused } : {});

    const onReportExited = (s, e) => {
      const r = s.report;
      if (!r.flight) {
        return {};
      }
      const fx = [{ t: "clear", name: "report" }, { t: "clear", name: "grace" }, { t: "clear", name: "stuck" }];
      if (s.phase === "disposed") {
        return { report: IDLE_REPORT, fx };
      }
      const item = r.flight.item;
      const delivered = e.ok && r.flight.stage === "run";
      let patch = {};
      let report = { ...r, flight: null, stuck: false };
      if (delivered) {
        report.failures = 0;
        report.noted = false;
        if (item.kind === "pending" && s.phase === "creating" && s.wantCreate) {
          patch = { wantCreate: null, call: true };
          fx.push({ t: "create", dir: s.wantCreate.dir });
        }
      } else {
        report.failures = r.failures + 1;
        report.need = requeue(r.need, item);
        if (report.failures >= FAILED_AFTER && !report.noted) {
          report.noted = true;
          fx.push({ t: "failed" });
        }
        const wait = RETRY_MS[Math.min(report.failures, RETRY_MS.length) - 1];
        if (wait > 0) {
          report.waiting = true;
          fx.push({ t: "timer", name: "retry", ms: wait });
        }
      }
      const next = startNext(report);
      return { ...patch, report: next.report, fx: [...fx, ...next.fx] };
    };

    const onRetryDue = (s) => {
      if (!s.report.waiting) {
        return {};
      }
      return startNext({ ...s.report, waiting: false });
    };

    const onReportTimeout = (s) => {
      const flight = s.report.flight;
      if (!flight || flight.stage !== "run") {
        return {};
      }
      return {
        report: { ...s.report, flight: { ...flight, stage: "term" } },
        fx: [{ t: "kill", signal: "SIGTERM" }, { t: "timer", name: "grace", ms: KILL_GRACE_MS }],
      };
    };

    const onGraceExpired = (s) => {
      const flight = s.report.flight;
      if (!flight || flight.stage !== "term") {
        return {};
      }
      return {
        report: { ...s.report, flight: { ...flight, stage: "kill" } },
        fx: [{ t: "kill", signal: "SIGKILL" }, { t: "timer", name: "stuck", ms: STUCK_AFTER_KILL_MS }],
      };
    };

    // A child still alive after SIGKILL cannot be abandoned: a later report
    // started beside it could be overtaken by it. The queue waits and the
    // condition is said once on stderr.
    const onStuckCheck = (s) => {
      const flight = s.report.flight;
      if (!flight || flight.stage !== "kill" || s.report.stuck) {
        return {};
      }
      return { report: { ...s.report, stuck: true }, fx: [{ t: "stuck" }] };
    };

    const onDispose = (s) => {
      if (s.phase === "disposed") {
        return {};
      }
      return {
        phase: "disposed",
        wantCreate: null,
        call: null,
        nav: null,
        report: { ...s.report, need: NOTHING, waiting: false },
        fx: [...(s.report.flight ? [{ t: "kill", signal: "SIGKILL" }] : []), { t: "teardown" }],
      };
    };

    // phase -> event -> (state, event) -> { patch fields, tell, fx }
    const TABLE = {
      idle: {
        routeSession: adoptRoute,
        created: onCreated,
        createResolved: onResolvedLate,
      },
      bound: {
        routeHome: leave,
        routeSession: keepOrAdoptRoute,
        created: onCreated,
        createResolved: onResolvedLate,
      },
      pending: {
        routeSession: adoptRoute,
        created: onCreated,
        createResolved: onResolvedLate,
      },
      creating: {
        routeSession: adoptRoute,
        created: onCreated,
        createResolved: onResolvedWhileCreating,
        navDone: (s, e) => (s.nav === e.id ? adopt(e.id) : {}),
        navFailed: (s) => (s.nav ? { phase: "pending", unused: s.nav, nav: null } : {}),
      },
      disposed: {},
    };
    const COMMON = {
      reportExited: onReportExited,
      reportTimeout: onReportTimeout,
      graceExpired: onGraceExpired,
      stuckCheck: onStuckCheck,
      retryDue: onRetryDue,
      dispose: onDispose,
    };

    const step = (s, event) => {
      let e = event;
      if (e.type === "routeSession" || e.type === "created" || e.type === "navDone") {
        if (!validId(e.id)) {
          return { s, fx: [] };
        }
      } else if (e.type === "createResolved") {
        e = { ...e, id: validId(e.id) ? e.id : null, routeId: validId(e.routeId) ? e.routeId : null };
      }
      const handler = TABLE[s.phase][e.type] || COMMON[e.type];
      if (!handler) {
        return { s, fx: [] };
      }
      const { tell, fx = [], ...patch } = handler(s, e);
      let next = { ...s, ...patch };
      let sent = [];
      if (tell) {
        const delivery = deliver(next.report, tell);
        next = { ...next, report: delivery.report };
        sent = delivery.fx;
      }
      return { s: next, fx: [...sent, ...fx] };
    };

    // ---- The runner: the only place that touches the host. ----
    const safe = (fn) => {
      try {
        return { ok: true, value: fn() };
      } catch {
        return { ok: false };
      }
    };
    const route = () => {
      const r = safe(() => ui.router.current());
      return r.ok ? r.value : null;
    };
    const sessionIdOf = (r) => (r && r.type === "session" && typeof r.sessionID === "string" ? r.sessionID : null);
    const homeDirOf = (r) => (r && r.location && typeof r.location.directory === "string" ? r.location.directory : null);

    let state = INITIAL;
    let child = null;
    let pollTimer = null;
    let unsubscribe = null;
    const timers = new Map();
    const inbox = [];
    let draining = false;

    const dispatch = (event) => {
      inbox.push(event);
      if (draining) {
        return;
      }
      draining = true;
      try {
        while (inbox.length > 0) {
          const result = safe(() => step(state, inbox.shift()));
          if (!result.ok) {
            continue;
          }
          state = result.value.s;
          for (const effect of result.value.fx) {
            safe(() => perform(effect));
          }
        }
      } finally {
        draining = false;
      }
    };

    const TIMER_EVENTS = { report: "reportTimeout", grace: "graceExpired", stuck: "stuckCheck", retry: "retryDue" };
    const startTimer = (name, ms) => {
      clearTimer(name);
      timers.set(
        name,
        setTimeout(() => {
          timers.delete(name);
          dispatch({ type: TIMER_EVENTS[name] });
        }, ms),
      );
    };
    const clearTimer = (name) => {
      if (timers.has(name)) {
        clearTimeout(timers.get(name));
        timers.delete(name);
      }
    };

    const spawnReport = (item) => {
      const bin = process.env.CROWDED_BIN;
      if (!bin) {
        // Outside a Crowded room there is nobody to tell.
        dispatch({ type: "reportExited", ok: true });
        return;
      }
      let mine;
      try {
        mine = Bun.spawn([bin, "session-report", ...argsOf(item)], { stdout: "ignore", stderr: "ignore" });
      } catch {
        dispatch({ type: "reportExited", ok: false });
        return;
      }
      child = mine;
      const exited = (code) => {
        if (child === mine) {
          child = null;
          dispatch({ type: "reportExited", ok: code === 0 });
        }
      };
      try {
        mine.exited.then(exited, () => exited(-1));
      } catch {
        exited(-1);
      }
    };

    const createSession = (dir) => {
      const settle = (created) => {
        const data = created && created.data;
        const id = (created && (created.id || (data && (data.id || data.sessionID)) || created.sessionID)) || null;
        dispatch({ type: "createResolved", id, routeId: sessionIdOf(route()) });
      };
      let pending;
      try {
        pending = Promise.resolve(ctx.client.session.create({ location: { directory: dir } }));
      } catch {
        pending = Promise.reject(new Error("create failed"));
      }
      pending.then(settle, () => settle(null));
    };

    const navigateTo = (id) => {
      let result;
      try {
        result = ui.router.navigate({ type: "session", sessionID: id });
      } catch {
        dispatch({ type: "navFailed" });
        return;
      }
      if (result && typeof result.then === "function") {
        result.then(
          () => dispatch({ type: "navDone", id }),
          () => dispatch({ type: "navFailed" }),
        );
      } else {
        dispatch({ type: "navDone", id });
      }
    };

    const perform = (effect) => {
      switch (effect.t) {
        case "spawn":
          spawnReport(effect.item);
          break;
        case "kill":
          if (child) {
            safe(() => child.kill(effect.signal));
          }
          break;
        case "timer":
          startTimer(effect.name, effect.ms);
          break;
        case "clear":
          clearTimer(effect.name);
          break;
        case "create":
          createSession(effect.dir);
          break;
        case "navigate":
          navigateTo(effect.id);
          break;
        case "stuck":
          process.stderr.write("crowded-tui: a session-report child survived SIGKILL; reports wait for it to exit\n");
          break;
        case "failed":
          process.stderr.write("crowded-tui: Crowded has not accepted the session report; retrying, and nothing is created until it does\n");
          break;
        case "teardown":
          if (pollTimer) {
            clearInterval(pollTimer);
            pollTimer = null;
          }
          for (const name of [...timers.keys()]) {
            clearTimer(name);
          }
          if (unsubscribe) {
            safe(() => unsubscribe());
            unsubscribe = null;
          }
          break;
      }
    };

    let lastRoute = "";
    pollTimer = setInterval(() => {
      const r = route();
      if (!r || typeof r !== "object") {
        return;
      }
      const key = JSON.stringify(r);
      if (key === lastRoute) {
        return;
      }
      lastRoute = key;
      if (r.type === "home") {
        dispatch({ type: "routeHome", dir: homeDirOf(r) || process.cwd() });
        return;
      }
      const id = sessionIdOf(r);
      if (id) {
        dispatch({ type: "routeSession", id });
      }
    }, POLL_MS);
    safe(() => {
      const off = ctx.data.on("session.created", (event) => {
        safe(() => {
          const id = event && event.data && event.data.sessionID;
          dispatch({ type: "created", id, displayed: validId(id) && sessionIdOf(route()) === id });
        });
      });
      if (typeof off === "function") {
        unsubscribe = off;
      }
    });
    return () => dispatch({ type: "dispose" });
  },
};
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

/// After deleting Crowded's own `tui.js`, drop the `crowded-tui` directory
/// and then `plugins` itself when both are empty. Anything else inside stops
/// the cleanup, and `.opencode` itself is never touched.
fn remove_empty_opencode_plugin_directory(path: &Path) -> io::Result<()> {
    if path.file_name().and_then(|name| name.to_str()) != Some("tui.js") {
        return Ok(());
    }
    let Some(plugin_dir) = path.parent() else {
        return Ok(());
    };
    if plugin_dir.file_name().and_then(|name| name.to_str()) != Some("crowded-tui") {
        return Ok(());
    }
    let Some(plugins_dir) = plugin_dir.parent() else {
        return Ok(());
    };
    if plugins_dir.file_name().and_then(|name| name.to_str()) != Some("plugins") {
        return Ok(());
    }
    if plugin_dir
        .read_dir()
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        fs::remove_dir(plugin_dir)?;
    } else {
        return Ok(());
    }
    if plugins_dir
        .read_dir()
        .map(|mut entries| entries.next().is_none())
        .unwrap_or(false)
    {
        fs::remove_dir(plugins_dir)?;
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
    const LEGACY_OPENCODE_PULSE_PLUGIN: &str = r###"const pulse = async (state, model) => {
  const crowded = process.env.CROWDED_BIN
  if (!crowded) return
  const args = [crowded, "pulse", state]
  if (model) args.push("--model", model)
  await Bun.spawn(args, {
    stdout: "ignore",
    stderr: "ignore",
  }).exited
}

let currentModel = null

export const CrowdedPulse = async () => {
  await pulse("starting")
  return {
    "chat.message": async (input) => {
      const model = input.model
      currentModel = model ? `${model.providerID}/${model.modelID}` : null
      await pulse("thinking", currentModel)
    },
    "tool.execute.before": async () => pulse("working", currentModel),
    event: async ({ event }) => {
      if (event.type === "session.idle") await pulse("ready", currentModel)
      if (event.type === "session.error") await pulse("error", currentModel)
      if (event.type === "session.deleted") await pulse("offline", currentModel)
    },
  }
}
"###;

    use std::{
        collections::BTreeMap,
        hash::{Hash, Hasher},
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
        assert_eq!(paths.len(), 7);
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
            "\"$CROWDED_BIN\" pulse working --hook-stdin"
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
            "& \"$env:CROWDED_BIN\" pulse working --hook-stdin"
        );
        let codex_session_starts = codex_hooks["hooks"]["SessionStart"]
            .as_array()
            .expect("codex SessionStart hooks");
        assert_eq!(codex_session_starts.len(), 2);
        assert!(codex_session_starts.iter().any(|entry| {
            entry["hooks"][0]["command"] == "\"$CROWDED_BIN\" pulse starting --hook-stdin"
        }));
        let report_entry = codex_session_starts
            .iter()
            .find(|entry| entry["hooks"][0]["command"] == "\"$CROWDED_BIN\" session-report")
            .expect("codex SessionStart session-report");
        assert!(report_entry.get("matcher").is_none());
        assert_eq!(
            report_entry["hooks"][0]["commandWindows"],
            "& \"$env:CROWDED_BIN\" session-report"
        );
        assert_eq!(report_entry["hooks"][0]["timeout"], 3);
        let claude_hooks_again: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert!(
            !serde_json::to_string(&claude_hooks_again["hooks"]).unwrap().contains("session-report"),
            "Claude hooks must not gain a session report"
        );
        let pulse_plugin =
            fs::read_to_string(root.join(".opencode/plugins/crowded-pulse.js")).unwrap();
        assert!(pulse_plugin.contains("ctx.session.hook(\"prompt\""));
        assert!(pulse_plugin.contains("session.execution.succeeded"));
        assert!(pulse_plugin.contains("model.variant"));
        let tui_plugin =
            fs::read_to_string(root.join(".opencode/plugins/crowded-tui/tui.js")).unwrap();
        assert!(tui_plugin.contains("id: \"crowded-tui\""));
        assert!(tui_plugin.contains("ui.router.current"));
        assert!(tui_plugin.contains("ctx.client.session.create"));
        assert!(tui_plugin.contains("session-report"));
        assert!(tui_plugin.contains("\"--pending\""));
        assert!(tui_plugin.contains("\"--session\""));
        assert!(tui_plugin.contains("CROWDED_TUI_AUTOCREATE"));
        assert!(tui_plugin.contains("CROWDED_BIN"));
        assert!(!tui_plugin.contains("CROWDED_TOKEN"));
        assert!(!tui_plugin.contains("commandWindows"));
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
        assert!(!root.join(".opencode/plugins/crowded-tui/tui.js").exists());
        assert!(!root.join(".opencode/plugins/crowded-tui").exists());
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
    fn opencode_tui_plugin_sync_is_idempotent_and_refuses_foreign_files() {
        let root = test_directory();
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "codex"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"
            "#,
        )
        .unwrap();

        sync(&root).unwrap();
        let first = fs::read(root.join(".opencode/plugins/crowded-tui/tui.js")).unwrap();
        resync(&root).unwrap();
        let second = fs::read(root.join(".opencode/plugins/crowded-tui/tui.js")).unwrap();
        assert_eq!(first, second, "a second sync must be byte-identical");
        assert!(native_files_are_active_at(&root).unwrap());

        // A resync keeps exactly one copy of each SessionStart entry.
        let hooks: Value =
            serde_json::from_str(&fs::read_to_string(root.join(".codex/hooks.json")).unwrap())
                .unwrap();
        let starts = hooks["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(starts.len(), 2);
        assert_eq!(
            starts
                .iter()
                .filter(|entry| entry["hooks"][0]["command"]
                    == "\"$CROWDED_BIN\" session-report")
                .count(),
            1
        );

        // A user-owned file at the same path is never overwritten.
        let foreign = test_directory();
        fs::write(
            foreign.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "codex"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"
            "#,
        )
        .unwrap();
        fs::create_dir_all(foreign.join(".opencode/plugins/crowded-tui")).unwrap();
        fs::write(
            foreign.join(".opencode/plugins/crowded-tui/tui.js"),
            "// mine\n",
        )
        .unwrap();
        assert!(sync(&foreign).is_err());
        assert_eq!(
            fs::read_to_string(foreign.join(".opencode/plugins/crowded-tui/tui.js")).unwrap(),
            "// mine\n"
        );

        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(foreign).unwrap();
    }

    #[test]
    fn removing_a_changed_tui_plugin_keeps_everything() {
        let root = test_directory();
        fs::write(
            root.join("crowded.toml"),
            r#"
                [[rooms]]
                command = "codex"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"
            "#,
        )
        .unwrap();

        sync(&root).unwrap();
        fs::write(
            root.join(".opencode/plugins/crowded-tui/tui.js"),
            "// touched\n",
        )
        .unwrap();
        assert!(remove(&root).is_err());
        assert!(root.join(".opencode/plugins/crowded-tui/tui.js").exists());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_tui_plugin_passes_node_checks_when_node_is_present() {
        use std::process::Command;
        if Command::new("node").arg("--version").output().is_err() {
            return;
        }
        let root = test_directory();
        let plugin = root.join("crowded-tui.mjs");
        let generated = generate(
            NativeTarget::OpenCodeTui,
            None,
            &[],
            &[],
            Path::new("tui.js"),
        )
        .unwrap();
        fs::write(&plugin, generated).unwrap();
        let check = Command::new("node")
            .arg("--check")
            .arg(&plugin)
            .output()
            .unwrap();
        assert!(
            check.status.success(),
            "node --check failed: {}",
            String::from_utf8_lossy(&check.stderr)
        );
        let suite = Command::new("node")
            .arg("--test")
            .arg("--test-reporter=spec")
            .arg(format!(
                "{}/tests/tui_session_report.mjs",
                env!("CARGO_MANIFEST_DIR")
            ))
            .env("TUI_UNDER_TEST", &plugin)
            .output()
            .unwrap();
        assert!(
            suite.status.success(),
            "node --test failed:\n{}\n{}",
            String::from_utf8_lossy(&suite.stdout),
            String::from_utf8_lossy(&suite.stderr)
        );
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
    fn toolbox_sync_upgrades_the_legacy_opencode_pulse_plugin_in_place() {
        let root = test_directory();
        fs::write(
            root.join("crowded.toml"),
            "[[rooms]]\ncommand = \"opencode\"\ntransport = \"raw\"\n\n[[rooms]]\ncommand = \"/bin/zsh\"\ntransport = \"shell\"\n",
        )
        .unwrap();
        let mut state = build_plan(&root, false).unwrap();
        let plugin_path = root.join(".opencode/plugins/crowded-pulse.js");
        let plugin_file = state
            .files
            .iter_mut()
            .find(|file| file.path == plugin_path)
            .unwrap();
        plugin_file.generated = LEGACY_OPENCODE_PULSE_PLUGIN.to_owned();
        for file in &state.files {
            fs::create_dir_all(file.path.parent().unwrap()).unwrap();
            fs::write(&file.path, &file.generated).unwrap();
        }
        save_state(&root, &state).unwrap();

        assert!(!native_files_are_active_at(&root).unwrap());
        assert!(sync(&root).unwrap().contains(&plugin_path));
        let installed = fs::read_to_string(&plugin_path).unwrap();
        assert_eq!(installed, OPENCODE_PULSE_PLUGIN);
        assert!(installed.starts_with("export default {\n  id: \"crowded.pulse\""));
        assert!(!installed.contains("chat.message"));

        fs::write(&plugin_path, "hand-edited plugin\n").unwrap();
        assert!(sync(&root).is_err());
        assert_eq!(
            fs::read_to_string(&plugin_path).unwrap(),
            "hand-edited plugin\n"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tool_name_events_route_through_pretooluse_matchers() {
        // TaskCreate/TaskUpdate/TodoWrite are tool names, not hook lifecycle
        // events; Claude Code silently ignores unknown top-level `hooks.<name>`
        // keys, so these must be installed as PreToolUse matchers instead.
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
            "#,
        )
        .unwrap();

        sync(&root).unwrap();
        let settings: Value = serde_json::from_str(
            &fs::read_to_string(root.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        let hooks = settings["hooks"].as_object().unwrap();

        for event in ["TaskCreate", "TaskUpdate", "TodoWrite"] {
            assert!(
                !hooks.contains_key(event),
                "`{event}` must not be installed as a top-level hook event"
            );
        }

        let matchers: Vec<&str> = hooks["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["matcher"].as_str())
            .collect();
        for event in ["TaskCreate", "TaskUpdate", "TodoWrite"] {
            assert!(
                matchers.contains(&event),
                "expected a PreToolUse matcher for `{event}`"
            );
        }

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

        // First sync with 2 rooms -> 5 files
        let first = sync(&root).unwrap();
        assert_eq!(first.len(), 5);
        assert!(native_files_are_active_at(&root).unwrap());

        // Add a new room -> stale state (5 vs 7 files)
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
        assert_eq!(second.len(), 7);
        let state: ToolboxState =
            serde_json::from_str(&fs::read_to_string(root.join(STATE_FILE)).unwrap()).unwrap();
        let mcp_file = state
            .files
            .iter()
            .find(|f| f.path.ends_with(".mcp.json"))
            .unwrap();
        assert_eq!(
            mcp_file.original.as_deref(),
            Some("{\n  \"keep\": true\n}\n")
        );
        let opencode_file = state
            .files
            .iter()
            .find(|f| f.path.ends_with("opencode.json"))
            .unwrap();
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
        assert_eq!(first.len(), 7);
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
        assert_eq!(second.len(), 5);
        // orphaned files must have been restored/deleted during stale handling
        assert_eq!(
            fs::read_to_string(root.join(".codex/config.toml")).unwrap(),
            "model = \"gpt-5\"\n"
        );
        assert!(!root.join(".codex/hooks.json").exists());
        // surviving files still active
        assert!(native_files_are_active_at(&root).unwrap());

        remove(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resync_refreshes_managed_files_without_removing() {
        let root = test_directory();
        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".mcp.json"), "{\n  \"keep\": true\n}\n").unwrap();
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
                name = "first"
                command = "npx"
            "#,
        )
        .unwrap();

        let first = sync(&root).unwrap();
        assert_eq!(first.len(), 4);
        let before: Value =
            serde_json::from_str(&fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
        assert!(before["mcpServers"].get("first").is_some());
        assert!(before["mcpServers"].get("second").is_none());

        // Same file set, so the state is not stale; plain sync refuses.
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
                name = "first"
                command = "npx"

                [[mcp]]
                name = "second"
                command = "npx"
            "#,
        )
        .unwrap();
        assert!(sync(&root).is_err());

        // resync refreshes the managed files in place, without a remove.
        let files = resync(&root).unwrap();
        assert_eq!(files.len(), 4);
        assert!(native_files_are_active_at(&root).unwrap());
        let after: Value =
            serde_json::from_str(&fs::read_to_string(root.join(".mcp.json")).unwrap()).unwrap();
        assert_eq!(after["keep"], true);
        assert!(after["mcpServers"].get("first").is_some());
        assert!(after["mcpServers"].get("second").is_some());

        // The original snapshot survives the refresh; remove still restores it.
        let state: ToolboxState =
            serde_json::from_str(&fs::read_to_string(root.join(STATE_FILE)).unwrap()).unwrap();
        let mcp_file = state
            .files
            .iter()
            .find(|file| file.path.ends_with(".mcp.json"))
            .unwrap();
        assert_eq!(
            mcp_file.original.as_deref(),
            Some("{\n  \"keep\": true\n}\n")
        );
        remove(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".mcp.json")).unwrap(),
            "{\n  \"keep\": true\n}\n"
        );
        assert!(!root.join(STATE_FILE).exists());

        fs::remove_dir_all(root).unwrap();
    }

    fn tree_hash(root: &Path) -> u64 {
        fn visit(root: &Path, path: &Path, files: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
            let mut entries = fs::read_dir(path)
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if entry.file_type().unwrap().is_dir() {
                    files.insert(relative, None);
                    visit(root, &path, files);
                } else {
                    files.insert(relative, Some(fs::read(path).unwrap()));
                }
            }
        }

        let mut files = BTreeMap::new();
        visit(root, root, &mut files);
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for (path, contents) in files {
            path.hash(&mut hash);
            contents.hash(&mut hash);
        }
        hash.finish()
    }

    fn write_preview_fixture(root: &Path) {
        fs::write(
            root.join("crowded.toml"),
            "[[rooms]]\ncommand = \"opencode\"\ntransport = \"raw\"\n\n[[rooms]]\ncommand = \"/bin/zsh\"\ntransport = \"shell\"\n",
        )
        .unwrap();
    }

    #[test]
    fn toolbox_preview_is_read_only_without_state() {
        let root = test_directory();
        write_preview_fixture(&root);
        let before = tree_hash(&root);
        preview(&root).unwrap();
        assert_eq!(tree_hash(&root), before);
        assert!(!root.join(STATE_FILE).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn toolbox_preview_is_read_only_with_current_state() {
        let root = test_directory();
        write_preview_fixture(&root);
        sync(&root).unwrap();
        let before = tree_hash(&root);
        let error = preview(&root).unwrap_err();
        assert!(error.to_string().contains("already synced"));
        assert_eq!(tree_hash(&root), before);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn toolbox_preview_is_read_only_with_stale_legacy_plugin_state() {
        let root = test_directory();
        write_preview_fixture(&root);
        let mut state = build_plan(&root, false).unwrap();
        let plugin_path = root.join(".opencode/plugins/crowded-pulse.js");
        state
            .files
            .iter_mut()
            .find(|file| file.path == plugin_path)
            .unwrap()
            .generated = LEGACY_OPENCODE_PULSE_PLUGIN.to_owned();
        for file in &state.files {
            fs::create_dir_all(file.path.parent().unwrap()).unwrap();
            fs::write(&file.path, &file.generated).unwrap();
        }
        save_state(&root, &state).unwrap();
        let (updates, removals) = stale_preview_targets(&root).unwrap().unwrap();
        assert!(updates.contains(&plugin_path));
        assert!(removals.is_empty());

        let before = tree_hash(&root);
        preview(&root).unwrap();
        assert_eq!(tree_hash(&root), before);
        assert!(root.join(STATE_FILE).exists());
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
