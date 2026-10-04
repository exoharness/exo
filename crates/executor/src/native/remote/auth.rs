use super::{
    oidc::{AuthConfig, Oidc},
    store::{Client, Session, Store},
};
use actix_web::{
    HttpRequest, HttpResponse, ResponseError,
    cookie::{Cookie, SameSite},
    http::{StatusCode, header},
    web,
};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use exoharness::{ExoHarness, Secret, vault::VaultId};
use openidconnect::{CsrfToken, Nonce, PkceCodeVerifier};
use oxide_auth::code_grant::extensions::Pkce;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use url::Url;

const ACCESS_SECONDS: i64 = 900;
const SESSION_SECONDS: i64 = 30 * 86400;
const LOGIN_SECONDS: i64 = 300;
const MAX_PENDING: usize = 256;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn random() -> String {
    CsrfToken::new_random().secret().clone()
}
fn digest(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

pub struct AuthServer {
    pub(super) config: AuthConfig,
    oidc: Oidc,
    pub(super) store: Arc<Store>,
    harness: Arc<dyn ExoHarness>,
    pending: Mutex<BTreeMap<String, Pending>>,
    codes: Mutex<BTreeMap<String, Code>>,
}

struct Pending {
    request: Option<Authorization>,
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    browser: String,
    expires: i64,
}

struct Code {
    request: Authorization,
    principal: String,
    expires: i64,
}

#[derive(Clone, Deserialize)]
struct Authorization {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    code_challenge_method: String,
    resource: String,
    #[serde(default)]
    scope: String,
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
}

#[derive(Serialize)]
struct Registered {
    client_id: String,
    redirect_uris: Vec<String>,
    token_endpoint_auth_method: &'static str,
    grant_types: [&'static str; 2],
    response_types: [&'static str; 1],
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: String,
    error: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "grant_type", rename_all = "snake_case")]
enum TokenRequest {
    AuthorizationCode {
        code: String,
        client_id: String,
        redirect_uri: String,
        code_verifier: String,
        resource: String,
    },
    RefreshToken {
        refresh_token: String,
        client_id: String,
        resource: String,
        scope: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    token_type: String,
    expires_in: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    scope: String,
}

#[derive(Deserialize)]
struct Revocation {
    token: String,
    client_id: String,
}

#[derive(Debug)]
struct OAuthError(&'static str);
impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl ResponseError for OAuthError {
    fn status_code(&self) -> StatusCode {
        StatusCode::BAD_REQUEST
    }
    fn error_response(&self) -> HttpResponse {
        #[derive(Serialize)]
        struct Body {
            error: &'static str,
        }
        HttpResponse::BadRequest()
            .insert_header((header::CACHE_CONTROL, "no-store"))
            .json(Body { error: self.0 })
    }
}
fn internal(error: anyhow::Error) -> actix_web::Error {
    tracing::error!(error = %error, "server authentication failed");
    actix_web::error::ErrorInternalServerError("server authentication failed")
}

impl AuthServer {
    pub async fn new(config: AuthConfig, harness: Arc<dyn ExoHarness>) -> Result<Self> {
        config.validate()?;
        let vault = harness
            .list_vaults()
            .await?
            .into_iter()
            .find(|v| {
                v.record().name == config.oidc.client_secret.vault
                    || v.record().id.to_string() == config.oidc.client_secret.vault
            })
            .context("OIDC client-secret vault not found")?;
        let secret = vault
            .list_secrets()
            .await?
            .into_iter()
            .find(|s| {
                s.name == config.oidc.client_secret.secret
                    || s.id.to_string() == config.oidc.client_secret.secret
            })
            .context("OIDC client secret not found")?;
        ensure!(
            secret.name != super::store::STATE_SECRET,
            "OIDC client secret must not use the reserved server-state secret"
        );
        let Some(Secret::Key { value }) = vault.get_secret(&secret.id).await? else {
            anyhow::bail!("OIDC client secret must be an API key secret");
        };
        ensure!(!value.is_empty(), "OIDC client secret is empty");
        let oidc = Oidc::discover(&config, value).await?;
        let store = Arc::new(Store::open(vault, &config).await?);
        Ok(Self {
            config,
            oidc,
            store,
            harness,
            pending: Mutex::default(),
            codes: Mutex::default(),
        })
    }

    pub fn auth_vault(&self) -> VaultId {
        self.store.vault_id()
    }
    fn origin(&self) -> String {
        self.config.public_url.origin().ascii_serialization()
    }
    fn resource(&self) -> String {
        format!("{}/exo", self.origin())
    }
    pub fn metadata_url(&self) -> String {
        format!("{}/.well-known/oauth-protected-resource/exo", self.origin())
    }

    pub async fn authenticate(&self, req: &HttpRequest) -> Result<(String, String)> {
        let bearer = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let cookie = req.cookie("exo-session");
        if bearer.is_none()
            && !matches!(
                *req.method(),
                actix_web::http::Method::GET | actix_web::http::Method::HEAD
            )
        {
            ensure!(
                req.headers()
                    .get(header::ORIGIN)
                    .and_then(|v| v.to_str().ok())
                    == Some(self.origin().as_str())
                    && req
                        .headers()
                        .get("X-Exo-CSRF")
                        .and_then(|v| v.to_str().ok())
                        == Some("1"),
                "invalid browser request origin"
            );
        }
        let token = bearer
            .or_else(|| cookie.as_ref().map(|c| c.value()))
            .context("authentication required")?;
        let hash = digest(token);
        let session = self
            .store
            .state
            .lock()
            .await
            .sessions
            .iter()
            .find(|(_, s)| s.access == hash && s.access_expires > now())
            .map(|(id, _)| id.clone())
            .context("access token expired or revoked")?;
        Ok((self.session_principal(&session).await?, session))
    }

    pub(crate) async fn session_principal(&self, id: &str) -> Result<String> {
        let state = self.store.state.lock().await;
        let session = state
            .sessions
            .get(id)
            .filter(|s| s.expires > now())
            .context("session expired or revoked")?;
        let principal = state
            .principals
            .get(&session.principal)
            .context("identity missing")?;
        ensure!(
            self.config.admitted(&principal.email),
            "identity admission has been removed"
        );
        Ok(principal.id.clone())
    }

    fn start_login(
        &self,
        request: Option<Authorization>,
    ) -> Result<HttpResponse, actix_web::Error> {
        let login = self.oidc.login();
        let browser = random();
        let mut pending = self.pending.lock().expect("login state poisoned");
        pending.retain(|_, p| p.expires > now());
        if pending.len() >= MAX_PENDING {
            return Err(OAuthError("temporarily_unavailable").into());
        }
        pending.insert(
            digest(&login.state),
            Pending {
                request,
                nonce: login.nonce,
                verifier: login.verifier,
                browser: digest(&browser),
                expires: now() + LOGIN_SECONDS,
            },
        );
        let cookie = Cookie::build(format!("exo-login-{}", login.state), browser)
            .path("/exo/auth")
            .http_only(true)
            .same_site(SameSite::Lax)
            .secure(self.config.public_url.scheme() == "https")
            .max_age(actix_web::cookie::time::Duration::seconds(LOGIN_SECONDS))
            .finish();
        Ok(HttpResponse::SeeOther()
            .insert_header((header::LOCATION, login.url.to_string()))
            .insert_header((header::CACHE_CONTROL, "no-store"))
            .cookie(cookie)
            .finish())
    }

    async fn issue(&self, principal: String, client: Option<String>) -> Result<Tokens> {
        let mut state = self.store.state.lock().await;
        let mut next = state.clone();
        next.sessions.retain(|_, s| s.expires > now());
        ensure!(
            next.sessions
                .values()
                .filter(|s| s.principal == principal)
                .count()
                < 64,
            "too many sessions; log out on another device"
        );
        let access = random();
        let refresh = client.as_ref().map(|_| random());
        next.sessions.insert(
            random(),
            Session {
                principal,
                client,
                access: digest(&access),
                refresh: refresh.as_deref().map(digest),
                access_expires: now()
                    + if refresh.is_some() {
                        ACCESS_SECONDS
                    } else {
                        SESSION_SECONDS
                    },
                expires: now() + SESSION_SECONDS,
            },
        );
        self.store.save(&next).await?;
        *state = next;
        Ok(Tokens {
            access_token: access,
            token_type: "Bearer".into(),
            expires_in: ACCESS_SECONDS,
            refresh_token: refresh,
            scope: "exo".into(),
        })
    }
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route(
        "/.well-known/oauth-protected-resource/exo",
        web::get().to(resource_metadata),
    )
    .route(
        "/.well-known/oauth-authorization-server",
        web::get().to(server_metadata),
    )
    .service(
        web::scope("/exo/auth")
            .app_data(web::JsonConfig::default().limit(16384))
            .app_data(web::FormConfig::default().limit(16384))
            .route("/register", web::post().to(register))
            .route("/authorize", web::get().to(authorize))
            .route("/login", web::get().to(login))
            .route("/callback", web::get().to(callback))
            .route("/token", web::post().to(token))
            .route("/revoke", web::post().to(revoke))
            .route("/logout", web::post().to(logout)),
    );
}

type Server = web::Data<Arc<AuthServer>>;

async fn resource_metadata(auth: Server) -> HttpResponse {
    #[derive(Serialize)]
    struct Metadata {
        resource: String,
        authorization_servers: Vec<String>,
        scopes_supported: [&'static str; 1],
        bearer_methods_supported: [&'static str; 1],
    }
    HttpResponse::Ok().json(Metadata {
        resource: auth.resource(),
        authorization_servers: vec![auth.origin()],
        scopes_supported: ["exo"],
        bearer_methods_supported: ["header"],
    })
}

async fn server_metadata(auth: Server) -> HttpResponse {
    #[derive(Serialize)]
    struct Metadata {
        issuer: String,
        authorization_endpoint: String,
        token_endpoint: String,
        registration_endpoint: String,
        revocation_endpoint: String,
        response_types_supported: [&'static str; 1],
        grant_types_supported: [&'static str; 2],
        token_endpoint_auth_methods_supported: [&'static str; 1],
        code_challenge_methods_supported: [&'static str; 1],
        scopes_supported: [&'static str; 1],
        authorization_response_iss_parameter_supported: bool,
    }
    let base = format!("{}/exo/auth", auth.origin());
    HttpResponse::Ok().json(Metadata {
        issuer: auth.origin(),
        authorization_endpoint: format!("{base}/authorize"),
        token_endpoint: format!("{base}/token"),
        registration_endpoint: format!("{base}/register"),
        revocation_endpoint: format!("{base}/revoke"),
        response_types_supported: ["code"],
        grant_types_supported: ["authorization_code", "refresh_token"],
        token_endpoint_auth_methods_supported: ["none"],
        code_challenge_methods_supported: ["S256"],
        scopes_supported: ["exo"],
        authorization_response_iss_parameter_supported: true,
    })
}

fn loopback_redirect(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "http"
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.query().is_none()
            && url.host_str().is_some_and(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            })
    })
}

async fn register(
    auth: Server,
    body: web::Json<Registration>,
) -> Result<HttpResponse, actix_web::Error> {
    if body.redirect_uris.is_empty()
        || body.redirect_uris.len() > 4
        || !body.redirect_uris.iter().all(|u| loopback_redirect(u))
        || body
            .token_endpoint_auth_method
            .as_deref()
            .is_some_and(|m| m != "none")
        || body.grant_types.as_ref().is_some_and(|g| {
            g.iter()
                .any(|g| g != "authorization_code" && g != "refresh_token")
        })
        || body.response_types.as_ref().is_some_and(|r| r != &["code"])
    {
        return Err(OAuthError("invalid_client_metadata").into());
    }
    let mut state = auth.store.state.lock().await;
    let mut next = state.clone();
    next.clients.retain(|_, c| c.expires > now());
    if next.clients.len() >= 1024 {
        return Err(OAuthError("temporarily_unavailable").into());
    }
    let id = random();
    next.clients.insert(
        id.clone(),
        Client {
            redirect_uris: body.redirect_uris.clone(),
            expires: now() + SESSION_SECONDS,
        },
    );
    auth.store.save(&next).await.map_err(internal)?;
    *state = next;
    Ok(HttpResponse::Created()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(Registered {
            client_id: id,
            redirect_uris: body.redirect_uris.clone(),
            token_endpoint_auth_method: "none",
            grant_types: ["authorization_code", "refresh_token"],
            response_types: ["code"],
        }))
}

async fn authorize(
    auth: Server,
    query: web::Query<Authorization>,
) -> Result<HttpResponse, actix_web::Error> {
    let state = auth.store.state.lock().await;
    let client = state
        .clients
        .get(&query.client_id)
        .filter(|c| c.expires > now())
        .ok_or(OAuthError("invalid_client"))?;
    if !client.redirect_uris.contains(&query.redirect_uri)
        || query.response_type != "code"
        || query.resource != auth.resource()
        || query.code_challenge_method != "S256"
        || query.code_challenge.len() != 43
        || URL_SAFE_NO_PAD
            .decode(&query.code_challenge)
            .map_or(true, |b| b.len() != 32)
        || query.state.is_empty()
        || query.scope.split_whitespace().any(|s| s != "exo")
    {
        return Err(OAuthError("invalid_request").into());
    }
    drop(state);
    auth.start_login(Some(query.into_inner()))
}

async fn login(auth: Server) -> Result<HttpResponse, actix_web::Error> {
    auth.start_login(None)
}

async fn callback(
    auth: Server,
    req: HttpRequest,
    query: web::Query<Callback>,
) -> Result<HttpResponse, actix_web::Error> {
    let cookie_name = format!("exo-login-{}", query.state);
    let browser = req
        .cookie(&cookie_name)
        .ok_or(OAuthError("invalid_request"))?;
    let pending = {
        let mut pending = auth.pending.lock().expect("login state poisoned");
        let key = digest(&query.state);
        pending
            .get(&key)
            .filter(|p| p.expires > now() && p.browser == digest(browser.value()))
            .ok_or(OAuthError("invalid_request"))?;
        if query.error.is_some() || query.code.is_none() {
            pending.remove(&key);
            return Err(OAuthError("access_denied").into());
        }
        pending.remove(&key).expect("validated pending login")
    };
    let identity = auth
        .oidc
        .identity(
            query.code.clone().expect("validated code"),
            pending.nonce,
            pending.verifier,
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, "OIDC login rejected");
            OAuthError("access_denied")
        })?;
    let principal = auth
        .store
        .enroll(identity, &auth.config, auth.harness.as_ref())
        .await
        .map_err(|error| {
            tracing::warn!(%error, "identity admission rejected");
            OAuthError("access_denied")
        })?;
    let mut response = if let Some(request) = pending.request {
        let mut codes = auth.codes.lock().expect("authorization codes poisoned");
        codes.retain(|_, c| c.expires > now());
        if codes.len() >= MAX_PENDING {
            return Err(OAuthError("temporarily_unavailable").into());
        }
        let code = random();
        let mut redirect =
            Url::parse(&request.redirect_uri).map_err(|_| OAuthError("invalid_request"))?;
        redirect
            .query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &request.state)
            .append_pair("iss", &auth.origin());
        codes.insert(
            digest(&code),
            Code {
                request,
                principal: principal.id,
                expires: now() + 60,
            },
        );
        HttpResponse::SeeOther()
            .insert_header((header::LOCATION, redirect.to_string()))
            .finish()
    } else {
        let tokens = auth.issue(principal.id, None).await.map_err(internal)?;
        HttpResponse::Ok()
            .cookie(
                Cookie::build("exo-session", tokens.access_token)
                    .path("/exo")
                    .http_only(true)
                    .same_site(SameSite::Lax)
                    .secure(auth.config.public_url.scheme() == "https")
                    .max_age(actix_web::cookie::time::Duration::seconds(SESSION_SECONDS))
                    .finish(),
            )
            .body("Signed in to Exo. You can close this window.")
    };
    let mut cookie = Cookie::build(cookie_name, "").path("/exo/auth").finish();
    cookie.make_removal();
    response.add_cookie(&cookie)?;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}

async fn token(
    auth: Server,
    body: web::Form<TokenRequest>,
) -> Result<HttpResponse, actix_web::Error> {
    let tokens = match body.into_inner() {
        TokenRequest::AuthorizationCode {
            code,
            client_id,
            redirect_uri,
            code_verifier,
            resource,
        } => {
            let grant = auth
                .codes
                .lock()
                .expect("authorization codes poisoned")
                .remove(&digest(&code))
                .ok_or(OAuthError("invalid_grant"))?;
            if grant.expires <= now()
                || grant.request.client_id != client_id
                || grant.request.redirect_uri != redirect_uri
                || grant.request.resource != resource
                || !(43..=128).contains(&code_verifier.len())
                || !code_verifier
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
            {
                return Err(OAuthError("invalid_grant").into());
            }
            let pkce = Pkce::required();
            let challenge = pkce
                .challenge(
                    Some("S256".into()),
                    Some(grant.request.code_challenge.into()),
                )
                .map_err(|()| OAuthError("invalid_grant"))?;
            pkce.verify(challenge, Some(code_verifier.into()))
                .map_err(|()| OAuthError("invalid_grant"))?;
            auth.issue(grant.principal, Some(client_id))
                .await
                .map_err(internal)?
        }
        TokenRequest::RefreshToken {
            refresh_token,
            client_id,
            resource,
            scope,
        } => {
            if resource != auth.resource()
                || scope.is_some_and(|s| s.split_whitespace().any(|s| s != "exo"))
            {
                return Err(OAuthError("invalid_grant").into());
            }
            let mut stored = auth.store.state.lock().await;
            let mut next = stored.clone();
            let session = next
                .sessions
                .values_mut()
                .find(|s| {
                    s.refresh.as_deref() == Some(digest(&refresh_token).as_str())
                        && s.client.as_deref() == Some(&client_id)
                        && s.expires > now()
                })
                .ok_or(OAuthError("invalid_grant"))?;
            let principal = next
                .principals
                .get(&session.principal)
                .ok_or(OAuthError("invalid_grant"))?;
            if !auth.config.admitted(&principal.email) {
                return Err(OAuthError("invalid_grant").into());
            }
            let access = random();
            let refresh = random();
            session.access = digest(&access);
            session.refresh = Some(digest(&refresh));
            session.access_expires = now() + ACCESS_SECONDS;
            auth.store.save(&next).await.map_err(internal)?;
            *stored = next;
            Tokens {
                access_token: access,
                token_type: "Bearer".into(),
                expires_in: ACCESS_SECONDS,
                refresh_token: Some(refresh),
                scope: "exo".into(),
            }
        }
    };
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .insert_header((header::PRAGMA, "no-cache"))
        .json(tokens))
}

async fn revoke(
    auth: Server,
    body: web::Form<Revocation>,
) -> Result<HttpResponse, actix_web::Error> {
    let mut stored = auth.store.state.lock().await;
    let mut next = stored.clone();
    let hash = digest(&body.token);
    next.sessions.retain(|_, s| {
        s.client.as_deref() != Some(&body.client_id)
            || (s.access != hash && s.refresh.as_deref() != Some(&hash))
    });
    auth.store.save(&next).await.map_err(internal)?;
    *stored = next;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .finish())
}

async fn logout(auth: Server, req: HttpRequest) -> Result<HttpResponse, actix_web::Error> {
    auth.authenticate(&req)
        .await
        .map_err(|_| OAuthError("invalid_request"))?;
    let cookie = req
        .cookie("exo-session")
        .ok_or(OAuthError("invalid_request"))?;
    let hash = digest(cookie.value());
    let mut stored = auth.store.state.lock().await;
    let mut next = stored.clone();
    next.sessions.retain(|_, s| s.access != hash);
    auth.store.save(&next).await.map_err(internal)?;
    *stored = next;
    let mut cookie = Cookie::build("exo-session", "").path("/exo").finish();
    cookie.make_removal();
    Ok(HttpResponse::Ok().cookie(cookie).finish())
}
