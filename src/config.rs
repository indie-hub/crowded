//! Room declarations and the per-CLI adapters for Crowded's shared toolbox.

use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Transport {
    Shell,
    Raw,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoomFile {
    pub(crate) rooms: Vec<RoomConfig>,
    #[serde(default, rename = "mcp")]
    pub(crate) mcp_servers: Vec<McpConfig>,
    #[serde(default, rename = "opencode_plugin")]
    pub(crate) opencode_plugins: Vec<OpenCodePluginConfig>,
    #[serde(default, rename = "plugin")]
    pub(crate) plugins: Vec<PluginConfig>,
    #[serde(default)]
    pub(crate) setup: Vec<SetupConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpConfig {
    pub(crate) name: String,
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
    #[serde(default)]
    pub(crate) clients: Vec<McpClient>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum McpClient {
    Claude,
    Codex,
    Opencode,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenCodePluginConfig {
    pub(crate) package: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PluginConfig {
    pub(crate) name: String,
    pub(crate) source: String,
    #[serde(rename = "ref")]
    pub(crate) reference: Option<String>,
    #[serde(default)]
    pub(crate) adapters: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetupConfig {
    pub(crate) name: String,
    pub(crate) command: String,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoomConfig {
    name: Option<String>,
    pub(crate) command: String,
    #[serde(default)]
    args: Vec<String>,
    pub(crate) transport: Transport,
    pub(crate) cwd: Option<PathBuf>,
    #[serde(default)]
    pub(crate) allow_control: bool,
}

#[derive(Clone)]
pub(crate) struct RoomSpec {
    pub(crate) title: String,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) transport: Transport,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) variables: Vec<(OsString, OsString)>,
    pub(crate) allow_control: bool,
}

impl RoomSpec {
    fn new(program: OsString, transport: Transport, room_number: usize) -> Self {
        let guest = Path::new(program.as_os_str())
            .file_name()
            .unwrap_or(program.as_os_str())
            .to_string_lossy();
        Self {
            title: format!("{guest} · {room_number}"),
            program,
            args: Vec::new(),
            transport,
            cwd: None,
            variables: Vec::new(),
            allow_control: false,
        }
    }

    fn configured(config: RoomConfig, room_number: usize) -> io::Result<Self> {
        if config.command.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "room command cannot be empty",
            ));
        }
        let program: OsString = config.command.into();
        let fallback = Path::new(program.as_os_str())
            .file_name()
            .unwrap_or(program.as_os_str())
            .to_string_lossy();
        let name = config.name.as_deref().unwrap_or(&fallback);
        if name.trim().is_empty() || name.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "room name cannot be empty or contain control characters",
            ));
        }

        Ok(Self {
            title: format!("{name} · {room_number}"),
            program,
            args: config.args.into_iter().map(Into::into).collect(),
            transport: config.transport,
            cwd: config.cwd,
            variables: Vec::new(),
            allow_control: config.allow_control,
        })
    }

    fn add_shared_toolbox(
        &mut self,
        servers: &[McpConfig],
        opencode_plugins: &[OpenCodePluginConfig],
    ) -> io::Result<()> {
        if servers.is_empty() && opencode_plugins.is_empty() {
            return Ok(());
        }
        let guest = Path::new(self.program.as_os_str())
            .file_name()
            .unwrap_or(self.program.as_os_str())
            .to_string_lossy()
            .to_ascii_lowercase();

        match guest.as_str() {
            "claude"
                if servers
                    .iter()
                    .any(|server| server.supports(McpClient::Claude)) =>
            {
                self.args.push("--mcp-config".into());
                self.args.push(claude_mcp_config(servers)?.into());
            }
            "codex"
                if servers
                    .iter()
                    .any(|server| server.supports(McpClient::Codex)) =>
            {
                self.prepend_args(codex_mcp_args(servers))
            }
            "opencode" => self.variables.push((
                "OPENCODE_CONFIG_CONTENT".into(),
                opencode_mcp_config(
                    env::var("OPENCODE_CONFIG_CONTENT").ok().as_deref(),
                    servers,
                    opencode_plugins,
                )?
                .into(),
            )),
            "claude" | "codex" => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{} cannot receive shared MCPs yet; supported commands are claude, codex, and opencode",
                        self.title
                    ),
                ));
            }
        }
        Ok(())
    }

    fn prepend_args(&mut self, args: impl IntoIterator<Item = OsString>) {
        let mut combined: Vec<_> = args.into_iter().collect();
        combined.append(&mut self.args);
        self.args = combined;
    }

    fn parse(value: OsString, room_number: usize) -> io::Result<Self> {
        let value = value.into_string().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "guest spec must be valid UTF-8",
            )
        })?;
        let (kind, program) = value.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "guest must be shell:PROGRAM or raw:PROGRAM",
            )
        })?;
        if program.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "guest program cannot be empty",
            ));
        }
        let transport = match kind {
            "shell" => Transport::Shell,
            "raw" => Transport::Raw,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "guest kind must be shell or raw",
                ));
            }
        };
        Ok(Self::new(program.into(), transport, room_number))
    }
}

impl McpConfig {
    pub(crate) fn supports(&self, client: McpClient) -> bool {
        self.clients.is_empty() || self.clients.contains(&client)
    }
}

pub(crate) fn validate_mcp_servers(servers: &[McpConfig]) -> io::Result<()> {
    let mut names = HashSet::new();
    for server in servers {
        if server.name.is_empty()
            || server.name.len() > 64
            || !server
                .name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MCP name must contain 1..=64 ASCII letters, digits, hyphens, or underscores",
            ));
        }
        if !names.insert(&server.name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate MCP name: {}", server.name),
            ));
        }
        if server.command.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("MCP {} command cannot be empty", server.name),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_opencode_plugins(plugins: &[OpenCodePluginConfig]) -> io::Result<()> {
    let mut packages = HashSet::new();
    for plugin in plugins {
        if plugin.package.trim().is_empty() || plugin.package.chars().any(char::is_control) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "OpenCode plugin package cannot be empty or contain control characters",
            ));
        }
        if !packages.insert(&plugin.package) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("duplicate OpenCode plugin package: {}", plugin.package),
            ));
        }
    }
    Ok(())
}

pub(crate) fn claude_mcp_config(servers: &[McpConfig]) -> io::Result<String> {
    let mut configured = serde_json::Map::new();
    for server in servers
        .iter()
        .filter(|server| server.supports(McpClient::Claude))
    {
        let mut entry = serde_json::json!({
            "command": server.command,
            "args": server.args,
        });
        if let Some(cwd) = &server.cwd {
            entry["cwd"] = serde_json::Value::String(cwd.to_string_lossy().into_owned());
        }
        configured.insert(server.name.clone(), entry);
    }
    serde_json::to_string(&serde_json::json!({ "mcpServers": configured }))
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn codex_mcp_args(servers: &[McpConfig]) -> Vec<OsString> {
    let mut args = Vec::new();
    for server in servers
        .iter()
        .filter(|server| server.supports(McpClient::Codex))
    {
        let prefix = format!("mcp_servers.{}", server.name);
        args.extend([
            "-c".into(),
            format!(
                "{prefix}.command={}",
                toml::Value::String(server.command.clone())
            )
            .into(),
            "-c".into(),
            format!(
                "{prefix}.args={}",
                toml::Value::Array(
                    server
                        .args
                        .iter()
                        .cloned()
                        .map(toml::Value::String)
                        .collect()
                )
            )
            .into(),
        ]);
        if let Some(cwd) = &server.cwd {
            args.extend([
                "-c".into(),
                format!(
                    "{prefix}.cwd={}",
                    toml::Value::String(cwd.to_string_lossy().into_owned())
                )
                .into(),
            ]);
        }
    }
    args
}

pub(crate) fn opencode_mcp_config(
    existing: Option<&str>,
    servers: &[McpConfig],
    plugins: &[OpenCodePluginConfig],
) -> io::Result<String> {
    let mut config = match existing {
        Some(existing) => serde_json::from_str(existing)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        None => serde_json::json!({}),
    };
    let root = config.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "OPENCODE_CONFIG_CONTENT must contain a JSON object",
        )
    })?;
    let open_code_servers: Vec<_> = servers
        .iter()
        .filter(|server| server.supports(McpClient::Opencode))
        .collect();
    if !open_code_servers.is_empty() {
        let mcp = root
            .entry("mcp")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OPENCODE_CONFIG_CONTENT mcp must be a JSON object",
                )
            })?;
        for server in open_code_servers {
            let mut command = vec![server.command.clone()];
            command.extend(server.args.iter().cloned());
            let mut entry = serde_json::json!({
                "type": "local",
                "command": command,
                "enabled": true,
            });
            if let Some(cwd) = &server.cwd {
                entry["cwd"] = serde_json::Value::String(cwd.to_string_lossy().into_owned());
            }
            mcp.insert(server.name.clone(), entry);
        }
    }
    if !plugins.is_empty() {
        let configured = root
            .entry("plugin")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "OPENCODE_CONFIG_CONTENT plugin must be an array",
                )
            })?;
        for plugin in plugins {
            let package = serde_json::Value::String(plugin.package.clone());
            if !configured.contains(&package) {
                configured.push(package);
            }
        }
    }

    serde_json::to_string(&config)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn parse_room_file(text: &str) -> io::Result<RoomFile> {
    toml::from_str(text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid crowded.toml: {error}"),
        )
    })
}

pub(crate) fn load_room_file(path: &Path) -> io::Result<RoomFile> {
    parse_room_file(&fs::read_to_string(path)?)
}

pub(crate) fn validate_room_file(file: &RoomFile) -> io::Result<()> {
    room_specs_from_file(file.clone(), false).map(drop)
}

fn add_shared_toolbox(
    rooms: &mut [RoomSpec],
    servers: &[McpConfig],
    opencode_plugins: &[OpenCodePluginConfig],
) -> io::Result<()> {
    validate_mcp_servers(servers)?;
    validate_opencode_plugins(opencode_plugins)?;
    for room in rooms {
        if room.transport == Transport::Raw {
            room.add_shared_toolbox(servers, opencode_plugins)?;
        }
    }
    Ok(())
}

fn room_specs_from_file(file: RoomFile, inject_shared_mcps: bool) -> io::Result<Vec<RoomSpec>> {
    if file.rooms.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "crowded.toml needs at least two rooms",
        ));
    }

    let mut rooms: Vec<_> = file
        .rooms
        .into_iter()
        .enumerate()
        .map(|(index, room)| RoomSpec::configured(room, index + 1))
        .collect::<io::Result<_>>()?;
    if inject_shared_mcps {
        add_shared_toolbox(&mut rooms, &file.mcp_servers, &file.opencode_plugins)?;
    } else {
        validate_mcp_servers(&file.mcp_servers)?;
        validate_opencode_plugins(&file.opencode_plugins)?;
    }
    Ok(rooms)
}

#[cfg(test)]
fn room_specs_from_toml(text: &str) -> io::Result<Vec<RoomSpec>> {
    room_specs_from_file(parse_room_file(text)?, true)
}

pub(crate) fn room_specs() -> io::Result<Vec<RoomSpec>> {
    let guests: Vec<_> = env::args_os().skip(1).collect();
    let config = Path::new("crowded.toml");
    let file = if config.try_exists()? {
        Some(load_room_file(config)?)
    } else {
        None
    };
    let inject_shared_mcps = !crate::toolbox::native_files_are_active()?;

    if guests.is_empty() {
        if let Some(file) = file {
            return room_specs_from_file(file, inject_shared_mcps);
        }
        let shell = env::var_os("SHELL").unwrap_or_else(|| {
            if cfg!(windows) {
                "cmd.exe".into()
            } else {
                "/bin/sh".into()
            }
        });
        return Ok(vec![
            RoomSpec::new(shell.clone(), Transport::Shell, 1),
            RoomSpec::new(shell, Transport::Shell, 2),
        ]);
    }
    if guests.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: crowded GUEST GUEST [GUEST ...]; a guest is shell:PROGRAM or raw:PROGRAM",
        ));
    }

    let mut rooms: Vec<_> = guests
        .into_iter()
        .enumerate()
        .map(|(index, guest)| RoomSpec::parse(guest, index + 1))
        .collect::<io::Result<_>>()?;
    if let Some(file) = file {
        if inject_shared_mcps {
            add_shared_toolbox(&mut rooms, &file.mcp_servers, &file.opencode_plugins)?;
        } else {
            validate_mcp_servers(&file.mcp_servers)?;
            validate_opencode_plugins(&file.opencode_plugins)?;
        }
    }
    Ok(rooms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_spec_selects_transport_and_name() {
        let guest = RoomSpec::parse("raw:codex".into(), 2).unwrap();
        assert_eq!(guest.transport, Transport::Raw);
        assert_eq!(guest.program, OsString::from("codex"));
        assert!(guest.args.is_empty());
        assert!(guest.cwd.is_none());
        assert_eq!(guest.title, "codex · 2");
    }

    #[test]
    fn toml_rooms_support_names_arguments_and_optional_cwd() {
        let rooms = room_specs_from_toml(
            r#"
                [[rooms]]
                name = "Claude"
                command = "claude"
                args = ["--continue"]
                transport = "raw"
                allow_control = true

                [[rooms]]
                command = "/bin/sh"
                transport = "shell"
                cwd = "examples"
            "#,
        )
        .unwrap();

        assert_eq!(rooms[0].title, "Claude · 1");
        assert_eq!(rooms[0].args, [OsString::from("--continue")]);
        assert!(rooms[0].allow_control);
        assert_eq!(rooms[1].title, "sh · 2");
        assert_eq!(rooms[1].cwd, Some(PathBuf::from("examples")));
        assert!(!rooms[1].allow_control);
        assert!(room_specs_from_toml("[[rooms]]\ncommand='codex'\ntransport='raw'").is_err());
        assert!(
            room_specs_from_toml("[[rooms]]\ncommand='codex'\ntransport='raw'\nworkdir='typo'")
                .is_err()
        );
    }

    #[test]
    fn shared_mcp_is_adapted_for_each_native_cli() {
        let rooms = room_specs_from_toml(
            r#"
                [[mcp]]
                name = "memory"
                command = "basic-memory"
                args = ["mcp"]
                cwd = "tools"

                [[rooms]]
                command = "claude"
                args = ["--continue"]
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"
            "#,
        )
        .unwrap();

        assert_eq!(rooms[0].args[0], "--continue");
        assert_eq!(rooms[0].args[1], "--mcp-config");
        let claude: serde_json::Value =
            serde_json::from_str(rooms[0].args[2].to_str().unwrap()).unwrap();
        assert_eq!(claude["mcpServers"]["memory"]["command"], "basic-memory");
        assert_eq!(claude["mcpServers"]["memory"]["args"][0], "mcp");
        assert_eq!(claude["mcpServers"]["memory"]["cwd"], "tools");

        let codex: Vec<_> = rooms[1]
            .args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect();
        assert!(codex.contains(&"mcp_servers.memory.command=\"basic-memory\"".into()));
        assert!(codex.contains(&"mcp_servers.memory.args=[\"mcp\"]".into()));
        assert!(codex.contains(&"mcp_servers.memory.cwd=\"tools\"".into()));

        let opencode: serde_json::Value = serde_json::from_str(
            rooms[2]
                .variables
                .iter()
                .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
                .unwrap()
                .1
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(opencode["mcp"]["memory"]["type"], "local");
        assert_eq!(opencode["mcp"]["memory"]["command"][0], "basic-memory");
        assert_eq!(opencode["mcp"]["memory"]["command"][1], "mcp");
        assert_eq!(opencode["mcp"]["memory"]["cwd"], "tools");
    }

    #[test]
    fn context_mode_uses_mcp_for_claude_and_codex_but_a_plugin_for_opencode() {
        let rooms = room_specs_from_toml(
            r#"
                [[mcp]]
                name = "context-mode"
                command = "npx"
                args = ["-y", "context-mode@1.0.169"]
                clients = ["claude", "codex"]

                [[opencode_plugin]]
                package = "context-mode@1.0.169"

                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"

                [[rooms]]
                command = "opencode"
                transport = "raw"
            "#,
        )
        .unwrap();

        assert!(
            rooms[0]
                .args
                .iter()
                .any(|arg| { arg.to_string_lossy().contains("context-mode@1.0.169") })
        );
        assert!(rooms[1].args.iter().any(|arg| {
            arg.to_string_lossy()
                .contains("mcp_servers.context-mode.command")
        }));
        let opencode: serde_json::Value = serde_json::from_str(
            rooms[2]
                .variables
                .iter()
                .find(|(key, _)| key == "OPENCODE_CONFIG_CONTENT")
                .unwrap()
                .1
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert!(opencode.get("mcp").is_none());
        assert_eq!(opencode["plugin"][0], "context-mode@1.0.169");
    }

    #[test]
    fn native_toolbox_mode_does_not_inject_launch_overrides() {
        let rooms = room_specs_from_file(
            parse_room_file(
                r#"
                    [[mcp]]
                    name = "memory"
                    command = "basic-memory"

                    [[rooms]]
                    command = "claude"
                    transport = "raw"

                    [[rooms]]
                    command = "codex"
                    transport = "raw"
                "#,
            )
            .unwrap(),
            false,
        )
        .unwrap();

        assert!(rooms.iter().all(|room| room.args.is_empty()));
        assert!(rooms.iter().all(|room| room.variables.is_empty()));
    }

    #[test]
    fn shared_mcp_skips_shell_rooms() {
        let rooms = room_specs_from_toml(
            r#"
                [[mcp]]
                name = "memory"
                command = "basic-memory"

                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "/bin/zsh"
                args = ["-l"]
                transport = "shell"
            "#,
        )
        .unwrap();

        assert_eq!(rooms[0].args[0], "--mcp-config");
        assert_eq!(rooms[1].args, [OsString::from("-l")]);
        assert!(rooms[1].variables.is_empty());
    }
}
