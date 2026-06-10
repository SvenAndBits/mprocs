use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::cfg::{CfgCx, CfgDoc, CfgNode, CfgObj, FromCfg};
use crate::config::health::{
  HealthCheckRegistry, HookRegistry, parse_hook_registry, parse_registry,
};
use crate::config::hook::{Hook, event_from_cfg};
use crate::config::keymap::KeymapConfig;
use crate::config::log::LogConfig;
use crate::config::proc::{ProcConfig, parse_proc_settings, proc_from_cfg};
use crate::config::tui::TuiConfig;

pub struct Config {
  pub log: LogConfig,
  pub procs: Vec<ProcConfig>,
  pub proc_defaults: ProcConfig,
  pub tui: TuiConfig,
  pub keymap: KeymapConfig,
  pub on_init: Option<Hook>,
  pub on_all_finished: Option<Hook>,
  pub on_client_exit: OnClientExit,
  pub healthchecks: HealthCheckRegistry,
  pub hooks: HookRegistry,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OnClientExit {
  #[default]
  Detach,
  StopAll,
}

impl FromCfg for OnClientExit {
  fn from_cfg(node: &CfgNode<'_>, _cx: &CfgCx) -> Result<Self> {
    match node.as_str()? {
      "detach" => Ok(Self::Detach),
      "stop_all" | "stop-all" => Ok(Self::StopAll),
      other => Err(node.error(format!(
        "on_client_exit must be 'detach' or 'stop_all', got '{other}'"
      ))),
    }
  }
}

impl Config {
  pub fn make_default() -> Self {
    Self {
      log: LogConfig::default(),
      procs: Vec::new(),
      proc_defaults: ProcConfig::default(),
      tui: TuiConfig::builtin(),
      keymap: KeymapConfig::default(),
      on_init: None,
      on_all_finished: None,
      on_client_exit: OnClientExit::default(),
      healthchecks: HealthCheckRegistry::new(),
      hooks: HookRegistry::new(),
    }
  }

  pub fn load_dir(working_dir: &Path, config_name: &str) -> Result<Config> {
    let mut config = Config::make_default();

    // GLOBAL
    if let Some(global) = global_config_path() {
      if global.exists() {
        let dir = global.parent().unwrap_or(working_dir).to_path_buf();
        let cx = CfgCx::new(dir);
        let doc = CfgDoc::load(&global, &cx)?;
        let obj = doc.root().as_obj()?;
        if obj.get("procs").is_some() {
          bail!(
            "'procs' is not allowed in the global config ({}); \
             define processes in the workspace dekit.yaml",
            global.display()
          );
        }
        config.apply(&obj, &cx)?;
      }
    }

    // LOCAL
    let ws = working_dir.join(config_name);
    if ws.exists() {
      let cx = CfgCx::new(working_dir.to_path_buf());
      let doc = CfgDoc::load(&ws, &cx)?;
      let obj = doc.root().as_obj()?;
      config.apply(&obj, &cx)?;
      if let Some(node) = obj.get("procs") {
        let procs = node
          .as_obj()?
          .iter()
          .map(|(path, proc)| {
            proc_from_cfg(
              path.to_string(),
              &proc,
              &cx,
              &config.healthchecks,
              &config.hooks,
            )
          })
          .collect::<Result<Vec<_>>>()?;
        config.procs = procs;
      }
    }

    Ok(config)
  }

  fn apply(&mut self, obj: &CfgObj<'_>, cx: &CfgCx) -> Result<()> {
    self.log.merge(obj, cx)?;
    if let Some(pd) = obj.get("proc_defaults") {
      let over = parse_proc_settings(&pd.as_obj()?, cx)?;
      self.proc_defaults =
        std::mem::take(&mut self.proc_defaults).overlay(over);
    }
    self.tui.merge(obj, cx)?;
    self.keymap.merge(obj)?;
    if let Some(hook) = event_from_cfg(obj, "on_init")? {
      self.on_init = Some(hook);
    }
    if let Some(hook) = event_from_cfg(obj, "on_all_finished")? {
      self.on_all_finished = Some(hook);
    }
    if let Some(v) = obj.optional("on_client_exit", cx)? {
      self.on_client_exit = v;
    }
    if let Some(node) = obj.get("healthchecks") {
      self.healthchecks.extend(parse_registry(&node)?);
    }
    if let Some(node) = obj.get("hooks") {
      self.hooks.extend(parse_hook_registry(&node)?);
    }
    Ok(())
  }
}

fn global_config_path() -> Option<PathBuf> {
  let mut base = match std::env::var_os("XDG_CONFIG_HOME") {
    Some(dir) => PathBuf::from(dir),
    None => default_config_dir()?,
  };
  base.push("dekit");
  base.push("dekit.yaml");
  Some(base)
}

#[cfg(windows)]
fn default_config_dir() -> Option<PathBuf> {
  Some(PathBuf::from(std::env::var_os("APPDATA")?))
}

#[cfg(not(windows))]
fn default_config_dir() -> Option<PathBuf> {
  let mut path = PathBuf::from(std::env::var_os("HOME")?);
  path.push(".config");
  Some(path)
}
