use std::fmt::Debug;

use crate::mprocs::proc_health::HookEvent;
use crate::term::{key::Key, mouse::MouseEvent};

#[derive(Debug)]
pub enum ProcMsg {
  SendKey(Key),
  SendMouse(MouseEvent),

  ScrollUp,
  ScrollDown,
  ScrollUpLines { n: usize },
  ScrollDownLines { n: usize },

  /// Manually re-run a lifecycle hook by event (e.g. "started"). The hook
  /// fires regardless of the proc's current status — useful for actions
  /// like the build-marker-reset trick where you may want to re-trigger
  /// the started hook to wipe a stale sentinel.
  RerunHook(HookEvent),
  /// Manually re-run a single healthcheck by index (parallel to the
  /// proc's `healthchecks` list).
  RerunCheck(usize),
}

#[derive(Debug)]
pub enum ProcEvent {
  Exited(u32),
  Started,
}
