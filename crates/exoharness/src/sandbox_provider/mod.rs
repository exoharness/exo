//! Per-provider [`crate::sandbox::ManagedSandboxBackend`] implementations,
//! selected via the harness's provider registry.
mod daytona;
mod process_bridge;
mod vercel;

pub use daytona::{
    DEFAULT_DAYTONA_API_URL, DEFAULT_DAYTONA_TOOLBOX_URL, DaytonaConfig, DaytonaSandboxBackend,
};
pub use vercel::{DEFAULT_VERCEL_API_URL, VercelConfig, VercelSandboxBackend};

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
