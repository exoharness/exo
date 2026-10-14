use std::fs::{File, TryLockError};
use std::io;

pub(super) struct SnapshotLease(File);

impl SnapshotLease {
    pub(super) fn exclusive(file: File) -> io::Result<Self> {
        file.lock()?;
        Ok(Self(file))
    }

    pub(super) fn shared(file: File) -> io::Result<Self> {
        file.lock_shared()?;
        Ok(Self(file))
    }

    pub(super) fn try_exclusive(file: File) -> Result<Self, TryLockError> {
        file.try_lock()?;
        Ok(Self(file))
    }
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            tracing::warn!(%error, "failed to release snapshot lease");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_drop_unlocks_even_with_an_inherited_descriptor() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("lease");
        for shared in [false, true] {
            let file = File::options()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)?;
            let lease = if shared {
                SnapshotLease::shared(file)?
            } else {
                SnapshotLease::exclusive(file)?
            };
            let inherited = lease.0.try_clone()?;
            let contender = File::open(&path)?;
            assert!(matches!(
                contender.try_lock(),
                Err(TryLockError::WouldBlock)
            ));
            drop(lease);
            let acquired =
                SnapshotLease::try_exclusive(contender).expect("dropped lease still locked");
            drop(acquired);
            drop(inherited);
        }
        Ok(())
    }
}
