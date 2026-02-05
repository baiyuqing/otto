#!/usr/bin/env python
import argparse
import dataclasses
import difflib
import hashlib
import json
import os
import queue
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

try:
    from watchdog.events import FileSystemEventHandler
    from watchdog.observers import Observer
except Exception:  # pragma: no cover - optional dependency
    FileSystemEventHandler = None
    Observer = None

try:
    from tree_sitter import Parser
    from tree_sitter_languages import get_language
except Exception:  # pragma: no cover - optional dependency
    Parser = None
    get_language = None

DEFAULT_EXCLUDES = {
    ".git",
    ".venv",
    "venv",
    "node_modules",
    ".pytest_cache",
    "docs/agent-trace.md",
    ".trace_state.json",
}

LANG_BY_EXT = {
    ".py": "python",
    ".pyi": "python",
    ".ts": "typescript",
    ".tsx": "tsx",
    ".js": "javascript",
    ".jsx": "javascript",
    ".go": "go",
}


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace")


def sha256(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8", errors="replace")).hexdigest()


def is_excluded(path: Path, root: Path) -> bool:
    rel = path.relative_to(root).as_posix()
    for part in path.parts:
        if part in DEFAULT_EXCLUDES:
            return True
    for prefix in DEFAULT_EXCLUDES:
        if rel.startswith(prefix):
            return True
    return False


def load_state(path: Path) -> dict:
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return {}


def save_state(path: Path, state: dict) -> None:
    path.write_text(json.dumps(state, indent=2, sort_keys=True), encoding="utf-8")


def get_latest_conversation(log_path: Path | None) -> dict:
    if not log_path or not log_path.exists():
        return {
            "id": None,
            "message_id": None,
            "role": None,
            "created_at": None,
            "excerpt": None,
        }
    # Expect JSONL; last valid line wins.
    latest = None
    with log_path.open("r", encoding="utf-8", errors="replace") as handle:
        for line in handle:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except Exception:
                continue
            latest = obj
    if not isinstance(latest, dict):
        return {
            "id": None,
            "message_id": None,
            "role": None,
            "created_at": None,
            "excerpt": None,
        }
    content = latest.get("content")
    if isinstance(content, list):
        content = " ".join(str(item) for item in content)
    if not isinstance(content, str):
        content = ""
    excerpt = content.strip().replace("\n", " ")[:140] or None
    return {
        "id": latest.get("conversation_id") or latest.get("id"),
        "message_id": latest.get("message_id") or latest.get("id"),
        "role": latest.get("role"),
        "created_at": latest.get("created_at") or latest.get("timestamp"),
        "excerpt": excerpt,
    }


def parse_diff(old: str, new: str) -> dict:
    old_lines = old.splitlines()
    new_lines = new.splitlines()
    diff = list(difflib.unified_diff(old_lines, new_lines, lineterm="", n=0))
    hunks = []
    added = 0
    deleted = 0
    for line in diff:
        if line.startswith("@@"):
            # @@ -a,b +c,d @@
            try:
                header = line.split("@@")[1].strip()
                old_part, new_part = header.split(" ")[:2]
                old_start, old_len = old_part[1:].split(",") if "," in old_part else (old_part[1:], "1")
                new_start, new_len = new_part[1:].split(",") if "," in new_part else (new_part[1:], "1")
                old_start = int(old_start)
                old_len = int(old_len)
                new_start = int(new_start)
                new_len = int(new_len)
                hunks.append(
                    {
                        "old_start": old_start,
                        "old_lines": old_len,
                        "new_start": new_start,
                        "new_lines": new_len,
                    }
                )
            except Exception:
                continue
        elif line.startswith("+") and not line.startswith("+++"):
            added += 1
        elif line.startswith("-") and not line.startswith("---"):
            deleted += 1
    return {"added": added, "deleted": deleted, "hunks": hunks}


def parse_ast(path: Path, new_text: str, hunks: list) -> list:
    if Parser is None or get_language is None:
        return []
    lang_name = LANG_BY_EXT.get(path.suffix)
    if not lang_name:
        return []
    try:
        language = get_language(lang_name)
    except Exception:
        return []
    parser = Parser()
    parser.set_language(language)
    tree = parser.parse(new_text.encode("utf-8", errors="replace"))

    ranges = []
    for h in hunks:
        start = h.get("new_start", 1)
        length = max(h.get("new_lines", 0), 1)
        ranges.append((start, start + length - 1))

    if not ranges:
        return []

    results = []

    def node_intersects(node_start, node_end) -> bool:
        for r_start, r_end in ranges:
            if node_end < r_start or node_start > r_end:
                continue
            return True
        return False

    def get_name(node):
        for child in node.children:
            if child.type in {"identifier", "name", "type_identifier"}:
                return new_text[child.start_byte : child.end_byte]
        return None

    def walk(node):
        start_line = node.start_point[0] + 1
        end_line = node.end_point[0] + 1
        if node_intersects(start_line, end_line):
            name = get_name(node)
            results.append(
                {
                    "type": node.type,
                    "name": name,
                    "start": start_line,
                    "end": end_line,
                }
            )
        for child in node.children:
            walk(child)

    walk(tree.root_node)

    # Compact: keep only top-level-ish nodes (prefer named parents over children)
    compact = []
    seen = set()
    for item in results:
        key = (item["type"], item["name"], item["start"], item["end"])
        if key in seen:
            continue
        seen.add(key)
        compact.append(item)
    return compact[:12]


def summarize_nodes(nodes: list) -> str:
    if not nodes:
        return "no AST matches"
    parts = []
    for node in nodes[:3]:
        label = node["type"]
        if node.get("name"):
            label += f" {node['name']}"
        label += f" ({node['start']}-{node['end']})"
        parts.append(label)
    if len(nodes) > 3:
        parts.append("...")
    return ", ".join(parts)


def append_markdown(log_path: Path, entry: dict) -> None:
    conv = entry.get("conversation", {})
    summary = entry.get("summary", "")
    with log_path.open("a", encoding="utf-8") as handle:
        handle.write("\n")
        handle.write(f"### {entry['timestamp']}\n")
        handle.write(
            "Conversation: "
            + f"id={conv.get('id')} message_id={conv.get('message_id')} role={conv.get('role')}\n"
        )
        if conv.get("excerpt"):
            handle.write(f"Excerpt: {conv.get('excerpt')}\n")
        handle.write(f"File: `{entry['file']}`\n")
        handle.write(f"Summary: {summary}\n")
        handle.write("```json\n")
        handle.write(json.dumps(entry, indent=2, sort_keys=True))
        handle.write("\n```\n")


def build_entry(root: Path, path: Path, old_text: str, new_text: str, conv: dict) -> dict:
    change = parse_diff(old_text, new_text)
    nodes = parse_ast(path, new_text, change.get("hunks", []))
    summary = f"+{change['added']} -{change['deleted']}; {summarize_nodes(nodes)}"
    return {
        "trace_entry": True,
        "timestamp": utc_now(),
        "conversation": conv,
        "file": path.relative_to(root).as_posix(),
        "change": change,
        "ast": nodes,
        "summary": summary,
    }


@dataclasses.dataclass
class PendingEvent:
    path: Path
    last_seen: float


class ChangeHandler(FileSystemEventHandler):
    def __init__(self, root: Path, pending: dict, debounce_ms: int):
        super().__init__()
        self.root = root
        self.pending = pending
        self.debounce = debounce_ms / 1000

    def on_modified(self, event):
        if event.is_directory:
            return
        path = Path(event.src_path)
        if not path.exists() or is_excluded(path, self.root):
            return
        self.pending[path] = PendingEvent(path=path, last_seen=time.time())

    def on_created(self, event):
        self.on_modified(event)


def main() -> int:
    parser = argparse.ArgumentParser(description="Watch files and log agent trace entries.")
    parser.add_argument("--root", default=".", help="Repo root")
    parser.add_argument("--log", default="docs/agent-trace.md", help="Markdown log path")
    parser.add_argument("--conversation-log", default=None, help="JSONL conversation log")
    parser.add_argument("--state", default=".trace_state.json", help="State file path")
    parser.add_argument("--debounce-ms", type=int, default=800, help="Debounce window")
    args = parser.parse_args()

    if Observer is None or FileSystemEventHandler is None:
        print("watchdog is not installed. Run: python -m pip install watchdog", file=sys.stderr)
        return 1

    root = Path(args.root).resolve()
    log_path = (root / args.log).resolve()
    state_path = (root / args.state).resolve()
    conversation_log = Path(args.conversation_log).expanduser().resolve() if args.conversation_log else None

    log_path.parent.mkdir(parents=True, exist_ok=True)

    state = load_state(state_path)
    pending: dict[Path, PendingEvent] = {}

    observer = Observer()
    handler = ChangeHandler(root, pending, args.debounce_ms)
    observer.schedule(handler, str(root), recursive=True)
    observer.start()

    try:
        while True:
            now = time.time()
            ready = [p for p in pending.values() if now - p.last_seen >= args.debounce_ms / 1000]
            for item in ready:
                path = item.path
                pending.pop(path, None)
                if is_excluded(path, root):
                    continue
                rel = path.relative_to(root).as_posix()
                try:
                    new_text = read_text(path)
                except Exception:
                    continue
                old_text = state.get(rel, {}).get("content", "")
                if sha256(new_text) == state.get(rel, {}).get("hash"):
                    continue
                conv = get_latest_conversation(conversation_log)
                entry = build_entry(root, path, old_text, new_text, conv)
                append_markdown(log_path, entry)
                state[rel] = {
                    "hash": sha256(new_text),
                    "content": new_text,
                    "updated_at": utc_now(),
                }
                save_state(state_path, state)
            time.sleep(0.2)
    except KeyboardInterrupt:
        observer.stop()
    observer.join()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
