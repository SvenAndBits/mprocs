use std::time::{Duration, Instant};

use tokio::sync::mpsc::{
  UnboundedReceiver, UnboundedSender, unbounded_channel,
};
use tokio::task::JoinHandle;

use crate::config::health::{HealthCheckDef, Vars, substitute_vars};
use crate::kernel::kernel_message::{KernelCommand, SharedVt, TaskContext};
use crate::kernel::task::TaskId;
use crate::task::child_vt::vt_process_safe;
use crate::task::hooks::{EnvOverrides, shell_command, spawn_pipe, stdio};

#[derive(Debug)]
pub enum HealthEvent {
  Pass(usize),
  Fail(usize),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum AggregateOutcome {
  BecameHealthy,
  BecameUnhealthy,
  Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckState {
  Starting,
  Passing,
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
  WaitingFirstHealthy,
  Healthy,
  Unhealthy,
}

pub struct HealthRunner {
  rx: UnboundedReceiver<HealthEvent>,
  per_check: Vec<PerCheck>,
  overall: OverallState,
  child_handles: Vec<JoinHandle<()>>,
}

impl HealthRunner {
  pub fn spawn(
    checks: &[HealthCheckDef],
    vars: &Vars,
    cwd: Option<&std::ffi::OsString>,
    env: &EnvOverrides,
    out_vts: &[Option<SharedVt>],
    check_task_ids: &[Option<TaskId>],
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
      let task_id = check_task_ids.get(idx).copied().flatten();
      let ks_clone = ks.clone();
      let env_clone = env.clone();
      let handle = tokio::spawn(async move {
        run_check_loop(
          idx, cmd, cwd, env_clone, interval, timeout, start_period,
          tx_clone, out_vt, task_id, ks_clone,
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

  pub async fn next(&mut self) -> AggregateOutcome {
    loop {
      let event = match self.rx.recv().await {
        Some(e) => e,
        None => {
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
        if pc.consecutive_passes < pc.min_passes {
          return AggregateOutcome::Noop;
        }
        pc.state = CheckState::Passing;
        if self.per_check.iter().all(|p| p.state == CheckState::Passing) {
          match self.overall {
            OverallState::WaitingFirstHealthy | OverallState::Unhealthy => {
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
        if pc.consecutive_fails < pc.retries {
          return AggregateOutcome::Noop;
        }
        pc.state = CheckState::Failing;
        match self.overall {
          OverallState::Healthy | OverallState::WaitingFirstHealthy => {
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
  env: EnvOverrides,
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
  ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
  ticker.tick().await;

  loop {
    ticker.tick().await;
    let in_start_period = started.elapsed() < start_period;

    write_banner(out_vt.as_ref(), &cmd);
    if let Some(id) = task_id {
      ks.send_for_task(id, KernelCommand::TaskStarted);
    }
    let result =
      run_check_once(&cmd, cwd.as_ref(), &env, timeout, out_vt.as_ref()).await;
    write_result(out_vt.as_ref(), &result, in_start_period);
    if let Some(id) = task_id {
      ks.send_for_task(id, KernelCommand::TaskStopped(result_exit(&result)));
    }
    let event = match &result {
      Ok(true) => HealthEvent::Pass(idx),
      _ => {
        if in_start_period {
          continue;
        }
        HealthEvent::Fail(idx)
      }
    };
    if tx.send(event).is_err() {
      return;
    }
  }
}

pub async fn run_check_once_manual(
  cmd: String,
  cwd: Option<std::ffi::OsString>,
  env: EnvOverrides,
  timeout: Duration,
  out_vt: Option<SharedVt>,
  task_id: Option<TaskId>,
  ks: TaskContext,
) {
  write_banner(out_vt.as_ref(), &cmd);
  if let Some(id) = task_id {
    ks.send_for_task(id, KernelCommand::TaskStarted);
  }
  let result =
    run_check_once(&cmd, cwd.as_ref(), &env, timeout, out_vt.as_ref()).await;
  write_result(out_vt.as_ref(), &result, false);
  if let Some(id) = task_id {
    ks.send_for_task(id, KernelCommand::TaskStopped(result_exit(&result)));
  }
}

fn result_exit(result: &CheckResult) -> u32 {
  match result {
    Ok(true) => 0,
    Ok(false) => 1,
    Err(_) => 254,
  }
}

fn write_banner(out_vt: Option<&SharedVt>, cmd: &str) {
  if let Some(vt) = out_vt {
    let line = format!(
      "\r\n\x1b[2m── {} ──\x1b[0m\r\n\x1b[1m$\x1b[0m {}\r\n",
      compact_time(),
      cmd
    );
    vt_process_safe(vt, line.as_bytes());
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
      (Err(why), true) => {
        format!("\x1b[33m! error (suppressed: start_period): {}\x1b[0m", why)
      }
      (Err(why), false) => format!("\x1b[31m! error: {}\x1b[0m", why),
    };
    vt_process_safe(vt, format!("{}\r\n", line).as_bytes());
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

type CheckResult = Result<bool, String>;

async fn run_check_once(
  cmd: &str,
  cwd: Option<&std::ffi::OsString>,
  env: &EnvOverrides,
  timeout: Duration,
  out_vt: Option<&SharedVt>,
) -> CheckResult {
  let mut command = shell_command(cmd);
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
  command.stdout(stdio(capture));
  command.stderr(stdio(capture));

  let mut child = match command.spawn() {
    Ok(c) => c,
    Err(e) => {
      return Err(format!("spawn failed: {} (kind: {:?})", e, e.kind()));
    }
  };
  if let Some(vt) = out_vt {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    spawn_pipe(stdout, vt.clone());
    spawn_pipe(stderr, vt.clone());
  }

  match tokio::time::timeout(timeout, child.wait()).await {
    Ok(Ok(s)) => Ok(s.success()),
    Ok(Err(e)) => Err(format!("wait failed: {} (kind: {:?})", e, e.kind())),
    Err(_) => Err(format!("timed out after {:?}", timeout)),
  }
}
