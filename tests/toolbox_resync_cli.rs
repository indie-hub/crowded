//! `crowded toolbox resync` and `crowded toolbox remove` against a real
//! project directory with an empty environment: the user's own edits in a
//! managed file must survive a re-sync, and removal must take away only what
//! Crowded wrote.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

const CROWDED_TOML: &str = r#"
[[rooms]]
command = "opencode"
transport = "raw"

[[rooms]]
command = "codex"
transport = "raw"
"#;

/// Runs the built binary in the project with no environment at all, the way a
/// hook or an editor plugin would.
fn toolbox(root: &Path, action: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crowded"))
        .args(["toolbox", action])
        .current_dir(root)
        .env_clear()
        .output()
        .expect("crowded toolbox must run")
}

fn synced_project() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("a scratch project");
    fs::write(root.path().join("crowded.toml"), CROWDED_TOML).expect("crowded.toml");
    let first = toolbox(root.path(), "sync");
    assert!(
        first.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    root
}

fn hooks_path(root: &Path) -> std::path::PathBuf {
    root.join(".codex").join("hooks.json")
}

fn add_user_key(root: &Path) -> Value {
    let path = hooks_path(root);
    let mut document: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("hooks.json")).expect("valid JSON");
    document["MyMarker"] = json!(true);
    fs::write(&path, format!("{}\n", serde_json::to_string_pretty(&document).unwrap()))
        .expect("write hooks.json");
    document
}

#[test]
fn resync_refuses_a_hand_edited_tui_plugin() {
    let root = synced_project();
    let plugin = root.path().join(".opencode").join("plugins").join("crowded-tui").join("tui.js");
    let written = fs::read(&plugin).expect("the generated plugin");
    let mut edited = written.clone();
    edited.extend_from_slice(b"\n// my own note\n");
    fs::write(&plugin, &edited).expect("edit the plugin");

    let out = toolbox(root.path(), "resync");
    assert!(
        !out.status.success(),
        "resync must not succeed over a hand-edited plugin: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("refusing to overwrite"), "{stderr}");
    assert_eq!(
        fs::read(&plugin).expect("the plugin after resync"),
        edited,
        "the user's edit must be preserved byte for byte"
    );
}

#[test]
fn resync_keeps_a_user_key_in_the_hooks_file() {
    let root = synced_project();
    add_user_key(root.path());

    let out = toolbox(root.path(), "resync");
    assert!(
        out.status.success(),
        "resync failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let merged: Value =
        serde_json::from_str(&fs::read_to_string(hooks_path(root.path())).unwrap()).unwrap();
    assert_eq!(merged["MyMarker"], json!(true), "the user's key survives");
    let starts = merged["hooks"]["SessionStart"].as_array().expect("SessionStart");
    assert_eq!(
        starts.len(),
        2,
        "exactly Crowded's pulse and session report, no duplicates: {starts:?}"
    );
}

#[test]
fn remove_takes_away_only_what_crowded_wrote() {
    let root = synced_project();
    add_user_key(root.path());

    let out = toolbox(root.path(), "remove");
    assert!(
        out.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let after: Value =
        serde_json::from_str(&fs::read_to_string(hooks_path(root.path())).unwrap()).unwrap();
    assert_eq!(after["MyMarker"], json!(true), "the user's key survives removal");
    assert!(
        after.get("hooks").is_none(),
        "Crowded's own hooks are gone: {after:?}"
    );
    assert!(
        !root.path().join(".opencode").join("plugins").join("crowded-tui").join("tui.js").exists(),
        "the plugin Crowded created is removed"
    );
}

#[test]
fn a_refused_resync_changes_no_file_and_keeps_the_state() {
    let root = synced_project();
    let state_path = root.path().join(".crowded").join("toolbox-state.json");
    let state_before = fs::read(&state_path).expect("state after sync");
    let hooks_before = fs::read(hooks_path(root.path())).expect("codex hooks after sync");
    let plugin = root.path().join(".opencode").join("plugins").join("crowded-tui").join("tui.js");
    let written = fs::read(&plugin).expect("the generated plugin");
    let mut edited = written.clone();
    edited.extend_from_slice(b"\n// my own note\n");
    fs::write(&plugin, &edited).expect("edit the plugin");
    // Dropping the codex room makes its files orphans that a successful
    // resync would restore; a refused one must not touch them.
    fs::write(
        root.path().join("crowded.toml"),
        "[[rooms]]\ncommand = \"opencode\"\ntransport = \"raw\"\n\n[[rooms]]\ncommand = \"opencode\"\ntransport = \"raw\"\n",
    )
    .expect("crowded.toml");

    let out = toolbox(root.path(), "resync");
    assert!(!out.status.success(), "resync must be refused");
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing to overwrite"));
    assert_eq!(fs::read(&state_path).expect("state survives a refusal"), state_before);
    assert_eq!(fs::read(&plugin).unwrap(), edited, "the edit is untouched");
    assert_eq!(
        fs::read(hooks_path(root.path())).expect("orphan hooks stay"),
        hooks_before,
        "no stale-file restore may run before the refusal"
    );

    fs::write(&plugin, &written).expect("revert the edit");
    let removed = toolbox(root.path(), "remove");
    assert!(
        removed.status.success(),
        "remove after a refusal failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!hooks_path(root.path()).exists(), "Crowded's own file is removed");
}
