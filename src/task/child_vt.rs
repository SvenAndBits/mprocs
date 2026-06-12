use crate::kernel::kernel_message::SharedVt;
use crate::term::Parser;

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
