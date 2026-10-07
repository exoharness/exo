use crate::LocalProvider;
use anyhow::Result;
use exo_managed_agents::permissions::PermissionPolicy;
use exo_managed_agents::{self as managed, AgentDefinition};
use exoharness::{BasicExoHarness, WriteArtifactRequest};
use std::sync::Arc;

#[tokio::test]
async fn resumed_threads_use_updated_agent_permissions() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let config = crate::test_support::local_test_config(temp.path().join("state"));
    let state = Arc::new(BasicExoHarness::new(config.clone()).await?);

    let runtime = crate::Runtime::new(
        LocalProvider::managed(
            state,
            config,
            Default::default(),
            Arc::new(cost::PricingTable::empty()),
        )?,
        None,
    );
    let source = "---\nharness: basic\nconfig:\n  model: fixture\npermission_policy: {type: always_allow}\n---\nUse tools.\n";
    let definition = AgentDefinition::parse(source.into())?;
    let agent = runtime
        .create_managed_agent(&definition, "permissions", "permissions")
        .await?;
    let opened = runtime
        .open_managed_thread(&agent, None, Default::default(), &Default::default())
        .await?;
    let thread = opened.thread;
    assert_eq!(
        runtime
            .get_conversation_config(thread.as_ref())
            .await?
            .permissions
            .for_tool("shell"),
        PermissionPolicy::AlwaysAllow {}
    );
    for (name, expected) in [
        ("always_ask", PermissionPolicy::AlwaysAsk {}),
        ("always_allow", PermissionPolicy::AlwaysAllow {}),
    ] {
        agent
            .write_artifact(WriteArtifactRequest {
                path: managed::AGENT_DEFINITION_PATH.into(),
                contents: source.replace("always_allow", name).into_bytes(),
            })
            .await?;
        let resumed = runtime
            .open_managed_thread(
                &agent,
                Some(&thread.record().id.to_string()),
                Default::default(),
                &Default::default(),
            )
            .await?;
        assert!(!resumed.created);
        assert_eq!(
            runtime
                .get_conversation_config(thread.as_ref())
                .await?
                .permissions
                .for_tool("shell"),
            expected
        );
        assert_eq!(
            crate::load_conversation_config(thread.as_ref())
                .await?
                .permissions
                .for_tool("shell"),
            expected
        );
    }
    runtime.shutdown().await?;
    Ok(())
}
