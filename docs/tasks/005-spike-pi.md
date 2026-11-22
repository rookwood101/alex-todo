# 005 — Spike: Pi RPC on this machine

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** prove Pi's RPC mode works here and lock the exact command lines for board mode and
power mode. This is a timeboxed investigation, not the bridge. Deliver findings, then stop.

**Depends on:** nothing. **Parallel with:** 010, 020.
**Owns:** `docs/tasks/005-findings.md`, `scripts/pi-smoke.ps1` (optional). Do not touch `src/`.

**Read first:** [Pi CLI integration](https://docs.pi.dev/latest/cli-integration),
[RPC](https://docs.pi.dev/latest/rpc), [RPC commands](https://docs.pi.dev/latest/rpc-commands),
[MCP](https://docs.pi.dev/latest/mcp), [security](https://docs.pi.dev/latest/security).

## Steps

1. Install and pin: `npm i -g @earendil-works/pi-coding-agent@1.6.0`. Record `pi --version`,
   Node version (needs ≥22.19; 24.11.1 is present), and whether `pi` resolves from PATH.
2. Auth DeepSeek: `pi auth login deepseek` (or `DEEPSEEK_API_KEY`). Run
   `pi --list-models deepseek` and record the exact model id to use in config.
3. RPC smoke: spawn `pi --mode rpc --no-session --provider deepseek --model <id>`. Write one
   `prompt` command with an `id`, read stdout as JSONL, and record the exact sequence for a
   normal turn: `response`, `message_update` text deltas, `agent_settled`, `agent_end`. Send
   `abort` mid-run and record how quickly the session returns to idle. Send `get_state`.
4. Tool allowlists: start sessions with `--tools read` and with
   `--tools 'mcp__rustboard__*'` (no server yet) and `--no-tools`. Record which tools the
   model is told about and whether a prompt can still invoke bash. Confirm board mode can be
   made shell-free.
5. MCP + trust: register a trivial stdio MCP server with `pi mcp add` at user level and as a
   project `.pi/mcp.json`. Confirm RPC mode cannot show a trust prompt; record whether
   `--approve`, `defaultProjectTrust`, or user-level config is needed. Confirm tool names are
   `mcp__<server>__<tool>`.
6. Write `docs/tasks/005-findings.md` (≤1 page): exact commands, model id, event samples
   (trimmed), abort latency, allowlist behavior, trust behavior, gotchas, and a recommended
   spawn command for 120.

## Acceptance

- A cold `pi` RPC prompt streams text deltas and settles; `abort` works.
- The exact board-mode and power-mode command lines are written down and reproducible.
- The MCP registration path that works in RPC (no interactive trust prompt) is identified.
- `docs/tasks/005-findings.md` is one page and contains no secrets.

## Out of scope

Rust process management, chat UI, the real MCP server, cost controls, session resume. If a step
fails, record the failure and the fallback (for example `--approve`, or user-level MCP config)
rather than building around it.
