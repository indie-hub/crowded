//! Cross-vendor room-detail tracking: a room's internal sub-agents and
//! todo-list state, collected from hook events (Claude Code) or persisted
//! artifacts (OpenCode SQLite, Codex rollout logs). The view renders the
//! collected [`RoomDetail`] uniformly; it carries no liveness or freshness
//! distinction.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RoomDetail {
    pub(crate) sub_agents: Vec<SubAgent>,
    pub(crate) todos: Vec<TodoItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SubAgent {
    pub(crate) id: String,
    pub(crate) kind: String,
    pub(crate) status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct TodoItem {
    pub(crate) id: String,
    pub(crate) content: String,
    pub(crate) status: String,
}

/// A single incremental sub-agent/todo event carried by a pulse hook report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", content = "value", rename_all = "snake_case")]
pub(crate) enum DetailEvent {
    SubAgentStarted {
        id: String,
        kind: String,
    },
    SubAgentStopped {
        id: String,
    },
    TodoUpsert {
        id: String,
        content: String,
        status: String,
    },
}

/// Apply one hook-driven event to a room's accumulated detail. Sub-agents
/// deduplicate by id; todos upsert by id, keeping the newest snapshot.
pub(crate) fn apply_detail_event(detail: &mut RoomDetail, event: DetailEvent) {
    match event {
        DetailEvent::SubAgentStarted { id, kind } => {
            if let Some(agent) = detail.sub_agents.iter_mut().find(|agent| agent.id == id) {
                agent.kind = kind;
                agent.status = "running".to_owned();
            } else {
                detail.sub_agents.push(SubAgent {
                    id,
                    kind,
                    status: "running".to_owned(),
                });
            }
        }
        DetailEvent::SubAgentStopped { id } => {
            if let Some(agent) = detail.sub_agents.iter_mut().find(|agent| agent.id == id) {
                agent.status = "completed".to_owned();
            } else {
                detail.sub_agents.push(SubAgent {
                    id,
                    kind: String::new(),
                    status: "completed".to_owned(),
                });
            }
        }
        DetailEvent::TodoUpsert {
            id,
            content,
            status,
        } => {
            if let Some(todo) = detail.todos.iter_mut().find(|todo| todo.id == id) {
                todo.content = content;
                todo.status = status;
            } else {
                detail.todos.push(TodoItem {
                    id,
                    content,
                    status,
                });
            }
        }
    }
}

const OPENCODE_DATABASE_PATH: &str = ".local/share/opencode/opencode.db";

/// Collect a room's current sub-agent/todo detail from its vendor artifact.
/// `guest` is the CLI program basename (`claude`, `codex`, or `opencode`).
/// Claude Code reports its detail through hook pulses instead, so this returns
/// an empty detail for it and the caller keeps the hook-accumulated state.
pub(crate) fn collect_detail(guest: &str, cwd: &Path, session_id: &str) -> Option<RoomDetail> {
    match guest {
        "opencode" => opencode_detail(cwd, session_id),
        "codex" => codex_detail(cwd, session_id),
        _ => Some(RoomDetail::default()),
    }
}

fn opencode_detail(_cwd: &Path, session_id: &str) -> Option<RoomDetail> {
    let home = home_dir()?;
    let database = home.join(OPENCODE_DATABASE_PATH);
    if !database.is_file() {
        return None;
    }
    let escaped = session_id.replace('\'', "''");
    let output = Command::new("sqlite3")
        .arg("-json")
        .arg(&database)
        .arg(format!(
            "SELECT data FROM session_message WHERE session_id = '{escaped}' AND type = 'assistant' ORDER BY time_created ASC;"
        ))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut detail = RoomDetail::default();
    // Process messages and tool entries oldest-first; the last completed todowrite snapshot wins.
    // Incomplete and errored todowrite entries are ignored.
    let rows = if output.stdout.iter().all(|byte| byte.is_ascii_whitespace()) {
        Vec::new()
    } else {
        parse_rows(&output.stdout)?
    };
    for row in rows {
        let Some(data) = row
            .get("data")
            .and_then(|value| value.as_str())
            .and_then(parse_json)
        else {
            continue;
        };
        for tool in data
            .get("content")
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter(|entry| entry.get("type").and_then(|value| value.as_str()) == Some("tool"))
        {
            let Some(state) = tool.get("state") else {
                continue;
            };
            match tool.get("name").and_then(|value| value.as_str()) {
                Some("task") => {
                    let Some(metadata) = state.get("metadata") else {
                        continue;
                    };
                    if metadata
                        .get("parentSessionId")
                        .and_then(|value| value.as_str())
                        != Some(session_id)
                    {
                        continue;
                    }
                    let Some(id) = metadata.get("sessionId").and_then(|value| value.as_str())
                    else {
                        continue;
                    };
                    detail.sub_agents.push(SubAgent {
                        id: id.to_owned(),
                        kind: "task".to_owned(),
                        status: state
                            .get("status")
                            .and_then(|value| value.as_str())
                            .unwrap_or("completed")
                            .to_owned(),
                    });
                }
                Some("todowrite")
                    if state.get("status").and_then(|value| value.as_str())
                        == Some("completed") =>
                {
                    detail.todos.clear();
                    if let Some(items) = state
                        .get("input")
                        .and_then(|value| value.get("todos"))
                        .and_then(|value| value.as_array())
                    {
                        detail.todos.extend(items.iter().enumerate().filter_map(
                            |(index, item)| {
                                Some(TodoItem {
                                    id: index.to_string(),
                                    content: item.get("content")?.as_str()?.to_owned(),
                                    status: item.get("status")?.as_str()?.to_owned(),
                                })
                            },
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    Some(detail)
}
fn codex_detail(_cwd: &Path, session_id: &str) -> Option<RoomDetail> {
    let sessions = home_dir()?.join(".codex").join("sessions");
    // Sub-agents: every rollout whose `session_meta` opens a sub-agent thread
    // whose `parent_thread_id` is this session. The correlation is
    // retrospective (read from the child's own persisted log).
    let mut sub_agents = Vec::new();
    for path in find_rollouts(&sessions) {
        let Some(meta) = codex_session_meta(&path) else {
            continue;
        };
        if meta.parent_thread_id.as_deref() != Some(session_id) {
            continue;
        }
        sub_agents.push(SubAgent {
            id: meta.id,
            kind: meta
                .agent_nickname
                .or(meta.agent_role)
                .unwrap_or_else(|| "task".to_owned()),
            status: "completed".to_owned(),
        });
    }
    // Todos: the newest `update_plan` function call in this session's rollout
    // carries a full `plan: [{step, status}]` snapshot, which supersedes any
    // earlier plan.
    let rollout = find_rollout(&sessions, session_id)?;
    let mut todos = Vec::new();
    for line in fs::read_to_string(&rollout).ok()?.lines() {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(payload) = record.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|value| value.as_str()) != Some("function_call")
            || payload.get("name").and_then(|value| value.as_str()) != Some("update_plan")
        {
            continue;
        }
        let Some(arguments) = payload.get("arguments").and_then(|value| value.as_str()) else {
            continue;
        };
        let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
            continue;
        };
        let Some(steps) = arguments.get("plan").and_then(|value| value.as_array()) else {
            continue;
        };
        todos.clear();
        for (index, step) in steps.iter().enumerate() {
            todos.push(TodoItem {
                id: index.to_string(),
                content: step
                    .get("step")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned(),
                status: step
                    .get("status")
                    .and_then(|value| value.as_str())
                    .unwrap_or("pending")
                    .to_owned(),
            });
        }
    }
    Some(RoomDetail { sub_agents, todos })
}

struct CodexSessionMeta {
    id: String,
    parent_thread_id: Option<String>,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
}

fn codex_session_meta(path: &Path) -> Option<CodexSessionMeta> {
    let first_line = fs::read_to_string(path).ok()?.lines().next()?.to_owned();
    let record: serde_json::Value = serde_json::from_str(&first_line).ok()?;
    if record.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let payload = record.get("payload")?;
    let id = payload
        .get("id")
        .and_then(|value| value.as_str())
        .or_else(|| payload.get("session_id").and_then(|value| value.as_str()))?
        .to_owned();
    let spawn = payload.pointer("/source/subagent/thread_spawn");
    Some(CodexSessionMeta {
        id,
        parent_thread_id: spawn
            .and_then(|value| value.get("parent_thread_id"))
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        agent_nickname: spawn
            .and_then(|value| value.get("agent_nickname"))
            .and_then(|value| value.as_str())
            .map(str::to_owned),
        agent_role: spawn
            .and_then(|value| value.get("agent_role"))
            .and_then(|value| value.as_str())
            .map(str::to_owned),
    })
}

fn find_rollout(directory: &Path, session_id: &str) -> Option<PathBuf> {
    for entry in fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_rollout(&path, session_id) {
                return Some(found);
            }
        } else if path
            .file_name()?
            .to_string_lossy()
            .ends_with(&format!("-{session_id}.jsonl"))
        {
            return Some(path);
        }
    }
    None
}

fn find_rollouts(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(find_rollouts(&path));
        } else if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("rollout-"))
            && path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
        {
            found.push(path);
        }
    }
    found
}

fn parse_rows(output: &[u8]) -> Option<Vec<serde_json::Value>> {
    serde_json::from_slice(output).ok()
}

fn parse_json(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok()
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(home) = test_home() {
            return Some(home);
        }
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
}

#[cfg(test)]
static TEST_HOME: std::sync::OnceLock<std::sync::RwLock<Option<PathBuf>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn test_home() -> Option<PathBuf> {
    let lock = TEST_HOME.get_or_init(|| std::sync::RwLock::new(None));
    lock.read().ok().and_then(|guard| guard.clone())
}

/// Serializes tests that swap `TEST_HOME`, so parallel tests cannot overwrite
/// or clear each other's temp home.
#[cfg(test)]
static HOME_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Points [`home_dir`] at a fresh temp tree while held, and restores the real
/// home on drop, so artifact readers can be tested against fixture data.
#[cfg(test)]
pub(super) struct HomeDirGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    home: PathBuf,
}

#[cfg(test)]
impl HomeDirGuard {
    pub(super) fn isolated() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let hold = HOME_DIR_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let home = env::temp_dir().join(format!(
            "crowded-detail-home-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let lock = TEST_HOME.get_or_init(|| std::sync::RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = Some(home.clone());
        }
        Self { _lock: hold, home }
    }

    pub(super) fn path(&self) -> &Path {
        &self.home
    }
}

#[cfg(test)]
impl Drop for HomeDirGuard {
    fn drop(&mut self) {
        let lock = TEST_HOME.get_or_init(|| std::sync::RwLock::new(None));
        if let Ok(mut guard) = lock.write() {
            *guard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_detail_events_accumulates_sub_agents_and_todos() {
        let mut detail = RoomDetail::default();
        apply_detail_event(
            &mut detail,
            DetailEvent::SubAgentStarted {
                id: "a1".to_owned(),
                kind: "Task".to_owned(),
            },
        );
        apply_detail_event(
            &mut detail,
            DetailEvent::SubAgentStarted {
                id: "a2".to_owned(),
                kind: "Explore".to_owned(),
            },
        );
        apply_detail_event(
            &mut detail,
            DetailEvent::TodoUpsert {
                id: "t1".to_owned(),
                content: "Build it".to_owned(),
                status: "pending".to_owned(),
            },
        );
        apply_detail_event(
            &mut detail,
            DetailEvent::TodoUpsert {
                id: "t1".to_owned(),
                content: "Build it".to_owned(),
                status: "completed".to_owned(),
            },
        );
        // A second start on the same id updates in place rather than duplicating.
        apply_detail_event(
            &mut detail,
            DetailEvent::SubAgentStarted {
                id: "a1".to_owned(),
                kind: "Task".to_owned(),
            },
        );
        // Stop on an unknown id still records the completion.
        apply_detail_event(
            &mut detail,
            DetailEvent::SubAgentStopped {
                id: "a3".to_owned(),
            },
        );

        assert_eq!(detail.sub_agents.len(), 3);
        assert_eq!(detail.sub_agents[0].status, "running");
        assert_eq!(detail.sub_agents[2].status, "completed");
        assert_eq!(detail.todos.len(), 1);
        assert_eq!(detail.todos[0].status, "completed");
    }

    #[test]
    fn detail_event_round_trips_through_serde() {
        let event = DetailEvent::TodoUpsert {
            id: "t1".to_owned(),
            content: "Ship".to_owned(),
            status: "in_progress".to_owned(),
        };
        let text = serde_json::to_string(&event).unwrap();
        let decoded: DetailEvent = serde_json::from_str(&text).unwrap();
        assert_eq!(decoded, event);
    }

    #[test]
    fn codex_detail_correlates_child_rollouts_and_reads_the_newest_plan() {
        let guard = HomeDirGuard::isolated();
        let sessions = guard.path().join(".codex/sessions/2026/08/26");
        fs::create_dir_all(&sessions).unwrap();
        let child = sessions.join("rollout-2026-08-26T10-00-00-aaaaaaaa.jsonl");
        fs::write(
            &child,
            r#"{"type":"session_meta","payload":{"id":"aaaaaaaa","session_id":"aaaaaaaa","cwd":"/work","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-123","depth":1,"agent_nickname":"researcher","agent_role":"research"}}}}}
"#,
        )
        .unwrap();
        // A sibling rollout whose parent is a different session must not match.
        let sibling = sessions.join("rollout-2026-08-26T10-05-00-bbbbbbbb.jsonl");
        fs::write(
            &sibling,
            r#"{"type":"session_meta","payload":{"id":"bbbbbbbb","cwd":"/work","source":{"subagent":{"thread_spawn":{"parent_thread_id":"other-parent","depth":1,"agent_nickname":"writer"}}}}}
"#,
        )
        .unwrap();
        let parent = sessions.join("rollout-2026-08-26T11-00-00-parent-123.jsonl");
        fs::write(
            &parent,
            r#"{"type":"session_meta","payload":{"id":"parent-123","cwd":"/work"}}
{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"One\",\"status\":\"completed\"},{\"step\":\"Two\",\"status\":\"in_progress\"}]}"}}
{"type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"step\":\"One\",\"status\":\"completed\"},{\"step\":\"Two\",\"status\":\"completed\"}]}"}}
"#,
        )
        .unwrap();

        let detail = codex_detail(Path::new("/work"), "parent-123").unwrap();
        assert_eq!(detail.sub_agents.len(), 1);
        assert_eq!(detail.sub_agents[0].id, "aaaaaaaa");
        assert_eq!(detail.sub_agents[0].kind, "researcher");
        assert_eq!(detail.todos.len(), 2);
        assert_eq!(detail.todos[0].content, "One");
        assert_eq!(detail.todos[0].status, "completed");
        assert_eq!(detail.todos[1].content, "Two");
        assert_eq!(detail.todos[1].status, "completed");
    }

    fn create_opencode_message_db(home: &Path, sql: &str) {
        let database = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(database.parent().unwrap()).unwrap();
        let setup = Command::new("sqlite3")
            .arg(&database)
            .arg(format!(
                r#"
                    PRAGMA foreign_keys=OFF;
                    CREATE TABLE IF NOT EXISTS session_v2 (id text PRIMARY KEY);
                    CREATE TABLE IF NOT EXISTS "session_message" (
                        `id` text PRIMARY KEY,
                        `session_id` text NOT NULL,
                        `type` text NOT NULL,
                        `seq` integer NOT NULL,
                        `time_created` integer NOT NULL,
                        `time_updated` integer NOT NULL,
                        `data` text NOT NULL,
                        CONSTRAINT `fk_session_message_session_id_session_v2_id_fk`
                            FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
                    );
                    CREATE UNIQUE INDEX `session_message_session_seq_idx`
                        ON `session_message` (`session_id`, `seq`);
                    CREATE INDEX `session_message_session_type_seq_idx`
                        ON `session_message` (`session_id`, `type`, `seq`);
                    CREATE INDEX `session_message_session_time_created_id_idx`
                        ON `session_message` (`session_id`, `time_created`, `id`);
                    CREATE INDEX `session_message_time_created_idx`
                        ON `session_message` (`time_created`);
                    {sql}
                "#
            ))
            .output()
            .unwrap();
        assert!(setup.status.success());
    }

    #[test]
    fn opencode_detail_reads_real_v2_session_message_shape() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let guard = HomeDirGuard::isolated();
        create_opencode_message_db(
            guard.path(),
            r#"
                INSERT INTO session_message VALUES
                ('m1', 'parent-1', 'assistant', 1, 1, 1, '{"content":[{"type":"tool","name":"task","state":{"status":"completed","metadata":{"parentSessionId":"parent-1","sessionId":"child-1"}}}]}');
            "#,
        );

        let detail = opencode_detail(Path::new("/work"), "parent-1").unwrap();
        assert_eq!(detail.sub_agents.len(), 1);
        assert_eq!(detail.sub_agents[0].id, "child-1");
        assert_eq!(detail.sub_agents[0].status, "completed");
    }

    #[test]
    fn opencode_detail_reads_v2_task_and_latest_completed_todowrite() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let guard = HomeDirGuard::isolated();
        create_opencode_message_db(
            guard.path(),
            r#"
                INSERT INTO session_message VALUES
                ('m1', 'parent-1', 'assistant', 1, 1, 1, '{"content":[{"type":"tool","name":"task","state":{"metadata":{"parentSessionId":"parent-1","sessionId":"child-1"}}},{"type":"tool","name":"task","state":{"status":"error","metadata":{"parentSessionId":"parent-1","sessionId":"child-2"}}},{"type":"tool","name":"todowrite","state":{"status":"completed","input":{"todos":[{"content":"Old snapshot","status":"pending","priority":"high"}]}}}]}'),
                ('m2', 'parent-1', 'assistant', 2, 2, 2, '{"content":[{"type":"tool","name":"todowrite","state":{"status":"completed","input":{"todos":[{"content":"Design","status":"completed","priority":"high"},{"content":"Build","status":"pending","priority":"normal"}]}}}]}'),
                ('m3', 'parent-1', 'assistant', 3, 3, 3, '{"content":[{"type":"tool","name":"todowrite","state":{"status":"running","input":{"todos":[{"content":"Incomplete","status":"pending"}]}}}]}'),
                ('m4', 'parent-1', 'assistant', 4, 4, 4, '{"content":[{"type":"tool","name":"todowrite","state":{"status":"error","input":{"todos":[{"content":"Failed","status":"pending"}]}}}]}'),
                ('m5', 'other-session', 'assistant', 5, 5, 5, '{"content":[{"type":"tool","name":"task","state":{"status":"completed","metadata":{"parentSessionId":"parent-1","sessionId":"ignored-child"}}},{"type":"tool","name":"todowrite","state":{"status":"completed","input":{"todos":[{"content":"Wrong session","status":"pending"}]}}}]}');
            "#,
        );

        let detail = opencode_detail(Path::new("/work"), "parent-1").unwrap();
        assert_eq!(detail.sub_agents.len(), 2);
        assert_eq!(detail.sub_agents[0].id, "child-1");
        assert_eq!(detail.sub_agents[0].status, "completed");
        assert_eq!(detail.sub_agents[1].id, "child-2");
        assert_eq!(detail.sub_agents[1].status, "error");
        assert_eq!(detail.todos.len(), 2);
        assert_eq!(detail.todos[0].content, "Design");
        assert_eq!(detail.todos[0].status, "completed");
        assert_eq!(detail.todos[1].content, "Build");
        assert_eq!(detail.todos[1].status, "pending");
    }

    #[test]
    fn opencode_detail_ignores_task_and_todowrite_in_user_messages() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let guard = HomeDirGuard::isolated();
        create_opencode_message_db(
            guard.path(),
            r#"
                INSERT INTO session_message VALUES
                ('m1', 'parent-1', 'user', 1, 1, 1, '{"content":[{"type":"tool","name":"task","state":{"status":"completed","metadata":{"parentSessionId":"parent-1","sessionId":"ignored-child"}}},{"type":"tool","name":"todowrite","state":{"status":"completed","input":{"todos":[{"content":"Ignored","status":"pending"}]}}}]}');
            "#,
        );

        let detail = opencode_detail(Path::new("/work"), "parent-1").unwrap();
        assert!(detail.sub_agents.is_empty());
        assert!(detail.todos.is_empty());
    }

    #[test]
    fn opencode_detail_without_todowrite_returns_empty_todos() {
        if Command::new("sqlite3").arg("--version").output().is_err() {
            return;
        }
        let guard = HomeDirGuard::isolated();
        create_opencode_message_db(
            guard.path(),
            r#"
                INSERT INTO session_message VALUES
                ('m1', 'parent-1', 'assistant', 1, 1, 1, '{"content":[]}');
            "#,
        );

        let detail = opencode_detail(Path::new("/work"), "parent-1").unwrap();
        assert!(detail.sub_agents.is_empty());
        assert!(detail.todos.is_empty());
    }
}
