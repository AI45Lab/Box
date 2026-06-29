# a3s-box agent skill

One `SKILL.md` that teaches an AI coding agent to drive the `a3s-box` CLI. It
uses the cross-tool **Agent Skills** format (`<name>/SKILL.md`), so the *same
file* works in every agent that supports skills — there is no per-agent variant.

## Install

```sh
./install.sh all                   # .agents + .claude + .codex + .a3s, this repo
./install.sh --home agents claude  # user-wide (~/.agents, ~/.claude)
./install.sh --dir ./agent/skills  # any SKILL.md-format skills dir
./install.sh --copy all            # copy instead of symlink
```

The installer symlinks the one `SKILL.md` into each skills root (single source
of truth). Manual equivalent: `ln -s "$(pwd)/a3s-box/SKILL.md" <root>/a3s-box/SKILL.md`.

## Which agents this reaches

Two skills roots cover almost every skill-capable coding agent (2026):

| Skills root | Reached by |
|-------------|-----------|
| `.agents/skills/` | OpenAI Codex · Gemini CLI · Sourcegraph Amp · Cursor · OpenCode · Zed |
| `.claude/skills/` | Claude Code · Claude Agent SDK · Cline · Cursor (compat) · OpenCode (compat) · the a3s CLI menus |
| `.codex/skills/` | Codex (project-specific path) |
| `.a3s/skills/` | a3s-code |

`--home` writes the `~/...` equivalents (`~/.agents/skills`, `~/.claude/skills`,
…). Reload the agent to pick up the skill. The skill directory name (`a3s-box`)
becomes the `/a3s-box` slash command.

## Agents without a skill mechanism

Some agents have no on-demand skills, only always-on instruction files:
**GitHub Copilot** (`.github/copilot-instructions.md`), **Windsurf / Devin**
(`.devin/rules/`), **Continue.dev** (`.continue/rules/`), **Aider**
(`CONVENTIONS.md`), **Jules / Factory** (`AGENTS.md`).

We deliberately do *not* ship a copy in each bespoke rules format. If you want
one of these to know about a3s-box, add a one-line pointer to `a3s-box/SKILL.md`
in that tool's instructions file. Most of them also read the cross-tool
**`AGENTS.md`** at the repo root, so a single `AGENTS.md` line reaches the
majority. (Claude Code is the exception — it reads `CLAUDE.md`, not `AGENTS.md`.)

`claude.ai` and the Claude API do not read the filesystem — upload the `a3s-box/`
folder as a skill ZIP / via the Skills API.

## Why a skill, not a plugin

A Claude Code *plugin* would only help Claude Code. A shared `SKILL.md` is the
single format the whole ecosystem discovers, so one file covers everything.

Note: the a3s-code loader caps skill bodies at 10 KiB and is fail-secure on
`allowed-tools` (omitting it denies all tool use), so the `SKILL.md` is kept
tight and declares `Bash(a3s-box*)`.
