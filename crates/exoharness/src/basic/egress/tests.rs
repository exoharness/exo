use super::*;
use crate::vault::SecretTarget;

#[test]
fn http_credentials_require_an_exact_https_origin() -> Result<()> {
    assert_eq!(
        SecretTarget::http("https://API.TEST:443/")?,
        SecretTarget::Http {
            origin: "https://api.test".into()
        },
    );
    for origin in [
        "http://api.test",
        "https://api.test/v1",
        "https://api.test?query",
        "https://api.test#fragment",
        "https://user:password@api.test",
    ] {
        assert!(SecretTarget::http(origin).is_err(), "{origin}");
    }
    Ok(())
}
use crate::{
    CredentialInjectionLocation, CredentialNetworkPolicy, EgressCredentialBinding, EgressPolicy,
    PutSecretRequest,
};

fn policy(name: &str) -> EgressPolicy {
    EgressPolicy {
        allowed_tcp_ports: None,
        networking: SandboxNetworkPolicy::Limited {
            allowed_hosts: vec!["api.test".into()],
        },
        credentials: vec![EgressCredentialBinding {
            name: name.into(),
            environment_variable: "API_KEY".into(),
            networking: CredentialNetworkPolicy::Limited {
                allowed_hosts: vec!["api.test".into()],
            },
            injection_location: CredentialInjectionLocation { header: true },
        }],
    }
}

fn destination(host: &str) -> EgressDestination {
    EgressDestination {
        host: host.into(),
        port: 443,
        method: hyper::Method::GET,
        path: "/v1/items".into(),
    }
}

fn resolver(harness: &BasicExoHarness) -> LocalEgressResolver {
    LocalEgressResolver {
        harness: Arc::downgrade(&harness.inner),
    }
}

async fn secret(vault: &dyn VaultHandle, name: &str, value: &str) -> Result<SecretId> {
    vault
        .put_secret(PutSecretRequest {
            name: name.into(),
            target: Some(SecretTarget::http("https://api.test")?),
            secret: Secret::Key {
                value: value.into(),
            },
        })
        .await
}

async fn bind(
    harness: &BasicExoHarness,
    scope: ResourceScope,
    name: &str,
) -> Result<EgressIdentity> {
    bind_request(
        harness,
        scope,
        CreateSandboxRequest {
            name: None,
            provider: SandboxProvider::Firecracker,
            image: "test-image".into(),
            resources: Default::default(),
            default_workdir: None,
            file_system_mounts: None,
            durable_file_systems: None,
            policy: Some(policy(name)),
            enable_networking: None,
            idle_seconds: None,
        },
    )
    .await
}

async fn bind_request(
    harness: &BasicExoHarness,
    scope: ResourceScope,
    request: CreateSandboxRequest,
) -> Result<EgressIdentity> {
    let prepared = prepare_sandbox_request(harness, scope, request).await?;
    let sandbox_id = format!("sandbox-{}", Uuid7::now());
    let owner_dir = harness.owner_dir(scope);
    harness
        .inner
        .storage
        .put_json(
            owner_dir
                .join("sandboxes")
                .join(format!("{sandbox_id}.json")),
            &prepared.stored_sandbox(sandbox_id.clone()),
        )
        .await?;
    Ok(EgressIdentity { sandbox_id, scope })
}

#[tokio::test]
async fn credentials_compose_by_scope_and_remain_pinned_after_restart() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let config = crate::test_support::local_test_config(directory.path());
    let harness = BasicExoHarness::new(config.clone()).await?;
    let global = global_vault(&harness).await?;
    let agent_vault = harness.create_vault("agent").await?;
    let alice = harness.create_vault("alice").await?;
    let bob = harness.create_vault("bob").await?;
    secret(global.as_ref(), "token", "global").await?;
    secret(agent_vault.as_ref(), "token", "agent").await?;
    let alice_id = secret(alice.as_ref(), "token", "alice").await?;
    secret(bob.as_ref(), "token", "bob").await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            slug: "agent".into(),
            name: "agent".into(),
            vaults: vec![agent_vault.record().id],
        })
        .await?;
    let first = agent
        .new_thread(crate::NewThreadRequest {
            vaults: vec![alice.record().id],
            ..Default::default()
        })
        .await?;
    let second = agent
        .new_thread(crate::NewThreadRequest {
            vaults: vec![bob.record().id],
            ..Default::default()
        })
        .await?;
    let first_scope = ResourceScope::Thread {
        agent_id: agent.record().id,
        thread_id: first.record().id,
    };
    let second_scope = ResourceScope::Thread {
        agent_id: agent.record().id,
        thread_id: second.record().id,
    };
    for (scope, expected) in [
        (ResourceScope::Global, "global"),
        (
            ResourceScope::Agent {
                agent_id: agent.record().id,
            },
            "agent",
        ),
        (first_scope, "alice"),
        (second_scope, "bob"),
    ] {
        let identity = bind(&harness, scope, "token").await?;
        assert_eq!(
            resolver(&harness)
                .resolve(&identity, "token", &destination("api.test"))
                .await?,
            expected
        );
    }
    assert!(
        bind(&harness, second_scope, &alice_id.to_string())
            .await
            .is_err()
    );
    assert!(
        bind(&harness, ResourceScope::Global, &alice_id.to_string())
            .await
            .is_err()
    );
    let identity = bind(&harness, first_scope, "token").await?;
    let other_scope = EgressIdentity {
        scope: second_scope,
        ..identity.clone()
    };
    assert!(
        resolver(&harness)
            .resolve(&other_scope, "token", &destination("api.test"))
            .await
            .is_err()
    );
    let other_agent = harness
        .new_agent(NewAgentRequest {
            slug: "other".into(),
            name: "other".into(),
            vaults: vec![alice.record().id],
        })
        .await?;
    assert!(
        bind(
            &harness,
            ResourceScope::Thread {
                agent_id: other_agent.record().id,
                thread_id: first.record().id,
            },
            "token"
        )
        .await
        .is_err()
    );

    alice
        .update_secret(
            &alice_id,
            Secret::Key {
                value: "rotated".into(),
            }
            .into(),
        )
        .await?;
    let reopened = BasicExoHarness::new(config).await?;
    assert_eq!(
        resolver(&reopened)
            .resolve(&identity, "token", &destination("api.test"))
            .await?,
        "rotated"
    );
    assert!(
        resolver(&reopened)
            .resolve(&identity, "token", &destination("other.test"))
            .await
            .is_err()
    );
    let mut wrong_port = destination("api.test");
    wrong_port.port = 8443;
    assert!(
        resolver(&reopened)
            .resolve(&identity, "token", &wrong_port)
            .await
            .is_err()
    );
    assert!(
        resolver(&reopened)
            .resolve(&identity, "unselected", &destination("api.test"))
            .await
            .is_err()
    );
    alice.delete_secret(&alice_id).await?;
    secret(alice.as_ref(), "token", "replacement").await?;
    assert!(
        resolver(&reopened)
            .resolve(&identity, "token", &destination("api.test"))
            .await
            .is_err()
    );
    let replacement = bind(&reopened, first_scope, "token").await?;
    assert_eq!(
        resolver(&reopened)
            .resolve(&replacement, "token", &destination("api.test"))
            .await?,
        "replacement"
    );
    assert!(reopened.delete_vault(&alice.record().id).await.is_err());
    reopened
        .inner
        .vaults
        .delete_vault(&alice.record().id)
        .await?;
    assert!(
        resolver(&reopened)
            .resolve(&replacement, "token", &destination("api.test"))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn http_credentials_require_explicit_destination_authorization() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let harness =
        BasicExoHarness::new(crate::test_support::local_test_config(directory.path())).await?;
    let global = global_vault(&harness).await?;
    for (name, target, host, allowed) in [
        ("model", None, "api.test", false),
        (
            "mcp",
            Some(SecretTarget::mcp("https://api.test/mcp")?),
            "api.test",
            false,
        ),
        (
            "github",
            Some(SecretTarget::http("https://github.com")?),
            "github.com",
            true,
        ),
        (
            "github",
            Some(SecretTarget::http("https://github.com")?),
            "api.github.com",
            true,
        ),
        (
            "github",
            Some(SecretTarget::http("https://github.com")?),
            "evil.github.com",
            false,
        ),
        (
            "github",
            Some(SecretTarget::http("https://github.com")?),
            "api.github.com.evil.test",
            false,
        ),
        (
            "other",
            Some(SecretTarget::http("https://other.test")?),
            "api.github.com",
            false,
        ),
    ] {
        let secret = global
            .put_secret(PutSecretRequest {
                name: name.into(),
                target,
                secret: Secret::Key {
                    value: "credential".into(),
                },
            })
            .await?;
        let identity = bind(&harness, ResourceScope::Global, name).await?;
        let resolved = resolver(&harness)
            .resolve(&identity, name, &destination(host))
            .await;
        assert_eq!(resolved.is_ok(), allowed, "{name}: {host}");
        if allowed {
            assert_eq!(resolved?, "credential");
            let mut wrong_port = destination(host);
            wrong_port.port = 8443;
            assert!(
                resolver(&harness)
                    .resolve(&identity, name, &wrong_port)
                    .await
                    .is_err()
            );
        }
        global.delete_secret(&secret).await?;
        assert!(
            resolver(&harness)
                .resolve(&identity, name, &destination(host))
                .await
                .is_err()
        );
    }
    Ok(())
}

#[tokio::test]
async fn mcp_credentials_refresh_persist_and_remain_destination_scoped() -> Result<()> {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };
    let directory = tempfile::tempdir()?;
    let config = crate::test_support::local_test_config(directory.path());
    let harness = BasicExoHarness::new(config.clone()).await?;
    let vault = harness.create_vault("mcp-account").await?;
    let target = SecretTarget::mcp("https://api.test/mcp")?;
    let oauth = MockServer::start().await;
    Mock::given(method("POST")).and(path("/token"))
        .and(body_string_contains("refresh_token=refresh-v1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token":"access-v2", "refresh_token":"refresh-v2", "token_type":"Bearer", "expires_in":3600
        }))).expect(1).mount(&oauth).await;
    let secret = vault
        .put_secret(PutSecretRequest {
            name: "mcp".into(),
            target: Some(target.clone()),
            secret: Secret::Oauth {
                access_token: "access-v1".into(),
                refresh_token: Some("refresh-v1".into()),
                expires_at: None,
                refresh: Some(crate::vault::OAuthRefresh {
                    token_endpoint: format!("{}/token", oauth.uri()),
                    client_id: "test-client".into(),
                    resource: Some("https://api.test/mcp".into()),
                    scopes: vec![],
                }),
            },
        })
        .await?;
    let agent = harness
        .new_agent(NewAgentRequest {
            name: "native".into(),
            slug: "native".into(),
            vaults: vec![vault.record().id],
        })
        .await?;
    let thread = agent.new_thread(Default::default()).await?;
    let name = secret.to_string();
    assert!(bind(&harness, ResourceScope::Global, &name).await.is_err());
    let identity = bind(
        &harness,
        ResourceScope::Thread {
            agent_id: agent.record().id,
            thread_id: thread.record().id,
        },
        &name,
    )
    .await?;
    let destination = EgressDestination {
        path: "/mcp".into(),
        ..destination("api.test")
    };
    assert_eq!(
        resolver(&harness)
            .resolve(&identity, &name, &destination)
            .await?,
        "access-v1"
    );
    for path in ["/other", "/mcp/other", "/mcp?other=true"] {
        assert!(
            resolver(&harness)
                .resolve(
                    &identity,
                    &name,
                    &EgressDestination {
                        path: path.into(),
                        host: destination.host.clone(),
                        port: destination.port,
                        method: destination.method.clone()
                    }
                )
                .await
                .is_err()
        );
    }
    assert_eq!(
        resolver(&harness)
            .refresh(&identity, &name, &destination, "access-v1")
            .await?,
        Some("access-v2".into())
    );
    let reopened = BasicExoHarness::new(config).await?;
    assert_eq!(
        resolver(&reopened)
            .resolve(&identity, &name, &destination)
            .await?,
        "access-v2"
    );
    assert_eq!(
        resolver(&reopened)
            .refresh(&identity, &name, &destination, "access-v1")
            .await?,
        Some("access-v2".into())
    );
    let stored = vault
        .get_secret(&secret)
        .await?
        .context("saved OAuth credential")?;
    assert!(
        matches!(stored, Secret::Oauth { refresh_token: Some(token), .. } if token == "refresh-v2")
    );
    vault
        .update_secret(
            &secret,
            Secret::Key {
                value: "rotated-key".into(),
            }
            .into(),
        )
        .await?;
    assert_eq!(
        resolver(&reopened)
            .resolve(&identity, &name, &destination)
            .await?,
        "rotated-key"
    );
    assert_eq!(
        resolver(&reopened)
            .refresh(&identity, &name, &destination, "rotated-key")
            .await?,
        None
    );
    vault.delete_secret(&secret).await?;
    assert!(
        resolver(&reopened)
            .resolve(&identity, &name, &destination)
            .await
            .is_err()
    );
    oauth.verify().await;
    Ok(())
}

#[tokio::test]
async fn container_registrations_reject_credentials_before_launch() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let harness =
        BasicExoHarness::in_memory(crate::test_support::local_test_config(temp.path())).await?;
    let mut policy = policy("api");
    policy.networking = SandboxNetworkPolicy::Unrestricted;
    let request = crate::SandboxRequest {
        sandbox_id: "native-egress-required".into(),
        scope: ResourceScope::Global,
        provider_state: None,
        spec: crate::SandboxSpec {
            image: "must-not-be-launched".into(),
            resources: crate::SandboxResourceShape::new(1, 512),
            mounts: vec![],
            durable_file_systems: vec![],
            policy,
            default_workdir: "/".into(),
        },
        lifecycle: crate::SandboxLifecycleConfig {
            idle_ttl: Some(std::time::Duration::from_secs(60)),
        },
    };
    for registration in [
        SandboxBackendRegistration::docker(),
        SandboxBackendRegistration::apple_container(),
    ] {
        let backend = (registration.factory)(&harness.inner).await?;
        let error = backend
            .acquire(request.clone())
            .await
            .err()
            .context("unenforced credential policy was accepted")?;
        assert!(
            error
                .to_string()
                .contains("does not support policy.credentials"),
            "{error:#}"
        );
    }
    Ok(())
}
