# agz

A terminal dashboard for [oh-my-pi](https://github.com/can1357/oh-my-pi) (omp) coding-agent
sessions running in [zellij](https://zellij.dev).

agz lists every interactive omp instance and its subagents in a live tree, shows what each one is
doing, and lets you jump to its zellij pane, watch its screen, or resume a pinned session that is no
longer running.

```
› ⠹  agent-tui  main *2       TUI: agent-tui
  ●  stats2                   PUSH: List slow CEP push selections
    ├ Explore  task
    └ Review  task
  ○  auth  main               Create Biscuit Go Svelte app
```

## Features

- **Live tree of agents.** Each omp instance is a root row; its subagents nest below it. Rows
  appear, change and disappear as omp runs, with no refresh key.
- **Run state at a glance.** A spinner while the agent works, a green dot when it is idle, magenta
  when it waits for you (a tool approval or a question), red after an error.
- **Folder and git branch.** Each row shows the session's folder and branch, colored and counted
  like omp's own status line (`*` unstaged, `+` staged, `?` untracked).
- **Jump to a session.** `Enter` focuses the instance's zellij pane.
- **Pinned sessions.** Sessions pinned with omp's `/pin` or agz's `p` end with a grey pin and stay
  listed after omp exits (`○`); `Enter` resumes them in a new zellij tab.
- **Session copy.** `d` splits the screen and shows a live, read-only copy of the selected
  session's screen under the list, exactly as omp draws it.
- **Usage gauges.** A column right of the list shows your Anthropic and OpenAI subscription
  windows, like omp's `/usage`: percentage used and time to reset, colored by omp's status.

## How it works

omp exposes its run state only on its own terminal, so the repository holds two parts:

- **`extension/`**: the omp extension *agentz* (TypeScript, Bun). Loaded by every omp, it writes
  each agent's state to `~/.omp/run/agentz/<pid>/<agentId>.json` whenever it changes.
- **The `agz` binary** (Rust, [ratatui](https://ratatui.rs)). It watches those files, the omp
  processes, the git repositories and `~/.omp/agent/session-pins.json`, and redraws on change.
  It writes only `session-pins.json`, on `p`, under the same lock as omp. Usage comes from
  `omp usage --json`, which serves omp's own cache, so agz needs no credentials of its own.

Updates are event-driven (FSEvents or inotify, kqueue or pidfd), with three timed refreshes: the
git status every 10 s, like omp's status line; the session copy every 250 ms while it is open,
because zellij does not render a pane whose tab is hidden; and usage every 15 min, on top of the
refresh after each finished turn, to catch usage made outside omp.

[`SPEC.md`](SPEC.md) is the full specification, in French.

## Requirements

- macOS or Linux
- [zellij](https://zellij.dev) 0.45 or later; agz also runs outside zellij, without pane jumps,
  resume or session copy
- [omp](https://github.com/can1357/oh-my-pi) with interactive (`tui`) sessions
- [fish](https://fishshell.com), used to resume pinned sessions
- [mise](https://mise.jdx.dev), which provides Rust and Bun
- A [Nerd Font](https://www.nerdfonts.com) in the terminal, for the folder, branch and pin glyphs

## Install

```sh
mise run install
```

This installs `agz` into `~/.cargo/bin` and links the extension as
`~/.omp/agent/extensions/agentz`. To install only the extension, run
`mise run extension:install`. omp instances started before the link exist do not load the
extension and do not appear until they restart.

To check that omp loads the extension and that it publishes, run `mise run extension:check`. It
starts a throwaway omp (`--no-session`) in a pseudo-terminal, waits for its record, then exits it
and checks that the record is removed.

## Usage

Run `agz` in a zellij pane. It renames its tab to `agentz`.

| Key | Action |
|---|---|
| `↑` `↓` / `k` `j` | Move the cursor |
| `Enter` | Go to the instance's pane (a subagent goes to its root's pane); resume a pinned session |
| `Space` | Fold or unfold a root's subagents |
| `p` | Pin or unpin the row's session (unpinning a dormant session removes its row) |
| `d` | Open or close the session copy |
| `Esc` | Close the session copy |
| `q` / `Ctrl-C` | Quit |

## Development

```sh
mise run build   # release build and extension typecheck
mise run test    # Rust and extension tests
mise run start   # build, then run ./target/release/agz
```
