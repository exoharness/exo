use super::AgentFrontmatter;
use anyhow::{Context, Result, ensure};
use exoharness::{
    FileSystemMountMode,
    resources::{ResourceDefinition, ResourceSource},
};
use serde::Deserialize;
use std::{
    fmt,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResourceType {
    Directory,
    Git,
}

// Serde buffers flattened, tagged enums and loses their leaf paths and marks.
// Deserialize the literal frontmatter fields first, then build the runtime enum.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ResourceInput {
    name: String,
    #[serde(rename = "type")]
    kind: ResourceType,
    mount_path: Option<String>,
    #[serde(default = "writable")]
    mode: FileSystemMountMode,
    path: Option<PathBuf>,
    url: Option<String>,
    checkout: Option<String>,
    credential: Option<String>,
}

fn writable() -> FileSystemMountMode {
    FileSystemMountMode::ReadWrite
}

impl ResourceInput {
    fn into_resource(self) -> Result<ResourceDefinition> {
        let source = match self.kind {
            ResourceType::Directory => {
                ensure!(
                    self.url.is_none() && self.checkout.is_none() && self.credential.is_none(),
                    "directory resources do not support url, checkout, or credential"
                );
                ResourceSource::Directory {
                    path: self.path.context("directory resource requires path")?,
                }
            }
            ResourceType::Git => ResourceSource::GitRepository {
                path: self.path,
                url: self.url,
                checkout: self.checkout,
                credential: self.credential,
            },
        };
        Ok(ResourceDefinition {
            name: self.name,
            mount_path: self.mount_path,
            mode: self.mode,
            source,
        })
    }
}

pub(super) fn parse(yaml: &str) -> Result<AgentFrontmatter> {
    let input: AgentFrontmatter<ResourceInput> =
        serde_path_to_error::deserialize(serde_yaml_ng::Deserializer::from_str(yaml))
            .map_err(|error| Diagnostic::new(error, yaml))?;
    Ok(AgentFrontmatter {
        resources: input
            .resources
            .into_iter()
            .enumerate()
            .map(|(index, input)| {
                input
                    .into_resource()
                    .with_context(|| format!("resources[{index}]"))
            })
            .collect::<Result<_>>()?,
        name: input.name,
        harness: input.harness,
        model: input.model,
        permission_policy: input.permission_policy,
        tool_policies: input.tool_policies,
        mcp_servers: input.mcp_servers,
        tools: input.tools,
        tool_creation: input.tool_creation,
        adapters: input.adapters,
    })
}

#[derive(Debug)]
struct Diagnostic {
    file: Option<PathBuf>,
    field: String,
    message: String,
    line: Option<usize>,
    column: Option<usize>,
    excerpt: Option<String>,
}

impl Diagnostic {
    fn new(error: serde_path_to_error::Error<serde_yaml_ng::Error>, yaml: &str) -> Self {
        let field = error.path().to_string();
        let error = error.into_inner();
        let location = error.location();
        let mut message = error.to_string();
        // The YAML formatter includes its own path and position. Present those
        // once, using the filename header and the tracked field path.
        if let Some(location) = &location {
            let suffix = format!(" at line {} column {}", location.line(), location.column());
            if let Some(reason) = message.strip_suffix(&suffix) {
                message = reason.to_owned();
            }
        }
        if let Some(reason) = message.strip_prefix(&format!("{field}: ")) {
            message = reason.to_owned();
        }
        let excerpt = location
            .as_ref()
            .and_then(|location| yaml.lines().nth(location.line() - 1))
            .map(str::to_owned);
        Self {
            file: None,
            field,
            message,
            line: location.as_ref().map(|location| location.line()),
            column: location.as_ref().map(|location| location.column()),
            excerpt,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(path) => write!(f, "{}", path.display())?,
            None => f.write_str("agent definition")?,
        }
        if let (Some(line), Some(column)) = (self.line, self.column) {
            write!(f, ":{line}:{column}")?;
        }
        f.write_str(": invalid agent frontmatter")?;
        if !self.field.is_empty() && self.field != "." {
            write!(f, " at {}", self.field)?;
        }
        write!(f, "\n{}", self.message)?;
        if let (Some(line), Some(column), Some(excerpt)) = (self.line, self.column, &self.excerpt) {
            let width = line.to_string().len();
            write!(
                f,
                "\n\n{line:>width$} | {excerpt}\n{:width$} | {:padding$}^",
                "",
                "",
                padding = column.saturating_sub(1)
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for Diagnostic {}

pub(super) fn with_file(error: anyhow::Error, path: &Path) -> anyhow::Error {
    match error.downcast::<Diagnostic>() {
        Ok(mut error) => {
            error.file = Some(path.to_owned());
            error.into()
        }
        Err(error) => error.context(format!("agent file {}", path.display())),
    }
}
