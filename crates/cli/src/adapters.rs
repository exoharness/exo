use std::path::Path;
use std::sync::Arc;
use std::{fs, process::Command};

use clap::Subcommand;
use executor::{AdapterRunOptions, AdapterStore, Harness, run_adapters_once, run_adapters_watch};

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
            println!("ADAPTER\tENABLED\tSOURCE\tNAME");
            for adapter in store
                .list_adapters()
                .await?
                .into_iter()
                .filter(|adapter| include_disabled || adapter.enabled)
            {
                println!(
                    "{}\t{}\t{:?}\t{}",
                    adapter.id, adapter.enabled, adapter.source, adapter.name
                );
            }
        }
        AdapterCommands::Run { watch, limit } => {
            if watch {
                let _lock = AdapterRunnerLock::acquire(root)?;
                run_adapters_watch(harness, store, AdapterRunOptions { limit }).await?;
            } else {
                let handled =
                    run_adapters_once(harness, &store, AdapterRunOptions { limit }).await?;
                println!("handled {handled} adapter event(s)");
            }
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

struct AdapterRunnerLock {
    path: std::path::PathBuf,
}

impl AdapterRunnerLock {
    fn acquire(root: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let path = root.join("exoclaw-adapters.lock");
        let pid = std::process::id().to_string();
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                use std::io::Write;
                writeln!(file, "{pid}")?;
                Ok(Self { path })
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing_pid = fs::read_to_string(&path).unwrap_or_default();
                if process_is_running(existing_pid.trim()) {
                    return Err(format!(
                        "adapter runner already appears to be running with pid {}",
                        existing_pid.trim()
                    )
                    .into());
                }
                fs::remove_file(&path)?;
                Self::acquire(root)
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for AdapterRunnerLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn process_is_running(pid: &str) -> bool {
    !pid.is_empty()
        && Command::new("kill")
            .arg("-0")
            .arg(pid)
            .status()
            .is_ok_and(|status| status.success())
}
