use std::time::Duration;

use anyhow::Result;
use futures::io::{AsyncReadExt, AsyncWriteExt};

use crate::{BoxAsyncRead, SandboxProcessParts};

/// How long output readers may keep draining after the process exits. A
/// background child can hold a pipe open after its parent exits.
const DRAIN_GRACE: Duration = Duration::from_secs(2);
const READ_CHUNK_BYTES: usize = 64 * 1024;

/// What a process produced when run to completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedProcessOutput {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Output beyond the byte limit was dropped from at least one stream, or a
    /// stream was still open when the drain grace ran out.
    pub truncated: bool,
}

/// The process outlived its timeout and was stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessTimedOut(pub Duration);

impl std::fmt::Display for ProcessTimedOut {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "sandbox process timed out after {} seconds",
            self.0.as_secs_f64()
        )
    }
}

impl std::error::Error for ProcessTimedOut {}

/// Write `stdin` to a started process, close its input, and collect up to
/// `max_output_bytes` from each output stream until it exits.
///
/// Dropping a process's pipes stops it, so a timeout, or dropping the returned
/// future, stops the process as well. A timeout fails with `ProcessTimedOut`.
pub async fn collect_process_output(
    parts: SandboxProcessParts,
    stdin: &[u8],
    timeout: Option<Duration>,
    max_output_bytes: usize,
) -> Result<CollectedProcessOutput> {
    let run = collect(parts, stdin, max_output_bytes);
    match timeout {
        Some(limit) => tokio::time::timeout(limit, run)
            .await
            .map_err(|_| anyhow::Error::new(ProcessTimedOut(limit)))?,
        None => run.await,
    }
}

async fn collect(
    parts: SandboxProcessParts,
    stdin: &[u8],
    max_output_bytes: usize,
) -> Result<CollectedProcessOutput> {
    let SandboxProcessParts {
        stdout,
        stderr,
        stdin: mut input,
        wait,
    } = parts;
    let mut stdout = AbortOnDrop(tokio::spawn(read_capped(stdout, max_output_bytes)));
    let mut stderr = AbortOnDrop(tokio::spawn(read_capped(stderr, max_output_bytes)));
    let write = async move {
        let result = async {
            input.write_all(stdin).await?;
            input.close().await
        }
        .await;
        match result {
            Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => Err(error),
            _ => Ok(()),
        }
    };
    let (written, exit_code) = tokio::join!(write, wait);
    let exit_code = exit_code?;
    written?;
    let drained = tokio::time::timeout(DRAIN_GRACE, async {
        ((&mut stdout.0).await, (&mut stderr.0).await)
    })
    .await;
    let Ok((stdout, stderr)) = drained else {
        return Ok(CollectedProcessOutput {
            exit_code,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: true,
        });
    };
    let (stdout, stdout_truncated) = stdout??;
    let (stderr, stderr_truncated) = stderr??;
    Ok(CollectedProcessOutput {
        exit_code,
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
    })
}

async fn read_capped(mut stream: BoxAsyncRead, max_bytes: usize) -> Result<(Vec<u8>, bool)> {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = vec![0; READ_CHUNK_BYTES];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok((kept, truncated));
        }
        let room = max_bytes.saturating_sub(kept.len());
        kept.extend_from_slice(&chunk[..read.min(room)]);
        truncated |= read > room;
    }
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::io::Cursor;

    use super::*;

    fn parts(
        stdout: &'static [u8],
        stderr: &'static [u8],
        exit: futures::future::BoxFuture<'static, Result<i32>>,
    ) -> (SandboxProcessParts, Arc<Mutex<Vec<u8>>>) {
        let written = Arc::new(Mutex::new(Vec::new()));
        (
            SandboxProcessParts {
                stdout: Box::pin(Cursor::new(stdout)),
                stderr: Box::pin(Cursor::new(stderr)),
                stdin: Box::pin(RecordingWriter(Arc::clone(&written))),
                wait: exit,
            },
            written,
        )
    }

    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl futures::io::AsyncWrite for RecordingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            buffer: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            std::task::Poll::Ready(Ok(buffer.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn collects_output_and_writes_stdin() -> Result<()> {
        let (parts, written) = parts(b"out", b"err", Box::pin(async { Ok(3) }));
        let output = collect_process_output(parts, b"input", None, 1024).await?;
        assert_eq!(
            output,
            CollectedProcessOutput {
                exit_code: 3,
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
                truncated: false,
            }
        );
        assert_eq!(*written.lock().unwrap(), b"input");
        Ok(())
    }

    #[tokio::test]
    async fn caps_each_stream() -> Result<()> {
        let (parts, _) = parts(b"0123456789", b"ab", Box::pin(async { Ok(0) }));
        let output = collect_process_output(parts, b"", None, 4).await?;
        assert_eq!(output.stdout, b"0123");
        assert_eq!(output.stderr, b"ab");
        assert!(output.truncated);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_a_process_that_never_exits() {
        let (parts, _) = parts(b"", b"", Box::pin(futures::future::pending()));
        let error = collect_process_output(parts, b"", Some(Duration::from_secs(5)), 1024)
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProcessTimedOut>(),
            Some(&ProcessTimedOut(Duration::from_secs(5)))
        );
    }
}
