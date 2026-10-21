use super::*;

#[test]
fn resource_declarations_validate_sources_and_mounts() -> Result<()> {
    let source = SOURCE.replace("model:\n", "resources:\n  - name: code\n    type: git\n    url: https://github.com/exoharness/exo\n    checkout: main\n    credential: github\n    mount_path: /workspace\n  - name: data\n    type: directory\n    path: ./fixtures\n    mount_path: /data\n    mode: ro\nmodel:\n");
    let definition = AgentDefinition::parse(source.clone())?;
    assert_eq!(definition.frontmatter.resources.len(), 2);
    let default_mount = AgentDefinition::parse(source.replace("    mount_path: /workspace\n", ""))?;
    assert_eq!(
        default_mount.frontmatter.resources[0].mount_path(),
        "/workspace/code"
    );
    let prepared = exoharness::resources::PreparedResource {
        definition: default_mount.frontmatter.resources[0].clone(),
        snapshot: None,
    };
    let mut explicit_mount = prepared.clone();
    explicit_mount.definition.mount_path = Some("/workspace/code".into());
    assert!(prepared.same_workspace(&explicit_mount));
    explicit_mount.definition.mount_path = Some("/elsewhere".into());
    assert!(!prepared.same_workspace(&explicit_mount));
    assert!(
        AgentDefinition::parse(
            source
                .replace("mount_path: /data", "mount_path: /workspace/code")
                .replace("    mount_path: /workspace\n", "")
        )
        .is_err()
    );
    for invalid in [
        source.replace(
            "https://github.com/exoharness/exo",
            "https://secret@github.com/org/repo",
        ),
        source.replace("https://github.com/exoharness/exo", "file:///etc"),
        source.replace("mount_path: /data", "mount_path: /workspace/data"),
        source.replace("name: data", "name: ../data"),
        source.replace("path: ./fixtures", "path: ./fixtures\n    typo: value"),
        source.replace("type: git", "type: git\n    path: ./repo"),
        source.replace("type: git", "type: git_repository"),
        source.replace("checkout: main", "checkout: {type: branch, name: main}"),
        source.replace("checkout: main", "checkout: HEAD~1"),
        source.replace("    mount_path: /data\n", ""),
    ] {
        assert!(AgentDefinition::parse(invalid).is_err());
    }
    Ok(())
}
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};

const SOURCE: &str = "---\nharness: codex\nmodel:\n  name: gpt-6.1-sol\n---\n\nInvestigate tickets.\n\n---\nCite each ticket.\n";

#[test]
fn sandbox_networking_defaults_to_enabled() -> Result<()> {
    let sandbox: AgentSandboxConfig = serde_yaml_ng::from_str("provider: docker")?;
    assert!(sandbox.enable_networking);
    Ok(())
}

struct TestMcpResolver(&'static str);

#[async_trait]
impl mcp::McpServerResolver for TestMcpResolver {
    async fn resolve(&self, _name: &str) -> Result<String> {
        Ok(self.0.to_string())
    }
}

#[tokio::test]
async fn provider_mcp_definitions_resolve_in_the_host_context() -> Result<()> {
    let source = SOURCE.replace("model:\n", "mcp_servers:\n  - type: provider\n    name: tickets\n    allowed_tools: [search]\n    blocked_tools: [delete]\nmodel:\n");
    let definition = AgentDefinition::parse(source.clone())?;
    let root = storage().await?;
    let agent = create_agent(
        &StorageBackend(root.clone()),
        &definition,
        "support",
        "support",
    )
    .await?;
    let saved = load_definition(agent.as_ref()).await?.unwrap();
    assert_eq!(saved.to_json()?, definition.to_json()?);
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
            "model:\n",
            "  - type: url\n    name: tickets\n    url: https://other.example/mcp\nmodel:\n",
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
    let definition = AgentDefinition::parse(SOURCE.replace("model:\n", "mcp_servers:\n  - type: provider\n    name: tickets\n  - type: provider\n    name: docs\nmodel:\n"))?;
    let servers = definition.resolve_mcp_servers(&Resolver).await?;
    assert_eq!(servers[0].url, "https://tickets.example/mcp");
    assert_eq!(servers[1].url, "https://docs.example/mcp");
    Ok(())
}

#[tokio::test]
async fn saved_mcp_definitions_preserve_filters_and_reject_embedded_credentials() -> Result<()> {
    let source = SOURCE.replace("model:\n", "mcp_servers:\n  - type: url\n    name: tickets\n    url: https://tickets.example/mcp\n    allowed_tools: [search, read]\n    blocked_tools: [delete]\nmodel:\n");
    let definition = AgentDefinition::parse(source.clone())?;
    let root = storage().await?;
    let agent = create_agent(
        &StorageBackend(root.clone()),
        &definition,
        "support",
        "support",
    )
    .await?;
    let saved = load_definition(agent.as_ref()).await?.unwrap();
    assert_eq!(saved.to_json()?, definition.to_json()?);
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
        SOURCE.replacen("---\n", "---\nname: support-analyst\n", 1),
        format!("\u{feff}{}", SOURCE.replace('\n', "\r\n")),
    ] {
        let definition = AgentDefinition::parse(source.clone())?;
        assert_eq!(definition.frontmatter.harness, "codex");
        assert_eq!(definition.frontmatter.model.name, "gpt-6.1-sol");
        assert_eq!(
            definition.instructions,
            "Investigate tickets.\n\n---\nCite each ticket."
        );
        assert_eq!(definition.source(), source);
    }
    for source in [
        SOURCE.replace("model:\n", "config:\n"),
        SOURCE.replace("name: gpt-6.1-sol", "model: gpt-6.1-sol"),
        SOURCE.replace("harness: codex", "harness: ''"),
        SOURCE.replace("name: gpt-6.1-sol", "name: ''"),
        SOURCE.replace("name: gpt-6.1-sol", "name: gpt-6.1-sol\n  temperature: 1"),
        SOURCE.replace("model:\n", "permissions: always_ask\nmodel:\n"),
        SOURCE.replace(
            "model:\n",
            "sandbox: {provider: docker, image: ubuntu:24.04}\nmodel:\n",
        ),
        SOURCE.replacen("---\n", "", 1),
        "---\nharness: codex".to_string(),
        SOURCE.split("\n\nInvestigate").next().unwrap().to_string(),
    ] {
        assert!(AgentDefinition::parse(source).is_err());
    }
    Ok(())
}

async fn storage() -> Result<Arc<dyn ExoHarness>> {
    Ok(Arc::new(
        BasicExoHarness::in_memory(BasicExoHarnessConfig {
            root: PathBuf::new(),
            secret_backend: SecretBackendChoice::Static([0; 32]),
            sandbox_default: SandboxProvider::LocalProcess,
            sandbox_policy: None,
            sandbox_backends: vec![SandboxBackendRegistration::local_process()],
        })
        .await?,
    ))
}

#[tokio::test]
async fn saves_definitions_and_resumes_only_threads_owned_by_the_agent() -> Result<()> {
    let root = storage().await?;
    let backend = StorageBackend(root.clone());
    let definition = AgentDefinition::parse(SOURCE.to_string())?;
    let agent = create_agent(&backend, &definition, "Support Analyst", "support").await?;
    assert_eq!(agent.record().name, "Support Analyst");
    assert_eq!(agent.record().slug, "support");
    assert!(
        create_agent(&backend, &definition, "support", "support")
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
        load_definition(agent.as_ref()).await?.unwrap().instructions,
        AgentDefinition::parse(SOURCE.into())?.instructions
    );
    let updated = SOURCE.replace("Investigate tickets.", "Investigate escalations.");
    agent
        .write_artifact(WriteArtifactRequest {
            path: AGENT_DEFINITION_PATH.to_string(),
            contents: AgentDefinition::parse(updated.clone())?.to_json()?,
        })
        .await?;
    assert_eq!(
        load_definition(agent.as_ref()).await?.unwrap().instructions,
        AgentDefinition::parse(updated)?.instructions
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
        &Default::default(),
    )
    .await?;
    assert!(opened.created);
    for reference in ["tickets".to_string(), opened.thread.record().id.to_string()] {
        let resumed = open_thread(
            &backend,
            &agent,
            Some(&reference),
            Default::default(),
            &Default::default(),
        )
        .await?;
        assert!(!resumed.created);
        assert_eq!(resumed.thread.record().id, opened.thread.record().id);
    }
    assert!(
        open_thread(
            &backend,
            &agent,
            Some("missing"),
            Default::default(),
            &Default::default()
        )
        .await
        .is_err()
    );
    assert_eq!(list_threads(agent.as_ref()).await?.len(), 1);
    assert!(find_agent(root.as_ref(), "support-analyst").await.is_err());
    let other = create_agent(&backend, &definition, "support-analyst", "support-analyst").await?;
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
    let result = create_agent(
        &FailingBackend(root.clone()),
        &definition,
        "support",
        "support",
    )
    .await;
    assert!(
        result
            .err()
            .unwrap()
            .to_string()
            .contains("runtime configuration failed")
    );
    assert!(root.list_agents().await?.is_empty());
    create_agent(
        &StorageBackend(root.clone()),
        &definition,
        "support",
        "support",
    )
    .await?;
    Ok(())
}

struct StorageBackend(Arc<dyn ExoHarness>);

impl AgentBackend for StorageBackend {
    fn exoharness(&self) -> Arc<dyn ExoHarness> {
        self.0.clone()
    }
}

#[test]
fn invalid_checkout_diagnostic_identifies_the_leaf_field() -> Result<()> {
    let source = "---\nharness: codex\nmodel:\n  name: gpt-6.1-sol\nresources:\n  - name: braintrust\n    type: git\n    url: https://github.com/braintrustdata/braintrust.git\n    checkout: { type: branch, name: main }\n---\nInvestigate.\n";
    let directory = tempfile::TempDir::new()?;
    let path = directory.path().join("sre.md");
    // The displayed location must refer to Markdown, including the delimiter,
    // even with Windows line endings and a UTF-8 BOM.
    std::fs::write(&path, format!("\u{feff}{}", source.replace('\n', "\r\n")))?;
    let error = AgentDefinition::load(&path).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("{}:9:15:", path.display())),
        "{error}"
    );
    assert!(error.contains("resources[0].checkout"), "{error}");
    assert!(
        error.contains("invalid type: map, expected a string"),
        "{error}"
    );
    assert!(
        error.contains("9 |     checkout: { type: branch, name: main }"),
        "{error}"
    );
    Ok(())
}

#[test]
fn model_type_errors_and_yaml_syntax_errors_have_source_diagnostics() -> Result<()> {
    let directory = tempfile::TempDir::new()?;
    let path = directory.path().join("agent.md");
    std::fs::write(
        &path,
        SOURCE.replace("name: gpt-6.1-sol", "name: [gpt-6.1-sol]"),
    )?;
    let error = AgentDefinition::load(&path).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("{}:4:9:", path.display())),
        "{error}"
    );
    assert!(error.contains("model.name"), "{error}");
    assert!(error.contains("4 |   name: [gpt-6.1-sol]"), "{error}");

    std::fs::write(&path, SOURCE.replace("name: gpt-6.1-sol", "name: @invalid"))?;
    let error = AgentDefinition::load(&path).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("{}:4:9:", path.display())),
        "{error}"
    );
    assert!(error.contains("4 |   name: @invalid"), "{error}");
    Ok(())
}

#[test]
fn structured_definitions_validate_and_export_markdown_without_transporting_source() -> Result<()> {
    let definition = AgentDefinition::parse(SOURCE.replace("model:\n", "resources:\n  - type: git\n    name: code\n    url: https://github.com/exoharness/exo\n    checkout: main\nmcp_servers:\n  - type: url\n    name: tickets\n    url: https://tickets.example/mcp\n    permission_policy: {type: always_ask}\nmodel:\n"))?;
    let bytes = definition.to_json()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert!(value.get("source").is_none());
    assert!(value.get("path").is_none());
    let decoded = AgentDefinition::from_json(&bytes)?;
    assert!(decoded.path().is_none());
    assert_eq!(decoded.to_json()?, bytes);
    assert_eq!(
        AgentDefinition::parse(decoded.source().into())?.to_json()?,
        bytes
    );
    let mut invalid = value;
    invalid["instructions"] = "".into();
    assert!(AgentDefinition::from_json(&serde_json::to_vec(&invalid)?).is_err());
    invalid["instructions"] = "Instructions".into();
    invalid["frontmatter"]["unexpected"] = true.into();
    assert!(AgentDefinition::from_json(&serde_json::to_vec(&invalid)?).is_err());
    Ok(())
}
