"""One JSON request on stdin, one JSON response on stdout."""
import json
import os
import sys


def handle(request):
    if not isinstance(request, dict) or request.get("version") != 1:
        raise ValueError("unsupported protocol version")
    event = request.get("event")
    if event == "before_tool":
        if request["data"]["arguments"]["label"] == "blocked":
            return {"decision": "deny", "reason": "Demo hook rejected the label 'blocked'."}
        return {"decision": "allow"}
    if event == "tool":
        label = request["arguments"]["label"]
        if not isinstance(label, str) or not 1 <= len(label) <= 64:
            raise ValueError("label must contain 1 to 64 characters")
        entries = sorted(os.listdir("."))
        return {"content": json.dumps({
            "label": label,
            "workspace": os.getcwd(),
            "entries": entries[:20],
            "total_entries": len(entries),
        }, ensure_ascii=False), "is_error": False}
    raise ValueError("unsupported event")


if __name__ == "__main__":
    try:
        print(json.dumps(handle(json.load(sys.stdin)), ensure_ascii=False))
    except (ValueError, KeyError, TypeError):
        print("Invalid demo request.", file=sys.stderr)
        sys.exit(1)
