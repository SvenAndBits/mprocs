use std::ffi::OsString;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::health::{HookDef, Vars, substitute_vars};
use crate::kernel::kernel_message::SharedVt;
use crate::task::child_vt::vt_process_safe;

pub type HookResult = Result<(), HookError>;

#[derive(Debug)]
pub enum HookError {
  ExitCode(i32),
  IoError(String),
}

pub type EnvOverrides = Vec<(String, Option<String>)>;

pub async fn run_hook(
  hook: &HookDef,
  vars: &Vars,
  cwd: Option<&OsString>,
  env: &EnvOverrides,
  out_vt: Option<SharedVt>,
) -> HookResult {
  let cmd_str = substitute_vars(&hook.cmd, vars);
  let cwd = cwd.cloned();
  let env_clone = env.clone();
  write_banner(out_vt.as_ref(), &cmd_str);
  if hook.async_ {
    let vt_for_task = out_vt.clone();
    tokio::spawn(async move {
      let _ =
        exec_shell(&cmd_str, cwd.as_ref(), &env_clone, vt_for_task).await;
    });
    return Ok(());
  }
  match exec_shell(&cmd_str, cwd.as_ref(), env, out_vt).await {
    Ok(0) => Ok(()),
    Ok(code) => Err(HookError::ExitCode(code)),
    Err(e) => Err(HookError::IoError(e)),
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

async fn exec_shell(
  cmd: &str,
  cwd: Option<&OsString>,
  env: &EnvOverrides,
  out_vt: Option<SharedVt>,
) -> Result<i32, String> {
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

pub fn shell_command(cmd: &str) -> Command {
  #[cfg(windows)]
  {
    let mut c = Command::new("pwsh.exe");
    c.arg("-Command").arg(cmd);
    c
  }
  #[cfg(not(windows))]
  {
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(cmd);
    c
  }
}

pub fn stdio(capture: bool) -> std::process::Stdio {
  if capture {
    std::process::Stdio::piped()
  } else {
    std::process::Stdio::null()
  }
}

pub fn spawn_pipe<R: AsyncReadExt + Unpin + Send + 'static>(
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
          vt_process_safe(&vt, &out);
        }
      }
    }
  });
}
