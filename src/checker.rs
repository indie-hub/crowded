//! Read-only validation for a workspace's `crowded.toml`.

use std::{env, io, path::Path};

use crate::{config::load_room_file, initializer::validate};

const CONFIG_FILE: &str = "crowded.toml";

pub(crate) fn command() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().nth(2).is_some() {
        return Err(invalid_input("usage: crowded check (no additional arguments)").into());
    }
    println!("{}", check_at(&env::current_dir()?)?);
    Ok(())
}

fn check_at(root: &Path) -> io::Result<String> {
    let path = root.join(CONFIG_FILE);
    let config = load_room_file(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read {}: {error}", path.display()),
        )
    })?;
    validate(&config)?;
    Ok(format!(
        "{} valid: {} room(s), {} MCP server(s), {} plugin(s), {} setup action(s)",
        CONFIG_FILE,
        config.rooms.len(),
        config.mcp_servers.len(),
        config.plugins.len(),
        config.setup.len(),
    ))
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn test_directory() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            env::temp_dir().join(format!("crowded-check-test-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        root
    }

    #[test]
    fn check_accepts_valid_config() {
        let root = test_directory();
        fs::write(root.join(CONFIG_FILE), "[[rooms]]\ncommand = \"claude\"\ntransport = \"raw\"\n\n[[rooms]]\ncommand = \"codex\"\ntransport = \"raw\"\n").unwrap();
        assert_eq!(
            check_at(&root).unwrap(),
            "crowded.toml valid: 2 room(s), 0 MCP server(s), 0 plugin(s), 0 setup action(s)"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_reports_missing_config() {
        let root = test_directory();
        let error = check_at(&root).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("crowded.toml"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_reports_malformed_config() {
        let root = test_directory();
        fs::write(root.join(CONFIG_FILE), "[[rooms]\n").unwrap();
        let error = check_at(&root).unwrap_err();
        assert!(error.to_string().contains("crowded.toml"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn check_reports_semantically_invalid_config() {
        let root = test_directory();
        fs::write(
            root.join(CONFIG_FILE),
            "[[rooms]]\ncommand = \"claude\"\ntransport = \"raw\"\n",
        )
        .unwrap();
        let error = check_at(&root).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("at least two rooms"));
        fs::remove_dir_all(root).unwrap();
    }
}
