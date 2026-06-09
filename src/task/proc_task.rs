use std::collections::HashMap;
use std::ffi::OsString;
use std::future::pending;

use tokio::sync::mpsc::UnboundedReceiver;

use crate::config::health::{HealthCheckDef, HookEvent, HookSet, Vars};
use crate::error::ResultLogger;
use crate::kernel::kernel_message::{KernelCommand, SharedVt, TaskContext};
use crate::kernel::task::{NoopTask, TaskCmd, TaskDef, TaskId, TaskStatus};
use crate::kernel::task_path::TaskPath;
use crate::kernel::task_screen::{TaskScreen, TaskScreenCmd, TaskScreenEffect};
use crate::process::NativeProcess;
use crate::process::process::Process as _;
use crate::process::process_spec::ProcessSpec;
use crate::task::child_vt::new_child_vt;
use crate::task::health::{HealthRunner, run_check_once_manual};
use crate::task::hooks::{EnvOverrides, run_hook};
use crate::task::logger::{LogResolver, spawn_logger};
use crate::term::encode::{KeyCodeEncodeModes, encode_key};
use crate::term::key::Key;
use crate::term::{Parser, Winsize};

const HOOK_EVENTS: [HookEvent; 5] = [
  HookEvent::Started,
  HookEvent::Running,
  HookEvent::Unhealthy,
  HookEvent::Stopped,
  HookEvent::Failed,
];

struct ProcExited(u32);

pub struct ProcInput(pub Key);

pub struct DuplicateProc(pub Option<String>);

pub enum ProcMsg {
  RerunHook(HookEvent),
  RerunCheck(usize),
}

/// How a proc task should react to `Stop` (`Kill` is always a hard kill).
#[derive(Clone, Debug, Default)]
pub enum StopSignal {
  SIGINT,
  #[default]
  SIGTERM,
  SIGKILL,
  SendKeys(Vec<Key>),
  HardKill,
  /// Run a shell command as the stop action. Useful for tools like
  /// `podman compose` that don't reliably respond to signals but do have
  /// an explicit teardown command (e.g. `podman compose down`). The main
  /// process is expected to exit on its own once the stop command
  /// completes (e.g. `compose up` exits when containers go away).
  Cmd(String),
}

pub struct ProcTaskConfig {
  pub spec: ProcessSpec,
  pub label: Option<String>,
  pub stop: StopSignal,
  pub log: Option<LogResolver>,
  pub autostart: bool,
  pub autorestart: bool,
  pub scrollback_len: usize,
  pub mouse_scroll_speed: usize,
  pub deps: Vec<TaskId>,
  pub vars: Vars,
  pub healthchecks: Vec<HealthCheckDef>,
  pub hooks: HookSet,
}

impl ProcTaskConfig {
  pub fn new(spec: ProcessSpec) -> Self {
    Self {
      spec,
      label: None,
      stop: StopSignal::default(),
      log: None,
      autostart: true,
      autorestart: false,
      scrollback_len: 1000,
      mouse_scroll_speed: 5,
      deps: Vec::new(),
      vars: Vars::new(),
      healthchecks: Vec::new(),
      hooks: HookSet::default(),
    }
  }
}

pub fn spawn_proc_task(
  parent: &TaskContext,
  task_path: Option<TaskPath>,
  config: ProcTaskConfig,
) -> TaskId {
  let task_id = parent.alloc_id();
  spawn_proc_task_with_id(parent, task_id, task_path, config);
  task_id
}

pub fn spawn_proc_task_with_id(
  parent: &TaskContext,
  task_id: TaskId,
  task_path: Option<TaskPath>,
  config: ProcTaskConfig,
) {
  let ProcTaskConfig {
    spec,
    stop,
    log,
    autostart,
    autorestart,
    scrollback_len,
    mouse_scroll_speed,
    deps,
    label,
    vars,
    healthchecks,
    hooks,
  } = config;
  let vt = SharedVt::new(Parser::new(24, 80, scrollback_len));
  let task_vt = vt.clone();

  let mut pending_children: Vec<PendingChild> = Vec::new();
  let mut check_vts: Vec<Option<SharedVt>> =
    Vec::with_capacity(healthchecks.len());
  let mut check_task_ids: Vec<Option<TaskId>> =
    Vec::with_capacity(healthchecks.len());
  for (i, def) in healthchecks.iter().enumerate() {
    let child_vt = new_child_vt();
    let id = plan_child(
      parent,
      task_path.as_ref(),
      &format!("check_{i}"),
      Some(def.name.clone()),
      child_vt.clone(),
      &mut pending_children,
    );
    check_vts.push(Some(child_vt));
    check_task_ids.push(id);
  }

  let mut hook_vts: HashMap<HookEvent, SharedVt> = HashMap::new();
  let mut hook_task_ids: HashMap<HookEvent, TaskId> = HashMap::new();
  for event in HOOK_EVENTS {
    if hooks.get(event).is_some() {
      let child_vt = new_child_vt();
      if let Some(id) = plan_child(
        parent,
        task_path.as_ref(),
        &format!("hook_{}", event.label()),
        None,
        child_vt.clone(),
        &mut pending_children,
      ) {
        hook_task_ids.insert(event, id);
      }
      hook_vts.insert(event, child_vt);
    }
  }

  let runtime = ProcRuntime {
    spec,
    log,
    stop,
    scrollback_len,
    mouse_scroll_speed,
    autorestart,
    vars,
    healthchecks,
    hooks,
    check_vts,
    check_task_ids,
    hook_vts,
    hook_task_ids,
  };

  parent.spawn_async_with_id(
    task_id,
    TaskDef {
      stop_on_quit: true,
      autostart,
      autorestart,
      deps,
      path: task_path,
      label,
      vt: Some(vt),
      ..Default::default()
    },
    move |ctx, receiver| async move {
      proc_main(ctx, receiver, task_vt, runtime).await;
    },
  );

  for child in pending_children {
    parent.register_with_id(
      child.id,
      TaskDef {
        stop_on_quit: false,
        path: Some(child.path),
        label: child.label,
        vt: Some(child.vt),
        ..Default::default()
      },
      Box::new(|_| Box::new(NoopTask)),
    );
  }
}

struct PendingChild {
  id: TaskId,
  path: TaskPath,
  label: Option<String>,
  vt: SharedVt,
}

struct ProcRuntime {
  spec: ProcessSpec,
  log: Option<LogResolver>,
  stop: StopSignal,
  scrollback_len: usize,
  mouse_scroll_speed: usize,
  autorestart: bool,
  vars: Vars,
  healthchecks: Vec<HealthCheckDef>,
  hooks: HookSet,
  check_vts: Vec<Option<SharedVt>>,
  check_task_ids: Vec<Option<TaskId>>,
  hook_vts: HashMap<HookEvent, SharedVt>,
  hook_task_ids: HashMap<HookEvent, TaskId>,
}

fn plan_child(
  parent: &TaskContext,
  parent_path: Option<&TaskPath>,
  seg: &str,
  label: Option<String>,
  vt: SharedVt,
  out: &mut Vec<PendingChild>,
) -> Option<TaskId> {
  let parent_path = parent_path?;
  let path =
    TaskPath::new(format!("{}/{}", parent_path.as_str(), seg)).ok()?;
  let id = parent.alloc_id();
  out.push(PendingChild {
    id,
    path,
    label,
    vt,
  });
  Some(id)
}

async fn proc_main(
  ctx: TaskContext,
  mut receiver: UnboundedReceiver<TaskCmd>,
  vt: SharedVt,
  runtime: ProcRuntime,
) {
  let ProcRuntime {
    spec,
    mut log,
    stop,
    scrollback_len,
    mouse_scroll_speed,
    autorestart,
    vars,
    healthchecks,
    hooks,
    check_vts,
    check_task_ids,
    hook_vts,
    hook_task_ids,
  } = runtime;

  let cwd: Option<OsString> = spec.cwd.clone().map(OsString::from);
  let env: EnvOverrides =
    spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

  let mut task_screen = TaskScreen::new(ctx.task_id, vt, mouse_scroll_speed);
  let mut screen_effects: Vec<TaskScreenEffect> = Vec::new();

  let mut process: Option<NativeProcess> = None;
  let mut current_log: Option<(std::path::PathBuf, u64)> = None;
  let mut read_buf = [0u8; 8 * 1024];
  let mut stdout_eof = false;
  let mut exit_code: Option<u32> = None;
  let mut health_runner: Option<HealthRunner> = None;
  let mut reported_running = false;

  loop {
    if stdout_eof
      && let Some(code) = exit_code
      && process.take().is_some()
    {
      ctx.send(KernelCommand::TaskStopped(code));
    }

    enum Next {
      Cmd(Option<TaskCmd>),
      Read(std::io::Result<usize>),
      Health(crate::task::health::AggregateOutcome),
    }
    let read_fut = async {
      match process.as_mut() {
        Some(p) if !stdout_eof => p.read(&mut read_buf).await,
        _ => pending().await,
      }
    };
    let health_fut = async {
      match health_runner.as_mut() {
        Some(r) => r.next().await,
        None => pending().await,
      }
    };
    let next = tokio::select! {
      cmd = receiver.recv() => Next::Cmd(cmd),
      n = read_fut => Next::Read(n),
      outcome = health_fut => Next::Health(outcome),
    };

    match next {
      Next::Cmd(None) => break,
      Next::Cmd(Some(cmd)) => match cmd {
        TaskCmd::Start => {
          if process.is_none() {
            process = start_instance(&ctx, &spec, task_screen.vt());
            if let Some(p) = &process {
              exit_code = None;
              stdout_eof = false;
              reported_running = false;
              update_log_observer(
                &mut task_screen,
                &mut log,
                &mut current_log,
                p.pid(),
              );
              let started_ok = run_lifecycle_hook(
                &ctx,
                &hooks,
                HookEvent::Started,
                &vars,
                cwd.as_ref(),
                &env,
                &hook_vts,
                &hook_task_ids,
              )
              .await;
              if !started_ok {
                log::warn!("started hook failed; killing proc");
                run_lifecycle_hook(
                  &ctx,
                  &hooks,
                  HookEvent::Failed,
                  &vars,
                  cwd.as_ref(),
                  &env,
                  &hook_vts,
                  &hook_task_ids,
                )
                .await;
                if let Some(p) = process.as_mut() {
                  p.kill().await.log_ignore();
                }
                continue;
              }
              if healthchecks.is_empty() {
                ctx.send(KernelCommand::TaskStarted);
                reported_running = true;
              } else {
                ctx.send(KernelCommand::TaskStatusChanged(
                  TaskStatus::Starting,
                ));
                health_runner = Some(HealthRunner::spawn(
                  &healthchecks,
                  &vars,
                  cwd.as_ref(),
                  &env,
                  &check_vts,
                  &check_task_ids,
                  &ctx,
                ));
              }
            }
          }
        }
        TaskCmd::Stop => {
          health_runner = None;
          if let Some(p) = process.as_mut() {
            stop_process(p, &stop, task_screen.vt(), &spec).await;
          }
        }
        TaskCmd::Kill => {
          health_runner = None;
          if let Some(p) = process.as_mut() {
            p.kill().await.log_ignore();
          }
        }
        TaskCmd::Msg(msg) => {
          let msg = match msg.downcast::<ProcExited>() {
            Ok(exited) => {
              exit_code = Some(exited.0);
              health_runner = None;
              reported_running = false;
              run_lifecycle_hook(
                &ctx,
                &hooks,
                HookEvent::Stopped,
                &vars,
                cwd.as_ref(),
                &env,
                &hook_vts,
                &hook_task_ids,
              )
              .await;
              if let Some(p) = process.as_mut() {
                p.on_exited();
              }
              continue;
            }
            Err(msg) => msg,
          };
          let msg = match msg.downcast::<TaskScreenCmd>() {
            Ok(cmd) => {
              task_screen.handle_cmd(*cmd, &mut screen_effects);
              apply_effects(
                &mut screen_effects,
                &mut process,
                task_screen.vt(),
              )
              .await;
              continue;
            }
            Err(msg) => msg,
          };
          let msg = match msg.downcast::<ProcInput>() {
            Ok(input) => {
              if let Some(p) = process.as_mut() {
                send_key(p, task_screen.vt(), input.0).await;
              }
              continue;
            }
            Err(msg) => msg,
          };
          let msg = match msg.downcast::<ProcMsg>() {
            Ok(proc_msg) => {
              handle_rerun(
                *proc_msg,
                &ctx,
                &hooks,
                &healthchecks,
                &vars,
                cwd.as_ref(),
                &env,
                &hook_vts,
                &hook_task_ids,
                &check_vts,
                &check_task_ids,
              );
              continue;
            }
            Err(msg) => msg,
          };
          let msg = match msg.downcast::<DuplicateProc>() {
            Ok(dup) => {
              let new_id = ctx.alloc_id();
              let path = TaskPath::new(format!("/{}", new_id.0)).ok();
              spawn_proc_task_with_id(
                &ctx,
                new_id,
                path,
                ProcTaskConfig {
                  spec: spec.clone(),
                  stop: stop.clone(),
                  log: None,
                  autostart: true,
                  autorestart,
                  scrollback_len,
                  mouse_scroll_speed,
                  deps: Vec::new(),
                  label: dup.0,
                  vars: Vars::new(),
                  healthchecks: Vec::new(),
                  hooks: HookSet::default(),
                },
              );
              continue;
            }
            Err(msg) => msg,
          };
          let _ = msg;
          log::error!("ProcTask received unknown Msg");
        }
      },

      Next::Health(outcome) => {
        use crate::task::health::AggregateOutcome;
        match outcome {
          AggregateOutcome::BecameHealthy => {
            let ok = run_lifecycle_hook(
              &ctx,
              &hooks,
              HookEvent::Running,
              &vars,
              cwd.as_ref(),
              &env,
              &hook_vts,
              &hook_task_ids,
            )
            .await;
            if !ok {
              run_lifecycle_hook(
                &ctx,
                &hooks,
                HookEvent::Failed,
                &vars,
                cwd.as_ref(),
                &env,
                &hook_vts,
                &hook_task_ids,
              )
              .await;
              ctx.send(KernelCommand::TaskStatusChanged(
                TaskStatus::Unhealthy,
              ));
              continue;
            }
            if reported_running {
              ctx.send(KernelCommand::TaskStatusChanged(TaskStatus::Running));
            } else {
              ctx.send(KernelCommand::TaskStarted);
              reported_running = true;
            }
          }
          AggregateOutcome::BecameUnhealthy => {
            ctx.send(KernelCommand::TaskStatusChanged(TaskStatus::Unhealthy));
            run_lifecycle_hook(
              &ctx,
              &hooks,
              HookEvent::Unhealthy,
              &vars,
              cwd.as_ref(),
              &env,
              &hook_vts,
              &hook_task_ids,
            )
            .await;
          }
          AggregateOutcome::Noop => {}
        }
      }

      Next::Read(Ok(0)) => stdout_eof = true,
      Next::Read(Ok(n)) => {
        task_screen
          .process(&read_buf[..n], &mut screen_effects)
          .await;
        apply_effects(&mut screen_effects, &mut process, task_screen.vt())
          .await;
      }
      Next::Read(Err(e)) => {
        log::warn!("Process read error: {}", e);
        stdout_eof = true;
      }
    }
  }
}

#[allow(clippy::too_many_arguments)]
async fn run_lifecycle_hook(
  ctx: &TaskContext,
  hooks: &HookSet,
  event: HookEvent,
  vars: &Vars,
  cwd: Option<&OsString>,
  env: &EnvOverrides,
  hook_vts: &HashMap<HookEvent, SharedVt>,
  hook_task_ids: &HashMap<HookEvent, TaskId>,
) -> bool {
  let Some(def) = hooks.get(event) else {
    return true;
  };
  let out_vt = hook_vts.get(&event).cloned();
  let task_id = hook_task_ids.get(&event).copied();
  if let Some(id) = task_id {
    ctx.send_for_task(id, KernelCommand::TaskStarted);
  }
  let res = run_hook(def, vars, cwd, env, out_vt).await;
  let (ok, code) = match &res {
    Ok(()) => (true, 0u32),
    Err(crate::task::hooks::HookError::ExitCode(c)) => (false, *c as u32),
    Err(crate::task::hooks::HookError::IoError(_)) => (false, 255),
  };
  if let Some(id) = task_id {
    ctx.send_for_task(id, KernelCommand::TaskStopped(code));
  }
  ok
}

#[allow(clippy::too_many_arguments)]
fn handle_rerun(
  msg: ProcMsg,
  ctx: &TaskContext,
  hooks: &HookSet,
  healthchecks: &[HealthCheckDef],
  vars: &Vars,
  cwd: Option<&OsString>,
  env: &EnvOverrides,
  hook_vts: &HashMap<HookEvent, SharedVt>,
  hook_task_ids: &HashMap<HookEvent, TaskId>,
  check_vts: &[Option<SharedVt>],
  check_task_ids: &[Option<TaskId>],
) {
  match msg {
    ProcMsg::RerunHook(event) => {
      let Some(def) = hooks.get(event) else {
        return;
      };
      let def = crate::config::health::HookDef {
        cmd: def.cmd.clone(),
        async_: false,
      };
      let vars = vars.clone();
      let cwd = cwd.cloned();
      let env = env.clone();
      let out_vt = hook_vts.get(&event).cloned();
      let task_id = hook_task_ids.get(&event).copied();
      let ks = ctx.clone();
      tokio::spawn(async move {
        if let Some(id) = task_id {
          ks.send_for_task(id, KernelCommand::TaskStarted);
        }
        let res = run_hook(&def, &vars, cwd.as_ref(), &env, out_vt).await;
        let code = match res {
          Ok(()) => 0u32,
          Err(crate::task::hooks::HookError::ExitCode(c)) => c as u32,
          Err(crate::task::hooks::HookError::IoError(_)) => 255,
        };
        if let Some(id) = task_id {
          ks.send_for_task(id, KernelCommand::TaskStopped(code));
        }
      });
    }
    ProcMsg::RerunCheck(idx) => {
      let Some(def) = healthchecks.get(idx) else {
        return;
      };
      let cmd = crate::config::health::substitute_vars(&def.cmd, vars);
      let timeout = def.timeout;
      let cwd = cwd.cloned();
      let env = env.clone();
      let out_vt = check_vts.get(idx).cloned().flatten();
      let task_id = check_task_ids.get(idx).copied().flatten();
      let ks = ctx.clone();
      tokio::spawn(async move {
        run_check_once_manual(cmd, cwd, env, timeout, out_vt, task_id, ks)
          .await;
      });
    }
  }
}

fn update_log_observer(
  task_screen: &mut TaskScreen,
  log: &mut Option<LogResolver>,
  current: &mut Option<(std::path::PathBuf, u64)>,
  pid: u32,
) {
  let Some(resolve) = log.as_mut() else {
    return;
  };
  let Some(sink) = resolve(pid) else {
    return;
  };
  if let Some((path, _)) = current {
    if *path == sink.path {
      return;
    }
  }
  if let Some((_, id)) = current.take() {
    task_screen.remove_direct_observer(id);
  }
  let path = sink.path.clone();
  let id = task_screen.add_direct_observer(spawn_logger(sink));
  *current = Some((path, id));
}

fn start_instance(
  ctx: &TaskContext,
  spec: &ProcessSpec,
  vt: &SharedVt,
) -> Option<NativeProcess> {
  let size = match vt.read() {
    Ok(parser) => {
      let s = parser.screen().size();
      Winsize {
        x: s.width,
        y: s.height,
        x_px: 0,
        y_px: 0,
      }
    }
    Err(_) => Winsize {
      x: 80,
      y: 24,
      x_px: 0,
      y_px: 0,
    },
  };
  if let Ok(mut parser) = vt.write() {
    parser.reset();
    parser.set_size(size.y, size.x);
  }
  match spawn_native(ctx, spec, size) {
    Ok(process) => Some(process),
    Err(err) => {
      log::warn!("Process spawn error: {}", err);
      ctx.send(KernelCommand::TaskStopped(255));
      None
    }
  }
}

async fn apply_effects(
  effects: &mut Vec<TaskScreenEffect>,
  process: &mut Option<NativeProcess>,
  vt: &SharedVt,
) {
  for effect in effects.drain(..) {
    match effect {
      TaskScreenEffect::Write(s) => {
        if let Some(p) = process.as_mut() {
          p.write_all(s.as_bytes()).await.log_ignore();
        }
      }
      TaskScreenEffect::Resize(size) => {
        if let Ok(mut parser) = vt.write() {
          parser.set_size(size.y, size.x);
        }
        if let Some(p) = process.as_mut() {
          p.resize(size).log_ignore();
        }
      }
    }
  }
}

async fn send_key(process: &mut NativeProcess, vt: &SharedVt, key: Key) {
  let application_cursor_keys = vt
    .read()
    .map(|parser| parser.screen().application_cursor())
    .unwrap_or(false);
  let modes = KeyCodeEncodeModes {
    enable_csi_u_key_encoding: true,
    application_cursor_keys,
    newline_mode: false,
  };
  match encode_key(&key, modes) {
    Ok(encoded) => process.write_all(encoded.as_bytes()).await.log_ignore(),
    Err(_) => log::warn!("Failed to encode key: {}", key.spec()),
  }
}

#[cfg(not(windows))]
async fn stop_process(
  process: &mut NativeProcess,
  stop: &StopSignal,
  vt: &SharedVt,
  spec: &ProcessSpec,
) {
  match stop {
    StopSignal::SIGINT => process.send_signal(libc::SIGINT).log_ignore(),
    StopSignal::SIGTERM => process.send_signal(libc::SIGTERM).log_ignore(),
    StopSignal::SIGKILL => process.send_signal(libc::SIGKILL).log_ignore(),
    StopSignal::SendKeys(keys) => {
      for key in keys {
        send_key(process, vt, key.clone()).await;
      }
    }
    StopSignal::HardKill => process.kill().await.log_ignore(),
    StopSignal::Cmd(shell) => run_stop_cmd(spec, shell.clone()),
  }
}

#[cfg(windows)]
async fn stop_process(
  process: &mut NativeProcess,
  stop: &StopSignal,
  vt: &SharedVt,
  spec: &ProcessSpec,
) {
  match stop {
    StopSignal::SIGINT => log::debug!("SIGINT signal is ignored on Windows"),
    StopSignal::SIGTERM | StopSignal::SIGKILL | StopSignal::HardKill => {
      process.kill().await.log_ignore()
    }
    StopSignal::SendKeys(keys) => {
      for key in keys {
        send_key(process, vt, key.clone()).await;
      }
    }
    StopSignal::Cmd(shell) => run_stop_cmd(spec, shell.clone()),
  }
}

fn run_stop_cmd(spec: &ProcessSpec, shell: String) {
  let cwd = spec.cwd.clone();
  let env = spec.env.clone();
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
#[cfg(test)]
mod tests {
  use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

  use crate::kernel::kernel::Kernel;
  use crate::kernel::kernel_message::{
    KernelCommand, KernelQuery, KernelQueryResponse, TaskContext,
  };
  use crate::kernel::task::TaskId;
  use crate::task::logger::LogSink;

  use super::*;

  async fn resolve(pc: &TaskContext, path: &str) -> TaskId {
    let (tx, rx) = tokio::sync::oneshot::channel();
    pc.send(KernelCommand::Query(
      KernelQuery::ResolvePath(TaskPath::new(path).unwrap()),
      tx,
    ));
    let resp = tokio::time::timeout(Duration::from_secs(1), rx)
      .await
      .expect("timed out resolving path")
      .expect("kernel query channel closed");
    match resp {
      KernelQueryResponse::ResolvedPath(Some(id)) => id,
      _ => panic!("path did not resolve: {path}"),
    }
  }

  #[tokio::test]
  async fn proc_output_is_logged_via_direct_observer() {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let mut log_path = std::env::temp_dir();
    log_path.push(format!("mprocs_log_{}_{}.log", std::process::id(), nanos));

    let kernel = Kernel::new();
    let pc = kernel.context();

    let path = TaskPath::new("/logged").unwrap();
    let spec = ProcessSpec::from_argv(vec![
      "sh".to_string(),
      "-c".to_string(),
      "printf hello-log".to_string(),
    ]);
    let sink_path = log_path.clone();
    spawn_proc_task(
      &pc,
      Some(path),
      ProcTaskConfig {
        log: Some(Box::new(move |_pid| {
          Some(LogSink {
            path: sink_path.clone(),
            append: false,
          })
        })),
        ..ProcTaskConfig::new(spec)
      },
    );

    let kernel_task = tokio::spawn(kernel.run());

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      if let Ok(contents) = std::fs::read_to_string(&log_path) {
        if contents.contains("hello-log") {
          break;
        }
      }
      assert!(Instant::now() < deadline, "log file never got output");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The SIGCHLD waiter isn't running in unit tests, so the proc never
    // transitions to Exited on its own; remove it explicitly to unblock quit.
    let id = resolve(&pc, "/logged").await;
    pc.send(KernelCommand::RemoveTask(id));
    pc.send(KernelCommand::Quit);
    tokio::time::timeout(Duration::from_secs(2), kernel_task)
      .await
      .expect("timed out waiting for kernel to quit")
      .unwrap();

    let _ = std::fs::remove_file(&log_path);
  }

  #[tokio::test]
  async fn log_path_is_resolved_with_real_pid() {
    use std::sync::{Arc, Mutex};

    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let mut dir = std::env::temp_dir();
    dir.push(format!("mprocs_pidlog_{}_{}", std::process::id(), nanos));
    std::fs::create_dir_all(&dir).unwrap();

    let kernel = Kernel::new();
    let pc = kernel.context();

    let spec = ProcessSpec::from_argv(vec![
      "sh".to_string(),
      "-c".to_string(),
      "printf hi".to_string(),
    ]);
    let seen_pid = Arc::new(Mutex::new(None::<u32>));
    let cap = seen_pid.clone();
    let log_dir = dir.clone();
    spawn_proc_task(
      &pc,
      Some(TaskPath::new("/pidlog").unwrap()),
      ProcTaskConfig {
        log: Some(Box::new(move |pid| {
          *cap.lock().unwrap() = Some(pid);
          Some(LogSink {
            path: log_dir.join(format!("{pid}.log")),
            append: false,
          })
        })),
        ..ProcTaskConfig::new(spec)
      },
    );

    let kernel_task = tokio::spawn(kernel.run());

    let deadline = Instant::now() + Duration::from_secs(2);
    let pid = loop {
      if let Some(pid) = *seen_pid.lock().unwrap() {
        let log = dir.join(format!("{pid}.log"));
        if std::fs::read_to_string(&log).is_ok_and(|c| c.contains("hi")) {
          break pid;
        }
      }
      assert!(Instant::now() < deadline, "pid-named log never got output");
      tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_ne!(pid, 0, "resolver should receive a real pid");

    let id = resolve(&pc, "/pidlog").await;
    pc.send(KernelCommand::RemoveTask(id));
    pc.send(KernelCommand::Quit);
    tokio::time::timeout(Duration::from_secs(2), kernel_task)
      .await
      .expect("timed out waiting for kernel to quit")
      .unwrap();

    let _ = std::fs::remove_dir_all(&dir);
  }

  #[tokio::test]
  async fn stop_signal_cmd_runs_shell_command() {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_nanos();
    let mut marker = std::env::temp_dir();
    marker.push(format!("mprocs_stopcmd_{}_{}", std::process::id(), nanos));

    let kernel = Kernel::new();
    let pc = kernel.context();

    let path = TaskPath::new("/sleeper").unwrap();
    let spec = ProcessSpec::from_argv(vec![
      "sh".to_string(),
      "-c".to_string(),
      "sleep 100".to_string(),
    ]);
    spawn_proc_task(
      &pc,
      Some(path),
      ProcTaskConfig {
        stop: StopSignal::Cmd(format!("printf done > {}", marker.display())),
        ..ProcTaskConfig::new(spec)
      },
    );

    let kernel_task = tokio::spawn(kernel.run());

    let id = resolve(&pc, "/sleeper").await;
    pc.send(KernelCommand::TaskCmd(id, TaskCmd::Stop));

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
      if marker.exists() {
        break;
      }
      assert!(Instant::now() < deadline, "stop command never ran");
      tokio::time::sleep(Duration::from_millis(10)).await;
    }

    pc.send(KernelCommand::TaskCmd(id, TaskCmd::Kill));
    pc.send(KernelCommand::RemoveTask(id));
    pc.send(KernelCommand::Quit);
    tokio::time::timeout(Duration::from_secs(2), kernel_task)
      .await
      .expect("timed out waiting for kernel to quit")
      .unwrap();

    let _ = std::fs::remove_file(&marker);
  }
}

fn spawn_native(
  ctx: &TaskContext,
  spec: &ProcessSpec,
  size: Winsize,
) -> anyhow::Result<NativeProcess> {
  let exit_ctx = ctx.clone();

  #[cfg(unix)]
  {
    Ok(crate::process::unix_process::UnixProcess::spawn(
      ctx.task_id,
      spec,
      size,
      Box::new(move |wait_status| {
        let code = wait_status.exit_status().unwrap_or(212) as u32;
        exit_ctx.send_self_custom(ProcExited(code));
      }),
    )?)
  }

  #[cfg(windows)]
  {
    use anyhow::Context as _;
    crate::process::win_process::WinProcess::spawn(
      ctx.task_id,
      spec,
      size,
      Box::new(move |exit_code| {
        let code = exit_code.unwrap_or(213) as u32;
        exit_ctx.send_self_custom(ProcExited(code));
      }),
    )
    .context("WinProcess::spawn")
  }
}
