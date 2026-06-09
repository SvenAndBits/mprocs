use std::time::Instant;

use crate::config::health::HookEvent;
use crate::kernel::kernel_message::SharedVt;
use crate::kernel::task::{TaskId, TaskStatus};
use crate::kernel::task_path::TaskPath;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ChildKind {
  Hook(HookEvent),
  Check(usize),
  Deps,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStatus {
  Idle,
  Running,
  LastExit(i32),
}

pub const RUN_PILL_DEBOUNCE_MS: u64 = 300;

pub struct ChildRow {
  pub task_id: Option<TaskId>,
  pub kind: ChildKind,
  pub name: String,
  pub vt: SharedVt,
  pub status: ChildStatus,
  pub last_stable_status: ChildStatus,
  pub status_changed_at: Option<Instant>,
}

impl ChildRow {
  pub fn new(
    task_id: Option<TaskId>,
    kind: ChildKind,
    name: String,
    vt: SharedVt,
  ) -> Self {
    Self {
      task_id,
      kind,
      name,
      vt,
      status: ChildStatus::Idle,
      last_stable_status: ChildStatus::Idle,
      status_changed_at: None,
    }
  }

  pub fn set_status(&mut self, status: ChildStatus) {
    if !matches!(self.status, ChildStatus::Running) {
      self.last_stable_status = self.status;
    }
    self.status = status;
    self.status_changed_at = Some(Instant::now());
  }

  pub fn apply_task_status(&mut self, status: TaskStatus) {
    let mapped = match status {
      TaskStatus::NotStarted => ChildStatus::Idle,
      TaskStatus::Starting | TaskStatus::Running => ChildStatus::Running,
      TaskStatus::Unhealthy => ChildStatus::LastExit(1),
      TaskStatus::Completed => ChildStatus::LastExit(0),
      TaskStatus::Exited(code) => ChildStatus::LastExit(code as i32),
    };
    self.set_status(mapped);
  }

  pub fn displayed_status(&self) -> ChildStatus {
    if matches!(self.status, ChildStatus::Running)
      && let Some(changed_at) = self.status_changed_at
      && changed_at.elapsed().as_millis() < RUN_PILL_DEBOUNCE_MS as u128
    {
      return self.last_stable_status;
    }
    self.status
  }
}

pub fn child_kind_from_segment(seg: &str) -> Option<ChildKind> {
  if let Some(idx) = seg.strip_prefix("check_") {
    return idx.parse::<usize>().ok().map(ChildKind::Check);
  }
  if let Some(event) = seg.strip_prefix("hook_") {
    return HookEvent::parse(event).map(ChildKind::Hook);
  }
  None
}

pub fn is_child_path(path: &TaskPath) -> bool {
  path.depth() >= 2
}
