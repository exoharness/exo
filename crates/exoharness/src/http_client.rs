use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, RequestBuilder, Response, header::HeaderValue, redirect::Policy};
use serde::de::DeserializeOwned;
use std::sync::Arc;
use url::Url;

#[async_trait::async_trait]
pub trait AccessTokenProvider: Send + Sync {
    async fn access_token(&self) -> Result<String>;
}

#[derive(Clone)]
enum Authorization {
    Bearer(String),
    Provider(Arc<dyn AccessTokenProvider>),
}

#[derive(Debug, thiserror::Error)]
#[error("HTTP request failed ({status}) at {url}: {body}")]
pub struct HttpResponseError {
    pub status: reqwest::StatusCode,
    pub url: Url,
    pub body: String,
}

#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    endpoint: Url,
    authorization: Option<Authorization>,
    context: Option<HeaderValue>,
}

impl HttpClient {
    pub fn new(endpoint: Url) -> Result<Self> {
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.cannot_be_a_base()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
        {
            bail!("HTTP endpoint must be an HTTP(S) URL without credentials or fragment");
        }
        Ok(Self {
            client: Client::builder().redirect(Policy::none()).build()?,
            endpoint,
            authorization: None,
            context: None,
        })
    }

    pub fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    pub fn with_context(
        mut self,
        context: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        self.context = if context.is_empty() {
            None
        } else {
            Some(HeaderValue::from_str(&serde_json::to_string(context)?)?)
        };
        Ok(self)
    }

    pub fn with_bearer_token(mut self, token: String) -> Self {
        self.authorization = Some(Authorization::Bearer(token));
        self
    }

    pub fn with_token_provider(mut self, provider: Arc<dyn AccessTokenProvider>) -> Self {
        self.authorization = Some(Authorization::Provider(provider));
        self
    }

    pub fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let mut url = self.endpoint.join(path)?;
        url.set_query(self.endpoint.query());
        let mut request = self.client.request(method, url);
        if let Some(context) = &self.context {
            request = request.header("x-exo-context", context);
        }
        Ok(match &self.authorization {
            Some(Authorization::Bearer(token)) => request.bearer_auth(token),
            Some(Authorization::Provider(_)) => request,
            None => request,
        })
    }

    pub async fn send(&self, request: RequestBuilder) -> Result<Response> {
        let request = match &self.authorization {
            Some(Authorization::Provider(provider)) => {
                request.bearer_auth(provider.access_token().await?)
            }
            _ => request,
        };
        let response = request.send().await.context("sending HTTP request")?;
        let status = response.status();
        if !status.is_success() {
            let url = response.url().clone();
            let body = response.text().await.context("reading HTTP error")?;
            return Err(HttpResponseError { status, url, body }.into());
        }
        Ok(response)
    }

    pub async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        self.send(request)
            .await?
            .json()
            .await
            .context("decoding HTTP response")
    }
}

#[cfg(all(test, feature = "basic-backend"))]
mod tests {
    use super::*;
    use actix_web::{App, HttpRequest, HttpResponse, HttpServer, web};

    #[test]
    fn endpoint_scope_is_preserved_with_request_filters() -> Result<()> {
        let client = HttpClient::new(Url::parse(
            "https://runtime.example/api/?workspace=team%2Fone&label=a%20b",
        )?)?;
        for path in ["", "identity", "agent", "agent/id/thread/id/events"] {
            let request = client
                .request(Method::GET, path)?
                .query(&[("cursor", "page/2")])
                .build()?;
            assert_eq!(request.url().path(), format!("/api/{path}"));
            assert_eq!(
                request.url().query_pairs().collect::<Vec<_>>(),
                [
                    ("workspace".into(), "team/one".into()),
                    ("label".into(), "a b".into()),
                    ("cursor".into(), "page/2".into()),
                ]
            );
        }
        Ok(())
    }

    #[actix_web::test]
    async fn shares_auth_response_errors_and_redirect_policy() -> Result<()> {
        async fn echo(req: HttpRequest) -> HttpResponse {
            match req
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
            {
                Some("Bearer test-token") => HttpResponse::Ok().json("authenticated"),
                _ => HttpResponse::Unauthorized().body("token required"),
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let endpoint = Url::parse(&format!("http://{}/api/", listener.local_addr()?))?;
        let server = HttpServer::new(|| {
            App::new()
                .route("/api/echo", web::get().to(echo))
                .route(
                    "/api/redirect",
                    web::get().to(|| async {
                        HttpResponse::TemporaryRedirect()
                            .insert_header(("location", "/api/echo"))
                            .finish()
                    }),
                )
                .route(
                    "/api/error",
                    web::get()
                        .to(|| async { HttpResponse::ServiceUnavailable().body("unavailable") }),
                )
                .route(
                    "/api/malformed",
                    web::get().to(|| async { HttpResponse::Ok().body("invalid JSON") }),
                )
        })
        .listen(listener)?
        .run();
        let handle = server.handle();
        actix_web::rt::spawn(server);
        let service_error = format!("503 Service Unavailable) at {endpoint}error: unavailable");
        let client = HttpClient::new(endpoint)?;
        let authenticated = client.clone().with_bearer_token("test-token".into());
        let response: String = authenticated
            .json(authenticated.request(Method::GET, "echo")?)
            .await?;
        assert_eq!(response, "authenticated");
        for (client, route, message) in [
            (&client, "echo", "401 Unauthorized"),
            (&authenticated, "redirect", "307 Temporary Redirect"),
            (&authenticated, "error", service_error.as_str()),
            (&authenticated, "malformed", "decoding HTTP response"),
        ] {
            let error = client
                .json::<String>(client.request(Method::GET, route)?)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error:#}");
        }
        handle.stop(false).await;
        Ok(())
    }
}
