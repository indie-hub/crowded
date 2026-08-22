//! CRUD operations for `[[mcp]]` entries in crowded.toml.
//!
//! Uses `toml_edit` to preserve comments and formatting.

use std::{fs, io, path::Path};

use toml_edit::{DocumentMut, Item, Table};

use crate::config::{McpClient, McpConfig, McpTransport, validate_mcp_servers};
use crate::toolbox;

const CROWDED_TOML: &str = "crowded.toml";

/// A new MCP server entry to be added to crowded.toml.
pub(crate) struct NewMcpEntry {
    pub(crate) name: String,
    pub(crate) command: Option<String>,
    pub(crate) args: Vec<String>,
    pub(crate) url: Option<String>,
    pub(crate) transport: Option<McpTransport>,
    pub(crate) clients: Vec<McpClient>,
}

/// Read all `[[mcp]]` entries from crowded.toml without mutating it.
pub(crate) fn list_entries(root: &Path) -> io::Result<Vec<McpConfig>> {
    let path = root.join(CROWDED_TOML);
    let text = fs::read_to_string(&path)?;
    let doc: DocumentMut = text.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid crowded.toml: {error}"),
        )
    })?;

    let mut entries = Vec::new();
    if let Some(mcp_item) = doc.get("mcp") {
        let array = mcp_item.as_array_of_tables().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "[[mcp]] must be an array of tables",
            )
        })?;
        for table in array.iter() {
            let entry = parse_mcp_table(table)?;
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Add a new `[[mcp]]` entry to crowded.toml, preserving comments and formatting.
pub(crate) fn add_entry(root: &Path, entry: NewMcpEntry) -> io::Result<()> {
    let path = root.join(CROWDED_TOML);
    let text = fs::read_to_string(&path)?;
    let mut doc: DocumentMut = text.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid crowded.toml: {error}"),
        )
    })?;

    let existing = list_entries_from_doc(&doc)?;
    let new_config = entry_to_config(&entry);

    let mut combined = existing.clone();
    combined.push(new_config);
    validate_mcp_servers(&combined)?;

    let mut table = Table::new();
    table.insert("name", toml_edit::value(&entry.name));
    if let Some(ref command) = entry.command {
        table.insert("command", toml_edit::value(command));
    }
    if !entry.args.is_empty() {
        let args_array: toml_edit::Array = entry.args.iter().map(|a| a.as_str()).collect();
        table.insert("args", toml_edit::value(args_array));
    }
    if let Some(ref url) = entry.url {
        table.insert("url", toml_edit::value(url));
    }
    if let Some(transport) = entry.transport {
        table.insert("transport", toml_edit::value(transport.as_str()));
    }
    if !entry.clients.is_empty() {
        let clients_array: toml_edit::Array = entry.clients.iter().map(|c| c.as_str()).collect();
        table.insert("clients", toml_edit::value(clients_array));
    }

    let mcp = doc
        .entry("mcp")
        .or_insert_with(|| Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    let array = mcp.as_array_of_tables_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "[[mcp]] must be an array of tables",
        )
    })?;
    array.push(table);

    fs::write(&path, doc.to_string())?;
    toolbox::sync(root)?;
    Ok(())
}

/// Remove the `[[mcp]]` entry matching the given name.
pub(crate) fn remove_entry(root: &Path, name: &str) -> io::Result<()> {
    let path = root.join(CROWDED_TOML);
    let text = fs::read_to_string(&path)?;
    let mut doc: DocumentMut = text.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid crowded.toml: {error}"),
        )
    })?;

    let mcp = doc.get_mut("mcp").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no [[mcp]] section in crowded.toml",
        )
    })?;
    let array = mcp.as_array_of_tables_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "[[mcp]] must be an array of tables",
        )
    })?;

    let original_len = array.len();
    let mut found = false;
    for i in (0..array.len()).rev() {
        if let Some(table) = array.get(i) {
            if table.get("name").and_then(|v| v.as_str()) == Some(name) {
                array.remove(i);
                found = true;
                break;
            }
        }
    }

    if !found {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("MCP server not found: {name}"),
        ));
    }

    assert_eq!(
        array.len(),
        original_len - 1,
        "exactly one entry should have been removed"
    );

    fs::write(&path, doc.to_string())?;
    toolbox::sync(root)?;
    Ok(())
}

fn entry_to_config(entry: &NewMcpEntry) -> McpConfig {
    McpConfig {
        name: entry.name.clone(),
        command: entry.command.clone(),
        args: entry.args.clone(),
        cwd: None,
        url: entry.url.clone(),
        transport: entry.transport,
        clients: entry.clients.clone(),
    }
}

fn parse_mcp_table(table: &Table) -> io::Result<McpConfig> {
    let name = table
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing [[mcp]] name"))?
        .to_owned();

    let command = table
        .get("command")
        .and_then(|v| v.as_str())
        .map(String::from);

    let args = table
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let url = table.get("url").and_then(|v| v.as_str()).map(String::from);

    let transport = table
        .get("transport")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "http" => Some(McpTransport::Http),
            "sse" => Some(McpTransport::Sse),
            _ => None,
        });

    let clients = table
        .get("clients")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .filter_map(|s| match s {
                    "claude" => Some(McpClient::Claude),
                    "codex" => Some(McpClient::Codex),
                    "opencode" => Some(McpClient::Opencode),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(McpConfig {
        name,
        command,
        args,
        cwd: None,
        url,
        transport,
        clients,
    })
}

fn list_entries_from_doc(doc: &DocumentMut) -> io::Result<Vec<McpConfig>> {
    let mut entries = Vec::new();
    if let Some(mcp_item) = doc.get("mcp") {
        let array = mcp_item.as_array_of_tables().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "[[mcp]] must be an array of tables",
            )
        })?;
        for table in array.iter() {
            entries.push(parse_mcp_table(table)?);
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const MINIMAL_CROWDED_TOML: &str = "\
[[rooms]]
command = \"claude\"
transport = \"raw\"

[[rooms]]
command = \"codex\"
transport = \"raw\"
";

    fn make_crowded_toml(dir: &Path, content: &str) {
        let full = if content.is_empty() {
            MINIMAL_CROWDED_TOML.to_string()
        } else {
            format!("{MINIMAL_CROWDED_TOML}\n{content}")
        };
        fs::write(dir.join("crowded.toml"), full).unwrap();
    }

    #[test]
    fn list_entries_reads_all_mcp_servers() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(
            dir.path(),
            r#"
[[mcp]]
name = "ccc"
command = "ccc"
args = ["mcp"]

[[mcp]]
name = "codegraph"
command = "npx"
args = ["-y", "@colbymchenry/codegraph", "serve", "--mcp"]
"#,
        );

        let entries = list_entries(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "ccc");
        assert_eq!(entries[1].name, "codegraph");
    }

    #[test]
    fn list_entries_returns_empty_when_no_mcp_section() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(dir.path(), "");

        let entries = list_entries(dir.path()).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn add_entry_appends_and_preserves_existing() {
        let dir = TempDir::new().unwrap();
        let original = "\
[[mcp]]
name = \"existing\"
command = \"existing-cmd\"
";
        make_crowded_toml(dir.path(), original);

        let entry = NewMcpEntry {
            name: "new-server".to_string(),
            command: Some("new-cmd".to_string()),
            args: vec!["--flag".to_string()],
            url: None,
            transport: None,
            clients: vec![],
        };

        add_entry(dir.path(), entry).unwrap();

        let text = fs::read_to_string(dir.path().join("crowded.toml")).unwrap();
        assert!(text.contains("existing"));
        assert!(text.contains("new-server"));
        assert!(text.contains("new-cmd"));
        assert!(text.contains("--flag"));

        let entries = list_entries(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "existing");
        assert_eq!(entries[1].name, "new-server");
    }

    #[test]
    fn add_entry_preserves_comments() {
        let dir = TempDir::new().unwrap();
        let original = "\
# Shared tools
[[mcp]]
name = \"existing\"
command = \"existing-cmd\"
";
        make_crowded_toml(dir.path(), original);

        let entry = NewMcpEntry {
            name: "new-server".to_string(),
            command: Some("new-cmd".to_string()),
            args: vec![],
            url: None,
            transport: None,
            clients: vec![],
        };

        add_entry(dir.path(), entry).unwrap();

        let text = fs::read_to_string(dir.path().join("crowded.toml")).unwrap();
        assert!(text.contains("# Shared tools"));
    }

    #[test]
    fn add_entry_rejects_duplicate_name() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(
            dir.path(),
            "\
[[mcp]]
name = \"existing\"
command = \"cmd\"
",
        );

        let entry = NewMcpEntry {
            name: "existing".to_string(),
            command: Some("other-cmd".to_string()),
            args: vec![],
            url: None,
            transport: None,
            clients: vec![],
        };

        let result = add_entry(dir.path(), entry);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn add_entry_rejects_invalid_name() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(dir.path(), "");

        let entry = NewMcpEntry {
            name: "has spaces!".to_string(),
            command: Some("cmd".to_string()),
            args: vec![],
            url: None,
            transport: None,
            clients: vec![],
        };

        let result = add_entry(dir.path(), entry);
        assert!(result.is_err());
    }

    #[test]
    fn remove_entry_deletes_named_entry() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(
            dir.path(),
            "\
[[mcp]]
name = \"first\"
command = \"cmd1\"

[[mcp]]
name = \"second\"
command = \"cmd2\"
",
        );

        remove_entry(dir.path(), "first").unwrap();

        let entries = list_entries(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "second");
    }

    #[test]
    fn remove_entry_preserves_other_entries() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(
            dir.path(),
            "\
[[mcp]]
name = \"a\"
command = \"cmd-a\"

[[mcp]]
name = \"b\"
command = \"cmd-b\"

[[mcp]]
name = \"c\"
command = \"cmd-c\"
",
        );

        remove_entry(dir.path(), "b").unwrap();

        let entries = list_entries(dir.path()).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a");
        assert_eq!(entries[1].name, "c");
    }

    #[test]
    fn remove_entry_preserves_comments() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(
            dir.path(),
            "\
# Important tools
[[mcp]]
name = \"keep\"
command = \"cmd-keep\"

[[mcp]]
name = \"drop\"
command = \"cmd-drop\"
",
        );

        remove_entry(dir.path(), "drop").unwrap();

        let text = fs::read_to_string(dir.path().join("crowded.toml")).unwrap();
        assert!(text.contains("# Important tools"));
        assert!(text.contains("keep"));
        assert!(!text.contains("drop"));
    }

    #[test]
    fn remove_entry_errors_on_unknown_name() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(
            dir.path(),
            "\
[[mcp]]
name = \"existing\"
command = \"cmd\"
",
        );

        let result = remove_entry(dir.path(), "nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("nonexistent"));
    }

    #[test]
    fn remove_entry_errors_when_no_mcp_section() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(dir.path(), "");

        let result = remove_entry(dir.path(), "anything");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn add_entry_with_remote_transport() {
        let dir = TempDir::new().unwrap();
        make_crowded_toml(dir.path(), "");

        let entry = NewMcpEntry {
            name: "remote-server".to_string(),
            command: None,
            args: vec![],
            url: Some("https://example.com/mcp".to_string()),
            transport: Some(McpTransport::Http),
            clients: vec![McpClient::Claude],
        };

        add_entry(dir.path(), entry).unwrap();

        let entries = list_entries(dir.path()).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "remote-server");
        assert_eq!(entries[0].url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(entries[0].transport, Some(McpTransport::Http));
        assert_eq!(entries[0].clients, vec![McpClient::Claude]);
    }
}
