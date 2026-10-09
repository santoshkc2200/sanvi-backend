#!/usr/bin/env python3
"""Fail closed unless the restart load artifact records zero lost requests."""
import json
import math
import sys

def validate(data):
    metrics = data.get("metrics")
    if not isinstance(metrics, dict):
        raise ValueError("artifact has no metrics object")
    values = {}
    for name in ("http_reqs_total", "http_req_failed_rate", "dropped_iterations_total"):
        value = metrics.get(name)
        if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
            raise ValueError(f"invalid or missing {name}")
        values[name] = value
    if values["http_reqs_total"] <= 0:
        raise ValueError("no requests were measured")
    if values["http_req_failed_rate"] != 0:
        raise ValueError(f"nonzero request failure rate: {values['http_req_failed_rate']}")
    if values["dropped_iterations_total"] != 0:
        raise ValueError("load generator dropped iterations")

if __name__ == "__main__":
    try:
        with open(sys.argv[1], encoding="utf-8") as artifact:
            validate(json.load(artifact))
    except (ValueError, OSError, IndexError, TypeError, AttributeError) as error:
        sys.exit(f"rolling-restart: FAILED — {error}")
    print("rolling-restart: zero failed requests and zero dropped iterations")
