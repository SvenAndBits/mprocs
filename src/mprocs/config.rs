use std::collections::HashMap;
use std::{ffi::OsString, path::PathBuf, str::FromStr};

use anyhow::{Result, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_yaml::Value;

use crate::mprocs::{
  event::AppEvent,
  proc::StopSignal,
  proc_health::{
    HealthCheckDef, HealthCheckRegistry, HookSet, parse_hooks, parse_proc_healthchecks,
    parse_registry, substitute_vars,
  },
  proc_log_config::LogConfig,
  settings::Settings,
  yaml_val::{Val, value_to_string},
};
use crate::process::process_spec::ProcessSpec;

pub struct ConfigContext {
  pub path: PathBuf,
}

fn resolve_config_path(path: &str, ctx: &ConfigContext) -> Result<PathBuf> {
  let mut buf = PathBuf::new();
  if let Some(rest) = path.strip_prefix("<CONFIG_DIR>") {
    if let Some(parent) = dunce::canonicalize(&ctx.path)?.parent() {
      buf.push(parent);
    }
    buf.push(rest.trim_start_matches(['/', '\\']));
  } else {
    buf.push(path);
  }

  Ok(buf)
}

pub struct Config {
  pub procs: Vec<ProcConfig>,
  pub healthchecks: HealthCheckRegistry,
  pub server: Option<ServerConfig>,
  pub exec: Option<AppEvent>,
  pub hide_keymap_window: bool,
  pub mouse_scroll_speed: usize,
  pub scrollback_len: usize,
  pub proc_list_width: usize,
  pub proc_list_title: String,
  pub on_init: Option<AppEvent>,
  pub on_all_finished: Option<AppEvent>,
  pub proc_log: Option<LogConfig>,
}

impl Config {
  pub fn from_value(
    value: &Value,
    ctx: &ConfigContext,
    settings: &Settings,
  ) -> Result<Config> {
    let config = Val::new(value)?;
    let config = config.as_object()?;

    let healthchecks =
      if let Some(v) = config.get(&Value::from("healthchecks")) {
        parse_registry(v)?
      } else {
        HealthCheckRegistry::new()
      };

    let procs = if let Some(procs) = config.get(&Value::from("procs")) {
      let procs = procs
        .as_object()?
        .into_iter()
        .map(|(name, proc)| {
          ProcConfig::from_val(
            value_to_string(&name)?,
            settings.mouse_scroll_speed,
            settings.scrollback_len,
            proc,
            ctx,
            &healthchecks,
          )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
      procs
    } else {
      Vec::new()
    };

    let server = if let Some(addr) = config.get(&Value::from("server")) {
      Some(ServerConfig::from_str(addr.as_str()?)?)
    } else {
      None
    };

    let proc_list_title =
      if let Some(title) = config.get(&Value::from("proc_list_title")) {
        title.as_str()?.to_string()
      } else {
        settings.proc_list_title.clone()
      };

    let on_all_finished =
      if let Some(val) = config.get(&Value::from("on_all_finished")) {
        Some(serde_yaml::from_value(val.raw().clone())?)
      } else {
        settings.on_all_finished.clone()
      };

    let proc_log = {
      match config.get(&Value::from("proc_log")) {
        Some(val) => {
          crate::mprocs::proc_log_config::parse_log_config(val, |path| {
            resolve_config_path(path, ctx)
          })?
        }
        None => settings.proc_log.clone(),
      }
    };

    let config = Config {
      procs,
      healthchecks,
      server,
      exec: None,
      hide_keymap_window: settings.hide_keymap_window,
      mouse_scroll_speed: settings.mouse_scroll_speed,
      scrollback_len: settings.scrollback_len,
      proc_list_width: settings.proc_list_width,
      proc_list_title,
      on_init: None,
      on_all_finished,
      proc_log,
    };

    Ok(config)
  }

  pub fn make_default(settings: &Settings) -> anyhow::Result<Self> {
    Ok(Self {
      procs: Vec::new(),
      healthchecks: HealthCheckRegistry::new(),
      server: None,
      exec: None,
      hide_keymap_window: settings.hide_keymap_window,
      mouse_scroll_speed: settings.mouse_scroll_speed,
      scrollback_len: settings.scrollback_len,
      proc_list_width: settings.proc_list_width,
      proc_list_title: settings.proc_list_title.clone(),
      on_init: None,
      on_all_finished: settings.on_all_finished.clone(),
      proc_log: settings.proc_log.clone(),
    })
  }
}

#[derive(Clone)]
pub struct ProcConfig {
  pub name: String,
  pub cmd: CmdConfig,
  pub cwd: Option<OsString>,
  pub env: Option<IndexMap<String, Option<String>>>,
  pub autostart: bool,
  pub autorestart: bool,

  pub stop: StopSignal,

  pub deps: Vec<String>,

  pub vars: HashMap<String, String>,
  pub healthchecks: Vec<HealthCheckDef>,
  pub hooks: HookSet,

  pub mouse_scroll_speed: usize,
  pub scrollback_len: usize,
  pub log: Option<LogConfig>,
}

impl ProcConfig {
  fn from_val(
    name: String,
    mouse_scroll_speed: usize,
    scrollback_len: usize,
    val: Val,
    ctx: &ConfigContext,
    hc_registry: &HealthCheckRegistry,
  ) -> Result<Option<ProcConfig>> {
    match val.raw() {
      Value::Null => Ok(None),
      Value::Bool(_) => todo!(),
      Value::Number(_) => todo!(),
      Value::String(shell) => Ok(Some(ProcConfig {
        name,
        cmd: CmdConfig::Shell {
          shell: shell.to_owned(),
        },
        cwd: None,
        env: None,
        autostart: true,
        autorestart: false,
        stop: StopSignal::default(),
        deps: Vec::new(),
        vars: HashMap::new(),
        healthchecks: Vec::new(),
        hooks: HookSet::default(),

        mouse_scroll_speed,
        scrollback_len,
        log: None,
      })),
      Value::Sequence(_) => {
        let cmd = val.as_array()?;
        let cmd = cmd
          .into_iter()
          .map(|item| item.as_str().map(|s| s.to_owned()))
          .collect::<Result<Vec<_>>>()?;

        Ok(Some(ProcConfig {
          name,
          cmd: CmdConfig::Cmd { cmd },
          cwd: None,
          env: None,
          autostart: true,
          autorestart: false,
          stop: StopSignal::default(),
          deps: Vec::new(),
          vars: HashMap::new(),
          healthchecks: Vec::new(),
          hooks: HookSet::default(),
          mouse_scroll_speed,
          scrollback_len,
          log: None,
        }))
      }
      Value::Mapping(_) => {
        let map = val.as_object()?;

        let cmd = {
          let shell = map.get(&Value::from("shell"));
          let cmd = map.get(&Value::from("cmd"));

          match (shell, cmd) {
            (None, Some(cmd)) => CmdConfig::Cmd {
              cmd: cmd
                .as_array()?
                .into_iter()
                .map(|v| v.as_str().map(|s| s.to_owned()))
                .collect::<Result<Vec<_>>>()?,
            },
            (Some(shell), None) => CmdConfig::Shell {
              shell: shell.as_str()?.to_owned(),
            },
            (None, None) => todo!(),
            (Some(_), Some(_)) => todo!(),
          }
        };

        let cwd = match map.get(&Value::from("cwd")) {
          Some(cwd) => {
            let cwd = cwd.as_str()?;
            let mut buf = OsString::new();
            if let Some(rest) = cwd.strip_prefix("<CONFIG_DIR>") {
              if let Some(parent) = dunce::canonicalize(&ctx.path)?.parent() {
                buf.push(parent);
              }
              buf.push(rest);
            } else {
              buf.push(cwd);
            }
            Some(buf)
          }
          None => None,
        };

        let log = {
          match map.get(&Value::from("log")) {
            Some(val) => {
              crate::mprocs::proc_log_config::parse_log_config(val, |path| {
                resolve_config_path(path, ctx)
              })?
            }
            None => None,
          }
        };

        let env = match map.get(&Value::from("env")) {
          Some(env) => {
            let env = env.as_object()?;
            let env = env
              .into_iter()
              .map(|(k, v)| {
                let v = match v.raw() {
                  Value::Null => Ok(None),
                  Value::String(v) => Ok(Some(v.to_owned())),
                  _ => Err(v.error_at("Expected string or null")),
                };
                Ok((value_to_string(&k)?, v?))
              })
              .collect::<Result<IndexMap<_, _>>>()?;
            Some(env)
          }
          None => None,
        };
        let env = match map.get(&Value::from("add_path")) {
          Some(add_path) => {
            let extra_paths = match add_path.raw() {
              Value::String(path) => vec![path.as_str()],
              Value::Sequence(paths) => paths
                .iter()
                .filter_map(|path| path.as_str())
                .collect::<Vec<_>>(),
              _ => {
                bail!(add_path.error_at("Expected string or array"));
              }
            };
            let extra_paths = extra_paths
              .into_iter()
              .map(|p| PathBuf::from_str(p).map_err(anyhow::Error::from))
              .collect::<Result<Vec<_>>>()?;
            let mut paths = std::env::var_os("PATH").map_or_else(
              || Vec::new(),
              |path_var| {
                std::env::split_paths(&path_var)
                  .map(|p| p.into_os_string())
                  .collect::<Vec<_>>()
              },
            );
            for p in extra_paths {
              paths.push(p.into_os_string());
            }
            let path_var =
              std::env::join_paths(paths)?.to_string_lossy().to_string();
            let env = if let Some(mut env) = env {
              env.insert("PATH".to_string(), Some(path_var));
              env
            } else {
              let mut env = IndexMap::with_capacity(1);
              env.insert("PATH".to_string(), Some(path_var));
              env
            };
            Some(env)
          }
          None => env,
        };

        let autostart = map
          .get(&Value::from("autostart"))
          .map_or(Ok(true), |v| v.as_bool())?;

        let autorestart = map
          .get(&Value::from("autorestart"))
          .map_or(Ok(false), |v| v.as_bool())?;

        let stop_signal = if let Some(val) = map.get(&Value::from("stop")) {
          StopSignal::from_val(val)?
        } else {
          StopSignal::default()
        };

        let deps = if let Some(deps) = map.get(&Value::from("deps")) {
          deps
            .as_array()?
            .iter()
            .map(|d| d.as_str().map(|s| s.to_string()))
            .collect::<Result<Vec<_>>>()?
        } else {
          Vec::new()
        };

        let vars = if let Some(v) = map.get(&Value::from("vars")) {
          let m = v.as_object()?;
          let mut out = HashMap::with_capacity(m.len());
          for (k, vv) in m {
            let key = value_to_string(&k)?;
            let val_str = match vv.raw() {
              Value::String(s) => s.clone(),
              Value::Bool(b) => b.to_string(),
              Value::Number(n) => n.to_string(),
              Value::Null => String::new(),
              _ => bail!(
                vv.error_at("vars values must be scalar (string/number/bool)")
              ),
            };
            out.insert(key, val_str);
          }
          out
        } else {
          HashMap::new()
        };

        let healthchecks =
          if let Some(v) = map.get(&Value::from("healthchecks")) {
            parse_proc_healthchecks(v, hc_registry)?
          } else {
            Vec::new()
          };

        let hooks = if let Some(v) = map.get(&Value::from("hooks")) {
          parse_hooks(v)?
        } else {
          HookSet::default()
        };

        Ok(Some(ProcConfig {
          name,
          cmd,
          cwd,
          env,
          autostart,
          autorestart,
          stop: stop_signal,
          deps,
          vars,
          healthchecks,
          hooks,
          mouse_scroll_speed,
          scrollback_len,
          log,
        }))
      }
      Value::Tagged(_) => anyhow::bail!("Yaml tags are not supported"),
    }
  }
}

pub enum ServerConfig {
  Tcp(String),
}

impl ServerConfig {
  pub fn from_str(server_addr: &str) -> Result<Self> {
    Ok(Self::Tcp(server_addr.to_string()))
  }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CmdConfig {
  Cmd { cmd: Vec<String> },
  Shell { shell: String },
}

impl From<&ProcConfig> for ProcessSpec {
  fn from(cfg: &ProcConfig) -> Self {
    let mut cmd = match &cfg.cmd {
      CmdConfig::Cmd { cmd } => {
        let cmd: Vec<String> = cmd
          .iter()
          .map(|s| substitute_vars(s, &cfg.vars))
          .collect();
        ProcessSpec::from_argv(cmd)
      }
      CmdConfig::Shell { shell } => {
        cmd_from_shell(&substitute_vars(shell, &cfg.vars))
      }
    };

    if let Some(env) = &cfg.env {
      for (k, v) in env {
        if let Some(v) = v {
          cmd.env(k, substitute_vars(v, &cfg.vars));
        } else {
          cmd.env_remove(k);
        }
      }
    }

    if let Some(cwd) = &cfg.cwd {
      let cwd_str = cwd.to_string_lossy();
      cmd.cwd(substitute_vars(&cwd_str, &cfg.vars));
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::path::PathBuf;

  fn parse(yaml: &str) -> Result<Config> {
    let value: Value = serde_yaml::from_str(yaml)?;
    let ctx = ConfigContext {
      path: PathBuf::from("/tmp/mprocs.yaml"),
    };
    let settings = Settings::default();
    Config::from_value(&value, &ctx, &settings)
  }

  #[test]
  fn parses_named_healthchecks_and_proc_refs() {
    let cfg = parse(
      r#"
healthchecks:
  is_port_open:
    cmd: "nc -z localhost %PORT%"
    interval: 5s
    timeout: 2s
    start_period: 1s
    retries: 4
procs:
  api:
    shell: "run-api"
    vars:
      PORT: 3000
    healthchecks:
      - is_port_open
      - cmd: "curl -fsS http://localhost:%PORT%/health"
        interval: 3s
"#,
    )
    .expect("parse");
    assert_eq!(cfg.healthchecks.len(), 1);
    assert!(cfg.healthchecks.contains_key("is_port_open"));
    let p = cfg.procs.iter().find(|p| p.name == "api").unwrap();
    assert_eq!(p.vars.get("PORT"), Some(&"3000".to_string()));
    assert_eq!(p.healthchecks.len(), 2);
    assert_eq!(p.healthchecks[0].cmd, "nc -z localhost %PORT%");
    assert_eq!(p.healthchecks[0].retries, 4);
    assert_eq!(
      p.healthchecks[1].cmd,
      "curl -fsS http://localhost:%PORT%/health"
    );
  }

  #[test]
  fn rejects_unknown_named_healthcheck() {
    let res = parse(
      r#"
procs:
  api:
    shell: "run-api"
    healthchecks: [does_not_exist]
"#,
    );
    let err = match res {
      Ok(_) => panic!("expected error"),
      Err(e) => e,
    };
    assert!(err.to_string().contains("does_not_exist"));
  }

  #[test]
  fn parses_hooks() {
    let cfg = parse(
      r#"
procs:
  api:
    shell: "run-api"
    hooks:
      started:
        cmd: "echo up"
      stopped:
        cmd: "echo down"
        async: true
"#,
    )
    .expect("parse");
    let p = cfg.procs.iter().find(|p| p.name == "api").unwrap();
    assert!(p.hooks.started.is_some());
    assert_eq!(p.hooks.started.as_ref().unwrap().cmd, "echo up");
    assert!(!p.hooks.started.as_ref().unwrap().async_);
    assert!(p.hooks.stopped.as_ref().unwrap().async_);
  }

  #[test]
  fn vars_substitute_into_shell() {
    let cfg = parse(
      r#"
procs:
  api:
    shell: "echo %PORT% %HOST%"
    vars:
      PORT: 5432
      HOST: localhost
"#,
    )
    .expect("parse");
    let p = cfg.procs.iter().find(|p| p.name == "api").unwrap();
    let spec: ProcessSpec = p.into();
    // The final argv element is the substituted shell string.
    let last = spec.args.last().expect("argv");
    assert_eq!(last, "echo 5432 localhost");
  }
}
