#!/usr/bin/env python
import argparse
import json
from pathlib import Path


def load_entries(log_path: Path) -> list:
    if not log_path.exists():
        return []
    text = log_path.read_text(encoding="utf-8", errors="replace")
    entries = []
    in_json = False
    buffer = []
    for line in text.splitlines():
        if line.strip() == "```json":
            in_json = True
            buffer = []
            continue
        if line.strip() == "```" and in_json:
            in_json = False
            try:
                obj = json.loads("\n".join(buffer))
                if isinstance(obj, dict) and obj.get("trace_entry"):
                    entries.append(obj)
            except Exception:
                pass
            buffer = []
            continue
        if in_json:
            buffer.append(line)
    return entries


def sanitize(text: str | None) -> str:
    if not text:
        return "unknown"
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def render_svg(entries: list, out_path: Path) -> None:
    if not entries:
        out_path.write_text("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"800\" height=\"120\"></svg>")
        return

    conv_nodes = []
    conv_index = {}
    for entry in entries:
        conv = entry.get("conversation", {})
        key = f"{conv.get('id')}::{conv.get('message_id')}::{conv.get('role')}"
        if key not in conv_index:
            conv_index[key] = len(conv_nodes)
            conv_nodes.append(conv)

    row_height = 52
    left_x = 40
    right_x = 520
    width = 1000
    height = max(180, (max(len(entries), len(conv_nodes)) + 1) * row_height)

    parts = [
        f"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\">",
        "<style>text{font-family:Arial,sans-serif;font-size:12px;}</style>",
        "<rect width=\"100%\" height=\"100%\" fill=\"#fff\" />",
    ]

    # Conversation column
    parts.append(f"<text x=\"{left_x}\" y=\"24\" fill=\"#111\">Conversation</text>")
    for i, conv in enumerate(conv_nodes):
        y = 52 + i * row_height
        label = f"{sanitize(conv.get('role'))} {sanitize(conv.get('message_id'))}"
        parts.append(f"<rect x=\"{left_x}\" y=\"{y}\" width=\"420\" height=\"36\" rx=\"6\" fill=\"#f2f4f8\" stroke=\"#cbd5e1\" />")
        parts.append(f"<text x=\"{left_x + 10}\" y=\"{y + 22}\" fill=\"#111\">{label}</text>")

    # Change column
    parts.append(f"<text x=\"{right_x}\" y=\"24\" fill=\"#111\">Change</text>")
    for i, entry in enumerate(entries):
        y = 52 + i * row_height
        file_label = sanitize(entry.get("file"))
        summary = sanitize(entry.get("summary"))
        parts.append(f"<rect x=\"{right_x}\" y=\"{y}\" width=\"440\" height=\"36\" rx=\"6\" fill=\"#eef6ff\" stroke=\"#93c5fd\" />")
        parts.append(f"<text x=\"{right_x + 10}\" y=\"{y + 16}\" fill=\"#0f172a\">{file_label}</text>")
        parts.append(f"<text x=\"{right_x + 10}\" y=\"{y + 30}\" fill=\"#475569\">{summary}</text>")

    # Links
    for i, entry in enumerate(entries):
        conv = entry.get("conversation", {})
        key = f"{conv.get('id')}::{conv.get('message_id')}::{conv.get('role')}"
        conv_row = conv_index.get(key, 0)
        y1 = 70 + conv_row * row_height
        y2 = 70 + i * row_height
        parts.append(
            f"<line x1=\"{left_x + 420}\" y1=\"{y1}\" x2=\"{right_x}\" y2=\"{y2}\" stroke=\"#94a3b8\" stroke-width=\"1.5\" />"
        )

    parts.append("</svg>")
    out_path.write_text("\n".join(parts), encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description="Render agent trace SVG from markdown log.")
    parser.add_argument("--log", default="docs/agent-trace.md", help="Markdown log path")
    parser.add_argument("--out", default="docs/agent-trace.svg", help="SVG output path")
    args = parser.parse_args()

    log_path = Path(args.log)
    out_path = Path(args.out)
    entries = load_entries(log_path)
    render_svg(entries, out_path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
