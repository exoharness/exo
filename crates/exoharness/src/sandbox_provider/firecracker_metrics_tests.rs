use super::*;

// Records written by the Firecracker v1.16.2 aarch64 release binary
// (firecracker-v1.16.2-aarch64.tgz) launched with --api-sock and a FIFO
// --metrics-path, one process per line, each stopped before boot by SIGHUP,
// SIGSEGV, SIGBUS, and SIGSYS. Its handlers for those signals flush metrics
// before exiting. The SIGHUP record has every counter read here at zero.
const RECORDS: &str = include_str!("testdata/firecracker-v1.16.2-metrics.jsonl");

fn record(index: usize) -> &'static str {
    RECORDS
        .lines()
        .nth(index)
        .expect("fixture has four records")
}

fn quiet_record() -> &'static str {
    record(0)
}

fn with_counter(record: &str, field: &str, value: u64) -> String {
    let zero = format!("\"{field}\":0");
    assert_eq!(record.matches(&zero).count(), 1, "{field} is not unique");
    record.replacen(&zero, &format!("\"{field}\":{value}"), 1)
}

fn deltas(stream: &mut MetricsStream, bytes: &[u8]) -> Vec<MetricsDelta> {
    let mut deltas = Vec::new();
    stream.push(bytes, |delta| deltas.push(delta));
    deltas
}

fn line(record: &str) -> Vec<u8> {
    format!("{record}\n").into_bytes()
}

#[test]
fn firecracker_records_report_fatal_signals_once() {
    let expected = [
        MetricsDelta::default(),
        MetricsDelta {
            sigsegv: 1,
            ..Default::default()
        },
        MetricsDelta {
            sigbus: 1,
            ..Default::default()
        },
        MetricsDelta {
            seccomp_faults: 1,
            ..Default::default()
        },
    ];
    for (index, expected) in expected.into_iter().enumerate() {
        let mut stream = MetricsStream::default();
        let bytes = line(record(index));
        assert_eq!(deltas(&mut stream, &bytes), [expected]);
        // These counters are absolute, so a repeated value is no new event.
        assert_eq!(deltas(&mut stream, &bytes), [MetricsDelta::default()]);
        assert_eq!((stream.records, stream.malformed), (2, 0));
    }
}

#[test]
fn incremental_counters_are_reported_as_flushed() {
    let mut record = quiet_record().to_string();
    for (field, value) in [
        ("failures", 1),
        ("execute_fails", 2),
        ("invalid_reqs_count", 3),
        ("tap_write_fails", 4),
        ("tx_malformed_frames", 5),
        ("conn_event_fails", 6),
        ("host_rng_fails", 7),
    ] {
        record = with_counter(&record, field, value);
    }
    let expected = MetricsDelta {
        vcpu_failures: 1,
        block_errors: 2,
        block_invalid_requests: 3,
        net_errors: 4,
        net_malformed_frames: 5,
        vsock_errors: 6,
        entropy_errors: 7,
        ..Default::default()
    };
    let mut stream = MetricsStream::default();
    let bytes = line(&record);
    // Firecracker resets these on every flush, so each record is a delta.
    assert_eq!(deltas(&mut stream, &bytes), [expected]);
    assert_eq!(deltas(&mut stream, &bytes), [expected]);
}

#[test]
fn records_split_across_reads_are_reassembled() {
    let mut whole = MetricsStream::default();
    let expected = deltas(&mut whole, RECORDS.as_bytes());
    assert_eq!(expected.len(), 4);
    let mut split = MetricsStream::default();
    let mut observed = Vec::new();
    for chunk in RECORDS.as_bytes().chunks(7) {
        observed.extend(deltas(&mut split, chunk));
    }
    assert_eq!(observed, expected);
    assert_eq!(split.malformed, 0);
}

#[test]
fn truncated_records_are_discarded_and_the_stream_resynchronizes() {
    let mut stream = MetricsStream::default();
    let segv = record(1);
    let mut bytes = segv.as_bytes()[..segv.len() / 2].to_vec();
    bytes.push(b'\n');
    bytes.extend(line(record(2)));
    assert_eq!(
        deltas(&mut stream, &bytes),
        [MetricsDelta {
            sigbus: 1,
            ..Default::default()
        }]
    );
    assert_eq!((stream.records, stream.malformed), (1, 1));

    assert!(deltas(&mut stream, &record(3).as_bytes()[..100]).is_empty());
    stream.finish();
    assert_eq!((stream.records, stream.malformed), (1, 2));
    assert!(stream.line.is_empty());
}

#[test]
fn records_that_do_not_match_the_schema_are_malformed() {
    let mut stream = MetricsStream::default();
    for invalid in [
        r#"{"seccomp":{"num_faults":1}}"#.to_string(),
        quiet_record().replacen(r#""num_faults":0"#, r#""num_faults":-1"#, 1),
        quiet_record().replacen(r#""num_faults":0"#, r#""num_faults":"1""#, 1),
        "not json".to_string(),
    ] {
        assert!(deltas(&mut stream, &line(&invalid)).is_empty());
    }
    assert_eq!((stream.records, stream.malformed), (0, 4));
    assert!(deltas(&mut stream, b"\n\n").is_empty());
    assert_eq!(stream.malformed, 4);
}

#[test]
fn records_over_the_size_cap_are_never_buffered() {
    let segv = record(1);
    let mut at_cap = segv.to_string();
    at_cap.push_str(&" ".repeat(MAX_RECORD_BYTES - segv.len()));
    let mut stream = MetricsStream::default();
    assert_eq!(deltas(&mut stream, &line(&at_cap)).len(), 1);

    let over_cap = format!("{at_cap} ");
    let mut stream = MetricsStream::default();
    for chunk in over_cap.as_bytes().chunks(READ_CHUNK_BYTES) {
        assert!(deltas(&mut stream, chunk).is_empty());
        assert!(stream.line.len() <= MAX_RECORD_BYTES);
    }
    assert!(stream.oversized);
    assert!(stream.line.is_empty());
    let mut bytes = b"\n".to_vec();
    bytes.extend(line(record(3)));
    assert_eq!(
        deltas(&mut stream, &bytes),
        [MetricsDelta {
            seccomp_faults: 1,
            ..Default::default()
        }]
    );
    assert_eq!((stream.records, stream.malformed), (1, 1));
}

#[test]
fn read_budget_refills_at_its_rate_and_caps_bursts() {
    let start = Instant::now();
    let mut budget = ReadBudget::new(start);
    assert!(budget.allows_read(start));
    budget.spend(READ_BURST_BYTES as usize);
    assert!(!budget.allows_read(start));
    let refill = std::time::Duration::from_secs(READ_CHUNK_BYTES as u64 / READ_BYTES_PER_SECOND);
    assert!(!budget.allows_read(start + refill - std::time::Duration::from_secs(1)));
    assert!(budget.allows_read(start + refill));
    assert!(budget.allows_read(start + std::time::Duration::from_secs(24 * 60 * 60)));
    assert_eq!(budget.available, READ_BURST_BYTES);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};

    use tracing::field::{Field, Visit};
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::{Context, SubscriberExt};

    use super::*;

    #[derive(Debug, Clone)]
    struct Captured {
        target: String,
        level: tracing::Level,
        fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<Captured>>>);

    impl Capture {
        fn events(&self) -> Vec<Captured> {
            self.0.lock().unwrap().clone()
        }

        fn signals(&self) -> Vec<Captured> {
            self.events()
                .into_iter()
                .filter(|event| event.target == crate::sandbox_signal::TARGET)
                .collect()
        }

        fn wait_for(&self, description: &str, ready: impl Fn(&[Captured]) -> bool) {
            let deadline = Instant::now() + std::time::Duration::from_secs(10);
            while !ready(&self.events()) {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {description}"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
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

    impl<S: tracing::Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
            let mut fields = BTreeMap::new();
            event.record(&mut FieldVisitor(&mut fields));
            self.0.lock().unwrap().push(Captured {
                target: event.metadata().target().to_string(),
                level: *event.metadata().level(),
                fields,
            });
        }
    }

    fn capture(emit: impl FnOnce()) -> Vec<Captured> {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        tracing::subscriber::with_default(subscriber, emit);
        capture.signals()
    }

    fn create_pipe(path: &Path) -> Result<MetricsPipe> {
        MetricsPipe::create(
            path,
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        )
    }

    fn kernel_reports_exit_status() -> bool {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
        let mut version = release
            .split(|character: char| !character.is_ascii_digit())
            .map(|part| part.parse::<u32>().unwrap());
        (version.next().unwrap(), version.next().unwrap()) >= (6, 15)
    }

    #[test]
    fn seccomp_evidence_is_reported_as_an_exploit() {
        let fault = MetricsDelta {
            seccomp_faults: 1,
            ..Default::default()
        };
        let segv = MetricsDelta {
            sigsegv: 1,
            ..Default::default()
        };
        let bad_syscall = Some(ExitStatus::from_raw(BAD_SYSCALL_EXIT_CODE << 8));
        let events = capture(|| {
            report("sbx", "fc-1", &fault, VmmState::Running);
            report("sbx", "fc-1", &segv, VmmState::Running);
            report(
                "sbx",
                "fc-1",
                &MetricsDelta::default(),
                VmmState::Exited(bad_syscall),
            );
            report(
                "sbx",
                "fc-1",
                &segv,
                VmmState::Exited(Some(ExitStatus::from_raw(150 << 8))),
            );
        });
        let [fault, segv, bad_syscall, segv_exit] = events.as_slice() else {
            panic!("expected four signals, got {events:?}");
        };
        for event in &events {
            assert_eq!(event.fields["signal"], "sandbox.vmm_metrics");
            assert_eq!(event.fields["sandbox_id"], "sbx");
            assert_eq!(event.fields["source"], "exo.firecracker");
            assert_eq!(event.fields["machine_id"], "fc-1");
        }
        assert_eq!(fault.fields["severity"], "exploit");
        assert_eq!(fault.level, tracing::Level::ERROR);
        assert_eq!(fault.fields["seccomp_faults"], "1");
        assert_eq!(fault.fields["vmm_exited"], "false");
        assert!(!fault.fields.contains_key("exit_code"));
        assert_eq!(segv.fields["severity"], "signal");
        assert_eq!(segv.fields["sigsegv"], "1");
        assert_eq!(bad_syscall.fields["severity"], "exploit");
        assert_eq!(bad_syscall.fields["vmm_exited"], "true");
        assert_eq!(bad_syscall.fields["exit_code"], "148");
        assert_eq!(bad_syscall.fields["seccomp_faults"], "0");
        assert_eq!(segv_exit.fields["severity"], "signal");
        assert_eq!(segv_exit.fields["exit_code"], "150");
        assert!(!segv_exit.fields.contains_key("exit_signal"));
    }

    #[test]
    fn poweroff_and_sigkill_are_expected_exits() {
        assert!(!unexpected_exit(ExitStatus::from_raw(0)));
        assert!(!unexpected_exit(ExitStatus::from_raw(libc::SIGKILL)));
        for status in [BAD_SYSCALL_EXIT_CODE << 8, 150 << 8, 1 << 8, libc::SIGABRT] {
            assert!(unexpected_exit(ExitStatus::from_raw(status)), "{status}");
        }
    }

    #[test]
    fn metrics_fifo_replaces_whatever_is_at_its_path() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("firecracker.metrics");
        let target = root.path().join("host-file");
        std::fs::write(&target, b"host")?;
        std::os::unix::fs::symlink(&target, &path)?;
        let uid = rustix::process::geteuid().as_raw();
        let pipe = MetricsPipe::create(&path, uid, rustix::process::getegid().as_raw())?;
        let metadata = std::fs::symlink_metadata(&path)?;
        assert!(metadata.file_type().is_fifo());
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&target)?, b"host");
        assert!(pipe.0.metadata()?.file_type().is_fifo());
        Ok(())
    }

    #[test]
    fn watch_reports_records_until_the_vmm_exits() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("firecracker.metrics");
        let pipe = create_pipe(&path)?;
        let mut vmm = Command::new("sh")
            .args(["-c", "read _; exit 148"])
            .stdin(Stdio::piped())
            .spawn()?;
        let pid_path = root.path().join("firecracker.pid");
        std::fs::write(&pid_path, vmm.id().to_string())?;
        let capture = Capture::default();
        let subscriber = tracing_subscriber::registry().with(capture.clone());
        let watcher = std::thread::spawn(move || {
            tracing::subscriber::with_default(subscriber, || {
                pipe.watch("sbx", "fc-1", Some(&pid_path));
            });
        });
        let mut writer = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)?;
        let observed = |count: usize| {
            move |events: &[Captured]| {
                events
                    .iter()
                    .filter(|event| {
                        event.fields.get("message").map(String::as_str)
                            == Some("observed Firecracker metrics record")
                    })
                    .count()
                    >= count
            }
        };

        writer.write_all(&line(quiet_record()))?;
        capture.wait_for("the quiet record", observed(1));
        writer.write_all(&line(&with_counter(
            quiet_record(),
            "invalid_reqs_count",
            2,
        )))?;
        capture.wait_for("the block record", observed(2));
        writer.write_all(&line(record(3)))?;
        capture.wait_for("the seccomp record", observed(3));
        vmm.stdin.take().unwrap().write_all(b"\n")?;
        assert_eq!(vmm.wait()?.code(), Some(BAD_SYSCALL_EXIT_CODE));
        watcher.join().unwrap();
        drop(writer);

        let signals = capture.signals();
        let (running, exited): (Vec<_>, Vec<_>) = signals
            .iter()
            .partition(|event| event.fields["vmm_exited"] == "false");
        let [block, seccomp] = running.as_slice() else {
            panic!("expected two running signals, got {running:?}");
        };
        assert_eq!(block.fields["severity"], "signal");
        assert_eq!(block.fields["block_invalid_requests"], "2");
        assert_eq!(seccomp.fields["severity"], "exploit");
        assert_eq!(seccomp.fields["seccomp_faults"], "1");
        if kernel_reports_exit_status() {
            let [exit] = exited.as_slice() else {
                panic!("expected one exit signal, got {exited:?}");
            };
            assert_eq!(exit.fields["severity"], "exploit");
            assert_eq!(exit.fields["exit_code"], "148");
            assert_eq!(exit.fields["seccomp_faults"], "0");
        } else {
            assert!(exited.is_empty(), "{exited:?}");
        }
        Ok(())
    }

    #[test]
    fn watch_without_a_vmm_drains_and_returns() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("firecracker.metrics");
        let pipe = create_pipe(&path)?;
        {
            let mut writer = std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)?;
            writer.write_all(&line(record(1)))?;
        }
        let events = capture(|| pipe.watch("sbx", "fc-1", None));
        let [segv] = events.as_slice() else {
            panic!("expected one signal, got {events:?}");
        };
        assert_eq!(segv.fields["sigsegv"], "1");
        assert_eq!(segv.fields["vmm_exited"], "true");
        assert!(!segv.fields.contains_key("exit_code"));
        Ok(())
    }
}
