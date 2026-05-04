# acc

`acc` is an interactive session switcher for Claude Code and Codex. It finds local agent sessions across projects, Git worktrees, and Jujutsu (`jj`) workspaces, then resumes the selected session from its recorded directory.

## Install

```sh
cargo install --git https://github.com/tommyip/agent-cli-cli acc
```

## Usage

```sh
acc
```

The picker opens with four inputs:

```text
Provider: both
Title:    ...
Location: ...
Messages: ...
```

Use the provider field to switch between all sessions, Claude Code sessions, and Codex sessions. Use title search when you remember the task, location search when you remember the directory, and message search when you remember something said in the chat.

Rows include the quick-launch key, provider, title, location, token count when available, and update time:

```text
key  provider  title/summary  location  tokens  updated
```

The location column shows the session cwd. Long paths are shortened from the left, and paths under `$HOME` use `~`.

## Controls

```text
tab          focus next input
shift-tab    focus previous input
space        cycle provider, when provider is focused
up/down      move selected session
enter        launch selected session
ctrl-1..9    launch visible row by number
esc          quit
ctrl-c       quit
```

## Search

Search is fuzzy. Title search matches titles, summaries, first prompts, first user messages, and names or slugs. Location search matches the session cwd. Message search matches transcript contents and shows the matching chat turn in the preview. When multiple searches are set, sessions must match all of them.

## Launching

When you launch a session, `acc` changes to that session's recorded working directory before running the underlying CLI through your interactive shell. This lets ordinary `fish`, `zsh`, and `bash` aliases or functions for `claude` and `codex` apply.

For Codex:

```sh
cd <session-cwd> && $SHELL -ic 'codex resume <session-id>'
```

For Claude Code:

```sh
cd <session-cwd> && $SHELL -ic 'claude --resume <session-id>'
```

If the recorded directory no longer exists, `acc` shows an error instead of launching.

## Data Sources

`acc` reads Claude Code and Codex session files from `~/.claude` and `~/.codex`. These files are treated as read-only.

Codex sessions mirror `codex resume --all`: active CLI or VS Code threads with a recorded first user message. Claude Code subagent, sidechain, and generated command sessions are hidden when they can be identified from local metadata.

## Requirements

`acc` supports Linux and macOS. To launch sessions, `claude` and/or `codex` must be available on `PATH`. `fzf` is not required.

## License

MIT
