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
                connections.spawn(handle(client?));
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
