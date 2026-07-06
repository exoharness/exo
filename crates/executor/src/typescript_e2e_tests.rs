//! End-to-end runner-pool test driving real Node runner processes.

use std::sync::Arc;
use std::time::Duration;

use exoharness::{ExoHarness, SandboxProvider};
use lingua::Message;
use lingua::universal::UserContent;
use tempfile::TempDir;

use crate::harness_tool::BasicToolRuntime;
use crate::test_support::local_test_config;
use crate::{
    AgentHarnessKind, BasicExoHarness, CreateAgentRequest, CreateConversationRequest, Harness,
    SendRequest, TypeScriptHarness, TypeScriptHarnessConfig,
};

/// One probe line per runTurn phase: `<pid> <start|end> <epoch_ms>`.
fn probe_harness_module(probe_path: &str, sleep_ms: u64) -> String {
    format!(
        r#"import {{ appendFileSync }} from "node:fs";

const PROBE = {probe_path:?};

export default {{
  async runTurn() {{
    appendFileSync(PROBE, `${{process.pid}} start ${{Date.now()}}\n`);
    await new Promise((resolve) => setTimeout(resolve, {sleep_ms}));
    appendFileSync(PROBE, `${{process.pid}} end ${{Date.now()}}\n`);
  }},
}};
"#
    )
}

fn parse_probe(contents: &str) -> Vec<(u64, String, u64)> {
    contents
        .lines()
        .map(|line| {
            let mut parts = line.split_whitespace();
            (
                parts.next().expect("pid").parse().expect("pid u64"),
                parts.next().expect("phase").to_string(),
                parts.next().expect("timestamp").parse().expect("ms u64"),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "spawns real Node runner processes (needs node + tsx); run with --ignored"]
async fn concurrent_turns_on_one_module_use_pooled_runners() {
    let dir = TempDir::new().expect("tempdir");
    let probe_path = dir.path().join("probe.log");
    let module_path = dir.path().join("pool-probe.ts");
    std::fs::write(
        &module_path,
        probe_harness_module(probe_path.to_str().expect("utf8 path"), 3000),
    )
    .expect("write harness module");

    let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root");
    let exoharness: Arc<dyn ExoHarness> = Arc::new(
        BasicExoHarness::new(local_test_config(dir.path().join("store")))
            .await
            .expect("exoharness"),
    );
    let harness = TypeScriptHarness::new(exoharness, workspace_root, Arc::new(BasicToolRuntime));

    let agent = harness
        .create_agent(CreateAgentRequest {
            slug: "pool-probe".to_string(),
            name: None,
            harness: AgentHarnessKind::TypeScript,
            typescript: Some(TypeScriptHarnessConfig {
                module_path: module_path.to_str().expect("utf8 path").to_string(),
                tool_module_paths: Vec::new(),
            }),
            enable_agent_tool_creation: false,
            sandbox_image: None,
            sandbox_provider: SandboxProvider::LocalProcess,
            enable_networking: false,
            model: "unused".to_string(),
            max_output_tokens: None,
            max_tool_round_trips: None,
            braintrust: None,
        })
        .await
        .expect("create agent");

    let mut conversations = Vec::new();
    for index in 0..4 {
        conversations.push(
            agent
                .create_conversation(CreateConversationRequest {
                    slug: Some(format!("probe-{index}")),
                    ..Default::default()
                })
                .await
                .expect("create conversation"),
        );
    }

    let send = |conversation: Arc<dyn crate::HarnessConversation>| async move {
        conversation
            .send(SendRequest {
                input: vec![Message::User {
                    content: UserContent::String("go".to_string()),
                }],
                session_id: None,
            })
            .await
            .expect("send");
    };

    tokio::time::timeout(
        Duration::from_secs(60),
        futures::future::join_all(conversations.iter().cloned().map(send)),
    )
    .await
    .expect("concurrent sends timed out");

    let records = parse_probe(&std::fs::read_to_string(&probe_path).expect("probe file"));
    let starts: Vec<_> = records.iter().filter(|r| r.1 == "start").collect();
    let ends: Vec<_> = records.iter().filter(|r| r.1 == "end").collect();
    assert_eq!(starts.len(), 4);
    assert_eq!(ends.len(), 4);

    let pids: std::collections::HashSet<u64> = starts.iter().map(|r| r.0).collect();
    assert_eq!(
        pids.len(),
        4,
        "4 concurrent turns must use 4 runner processes"
    );

    let last_start = starts.iter().map(|r| r.2).max().expect("starts");
    let first_end = ends.iter().map(|r| r.2).min().expect("ends");
    assert!(
        last_start < first_end,
        "all 4 turns must overlap: last start {last_start} >= first end {first_end}"
    );

    // A follow-up turn must reuse a warm runner instead of spawning a fifth.
    tokio::time::timeout(Duration::from_secs(60), send(Arc::clone(&conversations[0])))
        .await
        .expect("reuse send timed out");
    let records = parse_probe(&std::fs::read_to_string(&probe_path).expect("probe file"));
    let reuse_pid = records.last().expect("reuse record").0;
    assert!(
        pids.contains(&reuse_pid),
        "follow-up turn must reuse a pooled runner, got fresh pid {reuse_pid}"
    );
}
