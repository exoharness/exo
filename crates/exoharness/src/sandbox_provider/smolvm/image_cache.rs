use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use crate::local_volume::{create_volume, mount_read_only, mount_volume, unmount_volume};

const PREPARE: &str = r#"
set -eu
set -o pipefail
export TMPDIR=/storage
archive=/source/image.tar
if [ "$1" = gzip ]; then
    gzip -dc "$archive" > /storage/image.tar
    archive=/storage/image.tar
fi
config=$(tar -xOf "$archive" manifest.json | jq -er 'if length == 1 then .[0].Config | select(type == "string" and length > 0) else error("expected one image in the archive") end')
tar -xOf "$archive" -- "$config" > /output/config.json
case "$(uname -m)" in aarch64) arch=arm64;; x86_64) arch=amd64;; *) exit 1;; esac
jq -e --arg arch "$arch" 'if .architecture == $arch and .os == "linux" then true else error("image must target linux/" + $arch) end' /output/config.json >/dev/null
mkdir /output/0000_rootfs
crane export - - < "$archive" | tar -xp -C /output/0000_rootfs
printf '0000_rootfs\n' > /output/layer-order
"#;

pub(super) fn prepare(
    binary: &Path,
    boot_binary: Option<&Path>,
    cache: &Path,
    image: &str,
) -> Result<Option<PathBuf>> {
    let Some((digest, compressed)) = archive_key(image)? else {
        return Ok(None);
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
        eprintln!("Preparing SmolVM image (cached for new threads)...");
        let staging = tempfile::Builder::new()
            .prefix("prepare-")
            .tempdir_in(&cache)?;
        let source = staging.path().join("source");
        fs::create_dir(&source)?;
        let archive = source.join("image.tar");
        let copied = Command::new("cp")
            .arg("-c")
            .arg(Path::new(image).canonicalize()?)
            .arg(&archive)
            .output()
            .context("snapshotting image archive")?;
        ensure!(
            copied.status.success(),
            "snapshotting image archive: {}",
            String::from_utf8_lossy(&copied.stderr).trim()
        );
        ensure!(
            hash_file(&archive)? == digest,
            "image archive changed while preparing it; retry"
        );

        create_volume(staging.path())?;
        let result = (|| {
            let output = mount_volume(staging.path())?;
            let mut command = Command::new(binary);
            command
                .args([
                    "machine",
                    "run",
                    "--cpus",
                    "2",
                    "--mem",
                    "1024",
                    "--timeout",
                    "10m",
                ])
                .arg("--volume")
                .arg(format!("{}:/source:ro", source.display()))
                .arg("--volume")
                .arg(format!("{}:/output:rw", output.display()))
                .args([
                    "--",
                    "sh",
                    "-c",
                    PREPARE,
                    "prepare-image",
                    if compressed { "gzip" } else { "tar" },
                ]);
            if let Some(boot_binary) = boot_binary {
                command.env(super::SMOLVM_BOOT_BIN_ENV, boot_binary);
            }
            let output = command
                .output()
                .context("starting SmolVM image preparation")?;
            ensure!(
                output.status.success(),
                "SmolVM image preparation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(())
        })();
        if let Err(error) = unmount_volume(staging.path()) {
            let path = staging.keep();
            return Err(error).with_context(|| {
                format!("unmounting image preparation volume at {}", path.display())
            });
        }
        result?;
        fs::remove_dir_all(source)?;
        fs::rename(staging.path(), &destination)?;
    }
    mount_read_only(&destination).map(Some)
}

fn archive_key(image: &str) -> Result<Option<(String, bool)>> {
    if !(image.ends_with(".tar") || image.ends_with(".tar.gz") || image.ends_with(".tgz"))
        || Path::new(image).is_dir()
    {
        return Ok(None);
    }
    let mut file = File::open(image).with_context(|| format!("opening image archive {image}"))?;
    ensure!(file.metadata()?.is_file(), "image archive must be a file");
    let mut header = [0u8; 4];
    file.read_exact(&mut header)
        .context("reading image archive header")?;
    // Other archive formats continue through SmolVM's own importer.
    if header == [0x28, 0xb5, 0x2f, 0xfd] {
        return Ok(None);
    }
    file.rewind()?;
    Ok(Some((hash(&mut file)?, header.starts_with(&[0x1f, 0x8b]))))
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
        let original = archive_key(first.to_str().unwrap()).unwrap();
        assert_eq!(original, archive_key(second.to_str().unwrap()).unwrap());
        fs::write(&first, b"other image").unwrap();
        assert_ne!(original, archive_key(first.to_str().unwrap()).unwrap());
    }

    #[test]
    fn directories_and_registry_images_pass_through() {
        assert!(archive_key("alpine:latest").unwrap().is_none());
        assert!(archive_key("-").unwrap().is_none());
        let dir = tempfile::Builder::new().suffix(".tar").tempdir().unwrap();
        assert!(archive_key(dir.path().to_str().unwrap()).unwrap().is_none());
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
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path().extension().unwrap(), "lock");
        let lock = File::open(entries[0].path())?;
        lock.try_lock()?;
        Ok(())
    }
}
