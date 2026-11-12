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
    #[error("{source:#}")]
    Failure {
        failure: SandboxFailure,
        #[source]
        source: anyhow::Error,
    },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type SandboxResult<T> = std::result::Result<T, SandboxError>;

impl SandboxError {
    pub fn failure(&self) -> Option<&SandboxFailure> {
        match self {
            Self::Failure { failure, .. } => Some(failure),
            Self::Internal(_) => None,
        }
    }

    #[cfg(any(test, all(not(target_arch = "wasm32"), feature = "firecracker")))]
    pub(crate) fn context(self, context: impl std::fmt::Display + Send + Sync + 'static) -> Self {
        match self {
            Self::Failure { failure, source } => Self::Failure {
                failure,
                source: source.context(context),
            },
            Self::Internal(source) => Self::Internal(source.context(context)),
        }
    }

    /// Applies to acquisition failures. A capture failure must leave its source intact.
    /// The caller chooses eligible backends and backoff; cleanup failure forbids failover.
    pub fn acquisition_retry(&self) -> SandboxRetryAdvice {
        match self {
            Self::Failure {
                failure:
                    SandboxFailure::VmCapacityExhausted { .. }
                    | SandboxFailure::SnapshotBudgetExhausted { .. }
                    | SandboxFailure::DiskSpaceExhausted { .. },
                ..
            } => SandboxRetryAdvice::DifferentBackendAfterCleanup,
            Self::Failure {
                failure: SandboxFailure::SnapshotUnavailable { .. },
                ..
            } => SandboxRetryAdvice::RecoverSnapshot,
            Self::Failure {
                failure: SandboxFailure::InvalidRequest { .. },
                ..
            } => SandboxRetryAdvice::Never,
            Self::Internal(_) => SandboxRetryAdvice::Unknown,
        }
    }
}

impl From<SandboxFailure> for SandboxError {
    fn from(failure: SandboxFailure) -> Self {
        Self::Failure {
            source: anyhow::Error::new(failure.clone()),
            failure,
        }
    }
}

#[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
impl From<crate::SnapshotBudgetExhausted> for SandboxError {
    fn from(error: crate::SnapshotBudgetExhausted) -> Self {
        Self::Failure {
            failure: SandboxFailure::SnapshotBudgetExhausted {
                limit_bytes: error.limit_bytes,
                accounted_bytes: error.accounted_bytes,
            },
            source: error.into(),
        }
    }
}

impl From<std::io::Error> for SandboxError {
    fn from(error: std::io::Error) -> Self {
        Self::Internal(error.into())
    }
}

impl From<serde_json::Error> for SandboxError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.into())
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
        let error = SandboxError::from(failure.clone()).context("acquiring VM");
        assert_eq!(error.failure(), Some(&failure));
        assert!(format!("{error:#}").contains("acquiring VM"));
        assert_eq!(
            error.acquisition_retry(),
            SandboxRetryAdvice::DifferentBackendAfterCleanup
        );
        let round_trip = error.context("backend operation");
        assert_eq!(round_trip.failure(), Some(&failure));
        let diagnostic = format!("{round_trip:#}");
        assert!(diagnostic.contains("acquiring VM"));
        assert!(diagnostic.contains("backend operation"));
    }

    #[test]
    #[cfg(all(not(target_arch = "wasm32"), feature = "firecracker"))]
    fn existing_snapshot_budget_errors_retain_accounting_through_context() {
        let error = SandboxError::from(crate::SnapshotBudgetExhausted {
            limit_bytes: 100,
            accounted_bytes: 101,
        })
        .context("source resume also failed: connection closed");
        let failure = SandboxFailure::SnapshotBudgetExhausted {
            limit_bytes: 100,
            accounted_bytes: 101,
        };
        assert_eq!(error.failure(), Some(&failure));
        assert!(format!("{error:#}").contains("source resume also failed: connection closed"));
        let error = error.context("deleting failed filesystem capture: permission denied");
        assert_eq!(error.failure(), Some(&failure));
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("source resume also failed: connection closed"));
        assert!(diagnostic.contains("deleting failed filesystem capture: permission denied"));
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
            anyhow::Error::new(SandboxError::from(anyhow::anyhow!("connection closed")))
                .context("acquiring remote sandbox"),
        );
        assert!(format!("{error:#}").contains("acquiring remote sandbox"));
        assert_eq!(error.acquisition_retry(), SandboxRetryAdvice::Unknown);
        assert!(error.failure().is_none());
    }
}
