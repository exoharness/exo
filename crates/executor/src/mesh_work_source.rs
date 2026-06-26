//! A [`WorkSource`] that drains a meshi board as an optional source of work.
//!
//! ## How this honors meshi's contract
//!
//! meshi's one invariant is: **it coordinates, it never schedules, assigns,
//! merges, or deploys.** This source respects that completely:
//!
//! * It only **reads** the board's *open* items (`meshi board --json`). It never
//!   asks meshi to schedule or push work to exo; exo's own claim loop pulls.
//! * It **claims** via meshi's own atomic grab (`meshi self grab <id>`). The
//!   grab is meshi-owned and race-safe: if another runtime already grabbed the
//!   item, meshi reports `already_grabbed` and this source simply skips it.
//!   exo never overrides meshi's ownership decision.
//! * On completion it **acknowledges** back through meshi's own
//!   `meshi self complete <id>` so the coordination loop closes — again, exo
//!   reporting an outcome to meshi, not meshi driving exo.
//!
//! meshi therefore stays entirely *outside* exo as an optional source the
//! scheduler drains. The whole source is gated behind config and is off by
//! default.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

use crate::scheduler_types::{NewScheduledTask, ScheduledTaskRecord, now_ms};
use crate::work_source::{ClaimedWork, CompletionHook, WorkOutcome, WorkSource};

/// Configuration for a [`MeshBoardSource`]. Construct one only when meshi work
/// draining is explicitly enabled; the scheduler treats the absence of a source
/// as "meshi disabled".
#[derive(Debug, Clone)]
pub struct MeshBoardConfig {
    /// Path to the `meshi` CLI binary.
    pub meshi_bin: PathBuf,
    /// Board scope to drain (`--workstream`). `None` drains the default board.
    pub workstream: Option<String>,
    /// The exo agent that runs work claimed from the board.
    pub agent_id: String,
    /// The exo conversation that owns the run + its wakeup.
    pub conversation_id: String,
    /// Command template run for each grabbed item. The board item id is appended
    /// as the final argument, so the runner receives the item to act on.
    pub command_template: Vec<String>,
    /// Report-prompt handed to the scheduler's wakeup for each item.
    pub report_prompt: String,
}

impl MeshBoardConfig {
    fn validate(&self) -> Result<()> {
        if self.command_template.is_empty() {
            bail!("mesh board command_template must not be empty");
        }
        if self.agent_id.trim().is_empty() {
            bail!("mesh board agent_id must not be empty");
        }
        if self.conversation_id.trim().is_empty() {
            bail!("mesh board conversation_id must not be empty");
        }
        Ok(())
    }
}

/// One open item read from `meshi board --json`. Fields mirror what meshi
/// exposes in a board listing; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct MeshBoardItem {
    pub id: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub requested_action: Option<String>,
    #[serde(default)]
    pub body_preview: Option<String>,
}

/// The `meshi board --json` envelope.
#[derive(Debug, Clone, Deserialize)]
struct MeshBoardListing {
    #[serde(default)]
    items: Vec<MeshBoardItem>,
}

/// The `meshi self grab <id>` result. meshi owns the grab atomically; we only
/// read the outcome.
#[derive(Debug, Clone, Deserialize)]
struct MeshGrabResult {
    #[serde(default)]
    action: Option<String>,
}

impl MeshGrabResult {
    /// Whether this runtime won the grab. Any non-`grabbed` action (notably
    /// `already_grabbed`) means another runtime owns the item and exo skips it.
    fn won(&self) -> bool {
        self.action.as_deref() == Some("grabbed")
    }
}

/// Abstracts the meshi CLI so the source can be unit-tested without a live mesh.
/// The real implementation shells out to `meshi`; tests use an in-memory mock.
#[async_trait]
pub trait MeshClient: Send + Sync {
    /// `meshi board [--workstream <ws>] --json` -> raw stdout JSON.
    async fn board_json(&self, workstream: Option<&str>) -> Result<String>;
    /// `meshi self grab <id>` -> raw stdout JSON.
    async fn grab_json(&self, item_id: &str) -> Result<String>;
    /// `meshi self complete <id> --result <result>` -> ignored stdout.
    async fn complete(&self, item_id: &str, result: &str) -> Result<()>;
}

/// The production [`MeshClient`]: shells out to the `meshi` binary.
pub struct CliMeshClient {
    meshi_bin: PathBuf,
}

impl CliMeshClient {
    pub fn new(meshi_bin: PathBuf) -> Self {
        Self { meshi_bin }
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new(&self.meshi_bin)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .with_context(|| format!("failed to invoke meshi: {}", self.meshi_bin.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "meshi {} exited with {}: {}",
                args.join(" "),
                output.status,
                stderr.trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[async_trait]
impl MeshClient for CliMeshClient {
    async fn board_json(&self, workstream: Option<&str>) -> Result<String> {
        let mut args = vec!["board"];
        if let Some(ws) = workstream {
            args.push("--workstream");
            args.push(ws);
        }
        args.push("--json");
        self.run(&args).await
    }

    async fn grab_json(&self, item_id: &str) -> Result<String> {
        // `meshi self grab <id>` is the atomic, meshi-owned claim.
        self.run(&["self", "grab", item_id]).await
    }

    async fn complete(&self, item_id: &str, result: &str) -> Result<()> {
        self.run(&["self", "complete", item_id, "--result", result])
            .await
            .map(|_| ())
    }
}

/// Drains a meshi board as an optional exo work source.
pub struct MeshBoardSource {
    name: String,
    config: MeshBoardConfig,
    client: Box<dyn MeshClient>,
}

impl MeshBoardSource {
    /// Build a source backed by the real meshi CLI.
    pub fn from_config(config: MeshBoardConfig) -> Result<Self> {
        config.validate()?;
        let client = Box::new(CliMeshClient::new(config.meshi_bin.clone()));
        Ok(Self::with_client(config, client))
    }

    /// Build a source with an injected [`MeshClient`] (used in tests).
    pub fn with_client(config: MeshBoardConfig, client: Box<dyn MeshClient>) -> Self {
        Self {
            name: "meshi-board".to_string(),
            config,
            client,
        }
    }

    /// Map an open board item to a schedulable exo task. The command template
    /// gets the item id appended so the runner knows which item it owns.
    fn item_to_task(&self, item: &MeshBoardItem, now_ms: u64) -> Result<ScheduledTaskRecord> {
        let mut command = self.config.command_template.clone();
        command.push(item.id.clone());

        let report_prompt = compose_report_prompt(&self.config.report_prompt, item);

        // `@every` is required by the scheduler's schedule parser. A meshi item
        // is a single unit of work, not a recurring schedule; the lease + the
        // source's "open" filter prevent re-runs, so the interval is inert.
        ScheduledTaskRecord::new(
            NewScheduledTask {
                agent_id: self.config.agent_id.clone(),
                conversation_id: self.config.conversation_id.clone(),
                name: mesh_task_name(&item.id),
                schedule: "@every 1h".to_string(),
                sandbox_mode: None,
                setup_command: None,
                command,
                report_prompt,
                max_output_bytes: None,
            },
            now_ms,
        )
    }
}

/// Sanitize a board item id into a scheduler-legal task name (alnum, `-`, `_`).
fn mesh_task_name(item_id: &str) -> String {
    let sanitized: String = item_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    format!("mesh-{sanitized}")
}

fn compose_report_prompt(base: &str, item: &MeshBoardItem) -> String {
    let mut prompt = base.to_string();
    if let Some(subject) = &item.subject {
        prompt.push_str(&format!("\n\nBoard item: {subject}"));
    }
    if let Some(action) = &item.requested_action {
        prompt.push_str(&format!("\nRequested action: {action}"));
    }
    if let Some(body) = &item.body_preview {
        prompt.push_str(&format!("\nDetails: {body}"));
    }
    prompt
}

#[async_trait]
impl WorkSource for MeshBoardSource {
    fn name(&self) -> &str {
        &self.name
    }

    async fn claim_due(
        &self,
        _now_ms: u64,
        limit: usize,
        _lease_ms: u64,
    ) -> Result<Vec<ClaimedWork>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let board_raw = self
            .client
            .board_json(self.config.workstream.as_deref())
            .await?;
        let listing: MeshBoardListing = serde_json::from_str(&board_raw)
            .with_context(|| format!("failed to parse meshi board listing: {board_raw}"))?;

        let mut claimed = Vec::new();
        for item in listing.items {
            if claimed.len() >= limit {
                break;
            }
            // meshi owns the grab. We only act on items we win; `already_grabbed`
            // (another runtime owns it) is skipped — exo never overrides meshi.
            let grab_raw = self.client.grab_json(&item.id).await?;
            let grab: MeshGrabResult = serde_json::from_str(&grab_raw)
                .with_context(|| format!("failed to parse meshi grab result: {grab_raw}"))?;
            if !grab.won() {
                continue;
            }

            let task = self
                .item_to_task(&item, now_ms())
                .with_context(|| format!("failed to map meshi item {} to a task", item.id))?;

            claimed.push(ClaimedWork {
                source: self.name.clone(),
                task,
                on_complete: Some(self.completion_hook(item.id.clone())),
            });
        }
        Ok(claimed)
    }
}

impl MeshBoardSource {
    /// Build the completion hook that acknowledges the grabbed item back to
    /// meshi via `meshi self complete`. This is exo reporting an outcome to the
    /// coordination layer, not meshi scheduling exo.
    fn completion_hook(&self, item_id: String) -> CompletionHook {
        // The CLI client is cheap to reconstruct from config; rebuild one for
        // the hook so it owns its dependencies (the hook outlives the claim).
        let client = CliMeshClient::new(self.config.meshi_bin.clone());
        Box::new(move |outcome: WorkOutcome| {
            Box::pin(async move {
                let result = match outcome {
                    WorkOutcome::Completed { exit_code } => format!(
                        "exo ran the work; exit_code={}",
                        exit_code
                            .map(|c| c.to_string())
                            .unwrap_or_else(|| "unknown".to_string())
                    ),
                    WorkOutcome::Errored => "exo failed to run the work".to_string(),
                };
                client.complete(&item_id, &result).await
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// In-memory mock of the meshi CLI so we never require a live mesh.
    struct MockMeshClient {
        board: String,
        grabbed: Mutex<Vec<String>>,
        completed: Mutex<Vec<(String, String)>>,
        /// item ids meshi reports as `already_grabbed`.
        already_grabbed: Vec<String>,
    }

    impl MockMeshClient {
        fn new(board: &str) -> Self {
            Self {
                board: board.to_string(),
                grabbed: Mutex::new(Vec::new()),
                completed: Mutex::new(Vec::new()),
                already_grabbed: Vec::new(),
            }
        }

        fn with_already_grabbed(mut self, ids: Vec<String>) -> Self {
            self.already_grabbed = ids;
            self
        }
    }

    #[async_trait]
    impl MeshClient for MockMeshClient {
        async fn board_json(&self, _workstream: Option<&str>) -> Result<String> {
            Ok(self.board.clone())
        }

        async fn grab_json(&self, item_id: &str) -> Result<String> {
            if self.already_grabbed.iter().any(|id| id == item_id) {
                return Ok(r#"{"ok":true,"action":"already_grabbed"}"#.to_string());
            }
            self.grabbed.lock().unwrap().push(item_id.to_string());
            Ok(r#"{"ok":true,"action":"grabbed"}"#.to_string())
        }

        async fn complete(&self, item_id: &str, result: &str) -> Result<()> {
            self.completed
                .lock()
                .unwrap()
                .push((item_id.to_string(), result.to_string()));
            Ok(())
        }
    }

    fn test_config() -> MeshBoardConfig {
        MeshBoardConfig {
            meshi_bin: PathBuf::from("meshi"),
            workstream: Some("review".to_string()),
            agent_id: "agent".to_string(),
            conversation_id: "conversation".to_string(),
            command_template: vec!["dossbot".to_string(), "work".to_string()],
            report_prompt: "Report on the board item.".to_string(),
        }
    }

    #[test]
    fn mesh_task_name_sanitizes() {
        assert_eq!(mesh_task_name("msg-001"), "mesh-msg-001");
        assert_eq!(mesh_task_name("a/b c"), "mesh-a-b-c");
    }

    #[test]
    fn item_to_task_appends_id_and_validates() {
        let source = MeshBoardSource::with_client(
            test_config(),
            Box::new(MockMeshClient::new(r#"{"items":[]}"#)),
        );
        let item = MeshBoardItem {
            id: "msg-001".to_string(),
            subject: Some("Review the SOW".to_string()),
            requested_action: Some("SHIP or block".to_string()),
            body_preview: Some("Details here".to_string()),
        };
        let task = source.item_to_task(&item, 1).unwrap();
        assert_eq!(task.agent_id, "agent");
        assert_eq!(task.conversation_id, "conversation");
        assert_eq!(task.name, "mesh-msg-001");
        // Command template + the item id appended.
        assert_eq!(task.command, vec!["dossbot", "work", "msg-001"]);
        assert!(task.report_prompt.contains("Review the SOW"));
        assert!(task.report_prompt.contains("SHIP or block"));
    }

    #[tokio::test]
    async fn claim_due_maps_open_items_to_tasks() {
        let board = r#"{
            "open": 2,
            "items": [
                {"id":"msg-001","subject":"Review A","requested_action":"ship","body_preview":"a"},
                {"id":"msg-002","subject":"Review B","requested_action":"ship","body_preview":"b"}
            ]
        }"#;
        let source =
            MeshBoardSource::with_client(test_config(), Box::new(MockMeshClient::new(board)));
        let claimed = source.claim_due(now_ms(), 10, 1000).await.unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].source, "meshi-board");
        assert_eq!(claimed[0].task.command, vec!["dossbot", "work", "msg-001"]);
        assert_eq!(claimed[1].task.command, vec!["dossbot", "work", "msg-002"]);
        // Each carries a completion hook so the loop can ack meshi.
        assert!(claimed[0].on_complete.is_some());
    }

    #[tokio::test]
    async fn claim_due_skips_already_grabbed_items() {
        let board = r#"{
            "items": [
                {"id":"msg-001"},
                {"id":"msg-002"}
            ]
        }"#;
        let client = MockMeshClient::new(board).with_already_grabbed(vec!["msg-001".to_string()]);
        let source = MeshBoardSource::with_client(test_config(), Box::new(client));
        let claimed = source.claim_due(now_ms(), 10, 1000).await.unwrap();
        // msg-001 was grabbed by another runtime; exo honors meshi and skips it.
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].task.command, vec!["dossbot", "work", "msg-002"]);
    }

    #[tokio::test]
    async fn claim_due_respects_limit() {
        let board = r#"{
            "items": [
                {"id":"msg-001"},
                {"id":"msg-002"},
                {"id":"msg-003"}
            ]
        }"#;
        let source =
            MeshBoardSource::with_client(test_config(), Box::new(MockMeshClient::new(board)));
        let claimed = source.claim_due(now_ms(), 2, 1000).await.unwrap();
        assert_eq!(claimed.len(), 2);
    }

    #[tokio::test]
    async fn claim_due_empty_board_yields_nothing() {
        let source = MeshBoardSource::with_client(
            test_config(),
            Box::new(MockMeshClient::new(
                r#"{"open":0,"oldest_age_seconds":0,"items":[]}"#,
            )),
        );
        let claimed = source.claim_due(now_ms(), 10, 1000).await.unwrap();
        assert!(claimed.is_empty());
    }
}
