use crate::{AgentConfig, AgentHarnessKind, AgentSandboxConfig, TypeScriptHarnessConfig};
use anyhow::{Context, Result, bail};
use exo_managed_agents::AgentDefinition;
use exoharness::SandboxProvider;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            Self::Codex => Some("exo-codex-sandbox:latest"),
            Self::ClaudeCode => Some("exo-claude-code-sandbox:latest"),
            Self::Cursor => Some("exo-cursor-sdk-sandbox:latest"),
            Self::Pi => Some("exo-pi-sandbox:latest"),
        }
    }
}

pub(crate) fn installation() -> Result<PathBuf> {
    crate::typescript::typescript_workspace_root()
}

pub fn agent_config(
    definition: &AgentDefinition,
    sandbox: SandboxProvider,
    harness: Option<&str>,
    model: Option<&str>,
) -> Result<AgentConfig> {
    let explicit_harness = harness.is_some();
    let harness = harness.unwrap_or(&definition.frontmatter.harness);
    let preset = match harness {
        "codex" => Some(TypeScriptHarnessPreset::Codex),
        "claude-code" => Some(TypeScriptHarnessPreset::ClaudeCode),
        "cursor" | "cursor-sdk" => Some(TypeScriptHarnessPreset::Cursor),
        "pi" => Some(TypeScriptHarnessPreset::Pi),
        _ => None,
    };
    let (kind, mut module) = match harness {
        "basic" => (AgentHarnessKind::Basic, None),
        "rlm" => (AgentHarnessKind::Rlm, None),
        "typescript" | "exo" => {
            let path = definition
                .frontmatter
                .config
                .module
                .as_ref()
                .with_context(|| format!("{harness} agents require config.module"))?;
            let path = definition
                .path()
                .and_then(Path::parent)
                .unwrap_or(Path::new("."))
                .join(path);
            let path = path
                .canonicalize()
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
                installation()?.join(preset.module_path())
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
                let base = if explicit_harness {
                    Path::new(".")
                } else {
                    definition
                        .path()
                        .and_then(Path::parent)
                        .unwrap_or(Path::new("."))
                };
                base.join(path)
            };
            let module = module.canonicalize().with_context(|| {
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
                Ok(path
                    .canonicalize()
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
    let preset_image = preset
        .and_then(TypeScriptHarnessPreset::sandbox_image)
        .map(str::to_string);
    let sandbox = match definition.frontmatter.sandbox.clone() {
        Some(mut sandbox) => {
            if sandbox.image.is_none() {
                sandbox.image = preset_image;
            }
            sandbox
        }
        None => AgentSandboxConfig {
            image: preset_image,
            provider: sandbox,
            scope: Default::default(),
            mounts: vec![],
            enable_networking: true,
        },
    };
    Ok(AgentConfig {
        harness: kind,
        typescript: module,
        enable_agent_tool_creation: definition.frontmatter.tool_creation,
        instructions: vec![crate::harness_helpers::system_message(
            &definition.system_prompt(),
        )],
        sandbox,
        model: model.into(),
        max_output_tokens: definition.frontmatter.config.max_output_tokens,
        max_tool_round_trips: definition.frontmatter.config.max_tool_round_trips,
        braintrust: definition.frontmatter.config.braintrust.clone(),
    })
}
