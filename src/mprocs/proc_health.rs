//! Health checks, lifecycle hooks, and variable substitution for processes.
//!
//! See `docs/healthchecks.md` for the user-facing design.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_yaml::Value;

use crate::mprocs::yaml_val::{Val, value_to_string};

/// A concrete health check. Named entries in the top-level `healthchecks`
/// registry get resolved into this form, as do inline definitions on procs.
#[derive(Clone, Debug)]
pub struct HealthCheckDef {
  /// Display name. For named refs from the top-level registry: the key.
  /// For inline checks: the empty string at parse time, filled in by the
  /// proc-level resolver (e.g. "inline[2]").
  pub name: String,
  pub cmd: String,
  pub interval: Duration,
  pub timeout: Duration,
  pub start_period: Duration,
  pub retries: u32,
  /// Number of consecutive successful invocations required before this
  /// check is considered passing. Failure resets the counter. Default: 1
  /// (a single pass flips the check). Bumping this is the clean way to
  /// require stability — e.g. Postgres on Docker Desktop can briefly
  /// answer ready, then refuse the next connection during port-binding
  /// race; setting `min_passes: 3` forces 3-in-a-row before downstream
  /// procs are unblocked.
  pub min_passes: u32,
}

impl HealthCheckDef {
  fn defaults() -> Self {
    Self {
      name: String::new(),
      cmd: String::new(),
      interval: Duration::from_secs(10),
      timeout: Duration::from_secs(5),
      start_period: Duration::from_secs(0),
      retries: 3,
      min_passes: 1,
    }
  }

  /// Parse from a YAML mapping. Required key: `cmd`.
  pub fn from_val(val: &Val) -> Result<Self> {
    let map = val.as_object()?;
    let mut def = Self::defaults();

    let cmd = map
      .get(&Value::from("cmd"))
      .ok_or_else(|| val.error_at("healthcheck requires `cmd`"))?
      .as_str()?
      .to_owned();
    def.cmd = cmd;

    if let Some(v) = map.get(&Value::from("interval")) {
      def.interval = parse_duration(v.as_str()?)
        .map_err(|e| v.error_at(format!("invalid interval: {e}")))?;
    }
    if let Some(v) = map.get(&Value::from("timeout")) {
      def.timeout = parse_duration(v.as_str()?)
        .map_err(|e| v.error_at(format!("invalid timeout: {e}")))?;
    }
    if let Some(v) = map.get(&Value::from("start_period")) {
      def.start_period = parse_duration(v.as_str()?)
        .map_err(|e| v.error_at(format!("invalid start_period: {e}")))?;
    }
    if let Some(v) = map.get(&Value::from("retries")) {
      def.retries = v.as_usize()? as u32;
    }
    if let Some(v) = map.get(&Value::from("min_passes")) {
      def.min_passes = (v.as_usize()? as u32).max(1);
    }
    Ok(def)
  }
}

/// Top-level `healthchecks` registry: name → def.
pub type HealthCheckRegistry = HashMap<String, HealthCheckDef>;

pub fn parse_registry(val: &Val) -> Result<HealthCheckRegistry> {
  let map = val.as_object()?;
  let mut out = HashMap::with_capacity(map.len());
  for (k, v) in map {
    let name = value_to_string(&k)?;
    let mut def = HealthCheckDef::from_val(&v)?;
    def.name = name.clone();
    out.insert(name, def);
  }
  Ok(out)
}

/// Parse the proc-level `healthchecks` list. List items may be either a
/// string (referencing the registry) or an inline mapping.
pub fn parse_proc_healthchecks(
  val: &Val,
  registry: &HealthCheckRegistry,
) -> Result<Vec<HealthCheckDef>> {
  let items = val.as_array()?;
  let mut out = Vec::with_capacity(items.len());
  let mut inline_seq = 0usize;
  for item in items {
    match item.raw() {
      Value::String(name) => {
        let def = registry.get(name.as_str()).cloned().ok_or_else(|| {
          item.error_at(format!("unknown healthcheck `{}`", name))
        })?;
        out.push(def);
      }
      Value::Mapping(_) => {
        let mut def = HealthCheckDef::from_val(&item)?;
        // Inline checks have no name in the source; auto-label them.
        def.name = format!("inline[{}]", inline_seq);
        inline_seq += 1;
        out.push(def);
      }
      _ => {
        bail!(
          item.error_at("expected healthcheck name (string) or inline mapping")
        );
      }
    }
  }
  Ok(out)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HookEvent {
  Started,
  /// Fires when the proc transitions to the `Running` state — initial
  /// startup once all health checks have passed, AND on recovery from
  /// `Unhealthy`. Blocking by default; until it returns the kernel does
  /// not promote the proc to `Running` and dependent procs are NOT
  /// started. With `async: true` the cascade fires immediately.
  Running,
  Unhealthy,
  Stopped,
  Failed,
}

impl HookEvent {
  pub fn from_str(s: &str) -> Option<Self> {
    match s {
      "started" => Some(Self::Started),
      "running" => Some(Self::Running),
      "unhealthy" => Some(Self::Unhealthy),
      "stopped" => Some(Self::Stopped),
      "failed" => Some(Self::Failed),
      _ => None,
    }
  }
}

#[derive(Clone, Debug)]
pub struct HookDef {
  pub cmd: String,
  /// When true, the hook runs detached and never blocks the lifecycle
  /// transition. Default: false (blocking).
  pub async_: bool,
}

impl HookDef {
  pub fn from_val(val: &Val) -> Result<Self> {
    let map = val.as_object()?;
    let cmd = map
      .get(&Value::from("cmd"))
      .ok_or_else(|| val.error_at("hook requires `cmd`"))?
      .as_str()?
      .to_owned();
    let async_ = match map.get(&Value::from("async")) {
      Some(v) => v.as_bool()?,
      None => false,
    };
    Ok(Self { cmd, async_ })
  }
}

/// Top-level `hooks:` registry — named hooks reusable across procs. A
/// per-proc hook event can be either a string (reference to this
/// registry) or an inline mapping.
pub type HookRegistry = HashMap<String, HookDef>;

pub fn parse_hook_registry(val: &Val) -> Result<HookRegistry> {
  let map = val.as_object()?;
  let mut out = HashMap::with_capacity(map.len());
  for (k, v) in map {
    let name = value_to_string(&k)?;
    let def = HookDef::from_val(&v)?;
    out.insert(name, def);
  }
  Ok(out)
}

#[derive(Clone, Debug, Default)]
pub struct HookSet {
  pub started: Option<HookDef>,
  pub running: Option<HookDef>,
  pub unhealthy: Option<HookDef>,
  pub stopped: Option<HookDef>,
  pub failed: Option<HookDef>,
}

impl HookSet {
  pub fn get(&self, event: HookEvent) -> Option<&HookDef> {
    match event {
      HookEvent::Started => self.started.as_ref(),
      HookEvent::Running => self.running.as_ref(),
      HookEvent::Unhealthy => self.unhealthy.as_ref(),
      HookEvent::Stopped => self.stopped.as_ref(),
      HookEvent::Failed => self.failed.as_ref(),
    }
  }
}

pub fn parse_hooks(val: &Val, registry: &HookRegistry) -> Result<HookSet> {
  let map = val.as_object()?;
  let mut out = HookSet::default();
  for (k, v) in map {
    let name = value_to_string(&k)?;
    let event = HookEvent::from_str(&name)
      .ok_or_else(|| v.error_at(format!("unknown hook event `{}`", name)))?;
    let def = match v.raw() {
      Value::String(ref_name) => registry
        .get(ref_name.as_str())
        .cloned()
        .ok_or_else(|| v.error_at(format!("unknown hook `{}`", ref_name)))?,
      Value::Mapping(_) => HookDef::from_val(&v)?,
      _ => bail!(v.error_at("hook must be a name (string) or inline mapping")),
    };
    match event {
      HookEvent::Started => out.started = Some(def),
      HookEvent::Running => out.running = Some(def),
      HookEvent::Unhealthy => out.unhealthy = Some(def),
      HookEvent::Stopped => out.stopped = Some(def),
      HookEvent::Failed => out.failed = Some(def),
    }
  }
  Ok(out)
}

/// Parse a duration like `10s`, `500ms`, `2m`. Bare integer is treated as
/// seconds.
pub fn parse_duration(s: &str) -> Result<Duration> {
  let s = s.trim();
  if s.is_empty() {
    bail!("empty duration");
  }
  let (num_str, unit) = if let Some(idx) =
    s.find(|c: char| c.is_alphabetic())
  {
    (&s[..idx], &s[idx..])
  } else {
    (s, "s")
  };
  let n: f64 = num_str
    .trim()
    .parse()
    .map_err(|_| anyhow::format_err!("invalid number in duration: `{s}`"))?;
  if n < 0.0 {
    bail!("duration cannot be negative");
  }
  let ms = match unit {
    "ms" => n,
    "s" | "sec" | "secs" => n * 1000.0,
    "m" | "min" | "mins" => n * 60_000.0,
    "h" | "hr" | "hrs" => n * 3_600_000.0,
    other => bail!("unknown duration unit `{other}`"),
  };
  Ok(Duration::from_millis(ms as u64))
}

/// Substitute `%KEY%` tokens in `s` using values from `vars`. Unknown keys
/// are left untouched so existing literal `%` usage isn't disturbed.
pub fn substitute_vars(s: &str, vars: &HashMap<String, String>) -> String {
  if vars.is_empty() || !s.contains('%') {
    return s.to_owned();
  }
  let bytes = s.as_bytes();
  let mut out = String::with_capacity(s.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%' {
      if let Some(end) = bytes[i + 1..].iter().position(|&b| b == b'%') {
        let key = &s[i + 1..i + 1 + end];
        if !key.is_empty() && is_valid_key(key) {
          if let Some(val) = vars.get(key) {
            out.push_str(val);
            i += 2 + end;
            continue;
          }
        }
      }
    }
    out.push(bytes[i] as char);
    i += 1;
  }
  out
}

fn is_valid_key(s: &str) -> bool {
  s.chars()
    .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_durations() {
    assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
    assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
    assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
    assert_eq!(parse_duration("3").unwrap(), Duration::from_secs(3));
    assert!(parse_duration("bogus").is_err());
    assert!(parse_duration("-1s").is_err());
  }

  #[test]
  fn substitute() {
    let mut vars = HashMap::new();
    vars.insert("PORT".to_string(), "5432".to_string());
    vars.insert("HOST".to_string(), "localhost".to_string());
    assert_eq!(
      substitute_vars("nc -z %HOST% %PORT%", &vars),
      "nc -z localhost 5432"
    );
    // Unknown keys passed through
    assert_eq!(substitute_vars("%FOO%", &vars), "%FOO%");
    // Stray % at end
    assert_eq!(substitute_vars("100%", &vars), "100%");
    // No vars
    assert_eq!(substitute_vars("plain", &vars), "plain");
  }
}
