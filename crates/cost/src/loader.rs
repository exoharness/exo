//! Loads the LiteLLM price database. Resolution: explicit path -> fresh cache
//! -> fetch (cached on success) -> stale cache -> empty. Never fails: any error
//! degrades to an empty table (cost stays unset, tokens still persist).

use std::path::PathBuf;
use std::time::Duration;

use crate::PricingTable;

const DEFAULT_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Load the table once at startup. `path`/`url` are caller-supplied (CLI flags or
/// env), so this stays free of global config reads.
pub async fn load(path: Option<PathBuf>, url: Option<String>) -> PricingTable {
    if let Some(path) = path {
        return match std::fs::read_to_string(&path) {
            Ok(body) => parse_or_empty(&body),
            Err(err) => {
                tracing::error!(
                    path = %path.display(),
                    %err,
                    "reading pricing path failed; cost unavailable"
                );
                PricingTable::empty()
            }
        };
    }

    let cache = cache_path();
    let cached = cache.as_ref().and_then(read_cache);
    if let Some((body, true)) = &cached {
        return parse_or_empty(body);
    }

    let url = url.unwrap_or_else(|| DEFAULT_URL.to_string());
    match fetch(&url).await {
        Ok(body) if PricingTable::from_json_str(&body).is_ok() => {
            if let Some(path) = &cache {
                write_cache(path, &body);
            }
            parse_or_empty(&body)
        }
        Ok(_) => {
            tracing::error!("fetched pricing was unparseable; cost unavailable");
            PricingTable::empty()
        }
        Err(err) => match cached {
            Some((body, _)) => {
                tracing::warn!(%err, "pricing fetch failed; using stale cache");
                parse_or_empty(&body)
            }
            None => {
                tracing::error!(%err, "pricing fetch failed; cost unavailable");
                PricingTable::empty()
            }
        },
    }
}

/// A corrupt or truncated document degrades to an empty table rather than erroring.
fn parse_or_empty(body: &str) -> PricingTable {
    PricingTable::from_json_str(body).unwrap_or_else(|_| {
        tracing::error!("cached pricing is unparseable; cost unavailable until it refreshes");
        PricingTable::empty()
    })
}

async fn fetch(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build()?;
    Ok(client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?)
}

/// Returns `(body, is_fresh)`; `is_fresh` is true only within `CACHE_TTL`.
fn read_cache(path: &PathBuf) -> Option<(String, bool)> {
    let fresh = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < CACHE_TTL);
    Some((std::fs::read_to_string(path).ok()?, fresh))
}

fn write_cache(path: &PathBuf, body: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, body);
}

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("exo").join("litellm_prices.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FIXTURE: &str = r#"{ "claude-sonnet-4-6": { "litellm_provider": "anthropic", "input_cost_per_token": 3e-06 } }"#;

    #[tokio::test]
    async fn explicit_path_is_loaded() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("prices.json");
        std::fs::write(&path, FIXTURE).unwrap();
        let table = load(Some(path), None).await;
        assert!(table.lookup("claude-sonnet-4-6").is_some());
    }

    #[test]
    fn corrupt_body_degrades_to_empty() {
        assert!(parse_or_empty("{ not json").is_empty());
    }
}
