use std::{
    collections::BTreeMap,
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::Args;
use executor::{
    Runtime,
    http_service::{RuntimeHttpService, server},
};

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Serve only this saved agent; omit to serve the local provider.
    pub agent: Option<String>,
    /// Loopback address for the unauthenticated HTTP server.
    #[arg(long, default_value = "127.0.0.1:4766")]
    bind: SocketAddr,
    /// Deployment configuration for adapters named in agent specs.
    #[arg(long)]
    adapters_file: Option<PathBuf>,
    /// Maximum number of adapter workers.
    #[arg(long, default_value_t = 10)]
    adapter_limit: usize,
    #[arg(long)]
    drain_marker: Option<PathBuf>,
    #[arg(long)]
    reboot_notice: Option<PathBuf>,
    /// Run adapter supervision without opening the HTTP service.
    #[arg(long)]
    adapters_only: bool,
    #[cfg(feature = "firecracker")]
    #[command(flatten, next_help_heading = "Firecracker backend options")]
    pub(crate) firecracker: crate::FirecrackerArgs,
}

pub async fn run(runtime: Arc<Runtime>, root: &Path, args: ServeArgs) -> Result<()> {
    anyhow::ensure!(
        args.adapters_only || args.bind.ip().is_loopback(),
        "agent serve only binds loopback addresses because authentication is disabled"
    );
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initializing service logging: {error}"))?;
    let mut service = RuntimeHttpService::new(runtime.clone(), None)?;
    let mut store = executor::AdapterStore::new(root.join("adapters"));
    if let Some(reference) = args.agent {
        let agent = crate::must_get_agent(&runtime, &reference).await?;
        service = service.for_agent(agent.record().id);
        store = store.for_agent(agent.record().id.to_string());
    }
    let definitions = args
        .adapters_file
        .as_deref()
        .map(crate::read_config_file::<BTreeMap<String, executor::AdapterConfig>>)
        .transpose()?
        .unwrap_or_default();
    std::fs::create_dir_all(root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("service.lock"))?;
    lock.try_lock()
        .context("another agent service is using this root")?;
    configure_adapters(&runtime, &store, &definitions).await?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let adapter_options = executor::AdapterRunOptions {
        shutdown: shutdown.clone(),
        limit: args.adapter_limit,
        drain_marker: args.drain_marker,
        reboot_notice: args.reboot_notice,
    };
    if args.adapters_only {
        return executor::run_adapters_watch(runtime, store, adapter_options).await;
    }
    let listener = TcpListener::bind(args.bind)?;
    println!(
        "listening: http://{}{}",
        listener.local_addr()?,
        exo_managed_agents::http::RUNTIME_PATH
    );
    let server = server(listener, Arc::new(service))?;
    let handle = server.handle();
    let adapters = executor::run_adapters_watch(runtime, store, adapter_options);
    tokio::pin!(server, adapters);
    tokio::select! {
        result = &mut server => {
            shutdown.cancel();
            adapters.await?;
            result.context("serving managed agents")
        },
        result = &mut adapters => {
            handle.stop(true).await;
            server.await?;
            result
        }
    }
}

async fn configure_adapters(
    runtime: &Runtime,
    store: &executor::AdapterStore,
    definitions: &BTreeMap<String, executor::AdapterConfig>,
) -> Result<()> {
    use executor::{AdapterSource, CreateConversationRequest, NewAdapter};
    let mut desired = Vec::new();
    for record in runtime.list_agents().await? {
        if !store.includes_agent(&record.id.to_string()) {
            continue;
        }
        let agent = crate::must_get_agent(runtime, &record.id.to_string()).await?;
        if let Some(definition) = exo_managed_agents::load_definition(agent.as_ref()).await? {
            for name in definition.frontmatter.adapters {
                let config = definitions.get(&name).with_context(|| {
                    format!("adapter {name} needs a deployment in --adapters-file")
                })?;
                config.validate()?;
                let mut config = config.clone();
                config.state_dir.get_or_insert_with(|| {
                    store
                        .root()
                        .join(record.id.to_string())
                        .join(&name)
                        .to_string_lossy()
                        .into_owned()
                });
                desired.push((agent.clone(), name, config));
            }
        }
    }
    let existing = store.list_adapters().await?;
    for adapter in &existing {
        if adapter.source == AdapterSource::Spec
            && !desired.iter().any(|(agent, name, _)| {
                agent.record().id.to_string() == adapter.agent_id && name == &adapter.name
            })
        {
            store.disable_adapter(&adapter.id).await?;
        }
    }
    for (agent, name, config) in desired {
        if let Some(adapter) = existing.iter().find(|adapter| {
            adapter.agent_id == agent.record().id.to_string() && adapter.name == name
        }) {
            anyhow::ensure!(
                adapter.source == AdapterSource::Spec,
                "adapter {name} already exists outside the agent spec"
            );
            let mut adapter = adapter.clone();
            adapter.config = config;
            store.put_adapter(&adapter).await?;
            store.enable_adapter(&adapter.id).await?;
        } else {
            let slug = format!("adapter-{name}");
            let thread = match runtime.get_conversation(agent.as_ref(), &slug).await? {
                Some(thread) => thread,
                None => {
                    runtime
                        .create_conversation(
                            agent.as_ref(),
                            CreateConversationRequest {
                                slug: Some(slug),
                                name: Some(name.clone()),
                                ..Default::default()
                            },
                        )
                        .await?
                }
            };
            store
                .create_adapter(NewAdapter {
                    agent_id: agent.record().id.to_string(),
                    conversation_id: thread.record().id.to_string(),
                    name,
                    source: AdapterSource::Spec,
                    config,
                })
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use exoharness::{
        BasicExoHarness, BasicExoHarnessConfig, SandboxProvider, SecretBackendChoice,
    };
    use std::collections::HashMap;

    #[tokio::test]
    async fn spec_adapters_reuse_state_and_disable_removed_attachments() -> Result<()> {
        let temp = tempfile::TempDir::new()?;
        let config = BasicExoHarnessConfig {
            root: temp.path().join("state"),
            secret_backend: SecretBackendChoice::Static([7; 32]),
            sandbox_default: SandboxProvider::LocalProcess,
            sandbox_policy: None,
            sandbox_backends: vec![exoharness::SandboxBackendRegistration::local_process()],
        };
        let runtime = Arc::new(Runtime::new(
            executor::LocalProvider::managed(
                Arc::new(BasicExoHarness::new(config.clone()).await?),
                config,
                HashMap::new(),
                Arc::new(cost::PricingTable::empty()),
            )?,
            None,
        ));
        let source = "---\nname: worker\nharness: basic\nconfig:\n  model: test\nadapters: [inbox]\n---\nHandle incoming messages.";
        let definition = exo_managed_agents::AgentDefinition::parse(source.into())?;
        let agent = runtime.create_managed_agent(&definition, "worker").await?;
        let other = runtime.create_managed_agent(&definition, "other").await?;
        let store = executor::AdapterStore::new(temp.path().join("adapters"))
            .for_agent(agent.record().id.to_string());
        let mut definitions = BTreeMap::new();
        assert!(
            configure_adapters(&runtime, &store, &definitions)
                .await
                .is_err()
        );
        assert!(store.list_adapters().await?.is_empty());
        definitions.insert(
            "inbox".into(),
            executor::AdapterConfig {
                adapter_type: "test".into(),
                worker_command: vec!["cat".into()],
                initialization: serde_json::Value::Null,
                state_dir: None,
                secret_env: vec![],
            },
        );
        configure_adapters(&runtime, &store, &definitions).await?;
        let first = store.list_adapters().await?.remove(0);
        definitions.get_mut("inbox").unwrap().worker_command = vec![
            "sh".into(),
            "-c".into(),
            "printf '{\"type\":\"connected\"}\\n'; exec cat".into(),
        ];
        configure_adapters(&runtime, &store, &definitions).await?;
        let second = store.list_adapters().await?.remove(0);
        assert_eq!(first.id, second.id);
        assert_eq!(first.conversation_id, second.conversation_id);
        assert_eq!(
            second.config.worker_command,
            definitions["inbox"].worker_command
        );
        assert!(
            exo_managed_agents::list_threads(other.as_ref())
                .await?
                .is_empty()
        );
        let removed = exo_managed_agents::AgentDefinition::parse(
            source.replace("adapters: [inbox]", "adapters: []"),
        )?;
        runtime.update_managed_agent(&agent, &removed).await?;
        configure_adapters(&runtime, &store, &definitions).await?;
        assert!(!store.list_adapters().await?[0].enabled);
        runtime.update_managed_agent(&agent, &definition).await?;
        configure_adapters(&runtime, &store, &definitions).await?;
        assert!(store.list_adapters().await?[0].enabled);
        assert_eq!(
            exo_managed_agents::list_threads(agent.as_ref())
                .await?
                .len(),
            1
        );
        let shutdown = tokio_util::sync::CancellationToken::new();
        let runner = tokio::spawn(executor::run_adapters_watch(
            runtime.clone(),
            store.clone(),
            executor::AdapterRunOptions {
                shutdown: shutdown.clone(),
                ..Default::default()
            },
        ));
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let current = store.list_adapters().await?.remove(0);
                if let Some(error) = current.last_error {
                    anyhow::bail!("worker failed: {error}");
                }
                if current.last_connected_at_ms.is_some() {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .context("waiting for worker connection")??;
        shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), runner)
            .await
            .context("draining worker")???;
        runtime.shutdown().await
    }
}
