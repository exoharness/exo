use anyhow::{Context, Result, ensure};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::Deserialize;
use url::Url;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    pub public_url: Url,
    pub oidc: OidcConfig,
    pub owner_email: String,
    #[serde(default)]
    pub allowed_emails: Vec<String>,
    #[serde(default)]
    pub shared_vaults: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OidcConfig {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: VaultSecret,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultSecret {
    pub vault: String,
    pub secret: String,
}

pub(super) fn secure_url(url: &Url) -> Result<()> {
    let loopback = url.host_str().is_some_and(|host| {
        host.trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
            || host == "localhost"
    });
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "URL requires HTTPS outside loopback: {url}"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "URL must not contain credentials or a fragment"
    );
    Ok(())
}

impl AuthConfig {
    pub fn validate(&self) -> Result<()> {
        secure_url(&self.public_url)?;
        let issuer = Url::parse(&self.oidc.issuer)?;
        secure_url(&issuer)?;
        ensure!(
            issuer.query().is_none(),
            "OIDC issuer must not contain a query"
        );
        ensure!(
            self.public_url.path() == "/" && self.public_url.query().is_none(),
            "public_url must be an origin without a path or query"
        );
        ensure!(
            !self.oidc.client_id.trim().is_empty(),
            "OIDC client_id is required"
        );
        for email in std::iter::once(&self.owner_email).chain(&self.allowed_emails) {
            ensure!(
                !email.trim().is_empty() && email.contains('@'),
                "invalid admitted email"
            );
        }
        Ok(())
    }

    pub fn callback_url(&self) -> Url {
        self.public_url
            .join("exo/auth/callback")
            .expect("validated origin")
    }

    pub(super) fn admitted(&self, email: &str) -> bool {
        self.owner_email.eq_ignore_ascii_case(email)
            || self
                .allowed_emails
                .iter()
                .any(|e| e.eq_ignore_ascii_case(email))
    }
}

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

pub(super) struct Oidc {
    client: Client,
    http: reqwest::Client,
}

pub(super) struct Login {
    pub url: Url,
    pub state: String,
    pub nonce: Nonce,
    pub verifier: PkceCodeVerifier,
}

pub(super) struct Identity {
    pub issuer: String,
    pub subject: String,
    pub email: String,
}

impl Oidc {
    pub async fn discover(config: &AuthConfig, secret: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(config.oidc.issuer.to_string())?,
            &http,
        )
        .await
        .context("discovering OIDC provider")?;
        secure_url(metadata.authorization_endpoint().url())?;
        secure_url(
            metadata
                .token_endpoint()
                .context("OIDC token endpoint missing")?
                .url(),
        )?;
        let client = CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(config.oidc.client_id.clone()),
            Some(ClientSecret::new(secret)),
        )
        .set_redirect_uri(RedirectUrl::new(config.callback_url().to_string())?);
        Ok(Self { client, http })
    }

    pub fn login(&self) -> Login {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = self
            .client
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scope(Scope::new("email".into()))
            .add_scope(Scope::new("profile".into()))
            .set_pkce_challenge(challenge)
            .url();
        Login {
            url,
            state: state.secret().clone(),
            nonce,
            verifier,
        }
    }

    pub async fn identity(
        &self,
        code: String,
        nonce: Nonce,
        verifier: PkceCodeVerifier,
    ) -> Result<Identity> {
        let tokens = self
            .client
            .exchange_code(AuthorizationCode::new(code))?
            .set_pkce_verifier(verifier)
            .request_async(&self.http)
            .await
            .context("exchanging OIDC authorization code")?;
        let token = tokens
            .id_token()
            .context("OIDC response has no identity token")?;
        let verifier = self.client.id_token_verifier();
        let claims = token
            .claims(&verifier, &nonce)
            .context("validating OIDC identity")?;
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                tokens.access_token(),
                token.signing_alg()?,
                token.signing_key(&verifier)?,
            )?;
            ensure!(actual == *expected, "OIDC access token hash mismatch");
        }
        ensure!(
            claims.email_verified() == Some(true),
            "a verified email is required"
        );
        Ok(Identity {
            issuer: claims.issuer().as_str().to_owned(),
            subject: claims.subject().as_str().to_owned(),
            email: claims
                .email()
                .context("OIDC identity has no email")?
                .as_str()
                .to_lowercase(),
        })
    }
}
