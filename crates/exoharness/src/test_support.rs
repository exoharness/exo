use std::path::PathBuf;

use crate::{
    BasicExoHarnessConfig, DaytonaBackendSpec, SandboxBackendRegistration, SandboxProvider,
    SecretBackendChoice,
};

#[cfg(all(test, unix))]
pub(crate) fn write_test_executable(path: &std::path::Path, script: &str) -> crate::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // Write in a child so parallel test spawns cannot inherit a writable
    // executable descriptor, which briefly makes Linux exec return ETXTBSY.
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "printf '%s' \"$2\" > \"$1\"", "write-test-executable"])
        .arg(path)
        .arg(script)
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "writing test executable: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

pub(crate) fn local_test_config(root: impl Into<PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![SandboxBackendRegistration::local_process()],
    }
}

/// Like [`local_test_config`] but also advertises Daytona, so tests can exercise
/// lazy secret resolution. Daytona credentials are still read from the secret
/// store on first use.
pub(crate) fn local_test_config_with_daytona(root: impl Into<PathBuf>) -> BasicExoHarnessConfig {
    BasicExoHarnessConfig {
        root: root.into(),
        secret_backend: SecretBackendChoice::Static([7u8; 32]),
        sandbox_default: SandboxProvider::LocalProcess,
        sandbox_policy: None,
        sandbox_backends: vec![
            SandboxBackendRegistration::local_process(),
            SandboxBackendRegistration::daytona(DaytonaBackendSpec::default()),
        ],
    }
}
