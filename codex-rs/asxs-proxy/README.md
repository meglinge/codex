# asxs-proxy

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
client (OpenAI SDK / pi / any HTTP)  ──►  asxs-proxy  ──►  in-process codex app-server  ──►  chatgpt.com/backend-api/codex
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

## Build

The crate lives at `codex-rs/asxs-proxy` inside the Codex workspace (member
`asxs-proxy`, `workspace = ".."`), so it reuses the Codex build cache and pinned
toolchain (`1.95.0`). Run cargo from the workspace path:

```
cd D:\MegAiTools\codex\codex-rs
cargo build -p asxs-proxy            # -> target/debug/asxs-proxy.exe
cargo build -p asxs-proxy --release
```

(On the dev machine the sources live in `D:\MegAiTools\ASXSProxy` and
`codex-rs\asxs-proxy` is a junction to it; cargo must be run through the
workspace path so it can find the root manifest.)

### CI

`.github/workflows/asxs-proxy.yml` builds release binaries for
`x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc` on every push to the
`asxs-proxy` branch (workflow artifacts) and publishes a GitHub release for
tags named `asxs-proxy-v*`:

```
git tag asxs-proxy-v0.1.0
git push fork asxs-proxy-v0.1.0
```

Each archive contains `asxs-proxy`, `codex-code-mode-host`, the platform
sandbox helpers (`bwrap` on Linux; `codex-command-runner` +
`codex-windows-sandbox-setup` on Windows), `asxsproxy.example.toml` and this
README. Keep the helpers next to the `asxs-proxy` binary.

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

```
cp asxsproxy.example.toml asxsproxy.toml   # edit accounts / keys
..\target\debug\asxs-proxy.exe --config asxsproxy.toml
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
`x-asxs-account: <id>`, `x-asxs-codex-tools: none|full` (per-request override of
`defaults.codex_tools`).

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
offered.

## Smoke test

```
tests/smoke.sh http://127.0.0.1:8790
```
