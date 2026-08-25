//! Project-local plugins shared by Claude, Codex, and OpenCode.

mod adapters;

use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::fs::symlink;

use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::Value;

use crate::toolbox::remove_empty_directory;

const INSTALLS_DIRECTORY: &str = ".crowded/plugins";
const MANIFEST_FILE: &str = "crowded-plugin.toml";
const INSTALL_FILE: &str = ".crowded-install.toml";
const ADD_USAGE: &str = "usage: crowded plugin add SOURCE [--ref REF]";
const UPDATE_USAGE: &str = "usage: crowded plugin update PLUGIN [--ref REF]";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginManifest {
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct NativePluginManifest {
    name: String,
    version: String,
    #[serde(default)]
    hooks: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct InstallRecord {
    name: String,
    version: String,
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference: Option<String>,
    revision: String,
    skills: Vec<String>,
}

pub(crate) fn command() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(2);
    let action = args.next().ok_or_else(|| {
        invalid_input(
            "usage: crowded plugin add|update|list|preview|enable|disable|remove (run a command for details)",
        )
    })?;
    let root = env::current_dir()?;

    match action.as_str() {
        "add" => {
            let source = args.next().ok_or_else(|| invalid_input(ADD_USAGE))?;
            let reference = optional_reference(&mut args, ADD_USAGE)?;
            let installed = add(&root, &source, reference.as_deref())?;
            println!(
                "installed {} {} ({}) with {} shared skill(s)",
                installed.name,
                installed.version,
                &installed.revision[..installed.revision.len().min(12)],
                installed.skills.len()
            );
        }
        "update" => {
            let name = args.next().ok_or_else(|| invalid_input(UPDATE_USAGE))?;
            let reference = optional_reference(&mut args, UPDATE_USAGE)?;
            let (previous, installed, changed) = update(&root, &name, reference.as_deref())?;
            if changed {
                println!(
                    "updated {} {} -> {} ({}) with {} shared skill(s)",
                    installed.name,
                    previous.version,
                    installed.version,
                    &installed.revision[..installed.revision.len().min(12)],
                    installed.skills.len()
                );
            } else {
                println!(
                    "{} {} is already current ({})",
                    installed.name,
                    installed.version,
                    &installed.revision[..installed.revision.len().min(12)]
                );
            }
        }
        "list" if args.next().is_none() => {
            let installed = list(&root)?;
            if installed.is_empty() {
                println!("no local Crowded plugins");
            } else {
                for plugin in installed {
                    let active_skill_links = skill_link_presence(&root, &plugin)?
                        .into_iter()
                        .filter(|(_, _, present)| *present)
                        .count();
                    let skill_links = plugin.skills.len() * 3;
                    let skills = match active_skill_links {
                        0 => "disabled",
                        active if active == skill_links => "enabled",
                        _ => "partial",
                    };
                    let adapters = if root
                        .join(INSTALLS_DIRECTORY)
                        .join(&plugin.name)
                        .join(adapters::ADAPTER_FILE)
                        .is_file()
                    {
                        "enabled"
                    } else {
                        "disabled"
                    };
                    println!(
                        "{} {} {} {} skill(s), skills {skills}, adapters {adapters}",
                        plugin.name,
                        plugin.version,
                        &plugin.revision[..plugin.revision.len().min(12)],
                        plugin.skills.len(),
                    );
                }
            }
        }
        "preview" | "enable" | "disable" => {
            let name = args
                .next()
                .ok_or_else(|| invalid_input(format!("usage: crowded plugin {action} PLUGIN")))?;
            if args.next().is_some() {
                return Err(invalid_input(format!("usage: crowded plugin {action} PLUGIN")).into());
            }
            match action.as_str() {
                "preview" => adapters::preview_adapters(&root, &name)?,
                "enable" => {
                    let (skills, state) = enable_plugin(&root, &name)?;
                    let hooks = state.as_ref().map_or(0, |state| state.hooks.len());
                    let open_code = state.as_ref().map_or(0, |state| state.links.len());
                    println!(
                        "enabled {skills} shared skill(s), {hooks} hook target(s), and {open_code} OpenCode file(s)"
                    );
                }
                "disable" => {
                    let (skills, state) = disable_plugin(&root, &name)?;
                    let hooks = state.as_ref().map_or(0, |state| state.hooks.len());
                    let open_code = state.as_ref().map_or(0, |state| state.links.len());
                    println!(
                        "disabled {skills} shared skill(s), {hooks} hook target(s), and {open_code} OpenCode file(s)"
                    );
                }
                _ => unreachable!(),
            }
        }
        "remove" => {
            let name = args
                .next()
                .ok_or_else(|| invalid_input("usage: crowded plugin remove PLUGIN"))?;
            if args.next().is_some() {
                return Err(invalid_input("usage: crowded plugin remove PLUGIN").into());
            }
            let removed = remove(&root, &name)?;
            println!(
                "removed {} and {} shared skill(s)",
                removed.name,
                removed.skills.len()
            );
        }
        _ => {
            return Err(invalid_input(
                "usage: crowded plugin add SOURCE [--ref REF] | update PLUGIN [--ref REF] | list | preview|enable|disable|remove PLUGIN",
            )
            .into());
        }
    }
    Ok(())
}

fn optional_reference(
    args: &mut impl Iterator<Item = String>,
    usage: &str,
) -> io::Result<Option<String>> {
    match args.next() {
        None => Ok(None),
        Some(flag) if flag == "--ref" => {
            let reference = args.next().ok_or_else(|| invalid_input(usage))?;
            if args.next().is_some() {
                return Err(invalid_input(usage));
            }
            Ok(Some(reference))
        }
        Some(_) => Err(invalid_input(usage)),
    }
}

pub(crate) fn validate_install_request(
    name: &str,
    source: &str,
    reference: Option<&str>,
) -> io::Result<()> {
    validate_name("plugin", name)?;
    validate_source(source)?;
    if let Some(reference) = reference {
        validate_reference(reference)?;
    }
    Ok(())
}

pub(crate) fn ensure_installed(
    root: &Path,
    name: &str,
    source: &str,
    reference: Option<&str>,
) -> io::Result<bool> {
    validate_install_request(name, source, reference)?;
    let installed = root.join(INSTALLS_DIRECTORY).join(name);
    if installed.try_exists()? {
        let (_, record) = installed_record(root, name)?;
        if record.source != source || record.reference.as_deref() != reference {
            return Err(invalid_input(format!(
                "plugin `{name}` is installed from a different source or ref; update or remove it explicitly"
            )));
        }
        return Ok(false);
    }

    let record = add(root, source, reference)?;
    if record.name != name {
        let cleanup = remove(root, &record.name);
        return match cleanup {
            Ok(_) => Err(invalid_data(format!(
                "plugin source contains `{}`, expected `{name}`",
                record.name
            ))),
            Err(cleanup) => Err(io::Error::other(format!(
                "plugin source contains `{}`, expected `{name}`; cleanup failed: {cleanup}",
                record.name
            ))),
        };
    }
    Ok(true)
}

pub(crate) fn ensure_adapters_enabled(root: &Path, name: &str) -> io::Result<bool> {
    validate_name("plugin", name)?;
    let installed = root.join(INSTALLS_DIRECTORY).join(name);
    if installed.join(adapters::ADAPTER_FILE).try_exists()? {
        return Ok(false);
    }
    adapters::enable_adapters(root, name)?;
    Ok(true)
}

fn add(root: &Path, source: &str, reference: Option<&str>) -> io::Result<InstallRecord> {
    validate_source(source)?;
    if let Some(reference) = reference {
        validate_reference(reference)?;
    }

    create_install_directory(root)?;
    let temporary = temporary_plugin_path(root, "install")?;
    let clone_result = clone_source(source, reference, &temporary);
    if let Err(error) = clone_result {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    let result = prepare_install(root, source, reference, &temporary);
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result
}

fn prepare_install(
    root: &Path,
    source: &str,
    reference: Option<&str>,
    temporary: &Path,
) -> io::Result<InstallRecord> {
    let record = inspect_plugin(source, reference, temporary)?;
    let installed = root.join(INSTALLS_DIRECTORY).join(&record.name);
    match fs::symlink_metadata(&installed) {
        Ok(_) => {
            return Err(invalid_input(format!(
                "plugin `{}` is already installed",
                record.name
            )));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let links = skill_links(root, &record);
    for (link, _) in &links {
        match fs::symlink_metadata(link) {
            Ok(_) => {
                return Err(invalid_input(format!(
                    "{} already exists; refusing to replace it",
                    link.display()
                )));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    write_install_record(temporary, &record)?;
    fs::rename(temporary, &installed)?;

    let mut created = Vec::new();
    for (link, target) in &links {
        let result = link
            .parent()
            .ok_or_else(|| invalid_input("skill link has no parent"))
            .and_then(fs::create_dir_all)
            .and_then(|()| create_skill_link(target, link));
        if let Err(error) = result {
            for link in created {
                let _ = remove_skill_link(link);
            }
            let _ = fs::remove_dir_all(&installed);
            return Err(error);
        }
        created.push(link);
    }
    Ok(record)
}

fn inspect_plugin(
    source: &str,
    reference: Option<&str>,
    directory: &Path,
) -> io::Result<InstallRecord> {
    let manifest = load_manifest(directory)?;
    validate_name("plugin", &manifest.name)?;
    validate_version(&manifest.version)?;
    Ok(InstallRecord {
        name: manifest.name,
        version: manifest.version,
        source: source.to_owned(),
        reference: reference.map(str::to_owned),
        revision: git_revision(directory)?,
        skills: discover_skills(&directory.join("skills"))?,
    })
}

fn write_install_record(directory: &Path, record: &InstallRecord) -> io::Result<()> {
    fs::write(
        directory.join(INSTALL_FILE),
        toml::to_string_pretty(record).map_err(io::Error::other)?,
    )
}

fn list(root: &Path) -> io::Result<Vec<InstallRecord>> {
    let directory = root.join(INSTALLS_DIRECTORY);
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut installed = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            installed.push(load_record(&entry.path())?);
        }
    }
    installed.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(installed)
}

fn installed_record(root: &Path, name: &str) -> io::Result<(PathBuf, InstallRecord)> {
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
    Ok((installed, record))
}

fn update(
    root: &Path,
    name: &str,
    reference_override: Option<&str>,
) -> io::Result<(InstallRecord, InstallRecord, bool)> {
    validate_name("plugin", name)?;
    let (installed, previous) = installed_record(root, name)?;
    validate_source(&previous.source)?;
    if let Some(reference) = reference_override {
        validate_reference(reference)?;
    }
    let reference = reference_override.or(previous.reference.as_deref());

    let links = skill_link_presence(root, &previous)?;
    let active_links = links.iter().filter(|(_, _, present)| *present).count();
    let all_links = previous.skills.len() * 3;
    if active_links != 0 && active_links != all_links {
        return Err(invalid_input(format!(
            "plugin `{name}` has partially enabled skills; enable or disable it before updating"
        )));
    }
    let skills_enabled = active_links == all_links;
    let adapters_enabled = installed.join(adapters::ADAPTER_FILE).try_exists()?;

    create_install_directory(root)?;
    let temporary = temporary_plugin_path(root, "update")?;
    if let Err(error) = clone_source(&previous.source, reference, &temporary) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    let next = match inspect_plugin(&previous.source, reference, &temporary) {
        Ok(next) if next.name == name => next,
        Ok(next) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(invalid_data(format!(
                "plugin `{name}` source now contains plugin `{}`",
                next.name
            )));
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary);
            return Err(error);
        }
    };
    if next.revision == previous.revision && next.reference == previous.reference {
        fs::remove_dir_all(&temporary)?;
        return Ok((previous, next, false));
    }
    if let Err(error) = write_install_record(&temporary, &next) {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }

    let backup = temporary_plugin_path(root, "backup")?;
    if let Err(error) =
        deactivate_plugin_state(root, name, &previous, skills_enabled, adapters_enabled)
    {
        let _ = fs::remove_dir_all(&temporary);
        return Err(error);
    }
    if let Err(error) = fs::rename(&installed, &backup) {
        let rollback =
            activate_plugin_state(root, name, &previous, skills_enabled, adapters_enabled);
        let _ = fs::remove_dir_all(&temporary);
        return Err(update_error(error, rollback));
    }
    if let Err(error) = fs::rename(&temporary, &installed) {
        let rollback = restore_previous_plugin(
            root,
            name,
            &installed,
            &backup,
            &previous,
            skills_enabled,
            adapters_enabled,
        );
        let _ = fs::remove_dir_all(&temporary);
        return Err(update_error(error, rollback));
    }
    if let Err(error) = activate_plugin_state(root, name, &next, skills_enabled, adapters_enabled) {
        let rollback = restore_previous_plugin(
            root,
            name,
            &installed,
            &backup,
            &previous,
            skills_enabled,
            adapters_enabled,
        );
        return Err(update_error(error, rollback));
    }

    fs::remove_dir_all(backup)?;
    Ok((previous, next, true))
}

fn deactivate_plugin_state(
    root: &Path,
    name: &str,
    record: &InstallRecord,
    skills_enabled: bool,
    adapters_enabled: bool,
) -> io::Result<()> {
    let removed = if skills_enabled {
        disable_skill_links(root, record)?
    } else {
        Vec::new()
    };
    if adapters_enabled && let Err(error) = adapters::disable_adapters(root, name) {
        return match restore_skill_links(&removed) {
            Ok(()) => Err(error),
            Err(rollback) => Err(io::Error::other(format!(
                "{error}; could not restore shared skills: {rollback}"
            ))),
        };
    }
    Ok(())
}

fn activate_plugin_state(
    root: &Path,
    name: &str,
    record: &InstallRecord,
    skills_enabled: bool,
    adapters_enabled: bool,
) -> io::Result<()> {
    let created = if skills_enabled {
        enable_skill_links(root, record)?
    } else {
        Vec::new()
    };
    if adapters_enabled && let Err(error) = adapters::enable_adapters(root, name) {
        return match remove_skill_links(&created) {
            Ok(()) => Err(error),
            Err(rollback) => Err(io::Error::other(format!(
                "{error}; could not remove new shared skills: {rollback}"
            ))),
        };
    }
    Ok(())
}

fn restore_previous_plugin(
    root: &Path,
    name: &str,
    installed: &Path,
    backup: &Path,
    previous: &InstallRecord,
    skills_enabled: bool,
    adapters_enabled: bool,
) -> io::Result<()> {
    if installed.try_exists()? {
        fs::remove_dir_all(installed)?;
    }
    fs::rename(backup, installed)?;
    activate_plugin_state(root, name, previous, skills_enabled, adapters_enabled)
}

fn update_error(error: io::Error, rollback: io::Result<()>) -> io::Error {
    match rollback {
        Ok(()) => error,
        Err(rollback) => io::Error::other(format!(
            "{error}; could not restore the previous plugin: {rollback}"
        )),
    }
}

fn enable_plugin(root: &Path, name: &str) -> io::Result<(usize, Option<adapters::AdapterState>)> {
    let (installed, record) = installed_record(root, name)?;
    let state_path = installed.join(adapters::ADAPTER_FILE);
    let adapters_enabled = state_path.try_exists()?;
    let plan = if adapters_enabled {
        None
    } else {
        Some(adapters::adapter_plan(root, name)?)
    };
    let created_skills = enable_skill_links(root, &record)?;
    if adapters_enabled {
        if created_skills.is_empty() {
            return Err(invalid_input(format!("plugin `{name}` is already enabled")));
        }
        return Ok((record.skills.len(), None));
    }

    let plan = plan.unwrap();
    if plan.hooks.is_empty() && plan.links.is_empty() {
        if created_skills.is_empty() {
            return Err(invalid_input(format!("plugin `{name}` is already enabled")));
        }
        return Ok((record.skills.len(), None));
    }

    match adapters::enable_adapters(root, name) {
        Ok(state) => Ok((
            if created_skills.is_empty() {
                0
            } else {
                record.skills.len()
            },
            Some(state),
        )),
        Err(error) => match remove_skill_links(&created_skills) {
            Ok(()) => Err(error),
            Err(rollback) => Err(io::Error::other(format!(
                "{error}; could not roll back shared skills: {rollback}"
            ))),
        },
    }
}

fn disable_plugin(root: &Path, name: &str) -> io::Result<(usize, Option<adapters::AdapterState>)> {
    let (installed, record) = installed_record(root, name)?;
    let adapters_enabled = installed.join(adapters::ADAPTER_FILE).try_exists()?;
    let removed_skills = disable_skill_links(root, &record)?;
    if !adapters_enabled && removed_skills.is_empty() {
        return Err(invalid_input(format!(
            "plugin `{name}` is already disabled"
        )));
    }

    let state = if adapters_enabled {
        match adapters::disable_adapters(root, name) {
            Ok(state) => Some(state),
            Err(error) => match restore_skill_links(&removed_skills) {
                Ok(()) => return Err(error),
                Err(rollback) => {
                    return Err(io::Error::other(format!(
                        "{error}; could not restore shared skills: {rollback}"
                    )));
                }
            },
        }
    } else {
        None
    };
    remove_empty_skill_directories(root)?;
    Ok((
        if removed_skills.is_empty() {
            0
        } else {
            record.skills.len()
        },
        state,
    ))
}

fn remove(root: &Path, name: &str) -> io::Result<InstallRecord> {
    let (installed, record) = installed_record(root, name)?;
    if installed.join(adapters::ADAPTER_FILE).try_exists()? {
        adapters::disable_adapters(root, name)?;
    }
    disable_skill_links(root, &record)?;
    fs::remove_dir_all(&installed)?;
    let data = root.join(".crowded/plugin-data").join(name);
    if data.try_exists()? {
        fs::remove_dir_all(data)?;
        remove_empty_directory(&root.join(".crowded/plugin-data"))?;
    }
    remove_empty_skill_directories(root)?;
    remove_empty_directory(&root.join(INSTALLS_DIRECTORY))?;
    Ok(record)
}

fn clone_source(source: &str, reference: Option<&str>, destination: &Path) -> io::Result<()> {
    let source = normalized_source(source);
    let mut command = Command::new("git");
    command.args(["clone", "--quiet"]);
    if !Path::new(&source).exists() {
        command.args(["--depth", "1"]);
    }
    if let Some(reference) = reference {
        command.args(["--branch", reference]);
    }
    let status = command.arg("--").arg(source).arg(destination).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "git clone failed with status {status}"
        )))
    }
}

fn temporary_plugin_path(root: &Path, operation: &str) -> io::Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok(root
        .join(".crowded")
        .join(format!("plugin-{operation}-{}-{nonce}", std::process::id())))
}

fn create_install_directory(root: &Path) -> io::Result<()> {
    let crowded = root.join(".crowded");
    for directory in [&crowded, &root.join(INSTALLS_DIRECTORY)] {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid_input(format!(
                    "{} must not be a symbolic link",
                    directory.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    let installs = root.join(INSTALLS_DIRECTORY);
    fs::create_dir_all(&installs)?;
    Ok(())
}

fn normalized_source(source: &str) -> String {
    if Path::new(source).exists()
        || source.contains("://")
        || source.starts_with("git@")
        || source.matches('/').count() != 1
    {
        source.to_owned()
    } else {
        format!("https://github.com/{source}.git")
    }
}

fn git_revision(directory: &Path) -> io::Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(
            "could not read the installed Git revision",
        ));
    }
    let revision = String::from_utf8(output.stdout)
        .map_err(io::Error::other)?
        .trim()
        .to_owned();
    if revision.is_empty() {
        return Err(invalid_data("installed Git revision is empty"));
    }
    Ok(revision)
}

fn load_manifest(directory: &Path) -> io::Result<PluginManifest> {
    let crowded = directory.join(MANIFEST_FILE);
    if crowded.try_exists()? {
        return toml::from_str(&fs::read_to_string(&crowded)?)
            .map_err(|error| invalid_data(format!("invalid {}: {error}", crowded.display())));
    }

    for relative in [".codex-plugin/plugin.json", ".claude-plugin/plugin.json"] {
        let path = directory.join(relative);
        if path.try_exists()? {
            let native: NativePluginManifest = serde_json::from_str(&fs::read_to_string(&path)?)
                .map_err(|error| invalid_data(format!("invalid {}: {error}", path.display())))?;
            return Ok(PluginManifest {
                name: native.name,
                version: native.version,
            });
        }
    }

    Err(invalid_data(format!(
        "plugin needs `{MANIFEST_FILE}`, `.codex-plugin/plugin.json`, or `.claude-plugin/plugin.json`"
    )))
}

fn load_record(directory: &Path) -> io::Result<InstallRecord> {
    let path = directory.join(INSTALL_FILE);
    toml::from_str(&fs::read_to_string(&path)?)
        .map_err(|error| invalid_data(format!("invalid {}: {error}", path.display())))
}

fn discover_skills(directory: &Path) -> io::Result<Vec<String>> {
    let mut skills = Vec::new();
    let entries = fs::read_dir(directory).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            invalid_data("plugin needs a `skills/` directory containing at least one SKILL.md")
        } else {
            error
        }
    })?;
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_symlink() || !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| invalid_data("skill directory name must be UTF-8"))?;
        validate_name("skill", &name)?;
        let skill_file = entry.path().join("SKILL.md");
        if fs::symlink_metadata(&skill_file)?.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "{} must not be a symbolic link",
                skill_file.display()
            )));
        }
        validate_skill(&skill_file, &name)?;
        skills.push(name);
    }
    if skills.is_empty() {
        return Err(invalid_data(
            "plugin must contain at least one skills/*/SKILL.md",
        ));
    }
    skills.sort();
    Ok(skills)
}

fn validate_skill(path: &Path, directory_name: &str) -> io::Result<()> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return Err(invalid_data(format!(
            "{} must begin with YAML frontmatter",
            path.display()
        )));
    }
    let mut name = None;
    let mut description = None;
    let mut closed = false;
    for line in lines {
        if line == "---" {
            closed = true;
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches(['"', '\'']);
            match key.trim() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                _ => {}
            }
        }
    }
    if !closed || name != Some(directory_name) || description.is_none_or(str::is_empty) {
        return Err(invalid_data(format!(
            "{} needs matching `name` and non-empty `description` frontmatter",
            path.display()
        )));
    }
    Ok(())
}

fn skill_links(root: &Path, plugin: &InstallRecord) -> Vec<(PathBuf, PathBuf)> {
    let mut links = Vec::with_capacity(plugin.skills.len() * 3);
    for skill in &plugin.skills {
        let target = PathBuf::from("../../")
            .join(INSTALLS_DIRECTORY)
            .join(&plugin.name)
            .join("skills")
            .join(skill);
        links.push((root.join(".agents/skills").join(skill), target.clone()));
        links.push((root.join(".claude/skills").join(skill), target.clone()));
        links.push((root.join(".opencode/skills").join(skill), target));
    }
    links
}

fn skill_link_presence(
    root: &Path,
    plugin: &InstallRecord,
) -> io::Result<Vec<(PathBuf, PathBuf, bool)>> {
    skill_links(root, plugin)
        .into_iter()
        .map(|(link, expected)| {
            let present = match fs::read_link(&link) {
                Ok(actual) => {
                    if !skill_link_matches(&link, &actual, &expected)? {
                        return Err(invalid_data(format!(
                            "{} no longer points to the Crowded-managed skill",
                            link.display()
                        )));
                    }
                    true
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                    return Err(invalid_data(format!(
                        "{} exists but is not a Crowded-managed skill link",
                        link.display()
                    )));
                }
                Err(error) => return Err(error),
            };
            Ok((link, expected, present))
        })
        .collect()
}

fn enable_skill_links(root: &Path, plugin: &InstallRecord) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let links = skill_link_presence(root, plugin)?;
    let mut created = Vec::new();
    for (link, target, present) in links {
        if present {
            continue;
        }
        let result = link
            .parent()
            .ok_or_else(|| invalid_input("skill link has no parent"))
            .and_then(fs::create_dir_all)
            .and_then(|()| create_skill_link(&target, &link));
        if let Err(error) = result {
            return match remove_skill_links(&created) {
                Ok(()) => Err(error),
                Err(rollback) => Err(io::Error::other(format!(
                    "{error}; could not roll back shared skills: {rollback}"
                ))),
            };
        }
        created.push((link, target));
    }
    Ok(created)
}

fn skill_link_matches(link: &Path, actual: &Path, expected: &Path) -> io::Result<bool> {
    let parent = link
        .parent()
        .ok_or_else(|| invalid_input("skill link has no parent"))?;
    let resolve = |target: &Path| {
        fs::canonicalize(if target.is_absolute() {
            target.to_path_buf()
        } else {
            parent.join(target)
        })
    };
    Ok(resolve(actual)? == resolve(expected)?)
}

#[cfg(unix)]
fn create_skill_link(target: &Path, link: &Path) -> io::Result<()> {
    symlink(target, link)
}

#[cfg(windows)]
fn create_skill_link(target: &Path, link: &Path) -> io::Result<()> {
    // Windows directory skill links must always be junctions, never plain
    // symbolic links. Claude Code and Codex CLI skill scanners only reliably
    // discover junctions (especially not relative symlinks), so making the
    // link type depend on whether the process holds the symlink privilege
    // left some installs undiscovered. Junctions do not need that privilege,
    // so there is no scenario where the old symlink attempt would succeed but
    // a junction would not. Junctions also cannot be relative, so the
    // relative `target` is resolved against the link's directory first.
    let parent = link
        .parent()
        .ok_or_else(|| invalid_input("skill link has no parent"))?;
    let target = parent.join(target);
    let status = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "New-Item -ItemType Junction -Path $env:CROWDED_SKILL_LINK -Target $env:CROWDED_SKILL_TARGET -ErrorAction Stop | Out-Null",
        ])
        .env("CROWDED_SKILL_LINK", link)
        .env("CROWDED_SKILL_TARGET", target)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "could not create Windows skill junction: {status}"
        )))
    }
}

fn disable_skill_links(root: &Path, plugin: &InstallRecord) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let links = skill_link_presence(root, plugin)?;
    let mut removed = Vec::new();
    for (link, target, present) in links {
        if !present {
            continue;
        }
        if let Err(error) = remove_skill_link(&link) {
            return match restore_skill_links(&removed) {
                Ok(()) => Err(error),
                Err(rollback) => Err(io::Error::other(format!(
                    "{error}; could not restore shared skills: {rollback}"
                ))),
            };
        }
        removed.push((link, target));
    }
    Ok(removed)
}

fn remove_skill_links(links: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    for (link, _) in links.iter().rev() {
        match remove_skill_link(link) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn remove_skill_link(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn remove_skill_link(path: &Path) -> io::Result<()> {
    fs::remove_dir(path)
}

fn restore_skill_links(links: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    for (link, target) in links {
        let parent = link
            .parent()
            .ok_or_else(|| invalid_input("skill link has no parent"))?;
        fs::create_dir_all(parent)?;
        create_skill_link(target, link)?;
    }
    Ok(())
}

fn remove_empty_skill_directories(root: &Path) -> io::Result<()> {
    remove_empty_directory(&root.join(".agents/skills"))?;
    remove_empty_directory(&root.join(".claude/skills"))?;
    remove_empty_directory(&root.join(".opencode/skills"))
}

pub(crate) fn validate_name(kind: &str, name: &str) -> io::Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(invalid_input(format!(
            "{kind} name must be 1..=64 lowercase letters, digits, or single hyphens"
        )))
    }
}

fn validate_version(version: &str) -> io::Result<()> {
    if version.is_empty() || version.len() > 64 || version.chars().any(char::is_control) {
        Err(invalid_input(
            "plugin version must contain 1..=64 characters without controls",
        ))
    } else {
        Ok(())
    }
}

fn validate_source(source: &str) -> io::Result<()> {
    if source.is_empty() || source.len() > 2048 || source.chars().any(char::is_control) {
        Err(invalid_input(
            "plugin source must contain 1..=2048 characters without controls",
        ))
    } else {
        Ok(())
    }
}

fn validate_reference(reference: &str) -> io::Result<()> {
    if reference.is_empty()
        || reference.len() > 255
        || reference.starts_with('-')
        || reference.chars().any(char::is_control)
    {
        Err(invalid_input(
            "plugin ref must contain 1..=255 characters, no controls, and cannot start with `-`",
        ))
    } else {
        Ok(())
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_skill_links_are_junctions_with_absolute_targets() {
        let base = test_directory();
        let target = base.join("target");
        let link = base.join("skill-link");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("SKILL.md"), "skill").unwrap();

        create_skill_link(Path::new("target"), &link).unwrap();

        let resolved = fs::read_link(&link).unwrap();
        // Junctions require absolute targets; the old privilege-dependent code
        // created relative symbolic links when the symlink privilege was held,
        // which Windows skill scanners fail to discover.
        assert!(
            resolved.is_absolute(),
            "Windows skill link must be a junction with an absolute target, got {resolved:?}"
        );
        assert!(link.join("SKILL.md").is_file());
        assert!(skill_link_matches(&link, &resolved, Path::new("target")).unwrap());

        remove_skill_link(&link).unwrap();

        assert!(!link.exists());
        assert!(target.join("SKILL.md").is_file());
        fs::remove_file(target.join("SKILL.md")).unwrap();
        fs::remove_dir(target).unwrap();
        fs::remove_dir(base).unwrap();
    }

    #[test]
    fn local_plugin_is_shared_and_removed_without_touching_other_files() {
        let base = test_directory();
        let source = base.join("source");
        let project = base.join("project");
        fs::create_dir_all(source.join("skills/room-greeter")).unwrap();
        fs::create_dir_all(source.join("hooks")).unwrap();
        fs::create_dir_all(source.join(".opencode/plugins")).unwrap();
        fs::create_dir_all(source.join(".opencode/command")).unwrap();
        fs::create_dir(&project).unwrap();
        fs::create_dir(source.join(".codex-plugin")).unwrap();
        fs::create_dir(source.join(".claude-plugin")).unwrap();
        fs::write(
            source.join(".codex-plugin/plugin.json"),
            r#"{
                "name": "greetings",
                "version": "1.0.0",
                "description": "Native manifest fixture",
                "skills": "./skills/",
                "hooks": "./hooks/hooks.json"
            }"#,
        )
        .unwrap();
        fs::write(
            source.join(".claude-plugin/plugin.json"),
            r#"{
                "name": "greetings",
                "version": "1.0.0"
            }"#,
        )
        .unwrap();
        fs::write(
            source.join("hooks/hooks.json"),
            r#"{
                "hooks": {
                    "SessionStart": [{
                        "hooks": [{
                            "type": "command",
                            "command": "node \"${CLAUDE_PLUGIN_ROOT}/hooks/activate.js\""
                        }]
                    }]
                }
            }"#,
        )
        .unwrap();
        fs::write(source.join("hooks/activate.js"), "console.log('hello')\n").unwrap();
        fs::write(
            source.join(".opencode/plugins/greetings.mjs"),
            "export const Greetings = async () => ({})\n",
        )
        .unwrap();
        fs::write(
            source.join(".opencode/command/greet.md"),
            "---\ndescription: Greet\n---\nHello\n",
        )
        .unwrap();
        fs::write(
            source.join("skills/room-greeter/SKILL.md"),
            "---\nname: room-greeter\ndescription: Greets every room\n---\n\nSay hello.\n",
        )
        .unwrap();
        git(&source, &["init", "--quiet"]);
        git(&source, &["config", "user.name", "Crowded Test"]);
        git(
            &source,
            &["config", "user.email", "crowded@example.invalid"],
        );
        git(&source, &["add", "."]);
        git(&source, &["commit", "--quiet", "-m", "fixture"]);

        fs::create_dir_all(project.join(".claude")).unwrap();
        fs::create_dir_all(project.join(".codex")).unwrap();
        let existing_hooks = r#"{
            "hooks": {
                "SessionStart": [{
                    "hooks": [{"type": "command", "command": "pulse"}]
                }]
            }
        }"#;
        fs::write(project.join(".claude/settings.local.json"), existing_hooks).unwrap();
        fs::write(project.join(".codex/hooks.json"), existing_hooks).unwrap();

        assert!(ensure_installed(&project, "greetings", source.to_str().unwrap(), None).unwrap());
        assert!(!ensure_installed(&project, "greetings", source.to_str().unwrap(), None).unwrap());
        let installed = list(&project).unwrap().remove(0);
        assert_eq!(installed.name, "greetings");
        assert_eq!(list(&project).unwrap().len(), 1);
        for link in [
            project.join(".agents/skills/room-greeter"),
            project.join(".claude/skills/room-greeter"),
            project.join(".opencode/skills/room-greeter"),
        ] {
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert!(link.join("SKILL.md").is_file());
        }

        let (skills, adapters) = enable_plugin(&project, "greetings").unwrap();
        assert_eq!(skills, 0);
        let adapters = adapters.unwrap();
        assert_eq!(adapters.hooks.len(), 2);
        assert_eq!(adapters.links.len(), 2);
        let claude: Value = serde_json::from_str(
            &fs::read_to_string(project.join(".claude/settings.local.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
        assert!(
            claude["hooks"]["SessionStart"][1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .replace('\\', "/")
                .contains(".crowded/plugins/greetings/hooks/activate.js")
        );
        assert!(project.join(".opencode/plugins/greetings.mjs").is_file());

        // Reproduce v0.15.0's partial state: adapters off, skills still linked.
        adapters::disable_adapters(&project, "greetings").unwrap();
        let (skills, adapters) = disable_plugin(&project, "greetings").unwrap();
        assert_eq!(skills, 1);
        assert!(adapters.is_none());
        assert!(!project.join(".agents/skills/room-greeter").exists());

        let (skills, adapters) = enable_plugin(&project, "greetings").unwrap();
        assert_eq!(skills, 1);
        assert_eq!(adapters.unwrap().hooks.len(), 2);

        let (skills, adapters) = disable_plugin(&project, "greetings").unwrap();
        assert_eq!(skills, 1);
        assert_eq!(adapters.unwrap().hooks.len(), 2);
        for hook_file in [
            project.join(".claude/settings.local.json"),
            project.join(".codex/hooks.json"),
        ] {
            let hooks: Value =
                serde_json::from_str(&fs::read_to_string(hook_file).unwrap()).unwrap();
            assert_eq!(hooks["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
        }
        assert!(!project.join(".opencode/plugins/greetings.mjs").exists());
        for link in [
            project.join(".agents/skills/room-greeter"),
            project.join(".claude/skills/room-greeter"),
            project.join(".opencode/skills/room-greeter"),
        ] {
            assert!(!link.exists());
        }

        let (skills, adapters) = enable_plugin(&project, "greetings").unwrap();
        assert_eq!(skills, 1);
        assert_eq!(adapters.unwrap().hooks.len(), 2);
        assert!(project.join(".agents/skills/room-greeter").exists());
        assert!(project.join(".claude/skills/room-greeter").exists());
        assert!(project.join(".opencode/skills/room-greeter").exists());
        assert!(project.join(".opencode/plugins/greetings.mjs").exists());

        for manifest in [
            source.join(".codex-plugin/plugin.json"),
            source.join(".claude-plugin/plugin.json"),
        ] {
            let updated = fs::read_to_string(&manifest)
                .unwrap()
                .replace("1.0.0", "1.1.0");
            fs::write(manifest, updated).unwrap();
        }
        fs::create_dir_all(source.join("skills/room-farewell")).unwrap();
        fs::write(
            source.join("skills/room-farewell/SKILL.md"),
            "---\nname: room-farewell\ndescription: Bids every room farewell\n---\n\nSay goodbye.\n",
        )
        .unwrap();
        fs::write(
            source.join(".opencode/command/farewell.md"),
            "---\ndescription: Bid farewell\n---\nGoodbye\n",
        )
        .unwrap();
        fs::create_dir_all(project.join(".crowded/plugin-data/greetings")).unwrap();
        fs::write(
            project.join(".crowded/plugin-data/greetings/memory"),
            "preserve me\n",
        )
        .unwrap();
        git(&source, &["add", "."]);
        git(&source, &["commit", "--quiet", "-m", "update fixture"]);

        let (previous, updated, changed) = update(&project, "greetings", None).unwrap();
        assert!(changed);
        assert_eq!(previous.version, "1.0.0");
        assert_eq!(updated.version, "1.1.0");
        assert_eq!(updated.skills.len(), 2);
        assert!(project.join(".agents/skills/room-farewell").exists());
        assert!(project.join(".opencode/command/farewell.md").exists());
        assert!(
            project
                .join(".crowded/plugins/greetings/.crowded-adapters.json")
                .exists()
        );
        assert_eq!(
            fs::read_to_string(project.join(".crowded/plugin-data/greetings/memory")).unwrap(),
            "preserve me\n"
        );
        assert!(!update(&project, "greetings", None).unwrap().2);

        remove(&project, "greetings").unwrap();
        assert!(list(&project).unwrap().is_empty());
        assert!(!project.join(".agents/skills/room-greeter").exists());
        assert!(!project.join(".agents/skills/room-farewell").exists());
        assert!(!project.join(".claude/skills/room-greeter").exists());
        assert!(!project.join(".opencode/skills/room-greeter").exists());
        assert!(!project.join(".opencode/command/farewell.md").exists());

        fs::remove_dir_all(base).unwrap();
    }

    fn git(directory: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(directory)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    fn test_directory() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = env::temp_dir().join(format!(
            "crowded-plugin-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
}
