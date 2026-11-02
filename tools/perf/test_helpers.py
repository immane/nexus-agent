"""Shared helpers for isolated subprocess measurements in perf tests."""

import json
import os
import subprocess
import sys


HERE = os.path.dirname(os.path.abspath(__file__))


def isolated_measurement(operation, argv, **kwargs):
    """Run a measurement from a small process to avoid test-runner RSS."""
    worker = (
        "import json, sys, startup\n"
        "request = json.load(sys.stdin)\n"
        "result = getattr(startup, request['operation'])(request['argv'], **request['kwargs'])\n"
        "json.dump(result, sys.stdout)\n"
    )
    proc = subprocess.run(
        [sys.executable, "-c", worker], cwd=HERE,
        input=json.dumps({"operation": operation, "argv": argv, "kwargs": kwargs}),
        capture_output=True, text=True,
        timeout=kwargs.get("timeout", 60) * (kwargs.get("n", 1) * 2 + 1) + 30,
        check=True,
    )
    return json.loads(proc.stdout)
