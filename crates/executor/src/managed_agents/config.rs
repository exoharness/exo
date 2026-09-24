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
    let (kind, module) = match harness {
        "basic" => (AgentHarnessKind::Basic, None),
        "rlm" => (AgentHarnessKind::Rlm, None),
        "typescript" | "exo" => {
            bail!("{harness} agents require a TypeScript module path or a named harness preset")
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
    let model = model.unwrap_or(&definition.frontmatter.config.model);
    if model.trim().is_empty() {
        bail!("model must not be empty");
    }
    Ok(AgentConfig {
        harness: kind,
        typescript: module,
        enable_agent_tool_creation: false,
        instructions: vec![crate::harness_helpers::system_message(
            &definition.system_prompt(),
        )],
        sandbox: AgentSandboxConfig {
            image: preset
                .and_then(TypeScriptHarnessPreset::sandbox_image)
                .map(str::to_string),
            provider: sandbox,
            scope: Default::default(),
            mounts: vec![],
            enable_networking: true,
        },
        model: model.into(),
        max_output_tokens: None,
        max_tool_round_trips: None,
        braintrust: None,
    })
}
