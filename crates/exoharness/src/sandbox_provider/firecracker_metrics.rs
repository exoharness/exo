//! Firecracker VMM metrics, read on the host.
//!
//! Firecracker writes one JSON record per line on every metrics flush: when the
//! microVM starts and once a minute after, on a `FlushMetrics` action, and from
//! its fatal-signal handlers just before it exits.
//! https://github.com/firecracker-microvm/firecracker/blob/v1.16.2/docs/metrics.md#flushing-the-metrics
//! https://github.com/firecracker-microvm/firecracker/blob/v1.16.2/src/vmm/src/signal_handler.rs
//!
//! The metrics path is a FIFO that the host creates and opens before the VMM
//! starts. Firecracker opens it read-write and non-blocking, so a slow or absent
//! reader never blocks the VMM: once 64 KiB is unread its writes fail and it
//! drops records. A regular file would instead grow by a record a minute for the
//! life of the VM, and without bound once the VMM is compromised.
//! https://github.com/firecracker-microvm/firecracker/blob/v1.16.2/src/vmm/src/utils/mod.rs#L86-L98
//!
//! The jailed VMM owns its chroot and can replace the path, so the host reads
//! only from the descriptor it opened. A compromised VMM can still forge or drop
//! its own records: these signals catch attempts that trip seccomp, a fatal
//! signal, or a device error, not a completed compromise.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::fs::File;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::process::ExitStatus;
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use serde::Deserialize;

pub(super) const JAILED_METRICS: &str = "/firecracker.metrics";
// A 1.16 record is about 5 KiB plus about 0.5 KiB per block or network device,
// which leaves room for over a hundred devices.
const MAX_RECORD_BYTES: usize = 64 * 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;
// Firecracker flushes once a minute and on each FlushMetrics action. The budget
// allows several times that, so only a VMM writing records Firecracker never
// writes is throttled; its records then wait in the pipe until it fills.
const READ_BURST_BYTES: u64 = 16 * READ_CHUNK_BYTES as u64;
const READ_BYTES_PER_SECOND: u64 = 1024;
#[cfg(target_os = "linux")]
const THROTTLED_POLL_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const EXIT_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
// Firecracker exits with this code only after its SIGSYS handler intercepts a
// syscall that its seccomp filter trapped.
// https://github.com/firecracker-microvm/firecracker/blob/v1.16.2/src/vmm/src/lib.rs#L184-L185
const BAD_SYSCALL_EXIT_CODE: i32 = 148;

// Only the counters that show a VMM fault or a guest misusing a device. Each
// 1.16 record carries every section, so a record missing one is malformed.
// `block` and `net` aggregate every device of that kind.
// https://github.com/firecracker-microvm/firecracker/blob/v1.16.2/src/vmm/src/logger/metrics.rs
#[derive(Deserialize)]
struct MetricsRecord {
    seccomp: SeccompMetrics,
    signals: SignalMetrics,
    vcpu: VcpuMetrics,
    block: BlockMetrics,
    net: NetMetrics,
    vsock: VsockMetrics,
    entropy: EntropyMetrics,
}

// The SIGSYS handler counts into num_faults; 1.16 has no separate SIGSYS metric.
#[derive(Deserialize)]
struct SeccompMetrics {
    num_faults: u64,
}

#[derive(Deserialize)]
struct SignalMetrics {
    sigbus: u64,
    sigsegv: u64,
}

#[derive(Deserialize)]
struct VcpuMetrics {
    failures: u64,
}

#[derive(Deserialize)]
struct BlockMetrics {
    activate_fails: u64,
    cfg_fails: u64,
    event_fails: u64,
    execute_fails: u64,
    invalid_reqs_count: u64,
    update_fails: u64,
}

#[derive(Deserialize)]
struct NetMetrics {
    activate_fails: u64,
    cfg_fails: u64,
    event_fails: u64,
    rx_fails: u64,
    tap_read_fails: u64,
    tap_write_fails: u64,
    tx_fails: u64,
    tx_malformed_frames: u64,
}

#[derive(Deserialize)]
struct VsockMetrics {
    activate_fails: u64,
    cfg_fails: u64,
    rx_queue_event_fails: u64,
    tx_queue_event_fails: u64,
    ev_queue_event_fails: u64,
    muxer_event_fails: u64,
    conn_event_fails: u64,
    tx_flush_fails: u64,
    tx_write_fails: u64,
    rx_read_fails: u64,
}

#[derive(Deserialize)]
struct EntropyMetrics {
    activate_fails: u64,
    entropy_event_fails: u64,
    host_rng_fails: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MetricsDelta {
    seccomp_faults: u64,
    sigsegv: u64,
    sigbus: u64,
    vcpu_failures: u64,
    block_errors: u64,
    block_invalid_requests: u64,
    net_errors: u64,
    net_malformed_frames: u64,
    vsock_errors: u64,
    entropy_errors: u64,
}

impl MetricsDelta {
    fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn add(&mut self, other: Self) {
        self.seccomp_faults = self.seccomp_faults.saturating_add(other.seccomp_faults);
        self.sigsegv = self.sigsegv.saturating_add(other.sigsegv);
        self.sigbus = self.sigbus.saturating_add(other.sigbus);
        self.vcpu_failures = self.vcpu_failures.saturating_add(other.vcpu_failures);
        self.block_errors = self.block_errors.saturating_add(other.block_errors);
        self.block_invalid_requests = self
            .block_invalid_requests
            .saturating_add(other.block_invalid_requests);
        self.net_errors = self.net_errors.saturating_add(other.net_errors);
        self.net_malformed_frames = self
            .net_malformed_frames
            .saturating_add(other.net_malformed_frames);
        self.vsock_errors = self.vsock_errors.saturating_add(other.vsock_errors);
        self.entropy_errors = self.entropy_errors.saturating_add(other.entropy_errors);
    }
}

fn sum<const N: usize>(counters: [u64; N]) -> u64 {
    counters.into_iter().fold(0, u64::saturating_add)
}

/// Splits the metrics stream into records and turns them into deltas.
#[derive(Default)]
struct MetricsStream {
    line: Vec<u8>,
    oversized: bool,
    // The seccomp and signal counters hold absolute values. Every other
    // counter is already the change since the previous flush.
    seccomp_faults: u64,
    sigsegv: u64,
    sigbus: u64,
    records: u64,
    malformed: u64,
}

impl MetricsStream {
    fn push(&mut self, mut bytes: &[u8], mut observe: impl FnMut(MetricsDelta)) {
        while let Some(end) = bytes.iter().position(|byte| *byte == b'\n') {
            self.append(&bytes[..end]);
            if self.oversized {
                self.malformed += 1;
            } else if !self.line.is_empty() {
                match serde_json::from_slice::<MetricsRecord>(&self.line) {
                    Ok(record) => {
                        self.records += 1;
                        observe(self.delta(&record));
                    }
                    Err(_) => self.malformed += 1,
                }
            }
            self.line.clear();
            self.oversized = false;
            bytes = &bytes[end + 1..];
        }
        self.append(bytes);
    }

    fn append(&mut self, bytes: &[u8]) {
        if self.oversized {
            return;
        }
        if self.line.len() + bytes.len() > MAX_RECORD_BYTES {
            self.oversized = true;
            self.line.clear();
            return;
        }
        self.line.extend_from_slice(bytes);
    }

    // A line still unterminated when the stream ends was cut short.
    fn finish(&mut self) {
        if self.oversized || !self.line.is_empty() {
            self.malformed += 1;
        }
        self.line.clear();
        self.oversized = false;
    }

    fn delta(&mut self, record: &MetricsRecord) -> MetricsDelta {
        let block = &record.block;
        let net = &record.net;
        let vsock = &record.vsock;
        let entropy = &record.entropy;
        let delta = MetricsDelta {
            seccomp_faults: record
                .seccomp
                .num_faults
                .saturating_sub(self.seccomp_faults),
            sigsegv: record.signals.sigsegv.saturating_sub(self.sigsegv),
            sigbus: record.signals.sigbus.saturating_sub(self.sigbus),
            vcpu_failures: record.vcpu.failures,
            block_errors: sum([
                block.activate_fails,
                block.cfg_fails,
                block.event_fails,
                block.execute_fails,
                block.update_fails,
            ]),
            block_invalid_requests: block.invalid_reqs_count,
            net_errors: sum([
                net.activate_fails,
                net.cfg_fails,
                net.event_fails,
                net.rx_fails,
                net.tap_read_fails,
                net.tap_write_fails,
                net.tx_fails,
            ]),
            net_malformed_frames: net.tx_malformed_frames,
            vsock_errors: sum([
                vsock.activate_fails,
                vsock.cfg_fails,
                vsock.rx_queue_event_fails,
                vsock.tx_queue_event_fails,
                vsock.ev_queue_event_fails,
                vsock.muxer_event_fails,
                vsock.conn_event_fails,
                vsock.tx_flush_fails,
                vsock.tx_write_fails,
                vsock.rx_read_fails,
            ]),
            entropy_errors: sum([
                entropy.activate_fails,
                entropy.entropy_event_fails,
                entropy.host_rng_fails,
            ]),
        };
        self.seccomp_faults = record.seccomp.num_faults;
        self.sigsegv = record.signals.sigsegv;
        self.sigbus = record.signals.sigbus;
        delta
    }
}

/// Bounds how fast the host reads, whatever the VMM writes.
struct ReadBudget {
    available: u64,
    refilled_at: Instant,
}

impl ReadBudget {
    fn new(now: Instant) -> Self {
        Self {
            available: READ_BURST_BYTES,
            refilled_at: now,
        }
    }

    fn allows_read(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled_at).as_millis();
        let earned = u64::try_from(elapsed)
            .unwrap_or(u64::MAX)
            .saturating_mul(READ_BYTES_PER_SECOND)
            / 1000;
        if earned > 0 {
            self.available = self.available.saturating_add(earned).min(READ_BURST_BYTES);
            self.refilled_at = now;
        }
        self.available >= READ_CHUNK_BYTES as u64
    }

    fn spend(&mut self, bytes: usize) {
        self.available = self.available.saturating_sub(bytes as u64);
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum VmmState {
    Running,
    Exited(Option<ExitStatus>),
}

// A guest reboot or poweroff exits 0, and Exo stops a VMM with SIGKILL.
#[cfg(target_os = "linux")]
fn unexpected_exit(status: ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;

    status.code() != Some(0) && status.signal() != Some(libc::SIGKILL)
}

#[cfg(target_os = "linux")]
fn report(sandbox_id: &str, machine_id: &str, delta: &MetricsDelta, state: VmmState) {
    use std::os::unix::process::ExitStatusExt;

    use crate::sandbox_signal::sandbox_signal;

    let status = match state {
        VmmState::Running => None,
        VmmState::Exited(status) => status,
    };
    let exit_code = status.and_then(|status| status.code());
    macro_rules! report {
        ($severity:ident) => {
            sandbox_signal!(
                $severity,
                "sandbox.vmm_metrics",
                sandbox_id = sandbox_id,
                source = "exo.firecracker",
                machine_id,
                seccomp_faults = delta.seccomp_faults,
                sigsegv = delta.sigsegv,
                sigbus = delta.sigbus,
                vcpu_failures = delta.vcpu_failures,
                block_errors = delta.block_errors,
                block_invalid_requests = delta.block_invalid_requests,
                net_errors = delta.net_errors,
                net_malformed_frames = delta.net_malformed_frames,
                vsock_errors = delta.vsock_errors,
                entropy_errors = delta.entropy_errors,
                vmm_exited = matches!(state, VmmState::Exited(_)),
                exit_code,
                exit_signal = status.and_then(|status| status.signal()),
            )
        };
    }
    if delta.seccomp_faults > 0 || exit_code == Some(BAD_SYSCALL_EXIT_CODE) {
        report!(exploit);
    } else {
        report!(signal);
    }
}

/// The host's read end of a VMM's metrics FIFO.
pub(super) struct MetricsPipe(File);

#[cfg(target_os = "linux")]
impl MetricsPipe {
    /// Creates the FIFO for a VMM that runs as `uid`:`gid` and opens it for
    /// reading.
    pub(super) fn create(path: &Path, uid: u32, gid: u32) -> Result<Self> {
        use std::fs::{OpenOptions, Permissions};
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt, fchown};

        use anyhow::{Context, ensure};

        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("removing stale Firecracker metrics FIFO {}", path.display())
                });
            }
        }
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .with_context(|| format!("creating Firecracker metrics FIFO {}", path.display()))?;
        // A non-blocking read end opens without a writer, and polls neither
        // readable nor hung up until the VMM has opened the other end.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(path)
            .with_context(|| format!("opening Firecracker metrics FIFO {}", path.display()))?;
        ensure!(
            file.metadata()?.file_type().is_fifo(),
            "Firecracker metrics path is not a FIFO: {}",
            path.display()
        );
        fchown(&file, Some(uid), Some(gid))?;
        file.set_permissions(Permissions::from_mode(0o600))?;
        Ok(Self(file))
    }

    /// Reports metrics until the VMM exits. `pid_path` is `None` when the
    /// jailer failed, so no VMM was started.
    pub(super) fn watch(self, sandbox_id: &str, machine_id: &str, pid_path: Option<&Path>) {
        let pidfd = pid_path.and_then(|pid_path| {
            open_pidfd(pid_path)
                .inspect_err(|error| {
                    tracing::debug!(machine_id, %error, "Firecracker exited before its metrics were watched");
                })
                .ok()
        });
        let mut monitor = Monitor::new(self.0, sandbox_id, machine_id);
        let status = match pidfd.as_ref() {
            Some(pidfd) => {
                if !monitor.run(pidfd) {
                    return;
                }
                exit_status(pidfd)
            }
            None => None,
        };
        let mut last = MetricsDelta::default();
        monitor.drain(|delta| last.add(delta));
        if !last.is_empty() || status.is_some_and(unexpected_exit) {
            report(sandbox_id, machine_id, &last, VmmState::Exited(status));
        }
        tracing::debug!(
            machine_id,
            status = ?status,
            records = monitor.stream.records,
            malformed = monitor.stream.malformed,
            "Firecracker VMM exited"
        );
    }
}

#[cfg(not(target_os = "linux"))]
impl MetricsPipe {
    pub(super) fn create(_path: &Path, _uid: u32, _gid: u32) -> Result<Self> {
        anyhow::bail!("Firecracker sandbox execution is only supported on Linux")
    }

    pub(super) fn watch(self, _sandbox_id: &str, _machine_id: &str, _pid_path: Option<&Path>) {}
}

#[cfg(target_os = "linux")]
fn open_pidfd(pid_path: &Path) -> Result<std::os::fd::OwnedFd> {
    use anyhow::Context;
    use rustix::process::{Pid, PidfdFlags, pidfd_open};

    let pid = std::fs::read_to_string(pid_path).context("reading Firecracker pid")?;
    let pid = Pid::from_raw(pid.trim().parse().context("invalid Firecracker pid")?)
        .context("invalid Firecracker pid")?;
    pidfd_open(pid, PidfdFlags::empty()).context("opening Firecracker pidfd")
}

// With --new-pid-ns the VMM is not this process's child, so its status comes
// from the pidfd, which Linux 6.15 and later keep once the reaper collects it.
// Earlier kernels reject the request and the status is reported as unknown.
// https://github.com/torvalds/linux/blob/v6.15/include/uapi/linux/pidfd.h
#[cfg(target_os = "linux")]
fn exit_status(pidfd: &std::os::fd::OwnedFd) -> Option<ExitStatus> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::ExitStatusExt;

    let exit = u64::from(libc::PIDFD_INFO_EXIT);
    let deadline = Instant::now() + EXIT_STATUS_TIMEOUT;
    loop {
        // SAFETY: pidfd_info is plain integers, for which all zeroes is valid.
        let mut info: libc::pidfd_info = unsafe { std::mem::zeroed() };
        info.mask = exit;
        // SAFETY: PIDFD_GET_INFO writes at most size_of::<pidfd_info>() bytes,
        // the size it encodes, into `info`.
        if unsafe { libc::ioctl(pidfd.as_raw_fd(), libc::PIDFD_GET_INFO, &mut info) } != 0 {
            return None;
        }
        if info.mask & exit != 0 {
            return Some(ExitStatus::from_raw(info.exit_code));
        }
        // The VMM has exited but its reaper has not collected it yet.
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
struct Monitor<'a> {
    sandbox_id: &'a str,
    machine_id: &'a str,
    pipe: Option<File>,
    stream: MetricsStream,
    budget: ReadBudget,
    buffer: Vec<u8>,
    throttled: bool,
}

#[cfg(target_os = "linux")]
impl<'a> Monitor<'a> {
    fn new(pipe: File, sandbox_id: &'a str, machine_id: &'a str) -> Self {
        Self {
            sandbox_id,
            machine_id,
            pipe: Some(pipe),
            stream: MetricsStream::default(),
            budget: ReadBudget::new(Instant::now()),
            buffer: vec![0; READ_CHUNK_BYTES],
            throttled: false,
        }
    }

    /// Reports records until the VMM exits, and returns false if polling failed.
    fn run(&mut self, pidfd: &std::os::fd::OwnedFd) -> bool {
        use std::os::fd::AsFd;

        use rustix::event::{PollFd, PollFlags, Timespec, poll};

        let throttled_timeout = Timespec::try_from(THROTTLED_POLL_INTERVAL)
            .expect("the throttled poll interval fits a timespec");
        loop {
            let readable = self.budget.allows_read(Instant::now());
            if !readable && !self.throttled && self.pipe.is_some() {
                tracing::warn!(
                    machine_id = self.machine_id,
                    "Firecracker metrics exceeded the host read budget"
                );
                self.throttled = true;
            }
            let pipe = self.pipe.as_ref().filter(|_| readable);
            let mut fds = [
                PollFd::from_borrowed_fd(pidfd.as_fd(), PollFlags::IN),
                PollFd::from_borrowed_fd(
                    pipe.map_or(pidfd.as_fd(), |pipe| pipe.as_fd()),
                    PollFlags::IN,
                ),
            ];
            let fds = &mut fds[..1 + usize::from(pipe.is_some())];
            let timeout = (self.pipe.is_some() && !readable).then_some(&throttled_timeout);
            match poll(fds, timeout) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => {
                    tracing::warn!(machine_id = self.machine_id, %error, "failed polling Firecracker metrics");
                    return false;
                }
            }
            if !fds[0].revents().is_empty() {
                return true;
            }
            if fds.get(1).is_some_and(|fd| !fd.revents().is_empty()) {
                let (sandbox_id, machine_id) = (self.sandbox_id, self.machine_id);
                self.read(|delta| {
                    if !delta.is_empty() {
                        report(sandbox_id, machine_id, &delta, VmmState::Running);
                    }
                });
            }
        }
    }

    fn drain(&mut self, mut observe: impl FnMut(MetricsDelta)) {
        while self.budget.allows_read(Instant::now()) && self.read(&mut observe) {}
        self.pipe = None;
        self.stream.finish();
    }

    // Returns false once the pipe has nothing more to read for now.
    fn read(&mut self, observe: impl FnMut(MetricsDelta)) -> bool {
        use std::io::Read;

        let Some(pipe) = self.pipe.as_mut() else {
            return false;
        };
        match pipe.read(&mut self.buffer) {
            // Every writer has closed: the VMM exited or closed its end.
            Ok(0) => {
                self.pipe = None;
                false
            }
            Ok(read) => {
                self.budget.spend(read);
                let (records, malformed) = (self.stream.records, self.stream.malformed);
                self.stream.push(&self.buffer[..read], observe);
                if self.stream.records > records {
                    tracing::debug!(
                        machine_id = self.machine_id,
                        records = self.stream.records,
                        "observed Firecracker metrics record"
                    );
                }
                if malformed == 0 && self.stream.malformed > 0 {
                    tracing::warn!(
                        machine_id = self.machine_id,
                        "discarded a malformed Firecracker metrics record"
                    );
                }
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => true,
            Err(error) => {
                tracing::warn!(machine_id = self.machine_id, %error, "failed reading Firecracker metrics");
                self.pipe = None;
                false
            }
        }
    }
}

#[cfg(test)]
#[path = "firecracker_metrics_tests.rs"]
mod tests;
