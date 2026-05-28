//! Lifecycle hook runner.
//!
//! Each hook runs the configured command via the system shell. Blocking by
//! default — the caller awaits the future and the hook's exit code decides
//! whether the lifecycle transition succeeds. With `async_: true` the hook
//! is detached (spawned) and the awaited future returns immediately.

use std::collections::HashMap;
use std::ffi::OsString;

use tokio::process::Command;

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

pub async fn run_hook(
  hook: &HookDef,
  vars: &HashMap<String, String>,
  cwd: Option<&OsString>,
) -> HookResult {
  let cmd_str = substitute_vars(&hook.cmd, vars);
  let cwd = cwd.cloned();
  if hook.async_ {
    // Fire-and-forget. Detach a tokio task.
    tokio::spawn(async move {
      let _ = exec_shell(&cmd_str, cwd.as_ref()).await;
    });
    return Ok(());
  }
  match exec_shell(&cmd_str, cwd.as_ref()).await {
    Ok(code) if code == 0 => Ok(()),
    Ok(code) => Err(HookError::ExitCode(code)),
    Err(e) => Err(HookError::IoError(e)),
  }
}

async fn exec_shell(
  cmd: &str,
  cwd: Option<&OsString>,
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
  command.stdin(std::process::Stdio::null());
  command.stdout(std::process::Stdio::null());
  command.stderr(std::process::Stdio::null());
  match command.status().await {
    Ok(s) => Ok(s.code().unwrap_or(-1)),
    Err(e) => Err(e.to_string()),
  }
}
