#!/usr/bin/env python3
"""Translate Otto hook protocol v1 events to TokenEconomizer's `te-hook --host pi`.

Reads one Otto hook payload on stdin, sends the Claude-shaped event that the Pi
extension (plugin/pi/tokeneconomizer.ts) would send to
`$TE_PLUGIN_ROOT/scripts/te-hook --host pi`, and maps the reply back:
`hookSpecificOutput.additionalContext` becomes `context` (session_start,
before_prompt) or is appended to `content` (after_tool), and `systemMessage`
becomes `notice`. The bridge never fails a hook: every error produces empty
stdout, one stderr line and exit status 0.
"""
import json
import os
import re
import subprocess
import sys

# Subprocess timeout in seconds: the hook timeout in plugin.json minus 2 s.
TIMEOUT_SECONDS = {"session_start": 28, "before_prompt": 13, "after_tool": 13,
                   "turn_end": 13, "session_end": 28}
EVENT_NAMES = {"session_start": "SessionStart", "before_prompt": "UserPromptSubmit",
               "after_tool": "PostToolUse", "turn_end": "Stop", "session_end": "SessionEnd"}
CONTEXT_EVENTS = ("session_start", "before_prompt")
EXIT_CODE_LINE = re.compile(r"^exit_code: (-?\d+)$", re.MULTILINE)


def log(message):
    print(f"tokeneconomizer bridge: {message}", file=sys.stderr)


def build_event(request):
    """The Claude-shaped payload for an Otto request, or None when nothing is sent."""
    event = request["event"]
    session = request.get("session") or {}
    data = request.get("data") or {}
    session_id = session.get("session_id") or ""
    payload = {"hook_event_name": EVENT_NAMES[event], "session_id": session_id,
               "transcript_path": session.get("history_path") or "",
               "cwd": request.get("workspace") or ""}
    if session.get("parent_session_id") is not None:
        payload["agent_id"] = session_id
    if event == "session_start":
        payload["source"] = "startup" if session.get("mode") == "new" else "resume"
    elif event == "before_prompt":
        payload["prompt_id"] = data.get("turn_id") or ""
        payload["prompt"] = data.get("prompt") or ""
    elif event == "after_tool":
        content = data.get("content")
        content = content if isinstance(content, str) else ""
        response = {"content": content, "isError": bool(data.get("is_error"))}
        match = EXIT_CODE_LINE.search(content)
        if match:
            response["exit_code"] = int(match.group(1))
        arguments = data.get("arguments")
        payload.update(prompt_id=data.get("turn_id") or "", tool_name=data.get("tool") or "",
                       tool_use_id=data.get("tool_call_id") or "",
                       tool_input=arguments if isinstance(arguments, dict) else {},
                       tool_response=response)
    elif event == "turn_end":
        if data.get("status") != "success":
            return None
        payload.update(prompt_id=data.get("turn_id") or "", stop_hook_active=False)
    else:
        # Otto's session_end carries no reason; the Pi extension sends its shutdown reason.
        payload["reason"] = "quit"
    return payload


def te_environment(request):
    env = dict(os.environ)
    env["PYTHONUTF8"] = "1"
    if request.get("headless") is True and "TE_PI_MODE" not in env:
        env["TE_PI_MODE"] = "print"
    if "TOKENECONOMIZER_HOME" not in env:
        env["TOKENECONOMIZER_HOME"] = os.path.join(request["workspace"], ".otto", "tokeneconomizer")
    return env


def parse_reply(stdout):
    """The stdout as one JSON object, else its last line that is one (earlier lines are progress output)."""
    try:
        whole = json.loads(stdout)
    except ValueError:
        whole = None
    if isinstance(whole, dict):
        return whole
    for line in reversed(stdout.splitlines()):
        line = line.strip()
        if not line:
            continue
        try:
            value = json.loads(line)
        except ValueError:
            continue
        if isinstance(value, dict):
            return value
    return None


def translate(event, request, reply):
    """The Otto response for a te-hook reply."""
    out = {}
    specific = reply.get("hookSpecificOutput")
    text = specific.get("additionalContext") if isinstance(specific, dict) else None
    if isinstance(text, str) and text:
        if event in CONTEXT_EVENTS:
            out["context"] = text
        elif event == "after_tool":
            original = request["data"].get("content")
            out["content"] = (original if isinstance(original, str) else "") + "\n\n" + text
        else:
            log(f"dropped additionalContext on {event}; Otto accepts no context there")
    notice = reply.get("systemMessage")
    if isinstance(notice, str) and notice:
        out["notice"] = notice
    return out


def run_bridge(request):
    """The Otto response dict, or None for no output. Raises RuntimeError on failure."""
    if not isinstance(request, dict) or request.get("version") != 1:
        raise RuntimeError("unsupported protocol version")
    event = request.get("event")
    if event not in EVENT_NAMES:
        raise RuntimeError(f"unsupported event {event!r}")
    payload = build_event(request)
    if payload is None:
        return None
    plugin_root = os.environ.get("TE_PLUGIN_ROOT")
    if not plugin_root:
        raise RuntimeError("TE_PLUGIN_ROOT is not set; bind it in [plugins.bindings.tokeneconomizer]")
    te_hook = os.path.join(plugin_root, "scripts", "te-hook")
    if not os.path.isfile(te_hook):
        raise RuntimeError(f"{te_hook} does not exist")
    try:
        result = subprocess.run([te_hook, "--host", "pi"], input=json.dumps(payload),
                                capture_output=True, text=True, cwd=request["workspace"],
                                env=te_environment(request), timeout=TIMEOUT_SECONDS[event])
    except subprocess.TimeoutExpired:
        raise RuntimeError(f"te-hook exceeded {TIMEOUT_SECONDS[event]} s")
    except OSError as error:
        raise RuntimeError(f"te-hook did not start: {error}")
    if result.returncode != 0:
        raise RuntimeError(f"te-hook exited with status {result.returncode}")
    reply = parse_reply(result.stdout)
    if reply is None:
        raise RuntimeError("te-hook printed no JSON object")
    return translate(event, request, reply)


def main():
    try:
        response = run_bridge(json.load(sys.stdin))
    except (RuntimeError, ValueError, KeyError, TypeError) as error:
        log(f"{type(error).__name__}: {error}" if not isinstance(error, RuntimeError) else str(error))
        return 0
    if response:
        print(json.dumps(response, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    sys.exit(main())
