use std::future::Future;

use anyhow::Result;
use tokio::task::JoinSet;

pub async fn serve_connections<S, A, H>(
    mut accept: impl FnMut() -> A,
    handle: impl Fn(S) -> H,
    name: &str,
    log_connection_error: impl Fn(anyhow::Error),
) -> Result<()>
where
    A: Future<Output = Result<S>>,
    H: Future<Output = Result<()>> + Send + 'static,
{
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            client = accept() => {
                match client {
                    Ok(client) => { connections.spawn(handle(client)); }
                    Err(error) => {
                        tracing::warn!(%error, listener = name, "failed to accept connection");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            completed = connections.join_next(), if !connections.is_empty() => {
                match completed {
                    Some(Ok(Err(error))) => log_connection_error(error),
                    Some(Err(error)) => eprintln!("{name} task failed: {error}"),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Notify;

    #[tokio::test]
    async fn accept_error_does_not_stop_the_listener() -> Result<()> {
        let handled = Arc::new(Notify::new());
        let completed = handled.clone();
        let mut attempts = 0;
        let task = tokio::spawn(async move {
            serve_connections(
                move || {
                    attempts += 1;
                    let attempt = attempts;
                    async move {
                        match attempt {
                            1 => {
                                Err(std::io::Error::from(std::io::ErrorKind::ConnectionAborted)
                                    .into())
                            }
                            2 => Ok(()),
                            _ => std::future::pending().await,
                        }
                    }
                },
                move |()| {
                    let completed = completed.clone();
                    async move {
                        completed.notify_one();
                        Ok(())
                    }
                },
                "test",
                |_| {},
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), handled.notified()).await?;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        Ok(())
    }
}
