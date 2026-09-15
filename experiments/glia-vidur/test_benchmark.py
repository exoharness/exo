import csv
from pathlib import Path
import tempfile
import unittest

from benchmark import summarize


class MetricsTests(unittest.TestCase):
    def write_metrics(self, root, times):
        path = Path(root) / 'metrics.csv'
        with path.open('w') as target:
            writer = csv.DictWriter(target, fieldnames=[
                'request_e2e_time', 'request_scheduling_delay',
                'request_execution_time', 'request_num_restarts',
            ])
            writer.writeheader()
            for index, time in enumerate(times):
                writer.writerow(dict(request_e2e_time=time, request_scheduling_delay=2,
                                     request_execution_time=3, request_num_restarts=index))
        return path

    def test_mean_and_restart_metrics(self):
        with tempfile.TemporaryDirectory() as root:
            result = summarize(self.write_metrics(root, [10, 30]), 2)
            self.assertEqual(result['mean_request_e2e_seconds'], 20)
            self.assertEqual(result['p90_request_e2e_seconds'], 30)
            self.assertEqual(result['total_restarts'], 1)
            self.assertEqual(result['fraction_requests_restarted'], 0.5)

    def test_dropped_requests_cannot_improve_score(self):
        with tempfile.TemporaryDirectory() as root:
            with self.assertRaisesRegex(ValueError, 'Incomplete evaluation'):
                summarize(self.write_metrics(root, [10]), 2)

    def test_invalid_or_empty_metrics_are_rejected(self):
        for times in [[], ['nan'], ['inf'], [0], [-1]]:
            with self.subTest(times=times), tempfile.TemporaryDirectory() as root:
                with self.assertRaisesRegex(ValueError, 'Invalid request latency'):
                    summarize(self.write_metrics(root, times), len(times))


if __name__ == '__main__':
    unittest.main()
