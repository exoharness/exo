use super::*;

const EXO: i32 = 1_000;
const SPAWNER: i32 = 1_001;
const JAILER: i32 = 2_000;
const VMM: i32 = 2_001;
const FIRECRACKER: FileId = FileId { dev: 1, ino: 10 };
const SHELL: FileId = FileId { dev: 1, ino: 20 };

struct FakeHost;

impl Host for FakeHost {
    fn open(&self, _pid: i32) -> Option<Arc<OwnedFd>> {
        None
    }

    fn baseline(&self, pid: i32) -> Option<Baseline> {
        Some(Baseline {
            pid,
            exe: FIRECRACKER,
            jail_mount: 7,
        })
    }

    fn image(&self, _pid: i32) -> Option<Image> {
        Some(Image {
            id: SHELL,
            path: "/bin/sh".into(),
        })
    }
}

fn vmm(machine_id: &str) -> MonitoredVmm {
    MonitoredVmm {
        sandbox_id: format!("sandbox-{machine_id}"),
        machine_id: machine_id.into(),
        pid_file: PathBuf::from("/nonexistent/firecracker.pid"),
        cgroup: PathBuf::from("/nonexistent/cgroup"),
    }
}

fn fork(parent: i32, child: i32) -> ProcEvent {
    ProcEvent::Fork {
        parent_pid: parent,
        parent_tgid: parent,
        child_pid: child,
        child_tgid: child,
    }
}

fn spawn(spawner: i32, child: i32) -> ProcEvent {
    ProcEvent::Fork {
        parent_pid: spawner,
        parent_tgid: EXO,
        child_pid: child,
        child_tgid: child,
    }
}

fn feed(tracker: &mut Tracker, events: &[ProcEvent]) -> Vec<Emitted> {
    let mut out = Vec::new();
    for event in events {
        tracker.handle(*event, &FakeHost, &mut out);
    }
    out
}

fn details(emitted: &[Emitted]) -> Vec<&Detail> {
    emitted.iter().map(|emitted| &emitted.detail).collect()
}

fn launched(machine_id: &str) -> (Tracker, u64) {
    let mut tracker = Tracker::new(EXO);
    let id = tracker.expect_launch(vmm(machine_id), SPAWNER);
    let out = feed(
        &mut tracker,
        &[
            spawn(SPAWNER, JAILER),
            ProcEvent::Exec { tgid: JAILER },
            fork(JAILER, VMM),
            ProcEvent::Exec { tgid: VMM },
        ],
    );
    assert_eq!(details(&out), Vec::<&Detail>::new());
    (tracker, id)
}

#[test]
fn parses_proc_connector_datagrams() {
    let datagram = |what: u32, data: [i32; 4]| {
        let mut bytes = vec![0_u8; EVENT_DATA_OFFSET + 24];
        let length = bytes.len() as u32;
        bytes[0..4].copy_from_slice(&length.to_ne_bytes());
        bytes[16..20].copy_from_slice(&CN_IDX_PROC.to_ne_bytes());
        bytes[20..24].copy_from_slice(&CN_VAL_PROC.to_ne_bytes());
        bytes[CN_ACK_OFFSET..CN_ACK_OFFSET + 4].copy_from_slice(&9_u32.to_ne_bytes());
        bytes[EVENT_OFFSET..EVENT_OFFSET + 4].copy_from_slice(&what.to_ne_bytes());
        for (index, value) in data.iter().enumerate() {
            let offset = EVENT_DATA_OFFSET + index * 4;
            bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }
        bytes
    };
    assert_eq!(
        parse_event(&datagram(PROC_EVENT_FORK, [1, 2, 3, 4])),
        Some(ProcEvent::Fork {
            parent_pid: 1,
            parent_tgid: 2,
            child_pid: 3,
            child_tgid: 4,
        })
    );
    assert_eq!(
        parse_event(&datagram(PROC_EVENT_EXEC, [5, 6, 0, 0])),
        Some(ProcEvent::Exec { tgid: 6 })
    );
    assert_eq!(
        parse_event(&datagram(PROC_EVENT_NONE, [11, 0, 0, 0])),
        Some(ProcEvent::Ack { ack: 9, err: 11 })
    );
    assert_eq!(parse_event(&datagram(0x8000_0000, [1, 2, 3, 4])), None);
    let mut other_connector = datagram(PROC_EVENT_EXEC, [5, 6, 0, 0]);
    other_connector[16..20].copy_from_slice(&2_u32.to_ne_bytes());
    assert_eq!(parse_event(&other_connector), None);
    let truncated = datagram(PROC_EVENT_FORK, [1, 2, 3, 4]);
    assert_eq!(parse_event(&truncated[..EVENT_DATA_OFFSET + 8]), None);
}

#[test]
fn control_message_matches_the_connector_layout() {
    let message = control_message(PROC_CN_MCAST_LISTEN, 42);
    assert_eq!(read_u32(&message, 0), Some(40));
    assert_eq!(&message[4..6], &NLMSG_DONE.to_ne_bytes());
    assert_eq!(read_u32(&message, 16), Some(CN_IDX_PROC));
    assert_eq!(read_u32(&message, 20), Some(CN_VAL_PROC));
    assert_eq!(read_u32(&message, CN_ACK_OFFSET), Some(42));
    assert_eq!(&message[32..34], &4_u16.to_ne_bytes());
    assert_eq!(read_u32(&message, EVENT_OFFSET), Some(PROC_CN_MCAST_LISTEN));
}

#[test]
fn launch_through_the_jailer_is_silent() {
    let (tracker, id) = launched("fc-a");
    assert_eq!(
        tracker.instances[&id].phase,
        Phase::Running(Baseline {
            pid: VMM,
            exe: FIRECRACKER,
            jail_mount: 7,
        })
    );
    assert_eq!(tracker.processes[&VMM].role, Role::Vmm);
    assert!(tracker.pending.is_empty());
}

#[test]
fn jailer_registered_before_its_fork_event_is_confirmed_not_replaced() {
    let mut tracker = Tracker::new(EXO);
    let id = tracker.expect_launch(vmm("fc-a"), SPAWNER);
    tracker.spawned(id, SPAWNER, JAILER, None);
    let out = feed(
        &mut tracker,
        &[
            spawn(SPAWNER, JAILER),
            ProcEvent::Exec { tgid: JAILER },
            fork(JAILER, VMM),
            ProcEvent::Exec { tgid: VMM },
        ],
    );
    assert_eq!(details(&out), Vec::<&Detail>::new());
    assert!(matches!(tracker.instances[&id].phase, Phase::Running(_)));
}

#[test]
fn spawned_after_the_fork_event_keeps_the_tracked_jailer() {
    let mut tracker = Tracker::new(EXO);
    let id = tracker.expect_launch(vmm("fc-a"), SPAWNER);
    feed(&mut tracker, &[spawn(SPAWNER, JAILER)]);
    tracker.spawned(id, SPAWNER, JAILER, None);
    let out = feed(
        &mut tracker,
        &[
            ProcEvent::Exec { tgid: JAILER },
            fork(JAILER, VMM),
            ProcEvent::Exec { tgid: VMM },
        ],
    );
    assert_eq!(details(&out), Vec::<&Detail>::new());
    assert!(matches!(tracker.instances[&id].phase, Phase::Running(_)));
}

#[test]
fn other_exo_children_and_threads_are_ignored() {
    let (mut tracker, _) = launched("fc-a");
    let out = feed(
        &mut tracker,
        &[
            spawn(SPAWNER, 3_000),
            ProcEvent::Exec { tgid: 3_000 },
            ProcEvent::Fork {
                parent_pid: VMM,
                parent_tgid: VMM,
                child_pid: VMM + 1,
                child_tgid: VMM,
            },
        ],
    );
    assert_eq!(details(&out), Vec::<&Detail>::new());
    assert!(!tracker.processes.contains_key(&3_000));
}

#[test]
fn vmm_exec_is_an_exploit_and_fork_is_a_signal() {
    let (mut tracker, id) = launched("fc-a");
    let out = feed(
        &mut tracker,
        &[
            fork(VMM, 3_000),
            ProcEvent::Exec { tgid: 3_000 },
            ProcEvent::Exec { tgid: VMM },
        ],
    );
    assert_eq!(
        details(&out),
        vec![
            &Detail::Fork {
                pid: VMM,
                child: 3_000,
            },
            &Detail::Exec {
                pid: 3_000,
                image: Some("/bin/sh".into()),
            },
            &Detail::Exec {
                pid: VMM,
                image: Some("/bin/sh".into()),
            },
        ]
    );
    assert!(out.iter().all(|emitted| {
        emitted.sandbox_id == "sandbox-fc-a"
            && emitted.machine_id == "fc-a"
            && emitted.source == SOURCE_EVENTS
    }));
    let Phase::Running(baseline) = tracker.instances[&id].phase else {
        panic!("VMM stopped being tracked");
    };
    assert_eq!(baseline.exe, SHELL);
}

#[test]
fn jailer_deviations_are_launch_signals() {
    let mut tracker = Tracker::new(EXO);
    tracker.expect_launch(vmm("fc-a"), SPAWNER);
    let out = feed(
        &mut tracker,
        &[
            spawn(SPAWNER, JAILER),
            ProcEvent::Exec { tgid: JAILER },
            ProcEvent::Exec { tgid: JAILER },
            fork(JAILER, VMM),
            fork(JAILER, 3_000),
            fork(VMM, 3_001),
        ],
    );
    assert_eq!(
        details(&out),
        vec![
            &Detail::Launch {
                pid: JAILER,
                deviation: JailerDeviation::ExecAgain,
            },
            &Detail::Launch {
                pid: JAILER,
                deviation: JailerDeviation::ForkedAgain,
            },
            &Detail::Launch {
                pid: VMM,
                deviation: JailerDeviation::ChildForked,
            },
        ]
    );
}

#[test]
fn reused_pid_is_not_attributed_to_the_exited_vmm() {
    let (mut tracker, _) = launched("fc-a");
    let out = feed(
        &mut tracker,
        &[
            fork(1, VMM),
            ProcEvent::Exec { tgid: VMM },
            fork(VMM, 3_000),
        ],
    );
    assert_eq!(details(&out), Vec::<&Detail>::new());
    assert!(!tracker.processes.contains_key(&VMM));
}

#[test]
fn signals_per_vmm_are_bounded() {
    let (mut tracker, _) = launched("fc-a");
    let out = feed(
        &mut tracker,
        &vec![ProcEvent::Exec { tgid: VMM }; SIGNALS_PER_VMM as usize + 10],
    );
    assert_eq!(out.len(), SIGNALS_PER_VMM as usize + 1);
    assert_eq!(
        out.last().map(|emitted| &emitted.detail),
        Some(&Detail::Suppressed)
    );
}

#[test]
fn forget_and_cancel_drop_all_state() {
    let (mut tracker, _) = launched("fc-a");
    let pending = tracker.expect_launch(vmm("fc-b"), SPAWNER);
    tracker.forget("fc-a");
    tracker.remove_instance(pending);
    assert!(tracker.instances.is_empty());
    assert!(tracker.processes.is_empty());
    assert!(tracker.pending.is_empty());
    let out = feed(&mut tracker, &[ProcEvent::Exec { tgid: VMM }]);
    assert_eq!(details(&out), Vec::<&Detail>::new());
}

#[test]
fn relaunching_a_machine_replaces_its_previous_vmm() {
    let (mut tracker, first) = launched("fc-a");
    let second = tracker.expect_launch(vmm("fc-a"), SPAWNER);
    assert!(!tracker.instances.contains_key(&first));
    assert!(!tracker.processes.contains_key(&VMM));
    assert!(tracker.instances.contains_key(&second));
}

#[test]
fn coverage_loss_is_reported_per_vmm_and_marks_launches_unsynced() {
    let (mut tracker, running) = launched("fc-a");
    let launching = tracker.expect_launch(vmm("fc-b"), SPAWNER + 1);
    let mut out = Vec::new();
    tracker.coverage_lost(CoverageGap::Overflow, &mut out);
    let mut machines = out
        .iter()
        .map(|emitted| (emitted.machine_id.as_str(), &emitted.detail))
        .collect::<Vec<_>>();
    machines.sort_by_key(|(machine_id, _)| *machine_id);
    assert_eq!(
        machines,
        vec![
            ("fc-a", &Detail::EventsLost(CoverageGap::Overflow)),
            ("fc-b", &Detail::EventsLost(CoverageGap::Overflow)),
        ]
    );
    assert!(matches!(
        tracker.instances[&running].phase,
        Phase::Running(_)
    ));
    assert_eq!(
        tracker.instances[&launching].phase,
        Phase::Launching { unsynced: true }
    );
    assert_eq!(tracker.resync_targets().len(), 1);
}

#[test]
fn scan_findings_are_reported_once() {
    let (mut tracker, id) = launched("fc-a");
    let handle = Arc::new(eventfd(0, EventfdFlags::CLOEXEC).expect("eventfd"));
    let target = ScanTarget {
        instance: id,
        baseline: Baseline {
            pid: VMM,
            exe: FIRECRACKER,
            jail_mount: 7,
        },
        handle: Arc::clone(&handle),
        cgroup: PathBuf::from("/nonexistent/cgroup"),
        known: HashSet::from([JAILER, VMM]),
    };
    let inspection = || Inspection {
        image: Some(Image {
            id: FIRECRACKER,
            path: "/firecracker".into(),
        }),
        members: vec![
            Member {
                pid: 3_000,
                image: Some(Image {
                    id: FIRECRACKER,
                    path: "/firecracker".into(),
                }),
                handle: Arc::clone(&handle),
            },
            Member {
                pid: 3_001,
                image: Some(Image {
                    id: SHELL,
                    path: "/bin/sh".into(),
                }),
                handle: Arc::clone(&handle),
            },
        ],
        fds: vec![(
            9,
            FdFinding::Socket {
                target: "socket:[1]".into(),
                protocol: "TCP".into(),
            },
        )],
    };
    let mut out = Vec::new();
    tracker.apply_scan(&target, inspection(), &mut out);
    assert_eq!(
        details(&out),
        vec![
            &Detail::Fork {
                pid: VMM,
                child: 3_000,
            },
            &Detail::Fork {
                pid: VMM,
                child: 3_001,
            },
            &Detail::Exec {
                pid: 3_001,
                image: Some("/bin/sh".into()),
            },
            &Detail::Fd {
                pid: VMM,
                fd: 9,
                finding: FdFinding::Socket {
                    target: "socket:[1]".into(),
                    protocol: "TCP".into(),
                },
            },
        ]
    );
    assert!(out.iter().all(|emitted| emitted.source == SOURCE_SCAN));
    assert_eq!(tracker.processes[&3_001].role, Role::Descendant);
    let mut repeated = Vec::new();
    tracker.apply_scan(&target, inspection(), &mut repeated);
    assert_eq!(details(&repeated), Vec::<&Detail>::new());

    let mut changed = Vec::new();
    let mut exec = inspection();
    exec.image = Some(Image {
        id: SHELL,
        path: "/bin/sh".into(),
    });
    tracker.apply_scan(&target, exec, &mut changed);
    assert_eq!(
        details(&changed),
        vec![&Detail::Exec {
            pid: VMM,
            image: Some("/bin/sh".into()),
        }]
    );
}

#[test]
fn classifies_descriptor_links() {
    for (target, kind) in [
        ("socket:[123]", LinkKind::Socket),
        ("anon_inode:[eventfd]", LinkKind::AnonInode("[eventfd]")),
        ("anon_inode:kvm-vcpu:0", LinkKind::AnonInode("kvm-vcpu:0")),
        ("/memfd:guest_mem (deleted)", LinkKind::Memfd),
        ("/rootfs.ext4", LinkKind::Path),
        ("/memfd:not-deleted", LinkKind::Path),
        ("pipe:[5]", LinkKind::Other),
        ("net:[4026531840]", LinkKind::Other),
    ] {
        assert_eq!(link_kind(target), kind, "{target}");
    }
    for name in [
        "[eventfd]",
        "[eventpoll]",
        "[timerfd]",
        "[io_uring]",
        "[userfaultfd]",
        "kvm-vm",
        "kvm-vcpu:3",
        "kvm-arm-vgic-v3",
    ] {
        assert!(expected_anon_inode(name), "{name}");
    }
    for name in [
        "[pidfd]",
        "[signalfd]",
        "bpf-map",
        "[perf_event]",
        "inotify",
    ] {
        assert!(!expected_anon_inode(name), "{name}");
    }
}

#[test]
fn caps_reported_targets_on_a_character_boundary() {
    let capped = capped("é".repeat(MAX_TARGET_BYTES));
    assert!(capped.len() <= MAX_TARGET_BYTES);
    assert!(capped.chars().all(|character| character == 'é'));
}

// The tests below need root in the initial user, PID, and network namespaces.
// Build as a regular user, then run the test binary with
// `sudo -n <binary> --ignored process_monitor --test-threads=1`.

mod live {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    use tracing::field::{Field, Visit};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};

    use super::*;

    const ROLE: &str = "EXO_PROCESS_MONITOR_TEST_ROLE";
    const CGROUP: &str = "EXO_PROCESS_MONITOR_TEST_CGROUP";
    const CONTROL: &str = "EXO_PROCESS_MONITOR_TEST_CONTROL";
    const HELPER: &str =
        "sandbox_provider::firecracker::process_monitor::tests::live::helper_process";
    const TEST_SCAN_INTERVAL: Duration = Duration::from_millis(200);

    #[derive(Clone, Debug)]
    pub(crate) struct Captured {
        pub(crate) level: tracing::Level,
        pub(crate) fields: BTreeMap<String, String>,
    }

    impl Captured {
        fn field(&self, name: &str) -> &str {
            self.fields.get(name).map_or("", String::as_str)
        }
    }

    struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

    impl Visit for FieldVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.0
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            self.0.insert(field.name().to_string(), value.to_string());
        }
    }

    struct Capture(Arc<Mutex<Vec<Captured>>>);

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: LayerContext<'_, S>) {
            if event.metadata().target() != crate::sandbox_signal::TARGET {
                return;
            }
            let mut fields = BTreeMap::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.0.lock().unwrap().push(Captured {
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    // The monitor emits from its own thread, so capture needs the global
    // subscriber rather than a thread-local default.
    pub(crate) fn signals(sandbox_id: &str) -> Vec<Captured> {
        static EVENTS: OnceLock<Arc<Mutex<Vec<Captured>>>> = OnceLock::new();
        EVENTS
            .get_or_init(|| {
                let events = Arc::new(Mutex::new(Vec::new()));
                tracing::subscriber::set_global_default(
                    tracing_subscriber::registry().with(Capture(Arc::clone(&events))),
                )
                .expect("installing the signal capture subscriber");
                events
            })
            .lock()
            .unwrap()
            .iter()
            .filter(|signal| signal.field("sandbox_id") == sandbox_id)
            .cloned()
            .collect()
    }

    fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
        let started = Instant::now();
        while !ready() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_signal(
        sandbox_id: &str,
        what: &str,
        matches: impl Fn(&Captured) -> bool,
    ) -> Captured {
        let mut found = None;
        wait_for(what, || {
            found = signals(sandbox_id)
                .into_iter()
                .find(|signal| matches(signal));
            found.is_some()
        });
        found.expect("signal was found")
    }

    fn helper(role: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().expect("test binary path"));
        command
            .args([
                "--exact",
                HELPER,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE, role)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn running_pid(monitor: &ProcessMonitor, sandbox_id: &str) -> Option<i32> {
        monitor
            .shared
            .lock()
            .instances
            .values()
            .find(|instance| instance.vmm.sandbox_id == sandbox_id)
            .and_then(|instance| match instance.phase {
                Phase::Running(baseline) => Some(baseline.pid),
                Phase::Launching { .. } => None,
            })
    }

    // A directory on the same mount as `/`, which is the mimic VMM's root.
    fn root_mount_directory() -> tempfile::TempDir {
        let binary = std::env::current_exe().expect("test binary path");
        tempfile::tempdir_in(binary.parent().expect("test binary directory"))
            .expect("creating a test directory")
    }

    struct TestCgroup(PathBuf);

    impl TestCgroup {
        fn create(name: &str) -> Self {
            let path = PathBuf::from("/sys/fs/cgroup")
                .join(format!("exo-process-monitor-{name}-{}", std::process::id()));
            fs::create_dir(&path).expect("creating the test cgroup");
            Self(path)
        }
    }

    impl Drop for TestCgroup {
        fn drop(&mut self) {
            fs::write(self.0.join("cgroup.kill"), "1").expect("killing the test cgroup");
            let started = Instant::now();
            while let Err(error) = fs::remove_dir(&self.0) {
                assert!(
                    started.elapsed() < Duration::from_secs(5),
                    "removing the test cgroup: {error}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[test]
    #[ignore = "helper process for the root process monitor tests"]
    fn helper_process() {
        let Ok(role) = std::env::var(ROLE) else {
            return;
        };
        match role.as_str() {
            // Like the jailer: join the machine cgroup, start one child that
            // execs the VMM image, and exit without waiting for it.
            "jailer" => {
                let cgroup = PathBuf::from(std::env::var(CGROUP).expect("cgroup"));
                fs::write(cgroup.join("cgroup.procs"), std::process::id().to_string())
                    .expect("joining the test cgroup");
                helper("vmm").spawn().expect("starting the mimic VMM");
                std::process::exit(0);
            }
            "vmm" => mimic_vmm(),
            "start" => match ProcessMonitor::start() {
                Ok(_) => std::process::exit(0),
                Err(error) => {
                    eprintln!("{error:#}");
                    std::process::exit(3);
                }
            },
            other => panic!("unknown helper role {other}"),
        }
    }

    fn mimic_vmm() {
        let stream = UnixStream::connect(std::env::var(CONTROL).expect("control socket"))
            .expect("connecting to the test");
        let mut writer = stream.try_clone().expect("cloning the control socket");
        writeln!(writer, "{}", std::process::id()).expect("reporting the VMM pid");
        let mut held: Vec<Box<dyn std::any::Any>> = Vec::new();
        for line in BufReader::new(stream).lines() {
            let line = line.expect("reading a command");
            let (command, argument) = line.split_once(' ').unwrap_or((line.as_str(), ""));
            match command {
                "open" => held.push(Box::new(fs::File::open(argument).expect("opening a file"))),
                "tcp" => held.push(Box::new(
                    std::net::TcpListener::bind("127.0.0.1:0").expect("binding a TCP socket"),
                )),
                "clone_parent" => {
                    // SAFETY: the child only calls pause(2), which is
                    // async-signal-safe, until the test kills its cgroup.
                    let pid = unsafe {
                        libc::syscall(
                            libc::SYS_clone,
                            libc::CLONE_PARENT | libc::SIGCHLD,
                            0,
                            0,
                            0,
                            0,
                        )
                    };
                    if pid == 0 {
                        loop {
                            // SAFETY: pause(2) has no preconditions.
                            unsafe { libc::pause() };
                        }
                    }
                    assert!(pid > 0, "clone failed");
                }
                "spawn" => held.push(Box::new(
                    Command::new("/bin/sleep")
                        .arg("300")
                        .spawn()
                        .expect("spawning a child"),
                )),
                "exec" => {
                    writeln!(writer, "ok").expect("acknowledging");
                    let error = Command::new("/bin/sleep").arg("300").exec();
                    panic!("exec failed: {error}");
                }
                other => panic!("unknown command {other}"),
            }
            writeln!(writer, "ok").expect("acknowledging");
        }
    }

    struct Mimic {
        _cgroup: TestCgroup,
        _directory: tempfile::TempDir,
        stream: BufReader<UnixStream>,
        pid: i32,
    }

    impl Mimic {
        fn command(&mut self, command: &str) {
            writeln!(self.stream.get_mut(), "{command}").expect("sending a command");
            let mut reply = String::new();
            self.stream.read_line(&mut reply).expect("reading a reply");
            assert_eq!(reply.trim(), "ok", "command {command}");
        }
    }

    fn launch_mimic(monitor: &ProcessMonitor, sandbox_id: &str) -> Mimic {
        let cgroup = TestCgroup::create(sandbox_id);
        let directory = root_mount_directory();
        let control = directory.path().join("control");
        let listener = UnixListener::bind(&control).expect("binding the control socket");
        let mut jailer = helper("jailer");
        jailer.env(CGROUP, &cgroup.0).env(CONTROL, &control);
        let launch = monitor
            .expect_launch(MonitoredVmm {
                sandbox_id: sandbox_id.into(),
                machine_id: format!("fc-{sandbox_id}"),
                pid_file: directory.path().join("firecracker.pid"),
                cgroup: cgroup.0.clone(),
            })
            .expect("registering the launch");
        let mut jailer = jailer.spawn().expect("starting the mimic jailer");
        launch.spawned(jailer.id());
        assert!(
            jailer
                .wait()
                .expect("waiting for the mimic jailer")
                .success()
        );
        let (stream, _) = listener.accept().expect("accepting the mimic VMM");
        let mut stream = BufReader::new(stream);
        let mut pid = String::new();
        stream.read_line(&mut pid).expect("reading the VMM pid");
        let pid = pid.trim().parse().expect("VMM pid");
        wait_for("the VMM to be monitored", || {
            running_pid(monitor, sandbox_id) == Some(pid)
        });
        Mimic {
            _cgroup: cgroup,
            _directory: directory,
            stream,
            pid,
        }
    }

    #[test]
    #[ignore = "requires root in the initial user, PID, and network namespaces"]
    fn detects_a_vmm_that_escapes_its_jail_forks_and_execs() {
        let sandbox_id = "mimic-detects";
        signals(sandbox_id);
        let monitor = ProcessMonitor::start_with(TEST_SCAN_INTERVAL, RECEIVE_BUFFER_BYTES)
            .expect("starting the monitor");
        let mut mimic = launch_mimic(&monitor, sandbox_id);
        let vmm = mimic.pid.to_string();
        let inside = mimic._directory.path().join("inside");
        fs::write(&inside, b"jailed").expect("writing a file inside the root mount");
        mimic.command(&format!("open {}", inside.display()));
        std::thread::sleep(TEST_SCAN_INTERVAL * 3);
        let quiet = signals(sandbox_id);
        assert!(quiet.is_empty(), "unexpected signals: {quiet:#?}");

        mimic.command("open /proc/version");
        mimic.command("tcp");
        let outside = wait_for_signal(sandbox_id, "an outside-jail fd", |signal| {
            signal.field("reason") == "outside_jail"
        });
        assert_eq!(outside.field("target"), "/proc/version");
        let socket = wait_for_signal(sandbox_id, "a TCP socket", |signal| {
            signal.field("reason") == "socket_family"
        });
        assert_eq!(socket.field("protocol"), "TCP");
        for signal in [&outside, &socket] {
            assert_eq!(signal.field("signal"), "sandbox.vmm_fd");
            assert_eq!(signal.field("severity"), "signal");
            assert_eq!(signal.level, tracing::Level::INFO);
            assert_eq!(signal.field("pid"), vmm);
            assert_eq!(signal.field("machine_id"), format!("fc-{sandbox_id}"));
            assert_eq!(signal.field("source"), "exo.firecracker.process_monitor");
            assert_eq!(signal.field("detector"), "scan");
        }

        mimic.command("clone_parent");
        let sibling = wait_for_signal(sandbox_id, "a CLONE_PARENT child", |signal| {
            signal.field("signal") == "sandbox.vmm_fork" && signal.field("detector") == "scan"
        });
        assert_eq!(sibling.field("severity"), "signal");
        assert_eq!(sibling.field("pid"), vmm);

        mimic.command("spawn");
        let fork = wait_for_signal(sandbox_id, "a VMM fork", |signal| {
            signal.field("signal") == "sandbox.vmm_fork" && signal.field("detector") == "events"
        });
        assert_eq!(fork.field("severity"), "signal");
        assert_eq!(fork.field("pid"), vmm);
        let child = fork.field("child_pid").to_string();
        let child_exec = wait_for_signal(sandbox_id, "the child's exec", |signal| {
            signal.field("signal") == "sandbox.vmm_exec" && signal.field("pid") == child
        });
        assert_eq!(child_exec.field("severity"), "exploit");
        assert_eq!(child_exec.level, tracing::Level::ERROR);
        let sleep = fs::canonicalize("/bin/sleep").expect("resolving sleep");
        assert_eq!(Path::new(child_exec.field("image")), sleep);

        mimic.command("exec");
        let exec = wait_for_signal(sandbox_id, "the VMM's exec", |signal| {
            signal.field("signal") == "sandbox.vmm_exec" && signal.field("pid") == vmm
        });
        assert_eq!(exec.field("severity"), "exploit");
        assert_eq!(exec.field("detector"), "events");
        assert_eq!(Path::new(exec.field("image")), sleep);
        std::thread::sleep(TEST_SCAN_INTERVAL * 3);
        let all = signals(sandbox_id);
        let count = |name: &str| {
            all.iter()
                .filter(|signal| signal.field("signal") == name)
                .count()
        };
        assert_eq!(count("sandbox.vmm_fd"), 2, "{all:#?}");
        assert_eq!(count("sandbox.vmm_fork"), 2, "{all:#?}");
        assert_eq!(count("sandbox.vmm_exec"), 2, "{all:#?}");
        assert_eq!(all.len(), 6, "{all:#?}");
    }

    #[test]
    #[ignore = "requires root in the initial user, PID, and network namespaces"]
    fn startup_fails_outside_the_initial_namespaces() {
        for (namespaces, expected) in [
            (&["--net"][..], "initial network namespace"),
            (&["--pid", "--fork"][..], "initial user and PID namespaces"),
        ] {
            let start = helper("start");
            let output = Command::new("unshare")
                .args(namespaces)
                .arg(start.get_program())
                .args(start.get_args())
                .env(ROLE, "start")
                .output()
                .expect("running unshare");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(output.status.code(), Some(3), "{namespaces:?}: {stderr}");
            assert!(stderr.contains(expected), "{namespaces:?}: {stderr}");
        }
    }

    #[test]
    #[ignore = "requires root in the initial user, PID, and network namespaces"]
    fn overflow_is_reported_for_every_monitored_vmm() {
        let sandbox_id = "overflow";
        signals(sandbox_id);
        let monitor = ProcessMonitor::start_with(Duration::from_secs(60), 4096)
            .expect("starting the monitor");
        monitor.shared.lock().expect_launch(
            MonitoredVmm {
                sandbox_id: sandbox_id.into(),
                machine_id: "fc-overflow".into(),
                pid_file: PathBuf::from("/nonexistent/firecracker.pid"),
                cgroup: PathBuf::from("/nonexistent/cgroup"),
            },
            -1,
        );
        {
            // Stall the monitor thread so the small receive buffer overflows.
            let _stalled = monitor.shared.lock();
            let storm = Command::new("sh")
                .args([
                    "-c",
                    "i=0; while [ $i -lt 500 ]; do /bin/true; i=$((i+1)); done",
                ])
                .status()
                .expect("running the fork storm");
            assert!(storm.success());
        }
        let lost = wait_for_signal(sandbox_id, "an overflow signal", |signal| {
            signal.field("signal") == "sandbox.vmm_events_lost"
        });
        assert_eq!(lost.field("reason"), "overflow");
        assert_eq!(lost.field("severity"), "signal");
        assert_eq!(
            monitor.shared.lock().resync_targets().len(),
            1,
            "the pending launch must be resolved from its pid file"
        );
    }

    fn thread_cpu(name: &str) -> Duration {
        let ticks = fs::read_dir("/proc/self/task")
            .expect("listing threads")
            .flatten()
            .find(|task| {
                fs::read_to_string(task.path().join("comm")).is_ok_and(|comm| comm.trim() == name)
            })
            .map(|task| {
                let stat = fs::read_to_string(task.path().join("stat")).expect("thread stat");
                let fields = stat
                    .rsplit_once(") ")
                    .expect("stat fields")
                    .1
                    .split_whitespace()
                    .collect::<Vec<_>>();
                fields[11].parse::<u64>().expect("utime")
                    + fields[12].parse::<u64>().expect("stime")
            })
            .expect("monitor thread");
        // SAFETY: sysconf has no preconditions.
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
        Duration::from_millis(ticks * 1000 / hz)
    }

    fn unfiltered_events() -> OwnedFd {
        let socket = socket_with(
            AddressFamily::NETLINK,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            Some(netlink::CONNECTOR),
        )
        .expect("opening the connector");
        sockopt::set_socket_recv_buffer_size_force(&socket, 64 * 1024 * 1024).expect("sizing");
        bind(&socket, &SocketAddrNetlink::new(0, CN_IDX_PROC)).expect("binding");
        sendto(
            &socket,
            &control_message(PROC_CN_MCAST_LISTEN, 0),
            SendFlags::empty(),
            &SocketAddrNetlink::new(0, 0),
        )
        .expect("subscribing");
        socket
    }

    fn count_datagrams(socket: &OwnedFd) -> u64 {
        let mut datagram = [0; DATAGRAM_BYTES];
        let mut count = 0;
        loop {
            match recvfrom(socket, &mut datagram[..], RecvFlags::empty()) {
                Ok(_) => count += 1,
                Err(Errno::AGAIN) => return count,
                Err(error) => panic!("reading unfiltered events: {error}"),
            }
        }
    }

    #[test]
    #[ignore = "requires root in the initial user, PID, and network namespaces"]
    fn unrelated_exec_storm_is_filtered_cheaply() {
        let sandbox_id = "storm";
        signals(sandbox_id);
        let monitor = ProcessMonitor::start_with(Duration::from_secs(1), RECEIVE_BUFFER_BYTES)
            .expect("starting the monitor");
        let _mimic = launch_mimic(&monitor, sandbox_id);
        let unfiltered = unfiltered_events();
        count_datagrams(&unfiltered);
        let before_events = monitor.shared.received.load(Ordering::Relaxed);
        let before_cpu = thread_cpu("exo-fc-monitor");
        let started = Instant::now();
        let iterations = 5_000;
        let shells = 2;
        let storms = (0..shells)
            .map(|_| {
                Command::new("sh")
                    .args([
                        "-c",
                        &format!(
                            "i=0; while [ $i -lt {iterations} ]; do /bin/true; i=$((i+1)); done"
                        ),
                    ])
                    .spawn()
                    .expect("starting a storm")
            })
            .collect::<Vec<_>>();
        for mut storm in storms {
            assert!(storm.wait().expect("waiting for a storm").success());
        }
        let elapsed = started.elapsed();
        std::thread::sleep(Duration::from_millis(200));
        let events = monitor.shared.received.load(Ordering::Relaxed) - before_events;
        let cpu = thread_cpu("exo-fc-monitor") - before_cpu;
        let unfiltered = count_datagrams(&unfiltered);
        let spawned = iterations * shells;
        println!(
            "{spawned} processes in {elapsed:?} ({:.0}/s); kernel delivered {unfiltered} events unfiltered, {events} to the monitor; monitor thread CPU {cpu:?} ({:.2}% of one core)",
            f64::from(spawned) / elapsed.as_secs_f64(),
            cpu.as_secs_f64() * 100.0 / elapsed.as_secs_f64(),
        );
        assert!(events < unfiltered, "the socket filter dropped nothing");
        let signals = signals(sandbox_id);
        assert!(signals.is_empty(), "unexpected signals: {signals:#?}");
    }

    #[test]
    #[ignore = "requires root, EXO_FIRECRACKER_BINARY, and EXO_FIRECRACKER_JAILER"]
    fn real_jailer_launch_is_monitored_without_signals() {
        use super::super::super::{
            FirecrackerConfig, MachineRecord, jail_root, jailer_uid, prepare_api_run_dir,
            spawn_jailed_firecracker, stop_machine_process_blocking, tests::test_runtime,
            wait_for_firecracker_api,
        };
        use std::os::unix::fs::PermissionsExt;

        let sandbox_id = "real-jailer";
        signals(sandbox_id);
        let state = tempfile::tempdir().expect("creating a state root");
        let config = FirecrackerConfig {
            state_root: state.path().into(),
            firecracker_bin: std::env::var("EXO_FIRECRACKER_BINARY")
                .expect("EXO_FIRECRACKER_BINARY")
                .into(),
            jailer_bin: std::env::var("EXO_FIRECRACKER_JAILER")
                .expect("EXO_FIRECRACKER_JAILER")
                .into(),
            ..Default::default()
        };
        let record = MachineRecord {
            machine_id: format!("fc-{:016x}-00000000", std::process::id()),
            spec_hash: "spec".into(),
            runtime: test_runtime(),
            resolved_image: "image".into(),
            slot: 1,
            network_enabled: false,
            workspace_id: None,
            resource_paths: Vec::new(),
            idle_ttl_seconds: None,
            snapshot_template: None,
            snapshot_network_slot: None,
        };
        let root = jail_root(&config, &record.machine_id);
        fs::create_dir_all(&root).expect("creating the jail");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("jail permissions");
        prepare_api_run_dir(&root, jailer_uid(&config, &record).expect("jailer uid"))
            .expect("preparing the API directory");
        let monitor = ProcessMonitor::start_with(TEST_SCAN_INTERVAL, RECEIVE_BUFFER_BYTES)
            .expect("starting the monitor");
        spawn_jailed_firecracker(&config, &record, &root, &[], &monitor, sandbox_id)
            .expect("launching Firecracker");
        wait_for_firecracker_api(&root, &record.machine_id).expect("Firecracker API");
        let pid: i32 = fs::read_to_string(root.join("firecracker.pid"))
            .expect("pid file")
            .trim()
            .parse()
            .expect("pid");
        wait_for("Firecracker to be monitored", || {
            running_pid(&monitor, sandbox_id) == Some(pid)
        });
        let jail_mount = process_baseline(pid).expect("baseline").jail_mount;
        let started = Instant::now();
        let scanned_fds = inspect_fds(pid, jail_mount);
        println!(
            "scanned {} Firecracker descriptors in {:?}",
            fs::read_dir(format!("/proc/{pid}/fd"))
                .expect("fds")
                .count(),
            started.elapsed()
        );
        assert!(scanned_fds.is_empty(), "{scanned_fds:#?}");
        // A later controller adopts the running VMM.
        let restarted = ProcessMonitor::start_with(TEST_SCAN_INTERVAL, RECEIVE_BUFFER_BYTES)
            .expect("starting a second monitor");
        let vmm = || super::super::super::monitored_vmm(&config, sandbox_id, &record.machine_id);
        restarted.adopt(vmm()).expect("adopting Firecracker");
        restarted.adopt(vmm()).expect("adopting Firecracker again");
        assert_eq!(running_pid(&restarted, sandbox_id), Some(pid));
        std::thread::sleep(TEST_SCAN_INTERVAL * 5);
        stop_machine_process_blocking(&record.machine_id, &root.join("firecracker.pid"))
            .expect("stopping Firecracker");
        monitor.forget(&record.machine_id);
        fs::remove_dir(super::super::super::firecracker_cgroup_dir(
            &config,
            &record.machine_id,
        ))
        .expect("removing the Firecracker cgroup");
        let signals = signals(sandbox_id);
        assert!(signals.is_empty(), "unexpected signals: {signals:#?}");
    }
}
