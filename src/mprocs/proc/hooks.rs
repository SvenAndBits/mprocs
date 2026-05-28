//! Lifecycle hook runner.
//!
//! Each hook runs the configured command via the system shell. Blocking by
//! default — the caller awaits the future and the hook's exit code decides
//! whether the lifecycle transition succeeds. With `async_: true` the hook
//! is detached (spawned) and the awaited future returns immediately.

use std::collections::HashMap;
use std::ffi::OsString;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::kernel::kernel_message::SharedVt;
use crate::mprocs::proc_health::{HookDef, substitute_vars};

/// Outcome of (synchronously) awaiting a hook. For async hooks this is
/// always `Ok`.
pub type HookResult = Result<(), HookError>;

#[derive(Debug)]
pub enum HookError {
  /// Hook ran but exited with a non-zero status code.
  ExitCode(i32),
  /// Hook failed to launch or timed out.
  IoError(String),
}

/// (key, Some(value)) sets the env var on the child; (key, None) removes
/// any inherited value. Mirrors how mprocs already applies per-proc `env:`
/// to the proc's own subprocess (see `From<&ProcConfig> for ProcessSpec`).
pub type EnvOverrides = Vec<(String, Option<String>)>;

pub async fn run_hook(
  hook: &HookDef,
  vars: &HashMap<String, String>,
  cwd: Option<&OsString>,
  env: &EnvOverrides,
  out_vt: Option<SharedVt>,
) -> HookResult {
  let cmd_str = substitute_vars(&hook.cmd, vars);
  let cwd = cwd.cloned();
  let env_clone = env.clone();
  write_banner(out_vt.as_ref(), &cmd_str);
  if hook.async_ {
    // Fire-and-forget. Detach a tokio task.
    let vt_for_task = out_vt.clone();
    tokio::spawn(async move {
      let _ = exec_shell(&cmd_str, cwd.as_ref(), &env_clone, vt_for_task).await;
    });
    return Ok(());
  }
  match exec_shell(&cmd_str, cwd.as_ref(), env, out_vt).await {
    Ok(code) if code == 0 => Ok(()),
    Ok(code) => Err(HookError::ExitCode(code)),
    Err(e) => Err(HookError::IoError(e)),
  }
}

fn write_banner(out_vt: Option<&SharedVt>, cmd: &str) {
  if let Some(vt) = out_vt {
    if let Ok(mut p) = vt.write() {
      let stamp = chrono_like_now();
      let line = format!("\r\n\x1b[2m── {} ──\x1b[0m\r\n\x1b[1m$\x1b[0m {}\r\n", stamp, cmd);
      let mut events = Vec::new();
      p.screen.process(line.as_bytes(), &mut events);
    }
  }
}

/// Compact local timestamp without pulling in the chrono crate.
fn chrono_like_now() -> String {
  use std::time::{SystemTime, UNIX_EPOCH};
  let secs = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0);
  // Best-effort wall-clock HH:MM:SS in UTC (no TZ libs).
  let h = (secs / 3600) % 24;
  let m = (secs / 60) % 60;
  let s = secs % 60;
  format!("{:02}:{:02}:{:02} UTC", h, m, s)
}

async fn exec_shell(
  cmd: &str,
  cwd: Option<&OsString>,
  env: &EnvOverrides,
  out_vt: Option<SharedVt>,
) -> Result<i32, String> {
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

  let mut child = command.spawn().map_err(|e| e.to_string())?;
  if let Some(vt) = out_vt {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    spawn_pipe(stdout, vt.clone());
    spawn_pipe(stderr, vt);
  }
  match child.wait().await {
    Ok(s) => Ok(s.code().unwrap_or(-1)),
    Err(e) => Err(e.to_string()),
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
          if let Ok(mut p) = vt.write() {
            // Many shell tools emit bare LF; the TTY parser expects CRLF.
            // Insert CR before LF unless the previous byte was already CR.
            let bytes = &buf[..n];
            let mut needs_translate = false;
            let mut last = 0u8;
            for &b in bytes {
              if b == b'\n' && last != b'\r' {
                needs_translate = true;
                break;
              }
              last = b;
            }
            if needs_translate {
              let mut out = Vec::with_capacity(n + n / 8);
              let mut prev = 0u8;
              for &b in bytes {
                if b == b'\n' && prev != b'\r' {
                  out.push(b'\r');
                }
                out.push(b);
                prev = b;
              }
              let mut events = Vec::new();
              p.screen.process(&out, &mut events);
            } else {
              let mut events = Vec::new();
              p.screen.process(bytes, &mut events);
            }
          }
        }
      }
    }
  });
}
