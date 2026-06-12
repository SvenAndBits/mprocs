use std::{collections::HashMap, sync::Mutex};

use anyhow::{anyhow, bail};
use rustix::{
  process::{WaitOptions, WaitStatus},
  termios::Pid,
};
use tokio::signal::unix::SignalKind;

type Listener = Box<dyn Fn(WaitStatus) + Send + Sync>;

pub struct UnixProcessesWaiter {
  thread: tokio::task::JoinHandle<anyhow::Result<()>>,

  listeners: HashMap<Pid, Listener>,
}

static GLOBAL: Mutex<Option<UnixProcessesWaiter>> = Mutex::new(None);

impl UnixProcessesWaiter {
  pub fn wait_for(pid: Pid, f: Listener) {
    let mut already_exited = None;
    if let Ok(mut guard) = GLOBAL.lock()
      && let Some(pw) = guard.as_mut()
    {
      match rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {
        Ok(Some((_, status))) => already_exited = Some((f, status)),
        _ => {
          pw.listeners.insert(pid, f);
        }
      }
    }
    if let Some((f, status)) = already_exited {
      f(status);
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
          let mut fired: Vec<(Listener, WaitStatus)> = Vec::new();
          if let Ok(mut guard) = GLOBAL.lock()
            && let Some(pw) = guard.as_mut()
          {
            let pids: Vec<Pid> = pw.listeners.keys().copied().collect();
            for pid in pids {
              match rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {
                Ok(Some((_, wait_status))) => {
                  if let Some(listener) = pw.listeners.remove(&pid) {
                    fired.push((listener, wait_status));
                  }
                }
                Ok(None) => (),
                Err(e) => {
                  if e.raw_os_error() != libc::ECHILD {
                    log::error!(
                      "ProcessesWaiter waitpid({:?}) error: {} ({})",
                      pid,
                      e.kind(),
                      e.raw_os_error()
                    );
                  }
                  pw.listeners.remove(&pid);
                }
              }
            }
          }
          for (listener, wait_status) in fired {
            listener(wait_status);
          }
        }
        Ok(())
      });
    *holder = Some(UnixProcessesWaiter {
      thread,

      listeners: Default::default(),
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
