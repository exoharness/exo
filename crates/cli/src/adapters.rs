use std::path::Path;
use std::sync::Arc;

use clap::Subcommand;
use executor::{
    AdapterBuildStatus, AdapterRunOptions, AdapterStore, Harness, run_adapters_once,
    run_adapters_watch, validate_adapter_build,
};

#[derive(Debug, Subcommand)]
pub enum AdapterCommands {
    List {
        #[arg(long)]
        include_disabled: bool,
    },
    Run {
        #[arg(long)]
        watch: bool,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    Build {
        adapter_id: String,
    },
    Disable {
        adapter_id: String,
    },
    Delete {
        adapter_id: String,
    },
}

pub async fn handle_adapter_command(
    root: &Path,
    harness: Arc<dyn Harness>,
    command: AdapterCommands,
) -> Result<(), Box<dyn std::error::Error>> {
    let store = AdapterStore::new(root.join("adapters"));
    match command {
        AdapterCommands::List { include_disabled } => {
            println!("ADAPTER\tENABLED\tSOURCE\tKIND\tBUILD\tNAME");
            for adapter in store
                .list_adapters()
                .await?
                .into_iter()
                .filter(|adapter| include_disabled || adapter.enabled)
            {
                println!(
                    "{}\t{}\t{:?}\t{:?}\t{:?}\t{}",
                    adapter.id,
                    adapter.enabled,
                    adapter.source,
                    adapter.kind,
                    adapter.build_status,
                    adapter.name
                );
            }
        }
        AdapterCommands::Run { watch, limit } => {
            if watch {
                run_adapters_watch(harness, store, AdapterRunOptions { limit }).await?;
            } else {
                let handled =
                    run_adapters_once(harness, &store, AdapterRunOptions { limit }).await?;
                println!("handled {handled} adapter event(s)");
            }
        }
        AdapterCommands::Build { adapter_id } => {
            let Some(adapter) = store.get_adapter(&adapter_id).await? else {
                return Err(format!("adapter not found: {adapter_id}").into());
            };
            match validate_adapter_build(&adapter) {
                Ok(()) => {
                    store
                        .mark_built(&adapter_id, AdapterBuildStatus::Succeeded, None)
                        .await?;
                    println!("built adapter {}", adapter_id);
                }
                Err(error) => {
                    store
                        .mark_built(
                            &adapter_id,
                            AdapterBuildStatus::Failed,
                            Some(error.to_string()),
                        )
                        .await?;
                    return Err(error.into());
                }
            };
        }
        AdapterCommands::Disable { adapter_id } => {
            if store.disable_adapter(&adapter_id).await?.is_some() {
                println!("disabled adapter {}", adapter_id);
            } else {
                return Err(format!("adapter not found: {adapter_id}").into());
            }
        }
        AdapterCommands::Delete { adapter_id } => {
            if store.delete_adapter(&adapter_id).await?.is_some() {
                println!("deleted adapter {}", adapter_id);
            } else {
                return Err(format!("adapter not found: {adapter_id}").into());
            }
        }
    }
    Ok(())
}
