//! Room declarations and shared-toolbox configuration.

mod mcp;

use mcp::codex_mcp_args;
pub(crate) use mcp::{
    McpClient, McpConfig, McpTransport, OpenCodePluginConfig, claude_mcp_config,
    opencode_mcp_config, validate_mcp_servers, validate_opencode_plugins,
};

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Transport {
    Shell,
    Raw,
}

pub(crate) const DEFAULT_FUSE_LIMIT: usize = 20;
const TOKEN_PRICING_FILE: &str = "token_pricing.toml";
pub(crate) const CROWDED_TOML: &str = "crowded.toml";

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
    pub(crate) token_pricing: Vec<TokenPricing>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenPricing {
    pub(crate) model: String,
    pub(crate) input: f64,
    pub(crate) output: f64,
    pub(crate) cached_input: Option<f64>,
    pub(crate) cache_creation_input: Option<f64>,
    pub(crate) cache_read_input: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenPricingFile {
    #[serde(default)]
    token_pricing: Vec<TokenPricing>,
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
    model_tier: Option<String>,
    cost_tier: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RoomScheduling {
    pub(crate) model_tier: Option<String>,
    pub(crate) cost_tier: Option<String>,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
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
    pub(crate) scheduling: Option<RoomScheduling>,
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
            scheduling: None,
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
        let scheduling =
            configured_scheduling(config.model_tier, config.cost_tier, config.capabilities)?;

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
            scheduling,
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

fn configured_scheduling(
    model_tier: Option<String>,
    cost_tier: Option<String>,
    capabilities: Vec<String>,
) -> io::Result<Option<RoomScheduling>> {
    validate_scheduling_value(
        model_tier.as_deref(),
        "model_tier",
        &["fast", "balanced", "deep"],
    )?;
    validate_scheduling_value(
        cost_tier.as_deref(),
        "cost_tier",
        &["low", "medium", "high"],
    )?;
    for capability in &capabilities {
        validate_scheduling_value(
            Some(capability),
            "capabilities",
            &["produce", "implement", "validate", "qa", "audit"],
        )?;
    }
    if model_tier.is_none() && cost_tier.is_none() && capabilities.is_empty() {
        Ok(None)
    } else {
        Ok(Some(RoomScheduling {
            model_tier,
            cost_tier,
            capabilities,
        }))
    }
}

fn validate_scheduling_value(value: Option<&str>, field: &str, allowed: &[&str]) -> io::Result<()> {
    if value.is_none_or(|value| allowed.contains(&value)) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("room {field} must be one of {}", allowed.join(", ")),
    ))
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

fn load_token_pricing_file(path: &Path) -> io::Result<Vec<TokenPricing>> {
    if !path.try_exists()? {
        return Ok(Vec::new());
    }
    let file: TokenPricingFile = toml::from_str(&fs::read_to_string(path)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {TOKEN_PRICING_FILE}: {error}"),
        )
    })?;
    validate_token_pricing(&file.token_pricing)?;
    Ok(file.token_pricing)
}

pub(crate) fn validate_room_file(file: &RoomFile) -> io::Result<()> {
    // Validate the full launch-time room resolution, including shared-toolbox
    // injection for raw rooms. `crowded check` must reject the same unsupported
    // raw guest plus shared-MCP configuration that a normal launch rejects,
    // so `inject_shared_mcps` is true here (matching the pre-sync launch path);
    // validation is read-only and never writes files.
    room_specs_from_file(file.clone(), true, Vec::new()).map(drop)
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

fn room_specs_from_file(
    file: RoomFile,
    inject_shared_mcps: bool,
    token_pricing: Vec<TokenPricing>,
) -> io::Result<ResolvedConfig> {
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
        token_pricing,
    })
}

pub(crate) fn parse_fuse_size_input(input: &str) -> Result<usize, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("fuse_size cannot be empty".to_owned());
    }
    trimmed
        .parse::<usize>()
        .map_err(|_| format!("fuse_size must be a non-negative integer, got '{trimmed}'"))
}

pub(crate) fn persist_fuse_size(path: &Path, new_size: usize) -> io::Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut doc = if text.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        text.parse::<toml_edit::DocumentMut>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {CROWDED_TOML}: {error}"),
            )
        })?
    };
    doc["fuse_size"] = toml_edit::value(new_size as i64);
    fs::write(path, doc.to_string())
}

pub(crate) fn parse_allow_control_input(input: &str) -> Result<bool, String> {
    match input.trim() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        other => Err(format!(
            "allow_control must be true or false, got '{other}'"
        )),
    }
}

pub(crate) fn parse_model_tier_input(input: &str) -> Result<String, String> {
    parse_scheduling_enum_input("model_tier", input, &["fast", "balanced", "deep"])
}

pub(crate) fn parse_cost_tier_input(input: &str) -> Result<String, String> {
    parse_scheduling_enum_input("cost_tier", input, &["low", "medium", "high"])
}

fn parse_scheduling_enum_input(
    field: &str,
    input: &str,
    allowed: &[&str],
) -> Result<String, String> {
    let trimmed = input.trim();
    if allowed.contains(&trimmed) {
        Ok(trimmed.to_owned())
    } else {
        Err(format!(
            "room {field} must be one of {}",
            allowed.join(", ")
        ))
    }
}

pub(crate) fn parse_capabilities_input(input: &str) -> Result<Vec<String>, String> {
    const ALLOWED: [&str; 5] = ["produce", "implement", "validate", "qa", "audit"];
    let mut capabilities = Vec::new();
    for part in input.split(',') {
        let capability = part.trim();
        if capability.is_empty() {
            continue;
        }
        if !ALLOWED.contains(&capability) {
            return Err(format!(
                "capability '{capability}' invalid; must be one of {}",
                ALLOWED.join(", ")
            ));
        }
        capabilities.push(capability.to_owned());
    }
    Ok(capabilities)
}

#[derive(Clone, Debug, Default)]
pub(crate) struct RoomFieldUpdate {
    pub(crate) allow_control: Option<bool>,
    pub(crate) model_tier: Option<String>,
    pub(crate) cost_tier: Option<String>,
    pub(crate) capabilities: Option<Vec<String>>,
}

impl RoomFieldUpdate {
    pub(crate) fn is_empty(&self) -> bool {
        self.allow_control.is_none()
            && self.model_tier.is_none()
            && self.cost_tier.is_none()
            && self.capabilities.is_none()
    }
}

/// Persist per-room scheduling fields, mirroring [`persist_fuse_size`]'s
/// parse-or-create, set-the-touched-field, write-back shape so unrelated
/// content in the document is preserved byte-for-byte.
pub(crate) fn persist_room_fields(
    path: &Path,
    room_index: usize,
    updates: &RoomFieldUpdate,
) -> io::Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let mut doc = if text.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        text.parse::<toml_edit::DocumentMut>().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid {CROWDED_TOML}: {error}"),
            )
        })?
    };
    let rooms = doc["rooms"].as_array_of_tables_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "crowded.toml has no [[rooms]] table",
        )
    })?;
    let room = rooms.get_mut(room_index).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("room {} not found", room_index + 1),
        )
    })?;
    if let Some(value) = updates.allow_control {
        room["allow_control"] = toml_edit::value(value);
    }
    if let Some(value) = &updates.model_tier {
        room["model_tier"] = toml_edit::value(value.as_str());
    }
    if let Some(value) = &updates.cost_tier {
        room["cost_tier"] = toml_edit::value(value.as_str());
    }
    if let Some(capabilities) = &updates.capabilities {
        let mut array = toml_edit::Array::new();
        for capability in capabilities {
            array.push(capability.as_str());
        }
        room["capabilities"] = toml_edit::value(array);
    }
    fs::write(path, doc.to_string())
}

pub(crate) fn crowded_toml_path() -> PathBuf {
    PathBuf::from(CROWDED_TOML)
}

fn validate_token_pricing(pricing: &[TokenPricing]) -> io::Result<()> {
    let mut models = HashSet::new();
    for rate in pricing {
        if rate.model.trim().is_empty() || !models.insert(&rate.model) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "token pricing models must be non-empty and unique",
            ));
        }
        if [
            Some(rate.input),
            Some(rate.output),
            rate.cached_input,
            rate.cache_creation_input,
            rate.cache_read_input,
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.is_finite() || value < 0.0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "token pricing values must be finite and non-negative",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
fn room_specs_from_toml(text: &str) -> io::Result<ResolvedConfig> {
    room_specs_from_file(parse_room_file(text)?, true, Vec::new())
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
    let token_pricing = load_token_pricing_file(&config.with_file_name(TOKEN_PRICING_FILE))?;
    let file = if config.try_exists()? {
        Some(load_room_file(config)?)
    } else {
        None
    };
    let inject_shared_mcps = !crate::toolbox::native_files_are_active()?;

    if guests.is_empty() {
        if let Some(file) = file {
            return room_specs_from_file(file, inject_shared_mcps, token_pricing);
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
            token_pricing: Vec::new(),
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
        token_pricing,
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
    fn room_scheduling_parses_and_omits_empty_metadata() {
        let rooms = room_specs_from_toml(
            r#"
[[rooms]]
command = "codex"
transport = "raw"
model_tier = "balanced"
cost_tier = "medium"
capabilities = ["implement", "qa"]

[[rooms]]
command = "claude"
transport = "raw"
"#,
        )
        .unwrap()
        .specs;
        assert_eq!(
            rooms[0].scheduling,
            Some(RoomScheduling {
                model_tier: Some("balanced".to_owned()),
                cost_tier: Some("medium".to_owned()),
                capabilities: vec!["implement".to_owned(), "qa".to_owned()],
            })
        );
        assert_eq!(rooms[1].scheduling, None);
    }

    #[test]
    fn invalid_room_scheduling_values_are_rejected() {
        for (field, name) in [
            ("model_tier = \"slow\"", "model_tier"),
            ("cost_tier = \"free\"", "cost_tier"),
            ("capabilities = [\"deploy\"]", "capabilities"),
        ] {
            let result = room_specs_from_toml(&format!(
                "[[rooms]]\ncommand = \"codex\"\ntransport = \"raw\"\n{field}\n\n[[rooms]]\ncommand = \"claude\"\ntransport = \"raw\""
            ));
            let Err(error) = result else {
                panic!("invalid {field} should fail");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains(name));
        }
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
    fn token_pricing_file_is_optional_and_model_keyed() {
        let path =
            env::temp_dir().join(format!("crowded-token-pricing-{}.toml", std::process::id()));
        let _ = fs::remove_file(&path);
        fs::write(
            &path,
            r#"
                [[token_pricing]]
                model = "claude-sonnet-5"
                input = 0.000003
                cache_creation_input = 0.00000375
                cache_read_input = 0.0000003
                output = 0.000015
            "#,
        )
        .unwrap();

        let pricing = load_token_pricing_file(&path).unwrap();
        assert_eq!(pricing.len(), 1);
        assert_eq!(pricing[0].model, "claude-sonnet-5");
        assert_eq!(pricing[0].cached_input, None);
        fs::remove_file(&path).unwrap();
        assert!(load_token_pricing_file(&path).unwrap().is_empty());
    }

    #[test]
    fn token_pricing_in_crowded_toml_is_rejected() {
        assert!(
            room_specs_from_toml(
                r#"
                [[rooms]]
                command = "claude"
                transport = "raw"

                [[rooms]]
                command = "codex"
                transport = "raw"

                [[token_pricing]]
                model = "claude-sonnet-5"
                input = 0.000003
                output = 0.000015
            "#,
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
            Vec::new(),
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

    #[test]
    fn parse_fuse_size_input_accepts_zero_and_positive() {
        assert_eq!(parse_fuse_size_input("0").unwrap(), 0);
        assert_eq!(parse_fuse_size_input("20").unwrap(), 20);
        assert_eq!(parse_fuse_size_input("  10  ").unwrap(), 10);
    }

    #[test]
    fn parse_fuse_size_input_rejects_non_numeric() {
        assert!(parse_fuse_size_input("abc").is_err());
        assert!(parse_fuse_size_input("12.3").is_err());
        assert!(parse_fuse_size_input("").is_err());
        assert!(parse_fuse_size_input("-1").is_err());
        let err = parse_fuse_size_input("not-a-number").unwrap_err();
        assert!(err.contains("non-negative integer"));
    }

    #[test]
    fn persist_fuse_size_round_trip_preserves_other_sections() {
        let dir = env::temp_dir().join(format!("crowded-fuse-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("crowded.toml");
        let initial = r#"fuse_size = 20

[[rooms]]
command = "claude"
transport = "raw"

[[rooms]]
command = "codex"
transport = "raw"

[[mcp]]
name = "memory"
command = "basic-memory"
"#;
        fs::write(&path, initial).unwrap();

        persist_fuse_size(&path, 5).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("fuse_size = 5"));
        assert!(updated.contains("[[mcp]]"));
        assert!(updated.contains("[[rooms]]"));
        let parsed: RoomFile = toml::from_str(&updated).unwrap();
        assert_eq!(parsed.fuse_size, Some(5));

        // 0 = unlimited must persist correctly
        persist_fuse_size(&path, 0).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("fuse_size = 0"));
        let parsed: RoomFile = toml::from_str(&updated).unwrap();
        assert_eq!(parsed.fuse_size, Some(0));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn persist_fuse_size_leaves_file_unchanged_on_invalid_input() {
        let dir = env::temp_dir().join(format!("crowded-fuse-invalid-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("crowded.toml");
        let initial = r#"fuse_size = 20

[[rooms]]
command = "claude"
transport = "raw"

[[rooms]]
command = "codex"
transport = "raw"
"#;
        fs::write(&path, initial).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        // Simulate validation failure: do NOT call persist_fuse_size
        let err = parse_fuse_size_input("abc");
        assert!(err.is_err());
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn parse_room_field_inputs_accept_valid_values() {
        assert!(parse_allow_control_input("true").unwrap());
        assert!(!parse_allow_control_input("false").unwrap());
        assert_eq!(parse_model_tier_input(" deep ").unwrap(), "deep");
        assert_eq!(parse_cost_tier_input("low").unwrap(), "low");
        assert_eq!(
            parse_capabilities_input("produce, validate,qa").unwrap(),
            vec!["produce".to_owned(), "validate".to_owned(), "qa".to_owned()]
        );
        assert!(parse_capabilities_input("").unwrap().is_empty());
    }

    #[test]
    fn parse_room_field_inputs_reject_invalid_values() {
        assert!(parse_allow_control_input("maybe").is_err());
        assert!(parse_model_tier_input("ultra").is_err());
        assert!(parse_cost_tier_input("expensive").is_err());
        assert!(parse_capabilities_input("bogus").is_err());
    }

    #[test]
    fn persist_room_fields_round_trip_preserves_other_sections() {
        let dir = env::temp_dir().join(format!("crowded-room-fields-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("crowded.toml");
        let initial = r#"fuse_size = 20

[[rooms]]
command = "claude"
transport = "raw"

[[rooms]]
command = "codex"
transport = "raw"
model_tier = "fast"
cost_tier = "low"
capabilities = ["implement"]

[[mcp]]
name = "memory"
command = "basic-memory"
"#;
        fs::write(&path, initial).unwrap();

        let updates = RoomFieldUpdate {
            allow_control: Some(true),
            model_tier: Some("balanced".to_owned()),
            cost_tier: Some("medium".to_owned()),
            capabilities: Some(vec!["produce".to_owned(), "qa".to_owned()]),
        };
        persist_room_fields(&path, 0, &updates).unwrap();

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("allow_control = true"));
        assert!(updated.contains("model_tier = \"balanced\""));
        assert!(updated.contains("cost_tier = \"medium\""));
        assert!(updated.contains("capabilities = [\"produce\", \"qa\"]"));
        // Unrelated content is preserved.
        assert!(updated.contains("fuse_size = 20"));
        assert!(updated.contains("[[mcp]]"));
        // The untouched room keeps its original scheduling.
        assert!(updated.contains("model_tier = \"fast\""));
        assert!(updated.contains("capabilities = [\"implement\"]"));
        let parsed: RoomFile = toml::from_str(&updated).unwrap();
        assert!(parsed.rooms[0].allow_control);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn persist_room_fields_only_touches_the_indexed_room() {
        let dir = env::temp_dir().join(format!("crowded-room-touch-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("crowded.toml");
        let initial = r#"[[rooms]]
command = "claude"
transport = "raw"

[[rooms]]
command = "codex"
transport = "raw"
allow_control = true
"#;
        fs::write(&path, initial).unwrap();

        persist_room_fields(
            &path,
            1,
            &RoomFieldUpdate {
                allow_control: Some(false),
                ..RoomFieldUpdate::default()
            },
        )
        .unwrap();

        let updated = fs::read_to_string(&path).unwrap();
        assert!(updated.contains("allow_control = false"));
        assert_eq!(updated.matches("allow_control").count(), 1);
        assert!(updated.contains("command = \"claude\""));
        let parsed: RoomFile = toml::from_str(&updated).unwrap();
        assert!(!parsed.rooms[1].allow_control);
        assert!(!parsed.rooms[0].allow_control);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn persist_room_fields_out_of_range_room_leaves_file_unchanged() {
        let dir = env::temp_dir().join(format!("crowded-room-range-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("crowded.toml");
        let initial = r#"[[rooms]]
command = "claude"
transport = "raw"

[[rooms]]
command = "codex"
transport = "raw"
"#;
        fs::write(&path, initial).unwrap();
        let before = fs::read_to_string(&path).unwrap();

        let err = persist_room_fields(
            &path,
            5,
            &RoomFieldUpdate {
                allow_control: Some(true),
                ..RoomFieldUpdate::default()
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("room 6 not found"));
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }
}
