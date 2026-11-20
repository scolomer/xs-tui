# xs-tui

A terminal dashboard for [oh-my-pi](https://github.com/can1357/oh-my-pi) (omp) coding-agent
sessions running in [zellij](https://zellij.dev).

xs-tui (command `xst`) lists every interactive omp instance and its subagents in a live tree,
shows what each one is doing, and lets you jump to its zellij pane, watch its screen, or resume a
pinned session that is no longer running.

```
› ⠹  xs-tui  main *2          TUI: xs-tui
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
- **Pinned sessions.** Sessions pinned with omp's `/pin` or xst's `p` end with a grey pin and stay
  listed after omp exits (`○`); `Enter` resumes them in a new zellij tab.
- **Session copy.** `d` splits the screen and shows a live, read-only copy of the selected
  session's screen under the list, exactly as omp draws it.
- **Send a prompt.** `w` types a prompt in the bottom bar and sends it to the selected instance,
  as if typed in its editor: it starts a turn at rest and steers a running one. It reaches
  headless instances and instances outside xst's zellij session too.
- **Filter.** `f` opens a filter line under the list that narrows and sorts it by relevance as you
  type, like fzf: fuzzy, smart-case and accent-insensitive matching on each session's folder,
  branch, title and subagents, matched letters underlined.
- **Usage gauges.** A column right of the list shows your Anthropic and OpenAI subscription
  windows, like omp's `/usage`: percentage used and time to reset, colored by omp's status.
- **Capy threads.** With `CAPY_API_KEY` set, your unarchived [Capy](https://capy.ai) threads that
  are not settled (idle, read, nothing asked of you) join the list, mixed with omp instances by
  start date: a cyan cloud and the project segment mark them,
  where omp rows carry a blue `π`. State, project and title are refreshed from Capy's API (the
  list every 60 s, each thread every 10 s), and `d` shows the thread's conversation, followed live
  over its event stream only while it is on screen. `Enter` opens the thread in the Capy desktop
  app (`capy://app/thread/<id>`), and `w` sends it a prompt through the API, steering it like a
  message typed in Capy.
- **Session-manager xs-parley.** Inside zellij, xst joins every xs-parley parley
  of the machine as its session-manager (rank `M`). When a master asks a session that is offline,
  it sends xst a `spawn`: xst resumes that omp session headless, like `s`, and answers once the
  instance publishes, so the master can deliver its request. The bottom bar shows
  `manager : N parleys`, or the problem in red.

## How it works

omp exposes its run state only on its own terminal, so the repository holds two parts:

- **`extension/`**: the omp extension *xs-tui* (TypeScript, Bun). Loaded by every omp, it writes
  each agent's state to `~/.omp/run/xs-tui/<pid>/<agentId>.json` whenever it changes, and forwards
  the prompts dropped in `~/.omp/run/xs-tui/<pid>/inbox/` to the main session.
- **The `xst` binary** (Rust, [ratatui](https://ratatui.rs)). It watches those files, the omp
  processes, the git repositories and `~/.omp/agent/session-pins.json`, and redraws on change.
  It writes `session-pins.json`, on `p`, under the same lock as omp, and prompt files, on `w`,
  into an instance's inbox. Usage comes from `omp usage --json`, which serves omp's own cache, so
  xst needs no credentials of its own.

Updates are event-driven (FSEvents or inotify, kqueue or pidfd), with three timed refreshes: the
git status every 10 s, like omp's status line; the session copy every 250 ms while it is open,
because zellij does not render a pane whose tab is hidden; and usage every 15 min, on top of the
refresh after each finished turn, to catch usage made outside omp.

xst also embeds a native xs-parley client (`src/xsp/`: Ed25519, HPKE, XChaCha20-Poly1305 and a
rustls WebSocket per relay). Its identity lives in `~/.omp/xs-parley/manager.json` and
`keys/<fp>.key`; it watches `~/.omp/xs-parley/parleys/` to join and leave parleys, and logs every
`spawn` in `audit/<parley_id>.jsonl`, like the sessions do. The test vectors in
`src/xsp/vectors/` are a frozen copy of xs-parley's `doc/protocol/vectors`.

[`SPEC.md`](SPEC.md) is the full specification, in French.

## Requirements

- macOS or Linux
- [zellij](https://zellij.dev) 0.45 or later; xst also runs outside zellij, without pane jumps,
  resume or session copy
- [omp](https://github.com/can1357/oh-my-pi) with interactive (`tui`) sessions
- [fish](https://fishshell.com), used to resume pinned sessions
- [mise](https://mise.jdx.dev), which provides Rust and Bun
- A [Nerd Font](https://www.nerdfonts.com) in the terminal, for the folder, branch and pin glyphs

## Install

```sh
mise run install
```

This installs `xst` into `~/.cargo/bin` and links the extension as
`~/.omp/agent/extensions/xs-tui`. To install only the extension, run
`mise run extension:install`. omp instances started before the link exist do not load the
extension and do not appear until they restart.

To check that omp loads the extension and that it publishes, run `mise run extension:check`. It
starts a throwaway omp (`--no-session`) in a pseudo-terminal, waits for its record, then exits it
and checks that the record is removed.

## Usage

Run `xst` in a zellij pane. It renames its tab to `xs-tui`.

| Key | Action |
|---|---|
| `↑` `↓` / `k` `j` | Move the cursor |
| `Enter` | Go to the instance's pane (a subagent goes to its root's pane); resume a pinned session |
| `Space` | Fold or unfold a root's subagents |
| `p` | Pin or unpin the row's session (unpinning a dormant session removes its row) |
| `x` | Close the instance and its pane; press `x` again within 3 s to confirm |
| `d` | Open or close the session copy |
| `w` | Type a prompt for the instance (a subagent's goes to its root) or the Capy thread; `Enter` sends, `Alt-Enter` adds a line, `Esc` cancels |
| `f` | Filter the list; `Enter` keeps the filter, `Esc` clears it |
| `Esc` | Clear the filter, or close the session copy |
| `q` / `Ctrl-C` | Quit |

## Development

```sh
mise run build   # release build and extension typecheck
mise run test    # Rust and extension tests
mise run start   # build, then run ./target/release/xst
```
