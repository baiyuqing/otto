"""Offline check: python3 examples/plugins/test_tokeneconomizer_bridge.py."""
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

BRIDGE = Path(__file__).resolve().parent / "tokeneconomizer" / "bridge.py"
FAKE_TE_HOOK = """#!/bin/sh
out="$FAKE_RECORD_DIR"
printf '%s\\n' "$@" > "$out/argv"
{ echo "PYTHONUTF8=$PYTHONUTF8"; echo "TE_PI_MODE=${TE_PI_MODE-unset}"; echo "TOKENECONOMIZER_HOME=$TOKENECONOMIZER_HOME"; pwd; } > "$out/env"
cat > "$out/stdin"
[ -f "$out/exit" ] && exit "$(cat "$out/exit")"
cat "$out/reply"
"""


class Fixture:
    def __init__(self, root):
        self.root = Path(root)
        self.plugin_root = self.root / "te"
        script = self.plugin_root / "scripts" / "te-hook"
        script.parent.mkdir(parents=True)
        script.write_text(FAKE_TE_HOOK, encoding="utf-8")
        script.chmod(0o755)
        self.record = self.root / "record"
        self.workspace = self.root / "ws"
        self.workspace.mkdir()

    def run(self, event, data, reply="{}", headless=False, parent=None, mode="new",
            extra_env=None, plugin_root=True, exit_code=None):
        for name in ("argv", "env", "stdin", "exit", "reply"):
            (self.record / name).unlink(missing_ok=True)
        self.record.mkdir(exist_ok=True)
        (self.record / "reply").write_text(reply, encoding="utf-8")
        if exit_code is not None:
            (self.record / "exit").write_text(str(exit_code), encoding="utf-8")
        env = {k: v for k, v in os.environ.items()
               if k not in ("TE_PLUGIN_ROOT", "TE_PI_MODE", "TOKENECONOMIZER_HOME")}
        env["FAKE_RECORD_DIR"] = str(self.record)
        if plugin_root:
            env["TE_PLUGIN_ROOT"] = str(self.plugin_root)
        env.update(extra_env or {})
        request = {"version": 1, "event": event, "workspace": str(self.workspace),
                   "headless": headless,
                   "session": {"session_id": "s1", "parent_session_id": parent, "task_id": "",
                               "mode": mode, "history_path": "/h/s1.jsonl"},
                   "data": data}
        return subprocess.run([sys.executable, str(BRIDGE)], input=json.dumps(request),
                              text=True, capture_output=True, cwd=self.root, env=env, check=False)

    def sent(self):
        return json.loads((self.record / "stdin").read_text(encoding="utf-8"))

    def argv(self):
        return (self.record / "argv").read_text(encoding="utf-8").split()

    def env(self):
        lines = (self.record / "env").read_text(encoding="utf-8").splitlines()
        values = dict(line.split("=", 1) for line in lines[:3])
        values["cwd"] = lines[3]
        return values

    def ran(self):
        return (self.record / "argv").exists()


def ok(result):
    assert result.returncode == 0, result.stderr
    return result


def test_event_mapping(fx):
    ws = str(fx.workspace.resolve())
    result = ok(fx.run("session_start", {"turn_id": ""}))
    assert result.stdout == ""
    assert fx.argv()[-2:] == ["--host", "pi"]
    sent = fx.sent()
    assert sent["hook_event_name"] == "SessionStart" and sent["source"] == "startup"
    assert sent["session_id"] == "s1" and sent["transcript_path"] == "/h/s1.jsonl"
    assert sent["cwd"] == str(fx.workspace) and "agent_id" not in sent
    assert os.path.realpath(fx.env()["cwd"]) == ws
    ok(fx.run("session_start", {"turn_id": ""}, mode="resume"))
    assert fx.sent()["source"] == "resume"

    ok(fx.run("before_prompt", {"turn_id": "t1", "prompt": "hi", "followup": False, "has_image": False}))
    sent = fx.sent()
    assert sent["hook_event_name"] == "UserPromptSubmit"
    assert sent["prompt_id"] == "t1" and sent["prompt"] == "hi"

    tool = {"turn_id": "t1", "tool": "bash", "tool_call_id": "c1", "arguments": {"command": "false"},
            "content": "exit_code: 3\nfoo", "is_error": True}
    ok(fx.run("after_tool", tool))
    sent = fx.sent()
    assert sent["hook_event_name"] == "PostToolUse" and sent["prompt_id"] == "t1"
    assert sent["tool_name"] == "bash" and sent["tool_use_id"] == "c1"
    assert sent["tool_input"] == {"command": "false"}
    assert sent["tool_response"] == {"content": "exit_code: 3\nfoo", "isError": True, "exit_code": 3}
    ok(fx.run("after_tool", dict(tool, content="no exit line", is_error=False)))
    assert fx.sent()["tool_response"] == {"content": "no exit line", "isError": False}

    ok(fx.run("turn_end", {"turn_id": "t1", "status": "success"}))
    sent = fx.sent()
    assert sent["hook_event_name"] == "Stop" and sent["prompt_id"] == "t1"
    assert sent["stop_hook_active"] is False
    ok(fx.run("session_end", {"turn_id": ""}))
    assert fx.sent()["hook_event_name"] == "SessionEnd" and "reason" in fx.sent()


def test_agent_id_only_with_parent(fx):
    ok(fx.run("before_prompt", {"turn_id": "t", "prompt": "p"}, parent="root"))
    assert fx.sent()["agent_id"] == "s1"
    ok(fx.run("before_prompt", {"turn_id": "t", "prompt": "p"}))
    assert "agent_id" not in fx.sent()


def test_environment(fx):
    ok(fx.run("session_start", {}))
    env = fx.env()
    assert env["PYTHONUTF8"] == "1" and env["TE_PI_MODE"] == "unset"
    assert env["TOKENECONOMIZER_HOME"] == os.path.join(str(fx.workspace), ".otto", "tokeneconomizer")
    ok(fx.run("session_start", {}, headless=True))
    assert fx.env()["TE_PI_MODE"] == "print"
    ok(fx.run("session_start", {}, headless=True, extra_env={"TE_PI_MODE": "json"}))
    assert fx.env()["TE_PI_MODE"] == "json"
    ok(fx.run("session_start", {}, extra_env={"TOKENECONOMIZER_HOME": "/custom/home"}))
    assert fx.env()["TOKENECONOMIZER_HOME"] == "/custom/home"


def test_reply_translation(fx):
    context = json.dumps({"hookSpecificOutput": {"additionalContext": "facts"},
                          "systemMessage": "added 1 fact"})
    for event in ("session_start", "before_prompt"):
        result = ok(fx.run(event, {"turn_id": "t", "prompt": "p"}, reply=context))
        assert json.loads(result.stdout) == {"context": "facts", "notice": "added 1 fact"}
    result = ok(fx.run("after_tool", {"turn_id": "t", "tool": "bash", "content": "out"}, reply=context))
    assert json.loads(result.stdout) == {"content": "out\n\nfacts", "notice": "added 1 fact"}
    result = ok(fx.run("turn_end", {"turn_id": "t", "status": "success"}, reply=context))
    assert json.loads(result.stdout) == {"notice": "added 1 fact"}
    assert "additionalContext" in result.stderr
    result = ok(fx.run("session_end", {}, reply=context))
    assert json.loads(result.stdout) == {"notice": "added 1 fact"}
    assert ok(fx.run("before_prompt", {"prompt": "p"}, reply="{}\n")).stdout == ""
    progress = '{"type":"progress","message":"x"}\nnot json\n{"systemMessage":"ünï"}\n\n'
    result = ok(fx.run("before_prompt", {"prompt": "p"}, reply=progress))
    assert json.loads(result.stdout) == {"notice": "ünï"}


def test_fail_open(fx):
    for status in ("error", "cancelled"):
        result = ok(fx.run("turn_end", {"turn_id": "t", "status": status}))
        assert result.stdout == "" and not fx.ran()
    result = ok(fx.run("before_prompt", {"prompt": "p"}, plugin_root=False))
    assert result.stdout == "" and result.stderr.strip() and not fx.ran()
    result = ok(fx.run("before_prompt", {"prompt": "p"}, plugin_root=False,
                       extra_env={"TE_PLUGIN_ROOT": str(fx.root / "missing")}))
    assert result.stdout == "" and result.stderr.strip()
    result = ok(fx.run("before_prompt", {"prompt": "p"}, exit_code=1, reply='{"systemMessage":"x"}'))
    assert result.stdout == "" and result.stderr.strip()
    result = ok(fx.run("before_prompt", {"prompt": "p"}, reply="garbage"))
    assert result.stdout == "" and result.stderr.strip()
    for request in ({"version": 2, "event": "session_start"}, {"version": 1, "event": "unknown"}, []):
        result = subprocess.run([sys.executable, str(BRIDGE)], input=json.dumps(request),
                                text=True, capture_output=True, check=False)
        assert result.returncode == 0 and result.stdout == "" and result.stderr.strip()
    result = subprocess.run([sys.executable, str(BRIDGE)], input="not json",
                            text=True, capture_output=True, check=False)
    assert result.returncode == 0 and result.stdout == "" and result.stderr.strip()


with tempfile.TemporaryDirectory() as root:
    fixture = Fixture(root)
    for check in (test_event_mapping, test_agent_id_only_with_parent, test_environment,
                  test_reply_translation, test_fail_open):
        check(fixture)
print("tokeneconomizer-bridge: passed")
