use actix_web::{App, HttpResponse, HttpServer, web};
use anyhow::{Context, Result, anyhow, bail};
use oauth2::TokenResponse;
use rmcp::transport::auth::{
    AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    InMemoryCredentialStore, StoredCredentials,
};
use serde::Deserialize;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

pub(crate) struct Grant {
    pub credentials: StoredCredentials,
    pub resource: Option<String>,
}

struct CallbackState {
    session: AuthorizationSession,
    csrf: String,
    completed: Mutex<Option<oneshot::Sender<Result<String>>>>,
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
    iss: Option<String>,
    error: Option<String>,
}

async fn callback(
    query: web::Query<Callback>,
    state: web::Data<Arc<CallbackState>>,
) -> HttpResponse {
    if query.state.as_deref() != Some(state.csrf.as_str()) {
        return HttpResponse::BadRequest().body("Invalid OAuth state. Return to the login window.");
    }
    let result = async {
        if query.error.is_some() {
            bail!("OAuth authorization was denied; retry the login command");
        }
        let token = state
            .session
            .handle_callback_with_issuer(
                query.code.as_deref().context("missing OAuth code")?,
                query.state.as_deref().context("missing OAuth state")?,
                query.iss.as_deref(),
            )
            .await?;
        Ok(token.access_token().secret().clone())
    }
    .await;
    let response = if result.is_ok() {
        HttpResponse::Ok().body("Authorization complete. Return to Exo to finish logging in.")
    } else {
        HttpResponse::BadRequest().body("Exo login failed. See the terminal for details.")
    };
    if let Some(sender) = state
        .completed
        .lock()
        .expect("OAuth callback poisoned")
        .take()
        && sender.send(result).is_err()
    {
        tracing::debug!("OAuth login was canceled");
    }
    response
}

pub(crate) async fn authorize_with<F, Fut>(
    mut manager: AuthorizationManager,
    scopes: &[String],
    client_id: Option<&str>,
    launch: F,
    timeout: Duration,
) -> Result<Grant>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let pending = InMemoryCredentialStore::new();
    manager.set_credential_store(pending.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let redirect = format!("http://{}/callback", listener.local_addr()?);
    let mut request = AuthorizationRequest::new(redirect)
        .with_client_name("Exo")
        .with_application_type("native")
        .with_scopes(scopes.to_vec());
    if let Some(client_id) = client_id {
        request = request.with_preregistered_client(client_id);
    }
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, error)| error)?;
    let authorization_url = session.get_authorization_url().to_owned();
    let resource = url::Url::parse(&authorization_url)?
        .query_pairs()
        .find(|(name, _)| name == "resource")
        .map(|(_, value)| value.into_owned());
    let csrf = url::Url::parse(&authorization_url)?
        .query_pairs()
        .find(|(name, _)| name == "state")
        .context("OAuth state missing")?
        .1
        .into_owned();
    let (sender, receiver) = oneshot::channel();
    let state = Arc::new(CallbackState {
        session,
        csrf,
        completed: Mutex::new(Some(sender)),
    });
    let server = HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(state.clone()))
            .route("/callback", web::get().to(callback))
    })
    .workers(1)
    .disable_signals()
    .listen(listener)?
    .run();
    let handle = server.handle();
    let server = tokio::spawn(server);
    let result = async {
        tokio::select! {
            result = tokio::time::timeout(timeout, async {
                launch(authorization_url).await?;
                receiver.await.context("OAuth callback closed")?
            }) => result.context("OAuth login timed out")?,
            result = tokio::signal::ctrl_c() => { result?; Err(anyhow!("OAuth login canceled")) },
        }
    }
    .await;
    handle.stop(true).await;
    server.await??;
    result?;
    Ok(Grant {
        credentials: pending
            .load()
            .await?
            .context("OAuth credentials missing after login")?,
        resource,
    })
}

pub(crate) async fn open_browser(authorization_url: String, no_browser: bool) -> Result<()> {
    println!("Open this URL to log in:\n{authorization_url}");
    if !no_browser {
        let command = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let status = tokio::process::Command::new(command)
            .arg(&authorization_url)
            .status()
            .await?;
        if !status.success() {
            bail!("could not open browser; retry with --no-browser");
        }
    }
    Ok(())
}
