use std::{
    net::{SocketAddr, TcpListener},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::Args;
use executor::{
    Runtime,
    http_service::{RuntimeHttpService, server},
};

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// OIDC login configuration; required for a non-loopback listener.
    #[arg(long, requires = "serve")]
    auth_file: Option<PathBuf>,
    /// Share agents and threads among admitted users; keep vaults private.
    #[arg(long, requires_all = ["auth_file", "serve"])]
    multiplayer: bool,
    /// Address for the HTTP server.
    #[arg(long, default_value = "127.0.0.1:4766", requires = "serve")]
    bind: SocketAddr,
    /// DNS suffix for browser previews; resolve it to this host or an SSH tunnel.
    #[arg(long, default_value = "localhost", requires = "serve")]
    preview_domain: String,
    /// Concurrent thread resumptions during recovery.
    #[arg(long, default_value = "4", requires = "serve")]
    recovery_thread_concurrency: NonZeroUsize,
    /// Run adapter supervision without opening the HTTP service.
    #[arg(long, requires = "serve")]
    adapters_only: bool,
    #[cfg(feature = "firecracker")]
    #[command(flatten, next_help_heading = "Firecracker backend options")]
    pub(crate) firecracker: crate::FirecrackerArgs,
}

pub async fn run(
    runtime: Arc<Runtime>,
    root: &Path,
    args: ServeArgs,
    only_agent: Option<exoharness::AgentId>,
) -> Result<()> {
    runtime.set_recovery_concurrency(args.recovery_thread_concurrency)?;
    anyhow::ensure!(
        args.adapters_only || args.bind.ip().is_loopback() || args.auth_file.is_some(),
        "non-loopback serving requires --auth-file"
    );
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_max_level(tracing::Level::INFO)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initializing service logging: {error}"))?;
    std::fs::create_dir_all(root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("service.lock"))?;
    lock.try_lock()
        .context("another agent service is using this root")?;
    let auth = if let Some(path) = &args.auth_file {
        let config: executor::remote::AuthConfig = crate::read_config_file(path)?;
        eprintln!("OIDC callback: {}", config.callback_url());
        let auth =
            Arc::new(executor::remote::AuthServer::new(config, runtime.exoharness_handle()).await?);
        auth.adopt_local_state(runtime.exoharness_handle().as_ref())
            .await?;
        Some(auth)
    } else {
        None
    };
    let mut service = RuntimeHttpService::new(runtime.clone(), None)?;
    if let Some(auth) = &auth {
        service = service.with_auth(auth.clone(), args.multiplayer);
    }
    if let Some(agent) = only_agent {
        service = service.for_agent(agent);
    }
    let adapters = executor::managed_agents::adapters::AdapterService::new(
        runtime.clone(),
        auth.clone(),
        only_agent,
    )
    .await?;
    service = service.with_adapters(adapters.clone());
    let shutdown = adapters.cancellation();
    let adapter_task = adapters.clone().run();
    if args.adapters_only {
        anyhow::ensure!(
            adapters.webhook_paths().await?.is_empty(),
            "HTTP Slack adapters require the HTTP listener; omit --adapters-only"
        );
        service.spawn_recovery()?;
        if let Some(auth) = &auth {
            let caller = auth.caller(auth.owner().await, args.multiplayer);
            caller.policy.default_vault(&caller.principal).await?;
        }
        tokio::pin!(adapter_task);
        let result = tokio::select! {
            result = &mut adapter_task => result,
            result = shutdown_signal() => {
                result?;
                begin_shutdown();
                shutdown.cancel();
                adapter_task.await
            },
        };
        service.shutdown_callers().await?;
        return result;
    }
    let previews = runtime
        .start_preview_server(root, &args.preview_domain, only_agent)
        .await?;
    println!(
        "preview listener: 127.0.0.1:{} (domain: {})",
        previews.port, previews.domain
    );
    let listener = TcpListener::bind(args.bind)?;
    println!(
        "listening: http://{}{}",
        listener.local_addr()?,
        exo_managed_agents::http::RUNTIME_PATH
    );
    for path in adapters.webhook_paths().await? {
        println!("Slack webhook: http://{}{path}", listener.local_addr()?);
    }
    let service = Arc::new(service);
    let server = server(listener, service.clone())?;
    let handle = server.handle();
    tokio::pin!(server, adapter_task);
    let result = tokio::select! {
        result = &mut server => {
            shutdown.cancel();
            adapter_task.await?;
            result.context("serving managed agents")
        },
        result = &mut adapter_task => {
            handle.stop(true).await;
            server.await?;
            result
        },
        result = shutdown_signal() => {
            result?;
            begin_shutdown();
            shutdown.cancel();
            let ((), adapters, server) = tokio::join!(handle.stop(true), adapter_task, server);
            adapters?;
            server.context("stopping managed agent HTTP service")
        },
    };
    service.shutdown_callers().await?;
    result
}

async fn shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("listening for Ctrl-C"),
            signal = terminate.recv() => signal.context("termination signal stream closed"),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c()
        .await
        .context("listening for Ctrl-C")
}

fn begin_shutdown() {
    eprintln!("Shutting down. Press Ctrl-C again to force exit.");
    // Keep the second interrupt available through runtime cleanup in main.
    tokio::spawn(async {
        match shutdown_signal().await {
            Ok(()) => std::process::exit(130),
            Err(error) => tracing::error!(%error, "listening for forced shutdown"),
        }
    });
}
