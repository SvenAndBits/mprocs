use crate::console::proc::child::{ChildKind, ChildRow};
use crate::kernel::{
  kernel_message::SharedVt,
  task::{TaskId, TaskStatus},
  task_path::TaskPath,
};

pub struct ProcView {
  pub id: TaskId,
  pub name: String,
  pub path: Option<TaskPath>,
  pub deps: Vec<String>,

  pub status: TaskStatus,
  pub vt: SharedVt,
  pub present: Option<SharedVt>,

  pub changed: bool,

  pub children: Vec<ChildRow>,
  pub expanded: bool,
  pub focused_child: Option<usize>,
}

impl ProcView {
  pub fn new(
    id: TaskId,
    name: String,
    status: TaskStatus,
    vt: SharedVt,
    path: Option<TaskPath>,
    deps: Vec<String>,
  ) -> Self {
    Self {
      id,
      name,
      path,
      deps,
      status,
      vt,
      present: None,
      changed: false,
      children: Vec::new(),
      expanded: false,
      focused_child: None,
    }
  }

  pub fn set_name(&mut self, name: String) {
    self.name = name;
  }

  pub fn id(&self) -> TaskId {
    self.id
  }

  pub fn exit_code(&self) -> Option<u32> {
    match self.status {
      TaskStatus::Exited(code) => Some(code),
      _ => None,
    }
  }

  pub fn name(&self) -> &str {
    &self.name
  }

  pub fn is_up(&self) -> bool {
    match self.status {
      TaskStatus::Starting
      | TaskStatus::Running
      | TaskStatus::Unhealthy => true,
      TaskStatus::NotStarted | TaskStatus::Exited(_) => false,
    }
  }

  pub fn copy_active(&self) -> bool {
    self.present.is_some()
  }

  pub fn focus(&mut self) {
    self.changed = false;
  }

  pub fn focused_vt(&self) -> &SharedVt {
    match self.focused_child {
      Some(i) => self.children.get(i).map(|c| &c.vt).unwrap_or(&self.vt),
      None => self.present.as_ref().unwrap_or(&self.vt),
    }
  }

  pub fn focused_child_kind(&self) -> Option<ChildKind> {
    self
      .focused_child
      .and_then(|i| self.children.get(i))
      .map(|c| c.kind)
  }

  pub fn find_child_mut(
    &mut self,
    task_id: TaskId,
  ) -> Option<&mut ChildRow> {
    self
      .children
      .iter_mut()
      .find(|c| c.task_id == Some(task_id))
  }

  pub fn visible_row_count(&self) -> usize {
    if self.expanded {
      1 + self.children.len()
    } else {
      1
    }
  }
}
