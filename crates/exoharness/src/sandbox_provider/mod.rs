//! Per-provider [`crate::sandbox::ManagedSandboxBackend`] implementations,
//! selected via the harness's provider registry.
mod daytona;
mod e2b;
mod sprites;

pub use daytona::{
    DEFAULT_DAYTONA_API_URL, DEFAULT_DAYTONA_TOOLBOX_URL, DaytonaConfig, DaytonaSandboxBackend,
};
pub use e2b::{
    DEFAULT_E2B_API_URL, DEFAULT_E2B_ENVD_PORT, E2bConfig, E2bSandboxBackend,
};
pub use sprites::{DEFAULT_SPRITES_API_URL, SpritesConfig, SpritesSandboxBackend};
