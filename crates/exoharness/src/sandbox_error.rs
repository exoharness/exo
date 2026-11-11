use serde::{Deserialize, Serialize};

/// An expected sandbox failure, preserved across backend and bridge boundaries.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SandboxFailure {
    #[error("sandbox VM capacity exhausted: {occupied} occupied, limit {limit}")]
    VmCapacityExhausted { limit: usize, occupied: usize },
    #[error(
        "snapshot storage budget exhausted: {accounted_bytes} bytes accounted, limit {limit_bytes}"
    )]
    SnapshotBudgetExhausted {
        limit_bytes: u64,
        accounted_bytes: u64,
    },
    #[error(
        "insufficient snapshot disk space: {required_bytes} bytes required, {available_bytes} available after host reserve"
    )]
    DiskSpaceExhausted {
        required_bytes: u64,
        available_bytes: u64,
    },
    #[error("snapshot unavailable: {message}")]
    SnapshotUnavailable { message: String },
    #[error("invalid sandbox request: {message}")]
    InvalidRequest { message: String },
}

/// Guidance for a failed acquisition, not proof that the backend rolled it back.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxRetryAdvice {
    /// Retry cannot fix this request.
    Never,
    /// Try a compatible backend only after terminating the failed acquisition.
    DifferentBackendAfterCleanup,
    /// Recover the checkpoint or rebuild a template before attempting restoration.
    RecoverSnapshot,
    /// The outcome is unknown; this does not authorize creating another sandbox.
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error(transparent)]
    Failure(SandboxFailure),
    #[error(transparent)]
    Internal(anyhow::Error),
}

pub type SandboxResult<T> = std::result::Result<T, SandboxError>;

impl SandboxError {
    pub fn failure(&self) -> Option<&SandboxFailure> {
        match self {
            Self::Failure(failure) => Some(failure),
            Self::Internal(_) => None,
        }
    }

    /// Applies to acquisition failures. A capture failure must leave its source intact.
    /// The caller chooses eligible backends and backoff; cleanup failure forbids failover.
    pub fn acquisition_retry(&self) -> SandboxRetryAdvice {
        match self {
            Self::Failure(
                SandboxFailure::VmCapacityExhausted { .. }
                | SandboxFailure::SnapshotBudgetExhausted { .. }
                | SandboxFailure::DiskSpaceExhausted { .. },
            ) => SandboxRetryAdvice::DifferentBackendAfterCleanup,
            Self::Failure(SandboxFailure::SnapshotUnavailable { .. }) => {
                SandboxRetryAdvice::RecoverSnapshot
            }
            Self::Failure(SandboxFailure::InvalidRequest { .. }) => SandboxRetryAdvice::Never,
            Self::Internal(_) => SandboxRetryAdvice::Unknown,
        }
    }
}

impl From<SandboxFailure> for SandboxError {
    fn from(failure: SandboxFailure) -> Self {
        Self::Failure(failure)
    }
}

impl From<anyhow::Error> for SandboxError {
    fn from(error: anyhow::Error) -> Self {
        if matches!(error.downcast_ref::<Self>(), Some(Self::Internal(_))) {
            return Self::Internal(error);
        }
        let error = match error.downcast::<Self>() {
            Ok(error) => return error,
            Err(error) => error,
        };
        let error = match error.downcast::<SandboxFailure>() {
            Ok(failure) => return Self::Failure(failure),
            Err(error) => error,
        };
        #[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
        let error = match error.downcast::<crate::SnapshotBudgetExhausted>() {
            Ok(error) => {
                return SandboxFailure::SnapshotBudgetExhausted {
                    limit_bytes: error.limit_bytes,
                    accounted_bytes: error.accounted_bytes,
                }
                .into();
            }
            Err(error) => error,
        };
        Self::Internal(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classified_failures_survive_context_without_promising_rollback() {
        let failure = SandboxFailure::VmCapacityExhausted {
            limit: 4,
            occupied: 4,
        };
        let error = SandboxError::from(anyhow::Error::new(failure.clone()).context("acquiring VM"));
        assert_eq!(error.failure(), Some(&failure));
        assert_eq!(
            error.acquisition_retry(),
            SandboxRetryAdvice::DifferentBackendAfterCleanup
        );
        let round_trip = SandboxError::from(anyhow::Error::new(error).context("backend operation"));
        assert_eq!(round_trip.failure(), Some(&failure));
    }

    #[test]
    #[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
    fn existing_snapshot_budget_errors_retain_accounting_through_context() {
        let error = anyhow::Error::new(crate::SnapshotBudgetExhausted {
            limit_bytes: 100,
            accounted_bytes: 101,
        })
        .context("capturing snapshot");
        assert!(matches!(
            SandboxError::from(error),
            SandboxError::Failure(SandboxFailure::SnapshotBudgetExhausted {
                limit_bytes: 100,
                accounted_bytes: 101
            })
        ));
    }

    #[test]
    fn recovery_and_invalid_input_do_not_retry_acquisition_blindly() {
        for (failure, expected) in [
            (
                SandboxFailure::InvalidRequest {
                    message: "invalid workdir".into(),
                },
                SandboxRetryAdvice::Never,
            ),
            (
                SandboxFailure::SnapshotUnavailable {
                    message: "capture is missing".into(),
                },
                SandboxRetryAdvice::RecoverSnapshot,
            ),
        ] {
            assert_eq!(SandboxError::from(failure).acquisition_retry(), expected);
        }
        let error = SandboxError::from(
            anyhow::Error::new(SandboxError::Internal(anyhow::anyhow!("connection closed")))
                .context("acquiring remote sandbox"),
        );
        assert!(format!("{error:#}").contains("acquiring remote sandbox"));
        assert_eq!(error.acquisition_retry(), SandboxRetryAdvice::Unknown);
        assert!(error.failure().is_none());
    }
}
