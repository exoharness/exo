Design a global request-routing scheduler for the Vidur LLM-serving simulator.
Minimize mean end-to-end request response time on the fixed benchmark.

Environment:

- The simulator and source are at /opt/vidur; timing profiles are provided.
- Four A10 replicas, Meta-Llama-3-8B, Sarathi chunked prefill (8192 tokens).
- Published ShareGPT trace at 7.5 nominal QPS, about 1000 seconds of arrivals.
- Editable solution: /workspace/candidate.py, defining CustomGlobalScheduler
  as a subclass of BaseGlobalScheduler. Initially it implements LLQ.
- Run `python /opt/experiment/evaluate.py` from /workspace to evaluate.
- Each trial preserves candidate.py, config, per-request metrics, global
  scheduler logs, and summary.json under /workspace/trials/NN/result/.
- At most 15 evaluations, including your initial LLQ baseline and failed runs.
  The first evaluation also fits and caches the simulator's timing models.

Constraints:
Only modify candidate.py and write separate analysis scripts. Do not alter
Vidur, its trace, timing profiles, benchmark/evaluation scripts, metrics, or
trial history. Never consult a request's true decode length before it has
completed, or index the trace to obtain that information at routing time.
Do not change request/replica state except removing dispatched requests from
your own global queue. Route every request eventually; dropped or unfinished
requests invalidate a result. Use the evaluator for every simulation; do not
bypass the evaluation budget.

Measure LLQ first, then develop and test improvements. Analyze causes using
metrics and instrumentation rather than tuning only a scalar objective.
Before finishing, restore the best evaluated code to /workspace/candidate.py
and report its trial number, mean response time, improvement relative to LLQ,
reproduction command, and limitations. Report measured results honestly even
if you do not beat LLQ. External evaluation will rerun your final candidate.
