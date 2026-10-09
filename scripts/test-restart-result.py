"""Regression checks for the live restart drill's artifact gate."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("restart_result", Path(__file__).with_name("check-restart-result.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)

class RestartResultTests(unittest.TestCase):
    def test_nested_metrics_with_requests_and_no_failures_pass(self):
        gate.validate({"metrics": {"http_reqs_total": 6001, "http_req_failed_rate": 0, "dropped_iterations_total": 0}})

    def test_any_failure_including_fractional_count_fails(self):
        for rate in (1, 0.1, 1e-10):
            with self.subTest(rate=rate), self.assertRaises(ValueError):
                gate.validate({"metrics": {"http_reqs_total": 6001, "http_req_failed_rate": rate, "dropped_iterations_total": 0}})

    def test_missing_empty_or_malformed_metrics_fail_closed(self):
        for data in ({}, {"http_reqs_total": 6001, "http_req_failed_rate": 0}, {"metrics": {}}, {"metrics": {"http_reqs_total": 0, "http_req_failed_rate": 0, "dropped_iterations_total": 0}}, {"metrics": {"http_reqs_total": 5, "http_req_failed_rate": float("nan"), "dropped_iterations_total": 0}}):
            with self.subTest(data=data), self.assertRaises(ValueError):
                gate.validate(data)

    def test_dropped_iterations_fail(self):
        with self.assertRaises(ValueError):
            gate.validate({"metrics": {"http_reqs_total": 6001, "http_req_failed_rate": 0, "dropped_iterations_total": 1}})

if __name__ == "__main__":
    unittest.main()
