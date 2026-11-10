use clap::{Args, FromArgMatches};

use crate::{DEFAULT_SNAPSHOT_BUDGET_BYTES, FirecrackerArgs};

#[test]
fn snapshot_budget_flag_reaches_backend_config() {
    for (args, expected) in [
        (vec!["exo"], DEFAULT_SNAPSHOT_BUDGET_BYTES),
        (
            vec!["exo", "--firecracker-snapshot-budget-bytes", "137438953472"],
            128 * 1024 * 1024 * 1024,
        ),
    ] {
        let matches = FirecrackerArgs::augment_args(clap::Command::new("exo"))
            .try_get_matches_from(args)
            .unwrap();
        let config = FirecrackerArgs::from_arg_matches(&matches)
            .unwrap()
            .backend_spec()
            .unwrap()
            .config;
        assert_eq!(config.snapshot_budget_bytes, expected);
    }
}
