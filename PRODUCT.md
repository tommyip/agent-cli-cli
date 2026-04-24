# Agent Sessions Product Doc

## Problem

Developers who use Claude Code and Codex across many projects can lose track of which agent session belongs to which directory. The problem gets worse when one logical repository has multiple Jujutsu (`jj`) workspaces, because sessions from the same repo may live in different physical directories.

The tool should make it fast to find the right session, understand where it belongs, and resume it from the correct working directory.

## Product Goal

Build `acc` (`agent-cli cli`), an interactive-first terminal app for browsing, fuzzy-searching, and launching Claude Code and Codex sessions across all local projects and workspaces.

The app should feel like a focused session switcher: open it, type enough to find the session, press enter, and land back in the right agent session with the correct current directory.

## Non-Goals

- It is not a general transcript viewer.
- It is not a replacement for Claude Code or Codex resume internals.
- It is not a shell-script wrapper around `fzf`.
- It does not need non-interactive command workflows for the first version.
- It does not need to support Windows for the first version; Linux and macOS are the target platforms.

## Target Platforms

- Linux
- macOS

The tool should be distributed as a Rust binary and should not require users to install `fzf`.

## Default Experience

Running:

```sh
acc
```

opens a full-screen terminal UI.

The first screen contains:

- A provider selector.
- A title fuzzy-search input.
- A location fuzzy-search input.
- A message fuzzy-search input.
- A ranked session list.
- A preview/details panel for the selected session.

The app starts with provider set to `both`, title search empty, location search empty, and message search empty. Sessions are sorted by most recently updated when no search is active.

## Inputs

There are four primary input areas:

```text
Provider: both
Title:    <fuzzy title query>
Location: <fuzzy location query>
Messages: <fuzzy message query>
```

### Focus

`tab` cycles focus forward:

```text
provider -> title -> location -> messages -> provider
```

`shift-tab` cycles focus backward:

```text
provider <- title <- location <- messages <- provider
```

The focused input should be visually obvious.

### Provider Selector

When provider is focused, `space` cycles:

```text
both -> claude code -> codex -> both
```

The provider selector filters the result list immediately.

### Title Search

When title is focused, typed text updates the title fuzzy query.

The title query searches fuzzy against the session's short identifying text:

- Codex title.
- Codex first user message.
- Claude summary.
- Claude first prompt.
- Claude session name or slug, when present.

### Location Search

When location is focused, typed text updates the location fuzzy query.

The location query searches fuzzy against the displayed session cwd, including the `~` shorthand for paths under `$HOME`.

### Message Search

When messages is focused, typed text updates the message fuzzy query.

The message query searches fuzzy against transcript contents.

This search is intended for cases where the user remembers something discussed in the chat but does not remember the session title or directory.

## Result Matching

Search is fuzzy, not keyword or boolean search.

The active result set is determined by:

- Provider filter.
- Title fuzzy query.
- Location fuzzy query.
- Message fuzzy query.

If multiple queries are non-empty, a session must match all of them to appear.

Ranking should combine title, location, and message relevance with recency. A strong title match should generally beat a weak transcript match because the title is the fastest mental handle for a session.

When all queries are empty, recency is the primary ranking signal.

## Session List

Each row should make workspace confusion hard.

Rows should show:

```text
key  provider  title/summary  location  tokens  updated
```

Example:

```text
1  codex   soft block rollback   ~/repos/b2   184k   12m ago
2  claude  Address issue #63      ~/repos/b3    71k   2h ago
```

The location column shows the session cwd. Long paths should be shortened from the left so the meaningful tail remains visible. The row should include enough of the physical path to distinguish multiple workspaces from the same logical repo.

When a path is under the user's home directory, display `~` instead of the absolute home path.

### Quick Launch Keys

The first visible rows should have numeric launch keys.

Pressing `ctrl-1` launches the first visible row, `ctrl-2` launches the second visible row, and so on. This is intended for fast launching after filtering without making plain digits unusable in search fields.

The key column should update as filtering changes.

For the first version, quick-launch keys only need to cover rows `ctrl-1` through `ctrl-9`.

## Workspace Display

The app should support both Git worktrees and Jujutsu (`jj`) workspaces.

Most users will use Git worktrees, so Git worktree detection should be part of the default workspace labeling. `jj` should be auto-detected when a session cwd is inside a `jj` repo.

When a session's cwd is inside a `jj` repository, the UI should display:

- The logical repo identity when discoverable.
- The `jj` workspace name when discoverable.
- The physical cwd.

When a session's cwd is inside a Git worktree, the UI should display:

- The logical repo identity when discoverable.
- The Git worktree name or directory name when useful.
- The physical cwd.

Sessions from different `jj` workspaces or Git worktrees must remain separate launch targets, even if they belong to the same repository.

Example:

```text
beginnings:b2        ~/repos/b2
beginnings:b3        ~/repos/b3
beginnings:default   ~/repos/beginnings
```

## Preview Panel

The selected session preview should show:

- Provider.
- Session ID.
- Title or summary.
- cwd.
- Created and updated times.
- Session token count, when available.
- The tail of the message history by default, with the final visible turn highlighted.
- The matching chat turn with before/after context when message search is active, with matched characters highlighted when possible.
- A short snippet of matched title text, when title search is active.

The preview is for recognition, not full reading.

## Launch Behavior

Pressing `enter` launches the selected session.

Pressing a visible row shortcut launches that row directly.

The tool must change to the session's recorded working directory before launching the underlying agent CLI.

For Codex sessions:

```sh
cd <cwd> && codex resume <session-id>
```

For Claude Code sessions:

```sh
cd <cwd> && claude --resume <session-id>
```

The session ID is the canonical launch target because it is unambiguous.

## Missing Directory Behavior

If the recorded cwd no longer exists, pressing `enter` must not launch the session.

Instead, the UI should show an error state explaining:

- The session was found.
- The recorded cwd is missing.
- The missing path.

The user can then choose another session or quit. Directory override workflows are out of scope for the first version.

## Included Sessions

By default, include user-facing resumable sessions from:

- Claude Code.
- Codex.

The tool should avoid showing obvious internal/meta-only records when they can be detected reliably.

Subagent or sidechain sessions may be shown if they contain direct user-facing context and are launchable, but the default list should prioritize sessions a user would reasonably want to resume.

## Local Data Sources

The product reads local session data from the standard Claude Code and Codex locations.

Codex data observed locally:

- Metadata in `~/.codex/state_5.sqlite`, especially the `threads` table.
- Transcripts under `~/.codex/sessions/.../*.jsonl`.

Claude Code data observed locally:

- Session JSONL files under `~/.claude/projects/.../*.jsonl`.
- Optional per-project `sessions-index.json` files with summaries and metadata.

The app should treat these local records as read-only.

## Dependency Decision

The tool should not require the external `fzf` binary.

The tool should not depend on Helix editor UI code.

Use a small Rust terminal UI and keep the hot-path fuzzy scoring in-process:

```text
ratatui        terminal UI rendering
crossterm      terminal input/backend
```

The picker uses a lightweight linear fuzzy scorer over capped title/message search text. This keeps typing responsive across thousands of sessions and avoids pulling heavy picker/editor UI dependencies into the interactive loop.

Avoid `nucleo-ui`; it appears unmaintained and is not an official Helix library.

`nucleo-matcher` and `nucleo-picker` may be useful references, but the desired UI has three coordinated inputs and needs predictable low-latency filtering, so the product owns its TUI and search hot path.

## Keyboard Summary

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

Typing edits whichever search input is focused.

## Success Criteria

- A user can find a session without remembering the directory.
- A user can distinguish sessions from different `jj` workspaces in the same repo.
- A user can distinguish sessions from different Git worktrees in the same repo.
- A user can search by remembered title-like text.
- A user can search by remembered chat-message text.
- Launching resumes the selected Claude Code or Codex session from the correct cwd.
- A user can quick-launch a visible row with `ctrl-1` through `ctrl-9`.
- The binary works on Linux and macOS without requiring `fzf`.
