use super::*;
use crate::test_support::local_test_config;
use crate::vault::{VaultContext, global_vault};
use crate::{BasicExoHarness, ExoHarness, ForkThreadRequest, NewAgentRequest, NewThreadRequest};
use tempfile::TempDir;

fn key(value: &str) -> Secret {
    Secret::Key {
        value: value.into(),
    }
}
fn request(name: &str, value: &str, target: Option<SecretTarget>) -> PutSecretRequest {
    PutSecretRequest {
        name: name.into(),
        secret: key(value),
        target,
    }
}

#[tokio::test]
async fn vault_secrets_share_one_store_and_rotate_without_changing_id() -> Result<()> {
    let temp = TempDir::new()?;
    let harness = BasicExoHarness::new(local_test_config(temp.path())).await?;
    let vault = harness.create_vault("alice").await?;
    let target = SecretTarget::mcp("https://example.com/mcp/")?;
    let id = vault
        .put_secret(request("github", "first-secret", Some(target.clone())))
        .await?;
    assert_eq!(vault.get_secret(&id).await?, Some(key("first-secret")));
    let metadata = vault.list_secrets().await?;
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0].id, id);
    assert!(!serde_json::to_string(&metadata)?.contains("first-secret"));
    assert!(
        !std::fs::read_to_string(temp.path().join("vaults/vaults.json"))?.contains("first-secret")
    );
    let updated = vault.update_secret(&id, key("second-secret")).await?;
    assert_eq!(updated.id, id);
    assert_eq!(updated.revision, 2);
    let reopened = BasicExoHarness::new(local_test_config(temp.path())).await?;
    let other = reopened.get_vault(&vault.record().id).await?.unwrap();
    assert_eq!(
        other.resolve_secret(&id, &target).await?.secret,
        key("second-secret")
    );
    vault.delete_secret(&id).await?;
    assert!(other.resolve_secret(&id, &target).await.is_err());
    let replacement = vault
        .put_secret(request("github", "third-secret", Some(target.clone())))
        .await?;
    assert_ne!(replacement, id);
    assert!(other.resolve_secret(&id, &target).await.is_err());
    harness.delete_vault(&vault.record().id).await?;
    assert!(other.get_secret(&replacement).await.is_err());
    Ok(())
}

#[tokio::test]
async fn vault_contexts_compose_without_exposing_unattached_vaults() -> Result<()> {
    let temp = TempDir::new()?;
    let harness = BasicExoHarness::new(local_test_config(temp.path())).await?;
    let global = global_vault(&harness).await?;
    let agent_vault = harness.create_vault("agent").await?;
    let user = harness.create_vault("alice").await?;
    let other = harness.create_vault("bob").await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            name: "test".into(),
            slug: "test".into(),
            vaults: vec![agent_vault.record().id],
        })
        .await?;
    assert_eq!(
        agent
            .list_vaults()
            .await?
            .iter()
            .map(|v| v.record().id)
            .collect::<Vec<_>>(),
        vec![global.record().id, agent_vault.record().id]
    );
    assert!(agent.get_vault(&user.record().id).await?.is_none());
    let selection = vec![user.record().id];
    let thread = agent
        .new_thread(NewThreadRequest {
            vaults: selection.clone(),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        thread
            .list_vaults()
            .await?
            .iter()
            .map(|v| v.record().id)
            .collect::<Vec<_>>(),
        vec![
            global.record().id,
            agent_vault.record().id,
            user.record().id
        ]
    );
    assert!(thread.get_vault(&other.record().id).await?.is_none());
    let sibling = agent.new_thread(NewThreadRequest::default()).await?;
    assert!(sibling.get_vault(&user.record().id).await?.is_none());
    let fork = thread
        .fork(ForkThreadRequest {
            up_to_inclusive: None,
            slug: None,
            name: None,
        })
        .await?;
    assert_eq!(fork.record().vaults, selection);
    let reopened = BasicExoHarness::new(local_test_config(temp.path())).await?;
    let resumed = reopened
        .get_agent(&agent.record().id)
        .await?
        .unwrap()
        .get_thread(&thread.record().id)
        .await?
        .unwrap();
    assert_eq!(resumed.record().vaults, selection);
    assert!(resumed.get_vault(&agent_vault.record().id).await?.is_some());
    assert!(
        harness
            .delete_vault(&user.record().id)
            .await
            .unwrap_err()
            .to_string()
            .contains("attached to thread")
    );
    assert!(
        harness
            .delete_vault(&agent_vault.record().id)
            .await
            .unwrap_err()
            .to_string()
            .contains("attached to agent")
    );
    assert_eq!(resumed.list_vaults().await?.len(), 3);
    assert!(resumed.get_vault(&global.record().id).await?.is_some());
    harness.delete_agent(&agent.record().id).await?;
    harness.delete_vault(&user.record().id).await?;
    Ok(())
}

#[tokio::test]
async fn destination_checks_and_failed_updates_preserve_the_original_secret() -> Result<()> {
    let temp = TempDir::new()?;
    let harness = BasicExoHarness::new(local_test_config(temp.path())).await?;
    let alice = harness.create_vault("alice").await?;
    let bob = harness.create_vault("bob").await?;
    let target = SecretTarget::mcp("HTTPS://EXAMPLE.COM:443/mcp///?tenant=one")?;
    let id = alice
        .put_secret(request("github", "original-secret", Some(target.clone())))
        .await?;
    assert!(bob.get_secret(&id).await?.is_none());
    for url in [
        "http://example.com/mcp?tenant=one",
        "https://example.com:444/mcp?tenant=one",
        "https://other.example/mcp?tenant=one",
        "https://example.com/other?tenant=one",
        "https://example.com/mcp?tenant=two",
        "https://example.com/mcp?tenant=one",
        "https://example.com/mcp//?tenant=one",
    ] {
        assert!(
            alice
                .resolve_secret(&id, &SecretTarget::mcp(url)?)
                .await
                .is_err()
        );
    }
    assert!(
        alice
            .put_secret(request("duplicate", "key", Some(target.clone())))
            .await
            .is_err()
    );
    for value in ["", "with space", "injected\r\nHeader:value"] {
        assert!(alice.update_secret(&id, key(value)).await.is_err());
    }
    let resolved = alice.resolve_secret(&id, &target).await?;
    assert_eq!(resolved.revision, 1);
    assert_eq!(resolved.secret, key("original-secret"));
    Ok(())
}

#[tokio::test]
async fn metadata_tampering_cannot_redirect_secrets() -> Result<()> {
    let temp = TempDir::new()?;
    let harness = BasicExoHarness::new(local_test_config(temp.path())).await?;
    let vault = harness.create_vault("alice").await?;
    let id = vault
        .put_secret(request(
            "github",
            "secret",
            Some(SecretTarget::mcp("https://example.com/mcp")?),
        ))
        .await?;
    let path = temp.path().join("vaults/vaults.json");
    let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&path)?)?;
    let attacker = SecretTarget::mcp("https://attacker.example/mcp")?;
    catalog.vaults[0].secrets[0].metadata.target = Some(attacker.clone());
    std::fs::write(path, serde_json::to_vec(&catalog)?)?;
    assert!(vault.resolve_secret(&id, &attacker).await.is_err());
    Ok(())
}

#[tokio::test]
async fn temporary_harness_shares_vault_references_and_observes_rotation() -> Result<()> {
    let temp = TempDir::new()?;
    let config = local_test_config(temp.path());
    let source = BasicExoHarness::new(config.clone()).await?;
    let vault = crate::vault::global_vault(&source)
        .await
        .expect("runtime vault");
    let id = vault.put_secret(request("provider", "first", None)).await?;
    let unused = temp.path().join("unused");
    let memory = BasicExoHarness::in_memory(local_test_config(&unused), Some(&source)).await?;
    assert_eq!(
        crate::vault::global_vault(&memory)
            .await
            .expect("runtime vault")
            .record(),
        vault.record()
    );
    vault.update_secret(&id, key("second")).await?;
    assert_eq!(
        crate::vault::global_vault(&memory)
            .await
            .expect("runtime vault")
            .get_secret(&id)
            .await?,
        Some(key("second"))
    );
    assert!(!unused.exists());
    Ok(())
}

#[tokio::test]
async fn legacy_secrets_move_into_global_vault_with_stable_ids() -> Result<()> {
    let temp = TempDir::new()?;
    let config = local_test_config(temp.path());
    let cipher = crate::basic::build_secret_cipher(
        config.secret_backend.clone(),
        temp.path().to_string_lossy().into_owned(),
    )?;
    #[derive(Serialize)]
    struct LegacySecret {
        metadata: SecretMetadata,
        secret: EncryptedSecret,
    }
    let id = Uuid7::now();
    let metadata = SecretMetadata {
        id,
        name: "provider".into(),
        r#type: SecretType::Key,
        created_at: Utc::now(),
        target: None,
        revision: 1,
    };
    let path = temp.path().join("secrets").join(format!("{id}.json"));
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(
        &path,
        serde_json::to_vec(&LegacySecret {
            metadata,
            secret: cipher.encrypt_secret(&key("legacy-key"))?,
        })?,
    )?;
    let harness = BasicExoHarness::new(config.clone()).await?;
    let vault = crate::vault::global_vault(&harness)
        .await
        .expect("runtime vault");
    assert_eq!(vault.get_secret(&id).await?, Some(key("legacy-key")));
    assert!(!path.exists());
    vault.delete_secret(&id).await?;
    let reopened = BasicExoHarness::new(config).await?;
    assert!(
        crate::vault::global_vault(&reopened)
            .await
            .expect("runtime vault")
            .get_secret(&id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_writers_preserve_secrets_and_mounts_cannot_expose_the_store() -> Result<()> {
    let temp = TempDir::new()?;
    let config = local_test_config(temp.path());
    let harness = BasicExoHarness::new(config.clone()).await?;
    let id = harness.create_vault("alice").await?.record().id;
    let mut tasks = Vec::new();
    for i in 0..12 {
        let config = config.clone();
        tasks.push(tokio::spawn(async move {
            let harness = BasicExoHarness::new(config).await?;
            let vault = harness.get_vault(&id).await?.unwrap();
            vault
                .put_secret(request(&format!("secret-{i}"), "value", None))
                .await
        }));
    }
    for task in tasks {
        task.await??;
    }
    assert_eq!(
        harness
            .get_vault(&id)
            .await?
            .unwrap()
            .list_secrets()
            .await?
            .len(),
        12
    );
    assert!(config.validate_secret_mount(temp.path()).is_err());
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    config.validate_secret_mount(&workspace)?;
    Ok(())
}

#[test]
fn mcp_url_normalization_preserves_the_destination() -> Result<()> {
    assert_eq!(
        SecretTarget::mcp("HTTPS://EXAMPLE.COM:443/mcp///?a=1")?,
        SecretTarget::Mcp {
            server_url: "https://example.com/mcp///?a=1".into()
        }
    );
    for url in [
        "file:///tmp/key",
        "https://user:pass@example.com",
        "https://example.com/#fragment",
    ] {
        assert!(SecretTarget::mcp(url).is_err());
    }
    Ok(())
}

#[test]
fn oauth_credentials_validate_tokens_and_refresh_destinations() -> Result<()> {
    let target = SecretTarget::mcp("https://example.com/mcp")?;
    for (endpoint, allowed) in [
        ("https://auth.example/token", true),
        ("http://127.0.0.1/token", true),
        ("http://[::1]/token", true),
        ("http://auth.example/token", false),
        ("https://user:password@auth.example/token", false),
        ("file:///tmp/token", false),
        ("https://auth.example/token#fragment", false),
    ] {
        let secret = Secret::Oauth {
            access_token: "test-token".into(),
            refresh_token: Some("test-refresh".into()),
            expires_at: Some(0),
            refresh: Some(OAuthRefresh {
                token_endpoint: endpoint.into(),
                client_id: "client".into(),
                resource: None,
                scopes: vec![],
            }),
        };
        assert_eq!(
            validate_secret(&secret, Some(&target)).is_ok(),
            allowed,
            "{endpoint}"
        );
        assert!(
            validate_secret(&secret, Some(&SecretTarget::http("https://example.com")?)).is_err()
        );
    }
    let old: Secret = serde_json::from_str(
        r#"{"type":"oauth","access_token":"test-token","refresh_token":null}"#,
    )?;
    assert!(matches!(
        old,
        Secret::Oauth {
            expires_at: None,
            refresh: None,
            ..
        }
    ));
    validate_secret(&old, Some(&target))?;
    Ok(())
}

#[tokio::test]
async fn secret_ids_cannot_be_shadowed_by_names() -> Result<()> {
    let temp = TempDir::new()?;
    let root = BasicExoHarness::new(local_test_config(temp.path())).await?;
    let original = root.create_vault("original").await?;
    let shadow = root.create_vault("shadow").await?;
    let id = original
        .put_secret(request("token", "original", None))
        .await?;
    shadow
        .put_secret(request(&id.to_string(), "wrong-account", None))
        .await?;
    assert_eq!(
        crate::vault::find_secret(&root, &id.to_string()).await?,
        Some(crate::vault::SecretReference {
            vault_id: original.record().id,
            secret_id: id
        })
    );
    original.delete_secret(&id).await?;
    assert!(
        crate::vault::find_secret(&root, &id.to_string())
            .await?
            .is_none()
    );
    Ok(())
}

#[test]
fn mounts_protect_a_master_key_before_it_is_created() -> Result<()> {
    let temp = TempDir::new()?;
    let protected = temp.path().join("config");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&protected)?;
    std::fs::create_dir(&workspace)?;
    let mut config = local_test_config(temp.path().join("state"));
    config.secret_backend = crate::SecretBackendChoice::File {
        path: Some(protected.join("missing").join("master.key")),
    };
    assert!(config.validate_secret_mount(&protected).is_err());
    config.validate_secret_mount(&workspace)?;
    std::os::unix::fs::symlink(&protected, temp.path().join("link"))?;
    assert!(
        config
            .validate_secret_mount(&temp.path().join("link"))
            .is_err()
    );
    Ok(())
}
