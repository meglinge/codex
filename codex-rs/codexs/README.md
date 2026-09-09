# codexs

OpenAI-compatible HTTP API (**Responses API** + **Chat Completions**) whose backend
is the real Codex agent, running **in-process** through the same embedding path
the interactive CLI (`codex-tui`) and `codex exec` use. Every upstream request is
assembled by Codex itself — base instructions, environment context, built-in
tools, headers (`originator`, `session-id`, `User-Agent`), ChatGPT auth, prompt
cache key — so traffic is indistinguishable from a normal Codex session.

Client-supplied `tools` are bridged as Codex **dynamic tools** (the mechanism the
VS Code extension uses): when the model calls one, the HTTP response ends with
the tool call, the client executes it, and the next HTTP request (tool output)
resumes the *same* Codex turn. This mirrors what `pi-claude-bridge` does for
Claude Code, without an ACP layer — the crate is a member of the `codex-rs`
workspace and links `codex-app-server-client` directly, so Codex can be patched
freely.

```
client (OpenAI SDK / pi / any HTTP)  ──►  codexs  ──►  in-process codex app-server  ──►  chatgpt.com/backend-api/codex
        tools + messages                  session/turn         thread/start (dynamicTools)
        ◄── SSE / JSON                    state machine        item/tool/call  ◄──►  client tool result
```

## Layout

| path | role |
| --- | --- |
| `src/codex/runtime.rs` | one `InProcessAppServerClient` per account (`CODEX_HOME`); JSON-RPC requests, per-thread event routing, auto-answers for approval prompts |
| `src/codex/pool.rs` | account pool / least-loaded selection |
| `src/bridge/turn.rs` | Codex turn state machine: maps `item/*` notifications to bridge events, parks `item/tool/call` requests until the client answers, splits a turn into HTTP "segments" |
| `src/bridge/mod.rs` | session resolution (`previous_response_id`, `x-asxs-session`, or transcript-prefix matching for stateless clients), thread/turn creation, steering, reaper |
| `src/bridge/canon.rs` | canonical message model, transcript keys, history preamble |
| `src/api/openai.rs` | Responses / Chat request lowering |
| `src/api/responses.rs` | Responses API SSE + JSON (`response.output_item.*`, `response.output_text.delta`, `response.reasoning_summary_*`, `response.function_call_arguments.*`) |
| `src/api/chat.rs` | Chat Completions chunks (`content`, `reasoning_content`, `tool_calls`, usage) |

## Install

Prebuilt binaries for Linux (x86_64, glibc) are published on the
[releases page](https://github.com/meglinge/codex/releases). One-liner:

```
curl -fsSL https://raw.githubusercontent.com/meglinge/codex/codexs/codex-rs/codexs/install.sh | bash
```

It installs into `~/.codexs/bin` (the helper binaries stay next to `codexs`),
symlinks `~/.local/bin/codexs` and creates `~/.codexs/codexs.toml` from the
example on first install. Overrides: `CODEXS_VERSION=0.153.4` (default: latest
release), `CODEXS_INSTALL_DIR`, `CODEXS_BIN_DIR`, `CODEXS_REPO`.

Then edit `~/.codexs/codexs.toml` and run `codexs`. The config file is looked up
as `$CODEXS_CONFIG`, then `./codexs.toml`, then `~/.codexs/codexs.toml`.

## Build

The crate lives at `codex-rs/codexs` inside the Codex workspace (member
`codexs`, `workspace = ".."`), so it reuses the Codex build cache and pinned
toolchain (`1.95.0`). Run cargo from the workspace path:

```
cd D:\MegAiTools\codex\codex-rs
cargo build -p codexs            # -> target/debug/codexs.exe
cargo build -p codexs --release
```

(On the dev machine the sources live in `D:\MegAiTools\ASXSProxy\codex-patch` and
`codex-rs\codexs` is a junction to it; cargo must be run through the
workspace path so it can find the root manifest.)

### CI

`.github/workflows/codexs.yml` builds release binaries for
`x86_64-unknown-linux-gnu` on every push to the `codexs` branch (workflow
artifacts) and publishes a GitHub release for tags named `codexs-v<version>`:

```
git tag codexs-v0.153.4
git push fork codexs-v0.153.4
```

`<version>` must be an official openai/codex release version. CI stamps it into
the workspace `version` before building, so `codexs --version`,
`clientInfo.version` and the `codex_cli_rs/<version>` User-Agent all report the
same version as the official CLI (branch builds use the latest official
release). The install script is attached to every release.

The archive contains `codexs`, `codex-code-mode-host`, the bundled `bwrap`
sandbox helper, `codexs.example.toml` and this README. Keep the helpers next
to the `codexs` binary.

### Helper binaries

Codex re-execs helpers found **next to `codex.codex_self_exe`**. Point that
setting at a directory that also contains them, or build them into the same
target dir:

```
cargo build -p codex-cli --bin codex            # codex.exe
cargo build -p codex-code-mode-host             # codex-code-mode-host.exe  (Code Mode: how the model calls tools on newer models)
cargo build -p codex-windows-sandbox --bins     # codex-command-runner.exe, codex-windows-sandbox-setup.exe (Windows sandbox)
```

Without `codex-code-mode-host` every tool call (including client dynamic
tools) fails with "failed to spawn code-mode host".

## Run

### Server mode (one account per instance)

`codexs server` serves exactly one Codex account whose credentials and
outbound proxy are given at startup. Downstream clients send no Codex
credentials (an optional `--api-key` protects the endpoint). Run one instance
per account, each on its own port and proxy:

```
# existing login (CODEX_HOME with auth.json from `codex login`)
codexs server --port 8790 --proxy socks5h://127.0.0.1:1080 --codex-home ~/.codex

# raw OAuth tokens (prefer the env vars so tokens stay out of `ps`)
CODEXS_ACCESS_TOKEN=eyJ... CODEXS_REFRESH_TOKEN=... \
codexs server --port 8791 --proxy http://user:pass@10.0.0.2:3128 --api-key k1

# an auth.json exported from another machine
codexs server --port 8792 --auth-file ./acct-b/auth.json
```

| option | env | meaning |
| --- | --- | --- |
| `--proxy URL` | `CODEXS_PROXY` | outbound proxy for upstream traffic (`http://`, `socks5://`, `socks5h://`); applied process-wide via `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`, so Codex's helper processes inherit it |
| `--codex-home DIR` | `CODEXS_CODEX_HOME` | use an existing `CODEX_HOME` |
| `--access-token JWT` (+ `--refresh-token`, `--id-token`, `--account-id`) | `CODEXS_ACCESS_TOKEN` ... | import tokens into a private `CODEX_HOME` under `--identity-root` (default `~/.codexs/identities`) |
| `--auth-file FILE` | `CODEXS_AUTH_FILE` | import an `auth.json` the same way |
| `--api-key KEY` | | downstream must send `Authorization: Bearer KEY` (repeatable; default open) |
| `--max-concurrent-turns N` | | per-account turn limit |

`--config` still applies for everything else (thread defaults, session
settings). The file's `codex.proxy` is the fallback when `--proxy` is absent.

### Config-file mode (account pool)

```
cp codexs.example.toml codexs.toml   # edit accounts / keys
..\target\debug\codexs.exe --config codexs.toml
```

Each `[[accounts]]` entry points at a `CODEX_HOME` with an `auth.json` from
`codex login` (its `config.toml` is honoured: model, provider, MCP servers…).
Sessions are pinned to the account that created them.

## API

* `POST /v1/responses` — `input` (string or items), `instructions`, `tools`
  (function), `previous_response_id`, `stream`, `reasoning.{effort,summary}`,
  `text.format` (json_schema → Codex `outputSchema`). Output items:
  `message`, `reasoning` (summary), `function_call`, and — when
  `api.expose_activity = true` — `codex_activity` items carrying Codex's own
  command executions / file changes / web searches.
* `POST /v1/chat/completions` — `messages`, `tools`, `stream`,
  `stream_options.include_usage`, `reasoning_effort`, `response_format`.
  Reasoning summaries and Codex activity stream as `reasoning_content`.
* `GET /v1/models` — Codex's `model/list`.
* `GET /v1/sessions` — live sessions / accounts (debug).

Request headers: `x-asxs-session: <sess_id>` (pin a session),
`x-asxs-account: <id>`, `x-asxs-codex-tools: passthrough|none|full` (per-request
override of `defaults.codex_tools`).

### Server mode: client-supplied credentials

With `[auth] client_credentials = "allowed"` (default) or `"required"`, a
request can carry its own ChatGPT OAuth material instead of relying on the
static `[[accounts]]`:

```json
{
  "model": "gpt-5.5",
  "input": "...",
  "asxs": {
    "auth": {
      "access_token": "eyJ…",          // required (JWT)
      "id_token": "eyJ…",              // optional
      "refresh_token": "…",            // optional; enables Codex's own refresh
      "account_id": "…"                // optional; else taken from the JWT claims
    }
  }
}
```

Equivalent headers: `x-codex-access-token`, `x-codex-id-token`,
`x-codex-refresh-token`, `x-codex-account-id`, or simply
`Authorization: Bearer <access token JWT>` (a JWT bearer is accepted in place of
the proxy API key). Each account gets `identity_root/<account id>/auth.json` in
Codex's own format and a dedicated in-process Codex, started on first use and
stopped after `identity_idle_ttl_secs` of inactivity. Sessions are scoped to
the identity; presenting a newer access token for the same account rewrites
`auth.json` and restarts that identity's Codex. `GET /v1/asxs/auth` (with the
same credentials) returns the stored, possibly Codex-refreshed, tokens.

### Per-request thread parameters

Anything Codex needs for the thread can be passed in an `asxs` object in the
body (or `x-asxs-*` headers for the scalar ones). They apply when a new Codex
thread is created for the conversation:

| field | effect |
| --- | --- |
| `session_id`, `account_id` | pin an existing session / static account |
| `model`, `reasoning_effort`, `reasoning_summary`, `service_tier` | same as the standard OpenAI fields |
| `codex_tools` | `"passthrough"`, `"none"` or `"full"` |
| `sandbox`, `approval_policy`, `personality`, `cwd`, `ephemeral` | `thread/start` settings |
| `developer_instructions` | appended to the system prompt's developer block |
| `base_instructions` | replaces Codex's base system prompt (deviates from stock Codex) |
| `config` | dotted `config.toml` overrides for the thread, e.g. `{"model_reasoning_effort": "high", "web_search": "disabled"}` |

### Conversation state

* Stateful: pass `previous_response_id` (Responses API) — the proxy continues
  the Codex thread, delivering `function_call_output` items to the pending
  `item/tool/call` requests.
* Stateless (Chat Completions, or Responses without `previous_response_id`):
  the proxy hashes the conversation prefix (instructions + tools + messages up
  to the new turn) and reuses the live session whose recorded state matches;
  a user message arriving while a tool call is pending abandons that call and
  interrupts the turn. Text sent alongside tool results is delivered with
  `turn/steer`.
* No match: a new thread is created. Prior history is seeded per
  `sessions.history_seeding` (`preamble` folds it into the first user message;
  `patch` uses `initialHistory` on `thread/start`, requires the Codex patch;
  `reject` returns 400).

### Codex tools vs client tools

`defaults.codex_tools = "full"` keeps Codex's shell / apply_patch / update_plan /
web-search tools exactly as a normal session would send them (the model can run
commands in the per-session workspace under the configured sandbox).
`"none"` disables them via config overrides (`features.shell_tool=false`,
`features.view_image=false`, `web_search="disabled"`,
`tools.update_plan.enabled=false`, `mcp_servers={}`) so only client tools are
offered — but Codex still normalizes their schemas and, on models that use
code mode, folds them into the `exec` tool.

`"passthrough"` (the default of `codexs server`, `--codex-tools` to change) makes
the client's `tools` the *entire* tool list on the wire, value-for-value as
received (raw JSON schema, `strict`, order, names untouched; only JSON object
key order may differ). Codex's tools,
MCP tools and code mode are off for that thread; everything else — base
instructions, environment context, headers, prompt cache key, telemetry — is
still produced by Codex. This needs the ASXS Codex patch (`tools.client_only`
config + `verbatim` dynamic tools); stock Codex rejects it with 400.

## Smoke test

```
tests/smoke.sh http://127.0.0.1:8790
```
