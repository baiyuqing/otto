"""Read-only logical export for `otto storage migrate`; no SQLite in Otto's runtime."""
import json
import math
import pathlib
import sqlite3
import sys


def emit(value):
    print(json.dumps(value, ensure_ascii=True, allow_nan=False))


def cell(value):
    if value is None:
        return {"type": "null"}
    if isinstance(value, bytes):
        return {"type": "blob", "value": list(value)}
    if isinstance(value, int):
        return {"type": "integer", "value": value}
    if isinstance(value, float):
        if not math.isfinite(value):
            raise ValueError("non-finite legacy number")
        return {"type": "real", "value": value}
    return {"type": "text", "value": value}


def main():
    source = pathlib.Path(sys.argv[1]).resolve(strict=True)
    connection = sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)
    connection.execute("BEGIN")
    objects = connection.execute(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema "
        "WHERE sql IS NOT NULL ORDER BY (type <> 'table'), rowid"
    ).fetchall()
    tables = [name for kind, name, _, _ in objects
              if kind == "table" and not name.startswith("sqlite_")
              and not name.startswith("memory_records_fts_")]
    known = {
        "memory": {"memory_meta", "memory_records", "memory_candidates", "memory_observations", "memory_records_fts"},
        "usage": {"usage_events"},
        "tasks": {"tasks"},
        "workflow": {"workflow_runs", "workflow_steps", "workflow_attempts", "workflow_requests", "workflow_events"},
        "reflection": {"runs", "watermarks", "generated_skills", "skill_versions"},
        "skill-checks": {"skill_checks"},
    }
    family = next((kind for kind, names in known.items() if set(tables) == names), None)
    if family is None and set(tables) == {"runs", "watermarks"}:
        family = "reflection"
    if family is None:
        raise ValueError("unrecognized legacy database")
    if any(kind not in ("table", "index") for kind, _, _, _ in objects):
        raise ValueError("unexpected legacy schema object")
    counts = {name: connection.execute(f'SELECT count(*) FROM "{name}"').fetchone()[0]
              for name in tables}
    schema = [sql for kind, _, table, sql in objects
              if table in tables and table != "memory_records_fts"]
    # Memory has a new canonical manifest; the importer supplies its schema.
    emit({"type": "header", "family": family,
          "version": connection.execute("PRAGMA user_version").fetchone()[0],
          "schema": schema, "counts": counts})
    # Candidate foreign keys need observations first.
    tables.sort(key=lambda name: (name == "memory_candidates", name))
    for name in tables:
        for row in connection.execute(f'SELECT * FROM "{name}"'):
            emit({"type": "row", "table": name, "values": [cell(value) for value in row]})
    connection.close()


if __name__ == "__main__":
    try:
        main()
    except Exception:
        # Do not echo paths, SQL, or stored content on failure.
        print("legacy database export failed", file=sys.stderr)
        sys.exit(1)
