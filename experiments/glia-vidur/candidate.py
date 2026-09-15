"""Starting point for a discovery run: least-loaded queue (LLQ)."""
from vidur.scheduler.global_scheduler.base_global_scheduler import BaseGlobalScheduler


class CustomGlobalScheduler(BaseGlobalScheduler):
    def schedule(self):
        self.sort_requests()
        loads = {rid: replica.num_pending_requests + replica.num_active_requests
                 for rid, replica in self._replica_schedulers.items()}
        placements = []
        while self._request_queue:
            rid = min(loads, key=loads.get)
            placements.append((rid, self._request_queue.pop(0)))
            loads[rid] += 1
        return placements
