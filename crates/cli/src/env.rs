use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use executor::BraintrustRuntimeConfig;

#[derive(Debug, Clone, Default)]
pub(crate) struct CliEnvironment {
    vars: HashMap<String, String>,
}

impl CliEnvironment {
    pub(crate) fn load(env_file_if_exists: Option<&Path>, env_file: Option<&Path>) -> Result<Self> {
        let mut vars = HashMap::new();

        if let Some(path) = env_file_if_exists
            && path.exists()
        {
            vars.extend(parse_env_file(path)?);
        }

        if let Some(path) = env_file {
            vars.extend(parse_env_file(path)?);
        }

        Ok(Self { vars })
    }

    pub(crate) fn into_vars(self) -> HashMap<String, String> {
        self.vars
    }

    pub(crate) fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str)
    }

    pub(crate) fn braintrust_runtime_config(
        &self,
        api_key: Option<String>,
        app_url: Option<String>,
        api_url: Option<String>,
    ) -> Option<BraintrustRuntimeConfig> {
        let api_key = api_key.or_else(|| self.get("BRAINTRUST_API_KEY").map(str::to_owned))?;
        Some(BraintrustRuntimeConfig {
            api_key,
            app_url: app_url.or_else(|| self.get("BRAINTRUST_APP_URL").map(str::to_owned)),
            api_url: api_url.or_else(|| self.get("BRAINTRUST_API_URL").map(str::to_owned)),
        })
    }
}

fn parse_env_file(path: &Path) -> Result<HashMap<String, String>> {
    let contents = fs::read_to_string(path)?;
    let mut vars = HashMap::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, value)) = line.split_once('=') else {
            return Err(anyhow::anyhow!(
                "invalid env file line {} in {}",
                index + 1,
                path.display()
            ));
        };

        let key = key.trim();
        if key.is_empty() {
            return Err(anyhow::anyhow!(
                "invalid empty env key on line {} in {}",
                index + 1,
                path.display()
            ));
        }

        vars.insert(key.to_string(), strip_quotes(value.trim()).to_string());
    }

    Ok(vars)
}

fn strip_quotes(value: &str) -> &str {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        let first = bytes[0];
        let last = bytes[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }

    value
}
