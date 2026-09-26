use clap::Parser;

use super::*;

#[test]
fn provider_help_and_parser_only_expose_provider_options() {
    for command in ["create", "update", "switch", "login"] {
        let help = crate::Cli::try_parse_from(["exo", "provider", command, "--help"])
            .unwrap_err()
            .to_string();
        for option in [
            "--harness",
            "--root",
            "--egress-policy",
            "--secret-backend",
            "--pricing-path",
            "--exoharness-url",
        ] {
            assert!(!help.contains(option), "{help}");
            assert!(
                crate::Cli::try_parse_from(["exo", "provider", command, "test", option, "unused"])
                    .is_err()
            );
        }
        if command != "login" {
            assert!(help.contains("--context"), "{help}");
        }
    }
}

#[tokio::test]
async fn http_runtime_does_not_initialize_local_state_or_resolve_remote_harnesses() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let root = temp.path().join("local-state");
    let cli = crate::Cli::try_parse_from([
        "exo",
        "agent",
        "run",
        "--root",
        root.to_str().unwrap(),
        "--agent-file",
        "remote-agent.md",
        "--prompt",
        "hello",
    ])?;
    let definition = exo_managed_agents::AgentDefinition::parse(
        "---\nname: remote-agent\nharness: server-owned-harness\nconfig:\n  model: server-model\n---\n\nAnswer questions.".into(),
    )?;
    let runtime = runtime(
        &cli,
        Some(RuntimeClient::new("http://127.0.0.1:1/exo")?),
        Some(&definition),
        &crate::env::CliEnvironment::default(),
    )
    .await?;
    assert!(!root.exists());
    runtime.shutdown().await?;
    Ok(())
}

#[test]
fn http_commands_reject_local_execution_options() -> Result<()> {
    for args in [
        vec!["exo", "agent", "run", "--agent", "support", "--tui"],
        vec![
            "exo",
            "agent",
            "run",
            "--agent",
            "support",
            "--sandbox",
            "local-process",
        ],
        vec!["exo", "serve", "--agent", "support"],
        vec![
            "exo", "thread", "update", "support", "thread", "--vault", "personal", "--model",
            "other",
        ],
        vec![
            "exo",
            "thread",
            "update",
            "support",
            "thread",
            "--vault",
            "personal",
            "--clear-provider",
        ],
    ] {
        let cli = crate::Cli::try_parse_from(args)?;
        assert!(validate_http_command(&cli.command).is_err());
    }
    Ok(())
}

fn store_with_profiles(directory: &Path) -> Result<Store> {
    let mut store = Store::load(directory.into())?;
    store.update(|config| {
        for name in ["original", "other"] {
            config.profiles.insert(
                name.into(),
                Profile {
                    id: Uuid7::now(),
                    connection: Connection::Http {
                        endpoint: "http://localhost:1234/runtime".into(),
                    },
                    api_key_env: None,
                    account_id: None,
                    client_id: None,
                    stored_credentials: false,
                    scopes: vec![],
                    context: BTreeMap::new(),
                },
            );
        }
        Ok(())
    })?;
    Ok(store)
}

async fn configure(store: &mut Store, args: &[&str]) -> Result<()> {
    let cli =
        crate::Cli::try_parse_from(["exo", "provider"].into_iter().chain(args.iter().copied()))?;
    let crate::Commands::Provider { command } = cli.command else {
        unreachable!()
    };
    run(command.as_ref(), store).await
}

#[test]
fn aliases_pin_account_agent_and_provider_across_reload() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut first = store_with_profiles(temp.path())?;
    let mut stale = Store::load(temp.path().into())?;
    let agent = Uuid7::now();
    let thread = Uuid7::now();
    first.pin_agent(
        "agent".into(),
        &first.selection("original", None)?,
        "account-a",
        agent,
    )?;
    first.pin_thread(
        "thread".into(),
        &first.selection("original", None)?,
        "account-a",
        agent,
        thread,
    )?;
    for (provider, account) in [("other", "account-a"), ("original", "account-b")] {
        for error in [
            stale
                .pin_agent(
                    "agent".into(),
                    &stale.selection(provider, None)?,
                    account,
                    agent,
                )
                .unwrap_err(),
            stale
                .pin_thread(
                    "thread".into(),
                    &stale.selection(provider, None)?,
                    account,
                    agent,
                    thread,
                )
                .unwrap_err(),
        ] {
            assert!(
                error
                    .to_string()
                    .contains("provider original (account account-a)"),
                "{error}"
            );
        }
    }
    assert!(
        stale
            .pin_thread(
                "thread".into(),
                &stale.selection("original", None)?,
                "account-a",
                Uuid7::now(),
                thread
            )
            .is_err()
    );
    stale.pin_agent(
        "agent".into(),
        &stale.selection("original", None)?,
        "account-a",
        agent,
    )?;
    stale.pin_thread(
        "thread".into(),
        &stale.selection("original", None)?,
        "account-a",
        agent,
        thread,
    )?;
    let mut store = Store::load(temp.path().into())?;
    store.pin_agent(
        "thread".into(),
        &store.selection("other", None)?,
        "account-b",
        Uuid7::now(),
    )?;
    let mut cli = crate::Cli::try_parse_from(["exo", "thread", "get", "agent", "thread"])?;
    assert!(
        store
            .resolve_aliases(&mut cli.command, "account-b")
            .is_err()
    );
    store.resolve_aliases(&mut cli.command, "account-a")?;
    let (resolved_agent, resolved_thread) = crate::command_refs_mut(&mut cli.command);
    assert_eq!(resolved_agent.unwrap(), &agent.to_string());
    assert_eq!(resolved_thread.unwrap(), &thread.to_string());
    let mut cli =
        crate::Cli::try_parse_from(["exo", "thread", "get", &agent.to_string(), "thread"])?;
    assert!(
        store
            .resolve_aliases(&mut cli.command, "account-b")
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn stale_stores_preserve_unrelated_aliases_defaults_and_profiles() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut first = store_with_profiles(temp.path())?;
    let mut second = Store::load(temp.path().into())?;
    let removed_agent = Uuid7::now();
    let kept_agent = Uuid7::now();
    let removed_thread = Uuid7::now();
    let kept_thread = Uuid7::now();
    first.pin_agent(
        "removed".into(),
        &first.selection("original", None)?,
        "account",
        removed_agent,
    )?;
    second.pin_agent(
        "kept".into(),
        &second.selection("other", None)?,
        "account",
        kept_agent,
    )?;
    first.pin_thread(
        "removed-thread".into(),
        &first.selection("original", None)?,
        "account",
        removed_agent,
        removed_thread,
    )?;
    second.pin_thread(
        "kept-thread".into(),
        &second.selection("other", None)?,
        "account",
        kept_agent,
        kept_thread,
    )?;
    first.unpin_thread("original", "account", removed_thread)?;
    configure(&mut second, &["switch", "other"]).await?;
    configure(&mut first, &["switch", "original", "--local"]).await?;
    configure(&mut second, &["update", "original", "--scope", "read"]).await?;
    configure(
        &mut first,
        &["update", "other", "--client-id", "new-client"],
    )
    .await?;
    first.pin_thread(
        "child".into(),
        &first.selection("original", None)?,
        "account",
        removed_agent,
        Uuid7::now(),
    )?;
    second.unpin_agent("original", "account", removed_agent)?;
    let config = Store::load(temp.path().into())?.config;
    assert_eq!(config.agents.len(), 1);
    assert_eq!(config.agents["kept"].agent_id, kept_agent);
    assert_eq!(config.threads.len(), 1);
    assert_eq!(config.threads["kept-thread"].thread_id, kept_thread);
    assert_eq!(config.default.as_deref(), Some("other"));
    assert_eq!(
        config.directory_defaults[&std::env::current_dir()?],
        "original"
    );
    assert_eq!(config.profiles["original"].scopes, ["read"]);
    assert_eq!(
        config.profiles["other"].client_id.as_deref(),
        Some("new-client")
    );
    Ok(())
}

#[tokio::test]
async fn stale_store_rejects_changes_to_the_same_profile() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut first = store_with_profiles(temp.path())?;
    let mut stale = Store::load(temp.path().into())?;
    let prior = stale.profile("original")?.clone();
    configure(&mut first, &["update", "original", "--scope", "read"]).await?;
    let error = configure(&mut stale, &["update", "original", "--client-id", "stale"])
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("provider original changed in another process")
    );
    let mut logged_in = prior.clone();
    logged_in.account_id = Some("new-account".into());
    assert!(
        stale
            .replace_profile("original", &prior, logged_in)
            .is_err()
    );
    let profile = Store::load(temp.path().into())?
        .profile("original")?
        .clone();
    assert_eq!(profile.scopes, ["read"]);
    assert!(profile.account_id.is_none());
    assert!(profile.client_id.is_none());
    Ok(())
}

#[test]
fn aliases_keep_the_connection_used_before_a_concurrent_endpoint_change() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut editor = store_with_profiles(temp.path())?;
    let mut creating_agent = Store::load(temp.path().into())?;
    let mut creating_thread = Store::load(temp.path().into())?;
    let prior = editor.profile("original")?.clone();
    let mut changed = prior.clone();
    changed.connection = Connection::Http {
        endpoint: "http://localhost:5678/runtime".into(),
    };
    editor.replace_profile("original", &prior, changed.clone())?;
    let agent = Uuid7::now();
    creating_agent.pin_agent(
        "agent".into(),
        &creating_agent.selection("original", None)?,
        "account",
        agent,
    )?;
    creating_thread.pin_thread(
        "thread".into(),
        &creating_thread.selection("original", None)?,
        "account",
        agent,
        Uuid7::now(),
    )?;
    let mut store = Store::load(temp.path().into())?;
    assert_eq!(store.profile("original")?, &changed);
    assert_eq!(store.config.agents["agent"].connection, prior.connection);
    assert_eq!(
        store.config.threads["thread"].agent.connection,
        prior.connection
    );
    for (agent, thread) in [(Some("agent"), None), (None, Some("thread"))] {
        assert!(
            store
                .selected(None, agent, thread)
                .unwrap_err()
                .to_string()
                .contains("provider configuration changed for this alias")
        );
    }
    store.unpin_agent("other", "account", agent)?;
    store.unpin_agent("original", "another-account", agent)?;
    assert!(store.config.agents.contains_key("agent"));
    let thread = store.config.threads["thread"].thread_id;
    store.unpin_thread("original", "account", thread)?;
    assert!(store.config.threads.is_empty());
    store.unpin_agent("original", "account", agent)?;
    assert!(Store::load(temp.path().into())?.config.agents.is_empty());
    Ok(())
}

#[test]
fn local_commands_resolve_agent_and_thread_aliases() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut store = store_with_profiles(temp.path())?;
    let prior = store.profile("original")?.clone();
    let mut local = prior.clone();
    local.connection = Connection::Local {
        root: temp.path().join("state"),
    };
    store.replace_profile("original", &prior, local)?;
    let agent = Uuid7::now();
    let thread = Uuid7::now();
    store.pin_agent(
        "agent".into(),
        &store.selection("original", None)?,
        "account",
        agent,
    )?;
    store.pin_thread(
        "thread".into(),
        &store.selection("original", None)?,
        "account",
        agent,
        thread,
    )?;
    for args in [
        "agent update agent --file agent.md",
        "agent mount create agent /tmp /work",
        "thread fork agent thread",
        "thread update agent thread --model gpt-5-mini",
        "thread mount create agent thread /tmp /work",
        "thread sandbox run agent thread pwd",
    ] {
        let mut cli =
            crate::Cli::try_parse_from(["exo"].into_iter().chain(args.split_whitespace()))?;
        let (agent_ref, thread_ref) = crate::command_refs_mut(&mut cli.command);
        let has_thread = args.starts_with("thread ");
        assert_eq!(
            store
                .selected(
                    None,
                    agent_ref.map(|s| s.as_str()),
                    thread_ref.map(|s| s.as_str())
                )?
                .as_ref()
                .map(|selection| selection.name.as_str()),
            Some("original"),
            "{args:?}"
        );
        assert!(
            store
                .resolve_aliases(&mut cli.command, "wrong-account")
                .is_err(),
            "{args:?}"
        );
        store.resolve_aliases(&mut cli.command, "account")?;
        let (resolved_agent, resolved_thread) = crate::command_refs_mut(&mut cli.command);
        assert_eq!(resolved_agent.unwrap(), &agent.to_string(), "{args:?}");
        assert_eq!(
            resolved_thread.map(|s| s.as_str()),
            has_thread.then(|| thread.to_string()).as_deref(),
            "{args:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn logout_invalidates_a_pending_first_login() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut logout = store_with_profiles(temp.path())?;
    let mut login = Store::load(temp.path().into())?;
    let prior = login.profile("original")?.clone();
    assert!(!prior.stored_credentials);
    configure(&mut logout, &["logout", "original"]).await?;
    let mut authenticated = prior.clone();
    authenticated.account_id = Some("alice".into());
    authenticated.stored_credentials = true;
    assert!(
        login
            .replace_profile("original", &prior, authenticated)
            .is_err()
    );
    let saved = Store::load(temp.path().into())?;
    assert_ne!(saved.profile("original")?.id, prior.id);
    assert!(!saved.profile("original")?.stored_credentials);
    assert!(saved.profile("original")?.account_id.is_none());
    Ok(())
}

#[tokio::test]
async fn local_providers_reject_context_without_changing_saved_configuration() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut store = Store::load(temp.path().into())?;
    configure(&mut store, &["create", "local", "--local-root", ".exo"]).await?;
    for args in [
        vec![
            "create",
            "invalid",
            "--local-root",
            ".exo",
            "--context",
            "workspace=a",
        ],
        vec!["update", "local", "--context", "workspace=a"],
        vec!["switch", "local", "--context", "workspace=a"],
        vec!["switch", "local", "--local", "--context", "workspace=a"],
    ] {
        let before = std::fs::read(temp.path().join("providers.json"))?;
        let error = configure(&mut store, &args).await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "context is only supported for HTTP providers"
        );
        assert_eq!(std::fs::read(temp.path().join("providers.json"))?, before);
    }
    Ok(())
}

#[tokio::test]
async fn deleting_a_provider_removes_only_its_selection_contexts() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut store = store_with_profiles(temp.path())?;
    let removed = temp.path().join("removed");
    let kept = temp.path().join("kept");
    store.update(|config| {
        for (path, name) in [(&removed, "original"), (&kept, "other")] {
            config.directory_defaults.insert(path.clone(), name.into());
            config.directory_contexts.insert(
                path.clone(),
                BTreeMap::from([("workspace".into(), name.into())]),
            );
        }
        Ok(())
    })?;
    configure(
        &mut store,
        &["switch", "original", "--context", "workspace=global"],
    )
    .await?;
    configure(&mut store, &["delete", "original"]).await?;
    let config = Store::load(temp.path().into())?.config;
    assert!(config.default.is_none());
    assert!(config.default_context.is_none());
    assert_eq!(
        config.directory_defaults,
        BTreeMap::from([(kept.clone(), "other".into())])
    );
    assert_eq!(
        config.directory_contexts,
        BTreeMap::from([(kept, BTreeMap::from([("workspace".into(), "other".into())]))])
    );
    Ok(())
}

#[test]
fn context_errors_do_not_suggest_changing_a_pinned_alias() -> Result<()> {
    let temp = tempfile::TempDir::new()?;
    let mut store = store_with_profiles(temp.path())?;
    store.pin_agent(
        "saved".into(),
        &store.selection("original", None)?,
        "account",
        Uuid7::now(),
    )?;
    let selection = store.selected(None, Some("saved"), None)?.unwrap();
    let error = exoharness::HttpResponseError {
        status: reqwest::StatusCode::BAD_REQUEST,
        url: "http://localhost:1234/runtime/agent".parse()?,
        body: serde_json::to_string(
            &exo_managed_agents::http::protocol::ProviderError::ContextRequired {
                message: "Select a workspace.".into(),
                context: BTreeMap::from([("workspace".into(), "<workspace>".into())]),
            },
        )?,
    };
    let message = request_error(error.into(), &selection).to_string();
    assert!(
        message.contains("Saved aliases retain their context."),
        "{message}"
    );
    assert!(!message.contains("Run:"), "{message}");
    Ok(())
}
