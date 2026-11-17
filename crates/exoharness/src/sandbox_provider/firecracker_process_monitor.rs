//! Host-side monitoring of jailed Firecracker processes.
//!
//! Once the jailer has started it, Firecracker never execs, never creates a
//! process, opens files only inside its jail, and creates only `AF_UNIX`
//! stream sockets: its default seccomp filter permits nothing else, and the
//! jailer pivots it into a private root. This monitor watches each VMM from
//! outside the jail for anything else and reports it as a sandbox signal.
//!
//! One listener on the kernel's process events connector receives fork and
//! exec events for the whole host. A socket filter drops thread creation,
//! exits, and credential changes in the kernel, so unrelated host activity
//! costs one hash lookup per process created or exec. A periodic scan of each
//! VMM's executable, cgroup, and descriptors catches what events cannot
//! attribute, such as a `CLONE_PARENT` child, and repairs state after the
//! kernel drops events.
//!
//! https://github.com/torvalds/linux/blob/master/drivers/connector/cn_proc.c
//! https://github.com/firecracker-microvm/firecracker/blob/main/docs/jailer.md
//! https://github.com/firecracker-microvm/firecracker/blob/main/docs/seccomp.md

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use rustix::fs::{AtFlags, CWD, StatxFlags, getxattr, statx};
use rustix::io::Errno;
use rustix::net::netlink::{self, SocketAddrNetlink};
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketAddrAny, SocketFlags, SocketType, bind, recvfrom,
    sendto, socket_with, sockopt,
};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

use super::MonitoredVmm;
use crate::SandboxId;
use crate::sandbox_signal::sandbox_signal;

// Constants and layout from include/uapi/linux/connector.h and cn_proc.h.
// https://github.com/torvalds/linux/blob/master/include/uapi/linux/cn_proc.h
const CN_IDX_PROC: u32 = 1;
const CN_VAL_PROC: u32 = 1;
const PROC_CN_MCAST_LISTEN: u32 = 1;
const PROC_CN_MCAST_IGNORE: u32 = 2;
const PROC_EVENT_NONE: u32 = 0;
const PROC_EVENT_FORK: u32 = 1;
const PROC_EVENT_EXEC: u32 = 2;
const NLMSG_DONE: u16 = 3;
// Every datagram is a netlink header, a connector header, and one proc_event.
const NLMSG_HEADER_BYTES: usize = 16;
const CN_MSG_BYTES: usize = 20;
const CN_ACK_OFFSET: usize = NLMSG_HEADER_BYTES + 12;
const EVENT_OFFSET: usize = NLMSG_HEADER_BYTES + CN_MSG_BYTES;
const EVENT_DATA_OFFSET: usize = EVENT_OFFSET + 16;
const FORK_CHILD_PID_OFFSET: usize = EVENT_DATA_OFFSET + 8;
const FORK_CHILD_TGID_OFFSET: usize = EVENT_DATA_OFFSET + 12;
const DATAGRAM_BYTES: usize = 256;

const SOURCE_EVENTS: &str = "events";
const SOURCE_SCAN: &str = "scan";
const SCAN_INTERVAL: Duration = Duration::from_secs(5);
const ACK_TIMEOUT: Duration = Duration::from_secs(2);
// The kernel queues each event as its own skb of roughly 1 KiB, so this holds
// several thousand process creations while the monitor thread is descheduled.
const RECEIVE_BUFFER_BYTES: usize = 8 * 1024 * 1024;
const EVENT_BATCH: usize = 256;
const SIGNALS_PER_VMM: u32 = 64;
const MAX_TARGET_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProcEvent {
    Fork {
        parent_pid: i32,
        parent_tgid: i32,
        child_pid: i32,
        child_tgid: i32,
    },
    Exec {
        tgid: i32,
    },
    Ack {
        ack: u32,
        err: u32,
    },
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_i32(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn parse_event(datagram: &[u8]) -> Option<ProcEvent> {
    let length = usize::try_from(read_u32(datagram, 0)?).ok()?;
    if length > datagram.len()
        || length < EVENT_DATA_OFFSET + 16
        || read_u32(datagram, NLMSG_HEADER_BYTES)? != CN_IDX_PROC
        || read_u32(datagram, NLMSG_HEADER_BYTES + 4)? != CN_VAL_PROC
    {
        return None;
    }
    let datagram = &datagram[..length];
    match read_u32(datagram, EVENT_OFFSET)? {
        PROC_EVENT_FORK => Some(ProcEvent::Fork {
            parent_pid: read_i32(datagram, EVENT_DATA_OFFSET)?,
            parent_tgid: read_i32(datagram, EVENT_DATA_OFFSET + 4)?,
            child_pid: read_i32(datagram, FORK_CHILD_PID_OFFSET)?,
            child_tgid: read_i32(datagram, FORK_CHILD_TGID_OFFSET)?,
        }),
        PROC_EVENT_EXEC => Some(ProcEvent::Exec {
            tgid: read_i32(datagram, EVENT_DATA_OFFSET + 4)?,
        }),
        PROC_EVENT_NONE => Some(ProcEvent::Ack {
            ack: read_u32(datagram, CN_ACK_OFFSET)?,
            err: read_u32(datagram, EVENT_DATA_OFFSET)?,
        }),
        _ => None,
    }
}

fn control_message(operation: u32, ack: u32) -> [u8; EVENT_OFFSET + 4] {
    const LENGTH: usize = EVENT_OFFSET + 4;
    let mut message = [0; LENGTH];
    message[0..4].copy_from_slice(&(LENGTH as u32).to_ne_bytes());
    message[4..6].copy_from_slice(&NLMSG_DONE.to_ne_bytes());
    message[16..20].copy_from_slice(&CN_IDX_PROC.to_ne_bytes());
    message[20..24].copy_from_slice(&CN_VAL_PROC.to_ne_bytes());
    message[CN_ACK_OFFSET..CN_ACK_OFFSET + 4].copy_from_slice(&ack.to_ne_bytes());
    message[32..34].copy_from_slice(&4_u16.to_ne_bytes());
    message[EVENT_OFFSET..].copy_from_slice(&operation.to_ne_bytes());
    message
}

// Keep process creation (not threads), exec, and subscription acknowledgements.
// Classic BPF absolute loads are big-endian while proc_event fields are native,
// so constants are byte-swapped the same way the loads are.
// https://www.kernel.org/doc/html/latest/networking/filter.html
fn event_filter() -> [libc::sock_filter; 10] {
    let native = |value: u32| u32::from_be_bytes(value.to_ne_bytes());
    let statement = |code: u32, k: u32| libc::sock_filter {
        code: code as u16,
        jt: 0,
        jf: 0,
        k,
    };
    let jump = |code: u32, k: u32, jt: u8, jf: u8| libc::sock_filter {
        code: code as u16,
        jt,
        jf,
        k,
    };
    let load = |offset: usize| statement(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset as u32);
    let equal = libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K;
    [
        load(EVENT_OFFSET),
        jump(equal, native(PROC_EVENT_FORK), 2, 0),
        jump(equal, native(PROC_EVENT_EXEC), 5, 0),
        jump(equal, native(PROC_EVENT_NONE), 4, 5),
        load(FORK_CHILD_PID_OFFSET),
        statement(libc::BPF_MISC | libc::BPF_TAX, 0),
        load(FORK_CHILD_TGID_OFFSET),
        jump(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_X, 0, 0, 1),
        statement(libc::BPF_RET | libc::BPF_K, u32::MAX),
        statement(libc::BPF_RET | libc::BPF_K, 0),
    ]
}

fn attach_event_filter(socket: &OwnedFd) -> Result<()> {
    let mut program = event_filter();
    let filter = libc::sock_fprog {
        len: program.len() as u16,
        filter: program.as_mut_ptr(),
    };
    // SAFETY: `filter` points at `program`, which outlives the call, and the
    // kernel copies the program before setsockopt returns.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_ATTACH_FILTER,
            (&raw const filter).cast(),
            size_of::<libc::sock_fprog>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("attaching the process events socket filter");
    }
    Ok(())
}

fn from_kernel(address: Option<SocketAddrAny>) -> bool {
    address
        .and_then(|address| SocketAddrNetlink::try_from(address).ok())
        .is_some_and(|address| address.pid() == 0)
}

fn subscribe(receive_buffer_bytes: usize) -> Result<OwnedFd> {
    let socket = socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
        Some(netlink::CONNECTOR),
    )
    .context("opening the process events connector")?;
    sockopt::set_socket_recv_buffer_size_force(&socket, receive_buffer_bytes)
        .context("sizing the process events receive buffer")?;
    attach_event_filter(&socket)?;
    bind(&socket, &SocketAddrNetlink::new(0, CN_IDX_PROC))
        .context("joining the process events multicast group")?;
    let ack = std::process::id();
    let request = control_message(PROC_CN_MCAST_LISTEN, ack);
    let send = || {
        sendto(
            &socket,
            &request,
            SendFlags::empty(),
            &SocketAddrNetlink::new(0, 0),
        )
        .context(
            "subscribing to process events; the controller must run in the initial network namespace",
        )
    };
    send()?;
    // The kernel silently ignores subscribers outside the initial user and PID
    // namespaces, so only its acknowledgement proves events will arrive.
    let deadline = Instant::now() + ACK_TIMEOUT;
    let mut datagram = [0; DATAGRAM_BYTES];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!(
                "the kernel did not acknowledge the process events subscription; the controller must run in the initial user and PID namespaces"
            );
        }
        let timeout = Timespec::try_from(remaining).context("process events timeout")?;
        match poll(&mut [PollFd::new(&socket, PollFlags::IN)], Some(&timeout)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => return Err(error).context("waiting for process events"),
        }
        match recvfrom(&socket, &mut datagram[..], RecvFlags::empty()) {
            Ok((length, _, address)) => {
                if !from_kernel(address) {
                    continue;
                }
                if let Some(ProcEvent::Ack { ack: reply, err }) = parse_event(&datagram[..length])
                    && reply == ack.wrapping_add(1)
                {
                    ensure!(
                        err == 0,
                        "the kernel rejected the process events subscription: {}",
                        Errno::from_raw_os_error(err as i32)
                    );
                    return Ok(socket);
                }
            }
            // Repeating LISTEN on this socket is idempotent and asks for a
            // fresh acknowledgement after an overflow discarded the first.
            Err(Errno::NOBUFS) => {
                send()?;
            }
            Err(Errno::AGAIN | Errno::INTR) => {}
            Err(error) => return Err(error).context("reading process events"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Image {
    id: FileId,
    path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Baseline {
    pid: i32,
    exe: FileId,
    jail_mount: u64,
}

fn capped(mut text: String) -> String {
    if text.len() > MAX_TARGET_BYTES {
        let mut end = MAX_TARGET_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn process_image(pid: i32) -> std::io::Result<Image> {
    let path = format!("/proc/{pid}/exe");
    let metadata = fs::metadata(&path)?;
    let target = fs::read_link(&path)?;
    Ok(Image {
        id: FileId {
            dev: metadata.dev(),
            ino: metadata.ino(),
        },
        path: capped(target.to_string_lossy().into_owned()),
    })
}

fn process_baseline(pid: i32) -> std::io::Result<Baseline> {
    let image = process_image(pid)?;
    let root = statx(
        CWD,
        format!("/proc/{pid}/root"),
        AtFlags::empty(),
        StatxFlags::MNT_ID,
    )?;
    if !StatxFlags::from_bits_retain(root.stx_mask).contains(StatxFlags::MNT_ID) {
        return Err(std::io::Error::other(
            "the kernel does not report mount IDs",
        ));
    }
    Ok(Baseline {
        pid,
        exe: image.id,
        jail_mount: root.stx_mnt_id,
    })
}

fn open_process(pid: i32) -> std::io::Result<OwnedFd> {
    let pid = Pid::from_raw(pid).ok_or(Errno::INVAL)?;
    Ok(pidfd_open(pid, PidfdFlags::empty())?)
}

fn process_exited(handle: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(handle, PollFlags::IN)];
    match poll(&mut fds, Some(&Timespec::default())) {
        Ok(ready) => ready > 0,
        Err(error) => {
            tracing::warn!(%error, "failed polling a Firecracker process handle");
            false
        }
    }
}

fn cgroup_members(cgroup: &Path) -> std::io::Result<Vec<i32>> {
    Ok(fs::read_to_string(cgroup.join("cgroup.procs"))?
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect())
}

trait Host {
    fn open(&self, pid: i32) -> Option<Arc<OwnedFd>>;
    fn baseline(&self, pid: i32) -> Option<Baseline>;
    fn image(&self, pid: i32) -> Option<Image>;
}

struct LiveHost;

impl Host for LiveHost {
    fn open(&self, pid: i32) -> Option<Arc<OwnedFd>> {
        open_process(pid).ok().map(Arc::new)
    }

    fn baseline(&self, pid: i32) -> Option<Baseline> {
        process_baseline(pid).ok()
    }

    fn image(&self, pid: i32) -> Option<Image> {
        process_image(pid).ok()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FdFinding {
    OutsideJail { target: String },
    Socket { target: String, protocol: String },
    Unexpected { target: String },
}

impl FdFinding {
    fn target(&self) -> &str {
        match self {
            Self::OutsideJail { target }
            | Self::Socket { target, .. }
            | Self::Unexpected { target } => target,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum LinkKind<'a> {
    Socket,
    AnonInode(&'a str),
    Memfd,
    Path,
    Other,
}

fn link_kind(target: &str) -> LinkKind<'_> {
    if target.starts_with("socket:[") {
        LinkKind::Socket
    } else if let Some(name) = target.strip_prefix("anon_inode:") {
        LinkKind::AnonInode(name)
    } else if target.starts_with("/memfd:") && target.ends_with(" (deleted)") {
        LinkKind::Memfd
    } else if target.starts_with('/') {
        LinkKind::Path
    } else {
        LinkKind::Other
    }
}

// Firecracker's seccomp filter lets it create eventfds, timerfds, io_uring
// instances, and KVM objects after startup; it creates epoll instances and,
// for UFFD snapshot restores, a userfaultfd before the filter is installed.
fn expected_anon_inode(name: &str) -> bool {
    matches!(
        name,
        "[eventfd]" | "[eventpoll]" | "[timerfd]" | "[io_uring]" | "[userfaultfd]"
    ) || name.starts_with("kvm-")
}

fn is_dev_null(stat: &rustix::fs::Statx) -> bool {
    u32::from(stat.stx_mode) & libc::S_IFMT == libc::S_IFCHR
        && stat.stx_rdev_major == 1
        && stat.stx_rdev_minor == 3
}

fn classify_fd(path: &Path, target: String, jail_mount: u64) -> Option<FdFinding> {
    match link_kind(&target) {
        LinkKind::Socket => {
            let mut name = [0_u8; 32];
            let length = getxattr(path, "system.sockprotoname", &mut name[..]).ok()?;
            let protocol = String::from_utf8_lossy(&name[..length])
                .trim_end_matches('\0')
                .to_string();
            (!protocol.starts_with("UNIX")).then(|| FdFinding::Socket {
                target: capped(target),
                protocol,
            })
        }
        LinkKind::AnonInode(name) => (!expected_anon_inode(name)).then(|| FdFinding::Unexpected {
            target: capped(target),
        }),
        LinkKind::Memfd => None,
        LinkKind::Path => {
            let stat = statx(
                CWD,
                path,
                AtFlags::empty(),
                StatxFlags::TYPE | StatxFlags::MNT_ID,
            )
            .ok()?;
            // Exo hands Firecracker /dev/null as stdio from outside the jail.
            (stat.stx_mnt_id != jail_mount && !is_dev_null(&stat)).then(|| FdFinding::OutsideJail {
                target: capped(target),
            })
        }
        LinkKind::Other => Some(FdFinding::Unexpected {
            target: capped(target),
        }),
    }
}

fn inspect_fds(pid: i32, jail_mount: u64) -> Vec<(i32, FdFinding)> {
    let directory = PathBuf::from(format!("/proc/{pid}/fd"));
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(pid, %error, "failed listing Firecracker descriptors");
            return Vec::new();
        }
    };
    let mut findings = Vec::new();
    for entry in entries.flatten() {
        let Some(fd) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        else {
            continue;
        };
        let path = entry.path();
        // A descriptor closed between listing and reading is not a finding.
        let Ok(target) = fs::read_link(&path) else {
            continue;
        };
        if let Some(finding) = classify_fd(&path, target.to_string_lossy().into_owned(), jail_mount)
        {
            findings.push((fd, finding));
        }
    }
    findings
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JailerDeviation {
    ExecAgain,
    ForkedAgain,
    ChildForked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoverageGap {
    Overflow,
    MonitorStopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Detail {
    Exec {
        pid: i32,
        image: Option<String>,
    },
    Fork {
        pid: i32,
        child: i32,
    },
    Fd {
        pid: i32,
        fd: i32,
        finding: FdFinding,
    },
    Launch {
        pid: i32,
        deviation: JailerDeviation,
    },
    EventsLost(CoverageGap),
    Suppressed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Emitted {
    sandbox_id: SandboxId,
    machine_id: String,
    source: &'static str,
    detail: Detail,
}

fn emit(signal: &Emitted) {
    let sandbox_id = &signal.sandbox_id;
    let machine_id = signal.machine_id.as_str();
    let detector = signal.source;
    match &signal.detail {
        Detail::Exec {
            pid,
            image: Some(image),
        } => sandbox_signal!(
            exploit,
            "sandbox.vmm_exec",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            pid = *pid,
            detector,
            image = image.as_str(),
        ),
        Detail::Exec { pid, image: None } => sandbox_signal!(
            exploit,
            "sandbox.vmm_exec",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            pid = *pid,
            detector,
        ),
        Detail::Fork { pid, child } => sandbox_signal!(
            signal,
            "sandbox.vmm_fork",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            pid = *pid,
            detector,
            child_pid = *child,
        ),
        Detail::Fd {
            pid,
            fd,
            finding: FdFinding::OutsideJail { target },
        } => sandbox_signal!(
            signal,
            "sandbox.vmm_fd",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            pid = *pid,
            detector,
            fd = *fd,
            reason = "outside_jail",
            target = target.as_str(),
        ),
        Detail::Fd {
            pid,
            fd,
            finding: FdFinding::Socket { target, protocol },
        } => sandbox_signal!(
            signal,
            "sandbox.vmm_fd",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            pid = *pid,
            detector,
            fd = *fd,
            reason = "socket_family",
            target = target.as_str(),
            protocol = protocol.as_str(),
        ),
        Detail::Fd {
            pid,
            fd,
            finding: FdFinding::Unexpected { target },
        } => sandbox_signal!(
            signal,
            "sandbox.vmm_fd",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            pid = *pid,
            detector,
            fd = *fd,
            reason = "unexpected_kind",
            target = target.as_str(),
        ),
        Detail::Launch { pid, deviation } => sandbox_signal!(
            signal,
            "sandbox.vmm_launch",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            pid = *pid,
            detector,
            reason = match deviation {
                JailerDeviation::ExecAgain => "jailer_exec_again",
                JailerDeviation::ForkedAgain => "jailer_forked_again",
                JailerDeviation::ChildForked => "jailer_child_forked",
            },
        ),
        Detail::EventsLost(gap) => sandbox_signal!(
            signal,
            "sandbox.vmm_events_lost",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            detector,
            reason = match gap {
                CoverageGap::Overflow => "overflow",
                CoverageGap::MonitorStopped => "monitor_stopped",
            },
        ),
        Detail::Suppressed => sandbox_signal!(
            signal,
            "sandbox.vmm_signals_suppressed",
            sandbox_id = sandbox_id,
            source = "exo.firecracker.process_monitor",
            machine_id,
            detector,
            limit = SIGNALS_PER_VMM,
        ),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Launching { unsynced: bool },
    Running(Baseline),
}

struct Instance {
    vmm: MonitoredVmm,
    phase: Phase,
    remaining_signals: u32,
    reported_fds: HashSet<String>,
}

impl Instance {
    fn new(vmm: MonitoredVmm, phase: Phase) -> Self {
        Self {
            vmm,
            phase,
            remaining_signals: SIGNALS_PER_VMM,
            reported_fds: HashSet::new(),
        }
    }

    fn report(&mut self, out: &mut Vec<Emitted>, source: &'static str, detail: Detail) {
        if self.remaining_signals == 0 {
            return;
        }
        self.remaining_signals -= 1;
        let emitted = |detail| Emitted {
            sandbox_id: self.vmm.sandbox_id.clone(),
            machine_id: self.vmm.machine_id.clone(),
            source,
            detail,
        };
        out.push(emitted(detail));
        if self.remaining_signals == 0 {
            out.push(emitted(Detail::Suppressed));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    // Exo spawned this process to exec the jailer. `confirmed` records whether
    // the fork event naming `spawner` as its parent has been handled.
    Jailer {
        spawner: i32,
        confirmed: bool,
        execs: u32,
        children: u32,
    },
    // The jailer's clone, which must exec Firecracker exactly once.
    JailerChild,
    Vmm,
    Descendant,
}

struct Process {
    instance: u64,
    role: Role,
    handle: Option<Arc<OwnedFd>>,
}

struct ScanTarget {
    instance: u64,
    baseline: Baseline,
    handle: Arc<OwnedFd>,
    cgroup: PathBuf,
    known: HashSet<i32>,
}

struct Member {
    pid: i32,
    image: Option<Image>,
    handle: Arc<OwnedFd>,
}

struct Inspection {
    image: Option<Image>,
    members: Vec<Member>,
    fds: Vec<(i32, FdFinding)>,
}

struct Resync {
    instance: u64,
    pid_file: PathBuf,
    cgroup: PathBuf,
}

struct Resolved {
    instance: u64,
    baseline: Baseline,
    handle: Arc<OwnedFd>,
}

struct Tracker {
    own_tgid: i32,
    next_instance: u64,
    instances: HashMap<u64, Instance>,
    processes: HashMap<i32, Process>,
    // Launches keyed by the exo thread that is about to spawn the jailer. The
    // kernel names that thread as the parent in the jailer's fork event.
    pending: HashMap<i32, u64>,
    failure: Option<String>,
}

impl Tracker {
    fn new(own_tgid: i32) -> Self {
        Self {
            own_tgid,
            next_instance: 0,
            instances: HashMap::new(),
            processes: HashMap::new(),
            pending: HashMap::new(),
            failure: None,
        }
    }

    fn ensure_running(&self) -> Result<()> {
        match &self.failure {
            Some(failure) => bail!("Firecracker process monitor stopped: {failure}"),
            None => Ok(()),
        }
    }

    fn insert_instance(&mut self, vmm: MonitoredVmm, phase: Phase) -> u64 {
        let id = self.next_instance;
        self.next_instance += 1;
        self.instances.insert(id, Instance::new(vmm, phase));
        id
    }

    fn expect_launch(&mut self, vmm: MonitoredVmm, spawner: i32) -> u64 {
        self.forget(&vmm.machine_id);
        let id = self.insert_instance(vmm, Phase::Launching { unsynced: false });
        let previous = self.pending.insert(spawner, id);
        debug_assert!(previous.is_none(), "a thread launched two jailers at once");
        id
    }

    fn spawned(&mut self, id: u64, spawner: i32, jailer: i32, handle: Option<Arc<OwnedFd>>) {
        if self.pending.get(&spawner) != Some(&id) {
            // The jailer's fork event already registered it.
            return;
        }
        self.pending.remove(&spawner);
        self.processes.insert(
            jailer,
            Process {
                instance: id,
                role: Role::Jailer {
                    spawner,
                    confirmed: false,
                    execs: 0,
                    children: 0,
                },
                handle,
            },
        );
    }

    fn adopt(&mut self, vmm: MonitoredVmm, baseline: Baseline, handle: Arc<OwnedFd>) -> bool {
        if self.tracks(&vmm.machine_id) {
            return false;
        }
        let id = self.insert_instance(vmm, Phase::Running(baseline));
        self.processes.insert(
            baseline.pid,
            Process {
                instance: id,
                role: Role::Vmm,
                handle: Some(handle),
            },
        );
        true
    }

    fn tracks(&self, machine_id: &str) -> bool {
        self.instances
            .values()
            .any(|instance| instance.vmm.machine_id == machine_id)
    }

    fn remove_instance(&mut self, id: u64) {
        self.instances.remove(&id);
        self.processes.retain(|_, process| process.instance != id);
        self.pending.retain(|_, instance| *instance != id);
    }

    fn forget(&mut self, machine_id: &str) {
        let ids = self
            .instances
            .iter()
            .filter(|(_, instance)| instance.vmm.machine_id == machine_id)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ids {
            self.remove_instance(id);
        }
    }

    fn handle(&mut self, event: ProcEvent, host: &impl Host, out: &mut Vec<Emitted>) {
        match event {
            ProcEvent::Fork {
                parent_pid,
                parent_tgid,
                child_pid,
                child_tgid,
            } if child_pid == child_tgid => {
                self.fork(parent_pid, parent_tgid, child_tgid, host, out)
            }
            ProcEvent::Exec { tgid } => self.exec(tgid, host, out),
            ProcEvent::Fork { .. } | ProcEvent::Ack { .. } => {}
        }
    }

    fn fork(
        &mut self,
        parent_pid: i32,
        parent_tgid: i32,
        child: i32,
        host: &impl Host,
        out: &mut Vec<Emitted>,
    ) {
        if let Some(existing) = self.processes.get_mut(&child) {
            if let Role::Jailer {
                spawner,
                confirmed: confirmed @ false,
                ..
            } = &mut existing.role
                && *spawner == parent_pid
                && parent_tgid == self.own_tgid
            {
                *confirmed = true;
                return;
            }
            // A live process keeps its PID, so a new process with a tracked
            // PID means the tracked one exited and the kernel reused the PID.
            self.processes.remove(&child);
        }
        if parent_tgid == self.own_tgid {
            if let Some(id) = self.pending.remove(&parent_pid) {
                self.processes.insert(
                    child,
                    Process {
                        instance: id,
                        role: Role::Jailer {
                            spawner: parent_pid,
                            confirmed: true,
                            execs: 0,
                            children: 0,
                        },
                        handle: host.open(child),
                    },
                );
            }
            return;
        }
        let Some(parent) = self.processes.get_mut(&parent_tgid) else {
            return;
        };
        let id = parent.instance;
        let Some(instance) = self.instances.get_mut(&id) else {
            return;
        };
        let role = match &mut parent.role {
            Role::Jailer { children, .. } => {
                *children += 1;
                if *children == 1 {
                    Role::JailerChild
                } else {
                    instance.report(
                        out,
                        SOURCE_EVENTS,
                        Detail::Launch {
                            pid: parent_tgid,
                            deviation: JailerDeviation::ForkedAgain,
                        },
                    );
                    return;
                }
            }
            Role::JailerChild => {
                instance.report(
                    out,
                    SOURCE_EVENTS,
                    Detail::Launch {
                        pid: parent_tgid,
                        deviation: JailerDeviation::ChildForked,
                    },
                );
                return;
            }
            Role::Vmm | Role::Descendant => {
                instance.report(
                    out,
                    SOURCE_EVENTS,
                    Detail::Fork {
                        pid: parent_tgid,
                        child,
                    },
                );
                Role::Descendant
            }
        };
        self.processes.insert(
            child,
            Process {
                instance: id,
                role,
                handle: host.open(child),
            },
        );
    }

    fn exec(&mut self, tgid: i32, host: &impl Host, out: &mut Vec<Emitted>) {
        let Some(process) = self.processes.get_mut(&tgid) else {
            return;
        };
        let Some(instance) = self.instances.get_mut(&process.instance) else {
            return;
        };
        match &mut process.role {
            Role::Jailer { execs, .. } => {
                *execs += 1;
                if *execs > 1 {
                    instance.report(
                        out,
                        SOURCE_EVENTS,
                        Detail::Launch {
                            pid: tgid,
                            deviation: JailerDeviation::ExecAgain,
                        },
                    );
                }
            }
            Role::JailerChild => {
                process.role = Role::Vmm;
                // The jailer execs Firecracker before any guest code runs, so
                // the image and root recorded now are Firecracker's own.
                if let Some(baseline) = host.baseline(tgid) {
                    instance.phase = Phase::Running(baseline);
                    tracing::debug!(
                        machine_id = %instance.vmm.machine_id,
                        pid = tgid,
                        "monitoring Firecracker VMM"
                    );
                }
            }
            Role::Vmm | Role::Descendant => {
                let image = host.image(tgid);
                if let Phase::Running(baseline) = &mut instance.phase
                    && baseline.pid == tgid
                    && let Some(image) = &image
                {
                    baseline.exe = image.id;
                }
                instance.report(
                    out,
                    SOURCE_EVENTS,
                    Detail::Exec {
                        pid: tgid,
                        image: image.map(|image| image.path),
                    },
                );
            }
        }
    }

    fn coverage_lost(&mut self, gap: CoverageGap, out: &mut Vec<Emitted>) {
        for instance in self.instances.values_mut() {
            if let Phase::Launching { unsynced } = &mut instance.phase {
                *unsynced = true;
            }
            instance.report(out, SOURCE_EVENTS, Detail::EventsLost(gap));
        }
    }

    fn exited(&self) -> Vec<(i32, Arc<OwnedFd>)> {
        self.processes
            .iter()
            .filter_map(|(pid, process)| {
                let handle = process.handle.as_ref()?;
                process_exited(handle).then(|| (*pid, Arc::clone(handle)))
            })
            .collect()
    }

    fn prune(&mut self, exited: &[(i32, Arc<OwnedFd>)]) {
        for (pid, handle) in exited {
            if self.processes.get(pid).is_some_and(|process| {
                process
                    .handle
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, handle))
            }) {
                self.processes.remove(pid);
            }
        }
        let live = self
            .processes
            .values()
            .map(|process| process.instance)
            .collect::<HashSet<_>>();
        self.instances.retain(|id, instance| {
            matches!(instance.phase, Phase::Launching { .. }) || live.contains(id)
        });
    }

    fn scan_targets(&self) -> Vec<ScanTarget> {
        self.instances
            .iter()
            .filter_map(|(id, instance)| {
                let Phase::Running(baseline) = instance.phase else {
                    return None;
                };
                let handle = self.processes.get(&baseline.pid)?.handle.clone()?;
                Some(ScanTarget {
                    instance: *id,
                    baseline,
                    handle,
                    cgroup: instance.vmm.cgroup.clone(),
                    known: self
                        .processes
                        .iter()
                        .filter(|(_, process)| process.instance == *id)
                        .map(|(pid, _)| *pid)
                        .collect(),
                })
            })
            .collect()
    }

    fn resync_targets(&self) -> Vec<Resync> {
        self.instances
            .iter()
            .filter(|(_, instance)| instance.phase == Phase::Launching { unsynced: true })
            .map(|(id, instance)| Resync {
                instance: *id,
                pid_file: instance.vmm.pid_file.clone(),
                cgroup: instance.vmm.cgroup.clone(),
            })
            .collect()
    }

    fn resolve(&mut self, resolved: Resolved) {
        let Some(instance) = self.instances.get_mut(&resolved.instance) else {
            return;
        };
        if instance.phase != (Phase::Launching { unsynced: true }) {
            return;
        }
        instance.phase = Phase::Running(resolved.baseline);
        self.processes.insert(
            resolved.baseline.pid,
            Process {
                instance: resolved.instance,
                role: Role::Vmm,
                handle: Some(resolved.handle),
            },
        );
    }

    fn apply_scan(&mut self, target: &ScanTarget, inspection: Inspection, out: &mut Vec<Emitted>) {
        let Some(instance) = self.instances.get_mut(&target.instance) else {
            return;
        };
        let Phase::Running(baseline) = &mut instance.phase else {
            return;
        };
        if baseline.pid != target.baseline.pid {
            return;
        }
        let pid = baseline.pid;
        if let Some(image) = inspection.image
            && image.id != baseline.exe
        {
            baseline.exe = image.id;
            instance.report(
                out,
                SOURCE_SCAN,
                Detail::Exec {
                    pid,
                    image: Some(image.path),
                },
            );
        }
        let Phase::Running(baseline) = instance.phase else {
            return;
        };
        for member in inspection.members {
            if self.processes.contains_key(&member.pid) {
                continue;
            }
            instance.report(
                out,
                SOURCE_SCAN,
                Detail::Fork {
                    pid,
                    child: member.pid,
                },
            );
            if let Some(image) = member.image
                && image.id != baseline.exe
            {
                instance.report(
                    out,
                    SOURCE_SCAN,
                    Detail::Exec {
                        pid: member.pid,
                        image: Some(image.path),
                    },
                );
            }
            self.processes.insert(
                member.pid,
                Process {
                    instance: target.instance,
                    role: Role::Descendant,
                    handle: Some(member.handle),
                },
            );
        }
        for (fd, finding) in inspection.fds {
            if instance.remaining_signals > 0
                && instance.reported_fds.insert(finding.target().to_string())
            {
                instance.report(out, SOURCE_SCAN, Detail::Fd { pid, fd, finding });
            }
        }
    }
}

fn read_members(target: &ScanTarget) -> Vec<i32> {
    match cgroup_members(&target.cgroup) {
        Ok(members) => members,
        Err(error) => {
            tracing::warn!(cgroup = %target.cgroup.display(), %error, "failed reading Firecracker cgroup members");
            Vec::new()
        }
    }
}

// Unknown cgroup members are processes the VMM created. Each is attributed
// only if it is still a member and alive after its handle was opened, so a
// member that exited cannot lend its PID, and its image, to another process.
fn inspect_members(target: &ScanTarget) -> Vec<Member> {
    let opened = read_members(target)
        .into_iter()
        .filter(|pid| !target.known.contains(pid))
        .filter_map(|pid| {
            let handle = open_process(pid).ok()?;
            Some((pid, handle, process_image(pid).ok()))
        })
        .collect::<Vec<_>>();
    if opened.is_empty() {
        return Vec::new();
    }
    let members = read_members(target);
    opened
        .into_iter()
        .filter(|(pid, handle, _)| members.contains(pid) && !process_exited(handle))
        .map(|(pid, handle, image)| Member {
            pid,
            image,
            handle: Arc::new(handle),
        })
        .collect()
}

fn inspect(target: &ScanTarget) -> Inspection {
    let pid = target.baseline.pid;
    Inspection {
        image: process_image(pid).ok(),
        members: inspect_members(target),
        fds: inspect_fds(pid, target.baseline.jail_mount),
    }
}

fn resolve_launch(resync: Resync) -> Option<Resolved> {
    let pid = fs::read_to_string(&resync.pid_file)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if !cgroup_members(&resync.cgroup).ok()?.contains(&pid) {
        return None;
    }
    let handle = Arc::new(open_process(pid).ok()?);
    let baseline = process_baseline(pid).ok()?;
    (!process_exited(&handle)).then_some(Resolved {
        instance: resync.instance,
        baseline,
        handle,
    })
}

struct State {
    tracker: Mutex<Tracker>,
    wake: OwnedFd,
    stopping: AtomicBool,
    scan_requested: AtomicBool,
    #[cfg(test)]
    received: std::sync::atomic::AtomicU64,
}

impl State {
    fn lock(&self) -> MutexGuard<'_, Tracker> {
        self.tracker
            .lock()
            .expect("Firecracker process monitor state must not be poisoned")
    }

    fn wake(&self) {
        if let Err(error) = rustix::io::write(&self.wake, &1_u64.to_ne_bytes()) {
            tracing::error!(%error, "failed waking the Firecracker process monitor");
        }
    }

    fn fail(&self, reason: String) {
        let mut out = Vec::new();
        {
            let mut tracker = self.tracker.lock().unwrap_or_else(PoisonError::into_inner);
            if tracker.failure.is_some() {
                return;
            }
            tracker.coverage_lost(CoverageGap::MonitorStopped, &mut out);
            tracker.failure = Some(reason.clone());
        }
        tracing::error!(reason, "Firecracker process monitor stopped");
        out.iter().for_each(emit);
    }
}

pub(super) struct ProcessMonitor {
    shared: Arc<State>,
    worker: Option<JoinHandle<()>>,
}

pub(super) struct Launch<'a> {
    monitor: &'a ProcessMonitor,
    instance: u64,
    spawner: i32,
    spawned: bool,
}

impl Launch<'_> {
    pub(super) fn spawned(mut self, jailer_pid: u32) {
        let jailer = jailer_pid.cast_signed();
        let handle = LiveHost.open(jailer);
        self.monitor
            .shared
            .lock()
            .spawned(self.instance, self.spawner, jailer, handle);
        self.spawned = true;
    }
}

impl Drop for Launch<'_> {
    fn drop(&mut self) {
        if !self.spawned {
            self.monitor.shared.lock().remove_instance(self.instance);
        }
    }
}

impl ProcessMonitor {
    pub(super) fn start() -> Result<Self> {
        Self::start_with(SCAN_INTERVAL, RECEIVE_BUFFER_BYTES)
    }

    fn start_with(scan_interval: Duration, receive_buffer_bytes: usize) -> Result<Self> {
        process_baseline(std::process::id().cast_signed())
            .context("Firecracker process monitoring requires statx mount IDs (Linux 5.8)")?;
        let socket = subscribe(receive_buffer_bytes)?;
        let shared = Arc::new(State {
            tracker: Mutex::new(Tracker::new(std::process::id().cast_signed())),
            wake: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                .context("creating the process monitor wake descriptor")?,
            stopping: AtomicBool::new(false),
            scan_requested: AtomicBool::new(false),
            #[cfg(test)]
            received: std::sync::atomic::AtomicU64::new(0),
        });
        let worker = std::thread::Builder::new()
            .name("exo-fc-monitor".into())
            .spawn({
                let shared = Arc::clone(&shared);
                move || {
                    let _exit = WorkerExit(&shared);
                    run(&shared, &socket, scan_interval);
                }
            })
            .context("starting the Firecracker process monitor thread")?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    #[cfg(test)]
    pub(super) fn inert() -> Self {
        Self {
            shared: Arc::new(State {
                tracker: Mutex::new(Tracker::new(std::process::id().cast_signed())),
                wake: eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)
                    .expect("creating an eventfd"),
                stopping: AtomicBool::new(false),
                scan_requested: AtomicBool::new(false),
                received: std::sync::atomic::AtomicU64::new(0),
            }),
            worker: None,
        }
    }

    // Call immediately before spawning the jailer, on the spawning thread, and
    // spawn nothing else from this thread until `Launch::spawned`.
    pub(super) fn expect_launch(&self, vmm: MonitoredVmm) -> Result<Launch<'_>> {
        let spawner = rustix::thread::gettid().as_raw_nonzero().get();
        let mut tracker = self.shared.lock();
        tracker.ensure_running()?;
        let instance = tracker.expect_launch(vmm, spawner);
        Ok(Launch {
            monitor: self,
            instance,
            spawner,
            spawned: false,
        })
    }

    // Start monitoring a VMM that a previous controller launched. A VMM this
    // monitor already tracks is left as is.
    pub(super) fn adopt(&self, vmm: MonitoredVmm) -> Result<()> {
        {
            let tracker = self.shared.lock();
            tracker.ensure_running()?;
            if tracker.tracks(&vmm.machine_id) {
                return Ok(());
            }
        }
        let pid = fs::read_to_string(&vmm.pid_file)
            .with_context(|| format!("reading Firecracker pid file {}", vmm.pid_file.display()))?
            .trim()
            .parse()
            .context("invalid Firecracker pid")?;
        ensure!(
            cgroup_members(&vmm.cgroup)
                .with_context(|| format!("reading Firecracker cgroup {}", vmm.cgroup.display()))?
                .contains(&pid),
            "Firecracker process {pid} is not in its machine cgroup {}",
            vmm.cgroup.display()
        );
        let handle = Arc::new(open_process(pid).context("opening Firecracker process handle")?);
        let baseline = process_baseline(pid).context("inspecting Firecracker process")?;
        let machine_id = vmm.machine_id.clone();
        if self.shared.lock().adopt(vmm, baseline, handle) {
            tracing::debug!(machine_id, pid, "monitoring adopted Firecracker VMM");
            self.shared.scan_requested.store(true, Ordering::Release);
            self.shared.wake();
        }
        Ok(())
    }

    pub(super) fn forget(&self, machine_id: &str) {
        self.shared.lock().forget(machine_id);
    }
}

impl Drop for ProcessMonitor {
    fn drop(&mut self) {
        let Some(worker) = self.worker.take() else {
            return;
        };
        self.shared.stopping.store(true, Ordering::Release);
        self.shared.wake();
        if worker.join().is_err() {
            tracing::error!("Firecracker process monitor thread panicked");
        }
    }
}

struct WorkerExit<'a>(&'a State);

impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        if !self.0.stopping.load(Ordering::Acquire) {
            self.0.fail("the monitor thread exited unexpectedly".into());
        }
    }
}

fn run(shared: &State, socket: &OwnedFd, scan_interval: Duration) {
    let mut next_scan = Instant::now() + scan_interval;
    loop {
        let timeout = Timespec::try_from(next_scan.saturating_duration_since(Instant::now()))
            .expect("the scan interval fits in a timespec");
        let mut fds = [
            PollFd::new(socket, PollFlags::IN),
            PollFd::new(&shared.wake, PollFlags::IN),
        ];
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(Errno::INTR) => {}
            Err(error) => {
                shared.fail(format!("polling process events: {error}"));
                return;
            }
        }
        if fds[1].revents().contains(PollFlags::IN) {
            match rustix::io::read(&shared.wake, &mut [0_u8; 8][..]) {
                Ok(_) | Err(Errno::AGAIN) => {}
                Err(error) => {
                    shared.fail(format!("reading the wake descriptor: {error}"));
                    return;
                }
            }
        }
        if shared.stopping.load(Ordering::Acquire) {
            unsubscribe(socket);
            return;
        }
        let lost = match drain(shared, socket) {
            Ok(lost) => lost,
            Err(error) => {
                shared.fail(format!("reading process events: {error}"));
                return;
            }
        };
        if lost
            || shared.scan_requested.swap(false, Ordering::AcqRel)
            || Instant::now() >= next_scan
        {
            match scan(shared, socket) {
                Ok(true) => {
                    shared.scan_requested.store(true, Ordering::Release);
                    shared.wake();
                }
                Ok(false) => {}
                Err(error) => {
                    shared.fail(format!("reading process events: {error}"));
                    return;
                }
            }
            next_scan = Instant::now() + scan_interval;
        }
    }
}

fn unsubscribe(socket: &OwnedFd) {
    // Closing the socket does not decrement the kernel's listener count, which
    // keeps every fork and exec on the host paying for event delivery.
    if let Err(error) = sendto(
        socket,
        &control_message(PROC_CN_MCAST_IGNORE, 0),
        SendFlags::empty(),
        &SocketAddrNetlink::new(0, 0),
    ) {
        tracing::warn!(%error, "failed unsubscribing from process events");
    }
}

fn apply(shared: &State, batch: &mut Vec<ProcEvent>) {
    if batch.is_empty() {
        return;
    }
    let mut out = Vec::new();
    {
        let mut tracker = shared.lock();
        for event in batch.drain(..) {
            tracker.handle(event, &LiveHost, &mut out);
        }
    }
    out.iter().for_each(emit);
}

// Handles every queued event. Returns whether the kernel dropped events
// because the receive buffer filled.
fn drain(shared: &State, socket: &OwnedFd) -> Result<bool, Errno> {
    let mut lost = false;
    let mut batch = Vec::with_capacity(EVENT_BATCH);
    let mut datagram = [0; DATAGRAM_BYTES];
    loop {
        match recvfrom(socket, &mut datagram[..], RecvFlags::empty()) {
            Ok((length, _, address)) => {
                if from_kernel(address)
                    && let Some(event) = parse_event(&datagram[..length])
                {
                    #[cfg(test)]
                    shared.received.fetch_add(1, Ordering::Relaxed);
                    batch.push(event);
                    if batch.len() == EVENT_BATCH {
                        apply(shared, &mut batch);
                    }
                }
            }
            Err(Errno::AGAIN) => break,
            Err(Errno::INTR) => {}
            Err(Errno::NOBUFS) => {
                apply(shared, &mut batch);
                lost = true;
                let mut out = Vec::new();
                shared.lock().coverage_lost(CoverageGap::Overflow, &mut out);
                out.iter().for_each(emit);
            }
            Err(error) => {
                apply(shared, &mut batch);
                return Err(error);
            }
        }
    }
    apply(shared, &mut batch);
    Ok(lost)
}

fn scan(shared: &State, socket: &OwnedFd) -> Result<bool, Errno> {
    let started = Instant::now();
    // Collect exits before draining: anything an exited process did is already
    // queued, so it is handled before the process is forgotten.
    let exited = shared.lock().exited();
    let lost = drain(shared, socket)?;
    let (targets, resyncs) = {
        let mut tracker = shared.lock();
        tracker.prune(&exited);
        (tracker.scan_targets(), tracker.resync_targets())
    };
    let scanned = targets.len();
    let inspections = targets
        .into_iter()
        .filter_map(|target| {
            let inspection = inspect(&target);
            // A process that exited during inspection may have released its
            // PID to another process, so its results are not attributable.
            (!process_exited(&target.handle)).then_some((target, inspection))
        })
        .collect::<Vec<_>>();
    let resolved = resyncs
        .into_iter()
        .filter_map(resolve_launch)
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    {
        let mut tracker = shared.lock();
        for resolved in resolved {
            tracker.resolve(resolved);
        }
        for (target, inspection) in inspections {
            tracker.apply_scan(&target, inspection, &mut out);
        }
    }
    out.iter().for_each(emit);
    tracing::debug!(
        vmms = scanned,
        elapsed_us = started.elapsed().as_micros() as u64,
        "scanned Firecracker VMM processes"
    );
    Ok(lost)
}

#[cfg(test)]
#[path = "firecracker_process_monitor_tests.rs"]
mod tests;
