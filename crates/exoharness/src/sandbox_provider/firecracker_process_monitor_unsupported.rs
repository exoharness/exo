//! Firecracker runs only on Linux. `validate_host_blocking` rejects other hosts
//! before a backend starts, so no monitor exists to observe a VMM here.

use anyhow::{Result, bail};

use super::MonitoredVmm;

const UNSUPPORTED: &str = "Firecracker host process monitoring is only supported on Linux";

pub(super) struct ProcessMonitor;

pub(super) struct Launch;

impl Launch {
    pub(super) fn spawned(self, _jailer_pid: u32) {}
}

impl ProcessMonitor {
    pub(super) fn start() -> Result<Self> {
        bail!(UNSUPPORTED)
    }

    #[cfg(test)]
    pub(super) fn inert() -> Self {
        Self
    }

    pub(super) fn expect_launch(&self, _vmm: MonitoredVmm) -> Result<Launch> {
        bail!(UNSUPPORTED)
    }

    pub(super) fn adopt(&self, _vmm: MonitoredVmm) -> Result<()> {
        bail!(UNSUPPORTED)
    }

    pub(super) fn forget(&self, _machine_id: &str) {}
}
