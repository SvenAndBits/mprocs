use std::{collections::HashMap, sync::Mutex};

use anyhow::{anyhow, bail};
use rustix::{
  process::{WaitOptions, WaitStatus},
  termios::Pid,
};
use tokio::signal::unix::SignalKind;

pub struct UnixProcessesWaiter {
  thread: tokio::task::JoinHandle<anyhow::Result<()>>,

  listeners: HashMap<Pid, Box<dyn Fn(WaitStatus) + Send + Sync>>,
  unclaimed: HashMap<Pid, WaitStatus>,
}

static GLOBAL: Mutex<Option<UnixProcessesWaiter>> = Mutex::new(None);

impl UnixProcessesWaiter {
  pub fn wait_for(pid: Pid, f: Box<dyn Fn(WaitStatus) + Send + Sync>) {
    // Fast path: child already exited and the SIGCHLD-driven loop
    // stashed it.
    {
      let guard = GLOBAL.lock();
      if let Ok(mut guard) = guard {
        if let Some(pw) = guard.as_mut() {
          if let Some(wait_status) = pw.unclaimed.remove(&pid) {
            drop(guard);
            f(wait_status);
            return;
          }
        }
      }
    }
    // Slow path: SIGCHLD may have already fired before we registered
    // (and been ignored, since the new handler only reaps listed PIDs).
    // Do a non-blocking waitpid on this specific PID to catch that.
    if let Ok(Some((_, wait_status))) =
      rustix::process::waitpid(Some(pid), WaitOptions::NOHANG)
    {
      f(wait_status);
      return;
    }
    // Register and wait for SIGCHLD to fire.
    match GLOBAL.lock() {
      Ok(mut guard) => {
        if let Some(pw) = guard.as_mut() {
          pw.listeners.insert(pid, f);
        }
      }
      Err(_) => (),
    }
  }

  pub fn init() -> anyhow::Result<()> {
    let mut holder =
      GLOBAL.lock().map_err(|_e| anyhow!("Mutex is poisoned."))?;
    if holder.is_some() {
      bail!("UnixProcessWaiter is already initialized.");
    }
    let mut signals = tokio::signal::unix::signal(SignalKind::child())?;
    let thread: tokio::task::JoinHandle<anyhow::Result<()>> =
      tokio::spawn(async move {
        while let Some(()) = signals.recv().await {
          // Only reap PIDs we have registered listeners for. Reaping
          // any child (waitpid(-1)) races with tokio's own child reaper
          // for subprocesses spawned via tokio::process::Command (health
          // checks, hooks, stop cmds) and causes them to fail with
          // ECHILD on their wait().
          let pids: Vec<Pid> = match GLOBAL.lock() {
            Ok(guard) => guard
              .as_ref()
              .map(|pw| pw.listeners.keys().copied().collect())
              .unwrap_or_default(),
            Err(_) => continue,
          };
          for pid in pids {
            match rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {
              Ok(Some((_, wait_status))) => {
                let listener_opt = match GLOBAL.lock() {
                  Ok(mut guard) => guard
                    .as_mut()
                    .and_then(|pw| pw.listeners.remove(&pid)),
                  Err(_) => None,
                };
                if let Some(listener) = listener_opt {
                  listener(wait_status);
                } else if let Ok(mut guard) = GLOBAL.lock() {
                  if let Some(pw) = guard.as_mut() {
                    pw.unclaimed.insert(pid, wait_status);
                  }
                }
              }
              Ok(None) => (), // child not yet exited
              Err(e) => {
                // ECHILD just means our listener PID has already been
                // reaped (possibly by the wait_for slow path). Quietly
                // drop the listener.
                if e.raw_os_error() != libc::ECHILD {
                  log::error!(
                    "ProcessesWaiter waitpid({}) error: {} ({})",
                    pid.as_raw_nonzero(),
                    e.kind(),
                    e.raw_os_error()
                  );
                }
                if let Ok(mut guard) = GLOBAL.lock() {
                  if let Some(pw) = guard.as_mut() {
                    pw.listeners.remove(&pid);
                  }
                }
              }
            }
          }
        }
        Ok(())
      });
    *holder = Some(UnixProcessesWaiter {
      thread,

      listeners: Default::default(),
      unclaimed: Default::default(),
    });

    Ok(())
  }

  pub fn uninit() -> anyhow::Result<()> {
    let mut holder =
      GLOBAL.lock().map_err(|_e| anyhow!("Mutex is poisoned."))?;
    match holder.as_mut() {
      Some(pw) => {
        pw.thread.abort();
      }
      None => bail!("Cannot uninit None UnixProcessWaiter."),
    }
    *holder = None;

    Ok(())
  }
}
