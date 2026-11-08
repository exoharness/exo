//! Reject counters on each VM's nftables table, reported as
//! `sandbox.firewall_rejects` signals.
//!
//! Each reject or drop rule for guest traffic references a named counter for
//! its purpose. A poller reads every watched table and reports the packets and
//! bytes each counter added since the previous read; cleanup reads a table
//! once more before it is destroyed.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::sandbox_signal::sandbox_signal;

const POLL_INTERVAL: Duration = Duration::from_secs(60);
const MAX_CONCURRENT_READS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum RejectCounter {
    /// Link-local destinations, where cloud instance metadata services live.
    Metadata,
    /// Private, loopback, multicast, and other special-use destinations.
    PrivateRange,
    /// Addresses of this host's VMs.
    OtherVm,
    /// Traffic the sandbox's egress policy does not permit.
    EgressPolicy,
    /// Host addresses other than the egress proxy.
    Host,
    /// Packets whose source is not the VM's address.
    SpoofedSource,
}

impl RejectCounter {
    pub(super) const ALL: [Self; 6] = [
        Self::Metadata,
        Self::PrivateRange,
        Self::OtherVm,
        Self::EgressPolicy,
        Self::Host,
        Self::SpoofedSource,
    ];

    /// The nftables counter name, which is also the signal's `rule`.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::PrivateRange => "private_range",
            Self::OtherVm => "other_vm",
            Self::EgressPolicy => "egress_policy",
            Self::Host => "host",
            Self::SpoofedSource => "spoofed_source",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Count {
    pub(super) packets: u64,
    pub(super) bytes: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct RejectCounts([Count; RejectCounter::ALL.len()]);

// `nft -j list counters` output: a metainfo object followed by one object per
// counter. Tables from before named counters list none.
#[derive(Deserialize)]
struct NftOutput {
    nftables: Vec<NftObject>,
}

#[derive(Deserialize)]
struct NftObject {
    counter: Option<NftCounter>,
}

#[derive(Deserialize)]
struct NftCounter {
    name: RejectCounter,
    packets: u64,
    bytes: u64,
}

impl RejectCounts {
    pub(super) fn parse(json: &[u8]) -> Result<Self> {
        let output: NftOutput = serde_json::from_slice(json).context("parsing nft counters")?;
        let mut counts = Self::default();
        for counter in output
            .nftables
            .into_iter()
            .filter_map(|object| object.counter)
        {
            counts.0[counter.name as usize] = Count {
                packets: counter.packets,
                bytes: counter.bytes,
            };
        }
        Ok(counts)
    }

    /// Nonzero changes since `previous`. A counter below its previous value
    /// belongs to a recreated table, so all of it is new.
    pub(super) fn since(
        &self,
        previous: &Self,
    ) -> impl Iterator<Item = (RejectCounter, Count)> + use<'_> {
        let previous = *previous;
        RejectCounter::ALL.into_iter().filter_map(move |counter| {
            let current = self.0[counter as usize];
            let before = previous.0[counter as usize];
            let delta = if current.packets < before.packets || current.bytes < before.bytes {
                current
            } else {
                Count {
                    packets: current.packets - before.packets,
                    bytes: current.bytes - before.bytes,
                }
            };
            (delta.packets > 0).then_some((counter, delta))
        })
    }
}

struct Watch {
    sandbox_id: String,
    machine_id: String,
    table: String,
    // None until the first read of a table an earlier controller created.
    previous: Option<RejectCounts>,
    finished: bool,
}

/// Tables of running VMs, keyed by machine id.
#[derive(Default)]
pub(super) struct FirewallCounters {
    machines: StdMutex<HashMap<String, Arc<Mutex<Watch>>>>,
}

impl FirewallCounters {
    /// Watch `table` unless this machine is already watched. A table this
    /// controller just created starts at zero; for an adopted table, the first
    /// read only sets the baseline.
    pub(super) fn watch(&self, sandbox_id: &str, machine_id: &str, table: &str, created: bool) {
        self.machines
            .lock()
            .expect("Firecracker firewall counters poisoned")
            .entry(machine_id.to_owned())
            .or_insert_with(|| {
                Arc::new(Mutex::new(Watch {
                    sandbox_id: sandbox_id.to_owned(),
                    machine_id: machine_id.to_owned(),
                    table: table.to_owned(),
                    previous: created.then(RejectCounts::default),
                    finished: false,
                }))
            });
    }

    /// Report what the table counted since its last read and stop watching it.
    /// Call before the table is destroyed or replaced.
    pub(super) async fn finish(&self, machine_id: &str) {
        let watch = self
            .machines
            .lock()
            .expect("Firecracker firewall counters poisoned")
            .remove(machine_id);
        if let Some(watch) = watch {
            let mut watch = watch.lock().await;
            report(&mut watch).await;
            watch.finished = true;
        }
    }

    pub(super) async fn poll(&self) {
        let watches = self
            .machines
            .lock()
            .expect("Firecracker firewall counters poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        futures::stream::iter(watches)
            .for_each_concurrent(MAX_CONCURRENT_READS, |watch| async move {
                let mut watch = watch.lock().await;
                if !watch.finished {
                    report(&mut watch).await;
                }
            })
            .await;
    }
}

/// Read watched tables until the backend that owns `counters` is dropped.
pub(super) async fn poll(counters: Weak<FirewallCounters>) {
    let mut interval =
        tokio::time::interval_at(tokio::time::Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
    loop {
        interval.tick().await;
        let Some(counters) = counters.upgrade() else {
            return;
        };
        counters.poll().await;
    }
}

async fn report(watch: &mut Watch) {
    let table = watch.table.clone();
    let counts = match tokio::task::spawn_blocking(move || read_counts(&table)).await {
        Ok(Ok(counts)) => counts,
        Ok(Err(error)) => {
            tracing::warn!(machine_id = %watch.machine_id, error = format!("{error:#}"), "reading Firecracker firewall counters failed");
            return;
        }
        Err(error) => {
            tracing::warn!(machine_id = %watch.machine_id, %error, "joining Firecracker firewall counter read failed");
            return;
        }
    };
    if let Some(previous) = &watch.previous {
        emit(&watch.sandbox_id, &watch.machine_id, &counts, previous);
    }
    watch.previous = Some(counts);
}

pub(super) fn emit(
    sandbox_id: &str,
    machine_id: &str,
    counts: &RejectCounts,
    previous: &RejectCounts,
) {
    for (counter, delta) in counts.since(previous) {
        sandbox_signal!(
            signal,
            "sandbox.firewall_rejects",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.nftables",
            machine_id,
            rule = counter.name(),
            packets = delta.packets,
            bytes = delta.bytes,
        );
    }
}

pub(super) fn read_counts(table: &str) -> Result<RejectCounts> {
    let nft = super::trusted_host_command("nft")?;
    let output = Command::new(&nft)
        .args(["-j", "list", "counters", "table", "inet", table])
        .output()
        .with_context(|| format!("running {}", nft.display()))?;
    ensure!(
        output.status.success(),
        "nft failed with status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    RejectCounts::parse(&output.stdout)
}
