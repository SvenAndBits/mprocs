# Evaluation: a non-interactive CLI for agentic coding

Status: **evaluation / design — no implementation yet.**

Goal: let an agentic coding tool drive mprocs processes — start, stop, restart,
kill, inspect, read output — without a TUI, over scriptable commands with
machine-readable output.

## TL;DR

The big surprise: **the daemon + client/server architecture already exists
upstream.** The recent refactor split mprocs into a kernel that owns all task
state and a thin client that talks to it over a per-directory socket. A
non-interactive RPC client (`dk`) is already wired up and supports
`spawn / ls / start / stop / kill / restart / screen`.

So this is **not** a "potentially very big change" and **not** a rewrite. The
work is to *extend an existing, deliberately-designed RPC surface*, mostly
additively. Concretely, four gaps stand between today's `dk` and a CLI an agent
can rely on:

1. **No machine-readable output** — everything is `path\ttab` text.
2. **No logs / scrollback** — you can fetch the current *screen* snapshot, not a
   process's output history.
3. **No per-task `inspect`** — task info is just `{ path, status: String }`.
4. **No blocking semantics** — no "start and wait until running", no "wait for
   exit and give me the exit code".

This document evaluates the architecture, names the gaps precisely against the
current code, and proposes a command surface (with better names than the
`ci processes …` sketch).

## What already exists (current upstream)

Reference points in the tree:

- `src/daemon/lockfile.rs` — per-directory daemon. Lock + socket live at
  `$XDG_RUNTIME_DIR/<hash(working_dir)>.{lock,sock}`, guarded by an exclusive
  `flock`. Stale-detection, `list`, `clean` across the machine.
- `src/daemon/socket.rs` — `connect_client_socket(dir, spawn_server)`. With
  `spawn_server = true` it **auto-spawns the daemon on demand** and connects.
- `src/daemon/spawn.rs` — daemonizes and execs `dk server run --dir <dir>`.
- `src/dekit/server.rs` — `run_server` builds a `Kernel`, registers the
  configured procs as tasks, binds the socket, and dispatches each connection as
  **either** one-shot RPC **or** a streaming TUI session based on the first
  message.
- `src/protocol/rpc.rs` — `DkRequest` / `DkResponse` / `DkTaskInfo`.
- `src/protocol/attach.rs` — `CltToSrv` / `SrvToClt` (the TUI stream).
- `src/dekit/rpc_client.rs` — `rpc_request(dir, req, spawn_server)`: connect,
  send one `DkRequest`, await one `DkResponse`.
- `src/dekit/main.rs` — the `dk` CLI: `attach, up, down, spawn, ls, start, stop,
  kill, restart, screen, server {run,start,stop,status,list,clean}`.

Two facts that resolve the original design questions:

- **"Maybe some kind of daemon?"** — yes, and it's already built. The lockfile
  flow the sketch describes (`mprocs ci start` → lock file → detach) is exactly
  `connect_client_socket(dir, spawn_server = true)`: first client transparently
  forks the daemon, writes the lock, and connects. No explicit "start the
  server" step is required for the common case.
- **"Normal mprocs is essentially also client-server now."** — correct, and it
  already is. The interactive TUI is just a client that sends
  `CltToSrv::Init { width, height }` and renders the server's screen diffs. RPC
  and TUI share one socket and one dispatch path
  (`server.rs::dispatch_connection`). The kernel is the single source of truth;
  both clients are observers/controllers. **We don't have to build this.**

### Prior art to retire, not extend

`src/mprocs/ctl.rs` (`run_ctl`, the legacy `mprocs --ctl`) is the *old*
non-interactive mechanism: serialize an `AppEvent` as YAML over a TCP socket
that must be hand-configured in `mprocs.yaml` (`server:`). It is one-way (no
response), insecure-by-default (TCP), and event-shaped rather than
command-shaped. The new `dk` RPC path supersedes it; the agentic CLI should
build on RPC, not `--ctl`.

## Gap analysis (what an agent actually needs)

| Need | Today | Gap |
|---|---|---|
| Parse output reliably | `println!("{path}\t{status}")`, raw screen dump | **No JSON.** Add `--json` to every command. |
| Read a process's logs | `DkRequest::Screen` → current rendered screen only (`KernelQuery::GetScreen`) | No scrollback, no `--tail N`, no `--since`, no follow. |
| Inspect one task | `DkTaskInfo { path, status: String }` | No pid, exit code (typed), cmd, cwd, uptime, deps, health. |
| Wait for a state | fire-and-forget | No "start --wait" (block until running) or "wait --exit" (block, return code). |
| Trustworthy restart | `Stop` then `Start`, no barrier (`server.rs:198`) | Racy — `Start` can land before the process has stopped. |
| Rich status | RPC maps to `running / not-started / exited:N` strings | No `starting` / `unhealthy`; loses the health/deps states (see the deferred feature PR). |
| Lifecycle of ad-hoc tasks | `spawn` creates; no inverse | No `rm` / remove task, no `rename`. |

None of these require touching the kernel's ownership model. They are new
`DkRequest` variants + new `KernelQuery` variants + output formatting.

## Proposed command surface

### Naming: drop the `ci processes …` grouping

The sketch (`mprocs ci processes list`, `mprocs ci processes inspect :id`) has
two problems:

- **`ci` is overloaded** — it reads as "continuous integration". The intent is
  "control a running session non-interactively". A namespace that means that is
  clearer, or no namespace at all.
- **`processes` as a sub-group is redundant.** Processes *are* the only nouns in
  mprocs. `mprocs ci processes list` is three words for `ps`. Agents (and the
  tools that wrap them) do better with short, `docker`/`kubectl`-shaped verbs.

Recommendation: **flat, familiar verbs**, addressing procs by their config name
/ task path. Keep them on the existing `dk` client (it already *is* the
non-interactive client); mirror them under `mprocs` if a single binary is
preferred.

| Sketch | Recommended | Notes |
|---|---|---|
| `mprocs ci start` | `mprocs up` *(exists as `dk up`)* | Ensure daemon + autostart procs. Daemon auto-spawns; explicit `daemon start` stays available for scripting. |
| — (stop session) | `mprocs down` *(exists)* | Stop the daemon for this dir. |
| `mprocs ci processes list` | `mprocs ps` *(today: `ls`)* | `ps` is the universal "what's running" verb. `--json` for agents. |
| `mprocs ci processes inspect :id` | `mprocs inspect <name>` | Full typed detail for one proc. `--json`. |
| — | `mprocs logs <name>` | Output history. `--tail N`, `--since <dur>`, `--follow`. |
| — | `mprocs start/stop/restart/kill <name>` *(exist)* | Add `--wait` to `start`/`restart`. |
| — | `mprocs wait <name>` | Block until exit (`--exit`, returns code) or until running (`--running`). |
| — | `mprocs run <name> [--wait]` | One-shot: spawn (ad-hoc or config), optionally block to completion, return exit code. |

Addressing model: keep the existing **task path** (`/services/web`) as the
canonical id, accept the bare config name as shorthand. Avoid opaque numeric
`:id` — agents key off the human name they wrote in the config.

Daemon/session subcommands already exist and are well-named; keep them under a
`daemon` (or current `server`) group: `status`, `list`, `clean`.

### Output contract for agents

- `--json` on every read command. Stable, documented schema. Default stays
  human-readable so interactive use isn't punished.
- Exit codes that mean something: `0` success; non-zero for "task not found",
  "daemon not running", "command failed". Agents branch on these.
- `wait --exit` / `run --wait` propagate the child's exit code as the CLI's exit
  code — the single most useful primitive for "run a build, react to the
  result".

## Implementation sketch (additive, low-risk)

Each item is a new protocol variant + handler; no change to kernel ownership.

1. **JSON output** — formatting-only in `dk/main.rs`; add `--json`. No protocol
   change. *(smallest, do first)*
2. **Richer `DkTaskInfo` + `inspect`** — extend `DkTaskInfo` (pid, typed
   `exit_code`, cmd, cwd, deps, `started_at`); add `DkRequest::Inspect { path }`
   backed by a new `KernelQuery::GetTask`.
3. **Logs** — `DkRequest::Logs { path, tail, since }` backed by a new
   `KernelQuery::GetScrollback` reading the task's existing terminal buffer
   (the `SharedVt` already holds scrollback). Follow/stream reuses the
   `SrvToClt` streaming path.
4. **Blocking semantics** — `DkRequest::Wait { path, condition }` resolved by
   subscribing to task status updates (the kernel already emits `TaskNotify`
   status changes); `start --wait` / `restart --wait` compose on top.
5. **Fix `restart`** — make it a kernel-side stop-then-start that waits for the
   stop to complete, instead of two unsequenced sends.

Ordering: (1) unblocks agents immediately on existing data; (2)–(4) add the
data agents are missing; (5) is a correctness fix worth doing regardless.

## Interaction with the deferred health-checks / deps feature

The health-checks/hooks/deps feature (squash-merged, then deferred during the
upstream sync — to be re-implemented on the new architecture) introduces
`Starting` / `Unhealthy` task states and richer per-proc structure (checks,
hooks, deps as child rows). The agentic CLI should expose those:

- `wait --running` becomes meaningfully different from `wait --started` once
  health checks exist (running = passed its checks).
- `inspect` should surface health state, last check result, and dep readiness.

Recommendation: land the JSON/`inspect`/`logs`/`wait` surface against today's
state model first, then widen the status enum when the health feature is
re-applied. The two efforts are compatible and shouldn't block each other.

## Open questions

- **One binary or two?** Today `dk` is the non-interactive client and `mprocs`
  is legacy. Decide whether the agentic verbs live under `mprocs <verb>`, stay
  under `dk <verb>`, or both (alias). Recommendation: pick `dk` as the real
  client surface and make `mprocs` a thin alias, to avoid drift.
- **Log retention** — scrollback is bounded by the VT buffer. Is that enough for
  agent use, or do we need on-disk logs with `--since`? (Config already has a
  `log` section; `--since` likely wants file-backed logs.)
- **Auth / multi-user** — sockets are per-`$XDG_RUNTIME_DIR` (per-user) today.
  Fine for local agentic use; revisit only if remote control is ever in scope.
