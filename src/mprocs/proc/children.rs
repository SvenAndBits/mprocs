//! Per-proc child entries (hooks and healthchecks) used by the tree-style
//! sidebar to surface logs and per-item status.
//!
//! A `ProcChild` is purely a UI/state container. Its `vt` is shared with
//! the proc's main loop, which writes captured stdout+stderr from hook
//! and healthcheck command invocations.

use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::kernel::kernel_message::SharedVt;
use crate::kernel::task::TaskId;
use crate::mprocs::proc_health::HookEvent;
use crate::term::Parser;

/// Default dimensions for a per-child log VT. Hooks and checks emit a
/// handful of lines per run; a small width + decent scrollback is plenty.
pub const CHILD_VT_WIDTH: u16 = 200;
pub const CHILD_VT_HEIGHT: u16 = 24;
pub const CHILD_VT_SCROLLBACK: usize = 1000;

pub fn new_child_vt() -> SharedVt {
  SharedVt::new(Parser::new(
    CHILD_VT_HEIGHT,
    CHILD_VT_WIDTH,
    CHILD_VT_SCROLLBACK,
  ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ChildKind {
  Hook(HookEvent),
  /// Index into the proc's `healthchecks` list. Inline checks are
  /// addressable only by index; named checks duplicate the name in
  /// `ProcChild::name` for display purposes.
  Check(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildStatus {
  /// Never run yet.
  Idle,
  /// A command invocation is in progress.
  Running,
  /// Last invocation finished. For hooks: shell exit code. For checks:
  /// 0 = pass, non-zero = fail.
  LastExit(i32),
}

/// One row in the sidebar tree under a proc.
pub struct ProcChild {
  /// Kernel task id for this child (allows ctl/dekit addressing).
  pub task_id: TaskId,
  pub kind: ChildKind,
  /// Display label (e.g. "started", "is_port_open", "check[2]").
  pub name: String,
  pub vt: SharedVt,
  pub status: ChildStatus,
  /// Most recent non-Running status. Used by the UI to suppress
  /// brief RUN flickers on fast checks (Running → LastExit in <300ms)
  /// by continuing to show the previous stable pill.
  pub last_stable_status: ChildStatus,
  /// When the current status was set. Used together with
  /// `last_stable_status` for the debounce above — if status is
  /// Running and changed_at is fresh (< debounce window), the UI
  /// renders `last_stable_status` instead.
  pub status_changed_at: Option<Instant>,
}

/// How long a check must have been in `Running` before we actually
/// render the `RUN` pill. Fast checks (Running→LastExit under this
/// window) keep showing their previous stable pill to avoid visual
/// flicker.
pub const RUN_PILL_DEBOUNCE_MS: u64 = 300;

impl ProcChild {
  /// What the sidebar should display for this child — applies the
  /// RUN-flicker debounce.
  pub fn displayed_status(&self) -> ChildStatus {
    if matches!(self.status, ChildStatus::Running) {
      if let Some(changed_at) = self.status_changed_at {
        if changed_at.elapsed().as_millis()
          < RUN_PILL_DEBOUNCE_MS as u128
        {
          return self.last_stable_status;
        }
      }
    }
    self.status
  }
}

/// Feed bytes into a per-child VT, catching any panic the vt100 parser
/// might throw on unusual input (the parser has out-of-bounds asserts
/// that have been observed to fire on certain ANSI sequences from
/// captured subprocess output). A panic drops the offending frame
/// rather than taking the tokio runtime down.
pub fn vt_process_safe(vt: &SharedVt, bytes: &[u8]) {
  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    if let Ok(mut p) = vt.write() {
      let mut events = Vec::new();
      p.screen.process(bytes, &mut events);
    }
  }));
  if result.is_err() {
    log::warn!(
      "vt parser panicked on child output ({} bytes); frame dropped",
      bytes.len()
    );
  }
}

/// Thread-safe handle that the proc's main loop and the per-check tokio
/// tasks share, so output-capture writes can be routed without touching
/// the ProcView (which lives in the UI thread).
pub type ChildVtHandle = Arc<RwLock<ChildVtInner>>;

pub struct ChildVtInner {
  pub vt: SharedVt,
}

pub fn make_child_vt_handle(vt: SharedVt) -> ChildVtHandle {
  Arc::new(RwLock::new(ChildVtInner { vt }))
}
