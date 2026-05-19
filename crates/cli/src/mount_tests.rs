use std::path::PathBuf;

use executor::{FileSystemMount, FileSystemMountMode, SANDBOX_MAIN_MOUNT_DIR};

use crate::default_mount_path;

#[test]
fn default_mount_path_uses_host_basename_under_workspace_root() {
    let host_path = PathBuf::from("workspace").join("sample-app");
    let mount_path = default_mount_path(&host_path, &[]);

    assert_eq!(mount_path, format!("{SANDBOX_MAIN_MOUNT_DIR}/sample-app"));
}

#[test]
fn default_mount_path_suffixes_when_guest_path_collides() {
    let host_path = PathBuf::from("workspace").join("sample-app");
    let existing_mounts = vec![
        FileSystemMount {
            host_path: "workspace/one".to_string(),
            mount_path: format!("{SANDBOX_MAIN_MOUNT_DIR}/sample-app"),
            mode: FileSystemMountMode::ReadOnly,
            internal: Some(false),
        },
        FileSystemMount {
            host_path: "workspace/two".to_string(),
            mount_path: format!("{SANDBOX_MAIN_MOUNT_DIR}/sample-app-2"),
            mode: FileSystemMountMode::ReadOnly,
            internal: Some(false),
        },
    ];

    let mount_path = default_mount_path(&host_path, &existing_mounts);

    assert_eq!(mount_path, format!("{SANDBOX_MAIN_MOUNT_DIR}/sample-app-3"));
}
