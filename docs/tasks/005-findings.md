# 005 — Pi RPC spike findings

**Status:** pass — RPC protocol, streaming, abort, allowlists and MCP registration verified here.
**Pi 1.6.0**, Windows 11, pwsh 7, Node v24.11.1, npm 11.6.2. `[obs]` = measured; `[doc]` = from the docs below.

## Install, auth, model `[obs]`

`npm i -g @earendil-works/pi-coding-agent@1.6.0` → "added 123 packages in 27s"; `pi --version` → `1.6.0`;
`npm prefix -g` → `C:\nvm4w\nodejs`. `pi` resolves from PATH as `C:\nvm4w\nodejs\pi.ps1` (npm also writes
`pi.cmd` → `node ...\dist\bundle\cli.js`).
Cold state had no `DEEPSEEK_API_KEY` and an empty `~/.pi/agent/auth.json`: `pi auth check --provider deepseek --json`
→ `{"status":"not_ready","reason":"credentials_not_configured"}` (there is no `pi auth status`); `pi --list-models deepseek`
→ `No models available`; a `prompt` returned `{"command":"prompt","success":false,"error":"No API key found for deepseek."}`
while `get_state` still worked. With a key supplied as `DEEPSEEK_API_KEY` in the child env (never printed or
written) `auth check` → `{"status":"ready","authType":"api_key"}`, and:

`pi --list-models deepseek` → `deepseek-flash` (1M context, 384K max-out, thinking, images) and `deepseek-v4-pro`
(no images). Use **`deepseek/deepseek-flash`** (`--provider deepseek --model deepseek-flash`); default `thinkingLevel` is `high`.

## RPC turn `[obs]`

`pi --mode rpc --no-session --provider deepseek --model deepseek-flash --no-tools`, stdin
`{"id":"p1","type":"prompt","message":"Reply with exactly: pong"}`. Trimmed stdout JSONL (ms after spawn):

```
1633 response prompt {disposition:"started"}
1634 agent_start; turn_start
1635 message_start system/message_end; message_start user/message_end
1991 message_start assistant {stopReason:"pending"}
2279 message_update text_start; text_delta "p"
2301 message_update text_delta "ong"
2302 message_update text_end; message_end {stopReason:"stop"}
2303 turn_end; agent_end {willRetry:false}; agent_settled {aborted:false}
```

Prompt→settled **670 ms**. `agent_end` is not idle; wait for `agent_settled`. `get_state` returns
`model{provider,id}, thinkingLevel, isStreaming, messageCount, sessionId` and **no tool list**.

Abort mid-run (prompt: count 1..100), `abort` written at t+2724: `message_end {stopReason:"aborted",errorMessage:"Request was aborted"}`
at 2735, `agent_settled {aborted:true}` at 2737, `response abort {success:true}` at 2737 → **13 ms abort→idle**
(4 ms when already idle). `prompt success:true` only means accepted; provider failures arrive in the stream or as
an immediate `success:false`.

## Tool allowlists `[obs]`

Measured with a temporary extension calling `pi.getActiveTools()` 3 s after `session_start` (MCP connects async):

| flags | active tools |
|---|---|
| `--no-tools` | none |
| `--tools read` | `read` |
| `--tools mcp__probe__*` (server `exposure:"direct"`) | `mcp__probe__ping`, `mcp__probe__echo` — **no bash/powershell** |
| `--no-builtin-tools --tools mcp__probe__*` | same two |
| `--tools read,bash,powershell,edit,write,grep,find,ls,mcp__probe__*` | 8 built-ins + 2 MCP |
| `--tools read` with MCP connected | `read` only (MCP registered, inactive) |

Board mode is shell-free. **Gotcha:** with the default MCP exposure (`getAllTools()` reported `deferred`) `--tools 'mcp__probe__*'`
yielded **zero active tools** and Pi notified `"MCP tools are only reachable from the codemode or tool_search tool, but neither is active; they cannot be called."`
Set `"exposure":"direct"` (or `toolExposure:{"*":"direct"}`) on the rustboard server. Adding `codemode` to the allowlist
also works, but then tools are only reachable from JS scripts.

## MCP and trust `[obs]`

- `pi mcp add probe --exposure direct -- node probe-mcp.mjs` → writes `~/.pi/agent/mcp.json`; `pi mcp list` →
  `probe: connected, 2 tools`; model-facing names are `mcp__probe__ping` / `mcp__probe__echo`.
- RPC with that user-level file connected the server with no trust prompt (probe wrote its initialize/tools-list marker).
- Project `.pi/mcp.json` + RPC **without** `--approve`: ignored, no trust UI record, server never started;
  `pi mcp list` says it is ignored because the project is not trusted. **With** `--approve`: connects.
- RPC cannot show the trust prompt; without an override or saved/default decision it silently skips project
  resources. `[doc]` `defaultProjectTrust:"always"` in `~/.pi/agent/settings.json` is the alternative; `--approve` is per-run.

## Recommended for task 120

Board (register `rustboard` with `"exposure":"direct"` in project `.pi/mcp.json`):

```
pi --mode rpc --no-session --provider deepseek --model deepseek-flash --no-builtin-tools --tools "mcp__rustboard__*" --approve
```

Power (opt-in, warn in the UI):

```
pi --mode rpc --no-session --provider deepseek --model deepseek-flash --tools "read,bash,powershell,edit,write,grep,find,ls,mcp__rustboard__*" --approve
```

`--approve` is only needed for project-level config; a user-level `~/.pi/agent/mcp.json` needs no flag but changes
the user's global Pi config. On Windows spawn via `cmd.exe /d /s /c pi ...` or `node ...\pi-coding-agent\dist\bundle\cli.js ...`:
`pi` resolves to `.cmd`/`.ps1`, which `CreateProcess` cannot execute directly. Keep `--no-session`; parse stdout as
LF-delimited JSON only (not Node `readline` — U+2028/U+2029 are valid inside JSON); treat stderr as diagnostics; close
stdin for orderly shutdown.

Docs: [RPC](https://docs.pi.dev/latest/rpc), [RPC commands](https://docs.pi.dev/latest/rpc-commands), [JSON events](https://docs.pi.dev/latest/json),
[MCP](https://docs.pi.dev/latest/mcp), [security/trust](https://docs.pi.dev/latest/security), [CLI integration](https://docs.pi.dev/latest/cli-integration).