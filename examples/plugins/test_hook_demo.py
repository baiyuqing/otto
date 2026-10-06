"""Offline protocol check: python3 examples/plugins/test_hook_demo.py."""
import json
import subprocess
import sys
import tempfile
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "hook-demo" / "demo.py"


def run(workspace, request):
    return subprocess.run(
        [sys.executable, str(SCRIPT)], input=json.dumps(request),
        text=True, capture_output=True, cwd=workspace, check=False,
    )


with tempfile.TemporaryDirectory() as workspace:
    Path(workspace, "hello.txt").write_text("hello", encoding="utf-8")
    for label, decision in [("demo", "allow"), ("blocked", "deny")]:
        result = run(workspace, {"version": 1, "event": "before_tool",
                                 "data": {"arguments": {"label": label}}})
        assert result.returncode == 0, result.stderr
        assert json.loads(result.stdout)["decision"] == decision
    result = run(workspace, {"version": 1, "event": "tool", "arguments": {"label": "demo"}})
    assert result.returncode == 0, result.stderr
    response = json.loads(result.stdout)
    assert response["is_error"] is False
    content = json.loads(response["content"])
    assert content["label"] == "demo"
    assert content["entries"] == ["hello.txt"]
    assert content["total_entries"] == 1
    for request in [{"version": 2, "event": "tool"},
                    {"version": 1, "event": "unknown"},
                    {"version": 1, "event": "tool", "arguments": {"label": ""}}]:
        result = run(workspace, request)
        assert result.returncode != 0
        assert not result.stdout
print("hook-demo: passed")
