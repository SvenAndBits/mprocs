# dekit

_dekit_ is a process supervisor for local development. It runs the commands
your project needs — `webpack serve`, `jest --watch`, `node src/server.js`,
databases, containers — and gives you one place to start, watch, and interact
with all of them.

dekit is the successor name to [mprocs](https://github.com/pvolok/mprocs): a
per-directory background daemon supervises your processes and the terminal UI
is a client you can attach and detach at will. This fork builds on that base
and adds a **process-orchestration layer** — the pieces you reach for once a
dev stack grows past "a few commands in parallel":

- **Health checks.** Gate a process as _Running_ only once a command reports it
  is actually ready (Docker-style `interval` / `timeout` / `retries`).
- **Dependencies.** Start a process only after the ones it depends on are up.
- **One-shot tasks.** Migrations, seeders, builds — run once, then release
  whatever was waiting on them.
- **Lifecycle hooks.** Run a command when a process starts, becomes healthy,
  goes unhealthy, stops, or fails.
- **A dependency-graph dashboard.** See how your processes connect and what is
  gating what.
- Smaller conveniences: `env_file` (dotenv loading), a configurable
  `restart_delay`, and per-process `%VAR%` substitution.

The classic mprocs CLI remains available via `dekit mprocs …`.

<!--ts-->

- [Concepts](#concepts)
- [Installation](#installation)
- [Quick start](#quick-start)
- [The daemon and the TUI](#the-daemon-and-the-tui)
- [CLI](#cli)
- [Configuration](#configuration)
  - [Processes](#processes)
  - [Dependencies](#dependencies)
  - [Health checks](#health-checks)
  - [One-shot tasks](#one-shot-tasks)
  - [Lifecycle hooks](#lifecycle-hooks)
  - [Variables](#variables)
  - [Environment files](#environment-files)
  - [Reusable registries and defaults](#reusable-registries-and-defaults)
  - [Full config reference](#full-config-reference)
- [Dashboard](#dashboard)
- [Default keymap](#default-keymap)
- [Running the legacy mprocs CLI](#running-the-legacy-mprocs-cli)
- [Credits](#credits)

<!--te-->

## Concepts

A **process** (or _task_) is one command dekit supervises. Every process moves
through a lifecycle:

- **Starting** — spawned, but not yet considered ready. If it has health
  checks, it stays here until they pass. If it is a one-shot, it stays here
  while it runs.
- **Running** — up and (if configured) healthy. Dependents are now allowed to
  start.
- **Completed** — a one-shot that exited 0. Dependents are released.
- **Stopped / Exited / Failed** — no longer running (cleanly stopped, exited on
  its own, or failed a check / exited non-zero).

The orchestration features this fork adds — dependencies, health checks, and
one-shots — all work by controlling when a process is allowed to advance to
_Running_ / _Completed_, and therefore when the processes waiting on it may
start.

## Installation

This fork is distributed as source only — there is no npm, Homebrew, or other
prebuilt package. Build it from source with a recent Rust toolchain:

```sh
git clone https://github.com/SvenAndBits/mprocs.git
cd mprocs
cargo install --path src
```

This installs the `dekit` binary. (The crate lives under `src/`.)

## Quick start

Create a `dekit.yaml` in your project:

```yaml
procs:
  server:
    shell: "node src/server.js"
    autostart: true
  tests:
    shell: "jest -w"
    env:
      NODE_ENV: test
  webpack: "webpack serve"
```

Then, from that directory:

```sh
dekit          # start the daemon (if needed) and open the TUI
```

Switch between processes with `j`/`k`, start/stop them, and interact with their
output — you can even run `vim` inside a process pane.

## The daemon and the TUI

dekit separates supervision from the UI:

- The **daemon** owns the processes. There is one daemon per working directory.
- The **TUI** is a client that attaches to the daemon. Closing it does not stop
  your processes by default — reattach any time with `dekit attach`.

Control what happens when the last client disconnects with `on_client_exit` in
`dekit.yaml`:

```yaml
on_client_exit: detach   # default: leave the daemon and procs running
# on_client_exit: stop_all  # stop everything and shut the daemon down
```

Typical lifecycle:

```sh
dekit up        # start the daemon and all autostart procs (no TUI)
dekit attach    # open the TUI against the running daemon
dekit down      # stop all procs and shut the daemon down
```

## CLI

Every command talks to the daemon for the current directory (override with
`-C/--chdir`). Add `--json` to any read command for machine-readable output —
useful for scripts and coding agents.

Process control:

```sh
dekit ls [glob]              # list tasks (add --json)
dekit start <path>           # start a task
dekit stop <path>            # stop a task
dekit kill <path>            # force-kill a task
dekit restart <path>         # restart a task
dekit inspect <path>         # status + deps + health checks
dekit screen <path>          # dump a task's current terminal screen
dekit spawn --path <path> -- <cmd...>   # add and start an ad-hoc task
```

Daemon management:

```sh
dekit server start    # start the daemon for this directory
dekit server stop     # stop it
dekit server status   # show status (add --json)
dekit server list     # list daemons on this machine
dekit server clean    # remove stale lock files
dekit server run --dir <dir>   # run a daemon in the foreground
```

`-c/--config <name>` selects the config file to load (default: `dekit.yaml`).

## Configuration

dekit loads `dekit.yaml` from the current directory. The full JSON/YAML schema
is at
[`schemas/dekit.json`](https://raw.githubusercontent.com/SvenAndBits/mprocs/master/schemas/dekit.json).

### Processes

```yaml
procs:
  web:
    shell: "node server.js"     # shell command (or use `cmd: [...]` for argv)
    cwd: <CONFIG_DIR>/app       # working dir; <CONFIG_DIR> = config's directory
    env:
      NODE_ENV: development     # null clears an inherited variable
    add_path: ["./node_modules/.bin"]
    autostart: true             # start when dekit starts (default: false)
    autorestart: true           # restart on exit (default: false)
    restart_delay: 1000         # ms to wait before an autorestart (default: 1000)
    stop: SIGINT                # how to stop; see schema for all forms
```

`stop` accepts `SIGINT` / `SIGTERM` / `SIGKILL` / `hard-kill`, a
`{ send-keys: [...] }` form, or a `{ cmd: "docker compose down" }` form that
runs a command instead of signaling the process.

### Dependencies

`deps` lists processes that must be _Running_ (or _Completed_, for one-shots)
before this one starts:

```yaml
procs:
  db:
    shell: "docker compose up postgres"
  api:
    shell: "node api.js"
    deps: [db]        # api won't start until db is up
```

### Health checks

A process with health checks stays _Starting_ until a check passes, so its
dependents wait for it to be genuinely ready — not just spawned:

```yaml
procs:
  db:
    shell: "docker compose up postgres"
    healthchecks:
      - cmd: "pg_isready -h localhost -p 5432"
        interval: 2s        # time between checks (default: 10s)
        timeout: 5s         # per-check timeout (default: 5s)
        start_period: 10s   # grace period where failures don't count (default: 0s)
        retries: 3          # consecutive failures before unhealthy (default: 3)
        min_passes: 1       # consecutive passes before healthy (default: 1)
```

Durations accept `ms`, `s`, `m`, `h` (a bare number means seconds).

### One-shot tasks

A one-shot runs to completion, then releases its dependents. It becomes
_Completed_ on exit 0, or _Exited_ on failure (which keeps dependents blocked).
One-shots and health checks are mutually exclusive.

```yaml
procs:
  migrate:
    shell: "npm run db:migrate"
    oneshot: true
    deps: [db]
  api:
    shell: "node api.js"
    deps: [migrate]   # waits for the migration to finish successfully
```

### Lifecycle hooks

Run a command in response to a process transition. Set `async: true` to fire
and forget without blocking the transition:

```yaml
procs:
  api:
    shell: "node api.js"
    hooks:
      running:
        cmd: "curl -fsS http://localhost:3000/warm || true"
        async: true
      failed:
        cmd: "notify-send 'api failed'"
```

Available events: `started`, `running`, `unhealthy`, `stopped`, `failed`.

### Variables

`vars` defines per-process values substituted as `%NAME%` in `cmd` / `shell` /
`env` / `cwd` and in health-check and hook commands:

```yaml
procs:
  db:
    shell: "docker compose up postgres"
    vars:
      HOST: localhost
      PORT: 5432
    healthchecks:
      - cmd: "nc -z %HOST% %PORT%"
```

### Environment files

Load variables from one or more dotenv files. Files are applied in order (later
files override earlier ones), and inline `env` overrides file values. Values
support the same `%VAR%` substitution as `env` and `vars`.

```yaml
procs:
  web:
    shell: "node server.js"
    env_file:
      - <CONFIG_DIR>/.env
      - <CONFIG_DIR>/.env.local
```

### Reusable registries and defaults

Define named health checks and hooks once, then reference them by name. Use
`proc_defaults` to merge shared settings under every process:

```yaml
healthchecks:
  http-ok:
    cmd: "curl -fsS http://localhost:%PORT%/health"
    interval: 5s

hooks:
  slack-alert:
    cmd: "./scripts/slack.sh"
    async: true

proc_defaults:
  autorestart: true

procs:
  api:
    shell: "node api.js"
    vars: { PORT: 3000 }
    healthchecks: [http-ok]
    hooks:
      failed: slack-alert
```

### Full config reference

Top-level keys include `procs`, `proc_defaults`, `healthchecks`, `hooks`,
`log`, `tui`, `keymap`, `on_init`, `on_all_finished`, and `on_client_exit`.
See [`schemas/dekit.json`](schemas/dekit.json) for every field and its
defaults.

## Dashboard

The process panel includes a **dependency-graph dashboard** that renders your
processes as a DAG — showing how `deps`, health checks, and one-shots connect
and what is currently gating what.

## Default keymap

Process list focused:

- `q` — Quit (soft kill processes, then exit)
- `Q` — Force quit (terminate processes)
- `p` — All commands
- `C-a` — Focus output pane
- `x` — Soft kill selected process
- `X` — Hard kill selected process
- `s` — Start selected process
- `r` — Soft kill and restart
- `R` — Hard kill and restart
- `a` — Add new process
- `C` — Duplicate selected process
- `d` — Remove selected process (must be stopped first)
- `e` — Rename selected process
- `Tab` or `Space` — Expand/collapse a process's health-check and hook children
- `k` / `↑` — Select previous process
- `j` / `↓` — Select next process
- `M-1`–`M-8` — Select process 1–8
- `C-d` / `page down` — Scroll output down
- `C-u` / `page up` — Scroll output up
- `C-e` — Scroll output down by 3 lines
- `C-y` — Scroll output up by 3 lines
- `z` — Zoom into terminal window
- `v` — Enter copy mode
- `?` — Toggle the keymap window

Process output focused:

- `C-a` — Focus processes pane

Copy mode:

- `v` — Start selecting end point
- `c` — Copy selected text
- `Esc` — Leave copy mode
- `C-a` — Focus processes pane
- `k`/`l`/`j`/`h` or arrows — Move cursor

Key bindings can be overridden per scope under `keymap` in `dekit.yaml`. See
`schemas/dekit.json` for the available actions.

## Running the legacy mprocs CLI

The original mprocs behavior (the `mprocs.yaml` format, `--ctl`, `--npm`,
`--procfile`, the TCP remote-control server, etc.) is still available:

```sh
dekit mprocs "yarn test -w" "webpack serve"
dekit mprocs --npm
dekit mprocs --ctl '{c: quit}'
```

The legacy config schema is at
[`schemas/mprocs.json`](schemas/mprocs.json).

## Credits

dekit is built on [mprocs](https://github.com/pvolok/mprocs) by Pavel Volokitin
and contributors — including the daemon/client architecture, the terminal UI,
copy mode, the JS scripting layer, and the `dekit` rename itself. Huge thanks
to that project; this fork would not exist without it.

This fork's own contribution is the process-orchestration layer on top: health
checks, dependencies, lifecycle hooks, one-shot tasks, the dependency-graph
dashboard, and conveniences like `env_file` and `restart_delay`.
