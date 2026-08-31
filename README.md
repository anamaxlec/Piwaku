# Piwaku

A Pi-first fork of [Waku](https://github.com/egoist/waku), focused on making Pi and its extension ecosystem feel native inside a desktop coding-agent client.

Piwaku keeps Waku's Rust + GPUI desktop foundation and multi-agent workflow, but adds a much deeper Pi integration layer: extension dialogs, plugin management, Pi settings, custom providers/models, runtime reloads, and native UI support for several Pi extensions.

> Piwaku is primarily a personal-use fork. It moves quickly around my own Pi workflow and may diverge from upstream Waku in behavior, release cadence, and supported platforms.

## Why Piwaku?

Waku already provides a fast native interface for multiple coding agents. Piwaku keeps that base, while treating Pi as more than just another CLI provider.

The goal is to make Pi extensions work naturally in a GUI instead of losing features that were designed around Pi's terminal UI or RPC events.

## Pi integrations

### Native extension UI bridge

Pi extension UI requests are translated into Piwaku's native interface.

Supported `ctx.ui` interactions include:

- `select`
- `confirm`
- `input`
- `editor`
- notifications and status updates where supported

This allows extensions that expect interactive Pi UI to ask questions and receive answers without falling back to a broken or invisible terminal interaction.

### Extension manager

Piwaku adds a dedicated Pi extensions page for managing packages on the daemon host.

- Discover installed Pi extensions
- Global and project-scoped extensions
- Enable or disable packages
- Check for updates and update packages
- Remove packages
- Inspect extension settings
- Show compatibility status for integrations that are native, replaced, TUI-only, or generic

Changes apply to newly started Pi sessions.

### Pi settings UI

Common Pi configuration can be managed directly from Piwaku instead of editing JSON by hand.

- Default provider
- Default model
- Default thinking level
- Quiet startup
- Global and project settings
- Extension settings
- Open/reveal Pi configuration files
- Reload the Pi runtime while keeping the current session cursor

### Custom providers and models

Piwaku includes an editor for Pi's `models.json` custom providers and models.

Supported API styles currently include:

- `openai-completions`
- `openai-responses`
- `anthropic-messages`
- `google-generative-ai`

Provider API keys are never echoed back into the UI. Pi built-in/OAuth providers are intentionally outside this editor and remain managed by Pi itself.

### Deeper plugin integrations

Some Pi extensions receive additional native handling beyond the generic dialog bridge.

- **pi-goal** — goal state, status and goal controls are reflected in the desktop UI
- **pi-plan-mode** — Plan / Build state is synchronized with Piwaku
- **rpiv-todo** — task snapshots can be surfaced as native todo state
- **Magic Context** — extension status and persisted context status can be reflected in the UI
- **@gotgenes/pi-permission-system** — permission prompts are mapped into Piwaku's native permission flow

The integration layer is intentionally extensible, so more Pi extensions can be adapted without turning Piwaku into a collection of one-off terminal workarounds.

### Pi and Oh My Pi

Piwaku shares a common RPC transport for Pi and Oh My Pi while handling their protocol differences internally, including session branching/resume behavior and Oh My Pi's protocol-v2 chunking.

## Waku features retained

Piwaku still keeps the core workflow inherited from Waku:

- Multiple projects and independent agent sessions in one native app
- Claude Code, Codex CLI, Cursor CLI, OpenCode, Amp, Grok Build, Kimi Code, Pi and other supported agents
- Model, reasoning-effort and access-mode controls
- Queued and steerable follow-up messages
- Git-backed task checkpoints and rewind
- Skills, diffs, file editing, terminal and usage views
- Local project/session/transcript storage
- Standalone daemon architecture and browser client

## Install

Piwaku currently targets **macOS** for my own builds.

There are no public binary releases yet. Build from source for now.

Requirements:

- Rust 1.96 or newer
- Bun
- macOS native build prerequisites from [CONTRIBUTING.md](CONTRIBUTING.md)

```sh
git clone https://github.com/anamaxlec/Piwaku.git
cd Piwaku
git switch piwaku/dialog-bridge
bun install
bun run dev
```

The active Piwaku changes currently live on the `piwaku/dialog-bridge` branch while the fork is being developed.

## Supported agents

Piwaku inherits Waku's provider support, including:

- [Amp](https://ampcode.com/)
- Claude Code
- Codex CLI
- Cursor CLI
- [Fx](https://fx.sh/)
- Grok Build
- Kimi Code
- OpenCode
- Pi
- Oh My Pi

Install and authenticate the corresponding CLI before starting a session. Piwaku detects available providers and uses their native structured/session protocols where supported.

## Project status

Piwaku is not intended to be a drop-in replacement for upstream Waku for everyone.

It is a personal fork built around a Pi-heavy workflow, especially extensions that need richer GUI integration. Features may be experimental, incomplete, or tailored to specific plugins I use.

For the general-purpose upstream project, use [egoist/waku](https://github.com/egoist/waku).

## Upstream

Piwaku is forked from [Waku](https://github.com/egoist/waku) by [egoist](https://github.com/egoist).

Most of the desktop architecture, daemon/protocol design, provider framework, and general Waku functionality originates upstream. Piwaku's changes focus primarily on Pi integration and personal workflow adaptations.

## License

Piwaku follows the upstream project and is licensed under the [GNU General Public License v3.0 only](LICENSE).
