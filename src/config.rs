//! Room declarations and shared-toolbox configuration.

mod mcp;

use mcp::codex_mcp_args;
pub(crate) use mcp::{
    McpClient, McpConfig, OpenCodePluginConfig, claude_mcp_config, opencode_mcp_config,
    validate_mcp_servers, validate_opencode_plugins,
};

use std::{
    env,
    ffi::{OsStr, OsString},
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

pub(crate) const DEFAULT_FUSE_LIMIT: usize = 20;

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
    /// Automatic delivery fuse limit. 0 means unlimited (never trips).
    /// Omitting the field defaults to [`DEFAULT_FUSE_LIMIT`].
    #[serde(default)]
    pub(crate) fuse_size: Option<usize>,
}

/// Config values resolved from `RoomFile` for use at runtime.
pub(crate) struct ResolvedConfig {
    pub(crate) specs: Vec<RoomSpec>,
    pub(crate) fuse_size: usize,
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
    pub(crate) provides: Option<String>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RoomConfig {
    name: Option<String>,
    vendor: Option<String>,
    pub(crate) command: String,
    #[serde(default)]
    args: Vec<String>,
    pub(crate) transport: Transport,
    pub(crate) cwd: Option<PathBuf>,
    #[serde(default)]
    pub(crate) allow_control: bool,
    #[serde(default)]
    pub(crate) use_headroom: bool,
    #[serde(default)]
    headroom_args: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct RoomSpec {
    pub(crate) name: String,
    pub(crate) vendor: String,
    pub(crate) title: String,
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) transport: Transport,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) variables: Vec<(OsString, OsString)>,
    pub(crate) allow_control: bool,
    pub(crate) use_headroom: bool,
    /// Args for the `headroom` wrapper itself (e.g. its own flags), placed
    /// after the wrapped program name and before the program's own args:
    /// `headroom wrap <program> <headroom_args...> <args...>`. Ignored when
    /// `use_headroom` is false or `headroom` is not found on PATH.
    pub(crate) headroom_args: Vec<OsString>,
}

impl RoomSpec {
    fn new(program: OsString, transport: Transport, room_number: usize) -> Self {
        let guest = Path::new(program.as_os_str())
            .file_name()
            .unwrap_or(program.as_os_str())
            .to_string_lossy();
        Self {
            name: guest.to_string(),
            vendor: inferred_vendor(program.as_os_str()).to_owned(),
            title: format!("{guest} · {room_number}"),
            program,
            args: Vec::new(),
            transport,
            cwd: None,
            variables: Vec::new(),
            allow_control: false,
            use_headroom: false,
            headroom_args: Vec::new(),
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
        let vendor = configured_vendor(program.as_os_str(), config.vendor.as_deref())?;

        Ok(Self {
            name: name.to_owned(),
            vendor,
            title: format!("{name} · {room_number}"),
            program,
            args: config.args.into_iter().map(Into::into).collect(),
            transport: config.transport,
            cwd: config.cwd,
            variables: Vec::new(),
            allow_control: config.allow_control,
            use_headroom: config.use_headroom,
            headroom_args: config.headroom_args.into_iter().map(Into::into).collect(),
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

fn inferred_vendor(program: &OsStr) -> &'static str {
    let guest = Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .to_ascii_lowercase();
    match guest.as_str() {
        "claude" => "anthropic",
        "codex" => "openai",
        _ => "unknown",
    }
}

fn configured_vendor(program: &OsStr, configured: Option<&str>) -> io::Result<String> {
    let vendor = configured
        .unwrap_or_else(|| inferred_vendor(program))
        .trim()
        .to_ascii_lowercase();
    if vendor.is_empty()
        || !vendor.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "room vendor must contain only letters, numbers, '.', '_', or '-'",
        ));
    }
    Ok(vendor)
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

fn room_specs_from_file(file: RoomFile, inject_shared_mcps: bool) -> io::Result<ResolvedConfig> {
    if file.rooms.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "crowded.toml needs at least two rooms",
        ));
    }

    let fuse_size = file.fuse_size.unwrap_or(DEFAULT_FUSE_LIMIT);

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
    Ok(ResolvedConfig {
        specs: rooms,
        fuse_size,
    })
}

#[cfg(test)]
fn room_specs_from_toml(text: &str) -> io::Result<ResolvedConfig> {
    room_specs_from_file(parse_room_file(text)?, true)
}

pub(crate) fn room_specs() -> io::Result<ResolvedConfig> {
    room_specs_skipping(1)
}

/// Same room resolution as [`room_specs`], but skipping one extra leading
/// argument. Used by the `crowded resume` subcommand, whose own name
/// occupies the position a guest list would otherwise start at.
pub(crate) fn room_specs_resumed() -> io::Result<ResolvedConfig> {
    room_specs_skipping(2)
}

fn room_specs_skipping(skip: usize) -> io::Result<ResolvedConfig> {
    let guests: Vec<_> = env::args_os().skip(skip).collect();
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
        return Ok(ResolvedConfig {
            specs: vec![
                RoomSpec::new(shell.clone(), Transport::Shell, 1),
                RoomSpec::new(shell, Transport::Shell, 2),
            ],
            fuse_size: DEFAULT_FUSE_LIMIT,
        });
    }
    if guests.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: crowded GUEST GUEST [GUEST ...]; a guest is shell:PROGRAM or raw:PROGRAM",
        ));
    }

    let fuse_size = file
        .as_ref()
        .and_then(|f| f.fuse_size)
        .unwrap_or(DEFAULT_FUSE_LIMIT);
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
    Ok(ResolvedConfig {
        specs: rooms,
        fuse_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_spec_selects_transport_and_name() {
        let guest = RoomSpec::parse("raw:codex".into(), 2).unwrap();
        assert_eq!(guest.transport, Transport::Raw);
        assert_eq!(guest.program, OsString::from("codex"));
        assert_eq!(guest.vendor, "openai");
        assert!(guest.args.is_empty());
        assert!(guest.cwd.is_none());
        assert!(!guest.allow_control);
        assert!(!guest.use_headroom);
        assert!(guest.headroom_args.is_empty());
        assert_eq!(guest.title, "codex · 2");
    }

    #[test]
    fn toml_rooms_support_names_arguments_and_optional_cwd() {
        let config = room_specs_from_toml(
            r#"
                [[rooms]]
                name = "Claude"
                command = "claude"
                args = ["--continue"]
                transport = "raw"
                allow_control = true
                use_headroom = true
                headroom_args = ["--budget", "5000"]

                [[rooms]]
                command = "/bin/sh"
                vendor = "Local"
                transport = "shell"
                cwd = "examples"
            "#,
        )
        .unwrap();
        let rooms = &config.specs;

        assert_eq!(rooms[0].title, "Claude · 1");
        assert_eq!(rooms[0].args, [OsString::from("--continue")]);
        assert!(rooms[0].allow_control);
        assert!(rooms[0].use_headroom);
        assert_eq!(
            rooms[0].headroom_args,
            [OsString::from("--budget"), OsString::from("5000")]
        );
        assert_eq!(rooms[0].vendor, "anthropic");
        assert_eq!(rooms[1].title, "sh · 2");
        assert_eq!(rooms[1].vendor, "local");
        assert_eq!(rooms[1].cwd, Some(PathBuf::from("examples")));
        assert!(!rooms[1].allow_control);
        assert!(!rooms[1].use_headroom);
        assert!(rooms[1].headroom_args.is_empty());
        assert_eq!(config.fuse_size, DEFAULT_FUSE_LIMIT);
        assert!(room_specs_from_toml("[[rooms]]\ncommand='codex'\ntransport='raw'").is_err());
        assert!(
            room_specs_from_toml("[[rooms]]\ncommand='codex'\ntransport='raw'\nworkdir='typo'")
                .is_err()
        );
        assert!(
            room_specs_from_toml(
                "[[rooms]]\ncommand='codex'\nvendor='open ai'\ntransport='raw'\n[[rooms]]\ncommand='claude'\ntransport='raw'"
            )
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
        .unwrap()
        .specs;

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
    fn remote_mcps_are_adapted_for_each_native_cli() {
        let rooms = room_specs_from_toml(
            r#"
                [[mcp]]
                name = "streamable"
                url = "https://example.com/mcp"
                transport = "http"

                [[mcp]]
                name = "legacy"
                url = "https://example.com/sse"
                transport = "sse"
                clients = ["claude", "opencode"]

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
        .unwrap()
        .specs;

        let claude: serde_json::Value =
            serde_json::from_str(rooms[0].args[1].to_str().unwrap()).unwrap();
        assert_eq!(claude["mcpServers"]["streamable"]["type"], "http");
        assert_eq!(
            claude["mcpServers"]["streamable"]["url"],
            "https://example.com/mcp"
        );
        assert_eq!(claude["mcpServers"]["legacy"]["type"], "sse");
        assert_eq!(
            claude["mcpServers"]["legacy"]["url"],
            "https://example.com/sse"
        );

        let codex: Vec<_> = rooms[1]
            .args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect();
        assert!(codex.contains(&"mcp_servers.streamable.url=\"https://example.com/mcp\"".into()));
        assert!(codex.iter().all(|arg| !arg.contains("legacy")));

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
        assert_eq!(opencode["mcp"]["streamable"]["type"], "remote");
        assert_eq!(
            opencode["mcp"]["streamable"]["url"],
            "https://example.com/mcp"
        );
        assert_eq!(opencode["mcp"]["streamable"]["enabled"], true);
        assert_eq!(opencode["mcp"]["legacy"]["type"], "remote");
        assert_eq!(opencode["mcp"]["legacy"]["url"], "https://example.com/sse");
    }

    #[test]
    fn legacy_sse_cannot_target_codex() {
        let error = room_specs_from_toml(
            r#"
                [[mcp]]
                name = "legacy"
                url = "https://example.com/sse"
                transport = "sse"

                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"
            "#,
        )
        .err()
        .unwrap();

        assert!(error.to_string().contains("cannot target Codex"));
    }

    #[test]
    fn remote_mcp_url_requires_a_host() {
        for url in ["http://", "https:///mcp", "http://?x=1"] {
            let config = format!(
                r#"
                    [[mcp]]
                    name = "remote"
                    url = "{url}"
                    transport = "http"

                    [[rooms]]
                    command = "claude"
                    transport = "raw"

                    [[rooms]]
                    command = "codex"
                    transport = "raw"
                "#
            );
            let error = room_specs_from_toml(&config).err().unwrap();
            assert!(
                error.to_string().contains("must be an HTTP(S) URL"),
                "{url}: {error}"
            );
        }

        for url in ["http://localhost:8080/mcp", "http://[::1]:8080/mcp"] {
            let config = format!(
                r#"
                    [[mcp]]
                    name = "remote"
                    url = "{url}"
                    transport = "http"

                    [[rooms]]
                    command = "claude"
                    transport = "raw"

                    [[rooms]]
                    command = "codex"
                    transport = "raw"
                "#
            );
            assert!(room_specs_from_toml(&config).is_ok(), "{url}");
        }
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
        .unwrap()
        .specs;

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
        .unwrap()
        .specs;

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
        .unwrap()
        .specs;

        assert_eq!(rooms[0].args[0], "--mcp-config");
        assert_eq!(rooms[1].args, [OsString::from("-l")]);
        assert!(rooms[1].variables.is_empty());
    }

    #[test]
    fn fuse_size_defaults_to_20_when_omitted() {
        let config = room_specs_from_toml(
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
        assert_eq!(config.fuse_size, 20);
    }

    #[test]
    fn fuse_size_is_configurable() {
        let config = room_specs_from_toml(
            r#"
                fuse_size = 10

                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"
            "#,
        )
        .unwrap();
        assert_eq!(config.fuse_size, 10);
    }

    #[test]
    fn fuse_size_zero_means_unlimited() {
        let config = room_specs_from_toml(
            r#"
                fuse_size = 0

                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"
            "#,
        )
        .unwrap();
        assert_eq!(config.fuse_size, 0);
    }
}
