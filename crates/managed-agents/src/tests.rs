use super::*;
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};

const SOURCE: &str = "---\nname: support-analyst\nharness: codex\nconfig:\n  model: gpt-5.6-sol\n---\n\nInvestigate tickets.\n\n---\nCite each ticket.\n";

struct TestMcpResolver(&'static str);

#[async_trait]
impl mcp::McpServerResolver for TestMcpResolver {
    async fn resolve(&self, _name: &str) -> Result<String> {
        Ok(self.0.to_string())
    }
}

#[tokio::test]
async fn provider_mcp_definitions_resolve_in_the_host_context() -> Result<()> {
    let source = SOURCE.replace("config:\n", "mcp_servers:\n  - type: provider\n    name: tickets\n    allowed_tools: [search]\n    blocked_tools: [delete]\nconfig:\n");
    let definition = AgentDefinition::parse(source.clone())?;
    let root = storage().await?;
    let agent = create_agent(&StorageBackend(root.clone()), &definition, "support").await?;
    let saved = load_definition(agent.as_ref()).await?.unwrap();
    assert_eq!(saved.source, source);
    for url in ["http://127.0.0.1:8000/mcp", "https://tickets.example/mcp"] {
        let servers = saved.resolve_mcp_servers(&TestMcpResolver(url)).await?;
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "tickets");
        assert_eq!(servers[0].url, url);
        assert_eq!(servers[0].allowed_tools, Some(vec!["search".into()]));
        assert_eq!(servers[0].blocked_tools, ["delete"]);
    }
    assert!(
        saved
            .resolve_mcp_servers(&())
            .await
            .unwrap_err()
            .to_string()
            .contains("resolving MCP server tickets")
    );
    for url in [
        "file:///tmp/mcp",
        "https://user:secret@tickets.example/mcp",
        "https://tickets.example/mcp#fragment",
    ] {
        assert!(
            saved
                .resolve_mcp_servers(&TestMcpResolver(url))
                .await
                .is_err()
        );
    }
    for invalid in [
        source.replace("name: tickets", "name: ''"),
        source.replace(
            "name: tickets",
            "name: tickets\n    url: https://tickets.example/mcp",
        ),
        source.replace("name: tickets", "name: tickets\n    token: secret"),
        source.replace("name: tickets", "name: tickets\n    provider: internal"),
        source.replace(
            "config:\n",
            "  - type: url\n    name: tickets\n    url: https://other.example/mcp\nconfig:\n",
        ),
    ] {
        assert!(AgentDefinition::parse(invalid).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn provider_resolver_receives_each_server_name() -> Result<()> {
    struct Resolver;
    #[async_trait]
    impl mcp::McpServerResolver for Resolver {
        async fn resolve(&self, name: &str) -> Result<String> {
            Ok(format!("https://{name}.example/mcp"))
        }
    }
    let definition = AgentDefinition::parse(SOURCE.replace("config:\n", "mcp_servers:\n  - type: provider\n    name: tickets\n  - type: provider\n    name: docs\nconfig:\n"))?;
    let servers = definition.resolve_mcp_servers(&Resolver).await?;
    assert_eq!(servers[0].url, "https://tickets.example/mcp");
    assert_eq!(servers[1].url, "https://docs.example/mcp");
    Ok(())
}

#[tokio::test]
async fn saved_mcp_definitions_preserve_filters_and_reject_embedded_credentials() -> Result<()> {
    let source = SOURCE.replace("config:\n", "mcp_servers:\n  - type: url\n    name: tickets\n    url: https://tickets.example/mcp\n    allowed_tools: [search, read]\n    blocked_tools: [delete]\nconfig:\n");
    let definition = AgentDefinition::parse(source.clone())?;
    let root = storage().await?;
    let agent = create_agent(&StorageBackend(root.clone()), &definition, "support").await?;
    let saved = load_definition(agent.as_ref()).await?.unwrap();
    assert_eq!(saved.source, source);
    let servers = saved.resolve_mcp_servers(&()).await?;
    let server = &servers[0];
    assert_eq!(server.name, "tickets");
    assert_eq!(
        server.allowed_tools.as_deref(),
        Some(["search".to_string(), "read".to_string()].as_slice())
    );
    assert_eq!(server.blocked_tools, ["delete"]);
    for invalid in [
        source.replace("type: url", "type: stdio"),
        source.replace(
            "https://tickets.example",
            "https://user:secret@tickets.example",
        ),
        source.replace("name: tickets", "name: tickets\n    token: secret"),
    ] {
        assert!(AgentDefinition::parse(invalid).is_err());
    }
    Ok(())
}

#[test]
fn parses_frontmatter_and_preserves_the_original_document() -> Result<()> {
    for source in [
        SOURCE.to_string(),
        format!("\u{feff}{}", SOURCE.replace('\n', "\r\n")),
    ] {
        let definition = AgentDefinition::parse(source.clone())?;
        assert_eq!(definition.frontmatter.name, "support-analyst");
        assert_eq!(definition.frontmatter.harness, "codex");
        assert_eq!(definition.frontmatter.config.model, "gpt-5.6-sol");
        assert_eq!(
            definition.instructions,
            "Investigate tickets.\n\n---\nCite each ticket."
        );
        assert_eq!(definition.source, source);
        assert_eq!(
            definition.system_prompt(),
            "You are support-analyst.\n\nInvestigate tickets.\n\n---\nCite each ticket."
        );
    }
    for source in [
        SOURCE.replace("name: support-analyst", "name: ''"),
        SOURCE.replace("harness: codex", "harness: ''"),
        SOURCE.replace("model: gpt-5.6-sol", "model: ''"),
        SOURCE.replace("model: gpt-5.6-sol", "model: gpt-5.6-sol\n  temperature: 1"),
        SOURCE.replace("config:\n", "permissions: always_ask\nconfig:\n"),
        SOURCE.replacen("---\n", "", 1),
        "---\nname: support-analyst".to_string(),
        SOURCE.split("\n\nInvestigate").next().unwrap().to_string(),
    ] {
        assert!(AgentDefinition::parse(source).is_err());
    }
    Ok(())
}

async fn storage() -> Result<Arc<dyn ExoHarness>> {
    Ok(Arc::new(
        BasicExoHarness::in_memory(
            BasicExoHarnessConfig {
                root: PathBuf::new(),
                secret_backend: SecretBackendChoice::Static([0; 32]),
                sandbox_default: SandboxProvider::LocalProcess,
                sandbox_policy: None,
                sandbox_backends: vec![SandboxBackendRegistration::local_process()],
            },
            None,
        )
        .await?,
    ))
}

#[tokio::test]
async fn saves_definitions_and_resumes_only_threads_owned_by_the_agent() -> Result<()> {
    let root = storage().await?;
    let backend = StorageBackend(root.clone());
    let definition = AgentDefinition::parse(SOURCE.to_string())?;
    let agent = create_agent(&backend, &definition, "support").await?;
    assert!(
        create_agent(&backend, &definition, "support")
            .await
            .is_err()
    );
    for reference in ["support".to_string(), agent.record().id.to_string()] {
        assert_eq!(
            find_agent(root.as_ref(), &reference).await?.record().id,
            agent.record().id
        );
    }
    assert_eq!(
        load_definition(agent.as_ref()).await?.unwrap().source,
        SOURCE
    );
    let updated = SOURCE.replace("Investigate tickets.", "Investigate escalations.");
    agent
        .write_artifact(WriteArtifactRequest {
            path: AGENT_DEFINITION_PATH.to_string(),
            contents: updated.as_bytes().to_vec(),
        })
        .await?;
    assert_eq!(
        load_definition(agent.as_ref()).await?.unwrap().source,
        updated
    );
    let opened = open_thread(
        &backend,
        &agent,
        None,
        NewThreadRequest {
            environment: None,
            vaults: vec![],
            slug: Some("tickets".to_string()),
            name: None,
        },
    )
    .await?;
    assert!(opened.created);
    for reference in ["tickets".to_string(), opened.thread.record().id.to_string()] {
        let resumed = open_thread(&backend, &agent, Some(&reference), Default::default()).await?;
        assert!(!resumed.created);
        assert_eq!(resumed.thread.record().id, opened.thread.record().id);
    }
    assert!(
        open_thread(&backend, &agent, Some("missing"), Default::default())
            .await
            .is_err()
    );
    assert_eq!(list_threads(agent.as_ref()).await?.len(), 1);
    assert!(find_agent(root.as_ref(), "support-analyst").await.is_err());
    let other = create_agent(&backend, &definition, "support-analyst").await?;
    assert_eq!(
        find_agent(root.as_ref(), "support-analyst")
            .await?
            .record()
            .id,
        other.record().id
    );
    assert!(
        find_thread(other.as_ref(), &opened.thread.record().id.to_string())
            .await
            .is_err()
    );
    assert!(list_threads(other.as_ref()).await?.is_empty());
    Ok(())
}

struct FailingBackend(Arc<dyn ExoHarness>);

#[async_trait]
impl AgentBackend for FailingBackend {
    fn exoharness(&self) -> Arc<dyn ExoHarness> {
        self.0.clone()
    }

    async fn configure_agent(
        &self,
        _agent: &Arc<dyn AgentHandle>,
        _definition: &AgentDefinition,
    ) -> Result<()> {
        bail!("runtime configuration failed")
    }
}

#[tokio::test]
async fn removes_incomplete_agents_when_runtime_configuration_fails() -> Result<()> {
    let root = storage().await?;
    let definition = AgentDefinition::parse(SOURCE.to_string())?;
    let result = create_agent(&FailingBackend(root.clone()), &definition, "support").await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("runtime configuration failed")
    );
    assert!(root.list_agents().await?.is_empty());
    create_agent(&StorageBackend(root.clone()), &definition, "support").await?;
    Ok(())
}

struct StorageBackend(Arc<dyn ExoHarness>);

impl AgentBackend for StorageBackend {
    fn exoharness(&self) -> Arc<dyn ExoHarness> {
        self.0.clone()
    }
}
