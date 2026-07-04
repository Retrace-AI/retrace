# Retrace

**A local-first, provider-agnostic coding agent for your terminal.**

Retrace is a community fork of [OpenAI Codex](https://github.com/openai/codex) (Apache-2.0),
reworked so you can point it at **any** model provider — OpenAI-compatible or
Anthropic-compatible — by just giving it a URL and an API key. It probes each
model's real capabilities live, streams responses and reasoning, and runs
collaboration modes that are sandboxed by your OS, not by prompt text.

## Install (macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/Retrace-AI/retrace/main/install.sh | bash
```

Then:

```sh
retrace
```

On first launch there are no models configured — inside Retrace, run **`/model` →
"Add custom model"** and paste your provider's base URL and API key. That's it.

> Requires [Node.js](https://nodejs.org) (for the local proxy). macOS only for
> v1; Linux/Windows are on the roadmap.

## What Retrace adds over Codex

- **Bring any provider.** `/model` → *Add custom model* connects OpenAI-, Anthropic-,
  GLM-, Qwen-, DeepSeek-, Kimi-, MiniMax-, Grok-compatible endpoints. `/model add`
  enables more models from your connected providers.
- **Live capability probing.** `/model probe` detects each model's thinking format,
  validated reasoning-effort levels, prompt-cache support, streaming shape, and real
  context window — against the live endpoint, not a hardcoded table.
- **Always-streaming.** Tokens and reasoning stream live; `/thinking show|hide`
  controls whether the reasoning block is displayed.
- **Weak-model helper.** `/agent-check` re-checks each answer and auto-retries until
  it's actually complete.
- **OS-sandboxed modes.** Shift+Tab cycles modes; **Ask** is read-only and **Readonly
  Research** limits writes to the working directory — enforced by the OS sandbox
  (Seatbelt), with approve-to-escalate when a write is genuinely needed.
- **Usage you can see.** Status line shows input / output / **cached** tokens, with
  the most recent request in brackets, plus the probed context window.

## How it works

Retrace ships three pieces, wired up by the installer:

| Piece | Role |
| --- | --- |
| `retrace` (Rust binary) | the TUI / agent, a fork of the Codex CLI |
| `responses-chat-proxy.mjs` | a local proxy translating the Responses API to/from chat-completions, streaming, and running Agent Check |
| `retrace-admin.mjs` | the provider/model registry and live capability-probe engine |

Everything runs locally under `~/.retrace`. Your API keys never leave your machine.

## Build from source

```sh
git clone https://github.com/Retrace-AI/retrace.git
cd retrace/codex-src/codex-rs      # the Rust workspace (Codex fork)
cargo build --release -p codex-cli --bin retrace
```

The full fork source lives under `codex-src/` (see [`docs/BUILD.md`](docs/BUILD.md)).

## License & attribution

Retrace is licensed under the [Apache License 2.0](LICENSE). It is a modified fork of
OpenAI Codex; upstream copyright and attribution are retained in [`NOTICE`](NOTICE).
Retrace is not affiliated with or endorsed by OpenAI.
