//! Loader for the LiteLLM pricing database.
//!
//! Resolves a [`PricingTable`] from (in order):
//! 1. `EXO_LITELLM_PRICES_PATH` — local file override, used by tests and
//!    air-gapped deployments.
//! 2. On-disk cache at `$XDG_CACHE_HOME/exo/litellm_prices.json` (or
//!    `$HOME/.cache/exo/...`). Used directly if younger than
//!    [`CACHE_TTL`].
//! 3. HTTP fetch from
//!    [`EXO_LITELLM_PRICES_URL`] (defaulting to the LiteLLM main branch).
//!    The result is written back to the cache.
//! 4. If the fetch fails but a *stale* cache file exists, that stale
//!    cache is used rather than producing an empty table.
//! 5. Otherwise, the table is empty (every cost computation returns
//!    `None`; tokens are still persisted).
//!
//! The load runs at most once per process (gated by an
//! [`OnceCell`]); subsequent calls are zero-cost clones of an `Arc`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use exoharness::pricing::PricingTable;
use tokio::sync::OnceCell;

const LITELLM_PRICES_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const CACHE_FILENAME: &str = "litellm_prices.json";

static PRICING_TABLE: OnceCell<Arc<PricingTable>> = OnceCell::const_new();

/// Resolve the global pricing table, loading it on the first call.
///
/// Never panics; on any load failure returns an empty table (cost
/// computation will yield `None`, tokens are still persisted).
pub async fn get_pricing_table() -> Arc<PricingTable> {
    PRICING_TABLE
        .get_or_init(|| async {
            match try_load().await {
                Ok(table) => Arc::new(table),
                Err(err) => {
                    eprintln!(
                        "[exo] failed to load LiteLLM pricing table: {err}; \
                         per-message cost will be unavailable"
                    );
                    Arc::new(PricingTable::empty())
                }
            }
        })
        .await
        .clone()
}

async fn try_load() -> anyhow::Result<PricingTable> {
    // 1. Local path override — used by tests and air-gapped setups.
    if let Ok(path) = std::env::var("EXO_LITELLM_PRICES_PATH") {
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|err| anyhow::anyhow!("reading EXO_LITELLM_PRICES_PATH={path}: {err}"))?;
        return PricingTable::from_json_str(&content);
    }

    let cache = cache_path();
    let (cached_content, cache_is_fresh) = match &cache {
        Some(path) => read_cache(path).await,
        None => (None, false),
    };

    // 2. Fresh cache → use as-is. A corrupt cache is treated as "no
    //    pricing data" (empty table), never a hard error: a truncated or
    //    garbage cache file must not poison the process, and we'd rather
    //    leave cost unset than fill in wrong numbers.
    if let (true, Some(content)) = (cache_is_fresh, &cached_content) {
        return Ok(parse_cache_or_empty(content));
    }

    // 3. Try a network fetch.
    let url = std::env::var("EXO_LITELLM_PRICES_URL")
        .unwrap_or_else(|_| LITELLM_PRICES_URL.to_string());
    match fetch(&url).await {
        Ok(body) => match PricingTable::from_json_str(&body) {
            Ok(table) => {
                // Only cache a body we could actually parse — never persist
                // garbage (e.g. an HTML error page returned with 200).
                if let Some(cache) = &cache {
                    write_cache(cache, &body).await;
                }
                Ok(table)
            }
            Err(err) => {
                eprintln!(
                    "[exo] fetched LiteLLM pricing is unparseable ({err}); \
                     per-message cost will be unavailable"
                );
                Ok(PricingTable::empty())
            }
        },
        Err(fetch_err) => {
            // 4. Stale cache is better than no cache — same corrupt-safe
            //    handling as the fresh path.
            if let Some(content) = cached_content {
                eprintln!(
                    "[exo] LiteLLM pricing fetch failed ({fetch_err}); \
                     using stale cache"
                );
                return Ok(parse_cache_or_empty(&content));
            }
            // 5. Nothing usable.
            Err(fetch_err)
        }
    }
}

/// Parse a cached pricing document, degrading a corrupt/unparseable cache
/// to an empty table (every cost computation returns `None`) rather than
/// surfacing an error. Cost data is best-effort; a bad cache should never
/// take down a turn.
fn parse_cache_or_empty(content: &str) -> PricingTable {
    match PricingTable::from_json_str(content) {
        Ok(table) => table,
        Err(err) => {
            eprintln!(
                "[exo] cached LiteLLM pricing is unparseable ({err}); \
                 per-message cost will be unavailable until the cache refreshes"
            );
            PricingTable::empty()
        }
    }
}

async fn fetch(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build()?;
    let body = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(body)
}

/// Returns `(content, is_fresh)`. `is_fresh` is true only when the file
/// exists, is readable, and its mtime is younger than [`CACHE_TTL`].
async fn read_cache(path: &PathBuf) -> (Option<String>, bool) {
    let metadata = match tokio::fs::metadata(path).await {
        Ok(meta) => meta,
        Err(_) => return (None, false),
    };
    let is_fresh = metadata
        .modified()
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age < CACHE_TTL);
    let content = tokio::fs::read_to_string(path).await.ok();
    (content, is_fresh)
}

async fn write_cache(path: &PathBuf, content: &str) {
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    let _ = tokio::fs::write(path, content).await;
}

fn cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("exo").join(CACHE_FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const FIXTURE_JSON: &str = r#"{
        "claude-sonnet-4-6": {
            "litellm_provider": "anthropic",
            "mode": "chat",
            "input_cost_per_token": 3e-06,
            "output_cost_per_token": 1.5e-05,
            "cache_read_input_token_cost": 3e-07,
            "cache_creation_input_token_cost": 3.75e-06
        }
    }"#;

    /// We can't reuse the global OnceCell across test invocations, so
    /// the loader's public surface is tested only indirectly here via
    /// `try_load`. Exercises the EXO_LITELLM_PRICES_PATH branch.
    #[tokio::test(flavor = "current_thread")]
    async fn local_path_override_is_honored() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fixture.json");
        tokio::fs::write(&path, FIXTURE_JSON).await.unwrap();

        // Set in a scope so we can also test the absence path if needed.
        // SAFETY: tests run in a single tokio current_thread runtime here;
        // env mutation is not racing with other tasks.
        unsafe {
            std::env::set_var("EXO_LITELLM_PRICES_PATH", &path);
        }
        let table = try_load().await.expect("override load");
        unsafe {
            std::env::remove_var("EXO_LITELLM_PRICES_PATH");
        }

        assert!(!table.is_empty());
        let entry = table.lookup("claude-sonnet-4-6").expect("entry");
        assert_eq!(entry.litellm_provider.as_deref(), Some("anthropic"));
    }

    #[test]
    fn corrupt_cache_degrades_to_empty_table() {
        // A truncated/garbage cache must not error or fill in wrong numbers —
        // it degrades to an empty table so cost_usd ends up None.
        let table = parse_cache_or_empty("{ this is not valid json ");
        assert!(table.is_empty());
    }

    #[test]
    fn valid_cache_parses_to_populated_table() {
        let table = parse_cache_or_empty(FIXTURE_JSON);
        assert!(!table.is_empty());
        assert!(table.lookup("claude-sonnet-4-6").is_some());
    }
}
