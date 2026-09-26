use crate::{CredentialInjectionLocation, CredentialNetworkPolicy, EgressCredentialBinding};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialDestination {
    #[serde(alias = "http")]
    Origin { origin: String },
    #[serde(alias = "mcp")]
    Url {
        #[serde(alias = "server_url")]
        url: String,
    },
}

impl CredentialDestination {
    pub fn origin(origin: &str) -> Result<Self> {
        let url = Self::parse(origin)?;
        ensure!(
            (url.scheme() == "https" || is_loopback(&url))
                && url.path() == "/"
                && url.query().is_none(),
            "credential origins require HTTPS (or loopback HTTP) without a path or query"
        );
        Ok(Self::Origin {
            origin: url.origin().ascii_serialization(),
        })
    }

    pub fn url(value: &str) -> Result<Self> {
        Ok(Self::Url {
            url: Self::parse(value)?.to_string(),
        })
    }

    fn parse(value: &str) -> Result<url::Url> {
        let url = url::Url::parse(value).context("invalid credential destination")?;
        ensure!(
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none(),
            "credential destinations require an HTTP(S) URL without embedded credentials or a fragment"
        );
        Ok(url)
    }

    pub fn normalized(&self) -> Result<Self> {
        match self {
            Self::Origin { origin } => Self::origin(origin),
            Self::Url { url } => Self::url(url),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Origin { origin } => origin,
            Self::Url { url } => url,
        }
    }

    fn permits(&self, destination: &Self) -> bool {
        match (self, destination) {
            (Self::Origin { origin }, other) => url::Url::parse(other.as_str())
                .is_ok_and(|url| url.origin().ascii_serialization() == *origin),
            (Self::Url { url }, Self::Url { url: requested }) => url == requested,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialPolicy {
    pub networking: CredentialNetworkPolicy,
    pub injection_location: CredentialInjectionLocation,
}

impl From<CredentialDestination> for CredentialPolicy {
    fn from(destination: CredentialDestination) -> Self {
        Self::destinations(vec![destination])
    }
}

impl CredentialPolicy {
    pub fn destinations(allowed_destinations: Vec<CredentialDestination>) -> Self {
        Self {
            networking: CredentialNetworkPolicy::Destinations {
                allowed_destinations,
            },
            injection_location: CredentialInjectionLocation { header: true },
        }
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
    pub(crate) fn is_resource(&self) -> bool {
        matches!(&self.networking, CredentialNetworkPolicy::Destinations { allowed_destinations }
            if allowed_destinations.iter().any(|d| matches!(d, CredentialDestination::Url { .. })))
    }

    pub fn normalized(&self) -> Result<Self> {
        Ok(Self {
            networking: self.networking.normalized()?,
            injection_location: self.injection_location,
        })
    }

    pub fn permits(&self, destination: &CredentialDestination) -> bool {
        self.injection_location.header && self.networking.permits(destination)
    }

    pub fn for_destination(&self, destination: CredentialDestination) -> Result<Self> {
        ensure!(
            self.permits(&destination),
            "credential policy does not permit {}",
            destination.as_str()
        );
        Ok(destination.into())
    }

    pub fn binding(&self, name: String, environment_variable: String) -> EgressCredentialBinding {
        EgressCredentialBinding {
            name,
            environment_variable,
            networking: self.networking.clone(),
            injection_location: self.injection_location,
        }
    }
}

impl CredentialNetworkPolicy {
    pub fn normalized(&self) -> Result<Self> {
        Ok(match self {
            Self::Limited { allowed_hosts } => {
                let mut allowed_hosts: Vec<_> =
                    crate::types::canonical_egress_hosts(allowed_hosts)?
                        .into_iter()
                        .collect();
                allowed_hosts.sort();
                Self::Limited { allowed_hosts }
            }
            Self::Destinations {
                allowed_destinations,
            } => Self::Destinations {
                allowed_destinations: allowed_destinations
                    .iter()
                    .map(CredentialDestination::normalized)
                    .collect::<Result<_>>()?,
            },
        })
    }

    pub fn permits(&self, destination: &CredentialDestination) -> bool {
        match self {
            Self::Limited { allowed_hosts } => {
                url::Url::parse(destination.as_str()).is_ok_and(|url| {
                    url.scheme() == "https"
                        && url.host_str().is_some_and(|host| {
                            allowed_hosts
                                .iter()
                                .any(|allowed| allowed.eq_ignore_ascii_case(host))
                        })
                })
            }
            Self::Destinations {
                allowed_destinations,
            } => allowed_destinations
                .iter()
                .any(|allowed| allowed.permits(destination)),
        }
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
    pub(crate) fn hosts(&self) -> Result<std::collections::HashSet<String>> {
        match self {
            Self::Limited { allowed_hosts } => crate::types::canonical_egress_hosts(allowed_hosts),
            Self::Destinations {
                allowed_destinations,
            } => {
                let hosts = allowed_destinations
                    .iter()
                    .map(|destination| {
                        Ok(CredentialDestination::parse(destination.as_str())?
                            .host_str()
                            .context("credential destination host")?
                            .to_owned())
                    })
                    .collect::<Result<Vec<_>>>()?;
                crate::types::canonical_egress_hosts(&hosts)
            }
        }
    }
}

pub(crate) fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_policy_preserves_origins_ports_urls_and_injection_permissions() -> Result<()> {
        let mut policy = CredentialPolicy::destinations(vec![
            CredentialDestination::origin("HTTPS://API.EXAMPLE:8443")?,
            CredentialDestination::url("https://mcp.example/mcp/?tenant=one")?,
        ]);
        for (url, allowed) in [
            ("https://api.example:8443/v1/messages", true),
            ("https://api.example/v1/messages", false),
            ("http://api.example:8443/v1/messages", false),
            ("https://api.example.evil:8443/", false),
            ("https://mcp.example/mcp/?tenant=one", true),
            ("https://mcp.example/mcp/?tenant=two", false),
            ("https://mcp.example/mcp?tenant=one", false),
            ("https://mcp.example/other", false),
        ] {
            let destination = CredentialDestination::url(url)?;
            assert_eq!(policy.permits(&destination), allowed, "{url}");
            assert_eq!(
                policy.for_destination(destination).is_ok(),
                allowed,
                "{url}"
            );
        }
        assert!(!policy.permits(&CredentialDestination::origin("https://mcp.example")?));
        policy.injection_location.header = false;
        assert!(!policy.permits(&CredentialDestination::url(
            "https://api.example:8443/v1/messages"
        )?));
        Ok(())
    }
}
