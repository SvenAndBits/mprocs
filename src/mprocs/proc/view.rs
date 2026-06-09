use crate::kernel::{
  kernel_message::SharedVt,
  task::{TaskId, TaskStatus},
};
use crate::mprocs::config::ProcConfig;

use super::children::ProcChild;
use super::CopyMode;

use std::time::Instant;

/// Amount of time a process has to stay up for autorestart to trigger
pub const RESTART_THRESHOLD_SECONDS: f64 = 1.0;

#[derive(Clone, Copy)]
pub enum TargetState {
  None,
  Started,
  Stopped,
}

pub struct ProcView {
  pub id: TaskId,
  pub cfg: ProcConfig,

  pub status: TaskStatus,
  pub vt: SharedVt,
  pub copy_mode: CopyMode,

  pub target_state: TargetState,
  pub last_start: Option<Instant>,
  pub changed: bool,

  /// Hooks + healthchecks as a flat list (rendered as a sub-tree in the
  /// sidebar). Order: hooks (in declaration order) then checks.
  pub children: Vec<ProcChild>,
  /// Whether the sidebar shows this proc's children. Default collapsed.
  pub expanded: bool,
  /// When the focus is inside this proc's child tree, which child row is
  /// selected. `None` means the proc row itself is focused.
  pub focused_child: Option<usize>,
}

impl ProcView {
  pub fn new(id: TaskId, cfg: ProcConfig, vt: SharedVt) -> Self {
    Self::new_with_children(id, cfg, vt, Vec::new())
  }

  pub fn new_with_children(
    id: TaskId,
    cfg: ProcConfig,
    vt: SharedVt,
    children: Vec<ProcChild>,
  ) -> Self {
    Self {
      id,
      cfg,

      status: TaskStatus::NotStarted,
      vt,
      copy_mode: CopyMode::None(None),

      target_state: TargetState::None,
      last_start: None,
      changed: false,

      children,
      expanded: false,
      focused_child: None,
    }
  }

  /// VT that should be rendered in the right-hand term pane for the
  /// current focus on this proc — either the proc's own VT or the
  /// focused child's VT.
  pub fn focused_vt(&self) -> &SharedVt {
    match self.focused_child {
      Some(i) => self
        .children
        .get(i)
        .map(|c| &c.vt)
        .unwrap_or(&self.vt),
      None => &self.vt,
    }
  }

  pub fn rename(&mut self, name: &str) {
    self.cfg.name.replace_range(.., name);
  }

  pub fn id(&self) -> TaskId {
    self.id
  }

  pub fn exit_code(&self) -> Option<u32> {
    match self.status {
      TaskStatus::NotStarted
      | TaskStatus::Starting
      | TaskStatus::Running
      | TaskStatus::Unhealthy => None,
      TaskStatus::Exited(code) => Some(code),
    }
  }

  pub fn lock_view(&'_ self) -> ProcViewFrame<'_> {
    self
      .focused_vt()
      .read()
      .map_or(ProcViewFrame::Empty, |vt| ProcViewFrame::Vt(vt))
  }

  pub fn name(&self) -> &str {
    &self.cfg.name
  }

  pub fn is_up(&self) -> bool {
    match self.status {
      TaskStatus::NotStarted | TaskStatus::Exited(_) => false,
      TaskStatus::Starting
      | TaskStatus::Running
      | TaskStatus::Unhealthy => true,
    }
  }

  pub fn is_starting(&self) -> bool {
    matches!(self.status, TaskStatus::Starting)
  }

  pub fn is_unhealthy(&self) -> bool {
    matches!(self.status, TaskStatus::Unhealthy)
  }

  pub fn copy_mode(&self) -> &CopyMode {
    &self.copy_mode
  }

  pub fn focus(&mut self) {
    self.changed = false;
  }
}

pub enum ProcViewFrame<'a> {
  Empty,
  Vt(std::sync::RwLockReadGuard<'a, crate::term::Parser>),
}
