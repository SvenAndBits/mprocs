use std::fmt::Debug;
use std::future::pending;

use assert_matches::assert_matches;
use tokio::io::AsyncWriteExt;
use tokio::select;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::error::ResultLogger;
use crate::kernel::kernel_message::{KernelCommand, SharedVt, TaskContext};
use crate::kernel::task::{TaskCmd, TaskDef, TaskId, TaskStatus};
use crate::kernel::task_path::TaskPath;
use crate::kernel::task_screen::{TaskScreen, TaskScreenCmd, TaskScreenEffect};
use crate::mprocs::config::ProcConfig;
use crate::mprocs::proc_health::{HealthCheckDef, HookEvent, HookSet};
use crate::mprocs::proc_log_config::LogConfig;
use crate::process::process::Process as _;
use crate::process::process_spec::ProcessSpec;
use crate::term::encode::{encode_key, encode_mouse_event, KeyCodeEncodeModes};
use crate::term::grid::Rect;
use crate::term::key::Key;
use crate::term::mouse::{MouseEvent, MouseEventKind};
use crate::term::{MouseProtocolMode, Parser};

use super::children::{ChildKind, ChildStatus, ProcChild, new_child_vt};
use super::health::{AggregateOutcome, HealthRunner};
use super::hooks::run_hook;
use super::inst::Inst;
use super::msg::{ProcEvent, ProcMsg};
use super::view::ProcView;
use super::Size;
use super::StopSignal;

pub struct Proc {
  pub id: TaskId,
  pub spec: ProcessSpec,
  size: Size,

  name: String,
  stop_signal: StopSignal,
  log: Option<LogConfig>,

  pub vt: SharedVt,

  pub tx: UnboundedSender<ProcEvent>,

  pub inst: ProcState,

  /// Config fields needed for health checks and hooks (kept by-value so the
  /// proc can run them on any spawn, including restarts).
  vars: std::collections::HashMap<String, String>,
  cwd: Option<std::ffi::OsString>,
  /// Per-proc env overrides resolved from `cfg.env` + `add_path`, in the
  /// same shape used to spawn the proc's own subprocess (see
  /// `From<&ProcConfig> for ProcessSpec`). Threaded into every hook /
  /// healthcheck invocation so they see the same environment as the
  /// running proc.
  env_overrides: super::hooks::EnvOverrides,
  healthchecks: Vec<HealthCheckDef>,
  hooks: HookSet,
  /// Active health-check supervisor for the current instance. `None` when
  /// the proc has no healthchecks or no instance is running.
  health_runner: Option<HealthRunner>,
  /// Set to true once the proc has been reported as Running for the
  /// current instance (so we don't repeat the "Started" event on healthcheck
  /// flapping).
  reported_running: bool,
  /// Per-healthcheck VTs (output capture). Indexed parallel to
  /// `healthchecks`. The supervisor task pushes bytes here, the UI reads.
  check_vts: Vec<SharedVt>,
  /// Per-hook-event VTs. Created on demand for events that have a hook.
  hook_vts: std::collections::HashMap<HookEvent, SharedVt>,
  /// Kernel TaskIds for the registered hook child tasks. Used to emit
  /// per-hook lifecycle status (TaskStarted before the hook runs,
  /// TaskStopped(exit_code) after) so the UI sees status pills update.
  hook_task_ids: std::collections::HashMap<HookEvent, TaskId>,
  /// Kernel TaskIds for the registered healthcheck child tasks.
  check_task_ids: Vec<TaskId>,
  /// Captured kernel context, used to emit per-hook / per-check lifecycle
  /// events on behalf of the child tasks.
  ks: TaskContext,
}

#[derive(Debug)]
pub enum ProcState {
  None,
  Some(Inst),
}

pub fn launch_proc(
  parent_ks: &TaskContext,
  cfg: ProcConfig,
  task_id: TaskId,
  deps: Vec<TaskId>,
  path: Option<TaskPath>,
  size: Rect,
) -> ProcView {
  let vt = SharedVt::new(Parser::new(size.height, size.width, cfg.scrollback_len));

  // Pre-create per-child VTs in the launcher so the ProcView and the proc
  // task share the same Arc — both UI rendering (via ProcView.children)
  // and output capture (via Proc.{check_vts,hook_vts}) point at one VT.
  let check_vts: Vec<SharedVt> =
    cfg.healthchecks.iter().map(|_| new_child_vt()).collect();
  let mut hook_vts: std::collections::HashMap<HookEvent, SharedVt> =
    std::collections::HashMap::new();
  for event in [
    HookEvent::Started,
    HookEvent::Running,
    HookEvent::Unhealthy,
    HookEvent::Stopped,
    HookEvent::Failed,
  ] {
    if cfg.hooks.get(event).is_some() {
      hook_vts.insert(event, new_child_vt());
    }
  }

  // Register each hook / each check as a kernel child task so the UI and
  // tools like dekit can address them, and the proc task can report
  // lifecycle status via send_for_task.
  let mut children: Vec<ProcChild> = Vec::new();
  let mut hook_task_ids: std::collections::HashMap<HookEvent, TaskId> =
    std::collections::HashMap::new();
  let mut check_task_ids: Vec<TaskId> = Vec::with_capacity(cfg.healthchecks.len());

  let parent_path_str = path.as_ref().map(|p| p.as_str().to_owned());

  for event in [
    HookEvent::Started,
    HookEvent::Running,
    HookEvent::Unhealthy,
    HookEvent::Stopped,
    HookEvent::Failed,
  ] {
    if let Some(vt) = hook_vts.get(&event).cloned() {
      let label = hook_event_label(event);
      let child_path = parent_path_str
        .as_ref()
        .and_then(|p| TaskPath::new(format!("{}/hook/{}", p, label)).ok());
      let child_id = register_child_task(parent_ks, child_path, vt.clone());
      hook_task_ids.insert(event, child_id);
      children.push(ProcChild {
        task_id: child_id,
        kind: ChildKind::Hook(event),
        name: label.to_string(),
        vt,
        status: ChildStatus::Idle,
      });
    }
  }
  for (idx, def) in cfg.healthchecks.iter().enumerate() {
    let label = healthcheck_display_name(def, idx);
    let child_path = parent_path_str
      .as_ref()
      .and_then(|p| TaskPath::new(format!("{}/check/{}", p, label)).ok());
    let vt = check_vts[idx].clone();
    let child_id = register_child_task(parent_ks, child_path, vt.clone());
    check_task_ids.push(child_id);
    children.push(ProcChild {
      task_id: child_id,
      kind: ChildKind::Check(idx),
      name: label,
      vt,
      status: ChildStatus::Idle,
    });
  }

  let cfg_ = cfg.clone();
  let task_vt = vt.clone();
  let check_vts_for_task = check_vts.clone();
  let hook_vts_for_task = hook_vts.clone();
  let hook_task_ids_for_task = hook_task_ids.clone();
  let check_task_ids_for_task = check_task_ids.clone();
  let child_id = parent_ks.spawn_async_with_id(
    task_id,
    TaskDef {
      stop_on_quit: true,
      deps,
      path,
      vt: Some(vt.clone()),
      ..Default::default()
    },
    move |ks, cmd_receiver| async move {
      let cfg = cfg_;
      let task_id = ks.task_id;
      proc_main_loop(
        ks,
        task_id,
        &cfg,
        size,
        task_vt,
        check_vts_for_task,
        hook_vts_for_task,
        hook_task_ids_for_task,
        check_task_ids_for_task,
        cmd_receiver,
      )
      .await;
    },
  );

  ProcView::new_with_children(child_id, cfg, vt, children)
}

/// Register a no-op kernel task to host a child's path + VT. The task
/// does nothing on TaskCmd::{Start,Stop} — it exists purely so the UI
/// (and ctl/dekit) can address it, and so the parent proc can emit
/// lifecycle status via `send_for_task`.
fn register_child_task(
  parent_ks: &TaskContext,
  path: Option<TaskPath>,
  vt: SharedVt,
) -> TaskId {
  let id = parent_ks.alloc_id();
  parent_ks.register_with_id(
    id,
    TaskDef {
      stop_on_quit: false,
      path,
      vt: Some(vt),
      ..Default::default()
    },
    Box::new(|_ctx| Box::new(NoopChildTask)),
  );
  id
}

struct NoopChildTask;

impl crate::kernel::task::Task for NoopChildTask {
  fn handle_cmd(
    &mut self,
    _cmd: TaskCmd,
    _fx: &mut crate::kernel::task::Effects,
  ) {
    // Hook/check child tasks don't accept commands. All state is driven
    // by the parent proc via `send_for_task`.
  }
}

fn hook_event_label(event: HookEvent) -> &'static str {
  match event {
    HookEvent::Started => "started",
    HookEvent::Running => "running",
    HookEvent::Unhealthy => "unhealthy",
    HookEvent::Stopped => "stopped",
    HookEvent::Failed => "failed",
  }
}

/// Snapshot the proc's resolved env overrides (cfg.env after add_path
/// merging) into the shape used by hook/check spawns, applying `%VAR%`
/// substitution the same way ProcessSpec does for the proc itself.
fn build_env_overrides(cfg: &ProcConfig) -> super::hooks::EnvOverrides {
  let mut out: super::hooks::EnvOverrides = Vec::new();
  if let Some(env) = &cfg.env {
    for (k, v) in env {
      let v = v.as_ref().map(|s| {
        crate::mprocs::proc_health::substitute_vars(s, &cfg.vars)
      });
      out.push((k.clone(), v));
    }
  }
  out
}

fn healthcheck_display_name(
  def: &HealthCheckDef,
  idx: usize,
) -> String {
  if def.name.is_empty() {
    format!("check[{}]", idx)
  } else {
    def.name.clone()
  }
}

#[allow(clippy::too_many_arguments)]
async fn proc_main_loop(
  ks: TaskContext,
  task_id: TaskId,
  cfg: &ProcConfig,
  size: Rect,
  vt: SharedVt,
  check_vts: Vec<SharedVt>,
  hook_vts: std::collections::HashMap<HookEvent, SharedVt>,
  hook_task_ids: std::collections::HashMap<HookEvent, TaskId>,
  check_task_ids: Vec<TaskId>,
  mut cmd_receiver: UnboundedReceiver<TaskCmd>,
) -> ProcView {
  let (internal_sender, mut internal_receiver) =
    tokio::sync::mpsc::unbounded_channel();
  let mut proc = Proc::new(
    task_id,
    cfg,
    vt.clone(),
    internal_sender,
    size,
    check_vts,
    hook_vts,
    hook_task_ids,
    check_task_ids,
    ks.clone(),
  )
  .await;

  let mut task_screen = TaskScreen::new(task_id, vt);
  let mut screen_effects: Vec<TaskScreenEffect> = Vec::new();

  loop {
    enum NextValue {
      Cmd(Option<TaskCmd>),
      Internal(Option<ProcEvent>),
      Read(std::io::Result<usize>),
      Health(AggregateOutcome),
    }
    let mut read_buf = [0u8; 8 * 1024];
    let value = {
      // Disjoint field borrows so `read` and `health` can coexist in the
      // same `select!`.
      let Proc {
        health_runner,
        inst,
        ..
      } = &mut proc;
      let read_fut = async {
        if let ProcState::Some(inst) = inst {
          if !inst.stdout_eof {
            return inst.process.read(&mut read_buf).await;
          }
        }
        std::future::pending().await
      };
      let health_fut = async {
        match health_runner {
          Some(r) => r.next().await,
          None => std::future::pending::<AggregateOutcome>().await,
        }
      };
      select! {
        cmd = cmd_receiver.recv() => NextValue::Cmd(cmd),
        event = internal_receiver.recv() => NextValue::Internal(event),
        count = read_fut => NextValue::Read(count),
        outcome = health_fut => NextValue::Health(outcome),
      }
    };
    match value {
      NextValue::Cmd(Some(cmd)) => {
        let mut rendered = false;
        match cmd {
          TaskCmd::Start => {
            proc.start().await;
            rendered = true;
          }
          TaskCmd::Stop => proc.stop().await,
          TaskCmd::Kill => proc.kill().await,
          TaskCmd::Msg(msg) => {
            let msg = match msg.downcast::<ProcMsg>() {
              Ok(proc_msg) => {
                proc.handle_msg(*proc_msg, &mut rendered).await;
                continue;
              }
              Err(msg) => msg,
            };
            let msg = match msg.downcast::<TaskScreenCmd>() {
              Ok(cmd) => {
                task_screen.handle_cmd(*cmd, &mut screen_effects);
                apply_screen_effects(&mut screen_effects, &mut proc).await;
                continue;
              }
              Err(msg) => msg,
            };
            let _ = msg;
            log::error!("Proc received unknown Msg");
          }
        }
        let _ = rendered;
      }
      NextValue::Cmd(None) => (),
      NextValue::Internal(Some(proc_event)) => match proc_event {
        ProcEvent::Exited(exit_code) => {
          // Tear down health checks for the dead instance.
          proc.health_runner = None;
          // Run the `stopped` hook if configured (best-effort).
          run_lifecycle_hook(&proc, HookEvent::Stopped, &ks).await;
          proc.handle_exited(exit_code);
          if !proc.is_up() {
            ks.send(KernelCommand::TaskStopped(exit_code));
          }
        }
        ProcEvent::Started => {
          // Run the `started` hook (blocking by default — failure flips
          // the proc to a failed state and skips the lifecycle transition).
          let hook_ok = run_lifecycle_hook(&proc, HookEvent::Started, &ks).await;
          if !hook_ok {
            // Treat hook failure as a process failure: kill and report.
            log::warn!("started hook failed; killing proc");
            run_lifecycle_hook(&proc, HookEvent::Failed, &ks).await;
            proc.kill().await;
            // The subsequent ProcEvent::Exited will report Stopped.
            continue;
          }
          if proc.has_healthchecks() {
            ks.send(KernelCommand::TaskStatusChanged(TaskStatus::Starting));
          } else {
            ks.send(KernelCommand::TaskStarted);
            proc.reported_running = true;
          }
        }
      },
      NextValue::Internal(None) => (),
      NextValue::Health(outcome) => match outcome {
        AggregateOutcome::BecameHealthy => {
          // Blocking `running` hook fires BEFORE the kernel promotes the
          // task to Running and cascades to dependents. If it fails, the
          // proc is marked Unhealthy and the cascade is suppressed —
          // dependents won't see a green light they can't trust.
          let hook_ok =
            run_lifecycle_hook(&proc, HookEvent::Running, &ks).await;
          if !hook_ok {
            log::warn!(
              "`running` hook failed for proc `{}`; staying Unhealthy",
              proc.name
            );
            run_lifecycle_hook(&proc, HookEvent::Failed, &ks).await;
            ks.send(KernelCommand::TaskStatusChanged(TaskStatus::Unhealthy));
            continue;
          }
          if !proc.reported_running {
            ks.send(KernelCommand::TaskStarted);
            proc.reported_running = true;
          } else {
            // Recovered from Unhealthy back to Running.
            ks.send(KernelCommand::TaskStatusChanged(TaskStatus::Running));
          }
        }
        AggregateOutcome::BecameUnhealthy => {
          ks.send(KernelCommand::TaskStatusChanged(TaskStatus::Unhealthy));
          run_lifecycle_hook(&proc, HookEvent::Unhealthy, &ks).await;
        }
        AggregateOutcome::Noop => {}
      },
      NextValue::Read(Ok(count)) => {
        let inst = match &mut proc.inst {
          ProcState::Some(inst) => inst,
          ProcState::None => {
            log::error!("Expected proc.inst to be Some after a read.");
            continue;
          }
        };
        if count == 0 {
          inst.stdout_eof = true;
          if !proc.is_up() {
            ks.send(KernelCommand::TaskStopped(
              proc.exit_code().unwrap_or(199),
            ));
          }
        } else {
          let bytes = &read_buf[..count];

          // Write to log file if configured
          if let Some(ref mut writer) = inst.log_writer {
            writer.write_all(bytes).await.log_ignore();
            writer.flush().await.log_ignore();
          }

          task_screen.process(bytes, &mut screen_effects);
          apply_screen_effects(&mut screen_effects, &mut proc).await;
        }
      }
      NextValue::Read(Err(e)) => {
        log::warn!("Process read() error: {}", e);
        match &mut proc.inst {
          ProcState::Some(inst) => {
            inst.stdout_eof = true;
            if !proc.is_up() {
              ks.send(KernelCommand::TaskStopped(
                proc.exit_code().unwrap_or(198),
              ));
            }
          }
          ProcState::None => {}
        };
      }
    }
  }
}

/// Run a lifecycle hook if one is configured for `event`. Returns `false`
/// only when a *blocking* hook failed (the caller can then treat that as a
/// proc failure). Returns `true` if no hook is configured, the hook ran
/// successfully, or the hook is async.
async fn run_lifecycle_hook(
  proc: &Proc,
  event: HookEvent,
  ks: &TaskContext,
) -> bool {
  let hook = match proc.hooks.get(event) {
    Some(h) => h,
    None => return true,
  };
  let async_ = hook.async_;
  let out_vt = proc.hook_vts.get(&event).cloned();
  let hook_id = proc.hook_task_ids.get(&event).copied();

  if let Some(id) = hook_id {
    ks.send_for_task(id, KernelCommand::TaskStarted);
  }
  let result = run_hook(
    hook,
    &proc.vars,
    proc.cwd.as_ref(),
    &proc.env_overrides,
    out_vt,
  )
  .await;
  let (ok, exit_code) = match &result {
    Ok(()) => (true, 0u32),
    Err(super::hooks::HookError::ExitCode(c)) => (false, (*c as u32) | 0),
    Err(super::hooks::HookError::IoError(_)) => (false, 254u32),
  };
  if let Some(id) = hook_id {
    // Async hooks don't have a real exit code from this call's POV — the
    // detached future hasn't finished yet — but we still want the pill to
    // flip from RUN to either ✓ or ✗ in the UI, so report 0 and trust
    // best-effort.
    ks.send_for_task(id, KernelCommand::TaskStopped(exit_code));
  }
  if ok {
    true
  } else {
    log::warn!(
      "hook `{:?}` for proc `{}` failed: {:?}",
      event,
      proc.name,
      result.err()
    );
    // Async hook failures are advisory; only blocking failures abort the
    // lifecycle transition.
    async_
  }
}

async fn apply_screen_effects(
  effects: &mut Vec<TaskScreenEffect>,
  proc: &mut Proc,
) {
  for fx in effects.drain(..) {
    match fx {
      TaskScreenEffect::Reply(s) => {
        if let ProcState::Some(inst) = &mut proc.inst {
          inst.process.write_all(s.as_bytes()).await.log_ignore();
        }
      }
      TaskScreenEffect::Resize(ws) => {
        proc.resize(Size {
          width: ws.x,
          height: ws.y,
        });
      }
    }
  }
}

impl Proc {
  #[allow(clippy::too_many_arguments)]
  pub async fn new(
    id: TaskId,
    cfg: &ProcConfig,
    vt: SharedVt,
    tx: UnboundedSender<ProcEvent>,
    area: Rect,
    check_vts: Vec<SharedVt>,
    hook_vts: std::collections::HashMap<HookEvent, SharedVt>,
    hook_task_ids: std::collections::HashMap<HookEvent, TaskId>,
    check_task_ids: Vec<TaskId>,
    ks: TaskContext,
  ) -> Self {
    let size = Size {
      width: area.width,
      height: area.height,
    };
    let mut proc = Proc {
      id,
      spec: cfg.into(),
      size,

      name: cfg.name.clone(),
      stop_signal: cfg.stop.clone(),
      log: cfg.log.clone(),

      vt,

      tx,

      inst: ProcState::None,

      vars: cfg.vars.clone(),
      cwd: cfg.cwd.clone(),
      env_overrides: build_env_overrides(cfg),
      healthchecks: cfg.healthchecks.clone(),
      hooks: cfg.hooks.clone(),
      health_runner: None,
      reported_running: false,
      check_vts,
      hook_vts,
      hook_task_ids,
      check_task_ids,
      ks,
    };

    if cfg.autostart {
      proc.spawn_new_inst().await;
    }

    proc
  }

  pub fn has_healthchecks(&self) -> bool {
    !self.healthchecks.is_empty()
  }

  pub fn take_next_health_outcome(
    &mut self,
  ) -> Option<&mut HealthRunner> {
    self.health_runner.as_mut()
  }

  /// Reset per-instance health state, called whenever a new process is
  /// spawned (initial start or restart).
  fn reset_health_state(&mut self) {
    let ks = self.ks.clone();
    self.reported_running = false;
    if self.has_healthchecks() {
      let out_vts: Vec<Option<SharedVt>> =
        self.check_vts.iter().cloned().map(Some).collect();
      self.health_runner = Some(HealthRunner::spawn(
        &self.healthchecks,
        &self.vars,
        self.cwd.as_ref(),
        &self.env_overrides,
        &out_vts,
        &self.check_task_ids,
        &ks,
      ));
    } else {
      self.health_runner = None;
    }
  }

  async fn spawn_new_inst(&mut self) {
    assert_matches!(self.inst, ProcState::None);

    if let Ok(mut vt) = self.vt.write() {
      vt.reset();
      vt.set_size(self.size.height, self.size.width);
    }

    let spawned = Inst::spawn(
      self.id,
      &self.name,
      &self.spec,
      self.tx.clone(),
      &self.size,
      self.log.as_ref(),
    )
    .await;
    let inst = match spawned {
      Ok(inst) => ProcState::Some(inst),
      Err(err) => {
        log::warn!("Process spawn error: {}", err);
        ProcState::None
      }
    };
    self.inst = inst;
    if matches!(self.inst, ProcState::Some(_)) {
      self.reset_health_state();
    } else {
      self.health_runner = None;
    }
  }

  pub async fn start(&mut self) {
    if !self.is_up() {
      self.inst = ProcState::None;
      self.spawn_new_inst().await;
    }
  }

  pub fn handle_exited(&mut self, exit_code: u32) {
    match &mut self.inst {
      ProcState::None => (),
      ProcState::Some(inst) => {
        inst.exit_code = Some(exit_code);
        inst.process.on_exited();
      }
    }
  }

  pub fn is_up(&self) -> bool {
    if let ProcState::Some(inst) = &self.inst {
      inst.exit_code.is_none() || !inst.stdout_eof
    } else {
      false
    }
  }

  pub fn exit_code(&self) -> Option<u32> {
    match &self.inst {
      ProcState::Some(inst) => inst.exit_code,
      ProcState::None => None,
    }
  }

  pub fn lock_vt(
    &self,
  ) -> Option<std::sync::RwLockReadGuard<'_, Parser>> {
    self.vt.read().ok()
  }

  pub fn lock_vt_mut(
    &mut self,
  ) -> Option<std::sync::RwLockWriteGuard<'_, Parser>> {
    self.vt.write().ok()
  }

  pub async fn kill(&mut self) {
    if self.is_up() {
      if let ProcState::Some(inst) = &mut self.inst {
        inst.process.kill().await.log_ignore();
      }
    }
  }

  #[cfg(not(windows))]
  pub async fn stop(&mut self) {
    match self.stop_signal.clone() {
      StopSignal::SIGINT => self.send_signal(libc::SIGINT),
      StopSignal::SIGTERM => self.send_signal(libc::SIGTERM),
      StopSignal::SIGKILL => self.send_signal(libc::SIGKILL),
      StopSignal::SendKeys(keys) => {
        for key in keys {
          self.send_key(&key).await;
        }
      }
      StopSignal::HardKill => self.kill().await,
      StopSignal::Cmd(shell) => self.run_stop_cmd(shell),
    }
  }

  #[cfg(windows)]
  pub async fn stop(&mut self) {
    match self.stop_signal.clone() {
      StopSignal::SIGINT => log::debug!("SIGINT signal is ignored on Windows"),
      StopSignal::SIGTERM => self.kill().await,
      StopSignal::SIGKILL => self.kill().await,
      StopSignal::SendKeys(keys) => {
        for key in keys {
          self.send_key(&key).await;
        }
      }
      StopSignal::HardKill => self.kill().await,
      StopSignal::Cmd(shell) => self.run_stop_cmd(shell),
    }
  }

  /// Spawn the configured stop command as a separate subprocess. Inherits
  /// the proc's cwd and env so commands like `podman compose down` target
  /// the same project. Output is discarded. The main proc is expected to
  /// exit on its own once the command takes effect.
  fn run_stop_cmd(&self, shell: String) {
    let cwd = self.spec.cwd.clone();
    let env = self.spec.env.clone();
    tokio::spawn(async move {
      #[cfg(windows)]
      let mut cmd = {
        let mut c = tokio::process::Command::new("pwsh.exe");
        c.arg("-Command").arg(&shell);
        c
      };
      #[cfg(not(windows))]
      let mut cmd = {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.arg("-c").arg(&shell);
        c
      };
      if let Some(cwd) = &cwd {
        cmd.current_dir(cwd);
      }
      for (k, v) in &env {
        match v {
          Some(v) => {
            cmd.env(k, v);
          }
          None => {
            cmd.env_remove(k);
          }
        }
      }
      cmd.stdout(std::process::Stdio::null());
      cmd.stderr(std::process::Stdio::null());
      if let Err(e) = cmd.status().await {
        log::warn!("Stop command failed: {}", e);
      }
    });
  }

  #[cfg(not(windows))]
  fn send_signal(&mut self, sig: libc::c_int) {
    if let ProcState::Some(inst) = &self.inst {
      unsafe { libc::kill(inst.pid as i32, sig) };
    }
  }

  pub fn resize(&mut self, size: Size) {
    if let Ok(mut vt) = self.vt.write() {
      vt.set_size(size.height, size.width);
    }
    if let ProcState::Some(inst) = &mut self.inst {
      inst.resize(&size);
    }
    self.size = size;
  }

  pub async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
    if let ProcState::Some(inst) = &mut self.inst {
      if !inst.stdout_eof {
        return inst.process.read(buf).await;
      }
    }
    pending().await
  }

  pub async fn send_key(&mut self, key: &Key) {
    if self.is_up() {
      let application_cursor_keys = self
        .lock_vt()
        .is_some_and(|vt| vt.screen().application_cursor());
      let encoder = encode_key(
        key,
        KeyCodeEncodeModes {
          enable_csi_u_key_encoding: true,
          application_cursor_keys,
          newline_mode: false,
        },
      );
      match encoder {
        Ok(encoder) => {
          self.write_all(encoder.as_bytes()).await;
        }
        Err(_) => {
          log::warn!("Failed to encode key: {}", key.to_string());
        }
      }
    }
  }

  pub async fn write_all(&mut self, bytes: &[u8]) {
    if self.is_up() {
      if let Some(mut vt) = self.lock_vt_mut() {
        if vt.screen().scrollback() > 0 {
          vt.set_scrollback(0);
        }
      }
      if let ProcState::Some(inst) = &mut self.inst {
        inst.process.write_all(bytes).await.log_ignore();
      }
    }
  }

  pub fn scroll_up_lines(&mut self, n: usize) {
    if let Some(mut vt) = self.lock_vt_mut() {
      vt.screen.scroll_screen_up(n);
    }
  }

  pub fn scroll_down_lines(&mut self, n: usize) {
    if let Some(mut vt) = self.lock_vt_mut() {
      vt.screen.scroll_screen_down(n);
    }
  }

  pub fn scroll_half_screen_up(&mut self) {
    self.scroll_up_lines(self.size.height as usize / 2);
  }

  pub fn scroll_half_screen_down(&mut self) {
    self.scroll_down_lines(self.size.height as usize / 2);
  }

  pub async fn handle_mouse(&mut self, event: MouseEvent) {
    if let ProcState::Some(inst) = &mut self.inst {
      let mouse_mode = self.vt.read().unwrap().screen().mouse_protocol_mode();
      let seq = match mouse_mode {
        MouseProtocolMode::None => String::new(),
        MouseProtocolMode::Press => match event.kind {
          MouseEventKind::Down(_)
          | MouseEventKind::ScrollDown
          | MouseEventKind::ScrollUp
          | MouseEventKind::ScrollLeft
          | MouseEventKind::ScrollRight => encode_mouse_event(event),
          _ => String::new(),
        },
        MouseProtocolMode::PressRelease => match event.kind {
          MouseEventKind::Down(_)
          | MouseEventKind::Up(_)
          | MouseEventKind::ScrollDown
          | MouseEventKind::ScrollUp
          | MouseEventKind::ScrollLeft
          | MouseEventKind::ScrollRight => encode_mouse_event(event),
          MouseEventKind::Drag(_) | MouseEventKind::Moved => String::new(),
        },
        MouseProtocolMode::ButtonMotion => match event.kind {
          MouseEventKind::Down(_)
          | MouseEventKind::Up(_)
          | MouseEventKind::ScrollDown
          | MouseEventKind::Drag(_)
          | MouseEventKind::ScrollUp
          | MouseEventKind::ScrollLeft
          | MouseEventKind::ScrollRight => encode_mouse_event(event),
          MouseEventKind::Moved => String::new(),
        },
        MouseProtocolMode::AnyMotion => encode_mouse_event(event),
      };
      let _r = inst.process.write_all(seq.as_bytes()).await;
    }
  }
}

impl Proc {
  pub async fn handle_msg(&mut self, msg: ProcMsg, rendered: &mut bool) {
    match msg {
      ProcMsg::SendKey(key) => self.send_key(&key).await,
      ProcMsg::SendMouse(event) => self.handle_mouse(event).await,

      ProcMsg::ScrollUp => {
        self.scroll_half_screen_up();
        *rendered = true;
      }
      ProcMsg::ScrollDown => {
        self.scroll_half_screen_down();
        *rendered = true;
      }
      ProcMsg::ScrollUpLines { n } => {
        self.scroll_up_lines(n);
        *rendered = true;
      }
      ProcMsg::ScrollDownLines { n } => {
        self.scroll_down_lines(n);
        *rendered = true;
      }
      ProcMsg::RerunHook(event) => {
        // Fire the hook on demand (blocking semantics same as during
        // lifecycle transitions). We don't act on the failed return: the
        // proc's overall lifecycle isn't being transitioned by this run,
        // it's a manual invocation.
        let ks = self.ks.clone();
        let _ok = run_lifecycle_hook(self, event, &ks).await;
      }
      ProcMsg::RerunCheck(idx) => {
        // Run the check command once, write to its VT, and emit status
        // for the corresponding child kernel task. Independent of the
        // running supervisor (which will continue to tick on its own).
        if let Some(def) = self.healthchecks.get(idx).cloned() {
          let out_vt = self.check_vts.get(idx).cloned();
          let task_id = self.check_task_ids.get(idx).copied();
          let cwd = self.cwd.clone();
          let cmd = crate::mprocs::proc_health::substitute_vars(
            &def.cmd,
            &self.vars,
          );
          let timeout = def.timeout;
          let ks = self.ks.clone();
          let env = self.env_overrides.clone();
          tokio::spawn(async move {
            crate::mprocs::proc::health::run_check_once_manual(
              cmd, cwd, env, timeout, out_vt, task_id, ks,
            )
            .await;
          });
        }
      }
    }
  }
}
