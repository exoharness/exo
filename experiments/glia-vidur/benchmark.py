"""Pinned Vidur request-routing experiment; runs inside the experiment image."""
import argparse
import atexit
import csv
import hashlib
import importlib.metadata
import importlib.util
import json
import math
import os
from pathlib import Path
import platform
import shutil
import statistics
import sys
import time

VIDUR = Path('/opt/vidur')
TRACE = VIDUR / 'data/processed_traces/sharegpt_7.5.csv'
TRACE_SHA256 = '135700d7ce3c6efd7e2b2de171d2e393683553b390f8d2553cd0a44a6dba6820'
REVISION = '2666cb64a622d9b8532791c6fdbe852cf3c1ae7f'


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def summarize(metrics_path, expected_requests):
    with Path(metrics_path).open() as source:
        rows = list(csv.DictReader(source))
    if len(rows) != expected_requests:
        raise ValueError(f'Incomplete evaluation: {len(rows)}/{expected_requests} requests')
    latency = [float(row['request_e2e_time']) for row in rows]
    if not latency or any(not math.isfinite(value) or value <= 0 for value in latency):
        raise ValueError('Invalid request latency')
    return {
        'completed_requests': len(rows),
        'mean_request_e2e_seconds': statistics.mean(latency),
        'p90_request_e2e_seconds': sorted(latency)[math.ceil(0.9 * len(latency)) - 1],
        'mean_scheduling_delay_seconds': statistics.mean(float(row['request_scheduling_delay']) for row in rows),
        'mean_execution_seconds': statistics.mean(float(row['request_execution_time']) for row in rows),
        'total_restarts': sum(int(float(row['request_num_restarts'])) for row in rows),
        'fraction_requests_restarted': statistics.mean(float(row['request_num_restarts']) > 0 for row in rows),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--policy', choices=['llq', 'lor', 'round_robin', 'candidate'], default='llq')
    parser.add_argument('--candidate', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cache', type=Path, default=Path('/workspace/cache'))
    parser.add_argument('--seed', type=int, default=42)
    parser.add_argument('--smoke', action='store_true', help='64 requests and a tiny predictor grid; NOT a paper result')
    args = parser.parse_args()
    if (args.policy == 'candidate') != (args.candidate is not None):
        parser.error('--policy candidate requires --candidate, and vice versa')
    if (VIDUR / 'REVISION').read_text().strip() != REVISION or sha256(TRACE) != TRACE_SHA256:
        raise ValueError('Unexpected simulator revision or trace checksum')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    cache = args.cache.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    with TRACE.open() as source:
        trace_rows = list(csv.DictReader(source))
    trace = TRACE
    if args.smoke:
        trace_rows = trace_rows[:64]
        trace = output / 'smoke-trace.csv'
        with trace.open('w') as target:
            writer = csv.DictWriter(target, fieldnames=list(trace_rows[0]))
            writer.writeheader()
            writer.writerows(trace_rows)

    from vidur.config import SimulationConfig
    from vidur.simulator import Simulator
    from vidur.utils.random import set_seeds

    candidate_hash = None
    if args.candidate:
        # Snapshot exactly the code evaluated, before importing it.
        candidate = output / 'candidate.py'
        shutil.copyfile(args.candidate.resolve(), candidate)
        candidate_hash = sha256(candidate)
        spec = importlib.util.spec_from_file_location('candidate', candidate)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        from vidur.scheduler.global_scheduler.base_global_scheduler import BaseGlobalScheduler
        from vidur.scheduler.global_scheduler.global_scheduler_registry import GlobalSchedulerRegistry
        from vidur.types.global_scheduler_type import GlobalSchedulerType
        if not issubclass(module.CustomGlobalScheduler, BaseGlobalScheduler):
            raise TypeError('candidate.py must define CustomGlobalScheduler(BaseGlobalScheduler)')
        GlobalSchedulerRegistry.register(GlobalSchedulerType.CUSTOM, module.CustomGlobalScheduler)

    # Match the archived authors' env_evaluator.py (Sarathi branch).
    flags = {
        'seed': args.seed,
        'replica_config_device': 'a10',
        'replica_config_memory_margin_fraction': 0.2445,
        'replica_config_model_name': 'meta-llama/Meta-Llama-3-8B',
        'cluster_config_num_replicas': 4,
        'global_scheduler_config_type': 'custom' if args.candidate else args.policy,
        'replica_config_tensor_parallel_size': 1,
        'replica_config_num_pipeline_stages': 1,
        'replica_scheduler_config_type': 'sarathi',
        'sarathi_scheduler_config_num_blocks': 2240,
        'sarathi_scheduler_config_batch_size_cap': 128,
        'sarathi_scheduler_config_chunk_size': 8192,
        'sarathi_scheduler_config_block_size': 16,
        'sarathi_scheduler_config_watermark_blocks_fraction': 0.01,
        'random_forrest_execution_time_predictor_config_prediction_max_prefill_chunk_size': 8192,
        'random_forrest_execution_time_predictor_config_prediction_max_batch_size': 128,
        'random_forrest_execution_time_predictor_config_prediction_max_tokens_per_request': 8192,
        'random_forrest_execution_time_predictor_config_num_training_job_threads': 2,
        'request_generator_config_type': 'trace_replay',
        'trace_request_generator_config_trace_file': trace,
        'trace_request_generator_config_max_tokens': 8192,
        'length_generator_config_type': 'trace',
        'trace_request_length_generator_config_trace_file': trace,
        'trace_request_length_generator_config_max_tokens': 8192,
        'interval_generator_config_type': 'trace',
        'trace_request_interval_generator_config_trace_file': trace,
        'metrics_config_output_dir': output / 'simulator',
        'metrics_config_cache_dir': cache,
        'ledger_dir': output / 'ledger',
    }
    if args.smoke:
        flags.update({
            'random_forrest_execution_time_predictor_config_num_estimators': 10,
            'random_forrest_execution_time_predictor_config_max_depth': 8,
            'random_forrest_execution_time_predictor_config_min_samples_split': 2,
            'random_forrest_execution_time_predictor_config_k_fold_cv_splits': 2,
        })
    sys.argv = ['vidur']
    for key, value in flags.items():
        sys.argv.extend([f'--{key}', str(value)])
    sys.argv += [
        '--metrics_config_write_metrics', '--metrics_config_store_request_metrics',
        '--metrics_config_store_global_scheduler_logs',
        *[f'--no-metrics_config_{name}' for name in [
            'write_json_trace', 'enable_chrome_trace', 'save_table_to_wandb',
            'store_plots', 'store_operation_metrics', 'store_token_completion_metrics',
            'store_batch_metrics', 'store_utilization_metrics', 'keep_individual_batch_metrics',
        ]],
    ]
    manifest = {
        'vidur_revision': REVISION, 'trace_sha256': sha256(trace),
        'source_trace_sha256': TRACE_SHA256, 'candidate_sha256': candidate_hash,
        'mode': 'smoke_not_paper_result' if args.smoke else 'artifact_reproduction',
        'policy': args.policy, 'seed': args.seed, 'expected_requests': len(trace_rows),
        'cpu_architecture': platform.machine(), 'python_version': platform.python_version(),
        'profile_sha256': {str(p.relative_to(VIDUR)): sha256(p) for p in sorted(
            (VIDUR / 'data/profiling/compute/a10/meta-llama/Meta-Llama-3-8B').glob('*.csv'))},
        'last_arrival_seconds': float(trace_rows[-1]['arrived_at']),
        'simulator_argv': sys.argv,
        'packages': {p: importlib.metadata.version(p) for p in ['numpy', 'pandas', 'scikit-learn', 'scipy']},
    }
    (output / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n')
    os.chdir(VIDUR)
    started = time.monotonic()
    config = SimulationConfig.create_from_cli_args()
    set_seeds(config.seed)
    simulator = Simulator(config)
    # Only write metrics after a successful, fully drained simulation.
    atexit.unregister(simulator._write_output)
    simulator.run()
    simulator._write_output()
    metrics_path = Path(config.metrics_config.output_dir) / 'request_metrics.csv'
    result = {**manifest, **summarize(metrics_path, len(trace_rows)),
              'wall_seconds': time.monotonic() - started,
              'metrics_file': str(metrics_path)}
    (output / 'summary.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
