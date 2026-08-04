//! Vendor-adapter lifecycle for an installed plugin: planning, enabling and
//! disabling hooks/OpenCode links, and the persisted `.crowded-adapters.json`
//! state that tracks what Crowded manages so it can be rolled back cleanly.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::toolbox::remove_empty_directory;

use super::{
    INSTALLS_DIRECTORY, NativePluginManifest, invalid_data, invalid_input, load_record,
    validate_name,
};

pub(super) const ADAPTER_FILE: &str = ".crowded-adapters.json";
const ADAPTER_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
pub(super) struct AdapterState {
    version: u32,
    pub(super) links: Vec<AdapterLink>,
    pub(super) hooks: Vec<AdapterHooks>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct AdapterLink {
    path: PathBuf,
    target: PathBuf,
    #[serde(default)]
    copied: bool,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct AdapterHooks {
    path: PathBuf,
    entries: BTreeMap<String, Vec<Value>>,
    #[serde(default)]
    created: bool,
}

struct HookUpdate {
    managed: AdapterHooks,
    original: Option<String>,
    generated: Option<String>,
}

pub(super) fn preview_adapters(root: &Path, name: &str) -> io::Result<()> {
    let plan = adapter_plan(root, name)?;
    if plan.hooks.is_empty() && plan.links.is_empty() {
        println!("{name} has no supported vendor adapters");
        return Ok(());
    }
    for hooks in &plan.hooks {
        let count: usize = hooks.entries.values().map(Vec::len).sum();
        println!("hooks  {} ({count} handler(s))", hooks.path.display());
        for command in hook_commands(&hooks.entries) {
            println!("       {command}");
        }
    }
    for link in &plan.links {
        println!(
            "link   {} -> {}",
            link.path.display(),
            link.target.display()
        );
    }
    Ok(())
}

pub(super) fn enable_adapters(root: &Path, name: &str) -> io::Result<AdapterState> {
    validate_name("plugin", name)?;
    let installed = root.join(INSTALLS_DIRECTORY).join(name);
    let state_path = installed.join(ADAPTER_FILE);
    if state_path.try_exists()? {
        return Err(invalid_input(format!(
            "plugin `{name}` vendor adapters are already enabled"
        )));
    }

    let mut plan = adapter_plan(root, name)?;
    if plan.hooks.is_empty() && plan.links.is_empty() {
        return Err(invalid_input(format!(
            "plugin `{name}` has no supported vendor adapters"
        )));
    }
    for link in &plan.links {
        let path = root.join(&link.path);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(invalid_input(format!(
                    "{} already exists; refusing to replace it",
                    path.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let updates = plan
        .hooks
        .iter()
        .map(|hooks| prepare_hook_enable(root, hooks))
        .collect::<io::Result<Vec<_>>>()?;

    fs::create_dir_all(root.join(".crowded/plugin-data").join(name))?;
    let mut created_links = Vec::new();
    for link in &mut plan.links {
        let path = root.join(&link.path);
        let result = path
            .parent()
            .ok_or_else(|| invalid_input("adapter link has no parent"))
            .and_then(fs::create_dir_all)
            .and_then(|()| create_adapter_link(&path, &link.target));
        match result {
            Ok(copied) => link.copied = copied,
            Err(error) => {
                rollback_links(root, &created_links);
                return Err(error);
            }
        }
        created_links.push(link.clone());
    }

    for (applied, update) in updates.iter().enumerate() {
        if let Err(error) = apply_hook_update(root, update) {
            rollback_hooks(root, &updates[..applied]);
            rollback_links(root, &created_links);
            return Err(error);
        }
    }

    let state = AdapterState {
        version: ADAPTER_VERSION,
        links: plan.links,
        hooks: updates
            .iter()
            .map(|update| update.managed.clone())
            .collect(),
    };
    let contents = match serde_json::to_string_pretty(&state) {
        Ok(contents) => contents + "\n",
        Err(error) => {
            rollback_hooks(root, &updates);
            rollback_links(root, &created_links);
            return Err(io::Error::other(error));
        }
    };
    if let Err(error) = write_text_atomic(&state_path, &contents) {
        rollback_hooks(root, &updates);
        rollback_links(root, &created_links);
        return Err(error);
    }
    Ok(state)
}

pub(super) fn disable_adapters(root: &Path, name: &str) -> io::Result<AdapterState> {
    validate_name("plugin", name)?;
    let installed = root.join(INSTALLS_DIRECTORY).join(name);
    let state_path = installed.join(ADAPTER_FILE);
    let state: AdapterState = serde_json::from_str(&fs::read_to_string(&state_path)?)
        .map_err(|error| invalid_data(format!("invalid {}: {error}", state_path.display())))?;
    if state.version != ADAPTER_VERSION {
        return Err(invalid_data(format!(
            "{} uses unsupported adapter state version {}",
            state_path.display(),
            state.version
        )));
    }

    for link in &state.links {
        let path = root.join(&link.path);
        if link.copied {
            match fs::read(&path) {
                Ok(contents) if contents == fs::read(adapter_source(&path, &link.target)?)? => {}
                Ok(_) => {
                    return Err(invalid_data(format!(
                        "{} no longer matches the Crowded-managed adapter",
                        path.display()
                    )));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            continue;
        }
        match fs::read_link(&path) {
            Ok(target) if target == link.target => {}
            Ok(_) => {
                return Err(invalid_data(format!(
                    "{} no longer points to the Crowded-managed adapter",
                    path.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let updates = state
        .hooks
        .iter()
        .map(|hooks| prepare_hook_disable(root, hooks))
        .collect::<io::Result<Vec<_>>>()?;

    for (applied, update) in updates.iter().enumerate() {
        if let Err(error) = apply_hook_update(root, update) {
            rollback_hooks(root, &updates[..applied]);
            return Err(error);
        }
    }
    let mut removed_links = Vec::new();
    for link in &state.links {
        let path = root.join(&link.path);
        match fs::remove_file(&path) {
            Ok(()) => removed_links.push(link.clone()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                rollback_hooks(root, &updates);
                restore_links(root, &removed_links);
                return Err(error);
            }
        }
    }
    if let Err(error) = fs::remove_file(&state_path) {
        rollback_hooks(root, &updates);
        restore_links(root, &removed_links);
        return Err(error);
    }
    remove_empty_directory(&root.join(".opencode/plugins"))?;
    remove_empty_directory(&root.join(".opencode/command"))?;
    Ok(state)
}

pub(super) fn adapter_plan(root: &Path, name: &str) -> io::Result<AdapterState> {
    validate_name("plugin", name)?;
    let installed = root.join(INSTALLS_DIRECTORY).join(name);
    if fs::symlink_metadata(&installed)?.file_type().is_symlink() {
        return Err(invalid_data(format!(
            "{} must not be a symbolic link",
            installed.display()
        )));
    }
    let record = load_record(&installed)?;
    if record.name != name {
        return Err(invalid_data(format!(
            "{} belongs to plugin `{}`",
            installed.display(),
            record.name
        )));
    }

    let mut hooks = Vec::new();
    for (manifest_path, target, conventional_hooks) in [
        (
            ".claude-plugin/plugin.json",
            PathBuf::from(".claude/settings.local.json"),
            Some("hooks/hooks.json"),
        ),
        (
            ".codex-plugin/plugin.json",
            PathBuf::from(".codex/hooks.json"),
            None,
        ),
    ] {
        let manifest_path = installed.join(manifest_path);
        if !manifest_path.try_exists()? {
            continue;
        }
        let manifest = read_native_manifest(&manifest_path)?;
        if manifest.name != name {
            return Err(invalid_data(format!(
                "{} belongs to plugin `{}`",
                manifest_path.display(),
                manifest.name
            )));
        }
        let hook_path = manifest.hooks.as_deref().or(conventional_hooks);
        if let Some(hook_path) = hook_path
            && installed.join(hook_path).try_exists()?
        {
            let hook_path = safe_plugin_file(&installed, hook_path)?;
            let windows_command = target == Path::new(".codex/hooks.json");
            hooks.push(AdapterHooks {
                path: target,
                entries: adapted_hooks(root, name, &hook_path, windows_command)?,
                created: false,
            });
        }
    }

    let mut links = Vec::new();
    collect_adapter_links(name, &installed, ".opencode/plugins", &mut links)?;
    collect_adapter_links(name, &installed, ".opencode/command", &mut links)?;
    Ok(AdapterState {
        version: ADAPTER_VERSION,
        links,
        hooks,
    })
}

fn read_native_manifest(path: &Path) -> io::Result<NativePluginManifest> {
    serde_json::from_str(&fs::read_to_string(path)?)
        .map_err(|error| invalid_data(format!("invalid {}: {error}", path.display())))
}

fn safe_plugin_file(installed: &Path, relative: &str) -> io::Result<PathBuf> {
    let relative = Path::new(relative.strip_prefix("./").unwrap_or(relative));
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(invalid_data(
            "plugin adapter path must stay inside the plugin",
        ));
    }
    let path = installed.join(relative);
    let metadata = fs::symlink_metadata(&path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_data(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    Ok(path)
}

fn adapted_hooks(
    root: &Path,
    name: &str,
    hook_path: &Path,
    windows_command: bool,
) -> io::Result<BTreeMap<String, Vec<Value>>> {
    let document: Value = serde_json::from_str(&fs::read_to_string(hook_path)?)
        .map_err(|error| invalid_data(format!("invalid {}: {error}", hook_path.display())))?;
    let hooks = document
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_data(format!("{} needs a `hooks` object", hook_path.display())))?;
    let plugin_root = root.join(INSTALLS_DIRECTORY).join(name);
    let plugin_data = root.join(".crowded/plugin-data").join(name);
    let mut adapted = BTreeMap::new();
    for (event, handlers) in hooks {
        let mut handlers = handlers.as_array().cloned().ok_or_else(|| {
            invalid_data(format!(
                "{} `hooks.{event}` must contain an array",
                hook_path.display()
            ))
        })?;
        for handler in &mut handlers {
            adapt_hook_commands(handler, &plugin_root, &plugin_data, windows_command);
        }
        adapted.insert(event.clone(), handlers);
    }
    Ok(adapted)
}

fn hook_commands(entries: &BTreeMap<String, Vec<Value>>) -> Vec<&str> {
    let mut commands = Vec::new();
    for handlers in entries.values() {
        for handler in handlers {
            collect_hook_commands(handler, &mut commands);
        }
    }
    commands
}

fn collect_hook_commands<'a>(value: &'a Value, commands: &mut Vec<&'a str>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_hook_commands(value, commands);
            }
        }
        Value::Object(object) => {
            if let Some(command) = object.get("command").and_then(Value::as_str) {
                commands.push(command);
            }
            for value in object.values() {
                collect_hook_commands(value, commands);
            }
        }
        _ => {}
    }
}

fn adapt_hook_commands(
    value: &mut Value,
    plugin_root: &Path,
    plugin_data: &Path,
    windows_command: bool,
) {
    match value {
        Value::Array(values) => {
            for value in values {
                adapt_hook_commands(value, plugin_root, plugin_data, windows_command);
            }
        }
        Value::Object(object) => {
            let windows = windows_command
                .then(|| {
                    object
                        .get("commandWindows")
                        .or_else(|| object.get("command"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .flatten();
            if let Some(Value::String(command)) = object.get_mut("command") {
                let root = plugin_root.to_string_lossy();
                let data = plugin_data.to_string_lossy();
                let adapted = command
                    .replace("${CLAUDE_PLUGIN_ROOT}", &root)
                    .replace("$CLAUDE_PLUGIN_ROOT", &root)
                    .replace("${PLUGIN_ROOT}", &root)
                    .replace("$PLUGIN_ROOT", &root);
                *command = format!(
                    "CLAUDE_PLUGIN_ROOT={} PLUGIN_ROOT={} CLAUDE_PLUGIN_DATA={} PLUGIN_DATA={} {adapted}",
                    shell_quote(&root),
                    shell_quote(&root),
                    shell_quote(&data),
                    shell_quote(&data)
                );
            }
            if let Some(command) = windows {
                let root = plugin_root.to_string_lossy();
                let data = plugin_data.to_string_lossy();
                let adapted = command
                    .replace("${CLAUDE_PLUGIN_ROOT}", "$env:CLAUDE_PLUGIN_ROOT")
                    .replace("$CLAUDE_PLUGIN_ROOT", "$env:CLAUDE_PLUGIN_ROOT")
                    .replace("${PLUGIN_ROOT}", "$env:PLUGIN_ROOT")
                    .replace("$PLUGIN_ROOT", "$env:PLUGIN_ROOT");
                object.insert(
                    "commandWindows".into(),
                    Value::String(format!(
                        "$env:CLAUDE_PLUGIN_ROOT={}; $env:PLUGIN_ROOT={}; $env:CLAUDE_PLUGIN_DATA={}; $env:PLUGIN_DATA={}; {adapted}",
                        powershell_quote(&root),
                        powershell_quote(&root),
                        powershell_quote(&data),
                        powershell_quote(&data)
                    )),
                );
            }
            for value in object.values_mut() {
                adapt_hook_commands(value, plugin_root, plugin_data, windows_command);
            }
        }
        _ => {}
    }
}

fn collect_adapter_links(
    name: &str,
    installed: &Path,
    relative_directory: &str,
    links: &mut Vec<AdapterLink>,
) -> io::Result<()> {
    let source = installed.join(relative_directory);
    let entries = match fs::read_dir(&source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if metadata.is_symlink() {
            return Err(invalid_data(format!(
                "{} must not be a symbolic link",
                entry.path().display()
            )));
        }
        if !metadata.is_file() {
            continue;
        }
        let file = entry.file_name();
        let path = entry.path();
        let extension = path.extension().and_then(|extension| extension.to_str());
        let supported = match relative_directory {
            ".opencode/plugins" => {
                matches!(extension, Some("js" | "mjs" | "cjs" | "ts"))
            }
            ".opencode/command" => extension == Some("md"),
            _ => false,
        };
        if !supported {
            continue;
        }
        links.push(AdapterLink {
            path: Path::new(relative_directory).join(&file),
            target: PathBuf::from("../../")
                .join(INSTALLS_DIRECTORY)
                .join(name)
                .join(relative_directory)
                .join(file),
            copied: false,
        });
    }
    links.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(())
}

fn prepare_hook_enable(root: &Path, hooks: &AdapterHooks) -> io::Result<HookUpdate> {
    let path = root.join(&hooks.path);
    let original = read_optional_regular(&path)?;
    let mut document = parse_json_object(original.as_deref(), &path)?;
    let configured = document
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid_data(format!("{} `hooks` must be an object", path.display())))?;
    for (event, additions) in &hooks.entries {
        let handlers = configured
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                invalid_data(format!(
                    "{} `hooks.{event}` must be an array",
                    path.display()
                ))
            })?;
        for addition in additions {
            if handlers.contains(addition) {
                return Err(invalid_input(format!(
                    "{} already contains the `{event}` adapter hook",
                    path.display()
                )));
            }
            handlers.push(addition.clone());
        }
    }
    let mut generated = serde_json::to_string_pretty(&document).map_err(io::Error::other)?;
    generated.push('\n');
    let mut managed = hooks.clone();
    managed.created = original.is_none();
    Ok(HookUpdate {
        managed,
        original,
        generated: Some(generated),
    })
}

fn prepare_hook_disable(root: &Path, hooks: &AdapterHooks) -> io::Result<HookUpdate> {
    let path = root.join(&hooks.path);
    let original = read_optional_regular(&path)?;
    let mut document = parse_json_object(original.as_deref(), &path)?;
    let root_object = document.as_object_mut().unwrap();
    let configured = root_object
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid_data(format!("{} `hooks` must be an object", path.display())))?;
    for (event, removals) in &hooks.entries {
        let handlers = configured
            .get_mut(event)
            .and_then(Value::as_array_mut)
            .ok_or_else(|| {
                invalid_data(format!(
                    "{} `hooks.{event}` must be an array",
                    path.display()
                ))
            })?;
        for removal in removals {
            let position = handlers
                .iter()
                .position(|handler| handler == removal)
                .ok_or_else(|| {
                    invalid_data(format!(
                        "{} no longer contains the `{event}` adapter hook",
                        path.display()
                    ))
                })?;
            handlers.remove(position);
        }
        if handlers.is_empty() {
            configured.remove(event);
        }
    }
    if configured.is_empty() {
        root_object.remove("hooks");
    }
    let generated = if hooks.created && root_object.is_empty() {
        None
    } else {
        let mut output = serde_json::to_string_pretty(&document).map_err(io::Error::other)?;
        output.push('\n');
        Some(output)
    };
    Ok(HookUpdate {
        managed: hooks.clone(),
        original,
        generated,
    })
}

fn parse_json_object(original: Option<&str>, path: &Path) -> io::Result<Value> {
    let document: Value = match original {
        Some(original) => serde_json::from_str(original)
            .map_err(|error| invalid_data(format!("invalid {}: {error}", path.display())))?,
        None => serde_json::json!({}),
    };
    if document.is_object() {
        Ok(document)
    } else {
        Err(invalid_data(format!(
            "{} must contain a JSON object",
            path.display()
        )))
    }
}

fn read_optional_regular(path: &Path) -> io::Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(invalid_input(format!(
            "{} must not be a symbolic link",
            path.display()
        ))),
        Ok(metadata) if metadata.is_file() => fs::read_to_string(path).map(Some),
        Ok(_) => Err(invalid_input(format!(
            "{} must be a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn apply_hook_update(root: &Path, update: &HookUpdate) -> io::Result<()> {
    let path = root.join(&update.managed.path);
    match &update.generated {
        Some(contents) => write_text_atomic(&path, contents),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
    }
}

fn rollback_hooks(root: &Path, updates: &[HookUpdate]) {
    for update in updates.iter().rev() {
        let path = root.join(&update.managed.path);
        match &update.original {
            Some(original) => {
                let _ = write_text_atomic(&path, original);
            }
            None => {
                let _ = fs::remove_file(path);
            }
        }
    }
}

fn rollback_links(root: &Path, links: &[AdapterLink]) {
    for link in links.iter().rev() {
        let _ = fs::remove_file(root.join(&link.path));
    }
}

fn restore_links(root: &Path, links: &[AdapterLink]) {
    for link in links {
        let path = root.join(&link.path);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if link.copied {
            let _ = copy_adapter_file(&path, &link.target);
        } else {
            let _ = symlink(&link.target, path);
        }
    }
}

fn create_adapter_link(path: &Path, target: &Path) -> io::Result<bool> {
    #[cfg(unix)]
    {
        symlink(target, path)?;
        Ok(false)
    }
    #[cfg(windows)]
    {
        match symlink(target, path) {
            Ok(()) => Ok(false),
            Err(error) if error.raw_os_error() == Some(1314) => {
                copy_adapter_file(path, target)?;
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }
}

fn copy_adapter_file(path: &Path, target: &Path) -> io::Result<()> {
    match fs::copy(adapter_source(path, target)?, path) {
        Ok(_) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(path);
            Err(error)
        }
    }
}

fn adapter_source(path: &Path, target: &Path) -> io::Result<PathBuf> {
    path.parent()
        .map(|parent| parent.join(target))
        .ok_or_else(|| invalid_input("adapter link has no parent"))
}

fn write_text_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid_input("managed file has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".crowded-adapter-{}-{}.tmp",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos()
    ));
    fs::write(&temporary, contents)?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temporary, metadata.permissions())?;
    }
    match fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(temporary);
            Err(error)
        }
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_hooks_get_powershell_commands() {
        let root = Path::new(r"C:\Plugin O'Brien");
        let data = Path::new(r"C:\Data O'Brien");
        let mut hook = serde_json::json!({
            "command": "node \"${PLUGIN_ROOT}/hooks/codex/pretooluse.mjs\""
        });

        adapt_hook_commands(&mut hook, root, data, true);

        assert_eq!(
            hook["commandWindows"],
            r#"$env:CLAUDE_PLUGIN_ROOT='C:\Plugin O''Brien'; $env:PLUGIN_ROOT='C:\Plugin O''Brien'; $env:CLAUDE_PLUGIN_DATA='C:\Data O''Brien'; $env:PLUGIN_DATA='C:\Data O''Brien'; node "$env:PLUGIN_ROOT/hooks/codex/pretooluse.mjs""#
        );
    }
}
