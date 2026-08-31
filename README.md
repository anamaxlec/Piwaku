# Piwaku

[中文](#中文) · [English](#english)

## 中文

Piwaku 是一个以 **Pi 生态为核心** 的 [Waku](https://github.com/egoist/waku) fork，目标是让 Pi 以及它的扩展生态在桌面 Coding Agent 客户端里拥有更自然、更完整的原生体验。

Piwaku 保留了 Waku 基于 Rust + GPUI 的桌面基础和多 Agent 工作流，同时针对 Pi 加入了更深的一层集成，包括扩展交互对话框、插件管理、Pi 设置、自定义 Provider / Model、运行时重载，以及对多个 Pi 插件的原生 UI 适配。

> Piwaku 主要是一个自用 fork。开发会优先围绕我自己的 Pi 工作流快速推进，因此在行为、发布节奏和平台支持上都可能逐渐与上游 Waku 产生差异。

### 为什么做 Piwaku？

Waku 本身已经提供了一个很不错的多 Coding Agent 原生桌面客户端，但在原版 Waku 中，Pi 更多只是其中一个 CLI Provider。

Piwaku 的目标是继续保留 Waku 这套基础，同时真正把 Pi 的扩展生态接进 GUI，让原本为 Pi TUI 或 RPC 事件设计的能力不至于在桌面客户端中丢失。

### Pi 集成

#### 原生 Extension UI Bridge

Pi 插件发出的 UI 请求可以直接映射到 Piwaku 的原生界面。

目前支持的 `ctx.ui` 交互包括：

- `select`
- `confirm`
- `input`
- `editor`
- 在支持的场景下显示通知和状态更新

因此，依赖 Pi 交互式 UI 的插件可以直接在 Piwaku 中提问并接收用户输入，不需要退回终端，也不会出现 RPC 模式下交互不可见的问题。

#### 插件管理器

Piwaku 增加了独立的 Pi 插件管理页面，用于管理 daemon 所在主机上的 Pi packages。

- 自动发现已安装的 Pi 插件
- 区分全局与项目级插件
- 启用或禁用插件
- 检查插件更新并执行更新
- 移除插件
- 查看插件配置
- 显示兼容状态，包括原生适配、Piwaku 接管、仅 TUI 可用和通用兼容

插件状态变更会在新启动的 Pi 会话中生效。

#### Pi 设置界面

常用 Pi 配置可以直接在 Piwaku 中管理，不需要手动编辑 JSON。

- 默认 Provider
- 默认 Model
- 默认 Thinking Level
- Quiet Startup
- 全局与项目级设置
- 插件设置
- 打开或在文件管理器中定位 Pi 配置文件
- 保留当前 session cursor 的情况下重载 Pi runtime

#### 自定义 Provider 与 Model

Piwaku 内置了 Pi `models.json` 的 Provider / Model 编辑器。

目前支持：

- `openai-completions`
- `openai-responses`
- `anthropic-messages`
- `google-generative-ai`

Provider API Key 不会回显到 UI 中。Pi 内置 Provider 和 OAuth Provider 不属于这个编辑器的管理范围，仍然交给 Pi 自己处理。

#### 更深层的插件原生适配

除了通用 Extension UI Bridge 以外，一些 Pi 插件还做了额外的原生集成：

- **pi-goal** — 将 goal 状态、运行状态和 goal 控制同步到桌面 UI
- **pi-plan-mode** — 在 Piwaku 中同步 Plan / Build 状态
- **rpiv-todo** — 将任务快照转换成原生 Todo 状态
- **Magic Context** — 在 UI 中反映插件实时状态和持久化 Context 状态
- **@gotgenes/pi-permission-system** — 将权限请求映射到 Piwaku 原生权限交互

这套集成层会继续扩展，目标不是给每个插件单独做一套终端兼容补丁，而是让更多 Pi 插件可以自然地接入桌面 UI。

#### Pi 与 Oh My Pi

Piwaku 为 Pi 和 Oh My Pi 共用一套 RPC Transport，并在内部处理二者之间的协议差异，包括 session branch / resume 行为以及 Oh My Pi 的 protocol v2 大帧分块机制。

### 保留的 Waku 能力

Piwaku 仍然保留来自 Waku 的核心工作流：

- 在一个原生桌面应用中管理多个项目与独立 Agent Session
- 支持 Claude Code、Codex CLI、Cursor CLI、OpenCode、Amp、Grok Build、Kimi Code、Pi 等 Coding Agent
- Model、Reasoning Effort 和 Access Mode 控制
- 可排队、可 steer 的后续消息
- 基于 Git 的任务 checkpoint 与 rewind
- Skills、Diff、文件编辑、Terminal 和 Usage 页面
- 本地保存项目、Session 与 Transcript
- 独立 daemon 架构与浏览器客户端

### 安装

Piwaku 当前提供 **macOS** 安装包。推荐直接从 [GitHub Releases](https://github.com/anamaxlec/Piwaku/releases/latest) 下载最新的 `.dmg`，安装后将 `Piwaku.app` 拖入 `/Applications`。

#### macOS 首次启动 / ad-hoc 签名说明

目前发布的 macOS 构建使用 **ad-hoc 签名**，没有 Apple Developer ID 签名和 notarization。因此首次启动时，macOS Gatekeeper 可能会提示无法验证开发者或阻止应用打开。

确认安装包来自本仓库后，可以使用以下任一方式放行：

1. 先尝试打开一次 Piwaku，然后进入 **系统设置 → 隐私与安全性**，在安全提示处点击 **仍要打开 / Open Anyway**，再确认启动。
2. 或在终端中只移除 Piwaku 的 quarantine 标记：

```sh
xattr -dr com.apple.quarantine /Applications/Piwaku.app
```

执行后重新打开 Piwaku 即可。这个命令只应对你确认来自本仓库 Release 的 `Piwaku.app` 使用。

#### 从源码运行

需要开发或自行构建时：

- Rust 1.96 或更高版本
- Bun
- [CONTRIBUTING.md](CONTRIBUTING.md) 中列出的 macOS 原生构建依赖

```sh
git clone https://github.com/anamaxlec/Piwaku.git
cd Piwaku
bun install
bun run dev
```

### 支持的 Agent

Piwaku 继承 Waku 的 Provider 支持，包括：

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

使用前需要先安装并完成相应 CLI 的认证。Piwaku 会自动检测可用 Provider，并在支持的情况下使用各 Provider 原生的结构化协议和 Session 机制。

### 项目状态

Piwaku 并不是为了成为所有人的 Waku 替代品。

它是一个围绕 Pi-heavy 工作流开发的个人 fork，尤其关注那些需要更丰富 GUI 集成的 Pi 扩展。部分功能可能仍处于实验阶段、不完整，或者专门针对我实际使用的插件与工作流设计。

如果需要更通用、面向所有用户的版本，请使用上游 [egoist/waku](https://github.com/egoist/waku)。

### 上游项目

Piwaku fork 自 [egoist](https://github.com/egoist) 的 [Waku](https://github.com/egoist/waku)。

桌面架构、daemon / protocol 设计、Provider 框架以及大部分通用能力均来自 Waku。Piwaku 的主要改动集中在 Pi 集成以及个人工作流适配上。

### License

Piwaku 延续上游项目的许可协议，使用 [GNU General Public License v3.0 only](LICENSE)。

---

## English

Piwaku is a **Pi-first** fork of [Waku](https://github.com/egoist/waku), focused on making Pi and its extension ecosystem feel native inside a desktop coding-agent client.

Piwaku keeps Waku's Rust + GPUI desktop foundation and multi-agent workflow, but adds a much deeper Pi integration layer: extension dialogs, plugin management, Pi settings, custom providers/models, runtime reloads, and native UI support for several Pi extensions.

> Piwaku is primarily a personal-use fork. It moves quickly around my own Pi workflow and may diverge from upstream Waku in behavior, release cadence, and supported platforms.

### Why Piwaku?

Waku already provides a fast native interface for multiple coding agents. Piwaku keeps that base, while treating Pi as more than just another CLI provider.

The goal is to make Pi extensions work naturally in a GUI instead of losing features that were designed around Pi's terminal UI or RPC events.

### Pi integrations

#### Native extension UI bridge

Pi extension UI requests are translated into Piwaku's native interface.

Supported `ctx.ui` interactions include:

- `select`
- `confirm`
- `input`
- `editor`
- notifications and status updates where supported

This allows extensions that expect interactive Pi UI to ask questions and receive answers without falling back to a broken or invisible terminal interaction.

#### Extension manager

Piwaku adds a dedicated Pi extensions page for managing packages on the daemon host.

- Discover installed Pi extensions
- Global and project-scoped extensions
- Enable or disable packages
- Check for updates and update packages
- Remove packages
- Inspect extension settings
- Show compatibility status for integrations that are native, replaced, TUI-only, or generic

Changes apply to newly started Pi sessions.

#### Pi settings UI

Common Pi configuration can be managed directly from Piwaku instead of editing JSON by hand.

- Default provider
- Default model
- Default thinking level
- Quiet startup
- Global and project settings
- Extension settings
- Open/reveal Pi configuration files
- Reload the Pi runtime while keeping the current session cursor

#### Custom providers and models

Piwaku includes an editor for Pi's `models.json` custom providers and models.

Supported API styles currently include:

- `openai-completions`
- `openai-responses`
- `anthropic-messages`
- `google-generative-ai`

Provider API keys are never echoed back into the UI. Pi built-in/OAuth providers are intentionally outside this editor and remain managed by Pi itself.

#### Deeper plugin integrations

Some Pi extensions receive additional native handling beyond the generic dialog bridge.

- **pi-goal** — goal state, status and goal controls are reflected in the desktop UI
- **pi-plan-mode** — Plan / Build state is synchronized with Piwaku
- **rpiv-todo** — task snapshots can be surfaced as native todo state
- **Magic Context** — extension status and persisted context status can be reflected in the UI
- **@gotgenes/pi-permission-system** — permission prompts are mapped into Piwaku's native permission flow

The integration layer is intentionally extensible, so more Pi extensions can be adapted without turning Piwaku into a collection of one-off terminal workarounds.

#### Pi and Oh My Pi

Piwaku shares a common RPC transport for Pi and Oh My Pi while handling their protocol differences internally, including session branching/resume behavior and Oh My Pi's protocol-v2 chunking.

### Waku features retained

Piwaku still keeps the core workflow inherited from Waku:

- Multiple projects and independent agent sessions in one native app
- Claude Code, Codex CLI, Cursor CLI, OpenCode, Amp, Grok Build, Kimi Code, Pi and other supported agents
- Model, reasoning-effort and access-mode controls
- Queued and steerable follow-up messages
- Git-backed task checkpoints and rewind
- Skills, diffs, file editing, terminal and usage views
- Local project/session/transcript storage
- Standalone daemon architecture and browser client

### Install

Piwaku currently provides a **macOS** build. Download the latest `.dmg` from [GitHub Releases](https://github.com/anamaxlec/Piwaku/releases/latest), install it, and move `Piwaku.app` to `/Applications`.

#### First launch on macOS / ad-hoc signing

Current macOS releases are **ad-hoc signed** and are not signed with an Apple Developer ID or notarized. Gatekeeper may therefore report that the developer cannot be verified or prevent the app from opening on first launch.

After confirming that the app came from this repository, use either of these methods:

1. Try opening Piwaku once, then go to **System Settings → Privacy & Security**, click **Open Anyway** for Piwaku, and confirm the launch.
2. Or remove the quarantine attribute for Piwaku only:

```sh
xattr -dr com.apple.quarantine /Applications/Piwaku.app
```

Then open Piwaku again. Only run this command for a `Piwaku.app` that you trust and downloaded from this repository's Releases.

#### Build from source

For development or local builds:

- Rust 1.96 or newer
- Bun
- macOS native build prerequisites from [CONTRIBUTING.md](CONTRIBUTING.md)

```sh
git clone https://github.com/anamaxlec/Piwaku.git
cd Piwaku
bun install
bun run dev
```

### Supported agents

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

### Project status

Piwaku is not intended to be a drop-in replacement for upstream Waku for everyone.

It is a personal fork built around a Pi-heavy workflow, especially extensions that need richer GUI integration. Features may be experimental, incomplete, or tailored to specific plugins I use.

For the general-purpose upstream project, use [egoist/waku](https://github.com/egoist/waku).

### Upstream

Piwaku is forked from [Waku](https://github.com/egoist/waku) by [egoist](https://github.com/egoist).

Most of the desktop architecture, daemon/protocol design, provider framework, and general Waku functionality originates upstream. Piwaku's changes focus primarily on Pi integration and personal workflow adaptations.

### License

Piwaku follows the upstream project and is licensed under the [GNU General Public License v3.0 only](LICENSE).
