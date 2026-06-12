use std::time::Duration;

use anyhow::{Result, bail};
use indexmap::IndexMap;

use crate::cfg::CfgNode;

pub type Vars = IndexMap<String, String>;

#[derive(Clone, Debug)]
pub struct HealthCheckDef {
  pub name: String,
  pub cmd: String,
  pub interval: Duration,
  pub timeout: Duration,
  pub start_period: Duration,
  pub retries: u32,
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

  pub fn from_node(node: &CfgNode<'_>) -> Result<Self> {
    let obj = node.as_obj()?;
    let mut def = Self::defaults();
    def.cmd = obj
      .get("cmd")
      .ok_or_else(|| node.error("healthcheck requires `cmd`"))?
      .as_str()?
      .to_owned();
    if let Some(v) = obj.get("interval") {
      def.interval = parse_duration(v.as_str()?)
        .map_err(|e| v.error(format!("invalid interval: {e}")))?;
    }
    if let Some(v) = obj.get("timeout") {
      def.timeout = parse_duration(v.as_str()?)
        .map_err(|e| v.error(format!("invalid timeout: {e}")))?;
    }
    if let Some(v) = obj.get("start_period") {
      def.start_period = parse_duration(v.as_str()?)
        .map_err(|e| v.error(format!("invalid start_period: {e}")))?;
    }
    if let Some(v) = obj.get("retries") {
      def.retries = v.as_usize()? as u32;
    }
    if let Some(v) = obj.get("min_passes") {
      def.min_passes = (v.as_usize()? as u32).max(1);
    }
    Ok(def)
  }
}

pub type HealthCheckRegistry = IndexMap<String, HealthCheckDef>;

pub fn parse_registry(node: &CfgNode<'_>) -> Result<HealthCheckRegistry> {
  let obj = node.as_obj()?;
  let mut out = HealthCheckRegistry::new();
  for (k, v) in obj.iter() {
    let mut def = HealthCheckDef::from_node(&v)?;
    def.name = k.to_owned();
    out.insert(k.to_owned(), def);
  }
  Ok(out)
}

pub fn parse_proc_healthchecks(
  node: &CfgNode<'_>,
  registry: &HealthCheckRegistry,
) -> Result<Vec<HealthCheckDef>> {
  let arr = node.as_arr()?;
  let mut out = Vec::with_capacity(arr.len());
  let mut inline_seq = 0usize;
  for item in arr.iter() {
    if item.is_string() {
      let name = item.as_str()?;
      let def = registry.get(name).cloned().ok_or_else(|| {
        item.error(format!("unknown healthcheck `{name}`"))
      })?;
      out.push(def);
    } else if item.is_mapping() {
      let mut def = HealthCheckDef::from_node(&item)?;
      def.name = format!("inline[{inline_seq}]");
      inline_seq += 1;
      out.push(def);
    } else {
      bail!(
        item.error("expected healthcheck name (string) or inline mapping")
      );
    }
  }
  Ok(out)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HookEvent {
  Started,
  Running,
  Unhealthy,
  Stopped,
  Failed,
}

impl HookEvent {
  pub fn parse(s: &str) -> Option<Self> {
    match s {
      "started" => Some(Self::Started),
      "running" => Some(Self::Running),
      "unhealthy" => Some(Self::Unhealthy),
      "stopped" => Some(Self::Stopped),
      "failed" => Some(Self::Failed),
      _ => None,
    }
  }

  pub fn label(self) -> &'static str {
    match self {
      Self::Started => "started",
      Self::Running => "running",
      Self::Unhealthy => "unhealthy",
      Self::Stopped => "stopped",
      Self::Failed => "failed",
    }
  }
}

#[derive(Clone, Debug)]
pub struct HookDef {
  pub cmd: String,
  pub async_: bool,
}

impl HookDef {
  pub fn from_node(node: &CfgNode<'_>) -> Result<Self> {
    let obj = node.as_obj()?;
    let cmd = obj
      .get("cmd")
      .ok_or_else(|| node.error("hook requires `cmd`"))?
      .as_str()?
      .to_owned();
    let async_ = match obj.get("async") {
      Some(v) => v.as_bool()?,
      None => false,
    };
    Ok(Self { cmd, async_ })
  }
}

pub type HookRegistry = IndexMap<String, HookDef>;

pub fn parse_hook_registry(node: &CfgNode<'_>) -> Result<HookRegistry> {
  let obj = node.as_obj()?;
  let mut out = HookRegistry::new();
  for (k, v) in obj.iter() {
    out.insert(k.to_owned(), HookDef::from_node(&v)?);
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

  fn set(&mut self, event: HookEvent, def: HookDef) {
    match event {
      HookEvent::Started => self.started = Some(def),
      HookEvent::Running => self.running = Some(def),
      HookEvent::Unhealthy => self.unhealthy = Some(def),
      HookEvent::Stopped => self.stopped = Some(def),
      HookEvent::Failed => self.failed = Some(def),
    }
  }

  pub fn overlay(self, over: HookSet) -> HookSet {
    HookSet {
      started: over.started.or(self.started),
      running: over.running.or(self.running),
      unhealthy: over.unhealthy.or(self.unhealthy),
      stopped: over.stopped.or(self.stopped),
      failed: over.failed.or(self.failed),
    }
  }

  pub fn is_empty(&self) -> bool {
    self.started.is_none()
      && self.running.is_none()
      && self.unhealthy.is_none()
      && self.stopped.is_none()
      && self.failed.is_none()
  }
}

pub fn parse_hooks(
  node: &CfgNode<'_>,
  registry: &HookRegistry,
) -> Result<HookSet> {
  let obj = node.as_obj()?;
  let mut out = HookSet::default();
  for (k, v) in obj.iter() {
    let event = HookEvent::parse(k)
      .ok_or_else(|| v.error(format!("unknown hook event `{k}`")))?;
    let def = if v.is_string() {
      let ref_name = v.as_str()?;
      registry
        .get(ref_name)
        .cloned()
        .ok_or_else(|| v.error(format!("unknown hook `{ref_name}`")))?
    } else if v.is_mapping() {
      HookDef::from_node(&v)?
    } else {
      bail!(v.error("hook must be a name (string) or inline mapping"));
    };
    out.set(event, def);
  }
  Ok(out)
}

pub fn parse_duration(s: &str) -> Result<Duration> {
  let s = s.trim();
  if s.is_empty() {
    bail!("empty duration");
  }
  let (num_str, unit) =
    if let Some(idx) = s.find(|c: char| c.is_alphabetic()) {
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

pub fn substitute_vars(s: &str, vars: &Vars) -> String {
  if vars.is_empty() || !s.contains('%') {
    return s.to_owned();
  }
  let bytes = s.as_bytes();
  let mut out = String::with_capacity(s.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%'
      && let Some(end) = bytes[i + 1..].iter().position(|&b| b == b'%')
    {
      let key = &s[i + 1..i + 1 + end];
      if !key.is_empty()
        && is_valid_key(key)
        && let Some(val) = vars.get(key)
      {
        out.push_str(val);
        i += 2 + end;
        continue;
      }
    }
    out.push(bytes[i] as char);
    i += 1;
  }
  out
}

fn is_valid_key(s: &str) -> bool {
  s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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
    let mut vars = Vars::new();
    vars.insert("PORT".to_string(), "5432".to_string());
    vars.insert("HOST".to_string(), "localhost".to_string());
    assert_eq!(
      substitute_vars("nc -z %HOST% %PORT%", &vars),
      "nc -z localhost 5432"
    );
    assert_eq!(substitute_vars("%FOO%", &vars), "%FOO%");
    assert_eq!(substitute_vars("100%", &vars), "100%");
    assert_eq!(substitute_vars("plain", &vars), "plain");
  }
}
