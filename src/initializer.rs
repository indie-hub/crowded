//! Idempotent project bootstrap from `crowded.toml`.

use std::{
    collections::HashSet,
    env, fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Command,
};

use crate::{
    config::{RoomFile, SetupConfig, load_room_file, validate_room_file},
    plugins, toolbox,
};

const CONFIG_FILE: &str = "crowded.toml";
const MARKER_DIRECTORY: &str = ".crowded/init";
const RUNTIME_IGNORE: &str = "/.crowded/";
const STARTER_CONFIG: &str = r#"[[rooms]]
name = "Claude"
command = "claude"
transport = "raw"
allow_control = true

[[rooms]]
name = "Codex"
command = "codex"
transport = "raw"
allow_control = true

[[plugin]]
name = "code4me-ntg"
source = "https://github.com/indie-hub/code4me-ntg.git"

[[plugin]]
name = "ponytail"
source = "https://github.com/DietrichGebert/ponytail.git"

# Shared, pinned tools. `uvx` and `npx` cache lightweight runners. CCC is
# installed once as a user tool because its local embedding environment is big.

[[mcp]]
name = "ccc"
command = "ccc"
args = ["mcp"]

[[mcp]]
name = "codegraph"
command = "npx"
args = ["-y", "@colbymchenry/codegraph@1.5.0", "serve", "--mcp"]

[[mcp]]
name = "basic-memory"
command = "uvx"
args = ["--from", "basic-memory==0.22.1", "basic-memory", "mcp", "--project", __BASIC_MEMORY_PROJECT__]

[[mcp]]
name = "context-mode"
command = "npx"
args = ["-y", "context-mode@1.0.169"]

# Setup commands run once, in order. CCC asks you to choose its embedding model
# the first time and its local model dependencies can download several GB.

[[setup]]
name = "basic-memory-project"
command = "uvx"
args = ["--from", "basic-memory==0.22.1", "basic-memory", "project", "add", __BASIC_MEMORY_PROJECT__, "basic-memory", "--local"]

[[setup]]
name = "codegraph-init"
command = "npx"
args = ["-y", "@colbymchenry/codegraph@1.5.0", "init", "."]

[[setup]]
name = "ccc-install"
command = "uv"
args = ["tool", "install", "--upgrade", "cocoindex-code[full]==0.2.39"]

[[setup]]
name = "ccc-init"
command = "ccc"
args = ["init", "--force"]

[[setup]]
name = "ccc-index"
command = "ccc"
args = ["index"]
"#;

pub(crate) fn command() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().nth(2).is_some() {
        return Err(invalid_input("usage: crowded init (no additional arguments)").into());
    }
    run_at(&env::current_dir()?)?;
    Ok(())
}

fn run_at(root: &Path) -> io::Result<()> {
    let config_path = root.join(CONFIG_FILE);
    if !config_path.try_exists()? {
        ensure_runtime_ignored(root)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config_path)?
            .write_all(starter_config(root).as_bytes())?;
        println!(
            "created {CONFIG_FILE} and ensured {RUNTIME_IGNORE} is ignored; edit the config, then run `crowded init` again"
        );
        return Ok(());
    }

    let config = load_room_file(&config_path)?;
    validate(&config)?;
    let mut pending = Vec::new();
    for setup in &config.setup {
        if !setup_is_complete(root, &setup.name)? {
            pending.push(setup);
        }
    }
    ensure_runtime_ignored(root)?;

    let mut installed = 0;
    for plugin in &config.plugins {
        if plugins::ensure_installed(
            root,
            &plugin.name,
            &plugin.source,
            plugin.reference.as_deref(),
        )? {
            installed += 1;
            println!("installed plugin `{}`", plugin.name);
        } else {
            println!("plugin `{}` already installed", plugin.name);
        }
    }

    let synced = if toolbox::native_files_are_active_at(root)? {
        println!("native toolbox already synced");
        0
    } else {
        let files = toolbox::sync(root)?;
        for path in &files {
            println!("synced {}", path.display());
        }
        files.len()
    };

    if !pending.is_empty() {
        let directory = root.join(MARKER_DIRECTORY);
        match fs::symlink_metadata(&directory) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(invalid_input(format!(
                    "{} must be a regular directory",
                    directory.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&directory)?;
            }
            Err(error) => return Err(error),
        }
    }
    for setup in &pending {
        // Earlier actions may install an executable consumed by a later one.
        preflight_setup(root, setup)?;
        run_setup(root, setup)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(marker(root, &setup.name))?
            .write_all(b"complete\n")?;
        println!("completed setup `{}`", setup.name);
    }

    println!(
        "Crowded initialized: {installed} plugin(s) installed, {synced} native file(s) synced, {} setup action(s) run",
        pending.len()
    );
    Ok(())
}

fn starter_config(root: &Path) -> String {
    let workspace = root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let project = toml::Value::String(format!("crowded-{workspace}")).to_string();
    STARTER_CONFIG.replace("__BASIC_MEMORY_PROJECT__", &project)
}

fn validate(config: &RoomFile) -> io::Result<()> {
    validate_room_file(config)?;

    let mut plugins = HashSet::new();
    for plugin in &config.plugins {
        plugins::validate_install_request(
            &plugin.name,
            &plugin.source,
            plugin.reference.as_deref(),
        )?;
        if !plugins.insert(&plugin.name) {
            return Err(invalid_input(format!(
                "duplicate plugin declaration: {}",
                plugin.name
            )));
        }
    }

    let mut setups = HashSet::new();
    for setup in &config.setup {
        plugins::validate_name("setup", &setup.name)?;
        if setup.command.trim().is_empty() || setup.command.chars().any(char::is_control) {
            return Err(invalid_input(format!(
                "setup `{}` command cannot be empty or contain control characters",
                setup.name
            )));
        }
        if setup
            .args
            .iter()
            .any(|argument| argument.chars().any(char::is_control))
            || setup
                .cwd
                .as_ref()
                .is_some_and(|cwd| cwd.to_string_lossy().chars().any(char::is_control))
        {
            return Err(invalid_input(format!(
                "setup `{}` arguments and cwd cannot contain control characters",
                setup.name
            )));
        }
        if !setups.insert(&setup.name) {
            return Err(invalid_input(format!(
                "duplicate setup declaration: {}",
                setup.name
            )));
        }
    }
    Ok(())
}

fn preflight_setup(root: &Path, setup: &SetupConfig) -> io::Result<()> {
    let cwd = setup_cwd(root, setup);
    if !cwd.is_dir() {
        return Err(invalid_input(format!(
            "setup `{}` working directory does not exist: {}",
            setup.name,
            cwd.display()
        )));
    }
    if !command_exists(&setup.command, &cwd) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "setup `{}` needs `{}` on PATH or at the configured path",
                setup.name, setup.command
            ),
        ));
    }
    Ok(())
}

fn run_setup(root: &Path, setup: &SetupConfig) -> io::Result<()> {
    let cwd = setup_cwd(root, setup);
    let mut command = Command::new(&setup.command);
    command.args(&setup.args).current_dir(cwd);
    println!("running setup `{}`: {command:?}", setup.name);
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "setup `{}` failed with {status}",
            setup.name
        )));
    }
    Ok(())
}

fn setup_cwd(root: &Path, setup: &SetupConfig) -> PathBuf {
    match &setup.cwd {
        Some(cwd) if cwd.is_absolute() => cwd.clone(),
        Some(cwd) => root.join(cwd),
        None => root.to_path_buf(),
    }
}

fn command_exists(command: &str, cwd: &Path) -> bool {
    let command = Path::new(command);
    if command.components().count() > 1 {
        return (if command.is_absolute() {
            command.to_path_buf()
        } else {
            cwd.join(command)
        })
        .is_file();
    }
    env::var_os("PATH").is_some_and(|paths| {
        env::split_paths(&paths).any(|directory| directory.join(command).is_file())
    })
}

fn marker(root: &Path, name: &str) -> PathBuf {
    root.join(MARKER_DIRECTORY).join(format!("{name}.done"))
}

fn setup_is_complete(root: &Path, name: &str) -> io::Result<bool> {
    let path = marker(root, name);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            invalid_input(format!("{} must be a regular file", path.display())),
        ),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn ensure_runtime_ignored(root: &Path) -> io::Result<()> {
    let path = root.join(".gitignore");
    let existing = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(invalid_input(format!(
                "{} must be a regular file",
                path.display()
            )));
        }
        Ok(_) => fs::read_to_string(&path)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    if existing
        .lines()
        .any(|line| matches!(line.trim(), ".crowded/" | "/.crowded/"))
    {
        return Ok(());
    }

    let mut output = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        output.write_all(b"\n")?;
    }
    output.write_all(format!("# Crowded runtime\n{RUNTIME_IGNORE}\n").as_bytes())
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn init_creates_then_applies_config_once() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("crowded-init-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();

        run_at(&root).unwrap();
        assert!(root.join(CONFIG_FILE).is_file());
        let starter = load_room_file(&root.join(CONFIG_FILE)).unwrap();
        validate(&starter).unwrap();
        assert_eq!(starter.plugins.len(), 2);
        assert_eq!(starter.mcp_servers.len(), 4);
        assert_eq!(starter.setup.len(), 5);
        assert!(
            fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .contains(RUNTIME_IGNORE)
        );
        assert!(!root.join(MARKER_DIRECTORY).exists());

        fs::write(
            root.join(CONFIG_FILE),
            r#"[[rooms]]
command = "/bin/sh"
transport = "shell"

[[rooms]]
command = "/bin/sh"
transport = "shell"

[[setup]]
name = "make-tool"
command = "/bin/sh"
args = ["-c", "cp /usr/bin/true generated-tool && chmod +x generated-tool"]

[[setup]]
name = "use-tool"
command = "./generated-tool"
"#,
        )
        .unwrap();
        run_at(&root).unwrap();
        assert!(marker(&root, "make-tool").is_file());
        assert!(marker(&root, "use-tool").is_file());
        run_at(&root).unwrap();
        assert_eq!(
            fs::read_to_string(root.join(".gitignore"))
                .unwrap()
                .matches(RUNTIME_IGNORE)
                .count(),
            1
        );

        fs::remove_dir_all(root).unwrap();
    }
}
