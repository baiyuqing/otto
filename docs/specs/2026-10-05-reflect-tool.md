# Model-requested reflection and retrospectives

Status: proposed, 2026-10-05.

## Problem

When a user asks Otto to "do a retrospective" or "distil what we learned",
the model has no way to reach the reflection pipeline. It improvises: it
writes a Markdown file with `bash` or `write`, which can only land inside the
workspace, bypasses every reflection check, and fails in confusing ways under
the sandbox. The real reflection output (run logs under
`~/.otto/reflection/runs/`, memory candidates, skill approvals) is a second,
disconnected mechanism the model does not know exists. One request, two
products.

## Goal

A user request for a retrospective goes through the existing reflection use
case, and its result lives in one place under `~/.otto`. The model never
writes retrospective files itself.

## Design

### 1. `reflect` tool

A model tool, `reflect`, with one optional argument `focus` (the same free
text as `/reflect [focus]`).

- Its description says to call it only when the user asks to reflect,
  review, or distil lessons from the session.
- It runs `Reflector::run` with a new trigger `requested` (recorded in
  `reflection.db`). Every existing check applies unchanged: evidence quotes,
  source isolation, redaction, memory candidates queued for `/memory review`,
  skills published as approval requests.
- It runs beside the current turn the way background reflection already does:
  it does not take the controller's admission and reads only complete
  session records, so the slice ends at the user's request. The turn's cancel
  token cancels it.
- It shares the background single-flight flag. If a run is already active,
  the tool returns at once saying so; that run's result arrives as the usual
  notice. The automatic ten-minute cooldown does not apply, as for `/reflect`.
- It returns the report line and the run log path, plus the retrospective
  text (below), so the model can summarize the result in its reply.
- It is registered only when reflection is enabled and the session is
  file-backed; otherwise the model does not see it.
- It writes nothing that a person has not already approved, so it needs no
  approval of its own.

Wiring: the tool depends on a narrow port defined in `tool`. The
`app::Controller` implements it from the same parts `start_auto_reflection`
already uses. The port is injected at the composition root, which keeps the
one-way dependency that `runtime_boundary.rs` checks.

### 2. Retrospective in every run

The reflection answer gains one optional field, `retrospective`: a short
narrative of what happened, what took detours, and what to do differently.
It is limited to 8 KiB, passed through the same redaction, and never cites
withheld external entries. It is written as the first section of the
existing run log, `~/.otto/reflection/runs/<time>-<id>.md` (mode 0600). It
is never loaded into a model context automatically, so it is not rule-scanned
like a skill. This applies to every run (manual, requested, automatic), so
every run log is readable on its own. When `memories = false` and
`skills = false`, the run makes no model call and therefore writes no
retrospective, as before.

### 3. Finding past retrospectives

`/reflect list` shows the newest run logs (time, trigger, one-line result).
`/reflect show <id>` prints one. They are available wherever `/reflect`
already is: REPL, TUI, Web UI, ACP and the chat connector. HTTP:
`GET /v1/reflection/runs` and `GET /v1/reflection/runs/{id}`.

### 4. System prompt

Two lines are added. Retrospective requests use `reflect`. Files are created
or replaced with `write`, not shell here-documents.

## Not in scope

- Letting file tools write elsewhere under `~/.otto`.
- Changing what reflection may write to memory or skills.
- Retrospectives across several sessions.

## Tests

- The tool runs the pipeline with trigger `requested`. It reports "already
  running" while another run holds the flag. It is absent when reflection is
  disabled or the session has no file. Cancelling the turn cancels it.
- The retrospective field is parsed with its bound, redacted, and written
  first in the run log. Answers without the field stay valid.
- `/reflect list|show` in each frontend, and the HTTP endpoints.
- The boundary check still passes: `tool` does not depend on `app`.
