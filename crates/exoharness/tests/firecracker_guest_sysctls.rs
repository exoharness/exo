#![cfg(feature = "firecracker")]

// Syscall numbers come from the x86_64 and generic (arm64) Linux tables.
// Without the sysctls, root sees ENOEXEC or EBADF for these arguments.
const PROBE: &str = r#"
import ctypes, errno, os

libc = ctypes.CDLL(None, use_errno=True)
init_module, finit_module, kexec_file_load = {
    "x86_64": (175, 313, 320),
    "aarch64": (105, 273, 294),
}[os.uname().machine]
KEXEC_FILE_NO_INITRAMFS = 4

def call(number, *arguments):
    ctypes.set_errno(0)
    if libc.syscall(number, *arguments) != -1:
        return "ok"
    return errno.errorcode[ctypes.get_errno()]

print("uid", os.getuid())
for name in ("modules_disabled", "kexec_load_disabled"):
    path = "/proc/sys/kernel/" + name
    try:
        descriptor = os.open(path, os.O_WRONLY)
        try:
            os.write(descriptor, b"0")
        finally:
            os.close(descriptor)
        reset = "accepted"
    except OSError:
        reset = "rejected"
    with open(path) as value:
        print(name, value.read().strip(), reset)
print("init_module", call(init_module, None, ctypes.c_ulong(0), b""))
print("finit_module", call(finit_module, -1, b"", 0))
print("kexec_file_load", call(
    kexec_file_load, -1, -1, ctypes.c_ulong(0), None,
    ctypes.c_ulong(KEXEC_FILE_NO_INITRAMFS),
))
"#;

#[tokio::test]
#[ignore = "requires root, KVM, TMPDIR on XFS, the Firecracker host artifact bundle, and EXO_TEST_FIRECRACKER_SUDO_IMAGE"]
async fn firecracker_guest_root_cannot_load_kernel_code() -> anyhow::Result<()> {
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
        sandbox_id: "guest-sysctls".into(),
        scope: exoharness::ResourceScope::Global,
        provider_state: None,
        lifecycle: SandboxLifecycleConfig::default(),
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
    let result = async {
        let handle = backend.acquire(request.clone()).await?;
        let output = handle
            .exec(&SandboxCommand {
                argv: vec![
                    "/usr/bin/sudo".into(),
                    "-n".into(),
                    "/usr/bin/python3".into(),
                    "-c".into(),
                    PROBE.into(),
                ],
                timeout: Some(Duration::from_secs(30)),
                env: Default::default(),
                cwd: None,
                display_argv: None,
            })
            .await?;
        println!("{}", output.stdout);
        anyhow::ensure!(output.ok, "guest probe failed: {output:?}");
        anyhow::ensure!(
            output.stdout
                == "uid 0\n\
                    modules_disabled 1 rejected\n\
                    kexec_load_disabled 1 rejected\n\
                    init_module EPERM\n\
                    finit_module EPERM\n\
                    kexec_file_load EPERM\n",
            "guest root reached kernel code loading: {output:?}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = backend.terminate(request).await;
    result?;
    cleanup?;
    Ok(())
}
