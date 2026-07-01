#!/bin/sh
# Install the a3s-box agent skill into a coding agent's skills directory.
# One SKILL.md, reused across every agent that speaks the Agent-Skills (SKILL.md)
# format. Symlinks by default (single source of truth); --copy to detach.
#
# Usage:
#   ./install.sh [--copy] [--home] <agent>...
#   ./install.sh --dir <path>            # install into an explicit skills dir
#
#   agents:  agents  claude  codex  a3s-code  all
#     agents   -> .agents/skills   cross-tool standard: Codex, Gemini CLI, Amp,
#                                   Cursor, OpenCode, Zed all read this root
#     claude   -> .claude/skills   Claude Code/SDK, Cline, Cursor & OpenCode compat
#     codex    -> .codex/skills    Codex-specific; the a3s CLI menu also scans it
#     a3s-code -> .a3s/skills       a3s-code agent dir
#     all      -> agents + claude + codex + a3s-code
#   --home   install at user scope ($HOME) instead of the current project
#   --copy   copy the file instead of symlinking
#   --dir P  treat P as a skills root and drop a3s-box/SKILL.md inside it
#
# Examples:
#   ./install.sh all                     # wire every root in this repo
#   ./install.sh --home agents claude    # user-wide cross-tool + Claude Code
#   ./install.sh --dir ./my-agent/skills # any SKILL.md-format agent dir
set -eu

SRC="$(CDPATH= cd -- "$(dirname -- "$0")/a3s-box" && pwd)/SKILL.md"
[ -f "$SRC" ] || { echo "error: SKILL.md not found at $SRC" >&2; exit 1; }

COPY=0; SCOPE=project; DIR=""; AGENTS=""
while [ $# -gt 0 ]; do
  case "$1" in
    --copy) COPY=1 ;;
    --home) SCOPE=home ;;
    --dir)  shift; DIR="${1:?--dir needs a path}" ;;
    agents|claude|codex|a3s-code|all) AGENTS="$AGENTS $1" ;;
    -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
    *) echo "error: unknown arg '$1'" >&2; exit 1 ;;
  esac
  shift
done

# skills root for a named agent at the chosen scope
root_for() {
  base="."; [ "$SCOPE" = home ] && base="$HOME"
  case "$1" in
    agents)   echo "$base/.agents/skills" ;;  # cross-tool: Codex/Gemini/Amp/Cursor/OpenCode/Zed
    claude)   echo "$base/.claude/skills" ;;
    codex)    echo "$base/.codex/skills" ;;
    a3s-code) echo "$base/.a3s/skills" ;;      # agent-dir convention; pass --dir for a custom agent
  esac
}

place() {  # place <skills-root>
  dest="$1/a3s-box"
  mkdir -p "$dest"
  if [ "$COPY" -eq 1 ]; then
    cp "$SRC" "$dest/SKILL.md"; echo "copied   -> $dest/SKILL.md"
  else
    ln -sf "$SRC" "$dest/SKILL.md"; echo "linked   -> $dest/SKILL.md"
  fi
}

[ -n "$DIR" ] && { place "$DIR"; }

case "$AGENTS" in *all*) AGENTS="agents claude codex a3s-code" ;; esac
for a in $AGENTS; do place "$(root_for "$a")"; done

[ -z "$DIR$AGENTS" ] && { echo "nothing to do — pass an agent (agents|claude|codex|a3s-code|all) or --dir" >&2; exit 1; }
echo "done. reload the agent to pick up the skill."
