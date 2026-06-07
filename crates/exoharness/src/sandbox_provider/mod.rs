//! Per-provider [`crate::sandbox::ManagedSandboxBackend`] implementations,
//! selected via the harness's provider registry.
mod docker;

mod daytona;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod process_bridge;
mod vercel;

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use daytona::{
    DEFAULT_DAYTONA_API_URL, DEFAULT_DAYTONA_TOOLBOX_URL, DaytonaConfig, DaytonaSandboxBackend,
};
pub use daytona::{DEFAULT_DAYTONA_IMAGE, default_daytona_image};
pub use docker::{DEFAULT_DOCKER_IMAGE, default_docker_image};
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use vercel::{DEFAULT_VERCEL_API_URL, VercelConfig, VercelSandboxBackend};
pub use vercel::{DEFAULT_VERCEL_IMAGE, default_vercel_image};

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
fn shell_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':' | '=' | ',')
        })
    {
        return arg.to_string();
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    quoted
}
