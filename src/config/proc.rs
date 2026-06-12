use std::{ffi::OsString, path::PathBuf};

use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::cfg::{CfgCx, CfgNode, CfgObj};
use crate::config::health::{
  HealthCheckDef, HealthCheckRegistry, HookRegistry, HookSet, Vars,
  parse_hooks, parse_proc_healthchecks, substitute_vars,
};
use crate::config::proc_log::ProcLogConfig;
use crate::console::proc::StopSignal;
use crate::parse_shell::split_argv;
use crate::process::process_spec::ProcessSpec;

const DEFAULT_SCROLLBACK_LEN: usize = 1000;
const DEFAULT_MOUSE_SCROLL_SPEED: usize = 5;
const DEFAULT_RESTART_DELAY_MS: u64 = 1000;

#[derive(Clone, Default)]
pub struct ProcConfig {
  pub path: String,
  pub cmd: Option<CmdConfig>,
  pub deps: Vec<String>,

  pub cwd: Option<OsString>,
  pub env: Option<IndexMap<String, Option<String>>>,
  pub add_path: Option<Vec<PathBuf>>,
  pub autostart: Option<bool>,
  pub autorestart: Option<bool>,
  pub restart_delay: Option<u64>,
  pub oneshot: Option<bool>,
  pub stop: Option<StopSignal>,
  pub log: Option<ProcLogConfig>,
  pub scrollback_len: Option<usize>,
  pub mouse_scroll_speed: Option<usize>,

  pub vars: Vars,
  pub healthchecks: Vec<HealthCheckDef>,
  pub hooks: HookSet,
}

impl ProcConfig {
  pub fn overlay(self, over: ProcConfig) -> ProcConfig {
    ProcConfig {
      path: if over.path.is_empty() {
        self.path
      } else {
        over.path
      },
      cmd: over.cmd.or(self.cmd),
      deps: if over.deps.is_empty() {
        self.deps
      } else {
        over.deps
      },
      cwd: over.cwd.or(self.cwd),
      env: over.env.or(self.env),
      add_path: over.add_path.or(self.add_path),
      autostart: over.autostart.or(self.autostart),
      autorestart: over.autorestart.or(self.autorestart),
      restart_delay: over.restart_delay.or(self.restart_delay),
      oneshot: over.oneshot.or(self.oneshot),
      stop: over.stop.or(self.stop),
      log: match (over.log, self.log) {
        (Some(over), Some(base)) => Some(base.merged(&over)),
        (over, base) => over.or(base),
      },
      scrollback_len: over.scrollback_len.or(self.scrollback_len),
      mouse_scroll_speed: over.mouse_scroll_speed.or(self.mouse_scroll_speed),
      vars: {
        let mut merged = self.vars;
        merged.extend(over.vars);
        merged
      },
      healthchecks: if over.healthchecks.is_empty() {
        self.healthchecks
      } else {
        over.healthchecks
      },
      hooks: self.hooks.overlay(over.hooks),
    }
  }

  pub fn autostart(&self) -> bool {
    self.autostart.unwrap_or(false)
  }
  pub fn autorestart(&self) -> bool {
    self.autorestart.unwrap_or(false)
  }
  pub fn restart_delay(&self) -> std::time::Duration {
    std::time::Duration::from_millis(
      self.restart_delay.unwrap_or(DEFAULT_RESTART_DELAY_MS),
    )
  }
  pub fn oneshot(&self) -> bool {
    self.oneshot.unwrap_or(false)
  }
  pub fn stop(&self) -> StopSignal {
    self.stop.clone().unwrap_or_default()
  }
  pub fn scrollback_len(&self) -> usize {
    self.scrollback_len.unwrap_or(DEFAULT_SCROLLBACK_LEN)
  }
  pub fn mouse_scroll_speed(&self) -> usize {
    self
      .mouse_scroll_speed
      .unwrap_or(DEFAULT_MOUSE_SCROLL_SPEED)
  }
}

pub(crate) fn parse_proc_settings(
  obj: &CfgObj<'_>,
  cx: &CfgCx,
) -> Result<ProcConfig> {
  let mut p = ProcConfig::default();
  if let Some(cwd) = obj.get("cwd") {
    p.cwd = Some(cx.resolve_path(cwd.as_str()?).into_os_string());
  }
  p.env = obj.optional("env", cx)?;
  p.add_path = obj.optional("add_path", cx)?;
  p.autostart = obj.optional("autostart", cx)?;
  p.autorestart = obj.optional("autorestart", cx)?;
  p.restart_delay = obj.optional("restart_delay", cx)?;
  p.oneshot = obj.optional("oneshot", cx)?;
  p.stop = obj.optional("stop", cx)?;
  p.log = obj.optional("log", cx)?;
  p.scrollback_len = obj.optional("scrollback_len", cx)?;
  p.mouse_scroll_speed = obj.optional("mouse_scroll_speed", cx)?;
  if let Some(node) = obj.get("vars") {
    let mut vars = Vars::new();
    for (k, v) in node.as_obj()?.iter() {
      vars.insert(k.to_string(), var_value(&v)?);
    }
    p.vars = vars;
  }
  Ok(p)
}

fn var_value(node: &CfgNode<'_>) -> Result<String> {
  use serde_yaml::Value;
  match node.raw() {
    Value::String(s) => Ok(s.clone()),
    Value::Number(n) => Ok(n.to_string()),
    Value::Bool(b) => Ok(b.to_string()),
    _ => Err(node.error("var value must be a string, number, or bool")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::cfg::{CfgCx, CfgPath};

  #[test]
  fn vars_accept_scalar_values() {
    let raw: serde_yaml::Value =
      serde_yaml::from_str("shell: echo hi\nvars:\n  PORT: 3000\n  FLAG: true\n  HOST: localhost\n")
        .unwrap();
    let node = CfgNode::new(&raw, CfgPath::root());
    let cx = CfgCx::new(std::path::PathBuf::from("."));
    let cfg = proc_from_cfg(
      "web".to_string(),
      &node,
      &cx,
      &Default::default(),
      &Default::default(),
    )
    .unwrap();
    assert_eq!(cfg.vars.get("PORT").map(String::as_str), Some("3000"));
    assert_eq!(cfg.vars.get("FLAG").map(String::as_str), Some("true"));
    assert_eq!(cfg.vars.get("HOST").map(String::as_str), Some("localhost"));
  }
}

pub(crate) fn proc_from_cfg(
  path: String,
  node: &CfgNode<'_>,
  cx: &CfgCx,
  hc_registry: &HealthCheckRegistry,
  hook_registry: &HookRegistry,
) -> Result<ProcConfig> {
  let obj = node.as_obj()?;
  let mut p = parse_proc_settings(&obj, cx)?;
  p.path = path;
  p.cmd = Some(cmd_from_cfg(node)?);
  p.deps = obj.default("deps", Vec::new(), cx)?;
  if let Some(hc) = obj.get("healthchecks") {
    p.healthchecks = parse_proc_healthchecks(&hc, hc_registry)?;
  }
  if let Some(hooks) = obj.get("hooks") {
    p.hooks = parse_hooks(&hooks, hook_registry)?;
  }
  if p.oneshot() && !p.healthchecks.is_empty() {
    bail!(obj.error(
      "a `oneshot` process cannot also define `healthchecks` (one gates on \
       exit, the other on a check passing)"
    ));
  }
  Ok(p)
}

fn cmd_from_cfg(node: &CfgNode<'_>) -> Result<CmdConfig> {
  let obj = node.as_obj()?;
  match (obj.get("shell"), obj.get("cmd")) {
    (Some(shell), None) => Ok(CmdConfig::Shell {
      shell: shell.as_str()?.to_owned(),
    }),
    (None, Some(cmd)) => {
      let argv = if cmd.is_string() {
        split_argv(cmd.as_str()?).map_err(|err| cmd.error(err))?
      } else {
        cmd
          .as_arr()?
          .iter()
          .map(|item| Ok(item.as_str()?.to_owned()))
          .collect::<Result<Vec<_>>>()?
      };
      Ok(CmdConfig::Cmd { cmd: argv })
    }
    (None, None) => bail!(obj.error("process must define 'cmd' or 'shell'")),
    (Some(_), Some(_)) => {
      bail!(obj.error("process must define only one of 'cmd' or 'shell'"))
    }
  }
}

#[derive(Clone)]
pub enum CmdConfig {
  Cmd { cmd: Vec<String> },
  Shell { shell: String },
}

impl From<&ProcConfig> for ProcessSpec {
  fn from(cfg: &ProcConfig) -> Self {
    let vars = &cfg.vars;
    let mut cmd = match &cfg.cmd {
      Some(CmdConfig::Cmd { cmd }) => ProcessSpec::from_argv(
        cmd.iter().map(|a| substitute_vars(a, vars)).collect(),
      ),
      Some(CmdConfig::Shell { shell }) => {
        cmd_from_shell(&substitute_vars(shell, vars))
      }
      None => ProcessSpec::from_argv(Vec::new()),
    };

    if let Some(env) = &cfg.env {
      for (k, v) in env {
        if let Some(v) = v {
          cmd.env(k, substitute_vars(v, vars));
        } else {
          cmd.env_remove(k);
        }
      }
    }

    if let Some(add_path) = cfg.add_path.as_ref().filter(|p| !p.is_empty()) {
      // Base PATH is the proc's own `env` override if it sets one, otherwise
      // the ambient PATH resolved at spawn time.
      let base = cfg
        .env
        .as_ref()
        .and_then(|env| env.get("PATH").cloned().flatten())
        .or_else(|| std::env::var("PATH").ok());
      let mut paths: Vec<PathBuf> = base
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
      paths.extend(add_path.iter().cloned());
      if let Ok(joined) = std::env::join_paths(&paths) {
        cmd.env("PATH", joined.to_string_lossy().into_owned());
      }
    }

    if let Some(cwd) = &cfg.cwd {
      cmd.cwd(substitute_vars(&cwd.to_string_lossy(), vars));
    } else if let Ok(cwd) = std::env::current_dir() {
      cmd.cwd(cwd.to_string_lossy());
    }

    cmd
  }
}

#[cfg(windows)]
pub fn cmd_from_shell(shell: &str) -> ProcessSpec {
  ProcessSpec::from_argv(vec![
    "pwsh.exe".into(),
    "-Command".into(),
    shell.into(),
  ])
}

#[cfg(not(windows))]
pub fn cmd_from_shell(shell: &str) -> ProcessSpec {
  ProcessSpec::from_argv(vec!["/bin/sh".into(), "-c".into(), shell.into()])
}
