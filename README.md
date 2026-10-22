# Indocode

A calm desktop workspace for coding agents — **any model, any provider**. Indocode runs
the coding CLIs you already use — **Claude Code, Codex, Kimi Code, OpenCode, GLM Coding
Plan and Xiaomi MiMo** (through Claude Code), **GitHub Copilot CLI, Antigravity,
xAI's Grok Build, DeepSeek and Command Code** (through ACP), and **Pi** and its fork
**omp** (through Pi's RPC mode) — side by side with your projects, Git, a terminal and a
browser in one window.

Unlike stock PandaCode, Indocode is building a **direct-API engine**: bring any model by
API key — OpenRouter, OpenAI, Anthropic, Google Gemini, Groq, Mistral, xAI, DeepSeek
API, local Ollama/LM Studio, or any OpenAI-compatible endpoint — with the same
streaming, tool-calling, approvals and steering as the CLI harnesses. No subscription
required.

> **This is a fork of [PandaCode](https://github.com/aicodeking/pandacode)** — a superb
> open-source desktop workspace for coding agents. Indocode is built on its code and
> design system under the [PandaCode License](LICENSE) (MIT with an attribution
> requirement for forks). Credit for the original architecture and UI belongs to the
> PandaCode contributors.

Free and open source. No account, no license key, no telemetry.

Built with Tauri 2, Rust, SQLite and plain TypeScript. It uses the operating system's
webview: no bundled Chromium, resident Node server, React runtime or embedded editor
engine.

## Status

Early fork: rebranded and tracking upstream. The direct-API provider engine
(OpenRouter / OpenAI / Anthropic / Gemini / Ollama keys) is in development. Until a
release is cut, build from source below.

## Features

- **Every harness in one place.** Pick the harness, model and reasoning effort
  per task; switch between them from the composer. See [providers](docs/providers.md).
- **Tasks and a task board.** Parallel tasks across projects, a Pending → In
  progress → Review → Completed [board](docs/task-board.md), message queueing
  and steering while an agent runs.
- **Agent teams.** An [architect and up to three workers](docs/architect-workers.md),
  mixing harnesses and models, or named team profiles.
- **Bots.** Put an agent behind Telegram, Slack, Discord, WhatsApp or a local web
  chat; each conversation is an ordinary task. See [bots](docs/bots.md).
- **Git and worktrees.** Diffs, staging, commits, per-task [worktrees](docs/worktrees.md)
  with a one-click local **Merge into main**, and GitHub issues and pull requests
  through `gh` — including a full [GitHub view](docs/github.md).
- **Review every turn.** Saved [turn changes](docs/turn-changes.md) with
  unified/split diffs, an embedded [browser](docs/browser.md), and file previews.
- **Approvals at hand.** Requests wait in a slim bar above the prompt, one line
  each, grouped with **Approve all** when there are several.
- **Quick overlay.** A global shortcut (default ⌘/Ctrl + Shift + Space) opens a
  launcher for running tasks from anywhere.
- **Agent usage.** Account limits and cost estimates for each installed harness,
  and one-click CLI updates.
- **Attachments.** Images, PDFs, documents, folders and a built-in mockup sketcher.
- **Local VMs and cloud sandboxes, simulators, deploys** — platform-dependent;
  see the [docs](docs/).

## Build from source

You need Node.js 22, stable Rust, and the
[Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) for your OS.

```sh
git clone https://github.com/anshuman1900/indocode.git
cd indocode
npm ci
npm run desktop        # same as: npm run tauri dev
```

`npm run dev` serves only the frontend. In a regular browser it runs a labeled,
device-local sample workspace for UI work; files, Git and terminals are simulated
there. `npm run tauri build` makes a local installer.

## Use

Open a project folder, then click **New task** or press `⌘/Ctrl N`. A centered
"What should we build in …?" page opens in the last active project with the message
box focused. Choose the harness and model from the searchable picker in the message
box; new tasks remember your last choice. The **+** button attaches images, documents,
folders, project files or a mockup. Open **Changes** to inspect and stage diffs, or
the terminal to run shell commands.

Keyboard shortcuts: `⌘/Ctrl N` new task, `⌘/Ctrl K` command palette, `⌘/Ctrl J`
terminal, `⌘/Ctrl B` sidebar, `⌘/Ctrl ,` settings, `⌘/Ctrl F` find in thread,
`Enter` send (queues while an agent runs), `⌘/Ctrl Enter` steer, `Shift Enter`
newline, `⌘/Ctrl +/−/0` interface size.

## Privacy

- **No telemetry, no analytics, no accounts.** Indocode does not phone home.
- **Your code stays on your machine.** Projects, conversations and settings are
  stored locally in SQLite (`indocode.sqlite3` in the per-user application data
  directory). Prompts go only to the coding CLI or service you pick.
- **It only talks to services you choose.** Provider keys live in the system
  keychain and are sent only to that provider.
- Automatic checks send no personal data: installed-CLI versions (about every 6 h),
  a public model price list (daily), and Indocode's own updates from this
  repository's releases (turn off in Settings → About).

## Tests

```sh
npx tsc --noEmit           # typecheck
npm run build && npm test  # unit tests (some read dist/)
npm run test:rust          # cargo test
npx playwright test        # browser suite (Chrome)
```

## Contributing

Bugs and ideas go in [GitHub issues](https://github.com/anshuman1900/indocode/issues).
Code conventions follow [AGENTS.md](AGENTS.md).

## License

[PandaCode License](LICENSE) (MIT with an attribution requirement for forks).
Per that license: **Indocode is a fork of
[PandaCode](https://github.com/aicodeking/pandacode)** © the PandaCode contributors.
Provider names and logos belong to their owners and do not imply endorsement.
