use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn checked(command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .context("running filesystem volume command")?;
    ensure!(
        output.status.success(),
        "filesystem volume command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output)
}

#[cfg(target_os = "macos")]
pub(crate) fn create_volume(directory: &Path) -> Result<()> {
    checked(
        Command::new("hdiutil")
            .args([
                "create",
                "-quiet",
                "-size",
                "1t",
                "-type",
                "SPARSE",
                "-fs",
                "Case-sensitive APFS",
                "-volname",
                "ExoResource",
            ])
            .arg(directory.join("volume.sparseimage")),
    )?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn create_volume(directory: &Path) -> Result<()> {
    fs::create_dir_all(directory.join("workspace"))?;
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn clone_volume(source: &Path, target: &Path) -> Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let source = CString::new(source.join("volume.sparseimage").as_os_str().as_bytes())?;
    let target = CString::new(target.join("volume.sparseimage").as_os_str().as_bytes())?;
    if unsafe { libc::clonefile(source.as_ptr(), target.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("resource copies require an APFS filesystem");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn clone_volume(source: &Path, target: &Path) -> Result<()> {
    checked(
        Command::new("cp")
            .args(["-a", "--reflink=always", "--"])
            .arg(source.join("workspace"))
            .arg(target.join("workspace")),
    )
    .context("resource copies require a reflink-capable filesystem")?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn mounted(path: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(path.exists()
        && fs::metadata(path)?.dev() != fs::metadata(path.parent().context("mount parent")?)?.dev())
}

pub(crate) fn mount_volume(directory: &Path) -> Result<PathBuf> {
    mount_with_access(directory, false)
}

#[cfg(target_os = "macos")]
pub(crate) fn mount_read_only(directory: &Path) -> Result<PathBuf> {
    mount_with_access(directory, true)
}

fn mount_with_access(directory: &Path, read_only: bool) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    let mount = directory.join("mount");
    #[cfg(target_os = "macos")]
    if !mounted(&mount)? {
        fs::create_dir_all(&mount)?;
        let mut command = Command::new("hdiutil");
        command.args(["attach", "-quiet", "-nobrowse", "-owners", "on"]);
        if read_only {
            command.arg("-readonly");
        }
        checked(
            command
                .arg("-mountpoint")
                .arg(&mount)
                .arg(directory.join("volume.sparseimage")),
        )?;
    }
    #[cfg(target_os = "macos")]
    let workspace = mount.join("workspace");
    #[cfg(not(target_os = "macos"))]
    let workspace = directory.join("workspace");
    if read_only {
        ensure!(workspace.is_dir(), "prepared filesystem is missing");
    } else {
        fs::create_dir_all(&workspace)?;
    }
    Ok(workspace)
}

pub(crate) fn unmount_volume(directory: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let mount = directory.join("mount");
        if mounted(&mount)? {
            checked(Command::new("hdiutil").arg("detach").arg(&mount))?;
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = directory;
    Ok(())
}
