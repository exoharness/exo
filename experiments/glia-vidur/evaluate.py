"""Agent-facing evaluation entry point. Run from /workspace in the Vidur image."""
from pathlib import Path
import os
import signal
import subprocess
import sys

workspace = Path('/workspace')
trials = workspace / 'trials'
trials.mkdir(exist_ok=True)
for trial in range(1, 16):
    attempt = trials / f'{trial:02}'
    try:
        attempt.mkdir()
    except FileExistsError:
        continue
    break
else:
    sys.exit('The 15-simulation discovery budget is exhausted. Report the best measured candidate.')

print(f'Evaluation {trial}/15; logs: {attempt}/console.log', flush=True)
with (attempt / 'console.log').open('w') as log:
    with subprocess.Popen([
        sys.executable, '/opt/experiment/benchmark.py', '--policy', 'candidate',
        '--candidate', str(workspace / 'candidate.py'),
        '--output', str(attempt / 'result'), '--cache', str(workspace / 'cache'),
    ], stdout=log, stderr=subprocess.STDOUT, start_new_session=True) as process:
        try:
            returncode = process.wait(timeout=3600)
        except subprocess.TimeoutExpired:
            # Fitting predictors starts worker processes; terminate the whole trial.
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            sys.exit(f'Evaluation timed out; inspect {attempt}/console.log')
if returncode:
    print((attempt / 'console.log').read_text()[-6000:])
    sys.exit(returncode)
print((attempt / 'result/summary.json').read_text())
