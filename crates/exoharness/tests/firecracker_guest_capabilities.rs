#![cfg(feature = "firecracker")]

// Runs as guest root through sudo. Root still writes system files and mounts
// filesystems, but cannot open the raw memory or I/O port devices.
const PROBE: &str = r#"
import ctypes, errno, os

libc = ctypes.CDLL(None, use_errno=True)
mount_point = b"/mnt/exo-capability-check"

def attempt(action):
    try:
        action()
        return "ok"
    except OSError as error:
        return errno.errorcode[error.errno]

def check(result):
    if result != 0:
        raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))

def open_device(path):
    os.close(os.open(path, os.O_RDONLY))

def write_root_file():
    with open("/etc/exo-capability-check", "w") as file:
        file.write("root\n")

def mount_tmpfs():
    os.makedirs(mount_point, exist_ok=True)
    check(libc.mount(b"tmpfs", mount_point, b"tmpfs", 0, None))
    check(libc.umount2(mount_point, 0))

with open("/proc/self/status") as status:
    for line in status:
        if line.startswith(("Uid:", "CapEff:", "CapBnd:")):
            print(" ".join(line.split()))
for device in ("/dev/mem", "/dev/port"):
    print(device, attempt(lambda: open_device(device)))
print("write", attempt(write_root_file))
print("mount", attempt(mount_tmpfs))
"#;

// CAP_SYS_MODULE, CAP_SYS_RAWIO, CAP_SYS_BOOT, CAP_PERFMON, and CAP_BPF.
const DROPPED: u64 = 1 << 16 | 1 << 17 | 1 << 22 | 1 << 38 | 1 << 39;
const CAP_SYS_ADMIN: u64 = 1 << 21;

#[tokio::test]
#[ignore = "requires root, KVM, TMPDIR on XFS, the Firecracker host artifact bundle, and EXO_TEST_FIRECRACKER_SUDO_IMAGE"]
async fn firecracker_guest_root_lacks_device_and_boot_capabilities() -> anyhow::Result<()> {
    use exoharness::{
        FirecrackerConfig, FirecrackerSandboxBackend, ManagedSandboxBackend, SandboxCommand,
        SandboxLifecycleConfig, SandboxNetworkPolicy, SandboxRequest, SandboxResourceShape,
        SandboxSpec,
    };
    use std::time::Duration;

    let root = tempfile::TempDir::new()?;
    let image = std::env::var("EXO_TEST_FIRECRACKER_SUDO_IMAGE")?;
    let backend = FirecrackerSandboxBackend::new(FirecrackerConfig {
        state_root: root.path().join("state"),
        firecracker_bin: std::env::var("EXO_FIRECRACKER_BINARY")?.into(),
        jailer_bin: std::env::var("EXO_FIRECRACKER_JAILER")?.into(),
        workspace_size_gib: 1,
        allow_guest_root: true,
        allowed_local_images: vec![image.clone().into()],
        ..Default::default()
    })
    .await?;
    let request = SandboxRequest {
        sandbox_id: "guest-capabilities".into(),
        scope: exoharness::ResourceScope::Global,
        provider_state: None,
        // Without an idle TTL the VM is one-shot and is torn down after the
        // first command; this test runs two.
        lifecycle: SandboxLifecycleConfig {
            idle_ttl: Some(Duration::from_secs(300)),
        },
        spec: SandboxSpec {
            image,
            resources: SandboxResourceShape::new(1, 512),
            default_workdir: "/home/exo/workspace".into(),
            policy: SandboxNetworkPolicy::Disabled.into(),
            tcp_ports: vec![],
            mounts: vec![],
            durable_file_systems: vec![],
        },
    };
    let command = |argv: &[&str]| SandboxCommand {
        argv: argv.iter().map(ToString::to_string).collect(),
        timeout: Some(Duration::from_secs(30)),
        env: Default::default(),
        cwd: None,
        display_argv: None,
    };
    let result = async {
        let handle = backend.acquire(request.clone()).await?;
        let user = handle
            .exec(&command(&["/bin/grep", "^CapBnd:", "/proc/self/status"]))
            .await?;
        anyhow::ensure!(user.ok, "reading workload capabilities failed: {user:?}");
        anyhow::ensure!(
            capability_set(&user.stdout, "CapBnd:")? & DROPPED == 0,
            "workload bounding set kept dropped capabilities: {user:?}"
        );

        let root = handle
            .exec(&command(&[
                "/usr/bin/sudo",
                "-n",
                "/usr/bin/python3",
                "-c",
                PROBE,
            ]))
            .await?;
        println!("{}", root.stdout);
        anyhow::ensure!(root.ok, "guest root probe failed: {root:?}");
        for expected in [
            "Uid: 0 0 0 0",
            "/dev/mem EPERM",
            "/dev/port EPERM",
            "write ok",
            "mount ok",
        ] {
            anyhow::ensure!(
                root.stdout.lines().any(|line| line == expected),
                "guest root probe did not report {expected:?}: {root:?}"
            );
        }
        let bounding = capability_set(&root.stdout, "CapBnd:")?;
        let effective = capability_set(&root.stdout, "CapEff:")?;
        anyhow::ensure!(
            bounding & DROPPED == 0 && effective & DROPPED == 0,
            "guest root kept dropped capabilities: {root:?}"
        );
        anyhow::ensure!(
            effective & CAP_SYS_ADMIN != 0,
            "guest root lost CAP_SYS_ADMIN: {root:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = backend.terminate(request).await;
    result?;
    cleanup?;
    Ok(())
}

fn capability_set(status: &str, field: &str) -> anyhow::Result<u64> {
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix(field))
        .ok_or_else(|| anyhow::anyhow!("missing {field} in {status:?}"))?;
    Ok(u64::from_str_radix(value.trim(), 16)?)
}
