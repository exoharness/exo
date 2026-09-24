use std::{collections::HashMap, sync::Mutex};

use keyring_core::api::CredentialStoreApi;
use oauth2::{PkceCodeChallenge, PkceCodeVerifier};
use serde::Deserialize;
use serde_json::json;
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header, method, path, query_param},
};

use super::*;

struct Fixture {
    server: MockServer,
    profile: Profile,
    store: KeychainStore,
    _temp: TempDir,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let server = MockServer::start().await;
        let base = server.uri();
        let temp = TempDir::new()?;
        let profile = Profile {
            id: exoharness::Uuid7::now(),
            connection: Connection::Http {
                endpoint: format!("{base}/runtime?workspace=team%2Fone"),
            },
            api_key_env: None,
            account_id: None,
            client_id: Some("exo-test".into()),
            stored_credentials: true,
            scopes: vec![],
            context: std::collections::BTreeMap::from([("workspace".into(), "original".into())]),
        };
        let backend = keyring_core::mock::Store::new()?;
        let store = KeychainStore {
            entry: Arc::new(backend.build(SERVICE, &profile.id.to_string(), None)?),
            lock_path: temp.path().join("auth/profile.lock"),
        };
        Mock::given(path("/runtime/identity"))
            .and(query_param("workspace", "team/one"))
            .respond_with(ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!(
                    "Bearer resource_metadata=\"{base}/metadata/runtime\", scope=\"runtime:read\""
                ),
            ))
            .with_priority(10)
            .mount(&server)
            .await;
        for token in ["initial-token", "refreshed-token", "api-key"] {
            Mock::given(path("/runtime/identity"))
                .and(query_param("workspace", "team/one"))
                .and(header("authorization", format!("Bearer {token}")))
                .and(header("x-exo-context", "{\"workspace\":\"original\"}"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"account_id": "alice"})),
                )
                .with_priority(2)
                .mount(&server)
                .await;
        }
        Mock::given(path("/metadata/runtime"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "resource": format!("{base}/runtime"), "authorization_servers": [base],
                "scopes_supported": ["runtime:read"]
            })))
            .mount(&server)
            .await;
        Mock::given(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": base, "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"), "registration_endpoint": format!("{base}/register"),
                "response_types_supported": ["code"], "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
                "scopes_supported": ["runtime:read", "runtime:write", "offline_access"]
            }))).mount(&server).await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(|request: &wiremock::Request| {
                #[derive(Deserialize)]
                struct Registration {
                    redirect_uris: Vec<String>,
                }
                let request: Registration = request.body_json().unwrap();
                ResponseTemplate::new(201).set_body_json(json!({
                    "client_id": "exo-test", "redirect_uris": request.redirect_uris
                }))
            })
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(token_response("initial-token", "initial-refresh"))
            .mount(&server)
            .await;
        Ok(Self {
            server,
            profile,
            store,
            _temp: temp,
        })
    }

    async fn login(&self) -> Result<()> {
        let account = login_with(
            &self.profile,
            &self.store,
            |auth| async move {
                authorize(&auth, None, None).await?;
                Ok(())
            },
            Duration::from_secs(5),
        )
        .await?;
        assert_eq!(account, "alice");
        Ok(())
    }

    async fn expire(&self) -> Result<()> {
        let mut credentials = self.store.load().await?.context("stored credentials")?;
        credentials.token_received_at = Some(0);
        self.store.save(credentials).await?;
        Ok(())
    }
}

fn credentials(access: &str) -> StoredCredentials {
    StoredCredentials::new(
        "exo-test".into(),
        Some(rmcp::transport::auth::OAuthTokenResponse::new(
            oauth2::AccessToken::new(access.into()),
            oauth2::basic::BasicTokenType::Bearer,
            Default::default(),
        )),
        Vec::new(),
        None,
    )
}

fn token_response(access: &str, refresh: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "access_token": access, "refresh_token": refresh, "token_type": "Bearer",
        "expires_in": 3600, "scope": "runtime:read offline_access"
    }))
}

fn auth_query(auth: &str) -> HashMap<String, String> {
    url::Url::parse(auth)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect()
}

async fn authorize(
    auth: &str,
    state: Option<&str>,
    issuer: Option<&str>,
) -> Result<reqwest::StatusCode> {
    let query = auth_query(auth);
    let mut callback = url::Url::parse(&query["redirect_uri"])?;
    callback
        .query_pairs_mut()
        .append_pair("code", "test-code")
        .append_pair("state", state.unwrap_or(&query["state"]));
    if let Some(issuer) = issuer {
        callback.query_pairs_mut().append_pair("iss", issuer);
    }
    Ok(reqwest::get(callback).await?.status())
}

#[tokio::test]
async fn login_uses_discovery_pkce_scopes_and_persists_only_after_identity() -> Result<()> {
    for registered in [true, false] {
        let mut f = Fixture::new().await?;
        f.profile.client_id = registered.then(|| "exo-test".into());
        if !registered {
            f.profile.scopes = vec!["runtime:write".into()];
        }
        let auth_url = Arc::new(Mutex::new(String::new()));
        let capture = auth_url.clone();
        let account = login_with(
            &f.profile,
            &f.store,
            |auth| async move {
                *capture.lock().unwrap() = auth.clone();
                assert_eq!(authorize(&auth, Some("wrong-state"), None).await?, 400);
                assert_eq!(authorize(&auth, None, None).await?, 200);
                Ok(())
            },
            Duration::from_secs(5),
        )
        .await?;
        assert_eq!(account, "alice");
        let query = auth_query(&auth_url.lock().unwrap());
        assert_eq!(query["client_id"], "exo-test");
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["resource"], format!("{}/runtime", f.server.uri()));
        assert!(query["scope"].contains(if registered {
            "runtime:read"
        } else {
            "runtime:write"
        }));
        assert!(query["scope"].contains("offline_access"));
        let requests = f.server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path() == "/register")
                .count(),
            usize::from(!registered)
        );
        let exchanges: Vec<_> = requests
            .iter()
            .filter(|r| r.url.path() == "/token")
            .collect();
        assert_eq!(exchanges.len(), 1);
        let form: HashMap<String, String> = url::form_urlencoded::parse(&exchanges[0].body)
            .into_owned()
            .collect();
        let challenge = PkceCodeChallenge::from_code_verifier_sha256(&PkceCodeVerifier::new(
            form["code_verifier"].clone(),
        ));
        assert_eq!(challenge.as_str(), query["code_challenge"]);
        assert_eq!(form["redirect_uri"], query["redirect_uri"]);
        assert_eq!(form["resource"], query["resource"]);
        let credentials = f.store.load().await?.unwrap();
        assert_eq!(credentials.issuer.as_deref(), Some(f.server.uri().as_str()));
        assert_eq!(
            credentials
                .token_response
                .unwrap()
                .refresh_token()
                .unwrap()
                .secret(),
            "initial-refresh"
        );
        let reopened = ProviderTokens::new(&f.profile, f.store.clone());
        assert_eq!(reopened.access_token().await?, "initial-token");
        f.store.logout(&f.profile).await?;
        assert!(f.store.read()?.is_none());
        assert!(
            reopened
                .access_token()
                .await
                .unwrap_err()
                .to_string()
                .contains("not logged in")
        );
    }
    Ok(())
}

#[tokio::test]
async fn failed_login_preserves_previous_credentials_and_closes_callback() -> Result<()> {
    for failure in ["issuer", "denied", "identity", "browser", "timeout"] {
        let f = Fixture::new().await?;
        f.store.write(credentials("previous-token"))?;
        if failure == "identity" {
            Mock::given(path("/runtime/identity"))
                .and(header("authorization", "Bearer initial-token"))
                .respond_with(ResponseTemplate::new(403))
                .with_priority(1)
                .mount(&f.server)
                .await;
        }
        let callback_url = Arc::new(Mutex::new(String::new()));
        let capture = callback_url.clone();
        let result = login_with(
            &f.profile,
            &f.store,
            |auth| async move {
                let query = auth_query(&auth);
                *capture.lock().unwrap() = query["redirect_uri"].clone();
                match failure {
                    "issuer" => {
                        assert_eq!(
                            authorize(&auth, None, Some("https://wrong-issuer.example")).await?,
                            400
                        );
                    }
                    "denied" => {
                        let response = reqwest::Client::new()
                            .get(&query["redirect_uri"])
                            .query(&[
                                ("state", query["state"].as_str()),
                                ("error", "access_denied"),
                            ])
                            .send()
                            .await?;
                        assert_eq!(response.status(), 400);
                    }
                    "browser" => bail!("browser launch failed"),
                    "timeout" => {}
                    _ => {
                        authorize(&auth, None, None).await?;
                    }
                }
                Ok(())
            },
            if failure == "timeout" {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(5)
            },
        )
        .await;
        assert!(result.is_err(), "{failure} unexpectedly succeeded");
        assert_eq!(
            f.store
                .read()?
                .unwrap()
                .token_response
                .unwrap()
                .access_token()
                .secret(),
            "previous-token"
        );
        let callback = callback_url.lock().unwrap().clone();
        assert!(
            reqwest::get(callback).await.is_err(),
            "callback listener remained open"
        );
        if matches!(failure, "issuer" | "denied") {
            assert!(
                !f.server
                    .received_requests()
                    .await
                    .unwrap()
                    .iter()
                    .any(|r| r.url.path() == "/token")
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn active_http_client_refreshes_rotates_and_rejects_account_switch_or_logout() -> Result<()> {
    let f = Fixture::new().await?;
    f.login().await?;
    let tokens = Arc::new(ProviderTokens::new(&f.profile, f.store.clone()));
    let client = f.profile.client()?.with_token_provider(tokens.clone());
    assert_eq!(client.identity().await?.account_id, "alice");
    f.expire().await?;
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .and(body_string_contains("refresh_token=initial-refresh"))
        .respond_with(token_response("refreshed-token", "rotated-refresh"))
        .with_priority(2)
        .expect(1)
        .mount(&f.server)
        .await;
    assert_eq!(client.clone().identity().await?.account_id, "alice");
    let stored = f.store.load().await?.unwrap();
    assert_eq!(
        stored
            .token_response
            .unwrap()
            .refresh_token()
            .unwrap()
            .secret(),
        "rotated-refresh"
    );
    let restarted = ProviderTokens::new(&f.profile, f.store.clone());
    assert_eq!(restarted.access_token().await?, "refreshed-token");
    Mock::given(path("/runtime/identity"))
        .and(header("authorization", "Bearer other-account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"account_id": "bob"})))
        .mount(&f.server)
        .await;
    f.store.write(credentials("other-account"))?;
    assert!(
        client
            .identity()
            .await
            .unwrap_err()
            .to_string()
            .contains("different account")
    );
    f.store.logout(&f.profile).await?;
    assert!(
        client
            .identity()
            .await
            .unwrap_err()
            .to_string()
            .contains("not logged in")
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_refreshes_share_rotated_credentials_and_logout_wins() -> Result<()> {
    let f = Fixture::new().await?;
    f.login().await?;
    f.expire().await?;
    Mock::given(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(
            token_response("refreshed-token", "rotated-refresh")
                .set_delay(Duration::from_millis(150)),
        )
        .expect(1)
        .mount(&f.server)
        .await;
    let first = ProviderTokens::new(&f.profile, f.store.clone());
    let second = ProviderTokens::new(&f.profile, f.store.clone());
    let (a, b) = tokio::join!(first.access_token(), second.access_token());
    assert_eq!(a?, "refreshed-token");
    assert_eq!(b?, "refreshed-token");
    f.expire().await?;
    let refresh = Mock::given(path("/token"))
        .and(body_string_contains("refresh_token=rotated-refresh"))
        .respond_with(
            token_response("refreshed-token", "next-refresh").set_delay(Duration::from_millis(150)),
        )
        .with_priority(1)
        .expect(1)
        .mount_as_scoped(&f.server)
        .await;
    let task = tokio::spawn(async move { first.access_token().await });
    refresh.wait_until_satisfied().await;
    f.store.logout(&f.profile).await?;
    assert_eq!(task.await??, "refreshed-token");
    assert!(f.store.read()?.is_none());
    assert!(second.access_token().await.is_err());
    Ok(())
}

#[tokio::test]
async fn refresh_failures_are_actionable_and_do_not_overwrite_credentials() -> Result<()> {
    for status in [307, 400, 503] {
        let f = Fixture::new().await?;
        f.login().await?;
        f.expire().await?;
        Mock::given(path("/token")).and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(status)
                .insert_header("location", format!("{}/redirected-token", f.server.uri()))
                .set_body_json(json!({"error": if status == 400 { "invalid_grant" } else { "temporarily_unavailable" }})))
            .expect(1).mount(&f.server).await;
        let tokens = ProviderTokens::new(&f.profile, f.store.clone());
        let error = tokens.access_token().await.unwrap_err().to_string();
        if status == 400 {
            assert!(error.contains("run exo provider login"), "{error}");
        } else {
            assert!(!error.contains("login expired"), "{error}");
        }
        assert!(!error.contains("initial-token") && !error.contains("initial-refresh"));
        assert!(
            f.server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .all(|request| request.url.path() != "/redirected-token")
        );
        assert_eq!(
            f.store
                .load()
                .await?
                .unwrap()
                .token_response
                .unwrap()
                .refresh_token()
                .unwrap()
                .secret(),
            "initial-refresh"
        );
    }
    Ok(())
}

#[tokio::test]
async fn discovery_requires_metadata_and_api_keys_do_not_discover_oauth() -> Result<()> {
    let f = Fixture::new().await?;
    assert_eq!(
        f.profile
            .client()?
            .with_bearer_token("api-key".into())
            .identity()
            .await?
            .account_id,
        "alice"
    );
    assert!(
        f.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .all(|r| r.url.path() == "/runtime/identity")
    );
    f.server.reset().await;
    Mock::given(path("/runtime/identity"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&f.server)
        .await;
    let error = manager(&f.profile)
        .await
        .err()
        .context("expected discovery failure")?;
    assert!(
        error.to_string().contains("configure an API key"),
        "{error:#}"
    );
    f.server.reset().await;
    let error = manager(&f.profile)
        .await
        .err()
        .context("expected wrong-endpoint failure")?;
    let message = error.to_string();
    assert!(
        message.contains(&format!("{}/runtime/identity", f.server.uri())),
        "{message}"
    );
    assert!(
        message.contains("404") && message.contains("--url"),
        "{message}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the native OS credential store"]
async fn native_keychain_roundtrip() -> Result<()> {
    let f = Fixture::new().await?;
    let store = KeychainStore::new(&f.profile, f._temp.path())?;
    store.write(credentials("exo-isolated-keychain-test"))?;
    let reopened = KeychainStore::new(&f.profile, f._temp.path())?;
    let result = reopened.read();
    store.logout(&f.profile).await?;
    assert_eq!(
        result?
            .unwrap()
            .token_response
            .unwrap()
            .access_token()
            .secret(),
        "exo-isolated-keychain-test"
    );
    assert!(reopened.read()?.is_none());
    Ok(())
}

#[tokio::test]
async fn browser_login_allows_reads_and_rejects_concurrent_credential_changes() -> Result<()> {
    for action in ["unchanged", "logout", "rotation"] {
        let f = Fixture::new().await?;
        f.login().await?;
        let tokens = ProviderTokens::new(&f.profile, f.store.clone());
        let store = f.store.clone();
        let profile = f.profile.clone();
        let result = login_with(
            &f.profile,
            &f.store,
            |auth| async move {
                assert_eq!(
                    tokio::time::timeout(Duration::from_secs(5), tokens.access_token()).await??,
                    "initial-token"
                );
                match action {
                    "logout" => store.logout(&profile).await?,
                    "rotation" => store.write(credentials("concurrent-token"))?,
                    _ => {}
                }
                authorize(&auth, None, None).await?;
                Ok(())
            },
            Duration::from_secs(10),
        )
        .await;
        if action == "unchanged" {
            assert_eq!(result?, "alice");
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("credentials changed during login")
            );
            if action == "logout" {
                assert!(f.store.read()?.is_none());
            } else {
                assert_eq!(
                    f.store
                        .read()?
                        .unwrap()
                        .token_response
                        .unwrap()
                        .access_token()
                        .secret(),
                    "concurrent-token"
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn unusable_tokens_fail_before_an_authenticated_request() -> Result<()> {
    let f = Fixture::new().await?;
    f.login().await?;
    let tokens = Arc::new(ProviderTokens::new(&f.profile, f.store.clone()));
    assert_eq!(tokens.access_token().await?, "initial-token");
    let client = f.profile.client()?.with_token_provider(tokens);
    let requests = f.server.received_requests().await.unwrap().len();
    for (token, error) in [
        ("expired-token", "run exo provider login"),
        (" ", "provider credential is empty"),
        ("sensitive-token\r\nInjected: yes", "sending HTTP request"),
    ] {
        let mut stored = credentials(token);
        if token == "expired-token" {
            stored.token_received_at = Some(0);
            stored
                .token_response
                .as_mut()
                .unwrap()
                .set_expires_in(Some(&Duration::from_secs(1)));
        }
        f.store.write(stored)?;
        let actual = client.identity().await.unwrap_err().to_string();
        assert!(actual.contains(error), "{actual}");
        assert!(!actual.contains("sensitive-token"));
        assert_eq!(f.server.received_requests().await.unwrap().len(), requests);
    }
    Ok(())
}

#[tokio::test]
async fn discovery_rejects_mismatched_metadata_and_cross_origin_redirects() -> Result<()> {
    for failure in [
        "resource",
        "issuer",
        "identity_redirect",
        "metadata_redirect",
    ] {
        let f = Fixture::new().await?;
        let other = MockServer::start().await;
        let (route, response) = match failure {
            "resource" => ("/metadata/runtime", ResponseTemplate::new(200).set_body_json(json!({
                "resource": other.uri(), "authorization_servers": [f.server.uri()]
            }))),
            "issuer" => ("/.well-known/oauth-authorization-server", ResponseTemplate::new(200).set_body_json(json!({
                "issuer": other.uri(), "authorization_endpoint": format!("{}/authorize", f.server.uri()),
                "token_endpoint": format!("{}/token", f.server.uri())
            }))),
            "identity_redirect" => ("/runtime/identity", ResponseTemplate::new(302).insert_header("location", other.uri())),
            _ => ("/metadata/runtime", ResponseTemplate::new(302).insert_header("location", other.uri())),
        };
        Mock::given(path(route))
            .respond_with(response)
            .with_priority(1)
            .mount(&f.server)
            .await;
        assert!(manager(&f.profile).await.is_err(), "{failure} was accepted");
        assert!(
            other.received_requests().await.unwrap().is_empty(),
            "{failure} redirected"
        );
    }
    Ok(())
}

#[tokio::test]
async fn logout_revokes_before_removing_credentials_and_preserves_them_on_failure() -> Result<()> {
    for status in [200, 503] {
        let f = Fixture::new().await?;
        f.login().await?;
        let base = f.server.uri();
        Mock::given(path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": base, "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"), "revocation_endpoint": format!("{base}/revoke"),
                "response_types_supported": ["code"], "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"]
            }))).with_priority(1).mount(&f.server).await;
        Mock::given(method("POST"))
            .and(path("/revoke"))
            .and(body_string_contains("token=initial-refresh"))
            .and(body_string_contains("client_id=exo-test"))
            .and(body_string_contains("token_type_hint=refresh_token"))
            .respond_with(ResponseTemplate::new(status))
            .expect(1)
            .mount(&f.server)
            .await;
        let result = f.store.logout(&f.profile).await;
        assert_eq!(result.is_ok(), status == 200);
        assert_eq!(f.store.read()?.is_none(), status == 200);
    }
    Ok(())
}
