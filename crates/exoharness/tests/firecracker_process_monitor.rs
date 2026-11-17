#![cfg(all(feature = "firecracker", target_os = "linux"))]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

const MONITOR_TARGET: &str = "exoharness::sandbox_provider::firecracker::process_monitor";
// Longer than the monitor's scan interval, so every running VMM is scanned.
const SCAN_WAIT: Duration = Duration::from_secs(7);

#[derive(Debug)]
struct Captured {
    target: String,
    fields: BTreeMap<String, String>,
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
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        let target = event.metadata().target();
        if target != "sandbox_signal" && target != MONITOR_TARGET {
            return;
        }
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor(&mut fields));
        self.0.lock().unwrap().push(Captured {
            target: target.to_string(),
            fields,
        });
    }
}

#[tokio::test]
#[ignore = "requires root, KVM, TMPDIR on XFS, and the Firecracker host artifact bundle"]
async fn firecracker_lifecycle_raises_no_process_monitor_signals() -> anyhow::Result<()> {
    use exoharness::{
        FirecrackerConfig, FirecrackerNetworkDevicePolicy, FirecrackerSandboxBackend,
        ManagedSandboxBackend, SandboxCommand, SandboxLifecycleConfig, SandboxNetworkPolicy,
        SandboxRequest, SandboxResourceShape, SandboxSpec, SnapshotKind,
    };

    // The monitor emits from its own thread, so capture needs the global
    // subscriber, installed before the backend starts the monitor.
    let captured = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(Capture(Arc::clone(&captured))),
    )?;

    let root = tempfile::TempDir::new()?;
    let backend = FirecrackerSandboxBackend::new(FirecrackerConfig {
        state_root: root.path().join("state"),
        firecracker_bin: std::env::var("EXO_FIRECRACKER_BINARY")?.into(),
        jailer_bin: std::env::var("EXO_FIRECRACKER_JAILER")?.into(),
        image_size_gib: 1,
        workspace_size_gib: 1,
        network_device_policy: FirecrackerNetworkDevicePolicy::AllSandboxes,
        allow_guest_root: false,
        allowed_registries: vec!["docker.io".into()],
        ..Default::default()
    })
    .await?;
    let source = SandboxRequest {
        sandbox_id: "monitor-source".into(),
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
    restored.sandbox_id = "monitor-restored".into();
    let command = SandboxCommand {
        argv: vec![
            "/bin/sh".into(),
            "-c".into(),
            "for i in 1 2 3; do ls / >/dev/null; done; cat /proc/cpuinfo >/dev/null; echo ok"
                .into(),
        ],
        timeout: Some(Duration::from_secs(30)),
        env: Default::default(),
        cwd: None,
        display_argv: None,
    };
    let result = async {
        backend.acquire(source.clone()).await?;
        // Acquiring the warm sandbox again reuses, and adopts, its running VMM.
        let handle = backend.acquire(source.clone()).await?;
        let output = handle.exec(&command).await?;
        anyhow::ensure!(output.ok, "source exec failed: {output:?}");
        tokio::time::sleep(SCAN_WAIT).await;
        let snapshot = handle.snapshot_template(SnapshotKind::Full).await?;
        let output = handle.exec(&command).await?;
        anyhow::ensure!(output.ok, "exec after snapshot failed: {output:?}");
        let clone = backend
            .acquire_from_snapshot(restored.clone(), snapshot.clone())
            .await?;
        let output = clone.exec(&command).await?;
        anyhow::ensure!(output.ok, "restored exec failed: {output:?}");
        tokio::time::sleep(SCAN_WAIT).await;
        backend.stop(source.clone()).await?;
        backend.terminate(restored.clone()).await?;
        backend.delete_snapshot(snapshot).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = backend.terminate(restored).await;
    let source_cleanup = backend.terminate(source).await;
    result?;
    cleanup?;
    source_cleanup?;

    let captured = captured.lock().unwrap();
    let signals = captured
        .iter()
        .filter(|event| event.target == "sandbox_signal")
        .collect::<Vec<_>>();
    anyhow::ensure!(
        signals.is_empty(),
        "unexpected sandbox signals: {signals:#?}"
    );
    let monitored = captured
        .iter()
        .filter(|event| {
            event.fields.get("message").map(String::as_str) == Some("monitoring Firecracker VMM")
        })
        .filter_map(|event| event.fields.get("machine_id"))
        .collect::<std::collections::BTreeSet<_>>();
    anyhow::ensure!(
        monitored.len() >= 2,
        "the monitor did not track both the source and the restored VMM: {monitored:?}"
    );
    let busiest_scan = captured
        .iter()
        .filter(|event| {
            event.fields.get("message").map(String::as_str)
                == Some("scanned Firecracker VMM processes")
        })
        .filter_map(|event| event.fields.get("vmms")?.parse::<usize>().ok())
        .max();
    anyhow::ensure!(
        busiest_scan >= Some(2),
        "no scan covered both running VMMs: {busiest_scan:?}"
    );
    println!("monitored {monitored:?}; busiest scan covered {busiest_scan:?} VMMs; no signals");
    Ok(())
}
