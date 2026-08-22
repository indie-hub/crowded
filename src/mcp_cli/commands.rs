//! Command-line client for managing shared MCP servers.

use std::{env, io, path::Path};

use crate::config::{McpClient, McpConfig, McpTransport};

use super::{NewMcpEntry, add_entry, list_entries, remove_entry};

const USAGE: &str = "usage: crowded mcp list | add NAME [--command CMD] [--args ...] \
    [--url URL] [--transport http|sse] [--clients claude,codex,opencode] | remove NAME";
const ADD_USAGE: &str = "usage: crowded mcp add NAME [--command CMD] [--args ...] \
    [--url URL] [--transport http|sse] [--clients claude,codex,opencode]";
const REMOVE_USAGE: &str = "usage: crowded mcp remove NAME";

pub(crate) fn command() -> Result<(), Box<dyn std::error::Error>> {
    run(&env::current_dir()?, env::args().skip(2))
}

fn run(
    root: &Path,
    args: impl IntoIterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut args = args.into_iter();
    let action = args.next().ok_or_else(|| invalid_input(USAGE))?;

    match action.as_str() {
        "list" => {
            if args.next().is_some() {
                return Err(invalid_input("usage: crowded mcp list").into());
            }
            let entries = list_entries(root)?;
            if entries.is_empty() {
                println!("no MCP servers configured");
            } else {
                for entry in &entries {
                    println!("{}", format_entry(entry));
                }
            }
            Ok(())
        }
        "add" => {
            let entry = parse_add_args(args)?;
            println!("added MCP server {}", entry.name);
            add_entry(root, entry)?;
            Ok(())
        }
        "remove" => {
            let name = parse_remove_args(args)?;
            remove_entry(root, &name)?;
            println!("removed MCP server {name}");
            Ok(())
        }
        _ => Err(invalid_input(USAGE).into()),
    }
}

fn parse_add_args(args: impl IntoIterator<Item = String>) -> Result<NewMcpEntry, String> {
    let mut args = args.into_iter().peekable();
    let name = args.next().ok_or_else(|| ADD_USAGE.to_owned())?;
    let mut command = None;
    let mut url = None;
    let mut transport = None;
    let mut clients = Vec::new();
    let mut entry_args = Vec::new();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--command" => {
                let value = args.next().ok_or_else(|| ADD_USAGE.to_owned())?;
                if command.replace(value).is_some() {
                    return Err("--command may appear only once".to_owned());
                }
            }
            "--url" => {
                let value = args.next().ok_or_else(|| ADD_USAGE.to_owned())?;
                if url.replace(value).is_some() {
                    return Err("--url may appear only once".to_owned());
                }
            }
            "--transport" => {
                let value = args.next().ok_or_else(|| ADD_USAGE.to_owned())?;
                let parsed = match value.as_str() {
                    "http" => McpTransport::Http,
                    "sse" => McpTransport::Sse,
                    _ => return Err("--transport must be http or sse".to_owned()),
                };
                if transport.replace(parsed).is_some() {
                    return Err("--transport may appear only once".to_owned());
                }
            }
            "--clients" => {
                let value = args.next().ok_or_else(|| ADD_USAGE.to_owned())?;
                if !clients.is_empty() {
                    return Err("--clients may appear only once".to_owned());
                }
                for client in value.split(',') {
                    let client = match client {
                        "claude" => McpClient::Claude,
                        "codex" => McpClient::Codex,
                        "opencode" => McpClient::Opencode,
                        _ => {
                            return Err(format!(
                                "unknown client `{client}`; expected claude, codex, or opencode"
                            ));
                        }
                    };
                    clients.push(client);
                }
            }
            "--args" => {
                while args.peek().is_some_and(|next| !is_flag(next)) {
                    entry_args.push(args.next().unwrap());
                }
            }
            _ => return Err(ADD_USAGE.to_owned()),
        }
    }

    if command.is_none() && url.is_none() {
        return Err("mcp add requires --command or --url".to_owned());
    }

    Ok(NewMcpEntry {
        name,
        command,
        args: entry_args,
        url,
        transport,
        clients,
    })
}

fn parse_remove_args(args: impl IntoIterator<Item = String>) -> Result<String, String> {
    let mut args = args.into_iter();
    let name = args.next().ok_or_else(|| REMOVE_USAGE.to_owned())?;
    if args.next().is_some() {
        return Err(REMOVE_USAGE.to_owned());
    }
    Ok(name)
}

fn is_flag(token: &str) -> bool {
    matches!(
        token,
        "--command" | "--args" | "--url" | "--transport" | "--clients"
    )
}

fn format_entry(entry: &McpConfig) -> String {
    let target = match entry.url() {
        Some(url) => format!("url {url}"),
        None => {
            let mut target = entry.command().unwrap_or_default().to_owned();
            if !entry.args.is_empty() {
                target.push(' ');
                target.push_str(&entry.args.join(" "));
            }
            target
        }
    };
    let transport = match entry.transport {
        Some(McpTransport::Http) => "http",
        Some(McpTransport::Sse) => "sse",
        None => "stdio",
    };
    let clients = if entry.clients.is_empty() {
        "all".to_owned()
    } else {
        entry
            .clients
            .iter()
            .map(|&client| client_name(client))
            .collect::<Vec<_>>()
            .join(",")
    };
    format!("{} {target} {transport} {clients}", entry.name)
}

fn client_name(client: McpClient) -> &'static str {
    match client {
        McpClient::Claude => "claude",
        McpClient::Codex => "codex",
        McpClient::Opencode => "opencode",
    }
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn add_parses_local_command_with_args() {
        let entry = parse_add_args(args(&[
            "everything",
            "--command",
            "npx",
            "--args",
            "-y",
            "@modelcontextprotocol/server-everything",
            "--clients",
            "claude,codex",
        ]))
        .unwrap();
        assert_eq!(entry.name, "everything");
        assert_eq!(entry.command.as_deref(), Some("npx"));
        assert_eq!(
            entry.args,
            ["-y", "@modelcontextprotocol/server-everything"]
        );
        assert_eq!(entry.url, None);
        assert_eq!(entry.transport, None);
        assert_eq!(entry.clients, [McpClient::Claude, McpClient::Codex]);
    }

    #[test]
    fn add_parses_remote_url() {
        let entry = parse_add_args(args(&[
            "streamable",
            "--url",
            "https://example.com/mcp",
            "--transport",
            "http",
        ]))
        .unwrap();
        assert_eq!(entry.name, "streamable");
        assert_eq!(entry.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(entry.transport, Some(McpTransport::Http));
        assert!(entry.command.is_none());
    }

    #[test]
    fn add_rejects_missing_name() {
        assert!(parse_add_args(args(&[])).is_err());
    }

    #[test]
    fn add_rejects_missing_command_and_url() {
        assert!(parse_add_args(args(&["solo"])).is_err());
    }

    #[test]
    fn add_rejects_unknown_transport() {
        assert!(parse_add_args(args(&["solo", "--command", "npx", "--transport", "ws"])).is_err());
    }

    #[test]
    fn add_rejects_unknown_client() {
        assert!(
            parse_add_args(args(&["solo", "--command", "npx", "--clients", "gemini"])).is_err()
        );
    }

    #[test]
    fn add_rejects_duplicate_flag() {
        assert!(parse_add_args(args(&["solo", "--command", "npx", "--command", "echo"])).is_err());
    }

    #[test]
    fn add_accepts_unknown_flag_as_arg_after_args_marker() {
        let entry =
            parse_add_args(args(&["solo", "--command", "npx", "--args", "--yolo"])).unwrap();
        assert_eq!(entry.args, ["--yolo"]);
    }

    #[test]
    fn remove_parses_name_and_rejects_extras() {
        assert_eq!(
            parse_remove_args(args(&["everything"])).unwrap(),
            "everything"
        );
        assert!(parse_remove_args(args(&[])).is_err());
        assert!(parse_remove_args(args(&["everything", "extra"])).is_err());
    }

    #[test]
    fn unknown_action_returns_usage_error() {
        let result = run(Path::new("."), args(&["bogus"]));
        assert!(result.is_err());
    }

    #[test]
    fn list_rejects_extra_arguments() {
        let result = run(Path::new("."), args(&["list", "extra"]));
        assert!(result.is_err());
    }

    #[test]
    fn add_missing_name_returns_usage_error() {
        let result = run(Path::new("."), args(&["add"]));
        assert!(result.is_err());
    }

    #[test]
    fn remove_missing_name_returns_usage_error() {
        let result = run(Path::new("."), args(&["remove"]));
        assert!(result.is_err());
    }

    #[test]
    fn format_entry_renders_command_transport_and_clients() {
        let entry = McpConfig {
            name: "everything".to_owned(),
            command: Some("npx".to_owned()),
            args: vec![
                "-y".to_owned(),
                "@modelcontextprotocol/server-everything".to_owned(),
            ],
            cwd: None,
            url: None,
            transport: None,
            clients: vec![McpClient::Claude, McpClient::Codex],
        };
        assert_eq!(
            format_entry(&entry),
            "everything npx -y @modelcontextprotocol/server-everything stdio claude,codex"
        );
    }

    #[test]
    fn format_entry_renders_url_transport_and_all_clients() {
        let entry = McpConfig {
            name: "streamable".to_owned(),
            command: None,
            args: Vec::new(),
            cwd: None,
            url: Some("https://example.com/mcp".to_owned()),
            transport: Some(McpTransport::Sse),
            clients: Vec::new(),
        };
        assert_eq!(
            format_entry(&entry),
            "streamable url https://example.com/mcp sse all"
        );
    }
}
