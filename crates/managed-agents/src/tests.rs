use super::*;
use exoharness::{
    BasicExoHarness, BasicExoHarnessConfig, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};

const SOURCE: &str = "---\nname: support-analyst\nharness: codex\nconfig:\n  model: gpt-5.6-sol\n---\n\nInvestigate tickets.\n\n---\nCite each ticket.\n";

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
