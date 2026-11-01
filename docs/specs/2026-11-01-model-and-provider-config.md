# Provider and model configuration

Status: approved, 2026-11-01. Amended 2026-11-01: a model is written as one
`models."<model_id>" = { ... }` line in its provider table (section
[Written form of a model](#written-form-of-a-model)).

## Rule: a provider has an API type; a model belongs to a provider

LLM access has three levels:

1. API type: `claude`, `openai-compatible` or `chatgpt`. It fixes the request
   protocol and authentication. It is a field of a provider, not a table.
2. Provider, `[providers.<name>]`: one endpoint and credential binding of one
   API type.
3. Model, `models."<model_id>" = { ... }` in `[providers.<name>]`: one model
   of that provider. The key is the model ID the provider API receives.

A model is selected by `<provider>/<model_id>`, for example
`chatgpt/gpt-5.6-luna`. Every selector (`default_model`, `--model`,
`OTTO_MODEL`, `/model`, the HTTP and chat-connector commands,
`approval.reviewer_model`) takes this form. Otto has no model aliases and never
maps a family name such as `luna` to a generation: `gpt-5.6-luna` and a later
`luna` generation are different model IDs with different limits. The word
"profile" no longer appears in configuration, CLI flags, commands, HTTP routes
or documentation.

## Current state: one `[profiles.<name>]` table holds connection and model

`otto_core::config::Profile` (`crates/otto-core/src/config/mod.rs:224`) has
connection fields (`provider`, `base_url`, `api_key_env`, `api_version`,
`workspace_id_env`) and model fields (`model`, `thinking`, `subagent_models`,
`context_window`, `compaction_window`). Each additional model on the same
connection repeats the connection fields; the profile name is a third name
next to the provider type and the model ID, and does not say which of them it
stands for.

Example of a current user file with three ChatGPT models: 4 tables, 20 lines,
of which 8 lines (`context_window`, `compaction_window`) repeat values that
`model_limits.rs:138` already holds for `gpt-5.6-*`, and one table (`chatgpt`)
duplicates another (`luna`).

## New format

The same three ChatGPT models:

```toml
default_model = "chatgpt/gpt-5.6-luna"

[providers.chatgpt]
type = "chatgpt"
models."gpt-5.6-luna" = {}
models."gpt-5.6-terra" = {}
models."gpt-5.6-sol" = {}
```

A provider with its own endpoint and per-model settings:

```toml
[providers.deepseek]
type = "openai-compatible"
base_url = "https://api.deepseek.com/v1"
api_key_env = "DEEPSEEK_API_KEY"
models."deepseek-chat" = { thinking = "high", context_window = 128000, compaction_window = 100000 }
```

### `[providers.<name>]`

| Field | Required | Meaning |
| --- | --- | --- |
| `type` | yes, except for built-in names | `claude`, `openai-compatible` or `chatgpt`. |
| `base_url` | `openai-compatible` only | Endpoint; Claude default `https://api.anthropic.com`. Rejected for `chatgpt`. |
| `api_key_env` | no | Environment variable holding the key; Claude default `ANTHROPIC_API_KEY`. Rejected for `chatgpt`. |
| `api_version` | no | Claude only; default unchanged. |
| `workspace_id_env` | no | Claude only. |
| `models` | no | Table of model tables keyed by model ID. |

A provider name uses ASCII letters, digits, `_` and `-`, so it never contains
`/`. Three providers are built in and named after the three types: `claude`,
`openai-compatible` and `chatgpt`, with today's defaults. A table with a
built-in name adds models to the built-in provider or overrides its fields;
its `type` may be omitted and, when present, must equal the name.

### `models."<model_id>"`

| Field | Required | Meaning |
| --- | --- | --- |
| `thinking` | no | `low`, `medium`, `high`, `xhigh` or `max`. |
| `context_window` | no | Same meaning and validation as today. |
| `compaction_window` | no | Same meaning and validation as today; requires `context_window`. |

The model ID is quoted. A model entry may be empty (`{}`); it then lists the
model in the `/model` picker and uses built-in limits. One model ID has one
entry per provider, so it has one set of settings; a different thinking effort
for one session is chosen with `--thinking` or the `/model` effort picker.

### Written form of a model

The parsed structure is `providers.<name>.models.<model_id>`, a table. TOML
spells that table in two ways, and Otto accepts both:

```toml
[providers.chatgpt]
type = "chatgpt"
models."gpt-5.6-luna" = { thinking = "high" }   # one line per model
```

```toml
[providers.chatgpt.models."gpt-5.6-luna"]       # one table header per model
thinking = "high"
```

The one-line form is the written form: `otto setup`, `otto config set`, the
`/model` save, `POST /v1/config/changes` (`set_model`, `set_model_field`) and
the legacy-profile diagnostic emit it, inside `[providers.<name>]`, and write
that header with `type = "<name>"` when the file does not hold it, built-in
providers included. An existing model is edited or removed where it is
written: a `models."<id>"` line is replaced or deleted; a
`[providers.<name>.models."<id>"]` table has its key set or is removed as a
whole. `otto_core::config::edit` implements both in-place edits
(`set_inline_table`, `set_value`, `remove_table`). The array form
`models = [{ id = "..." }]` was rejected: it changes the schema and the
addressing by model ID that `otto config set` and the HTTP changes use.

## Selection

A selector is `<provider>/<model_id>`, split at the first `/`; the model ID
may itself contain `/` (`openrouter/anthropic/claude-x`).

| Today | New | Meaning |
| --- | --- | --- |
| `default_profile = "luna"` | `default_model = "chatgpt/gpt-5.6-luna"` | Default model. |
| `--profile NAME` | `--model PROVIDER/MODEL_ID` | Model for this run. |
| `--provider NAME --model ID` | unchanged, deprecated | Same as `--model NAME/ID`. |
| `--model ID` without `--provider` | error | Hint: `--model <provider>/ID`. Otto does not take the provider from the default model. |
| `--base-url URL` | unchanged | Overrides the selected provider's endpoint. |
| `OTTO_PROFILE` | `OTTO_MODEL=PROVIDER/MODEL_ID` | |
| `OTTO_PROVIDER` + `OTTO_MODEL=ID` | unchanged, deprecated | Same as `OTTO_MODEL=PROVIDER/ID`. |
| `approval.reviewer_profile` | `approval.reviewer_model` | Selector. |

A selector may name a model ID with no model table, under any configured or
built-in provider; built-in limits and default settings then apply. This
replaces today's `--model ID` override. An unknown provider name is an error.

Precedence keeps today's order: `--model` (or deprecated `--profile`), then
`OTTO_MODEL`, then the resumed session's model (only when neither CLI nor
`OTTO_MODEL` selected one), then deprecated `OTTO_PROFILE`, then
`default_model`. `--provider` without `--model` is an error. A session
recorded before this change stores a provider type and model ID but no
selector; it resumes on the provider of `default_model` when that provider
has the same type, and otherwise on the built-in provider of that type.

## Name conflicts

| Case | Rule |
| --- | --- |
| The same model ID under two providers | Two models, `a/x` and `b/x`. |
| A provider name equal to a model ID | No ambiguity: the selector form fixes which part is which. |
| The same table twice in one file | Parse error from TOML. |
| `[providers.claude]`, `[providers.chatgpt]` or `[providers.openai-compatible]` | Extends or overrides the built-in provider; `type` is omitted or equals the name, otherwise an error. |
| The same provider or model in two layers (managed, user, project) | Merged by provider name, then model ID, then field. When a higher layer sets a provider `type` different from the lower layer's, the lower layer's `base_url`, `api_key_env`, `api_version` and `workspace_id_env` are dropped, the same rule as an MCP transport change (`layered.rs:131`); its model tables are kept. |

## Commands and APIs

- TUI/REPL `/model`: lists `provider/model_id` for every model table, grouped
  by provider; `/model <selector>` switches; `s` saves `default_model`.
- Chat connector `/model` and `/model <selector>`; replies say "model", not
  "profile". `chat-connector/internal/ottoconfig` reads `default_model` and
  `[providers.*.models]` (and the legacy tables).
- HTTP (writes keep today's typed-change flow: `POST /v1/config/changes`
  previews and creates an approval, `POST|DELETE /v1/config/changes/{id}`
  confirms or cancels):
  - `GET /v1/config/providers` replaces `GET /v1/config/profiles` and returns
    `{"default_model": "<selector>", "providers": [<provider>]}`. A provider
    object is `{"name", "type", "builtin", "base_url"?, "api_key_env"?,
    "api_version"?, "workspace_id_env"?, "models": [<model>]}` and a model
    object is `{"model_id", "selector", "default", "thinking"?,
    "context_window"?, "compaction_window"?}`. The three built-in providers are
    always listed. `GET /v1/config/providers/{name}` returns one provider
    object or `404 provider_not_found`; it replaces
    `GET /v1/config/profiles/{name}`.
  - `GET /v1/config/models?provider=<name>` lists the model IDs the provider
    API reports, as `{"provider": "<name>", "models": [...]}`; the `profile`
    query parameter is removed.
  - Change kinds for `POST /v1/config/changes` (field `kind`):
    `set_default_model {model}`; `set_provider {provider, type, base_url?,
    api_key_env?, api_version?, workspace_id_env?}` (create, or replace the
    connection fields; model tables are kept); `set_provider_field {provider,
    field, value}` with `field` one of `base_url`, `api_key_env`,
    `api_version`, `workspace_id_env`; `remove_provider {provider}` (removes
    its model tables; for a built-in name it removes the overrides);
    `set_model {model, thinking?}`; `set_model_field {model, field, value}`
    with `field` = `thinking`; `remove_model {model}`. `model` is a selector.
    Removing a provider or model that `default_model` or `reviewer_model`
    selects returns `409 model_in_use`. They replace `set_default_profile`,
    `create_profile`, `set_profile_field` and `remove_profile`. Change
    metadata reports `operation`, `target` (provider name or selector) and
    `field`, replacing `profile`.
  - `POST /v1/sessions/{id}/model` with `{"model":"<selector>"}` replaces
    `POST /v1/sessions/{id}/profile`. Errors: `400` for a missing or invalid
    selector or unknown provider; `409 turn_active`; `409
    model_switch_unavailable` (was `profile_switch_unavailable`).
  - In session, info and usage response objects, `provider` is the provider
    name (for the built-in providers it equals today's value),
    `provider_type` is added, `model` is the model ID, and `profile` is
    removed. `profiles` in `GET /v1/info` becomes `models`, a list of
    selectors.
  - otto-plus does not call these routes (checked 2026-11-01), so the old
    routes and change kinds are removed without aliases.
- ACP and chat `/model` replies: bare `/model` answers
  `Model <selector> (type <type>, thinking <effort>)`, then
  `Configured models: <selectors>` and
  `Switch with /model <provider>/<model_id>.`; `/model <selector>` answers
  `Switched to model <selector> (type <type>).`
- Approval-gated configuration changes: `ProfileChange` becomes `ModelChange`
  with one variant per change kind above. The approval artifact still
  contains no API-key values.
- `otto setup` writes `default_model`, `[providers.<name>]` with `type` (and
  `base_url`/`api_key_env` when given) and one `models."<id>" = {}` line.
- `otto config set 'providers.chatgpt.models."gpt-5.6-luna".thinking' '"high"'`
  and `otto config explain` use the new paths.

## Project scope

Projects may set `default_model`, and the fields `thinking`, `context_window`
and `compaction_window` of a model entry of `[providers.<name>]` where `<name>` is
a built-in or host-defined provider. Projects may not set any provider-level
field and may not define a provider. Selecting a model of a host-defined
provider is the same authority a project has today through
`default_profile`.

## Compatibility

### Legacy `[profiles.*]`

A source file that contains `[profiles.*]` or `default_profile` and no
`[providers]` or `default_model` is converted after all layers are loaded:

- Each profile `P` becomes the model `<provider>/<model>` with its `thinking`,
  `context_window` and `compaction_window`.
- `<provider>` is the built-in provider of its type when every connection
  field equals the built-in default (after applying today's defaults).
  Otherwise profiles with identical connection fields share one provider,
  named after the first such profile in name order: `P` when no built-in or
  configured provider in any layer has that name, and `P-custom` otherwise.
  A profile named `claude` with a custom `base_url` therefore becomes provider
  `claude-custom` and does not change the built-in `claude` used by other
  models.
- `default_profile = "P"`, `--profile P`, `OTTO_PROFILE=P` and
  `reviewer_profile = "P"` select the model converted from `P`.
- `subagent_models` (already deprecated) is carried as an internal field of
  the converted model; the new format has no such field.
- Two profiles that become the same `<provider>/<model>` with different
  `thinking` or window settings cannot both be represented. Loading fails
  with an error that names both profiles and the selector.

Each converted source produces one diagnostic, shown at startup and by
`otto config explain`, containing the equivalent new TOML. A source file that
mixes legacy and new tables is an error.

### Managed policy

Managed `force` accepts `default_model`, `providers.<name>.<field>` and
`providers.<name>.models."<id>".thinking`. A managed policy that still forces
`default_profile` or `profiles.*` fails to load with an error naming the new
keys, so a forced value is never silently dropped.

### Stored data

Session history (`otto.runtime` entry field `profile`), the usage database
column `profile` and the workflow table column `profile` keep their key names;
new writes store the selector there. Existing sessions, usage rows and
workflows load without migration.

## Rust names

`Profile` is replaced by `ProviderEntry` (with `models: BTreeMap<String,
ModelEntry>`) and `ModelEntry`; `File::profiles` becomes `File::providers`;
`File::default_profile` becomes `default_model`; a `ModelSelector { provider,
model_id }` type parses and prints selectors; `Runtime::profile` and
`Overrides::profile` are replaced by the selector. `ProfileSummary`,
`profile_change.rs`, `app/profile.rs` and related identifiers are renamed to
the model/provider domain. Sandbox "profile" identifiers (Seatbelt) are
unrelated and stay.

## Out of scope

- A command that rewrites a user's file to the new format. The diagnostic
  prints the replacement text.
- Model aliases or "latest generation" names.
- Removing the legacy spellings.
- Changes to built-in model limits or to sub-agent model selection.

## Tests

- Parsing and validation: built-in providers without a table, adding models
  to a built-in provider, overriding a built-in field, `type` mismatch on a
  built-in name, unknown `type`, fields invalid for the type, provider name
  with `/`.
- Selectors: split at the first `/`, model ID containing `/`, unknown
  provider, model ID without a table, `--model ID` without `--provider` fails
  with the hint, deprecated `--provider` + `--model` and `OTTO_PROVIDER` +
  `OTTO_MODEL` pairs, precedence with session and `default_model`.
- Name conflicts: the same model ID under two providers resolves to two
  connections; a higher-layer `type` change drops the lower layer's
  connection fields and keeps its model tables.
- Legacy conversion: built-in connection detection; profile `claude` with a
  custom `base_url` becomes `claude-custom` and another default-Claude profile
  keeps `https://api.anthropic.com`; shared custom connections; the
  same-selector conflict error; diagnostic text; mixed-file error;
  `reviewer_profile` mapping.
- Project-scope filter: allowed model fields, rejected provider fields and
  provider definitions.
- HTTP routes, change kinds and response fields; `/model` in
  TUI/REPL and chat connector; `ModelChange` approval preview contains no key
  values.
- Session history and usage records written before the change load and show
  the stored name.
