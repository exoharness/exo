use crate::{
    AgentConfig, AgentHarnessKind, AgentSandboxConfig, ConversationHarnessConfig,
    TypeScriptHarnessConfig,
};
use anyhow::{Context, Result, bail};
use exo_managed_agents::AgentDefinition;
use exoharness::{CredentialDestination, SandboxProvider};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TypeScriptHarnessPreset {
    Codex,
    ClaudeCode,
    Cursor,
    Pi,
}

impl TypeScriptHarnessPreset {
    pub fn agent_slug(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::ClaudeCode => "claude-code",
            Self::Cursor => "cursor",
            Self::Pi => "pi",
        }
    }

    pub fn module_path(self) -> &'static Path {
        match self {
            Self::Codex => Path::new("exoharness/examples/typescript/codex-harness.ts"),
            Self::ClaudeCode => Path::new("exoharness/examples/typescript/claude-code-harness.ts"),
            Self::Cursor => Path::new("exoharness/examples/typescript/cursor-sdk-harness.ts"),
            Self::Pi => Path::new("exoharness/examples/typescript/pi-harness.ts"),
        }
    }

    pub fn sandbox_image(self) -> Option<&'static str> {
        match self {
            Self::Codex => Some(
                "ghcr.io/exoharness/codex-devbox@sha256:d6c147fb855862aef256672a81f28ad7970e510bee7c07553b69c282709311f7",
            ),
            Self::ClaudeCode => Some(
                "ghcr.io/exoharness/claude-code-devbox@sha256:7d8bb4cc57a3c31a6155737b5fb0d0d939802504ff84143df4219f8093b5b819",
            ),
            Self::Cursor => Some("exo-cursor-sdk-sandbox:latest"),
            Self::Pi => Some(
                "ghcr.io/exoharness/pi-devbox@sha256:15a8b71b99a0b0583b651e48f24fbfe2e10af02add9aa7fa7bccd2f74af429a5",
            ),
        }
    }
}

pub(crate) fn is_preset_sandbox_image(image: &str) -> bool {
    [
        TypeScriptHarnessPreset::Codex,
        TypeScriptHarnessPreset::ClaudeCode,
        TypeScriptHarnessPreset::Cursor,
        TypeScriptHarnessPreset::Pi,
    ]
    .into_iter()
    .filter_map(TypeScriptHarnessPreset::sandbox_image)
    .any(|default| match default.split_once("@sha256:") {
        Some((repository, _)) => image
            .split_once("@sha256:")
            .is_some_and(|(candidate, _)| candidate == repository),
        None => image == default,
    })
}

pub(crate) fn sandbox_model_credential_variable(
    config: &AgentConfig,
) -> Result<Option<&'static str>> {
    let module = config
        .typescript
        .as_ref()
        .and_then(|config| Path::new(&config.module_path).file_name())
        .and_then(|name| name.to_str());
    Ok(match module {
        Some("codex-harness.ts") => Some("OPENAI_API_KEY"),
        Some("claude-code-harness.ts") => Some("ANTHROPIC_API_KEY"),
        Some("pi-harness.ts") => Some(
            match config
                .model
                .split_once('/')
                .map(|(provider, _)| provider)
                .unwrap_or("openai")
            {
                "openai" => "OPENAI_API_KEY",
                "anthropic" => "ANTHROPIC_API_KEY",
                "google" => "GEMINI_API_KEY",
                provider => {
                    bail!("Pi API-key credentials are not configured for provider {provider}")
                }
            },
        ),
        _ => None,
    })
}

pub fn model_credential_destination(config: &AgentConfig) -> Result<Option<CredentialDestination>> {
    let endpoint = if let Some(variable) = sandbox_model_credential_variable(config)? {
        exoharness::vault::model_endpoint(config.base_url.as_deref(), variable)?
    } else if !matches!(config.harness, AgentHarnessKind::TypeScript) {
        match config.base_url.as_deref() {
            Some(url) => url::Url::parse(url)?,
            None => exoharness::vault::model_endpoint(
                None,
                if crate::model_config::is_anthropic_model(&config.model) {
                    "ANTHROPIC_API_KEY"
                } else {
                    "OPENAI_API_KEY"
                },
            )?,
        }
    } else {
        return Ok(None);
    };
    Ok(Some(CredentialDestination::origin(
        &endpoint.origin().ascii_serialization(),
    )?))
}

pub trait HarnessModules: Send + Sync {
    fn installation(&self) -> Result<PathBuf>;
    fn resolve(&self, path: &Path) -> Result<PathBuf>;
    fn preset_image(&self, preset: TypeScriptHarnessPreset) -> Option<String>;
}

fn resolve_harness(
    harness: &str,
    base: &Path,
    configured_module: Option<&Path>,
    modules: &dyn HarnessModules,
) -> Result<(
    AgentHarnessKind,
    Option<TypeScriptHarnessConfig>,
    Option<TypeScriptHarnessPreset>,
)> {
    let preset = match harness {
        "codex" => Some(TypeScriptHarnessPreset::Codex),
        "claude-code" => Some(TypeScriptHarnessPreset::ClaudeCode),
        "cursor" | "cursor-sdk" => Some(TypeScriptHarnessPreset::Cursor),
        "pi" => Some(TypeScriptHarnessPreset::Pi),
        _ => None,
    };
    let (kind, module) = match harness {
        "basic" => (AgentHarnessKind::Basic, None),
        "rlm" => (AgentHarnessKind::Rlm, None),
        "typescript" | "exo" => {
            let path = configured_module
                .with_context(|| format!("{harness} agents require config.module"))?;
            let path = modules
                .resolve(path)
                .with_context(|| format!("resolving harness module {}", path.display()))?;
            (
                if harness == "exo" {
                    AgentHarnessKind::Exo
                } else {
                    AgentHarnessKind::TypeScript
                },
                Some(TypeScriptHarnessConfig {
                    module_path: path.to_string_lossy().into_owned(),
                    tool_module_paths: vec![],
                }),
            )
        }
        _ => {
            let module = if let Some(preset) = preset {
                modules.installation()?.join(preset.module_path())
            } else {
                let path = Path::new(harness);
                if !harness.contains(std::path::MAIN_SEPARATOR)
                    && !path
                        .extension()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| matches!(s, "ts" | "tsx" | "js" | "mjs" | "cjs"))
                {
                    bail!("unknown harness: {harness}");
                }
                base.join(path)
            };
            let module = modules.resolve(&module).with_context(|| {
                format!(
                    "resolving harness module {} on this provider",
                    module.display()
                )
            })?;
            (
                AgentHarnessKind::TypeScript,
                Some(TypeScriptHarnessConfig {
                    module_path: module.to_string_lossy().into_owned(),
                    tool_module_paths: vec![],
                }),
            )
        }
    };
    Ok((kind, module, preset))
}

pub(crate) async fn resolve_thread_harness(
    thread: &dyn exoharness::ThreadHandle,
    config: &AgentConfig,
    harness: &str,
    modules: &dyn HarnessModules,
) -> Result<ConversationHarnessConfig> {
    if !matches!(
        harness,
        "basic" | "native" | "rlm" | "codex" | "claude-code" | "cursor" | "cursor-sdk" | "pi"
    ) && let Some(caller) = thread.caller()
    {
        caller.policy.check_operator(&caller.principal).await?;
    }
    let (kind, module, preset) = resolve_harness(
        if harness == "native" {
            "basic"
        } else {
            harness
        },
        Path::new("."),
        config
            .typescript
            .as_ref()
            .map(|config| Path::new(&config.module_path)),
        modules,
    )?;
    Ok(ConversationHarnessConfig {
        kind,
        module_path: module.map(|config| config.module_path),
        preset,
    })
}

pub(crate) fn apply_thread_harness(
    config: &mut AgentConfig,
    harness: Option<&ConversationHarnessConfig>,
) -> Result<()> {
    let Some(harness) = harness else {
        return Ok(());
    };
    let mut module = harness
        .module_path
        .as_ref()
        .map(|module_path| TypeScriptHarnessConfig {
            module_path: module_path.clone(),
            tool_module_paths: vec![],
        });
    if let Some(previous) = &config.typescript
        && !previous.tool_module_paths.is_empty()
    {
        module
            .as_mut()
            .context("tool modules require a TypeScript harness")?
            .tool_module_paths = previous.tool_module_paths.clone();
    }
    if config.reasoning_effort.is_some() && harness.preset != Some(TypeScriptHarnessPreset::Codex) {
        bail!("model.reasoning_effort is only supported by the codex harness");
    }
    config.harness = harness.kind;
    config.typescript = module;
    if config
        .sandbox
        .image
        .as_deref()
        .is_none_or(is_preset_sandbox_image)
    {
        config.sandbox.image = harness
            .preset
            .and_then(TypeScriptHarnessPreset::sandbox_image)
            .map(str::to_owned);
    }
    Ok(())
}

pub fn agent_config_with_modules(
    definition: &AgentDefinition,
    sandbox: SandboxProvider,
    harness: Option<&str>,
    model: Option<&str>,
    modules: &dyn HarnessModules,
) -> Result<AgentConfig> {
    let base = definition
        .path()
        .and_then(Path::parent)
        .unwrap_or(Path::new("."));
    let (kind, mut module, preset) = resolve_harness(
        harness.unwrap_or(&definition.frontmatter.harness),
        if harness.is_some() {
            Path::new(".")
        } else {
            base
        },
        definition
            .frontmatter
            .config
            .module
            .as_deref()
            .map(|path| base.join(path))
            .as_deref(),
        modules,
    )?;
    if !definition.frontmatter.tools.is_empty() {
        let module = module
            .as_mut()
            .context("tool modules require a TypeScript harness")?;
        let base = definition
            .path()
            .and_then(Path::parent)
            .unwrap_or(Path::new("."));
        module.tool_module_paths = definition
            .frontmatter
            .tools
            .iter()
            .map(|path| {
                let path = base.join(path);
                Ok(modules
                    .resolve(&path)
                    .with_context(|| {
                        format!("resolving tool module {} on this provider", path.display())
                    })?
                    .to_string_lossy()
                    .into_owned())
            })
            .collect::<Result<_>>()?;
    }
    let model = model.unwrap_or(&definition.frontmatter.config.model);
    if model.trim().is_empty() {
        bail!("model must not be empty");
    }
    if let Some(effort) = definition.frontmatter.config.reasoning_effort.as_deref() {
        if preset != Some(TypeScriptHarnessPreset::Codex) {
            bail!("model.reasoning_effort is only supported by the codex harness");
        }
        if effort.trim().is_empty() {
            bail!("model.reasoning_effort must not be empty");
        }
    }
    Ok(AgentConfig {
        frontend_tools: Vec::new(),
        resources: Vec::new(),
        harness: kind,
        typescript: module,
        enable_agent_tool_creation: definition.frontmatter.tool_creation,
        instructions: vec![crate::harness_helpers::system_message(
            &definition.instructions,
        )],
        sandbox: AgentSandboxConfig {
            image: preset.and_then(|preset| modules.preset_image(preset)),
            provider: sandbox,
            scope: Default::default(),
            mounts: vec![],
            enable_networking: true,
        },
        model: model.into(),
        credential: definition.frontmatter.config.credential.clone(),
        base_url: definition.frontmatter.config.base_url.clone(),
        reasoning_effort: definition.frontmatter.config.reasoning_effort.clone(),
        max_output_tokens: definition.frontmatter.config.max_output_tokens,
        max_tool_round_trips: definition.frontmatter.config.max_tool_round_trips,
        braintrust: definition.frontmatter.config.braintrust.clone(),
    })
}
