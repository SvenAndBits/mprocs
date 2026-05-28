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

use tokio::process::Command;
use tokio::sync::mpsc::{
  UnboundedReceiver, UnboundedSender, unbounded_channel,
};
use tokio::task::JoinHandle;

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
  retries: u32,
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
  pub fn spawn(
    checks: &[HealthCheckDef],
    vars: &HashMap<String, String>,
    cwd: Option<&std::ffi::OsString>,
  ) -> Self {
    let (tx, rx) = unbounded_channel::<HealthEvent>();
    let mut per_check = Vec::with_capacity(checks.len());
    let mut child_handles = Vec::with_capacity(checks.len());

    for (idx, def) in checks.iter().enumerate() {
      per_check.push(PerCheck {
        state: CheckState::Starting,
        consecutive_fails: 0,
        retries: def.retries.max(1),
      });
      let cmd = substitute_vars(&def.cmd, vars);
      let cwd = cwd.cloned();
      let interval = def.interval;
      let timeout = def.timeout;
      let start_period = def.start_period;
      let tx_clone = tx.clone();
      let handle = tokio::spawn(async move {
        run_check_loop(
          idx,
          cmd,
          cwd,
          interval,
          timeout,
          start_period,
          tx_clone,
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
        let was = pc.state;
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
        let _ = was;
        AggregateOutcome::Noop
      }
      HealthEvent::Fail(idx) => {
        let pc = match self.per_check.get_mut(idx) {
          Some(p) => p,
          None => return AggregateOutcome::Noop,
        };
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

async fn run_check_loop(
  idx: usize,
  cmd: String,
  cwd: Option<std::ffi::OsString>,
  interval: Duration,
  timeout: Duration,
  start_period: Duration,
  tx: UnboundedSender<HealthEvent>,
) {
  let started = Instant::now();
  let mut ticker = tokio::time::interval(interval);
  // Skip the immediate tick fired by `interval` — wait one period before
  // the first check (gives the process a chance to come up).
  ticker.tick().await;

  loop {
    ticker.tick().await;
    let in_start_period = started.elapsed() < start_period;

    let result = run_check_once(&cmd, cwd.as_ref(), timeout).await;
    let event = match result {
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

async fn run_check_once(
  cmd: &str,
  cwd: Option<&std::ffi::OsString>,
  timeout: Duration,
) -> Result<bool, ()> {
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
  command.stdin(std::process::Stdio::null());
  command.stdout(std::process::Stdio::null());
  command.stderr(std::process::Stdio::null());

  let fut = command.status();
  let status = match tokio::time::timeout(timeout, fut).await {
    Ok(Ok(s)) => s,
    Ok(Err(_)) => return Err(()),
    Err(_) => return Err(()), // timed out
  };
  Ok(status.success())
}
