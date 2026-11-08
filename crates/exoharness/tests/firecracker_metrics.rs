#![cfg(feature = "firecracker")]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

type Fields = BTreeMap<String, String>;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<Fields>>>);

impl Capture {
    async fn wait_for(&self, description: &str, ready: impl Fn(&Fields) -> bool) -> Fields {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(event) = self.0.lock().unwrap().iter().find(|event| ready(event)) {
                return event.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct FieldVisitor<'a>(&'a mut Fields);

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
        let mut fields = Fields::new();
        fields.insert("target".into(), event.metadata().target().into());
        event.record(&mut FieldVisitor(&mut fields));
        self.0.lock().unwrap().push(fields);
    }
}

#[derive(serde::Deserialize)]
struct ProviderState {
    machine_id: String,
}

fn machine_id(handle: &dyn exoharness::ManagedSandboxHandle) -> anyhow::Result<String> {
    let state = handle
        .provider_state()
        .ok_or_else(|| anyhow::anyhow!("Firecracker handle has no provider state"))?;
    Ok(serde_json::from_value::<ProviderState>(state)?.machine_id)
}

fn flush_metrics(jail: &Path) -> anyhow::Result<()> {
    let body = r#"{"action_type":"FlushMetrics"}"#;
    let mut stream = UnixStream::connect(jail.join("run/firecracker.socket"))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "PUT /actions HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )?;
    let mut response = [0; 256];
    let read = stream.read(&mut response)?;
    let response = String::from_utf8_lossy(&response[..read]);
    anyhow::ensure!(
        response.starts_with("HTTP/1.1 204"),
        "FlushMetrics failed: {response}"
    );
    Ok(())
}

fn observed_records(machine_id: &str, records: u64) -> impl Fn(&Fields) -> bool + '_ {
    move |event| {
        event.get("message").map(String::as_str) == Some("observed Firecracker metrics record")
            && event.get("machine_id").map(String::as_str) == Some(machine_id)
            && event
                .get("records")
                .and_then(|count| count.parse::<u64>().ok())
                .is_some_and(|count| count >= records)
    }
}

fn vmm_metrics_signal<'a>(
    sandbox_id: &'a str,
    machine_id: &'a str,
) -> impl Fn(&Fields) -> bool + 'a {
    move |event| {
        event.get("target").map(String::as_str) == Some("sandbox_signal")
            && event.get("signal").map(String::as_str) == Some("sandbox.vmm_metrics")
            && event.get("sandbox_id").map(String::as_str) == Some(sandbox_id)
            && event.get("machine_id").map(String::as_str) == Some(machine_id)
    }
}

// Linux 6.15 added PIDFD_INFO_EXIT, which reports the exit status of a VMM
// that is not the host process's child.
fn kernel_reports_exit_status() -> anyhow::Result<bool> {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")?;
    let mut version = release
        .split(|character: char| !character.is_ascii_digit())
        .map(str::parse::<u32>);
    Ok((version.next().transpose()?, version.next().transpose()?) >= (Some(6), Some(15)))
}

#[tokio::test]
#[ignore = "requires root, KVM, TMPDIR on XFS, and the Firecracker host artifact bundle"]
async fn firecracker_metrics_reach_the_host_after_boot_and_restore() -> anyhow::Result<()> {
    use exoharness::{
        FirecrackerConfig, FirecrackerNetworkDevicePolicy, FirecrackerSandboxBackend,
        ManagedSandboxBackend, SandboxLifecycleConfig, SandboxNetworkPolicy, SandboxRequest,
        SandboxResourceShape, SandboxSpec, SnapshotKind,
    };

    let capture = Capture::default();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(capture.clone()))?;
    let root = tempfile::TempDir::new()?;
    let firecracker_bin = std::fs::canonicalize(std::env::var("EXO_FIRECRACKER_BINARY")?)?;
    let state_root = root.path().join("state");
    let backend = FirecrackerSandboxBackend::new(FirecrackerConfig {
        state_root: state_root.clone(),
        firecracker_bin: firecracker_bin.clone(),
        jailer_bin: std::env::var("EXO_FIRECRACKER_JAILER")?.into(),
        image_size_gib: 1,
        workspace_size_gib: 1,
        // Template capture needs a network device so a restore can enable
        // networking, even for a sandbox whose networking is disabled.
        network_device_policy: FirecrackerNetworkDevicePolicy::AllSandboxes,
        allow_guest_root: false,
        allowed_registries: vec!["docker.io".into()],
        ..Default::default()
    })
    .await?;
    let jails = std::fs::canonicalize(&state_root)?
        .join("jailer")
        .join(firecracker_bin.file_name().unwrap());
    let jail_root = |machine_id: &str| -> PathBuf { jails.join(machine_id).join("root") };
    let source = SandboxRequest {
        sandbox_id: "metrics-source".into(),
        scope: exoharness::ResourceScope::Global,
        provider_state: None,
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        spec: SandboxSpec {
            image: "docker.io/library/alpine:3.22".into(),
            resources: SandboxResourceShape::new(1, 512),
            default_workdir: "/workspace".into(),
            policy: SandboxNetworkPolicy::Disabled.into(),
            tcp_ports: vec![],
            mounts: vec![],
            durable_file_systems: vec![],
        },
    };
    let mut restored = source.clone();
    restored.sandbox_id = "metrics-restored".into();
    let result = async {
        // Firecracker writes a record when the microVM starts and another on
        // every FlushMetrics action.
        let handle = backend.acquire(source.clone()).await?;
        let source_machine = machine_id(handle.as_ref())?;
        flush_metrics(&jail_root(&source_machine))?;
        capture
            .wait_for(
                "the booted VMM's flushed record",
                observed_records(&source_machine, 2),
            )
            .await;

        // A restored VMM is a fresh process with its own metrics FIFO.
        let snapshot = handle.snapshot_template(SnapshotKind::Full).await?;
        backend.terminate(source.clone()).await?;
        let handle = backend
            .acquire_from_snapshot(restored.clone(), snapshot.clone())
            .await?;
        let restored_machine = machine_id(handle.as_ref())?;
        let jail = jail_root(&restored_machine);
        flush_metrics(&jail)?;
        capture
            .wait_for(
                "the restored VMM's flushed record",
                observed_records(&restored_machine, 2),
            )
            .await;

        // Firecracker's SIGSEGV handler flushes metrics, then exits with 150.
        let pid: i32 = std::fs::read_to_string(jail.join("firecracker.pid"))?
            .trim()
            .parse()?;
        rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::Signal::SEGV,
        )?;
        let matches = vmm_metrics_signal("metrics-restored", &restored_machine);
        let segv = capture
            .wait_for("the SIGSEGV signal", |event| {
                matches(event) && event.get("sigsegv").map(String::as_str) == Some("1")
            })
            .await;
        println!("observed {segv:?}");
        anyhow::ensure!(segv["severity"] == "signal", "unexpected {segv:?}");
        if kernel_reports_exit_status()? {
            let exit = capture
                .wait_for("the VMM exit status", |event| {
                    matches(event) && event.get("exit_code").is_some()
                })
                .await;
            println!("observed {exit:?}");
            anyhow::ensure!(exit["exit_code"] == "150", "unexpected {exit:?}");
        } else {
            println!("this kernel does not report the exit status of a non-child");
        }
        backend.delete_snapshot(snapshot).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = backend.terminate(restored).await;
    let source_cleanup = backend.terminate(source).await;
    result?;
    cleanup?;
    source_cleanup?;
    Ok(())
}
