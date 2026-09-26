use super::{
    AuthConfig, AuthServer,
    oidc::{OidcConfig, VaultSecret},
};
use anyhow::{Context, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use exoharness::{
    BasicExoHarness, ExoHarness, NewAgentRequest, PutSecretRequest, ResourceScope, Secret,
    vault::VaultContext,
};
use openidconnect::{
    Audience, EmptyAdditionalClaims, EndUserEmail, IssuerUrl, JsonWebKeyId, Nonce,
    OAuth2TokenResponse, PrivateSigningKey, StandardClaims, SubjectIdentifier,
    core::{
        CoreIdToken, CoreIdTokenClaims, CoreJsonWebKeySet, CoreJwsSigningAlgorithm,
        CoreRsaPrivateSigningKey,
    },
};
use rmcp::transport::auth::{
    AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    InMemoryCredentialStore,
};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, rand_core::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{net::TcpListener, sync::Arc};
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Serialize, Deserialize)]
struct IdentityCode {
    nonce: String,
    email: String,
    subject: String,
    verified: bool,
}
#[derive(Deserialize)]
struct OidcAuthorization {
    nonce: String,
    state: String,
    redirect_uri: Url,
}
#[derive(Deserialize)]
struct CodeForm {
    code: String,
}
#[derive(Deserialize)]
struct CliCallback {
    code: String,
    state: String,
    iss: String,
}

struct Fixture {
    _temp: tempfile::TempDir,
    _oidc: MockServer,
    base: String,
    auth: Arc<AuthServer>,
    harness: Arc<BasicExoHarness>,
    runtime: Arc<crate::Runtime>,
    browser: reqwest::Client,
    server: actix_web::dev::ServerHandle,
}
impl Fixture {
    async fn new(multiplayer: bool) -> Result<Self> {
        let oidc = MockServer::start().await;
        let issuer = oidc.uri();
        let pem = RsaPrivateKey::new(&mut OsRng, 2048)?.to_pkcs1_pem(Default::default())?;
        let key = Arc::new(
            CoreRsaPrivateSigningKey::from_pem(&pem, Some(JsonWebKeyId::new("test".into())))
                .map_err(anyhow::Error::msg)?,
        );
        Mock::given(path("/.well-known/openid-configuration")).respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "issuer": issuer, "authorization_endpoint": format!("{issuer}/authorize"), "token_endpoint": format!("{issuer}/token"), "jwks_uri": format!("{issuer}/jwks"),
            "response_types_supported": ["code"], "subject_types_supported": ["public"], "id_token_signing_alg_values_supported": ["RS256"]
        }))).mount(&oidc).await;
        Mock::given(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(CoreJsonWebKeySet::new(vec![key.as_verification_key()])),
            )
            .mount(&oidc)
            .await;
        Mock::given(method("POST")).and(path("/token")).respond_with(move |request: &wiremock::Request| {
            let form: CodeForm = actix_web::web::Query::<CodeForm>::from_query(std::str::from_utf8(&request.body).unwrap()).unwrap().into_inner();
            let identity: IdentityCode = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(form.code).unwrap()).unwrap();
            let claims = CoreIdTokenClaims::new(
                IssuerUrl::new(issuer.clone()).unwrap(), vec![Audience::new("google-app".into())],
                chrono::Utc::now() + chrono::Duration::minutes(5), chrono::Utc::now(),
                StandardClaims::new(SubjectIdentifier::new(identity.subject))
                    .set_email(Some(EndUserEmail::new(identity.email))).set_email_verified(Some(identity.verified)),
                EmptyAdditionalClaims {},
            ).set_nonce(Some(Nonce::new(identity.nonce)));
            let token = CoreIdToken::new(claims, key.as_ref(), CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256, None, None).unwrap();
            ResponseTemplate::new(200).set_body_json(json!({"access_token": "google-access", "token_type": "Bearer", "expires_in": 300, "id_token": token.to_string()}))
        }).mount(&oidc).await;
        let temp = tempfile::TempDir::new()?;
        let config = crate::test_support::local_test_config(temp.path().join("state"));
        let harness = Arc::new(BasicExoHarness::new(config.clone()).await?);
        let vault = harness.create_vault("server-auth").await?;
        vault
            .put_secret(PutSecretRequest {
                name: "google".into(),
                policy: None,
                secret: Secret::Key {
                    value: "client-secret".into(),
                },
            })
            .await?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let base = format!("http://{}", listener.local_addr()?);
        let auth = Arc::new(
            AuthServer::new(
                AuthConfig {
                    public_url: Url::parse(&base)?,
                    oidc: OidcConfig {
                        issuer: oidc.uri(),
                        client_id: "google-app".into(),
                        client_secret: VaultSecret {
                            vault: "server-auth".into(),
                            secret: "google".into(),
                        },
                    },
                    owner_email: "alice@example.com".into(),
                    allowed_emails: vec!["bob@example.com".into()],
                    shared_vaults: vec![],
                },
                harness.clone(),
            )
            .await?,
        );
        auth.adopt_local_state(harness.as_ref()).await?;
        let runtime = Arc::new(crate::Runtime::new(
            crate::LocalProvider::managed(
                harness.clone(),
                config,
                Default::default(),
                Arc::new(cost::PricingTable::empty()),
            )?,
            None,
        ));
        let service = crate::http_service::RuntimeHttpService::new(runtime.clone(), None)?
            .with_auth(auth.clone(), multiplayer);
        let server = crate::http_service::server(listener, Arc::new(service))?;
        let handle = server.handle();
        tokio::spawn(server);
        Ok(Self {
            _temp: temp,
            _oidc: oidc,
            base,
            auth,
            harness,
            runtime,
            browser: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            server: handle,
        })
    }

    async fn restart(&mut self, auth_config: AuthConfig, multiplayer: bool) -> Result<()> {
        self.server.stop(false).await;
        let config = crate::test_support::local_test_config(self._temp.path().join("state"));
        self.harness = Arc::new(BasicExoHarness::new(config.clone()).await?);
        self.auth = Arc::new(AuthServer::new(auth_config, self.harness.clone()).await?);
        self.auth.adopt_local_state(self.harness.as_ref()).await?;
        self.runtime = Arc::new(crate::Runtime::new(
            crate::LocalProvider::managed(
                self.harness.clone(),
                config,
                Default::default(),
                Arc::new(cost::PricingTable::empty()),
            )?,
            None,
        ));
        let listener = TcpListener::bind(Url::parse(&self.base)?.socket_addrs(|| None)?[0])?;
        let service = crate::http_service::RuntimeHttpService::new(self.runtime.clone(), None)?
            .with_auth(self.auth.clone(), multiplayer);
        let server = crate::http_service::server(listener, Arc::new(service))?;
        self.server = server.handle();
        tokio::spawn(server);
        Ok(())
    }

    async fn session(&self) -> Result<(AuthorizationSession, InMemoryCredentialStore)> {
        let endpoint = format!("{}/exo", self.base);
        let mut manager = AuthorizationManager::new(&endpoint).await?;
        let response = self
            .browser
            .get(format!("{endpoint}/identity"))
            .send()
            .await?;
        assert_eq!(response.status(), 401);
        let challenge = response
            .headers()
            .get("www-authenticate")
            .context("missing auth challenge")?
            .to_str()?;
        let metadata = manager
            .resolve_metadata_from_challenge(Some(challenge))
            .await?;
        assert!(metadata.source.is_discovered());
        manager.set_metadata(metadata.metadata);
        let credentials = InMemoryCredentialStore::new();
        manager.set_credential_store(credentials.clone());
        let session = AuthorizationSession::new(
            manager,
            AuthorizationRequest::new("http://127.0.0.1:19870/callback")
                .with_client_name("Exo test")
                .with_application_type("native"),
        )
        .await
        .map_err(|(_, e)| e)?;
        Ok((session, credentials))
    }

    async fn browser_login(
        &self,
        authorization: &str,
        email: &str,
        subject: &str,
        verified: bool,
    ) -> Result<reqwest::Response> {
        let response = self.browser.get(authorization).send().await?;
        assert_eq!(response.status(), 303, "{}", response.text().await?);
        let cookie = response
            .headers()
            .get("set-cookie")
            .context("missing browser binding")?
            .to_str()?
            .split(';')
            .next()
            .unwrap()
            .to_string();
        let location = Url::parse(
            response
                .headers()
                .get("location")
                .context("missing OIDC redirect")?
                .to_str()?,
        )?;
        let query: OidcAuthorization =
            actix_web::web::Query::<OidcAuthorization>::from_query(location.query().unwrap())?
                .into_inner();
        let code = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&IdentityCode {
            nonce: query.nonce,
            email: email.into(),
            subject: subject.into(),
            verified,
        })?);
        Ok(self
            .browser
            .get(query.redirect_uri)
            .query(&[("state", query.state), ("code", code)])
            .header("cookie", cookie)
            .send()
            .await?)
    }

    async fn login(
        &self,
        email: &str,
        subject: &str,
    ) -> Result<rmcp::transport::auth::StoredCredentials> {
        let (session, credentials) = self.session().await?;
        let response = self
            .browser_login(session.get_authorization_url(), email, subject, true)
            .await?;
        assert_eq!(response.status(), 303, "{}", response.text().await?);
        let callback = Url::parse(
            response
                .headers()
                .get("location")
                .context("missing CLI redirect")?
                .to_str()?,
        )?;
        let query: CliCallback =
            actix_web::web::Query::<CliCallback>::from_query(callback.query().unwrap())?
                .into_inner();
        session
            .handle_callback_with_issuer(&query.code, &query.state, Some(&query.iss))
            .await?;
        credentials.load().await?.context("credentials missing")
    }

    fn client(
        &self,
        credentials: &rmcp::transport::auth::StoredCredentials,
    ) -> Result<exo_managed_agents::http::RuntimeClient> {
        Ok(
            exo_managed_agents::http::RuntimeClient::new(&format!("{}/exo", self.base))?
                .with_bearer_token(
                    credentials
                        .token_response
                        .as_ref()
                        .unwrap()
                        .access_token()
                        .secret()
                        .clone(),
                ),
        )
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let handle = self.server.clone();
        tokio::spawn(async move {
            handle.stop(false).await;
        });
    }
}

#[tokio::test]
async fn rmcp_login_private_vaults_and_session_revocation() -> Result<()> {
    let f = Fixture::new(false).await?;
    let alice = f.login("alice@example.com", "alice-sub").await?;
    let alice_client = f.client(&alice)?;
    let account = alice_client.identity().await?.account_id;
    let again = f.login("alice@example.com", "alice-sub").await?;
    assert_eq!(f.client(&again)?.identity().await?.account_id, account);
    let bob = f.login("bob@example.com", "bob-sub").await?;
    let bob_client = f.client(&bob)?;
    assert_ne!(bob_client.identity().await?.account_id, account);
    let local = f
        .harness
        .new_agent(NewAgentRequest {
            slug: "local-owner".into(),
            name: "Local owner".into(),
            vaults: vec![],
        })
        .await?;
    assert!(alice_client.get_agent(local.record().id).await?.is_some());
    assert!(bob_client.get_agent(local.record().id).await?.is_none());
    let alice_vaults = alice_client.list_vaults(ResourceScope::Global).await?;
    let bob_vaults = bob_client.list_vaults(ResourceScope::Global).await?;
    assert_eq!(bob_vaults.len(), 1);
    assert_eq!(bob_vaults[0].name, "personal");
    assert!(
        alice_vaults
            .iter()
            .all(|v| v.id != f.auth.auth_vault() && v.id != bob_vaults[0].id)
    );
    let private = alice_client.create_vault("test").await?;
    let other = bob_client.create_vault("test").await?;
    assert_ne!(private.id, other.id);
    let secret = alice_client
        .put_secret(
            ResourceScope::Global,
            private.id,
            &PutSecretRequest {
                name: "key".into(),
                secret: Secret::Key {
                    value: "private".into(),
                },
                policy: None,
            },
        )
        .await?;
    assert!(
        bob_client
            .list_secrets(ResourceScope::Global, private.id)
            .await
            .is_err()
    );
    assert!(
        bob_client
            .delete_secret(ResourceScope::Global, private.id, secret)
            .await
            .is_err()
    );
    assert!(
        alice_client
            .put_secret(
                ResourceScope::Global,
                f.auth.auth_vault(),
                &PutSecretRequest {
                    name: "attack".into(),
                    secret: Secret::Key {
                        value: "bad".into()
                    },
                    policy: None
                }
            )
            .await
            .is_err()
    );
    let agent = alice_client
        .create_agent(&NewAgentRequest {
            slug: "private".into(),
            name: "Private".into(),
            vaults: vec![private.id],
        })
        .await?;
    assert!(bob_client.get_agent(agent.id).await?.is_none());
    assert!(bob_client.delete_agent(agent.id).await.is_err());
    assert!(
        bob_client
            .create_agent(&NewAgentRequest {
                slug: "attack".into(),
                name: "Attack".into(),
                vaults: vec![f.auth.auth_vault()]
            })
            .await
            .is_err()
    );
    let tokens = alice.token_response.as_ref().unwrap();
    f.browser
        .post(format!("{}/exo/auth/revoke", f.base))
        .form(&[
            ("client_id", alice.client_id.as_str()),
            ("token", tokens.refresh_token().unwrap().secret().as_str()),
        ])
        .send()
        .await?
        .error_for_status()?;
    assert!(alice_client.identity().await.is_err());
    assert!(f.client(&again)?.identity().await.is_ok());
    Ok(())
}

#[tokio::test]
async fn rejected_identities_create_no_vaults_or_accounts() -> Result<()> {
    let f = Fixture::new(false).await?;
    let before = f.harness.list_vaults().await?.len();
    for (email, verified) in [("mallory@example.com", true), ("alice@example.com", false)] {
        let (session, _) = f.session().await?;
        let response = f
            .browser_login(session.get_authorization_url(), email, "unknown", verified)
            .await?;
        assert_eq!(response.status(), 400);
        assert!(f.auth.store.state.lock().await.principals.is_empty());
        assert_eq!(f.harness.list_vaults().await?.len(), before);
    }
    f.login("alice@example.com", "original").await?;
    let count = f.harness.list_vaults().await?.len();
    let (session, _) = f.session().await?;
    assert_eq!(
        f.browser_login(
            session.get_authorization_url(),
            "alice@example.com",
            "different-sub",
            true
        )
        .await?
        .status(),
        400
    );
    assert_eq!(f.harness.list_vaults().await?.len(), count);
    Ok(())
}

#[derive(Deserialize)]
struct Registration {
    client_id: String,
}
#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
}

async fn manual_grant(f: &Fixture) -> Result<(Registration, CliCallback, String)> {
    let client: Registration = f.browser.post(format!("{}/exo/auth/register", f.base)).json(&json!({
        "redirect_uris": ["http://127.0.0.1:19870/callback"], "token_endpoint_auth_method": "none"
    })).send().await?.error_for_status()?.json().await?;
    let (challenge, verifier) = openidconnect::PkceCodeChallenge::new_random_sha256();
    let mut url = Url::parse(&format!("{}/exo/auth/authorize", f.base))?;
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", client.client_id.as_str()),
        ("redirect_uri", "http://127.0.0.1:19870/callback"),
        ("state", "test-state"),
        ("resource", &format!("{}/exo", f.base)),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]);
    let response = f
        .browser_login(url.as_str(), "alice@example.com", "alice-sub", true)
        .await?;
    assert_eq!(response.status(), 303);
    let redirect = Url::parse(response.headers().get("location").unwrap().to_str()?)?;
    let callback =
        actix_web::web::Query::<CliCallback>::from_query(redirect.query().unwrap())?.into_inner();
    Ok((client, callback, verifier.secret().clone()))
}

#[tokio::test]
async fn codes_bind_pkce_client_redirect_and_resource_and_cannot_be_replayed() -> Result<()> {
    let f = Fixture::new(false).await?;
    for altered in [
        "code_verifier",
        "client_id",
        "redirect_uri",
        "resource",
        "none",
    ] {
        let (client, callback, verifier) = manual_grant(&f).await?;
        let mut form = std::collections::BTreeMap::from([
            ("grant_type", "authorization_code".into()),
            ("code", callback.code),
            ("client_id", client.client_id),
            ("redirect_uri", "http://127.0.0.1:19870/callback".into()),
            ("resource", format!("{}/exo", f.base)),
            ("code_verifier", verifier),
        ]);
        if altered != "none" {
            form.insert(altered, "a".repeat(43));
        }
        let endpoint = format!("{}/exo/auth/token", f.base);
        let response = f.browser.post(&endpoint).form(&form).send().await?;
        assert_eq!(
            response.status().as_u16(),
            if altered == "none" { 200 } else { 400 },
            "case {altered}"
        );
        assert_eq!(
            f.browser.post(&endpoint).form(&form).send().await?.status(),
            400
        );
    }
    assert_eq!(
        f.browser
            .post(format!("{}/exo/auth/register", f.base))
            .json(&json!({"redirect_uris": ["https://attacker.example/callback"]}))
            .send()
            .await?
            .status(),
        400
    );
    assert_eq!(
        f.browser
            .post(format!("{}/exo/auth/register", f.base))
            .json(&json!({"redirect_uris": ["http://127.0.0.1.evil.example/callback"]}))
            .send()
            .await?
            .status(),
        400
    );
    Ok(())
}

#[tokio::test]
async fn refresh_survives_restart_rotates_and_honors_revocation() -> Result<()> {
    let mut f = Fixture::new(false).await?;
    let credentials = f.login("alice@example.com", "alice-sub").await?;
    let identity = f.client(&credentials)?.identity().await?.account_id;
    let token = credentials.token_response.as_ref().unwrap();
    let refresh = token.refresh_token().unwrap().secret().clone();
    f.restart(f.auth.config.clone(), false).await?;
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
        ("client_id", &credentials.client_id),
        ("resource", &format!("{}/exo", f.base)),
    ];
    let endpoint = format!("{}/exo/auth/token", f.base);
    let response = f.browser.post(&endpoint).form(&form).send().await?;
    assert_eq!(response.status(), 200, "{}", response.text().await?);
    let tokens: Tokens = response.json().await?;
    assert_ne!(tokens.refresh_token, refresh);
    assert_eq!(
        f.browser.post(&endpoint).form(&form).send().await?.status(),
        400
    );
    let identity_response = f
        .browser
        .get(format!("{}/exo/identity", f.base))
        .bearer_auth(&tokens.access_token)
        .send()
        .await?;
    let found: exo_managed_agents::http::protocol::ProviderIdentity =
        identity_response.error_for_status()?.json().await?;
    assert_eq!(found.account_id, identity);
    assert!(f.client(&credentials)?.identity().await.is_err());
    f.browser
        .post(format!("{}/exo/auth/revoke", f.base))
        .form(&[
            ("token", tokens.refresh_token.as_str()),
            ("client_id", credentials.client_id.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(
        f.browser
            .get(format!("{}/exo/identity", f.base))
            .bearer_auth(&tokens.access_token)
            .send()
            .await?
            .status(),
        401
    );
    Ok(())
}

#[tokio::test]
async fn multiplayer_shares_history_but_keeps_vaults_models_and_approvals_private() -> Result<()> {
    let f = Fixture::new(true).await?;
    let alice = f.login("alice@example.com", "alice-sub").await?;
    let bob = f.login("bob@example.com", "bob-sub").await?;
    let ac = f.client(&alice)?;
    let bc = f.client(&bob)?;
    let aid = ac.identity().await?.account_id;
    let bid = bc.identity().await?.account_id;
    let ar = f.runtime.with_caller(f.auth.caller(aid.clone(), true))?;
    let br = f.runtime.with_caller(f.auth.caller(bid.clone(), true))?;
    let a = ar.exoharness_handle();
    let b = br.exoharness_handle();
    let av = a
        .list_vaults()
        .await?
        .into_iter()
        .find(|v| v.record().name == "personal")
        .unwrap();
    let bv = b
        .list_vaults()
        .await?
        .into_iter()
        .find(|v| v.record().name == "personal")
        .unwrap();
    let agent = a
        .new_agent(NewAgentRequest {
            slug: "shared".into(),
            name: "Shared".into(),
            vaults: vec![av.record().id],
        })
        .await?;
    let thread = agent.new_thread(Default::default()).await?;
    assert_eq!(bc.list_agents(None).await?.agents.len(), 1);
    let shared = b.get_agent(&agent.record().id).await?.unwrap();
    let bob_thread = shared.get_thread(&thread.record().id).await?.unwrap();
    assert_eq!(
        bob_thread
            .list_vaults()
            .await?
            .iter()
            .map(|v| v.record().id)
            .collect::<Vec<_>>(),
        vec![bv.record().id]
    );
    assert!(
        bob_thread
            .attach_vaults(vec![av.record().id])
            .await
            .is_err()
    );
    assert!(
        thread
            .attach_vaults(vec![f.auth.auth_vault()])
            .await
            .is_err()
    );
    let turn = thread
        .begin_turn(exoharness::BeginTurnRequest {
            session_id: None,
            input: vec![],
        })
        .await?;
    assert_eq!(
        crate::permissions::turn_caller(bob_thread.as_ref(), turn.record().id).await?,
        Some(aid)
    );
    let before = bob_thread.get_events(None).await?.events.len();
    assert!(before > 0);
    // A decision cannot be written through another caller's Runtime, even with all IDs.
    let body = exo_managed_agents::http::protocol::ApprovalResponseBody {
        session_id: turn.record().session_id,
        approval_id: "missing".into(),
        approved: true,
        allow_for_tool: false,
    };
    let error = br
        .approval_response(
            agent.record().id,
            thread.record().id,
            turn.record().id,
            &body,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("only the caller"), "{error}");
    assert_eq!(bob_thread.get_events(None).await?.events.len(), before);
    let key = av
        .put_secret(PutSecretRequest {
            name: "openai".into(),
            secret: Secret::Key {
                value: "alice-key".into(),
            },
            policy: None,
        })
        .await?;
    assert!(
        bc.delete_secret(ResourceScope::Global, av.record().id, key)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn browser_login_requires_its_cookie_and_mutations_require_csrf() -> Result<()> {
    let f = Fixture::new(false).await?;
    let response = f
        .browser
        .get(format!("{}/exo/auth/login", f.base))
        .send()
        .await?;
    let location = Url::parse(response.headers().get("location").unwrap().to_str()?)?;
    let query = actix_web::web::Query::<OidcAuthorization>::from_query(location.query().unwrap())?
        .into_inner();
    let code = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&IdentityCode {
        nonce: query.nonce,
        email: "alice@example.com".into(),
        subject: "alice-sub".into(),
        verified: true,
    })?);
    let url = query.redirect_uri;
    let request = [("state", query.state), ("code", code)];
    assert_eq!(
        f.browser
            .get(url.clone())
            .query(&request)
            .send()
            .await?
            .status(),
        400
    );
    assert!(f.auth.store.state.lock().await.principals.is_empty());
    let login_cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()?
        .split(';')
        .next()
        .unwrap();
    let completed = f
        .browser
        .get(url)
        .query(&request)
        .header("cookie", login_cookie)
        .send()
        .await?;
    assert_eq!(completed.status(), 200);
    let cookie = completed
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("exo-session="))
        .unwrap();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"));
    let cookie = cookie.split(';').next().unwrap();
    let endpoint = format!("{}/exo/vault", f.base);
    assert_eq!(
        f.browser
            .post(&endpoint)
            .header("cookie", cookie)
            .json(&json!({"name":"no-csrf"}))
            .send()
            .await?
            .status(),
        401
    );
    assert_eq!(
        f.browser
            .post(&endpoint)
            .header("cookie", cookie)
            .header("Origin", "https://attacker.example")
            .header("X-Exo-CSRF", "1")
            .json(&json!({"name":"wrong-origin"}))
            .send()
            .await?
            .status(),
        401
    );
    f.browser
        .post(&endpoint)
        .header("cookie", cookie)
        .header("Origin", &f.base)
        .header("X-Exo-CSRF", "1")
        .json(&json!({"name":"allowed"}))
        .send()
        .await?
        .error_for_status()?;
    f.browser
        .post(format!("{}/exo/auth/logout", f.base))
        .header("cookie", cookie)
        .header("Origin", &f.base)
        .header("X-Exo-CSRF", "1")
        .send()
        .await?
        .error_for_status()?;
    assert_eq!(
        f.browser
            .get(format!("{}/exo/identity", f.base))
            .header("cookie", cookie)
            .send()
            .await?
            .status(),
        401
    );
    Ok(())
}

#[tokio::test]
async fn event_stream_survives_token_rotation_but_closes_on_revocation() -> Result<()> {
    use futures::StreamExt;
    let f = Fixture::new(false).await?;
    let credentials = f.login("alice@example.com", "alice-sub").await?;
    let client = f.client(&credentials)?;
    let principal = client.identity().await?.account_id;
    let runtime = f.runtime.with_caller(f.auth.caller(principal, false))?;
    let agent = runtime
        .exoharness_handle()
        .new_agent(NewAgentRequest {
            slug: "events".into(),
            name: "Events".into(),
            vaults: vec![],
        })
        .await?;
    let thread = agent.new_thread(Default::default()).await?;
    let mut events = client
        .watch(agent.record().id, thread.record().id, &Default::default())
        .await?;
    let tokens: Tokens = f
        .browser
        .post(format!("{}/exo/auth/token", f.base))
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", credentials.client_id.as_str()),
            ("resource", &format!("{}/exo", f.base)),
            (
                "refresh_token",
                credentials
                    .token_response
                    .as_ref()
                    .unwrap()
                    .refresh_token()
                    .unwrap()
                    .secret()
                    .as_str(),
            ),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let turn = thread.begin_turn(Default::default()).await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = events
                .next()
                .await
                .context("rotation closed the stream")??;
            if event.turn_id == Some(turn.record().id) {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    f.browser
        .post(format!("{}/exo/auth/revoke", f.base))
        .form(&[
            ("token", tokens.refresh_token.as_str()),
            ("client_id", credentials.client_id.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?;
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        while let Some(event) = events.next().await {
            event?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn switching_callers_stops_old_sandboxes_and_does_not_reuse_them() -> Result<()> {
    let f = Fixture::new(true).await?;
    let alice = f.login("alice@example.com", "alice-sub").await?;
    let bob = f.login("bob@example.com", "bob-sub").await?;
    let ar = f.runtime.with_caller(
        f.auth
            .caller(f.client(&alice)?.identity().await?.account_id, true),
    )?;
    let br = f.runtime.with_caller(
        f.auth
            .caller(f.client(&bob)?.identity().await?.account_id, true),
    )?;
    let agent = ar
        .exoharness_handle()
        .new_agent(NewAgentRequest {
            slug: "sandbox".into(),
            name: "Sandbox".into(),
            vaults: vec![],
        })
        .await?;
    let a = agent.new_thread(Default::default()).await?;
    let b = br
        .exoharness_handle()
        .get_agent(&agent.record().id)
        .await?
        .unwrap()
        .get_thread(&a.record().id)
        .await?
        .unwrap();
    assert!(!a.activate_caller().await?);
    let request = exoharness::CreateSandboxRequest {
        provider: exoharness::SandboxProvider::LocalProcess,
        name: Some("warm".into()),
        image: "local".into(),

        resources: None,
        default_workdir: None,
        file_system_mounts: None,
        durable_file_systems: None,
        policy: None,
        enable_networking: None,
        idle_seconds: None,
    };
    let first = a.create_sandbox(request.clone()).await?;
    assert!(b.list_sandboxes().await?.is_empty());
    assert!(b.activate_caller().await?);
    assert!(a.list_sandboxes().await?.iter().all(|s| !s.running));
    let second = b.create_sandbox(request).await?;
    assert_ne!(first, second);
    assert!(a.activate_caller().await?);
    assert!(b.list_sandboxes().await?.iter().all(|s| !s.running));
    Ok(())
}

#[tokio::test]
async fn shared_vaults_require_attachment_and_keep_writes_with_the_owner() -> Result<()> {
    let mut f = Fixture::new(false).await?;
    let alice = f.login("alice@example.com", "alice-sub").await?;
    let bob = f.login("bob@example.com", "bob-sub").await?;
    let team = f.harness.create_vault("team").await?;
    let mut config = f.auth.config.clone();
    config.shared_vaults = vec!["team".into()];
    f.restart(config, false).await?;
    let ac = f.client(&alice)?;
    let bc = f.client(&bob)?;
    let runtime = f
        .runtime
        .with_caller(f.auth.caller(bc.identity().await?.account_id, false))?;
    let agent = runtime
        .exoharness_handle()
        .new_agent(NewAgentRequest {
            slug: "bob".into(),
            name: "Bob".into(),
            vaults: vec![],
        })
        .await?;
    let thread = agent.new_thread(Default::default()).await?;
    assert!(thread.get_vault(&team.record().id).await?.is_none());
    let thread = thread.attach_vaults(vec![team.record().id]).await?;
    let vault = thread.get_vault(&team.record().id).await?.unwrap();
    let secret = ac
        .put_secret(
            ResourceScope::Global,
            team.record().id,
            &PutSecretRequest {
                name: "key".into(),
                policy: Some(
                    (exoharness::vault::CredentialDestination::origin("https://api.openai.com")?)
                        .into(),
                ),
                secret: Secret::Key {
                    value: "first".into(),
                },
            },
        )
        .await?;
    let scope = ResourceScope::Thread {
        agent_id: agent.record().id,
        thread_id: thread.record().id,
    };
    assert_eq!(bc.list_secrets(scope, team.record().id).await?.len(), 1);
    assert!(
        bc.update_secret(
            scope,
            team.record().id,
            secret,
            &exoharness::UpdateSecretRequest {
                secret: None,
                policy: Some(
                    (exoharness::vault::CredentialDestination::origin("https://example.com")?)
                        .into()
                ),
            }
        )
        .await
        .is_err()
    );
    assert!(bc.delete_vault(team.record().id).await.is_err());
    ac.update_secret(
        ResourceScope::Global,
        team.record().id,
        secret,
        &Secret::Key {
            value: "rotated".into(),
        }
        .into(),
    )
    .await?;
    assert!(vault.get_secret(&secret).await.is_err());
    let definition = exo_managed_agents::AgentDefinition::parse("---\nname: shared\nharness: basic\nconfig:\n  model: gpt-5-mini\n  credential: key\n---\nHelp.\n".into())?;
    let mut model = crate::managed_agents::agent_config(
        &definition,
        exoharness::SandboxProvider::LocalProcess,
        None,
        None,
    )?;
    let resolved = crate::harness_helpers::resolve_model(thread.as_ref(), &model).await?;
    assert_eq!(resolved.api_key.as_deref(), Some("rotated"));
    model.base_url = Some("https://attacker.example".into());
    assert!(
        crate::harness_helpers::resolve_model(thread.as_ref(), &model)
            .await
            .is_err()
    );
    model.base_url = None;
    assert!(ac.delete_vault(team.record().id).await.is_err());
    ac.delete_secret(ResourceScope::Global, team.record().id, secret)
        .await?;
    assert!(
        crate::harness_helpers::resolve_model(thread.as_ref(), &model)
            .await
            .is_err()
    );
    let disposable = bc.create_vault("scratch").await?;
    bc.delete_vault(disposable.id).await?;
    assert_ne!(bc.create_vault("scratch").await?.id, disposable.id);
    let mut config = f.auth.config.clone();
    config.shared_vaults.clear();
    f.restart(config, false).await?;
    assert!(bc.list_secrets(scope, team.record().id).await.is_err());
    Ok(())
}

#[tokio::test]
async fn expiration_and_removed_admission_reject_saved_credentials() -> Result<()> {
    let mut f = Fixture::new(false).await?;
    let credentials = f.login("bob@example.com", "bob-sub").await?;
    let client = f.client(&credentials)?;
    let form = [
        ("grant_type", "refresh_token"),
        (
            "refresh_token",
            credentials
                .token_response
                .as_ref()
                .unwrap()
                .refresh_token()
                .unwrap()
                .secret()
                .as_str(),
        ),
        ("client_id", credentials.client_id.as_str()),
        ("resource", &format!("{}/exo", f.base)),
    ];
    let expired = chrono::Utc::now().timestamp() - 1;
    for session in f.auth.store.state.lock().await.sessions.values_mut() {
        session.access_expires = expired;
    }
    assert!(client.identity().await.is_err());
    let response = f
        .browser
        .post(format!("{}/exo/auth/token", f.base))
        .form(&form)
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    let tokens: Tokens = response.json().await?;
    for session in f.auth.store.state.lock().await.sessions.values_mut() {
        session.expires = expired;
    }
    let mut form = form;
    form[1].1 = &tokens.refresh_token;
    assert_eq!(
        f.browser
            .post(format!("{}/exo/auth/token", f.base))
            .form(&form)
            .send()
            .await?
            .status(),
        400
    );
    let credentials = f.login("bob@example.com", "bob-sub").await?;
    let client = f.client(&credentials)?;
    assert!(client.identity().await.is_ok());
    let mut config = f.auth.config.clone();
    config.allowed_emails.clear();
    f.restart(config, false).await?;
    assert!(client.identity().await.is_err());
    let (session, _) = f.session().await?;
    assert_eq!(
        f.browser_login(
            session.get_authorization_url(),
            "bob@example.com",
            "bob-sub",
            true
        )
        .await?
        .status(),
        400
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_first_logins_share_one_identity_and_personal_vault() -> Result<()> {
    let f = Fixture::new(false).await?;
    let before = f.harness.list_vaults().await?.len();
    let (one, two) = tokio::try_join!(
        f.login("bob@example.com", "bob-sub"),
        f.login("bob@example.com", "bob-sub")
    )?;
    assert_eq!(
        f.client(&one)?.identity().await?.account_id,
        f.client(&two)?.identity().await?.account_id
    );
    assert_eq!(f.auth.store.state.lock().await.principals.len(), 1);
    assert_eq!(f.harness.list_vaults().await?.len(), before + 1);
    Ok(())
}

#[tokio::test]
async fn host_configuration_requires_the_operator() -> Result<()> {
    let f = Fixture::new(false).await?;
    let credentials = f.login("bob@example.com", "bob-sub").await?;
    let runtime = f.runtime.with_caller(
        f.auth
            .caller(f.client(&credentials)?.identity().await?.account_id, false),
    )?;
    for extra in [
        "tools: [module.ts]",
        "tool_creation: true",
        "adapters: [inbox]",
    ] {
        let definition = exo_managed_agents::AgentDefinition::parse(format!(
            "---\nname: worker\nharness: basic\nconfig:\n  model: test\n{extra}\n---\nWork"
        ))?;
        let error = runtime
            .create_managed_agent(&definition, "worker")
            .await
            .err()
            .context("non-owner configured host execution")?;
        assert!(format!("{error:#}").contains("server owner"), "{error:#}");
    }
    let definition = exo_managed_agents::AgentDefinition::parse(
        "---\nname: worker\nharness: basic\nconfig:\n  model: test\n---\nWork".into(),
    )?;
    let agent = runtime.create_managed_agent(&definition, "worker").await?;
    let environment = exoharness::EnvironmentDefinition {
        name: "published".into(),
        config: serde_json::from_value(json!({ "provider": "local-process", "image": "local" }))?,
    };
    assert!(
        runtime
            .exoharness_handle()
            .put_environment(environment.clone())
            .await
            .is_err()
    );
    assert!(
        agent
            .new_thread(exoharness::NewThreadRequest {
                environment: Some(environment.clone()),
                ..Default::default()
            })
            .await
            .is_err()
    );
    f.harness.put_environment(environment.clone()).await?;
    agent
        .new_thread(exoharness::NewThreadRequest {
            environment: Some(environment),
            ..Default::default()
        })
        .await?;
    Ok(())
}
