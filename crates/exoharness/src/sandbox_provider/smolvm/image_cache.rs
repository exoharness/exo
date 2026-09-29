use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::local_volume::{create_volume, mount_read_only, mount_volume, unmount_volume};

const PREPARE: &str = r#"
set -eu
set -o pipefail
export TMPDIR=/storage
case "$(uname -m)" in aarch64) arch=arm64;; x86_64) arch=amd64;; *) exit 1;; esac
archive=/source/image.tar
case "$1" in
    registry)
        crane config --platform "linux/$arch" "$2" > /output/config.json
        ;;
    gzip)
        gzip -dc "$archive" > /storage/image.tar
        archive=/storage/image.tar
        ;;
esac
if [ "$1" != registry ]; then
    config=$(tar -xOf "$archive" manifest.json | jq -er 'if length == 1 then .[0].Config | select(type == "string" and length > 0) else error("expected one image in the archive") end')
    tar -xOf "$archive" -- "$config" > /output/config.json
fi
jq -e --arg arch "$arch" 'if .architecture == $arch and .os == "linux" then true else error("image must target linux/" + $arch) end' /output/config.json >/dev/null
mkdir /output/0000_rootfs
printf '[exo image prep] rootfs export start: %s\n' "$(date +%s)"
if [ "$1" = registry ]; then
    crane export --platform "linux/$arch" "$2" - | tar -xp -C /output/0000_rootfs
else
    crane export - - < "$archive" | tar -xp -C /output/0000_rootfs
fi
printf '[exo image prep] rootfs export end: %s\n' "$(date +%s)"
printf '0000_rootfs\n' > /output/layer-order
"#;

pub(super) fn prepare(
    binary: &Path,
    boot_binary: Option<&Path>,
    cache: &Path,
    image: &str,
) -> Result<Option<PathBuf>> {
    let local_image = super::is_local_image_ref(image);
    let pinned_registry = !local_image && pinned_registry_image(image);
    // A digest pin is immutable, so an existing prepared image needs no
    // registry or Docker lookup. Tags still use their existing resolution path.
    let registry_cache_id = format!("registry-{:x}", Sha256::digest(image.as_bytes()));
    let registry_key = format!("v1-{}-{registry_cache_id}", std::env::consts::ARCH);
    if pinned_registry {
        let prepared = cache.join(&registry_key);
        if prepared.exists() {
            return mount_read_only(&prepared.canonicalize()?).map(Some);
        }
    }
    let docker_image = if local_image {
        None
    } else {
        docker_archive(cache, image)?
    };
    let registry_image = pinned_registry && docker_image.is_none();
    let (source_archive, digest, compressed, verify_digest) = if registry_image {
        (None, registry_cache_id, false, None)
    } else if let Some((archive, docker_digest)) = docker_image {
        (
            Some(archive),
            format!("docker-{docker_digest}"),
            false,
            None,
        )
    } else {
        let Some((digest, compressed)) = archive_key(image, cache)? else {
            return Ok(None);
        };
        (
            Some(PathBuf::from(image)),
            digest.clone(),
            compressed,
            Some(digest),
        )
    };
    fs::create_dir_all(cache)?;
    fs::set_permissions(cache, fs::Permissions::from_mode(0o700))?;
    let cache = cache.canonicalize()?;
    let key = format!("v1-{}-{digest}", std::env::consts::ARCH);
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(cache.join(format!("{key}.lock")))?;
    lock.lock()?;
    let destination = cache.join(&key);
    if !destination.exists() {
        tracing::info!(target: "exoharness::progress", "Preparing SmolVM image (cached for new threads)");
        let staging = tempfile::Builder::new()
            .prefix("prepare-")
            .tempdir_in(&cache)?;
        let source = staging.path().join("source");
        fs::create_dir(&source)?;
        let archive = source.join("image.tar");
        if let Some(image_archive) = source_archive {
            let copied = Command::new("cp")
                .arg("-c")
                .arg(image_archive.canonicalize()?)
                .arg(&archive)
                .output()
                .context("snapshotting image archive")?;
            ensure!(
                copied.status.success(),
                "snapshotting image archive: {}",
                String::from_utf8_lossy(&copied.stderr).trim()
            );
            if let Some(verify_digest) = verify_digest {
                ensure!(
                    hash_file(&archive)? == verify_digest,
                    "image archive changed while preparing it; retry"
                );
            }
        }

        let volume_started = Instant::now();
        create_volume(staging.path())?;
        image_cache_timing("create volume", volume_started);
        let result = (|| {
            let mount_started = Instant::now();
            let output = mount_volume(staging.path())?;
            image_cache_timing("mount preparation volume", mount_started);
            let mut command = Command::new(binary);
            command.args([
                "machine",
                "run",
                "--cpus",
                "2",
                "--mem",
                "1024",
                "--timeout",
                "10m",
            ]);
            if registry_image {
                command.arg("--net");
            } else {
                command
                    .arg("--volume")
                    .arg(format!("{}:/source:ro", source.display()));
            }
            command
                .arg("--volume")
                .arg(format!("{}:/output:rw", output.display()))
                .args(["--", "sh", "-c", PREPARE, "prepare-image"])
                .arg(if registry_image {
                    "registry"
                } else if compressed {
                    "gzip"
                } else {
                    "tar"
                })
                .arg(image);
            if let Some(boot_binary) = boot_binary {
                command.env(super::SMOLVM_BOOT_BIN_ENV, boot_binary);
            }
            let run_started = Instant::now();
            let output = command
                .output()
                .context("starting SmolVM image preparation")?;
            image_cache_timing("run image preparation VM", run_started);
            if std::env::var_os("EXO_RESOURCE_TIMING").is_some() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    if line.starts_with("[exo image prep]") {
                        eprintln!("{line}");
                    }
                }
            }
            ensure!(
                output.status.success(),
                "SmolVM image preparation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(())
        })();
        let unmount_started = Instant::now();
        if let Err(error) = unmount_volume(staging.path()) {
            let path = staging.keep();
            return Err(error).with_context(|| {
                format!("unmounting image preparation volume at {}", path.display())
            });
        }
        image_cache_timing("unmount preparation volume", unmount_started);
        if let Err(error) = result {
            if registry_image {
                // SmolVM's own registry pull may have credentials unavailable
                // to crane in the preparation VM. Preserve that path.
                tracing::warn!(
                    target: "exoharness::progress",
                    image,
                    %error,
                    "SmolVM image could not be cached; pulling it at machine start"
                );
                return Ok(None);
            }
            return Err(error);
        }
        fs::remove_dir_all(source)?;
        fs::rename(staging.path(), &destination)?;
    }
    let mount_started = Instant::now();
    let prepared = mount_read_only(&destination)?;
    image_cache_timing("mount cached image", mount_started);
    Ok(Some(prepared))
}

fn image_cache_timing(stage: &str, started: Instant) {
    if std::env::var_os("EXO_RESOURCE_TIMING").is_some() {
        eprintln!(
            "[exo resource timing] SmolVM image cache {stage}: {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
}

fn pinned_registry_image(image: &str) -> bool {
    image.rsplit_once("@sha256:").is_some_and(|(name, digest)| {
        !name.is_empty()
            && digest.len() == 64
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn docker_archive(cache: &Path, image: &str) -> Result<Option<(PathBuf, String)>> {
    let output = match Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", image])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("checking the local Docker image store"),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let id = String::from_utf8(output.stdout)?;
    let id = id.trim();
    let digest = id
        .strip_prefix("sha256:")
        .context("Docker image has no content digest")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid Docker image digest"
    );
    fs::create_dir_all(cache)?;
    fs::set_permissions(cache, fs::Permissions::from_mode(0o700))?;
    let archive = cache.join(format!("docker-{digest}.tar"));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(cache.join(format!("docker-{digest}.lock")))?;
    lock.lock()?;
    if !archive.exists() {
        tracing::info!(target: "exoharness::progress", "Importing local Docker image {image} for SmolVM");
        let staging = tempfile::NamedTempFile::new_in(cache)?;
        let output = Command::new("docker")
            .args(["image", "save", "--output"])
            .arg(staging.path())
            .arg(id)
            .output()
            .context("exporting the local Docker image")?;
        ensure!(
            output.status.success(),
            "exporting Docker image {image}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        staging.persist(&archive)?;
    }
    Ok(Some((archive, digest.to_string())))
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct ArchiveIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

impl ArchiveIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.len(),
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanos: metadata.ctime_nsec(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct ArchiveKeyMemo {
    identity: ArchiveIdentity,
    digest: String,
    compressed: bool,
}

fn archive_key(image: &str, cache: &Path) -> Result<Option<(String, bool)>> {
    if !(image.ends_with(".tar") || image.ends_with(".tar.gz") || image.ends_with(".tgz"))
        || Path::new(image).is_dir()
    {
        return Ok(None);
    }
    fs::create_dir_all(cache)?;
    fs::set_permissions(cache, fs::Permissions::from_mode(0o700))?;
    let canonical = Path::new(image).canonicalize()?;
    let path_digest = format!("{:x}", Sha256::digest(canonical.as_os_str().as_bytes()));
    let memo_path = cache.join(format!("archive-key-{path_digest}.json"));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(cache.join(format!("archive-key-{path_digest}.lock")))?;
    lock.lock()?;
    let mut file = File::open(image).with_context(|| format!("opening image archive {image}"))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "image archive must be a file");
    let identity = ArchiveIdentity::from_metadata(&metadata);
    if let Ok(contents) = fs::read(&memo_path)
        && let Ok(memo) = serde_json::from_slice::<ArchiveKeyMemo>(&contents)
        && memo.identity == identity
        && identity == ArchiveIdentity::from_metadata(&file.metadata()?)
    {
        return Ok(Some((memo.digest, memo.compressed)));
    }
    let mut header = [0u8; 4];
    file.read_exact(&mut header)
        .context("reading image archive header")?;
    // Other archive formats continue through SmolVM's own importer.
    if header == [0x28, 0xb5, 0x2f, 0xfd] {
        return Ok(None);
    }
    file.rewind()?;
    let memo = ArchiveKeyMemo {
        identity,
        digest: hash(&mut file)?,
        compressed: header.starts_with(&[0x1f, 0x8b]),
    };
    ensure!(
        memo.identity == ArchiveIdentity::from_metadata(&file.metadata()?),
        "image archive changed while hashing it; retry"
    );
    let staging = tempfile::NamedTempFile::new_in(cache)?;
    serde_json::to_writer(&staging, &memo)?;
    staging.persist(&memo_path)?;
    Ok(Some((memo.digest, memo.compressed)))
}

fn hash_file(path: &Path) -> Result<String> {
    hash(&mut File::open(path)?)
}

fn hash(file: &mut File) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_identity_tracks_contents_instead_of_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.tar");
        let second = dir.path().join("second.tgz");
        fs::write(&first, b"first image").unwrap();
        fs::write(&second, b"first image").unwrap();
        let original = archive_key(first.to_str().unwrap(), dir.path()).unwrap();
        assert_eq!(
            original,
            archive_key(second.to_str().unwrap(), dir.path()).unwrap()
        );
        assert_eq!(
            original,
            archive_key(first.to_str().unwrap(), dir.path()).unwrap()
        );
        fs::write(&first, b"other image").unwrap();
        assert_ne!(
            original,
            archive_key(first.to_str().unwrap(), dir.path()).unwrap()
        );
    }

    #[test]
    fn directories_and_registry_images_pass_through() {
        assert!(
            archive_key("alpine:latest", Path::new("."))
                .unwrap()
                .is_none()
        );
        assert!(archive_key("-", Path::new(".")).unwrap().is_none());
        let dir = tempfile::Builder::new().suffix(".tar").tempdir().unwrap();
        assert!(
            archive_key(dir.path().to_str().unwrap(), dir.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[ignore = "requires SmolVM and APFS disk images"]
    fn failed_preparation_does_not_publish_or_leave_mounted_volumes() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let cache = temp.path().join("cache");
        let image = temp.path().join("invalid.tar");
        fs::write(&image, "not a Docker image archive")?;
        let binary = std::env::var_os("SMOLVM_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("smolvm"));
        let error = prepare(&binary, None, &cache, image.to_str().unwrap()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("SmolVM image preparation failed"),
            "{error:#}"
        );
        let entries = fs::read_dir(&cache)?.collect::<std::io::Result<Vec<_>>>()?;
        assert!(entries.iter().all(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "lock" || ext == "json")
        }));
        for entry in entries
            .iter()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lock"))
        {
            File::open(entry.path())?.try_lock()?;
        }
        Ok(())
    }
}
