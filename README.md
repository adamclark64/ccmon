# ccmon — claude code monitor

Interactive TUI for active Claude Code agents and subagents. Two-pane picker: list on the left, live-tailing transcript preview on the right. Select one and a new terminal tab opens with a live viewer of that session — for tmux-backed sessions you actually attach and can drive it.

## What it shows

Walks `~/.claude/projects/` and lists:

- **agent** — top-level sessions (`<project>/<session>.jsonl`)
- **subagent** — Task-tool subagent runs (`<project>/<session>/subagents/agent-*.jsonl`)

Sorted live → running → others, newest first within each group. Default age cutoff is 6h (override with `CCMON_MAX_AGE_HOURS`).

## Usage

```sh
ccmon                  # interactive TUI picker
ccmon --list           # plain-text list, non-interactive
ccmon spawn [PATH]     # launch claude in a new Ghostty window wrapped in a named tmux session
```

### TUI keys

| key | action |
| --- | --- |
| `↑` / `↓` / PgUp / PgDn | move selection |
| type letters | filter list (summary + project + agent-type) |
| `⌫` | delete filter char |
| `enter` | attach to selected (tmux attach for live, transcript tailer otherwise) |
| `esc` / `q` / `ctrl-c` | quit |

### Picker behavior

Each entry has one of three states, detected by scanning `ps` for `claude --session-id <UUID>` processes and cross-referencing their TTY with `tmux list-panes`:

- **● live** — running claude process whose TTY is a tmux pane. Selecting splits the current terminal pane (sends Cmd+D) and runs `tmux attach -t <session>` (real attach: typing drives the same claude).
- **○ running** — running claude process not inside tmux. Selecting splits the current pane and opens the transcript tailer (read-only — you can see what it's doing live, but there's no PTY to attach to since it was spawned in a raw terminal).
- **· stale** — historical session; transcript tailer only.

### Making agents attachable

If you launch `claude` directly in Ghostty, it's `○ running` but not attachable. To make it real-attachable, launch it inside tmux. The `ccmon spawn` helper does this for you:

```sh
cd ~/repos/job-search-app
ccmon spawn               # opens new Ghostty: `tmux new-session -s claude-job-search-app claude`
```

You can also just use plain tmux — any naming convention works since detection is by PID+TTY, not by session name.

## Install

### Homebrew (recommended)

```sh
brew install adamclark64/tap/ccmon
```

(Requires the [adamclark64/homebrew-tap](https://github.com/adamclark64/homebrew-tap) tap; the formula in `homebrew/ccmon.rb` is auto-updated on every release.)

### From crates.io

```sh
cargo install ccmon
```

### From source

```sh
cargo install --path .
```

## Runtime deps

- `jq` (used by the transcript viewer; degrades to raw `tail -f` if absent)
- `tmux` (required for live attach)
- macOS Accessibility permission for your terminal app (so ccmon can send Cmd+D to split the pane). First run will silently fail to split if permission isn't granted — ccmon prints the manual command as fallback. Grant via System Settings → Privacy & Security → Accessibility → enable your terminal (Ghostty, iTerm, etc).
- Any terminal that maps Cmd+D to "split right" (Ghostty default, iTerm2 default, Warp default).
