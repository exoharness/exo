"""Figure 10 HRA policy, reimplemented for validation; keep out of discovery runs.

Source: Hamadanian et al., Glia, arXiv:2510.27176v5, pp. 15-16.
Shortest-prompt-first, decode/prefill ratio 0.6, safety fraction 0.03.
"""
from math import ceil
from vidur.scheduler.global_scheduler.base_global_scheduler import BaseGlobalScheduler


class CustomGlobalScheduler(BaseGlobalScheduler):
    def schedule(self):
        self._request_queue.sort(key=lambda request: (request.num_prefill_tokens, request.arrived_at))
        if not self._request_queue:
            return []
        first = next(iter(self._replica_schedulers.values()))
        block_size = first._config.block_size
        capacity = first._config.num_blocks
        safety = int(capacity * 0.03)
        allocated = {rid: replica.num_allocated_blocks for rid, replica in self._replica_schedulers.items()}
        reserved = {
            rid: ceil(sum(request.num_prefill_tokens * 1.6 for request in replica._request_queue) / block_size)
            for rid, replica in self._replica_schedulers.items()
        }
        queued = {rid: replica.num_pending_requests + replica.num_active_requests
                  for rid, replica in self._replica_schedulers.items()}
        placements = []
        while self._request_queue:
            request = self._request_queue[0]
            blocks = ceil(request.num_prefill_tokens * 1.6 / block_size)
            eligible = [rid for rid in allocated if capacity - allocated[rid] - reserved[rid] - blocks >= safety]
            if not eligible:
                break
            rid = min(eligible, key=lambda rid: (allocated[rid] + reserved[rid], queued[rid]))
            placements.append((rid, self._request_queue.pop(0)))
            reserved[rid] += blocks
            queued[rid] += 1
        return placements
