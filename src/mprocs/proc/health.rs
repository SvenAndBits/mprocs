//! Per-process health check supervisor.
//!
//! Spawns one tokio task per check, each running its command on its own
//! interval. Results are aggregated with AND semantics: the proc is "healthy"
//! only when every check has passed at least once since spawn AND no check
//! is currently in a failed-past-retries state.
//!
//! See `proc_health` for the config types.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc::{
  UnboundedReceiver, UnboundedSender, unbounded_channel,
};
use tokio::task::JoinHandle;

use crate::kernel::kernel_message::{KernelCommand, SharedVt, TaskContext};
use crate::kernel::task::TaskId;
use crate::mprocs::proc_health::{HealthCheckDef, substitute_vars};

#[derive(Debug)]
pub enum HealthEvent {
  Pass(usize),
  Fail(usize),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AggregateOutcome {
  /// First time all checks have passed since the supervisor started.
  /// Also fires when recovering from Unhealthy.
  BecameHealthy,
  /// A check exceeded its retry threshold after its start_period.
  BecameUnhealthy,
  /// No state change worth reporting.
  Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckState {
  /// Waiting in start_period; failures don't count yet.
  Starting,
  /// Has passed at least once and is currently considered passing.
  Passing,
  /// Has failed past its retry threshold (after start_period).
  Failing,
}

struct PerCheck {
  state: CheckState,
  consecutive_fails: u32,
  consecutive_passes: u32,
  retries: u32,
  min_passes: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverallState {
  /// At least one check has not yet passed for the first time.
  WaitingFirstHealthy,
  /// All checks passing.
  Healthy,
  /// Was healthy at some point but is now degraded.
  Unhealthy,
}

pub struct HealthRunner {
  rx: UnboundedReceiver<HealthEvent>,
  per_check: Vec<PerCheck>,
  overall: OverallState,
  // Child tasks: held to abort on drop.
  child_handles: Vec<JoinHandle<()>>,
}

impl HealthRunner {
  /// Spawn one tokio task per check. The runner's `next` future yields
  /// aggregate state transitions for the proc to react to.
  /// `check_task_ids` are the kernel TaskIds for the registered check
  /// child tasks (parallel to `checks`). Each per-check tokio task emits
  /// TaskStarted before its command and TaskStopped(exit_code) after,
  /// driving the UI's per-check status pill.
  ///
  /// `env` is the proc's full set of env overrides — applied to each
  /// check-command spawn so checks see the same env the running proc
  /// sees (mprocs's inherited env + any per-proc `env:` mods).
  #[allow(clippy::too_many_arguments)]
  pub fn spawn(
    checks: &[HealthCheckDef],
    vars: &HashMap<String, String>,
    cwd: Option<&std::ffi::OsString>,
    env: &super::hooks::EnvOverrides,
    out_vts: &[Option<SharedVt>],
    check_task_ids: &[TaskId],
    ks: &TaskContext,
  ) -> Self {
    let (tx, rx) = unbounded_channel::<HealthEvent>();
    let mut per_check = Vec::with_capacity(checks.len());
    let mut child_handles = Vec::with_capacity(checks.len());

    for (idx, def) in checks.iter().enumerate() {
      per_check.push(PerCheck {
        state: CheckState::Starting,
        consecutive_fails: 0,
        consecutive_passes: 0,
        retries: def.retries.max(1),
        min_passes: def.min_passes.max(1),
      });
      let cmd = substitute_vars(&def.cmd, vars);
      let cwd = cwd.cloned();
      let interval = def.interval;
      let timeout = def.timeout;
      let start_period = def.start_period;
      let tx_clone = tx.clone();
      let out_vt = out_vts.get(idx).cloned().flatten();
      let task_id = check_task_ids.get(idx).copied();
      let ks_clone = ks.clone();
      let env_clone = env.clone();
      let handle = tokio::spawn(async move {
        run_check_loop(
          idx,
          cmd,
          cwd,
          env_clone,
          interval,
          timeout,
          start_period,
          tx_clone,
          out_vt,
          task_id,
          ks_clone,
        )
        .await;
      });
      child_handles.push(handle);
    }

    HealthRunner {
      rx,
      per_check,
      overall: OverallState::WaitingFirstHealthy,
      child_handles,
    }
  }

  /// Await the next aggregate-state transition.
  pub async fn next(&mut self) -> AggregateOutcome {
    loop {
      let event = match self.rx.recv().await {
        Some(e) => e,
        None => {
          // All senders dropped; sleep forever so we don't busy-loop.
          std::future::pending::<()>().await;
          unreachable!();
        }
      };
      let outcome = self.apply(event);
      if outcome != AggregateOutcome::Noop {
        return outcome;
      }
    }
  }

  fn apply(&mut self, event: HealthEvent) -> AggregateOutcome {
    match event {
      HealthEvent::Pass(idx) => {
        let pc = match self.per_check.get_mut(idx) {
          Some(p) => p,
          None => return AggregateOutcome::Noop,
        };
        pc.consecutive_fails = 0;
        pc.consecutive_passes = pc.consecutive_passes.saturating_add(1);
        // A check is "passing" only once it has racked up `min_passes`
        // successes in a row. Default is 1 so the typical case is
        // unchanged.
        if pc.consecutive_passes < pc.min_passes {
          return AggregateOutcome::Noop;
        }
        pc.state = CheckState::Passing;
        // Maybe overall is now healthy?
        if self.per_check.iter().all(|p| p.state == CheckState::Passing) {
          match self.overall {
            OverallState::WaitingFirstHealthy
            | OverallState::Unhealthy => {
              self.overall = OverallState::Healthy;
              return AggregateOutcome::BecameHealthy;
            }
            OverallState::Healthy => return AggregateOutcome::Noop,
          }
        }
        AggregateOutcome::Noop
      }
      HealthEvent::Fail(idx) => {
        let pc = match self.per_check.get_mut(idx) {
          Some(p) => p,
          None => return AggregateOutcome::Noop,
        };
        pc.consecutive_passes = 0;
        pc.consecutive_fails = pc.consecutive_fails.saturating_add(1);
        // During the start_period the check loop sends Pass only on
        // success — failures within the start_period are suppressed there.
        // So any Fail we see here counts toward retries.
        if pc.consecutive_fails < pc.retries {
          return AggregateOutcome::Noop;
        }
        pc.state = CheckState::Failing;
        match self.overall {
          OverallState::Healthy => {
            self.overall = OverallState::Unhealthy;
            AggregateOutcome::BecameUnhealthy
          }
          OverallState::WaitingFirstHealthy => {
            // Never went healthy; transition to Unhealthy too (proc is up
            // but cannot become healthy).
            self.overall = OverallState::Unhealthy;
            AggregateOutcome::BecameUnhealthy
          }
          OverallState::Unhealthy => AggregateOutcome::Noop,
        }
      }
    }
  }
}

impl Drop for HealthRunner {
  fn drop(&mut self) {
    for h in self.child_handles.drain(..) {
      h.abort();
    }
  }
}

#[allow(clippy::too_many_arguments)]
async fn run_check_loop(
  idx: usize,
  cmd: String,
  cwd: Option<std::ffi::OsString>,
  env: super::hooks::EnvOverrides,
  interval: Duration,
  timeout: Duration,
  start_period: Duration,
  tx: UnboundedSender<HealthEvent>,
  out_vt: Option<SharedVt>,
  task_id: Option<TaskId>,
  ks: TaskContext,
) {
  let started = Instant::now();
  let mut ticker = tokio::time::interval(interval);
  // Pace from the end of the last invocation, not from a fixed wall
  // clock. Default `Burst` semantics make `tokio::time::interval` fire
  // missed ticks back-to-back when a check ran longer than its
  // interval, which compounds spawn pressure across all checks and
  // can trigger EAGAIN on fork. `Delay` keeps invocations spaced.
  ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
  // Skip the immediate tick fired by `interval` — wait one period before
  // the first check (gives the process a chance to come up).
  ticker.tick().await;

  loop {
    ticker.tick().await;
    let in_start_period = started.elapsed() < start_period;

    write_banner(out_vt.as_ref(), &cmd);
    if let Some(id) = task_id {
      ks.send_for_task(id, KernelCommand::TaskStarted);
    }
    let result = run_check_once(&cmd, cwd.as_ref(), &env, timeout, out_vt.as_ref()).await;
    write_result(out_vt.as_ref(), &result, in_start_period);
    if let Some(id) = task_id {
      let exit = match &result {
        Ok(true) => 0,
        Ok(false) => 1,
        Err(_) => 254,
      };
      ks.send_for_task(id, KernelCommand::TaskStopped(exit));
    }
    let event = match &result {
      Ok(true) => HealthEvent::Pass(idx),
      _ => {
        if in_start_period {
          // Suppress failures during the start period.
          continue;
        }
        HealthEvent::Fail(idx)
      }
    };
    if tx.send(event).is_err() {
      // Receiver dropped — supervisor is gone.
      return;
    }
  }
}

/// One-shot run of a healthcheck command, intended for the "press r to
/// re-run" UI action. Mirrors what the supervisor loop does for a single
/// tick: writes a banner + result into `out_vt`, emits TaskStarted /
/// TaskStopped lifecycle events for `task_id`. Does NOT coordinate with
/// the supervisor — the next supervisor tick still fires independently.
pub async fn run_check_once_manual(
  cmd: String,
  cwd: Option<std::ffi::OsString>,
  env: super::hooks::EnvOverrides,
  timeout: Duration,
  out_vt: Option<SharedVt>,
  task_id: Option<TaskId>,
  ks: TaskContext,
) {
  write_banner(out_vt.as_ref(), &cmd);
  if let Some(id) = task_id {
    ks.send_for_task(id, KernelCommand::TaskStarted);
  }
  let result = run_check_once(&cmd, cwd.as_ref(), &env, timeout, out_vt.as_ref()).await;
  write_result(out_vt.as_ref(), &result, false);
  if let Some(id) = task_id {
    let exit = match &result {
      Ok(true) => 0,
      Ok(false) => 1,
      Err(_) => 254,
    };
    ks.send_for_task(id, KernelCommand::TaskStopped(exit));
  }
}

fn write_banner(out_vt: Option<&SharedVt>, cmd: &str) {
  if let Some(vt) = out_vt {
    let line = format!(
      "\r\n\x1b[2m── {} ──\x1b[0m\r\n\x1b[1m$\x1b[0m {}\r\n",
      compact_time(),
      cmd
    );
    super::children::vt_process_safe(vt, line.as_bytes());
  }
}

fn write_result(
  out_vt: Option<&SharedVt>,
  result: &CheckResult,
  in_start_period: bool,
) {
  if let Some(vt) = out_vt {
    let line = match (result, in_start_period) {
      (Ok(true), _) => "\x1b[32m✓ pass\x1b[0m".to_string(),
      (Ok(false), true) => {
        "\x1b[33m✗ fail (suppressed: start_period)\x1b[0m".to_string()
      }
      (Ok(false), false) => "\x1b[31m✗ fail\x1b[0m".to_string(),
      (Err(why), true) => format!(
        "\x1b[33m! error (suppressed: start_period): {}\x1b[0m",
        why
      ),
      (Err(why), false) => {
        format!("\x1b[31m! error: {}\x1b[0m", why)
      }
    };
    super::children::vt_process_safe(vt, format!("{}\r\n", line).as_bytes());
  }
}

fn compact_time() -> String {
  use std::time::{SystemTime, UNIX_EPOCH};
  let secs = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
  let h = (secs / 3600) % 24;
  let m = (secs / 60) % 60;
  let s = secs % 60;
  format!("{:02}:{:02}:{:02} UTC", h, m, s)
}

/// Outcome of a single check invocation. `Ok(bool)` = command ran to
/// completion; bool is whether the exit code was 0. `Err(reason)` = we
/// couldn't even get an exit code (spawn failed, wait failed, or our
/// timeout fired). The reason flows up into the captured-output VT so
/// the user can see it in the tree.
type CheckResult = Result<bool, String>;

async fn run_check_once(
  cmd: &str,
  cwd: Option<&std::ffi::OsString>,
  env: &super::hooks::EnvOverrides,
  timeout: Duration,
  out_vt: Option<&SharedVt>,
) -> CheckResult {
  #[cfg(windows)]
  let mut command = {
    let mut c = Command::new("pwsh.exe");
    c.arg("-Command").arg(cmd);
    c
  };
  #[cfg(not(windows))]
  let mut command = {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
    c
  };
  if let Some(d) = cwd {
    command.current_dir(d);
  }
  for (k, v) in env {
    match v {
      Some(val) => {
        command.env(k, val);
      }
      None => {
        command.env_remove(k);
      }
    }
  }
  let capture = out_vt.is_some();
  command.stdin(std::process::Stdio::null());
  command.stdout(if capture {
    std::process::Stdio::piped()
  } else {
    std::process::Stdio::null()
  });
  command.stderr(if capture {
    std::process::Stdio::piped()
  } else {
    std::process::Stdio::null()
  });

  let mut child = match command.spawn() {
    Ok(c) => c,
    Err(e) => return Err(format!("spawn failed: {} (kind: {:?})", e, e.kind())),
  };
  if let Some(vt) = out_vt {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    spawn_pipe(stdout, vt.clone());
    spawn_pipe(stderr, vt.clone());
  }

  let fut = child.wait();
  match tokio::time::timeout(timeout, fut).await {
    Ok(Ok(s)) => Ok(s.success()),
    Ok(Err(e)) => Err(format!("wait failed: {} (kind: {:?})", e, e.kind())),
    Err(_) => Err(format!("timed out after {:?}", timeout)),
  }
}

fn spawn_pipe<R: AsyncReadExt + Unpin + Send + 'static>(
  reader: Option<R>,
  vt: SharedVt,
) {
  let mut reader = match reader {
    Some(r) => r,
    None => return,
  };
  tokio::spawn(async move {
    let mut buf = [0u8; 4096];
    loop {
      match reader.read(&mut buf).await {
        Ok(0) | Err(_) => break,
        Ok(n) => {
          // Convert bare LF to CRLF for the VT parser.
          let bytes = &buf[..n];
          let mut out = Vec::with_capacity(n + n / 8);
          let mut prev = 0u8;
          for &b in bytes {
            if b == b'\n' && prev != b'\r' {
              out.push(b'\r');
            }
            out.push(b);
            prev = b;
          }
          super::children::vt_process_safe(&vt, &out);
        }
      }
    }
  });
}
