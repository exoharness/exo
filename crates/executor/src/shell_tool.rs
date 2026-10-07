use crate::ShellToolResult;
use anyhow::Result;
use exoharness::{SandboxProcess, ToolResult};
use futures::io::AsyncReadExt;

pub async fn read_shell_process(process: Box<dyn SandboxProcess>) -> Result<ToolResult> {
    let parts = process.into_parts();
    let mut stdout = parts.stdout;
    let mut stderr = parts.stderr;
    drop(parts.stdin);

    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let (stdout_result, stderr_result, wait_result) = futures::join!(
        stdout.read_to_end(&mut stdout_bytes),
        stderr.read_to_end(&mut stderr_bytes),
        parts.wait,
    );
    stdout_result?;
    stderr_result?;
    let exit_code = wait_result?;

    Ok(serde_json::to_value(ShellToolResult {
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr_bytes).into_owned(),
        exit_code,
    })?)
}
