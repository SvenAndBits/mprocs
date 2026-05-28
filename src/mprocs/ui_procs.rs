use std::borrow::Cow;

use unicode_width::UnicodeWidthStr;

use crate::kernel::task::TaskStatus;
use crate::mprocs::{
  config::Config,
  proc::children::{ChildKind, ChildStatus, ProcChild},
  proc::view::ProcView,
  state::{Scope, State},
};
use crate::term::{
  Color, Grid,
  attrs::Attrs,
  grid::{BorderType, Rect},
};

pub fn render_procs(
  area: Rect,
  grid: &mut Grid,
  state: &mut State,
  config: &Config,
) {
  state.procs_list.fit(area.inner(1), state.procs.len());

  if area.width <= 2 {
    return;
  }

  let active = state.scope == Scope::Procs;

  grid.draw_block(
    area.into(),
    &if active {
      BorderType::Thick
    } else {
      BorderType::Plain
    }
    .chars(),
    Attrs::default(),
  );
  let title_area = Rect {
    x: area.x + 1,
    y: area.y,
    width: area.width - 2,
    height: 1,
  };
  let r = grid.draw_text(
    title_area,
    config.proc_list_title.as_str(),
    if active {
      Attrs::default().set_bold(true)
    } else {
      Attrs::default()
    },
  );
  if state.quitting {
    let area = title_area.inner((0, 0, 0, r.width + 1));
    grid.draw_text(
      area,
      "QUITTING",
      Attrs::default()
        .fg(Color::BLACK)
        .bg(Color::RED)
        .set_bold(true),
    );
  }

  // Render: walk the proc list, emit one row per proc, plus child rows
  // immediately under each expanded proc. We render until we run out of
  // vertical space; pagination tracks procs (not rows), so selection
  // stays on the proc the user picked even if children push things down.
  let inner = area.inner(1);
  let mut y_cursor = inner.y;
  let y_max = inner.y + inner.height;
  let range = state.procs_list.visible_range();
  for proc_idx in range {
    if y_cursor >= y_max {
      break;
    }
    let proc = match state.procs.get(proc_idx) {
      Some(p) => p,
      None => continue,
    };
    let selected = proc_idx == state.selected();
    let row_area = Rect {
      x: area.x + 1,
      y: y_cursor,
      width: area.width.saturating_sub(2),
      height: 1,
    };
    render_proc_row(grid, row_area, proc, selected);
    y_cursor += 1;

    if proc.expanded {
      for child in &proc.children {
        if y_cursor >= y_max {
          break;
        }
        let row_area = Rect {
          x: area.x + 1,
          y: y_cursor,
          width: area.width.saturating_sub(2),
          height: 1,
        };
        render_child_row(grid, row_area, child);
        y_cursor += 1;
      }
    }
  }
}

fn render_proc_row(
  grid: &mut Grid,
  area: Rect,
  proc: &ProcView,
  selected: bool,
) {
  let attrs = if selected {
    Attrs::default().bg(Color::Idx(240))
  } else {
    Attrs::default()
  };
  let mut row_area = area;

  let r = grid.draw_text(row_area, if selected { "•" } else { " " }, attrs);
  row_area.x += r.width;
  row_area.width = row_area.width.saturating_sub(r.width);

  let chevron = if !proc.children.is_empty() {
    if proc.expanded { "▼ " } else { "▶ " }
  } else {
    "  "
  };
  let r = grid.draw_text(
    row_area,
    chevron,
    attrs.clone().fg(Color::BRIGHT_BLACK),
  );
  row_area.x += r.width;
  row_area.width = row_area.width.saturating_sub(r.width);

  let r = grid.draw_text(row_area, proc.name(), attrs);
  row_area.x += r.width;
  row_area.width = row_area.width.saturating_sub(r.width);

  let (status_text, status_attrs) = status_pill_for_proc(proc, attrs);
  draw_right_aligned_pill(grid, row_area, &status_text, status_attrs, attrs);
}

fn render_child_row(grid: &mut Grid, area: Rect, child: &ProcChild) {
  let attrs = Attrs::default();
  let mut row_area = area;

  // Selection-marker column stays blank for child rows (not selectable in
  // this iteration).
  let r = grid.draw_text(row_area, " ", attrs);
  row_area.x += r.width;
  row_area.width = row_area.width.saturating_sub(r.width);

  let r = grid.draw_text(
    row_area,
    "  ├─ ",
    attrs.clone().fg(Color::BRIGHT_BLACK),
  );
  row_area.x += r.width;
  row_area.width = row_area.width.saturating_sub(r.width);

  let kind_label = match child.kind {
    ChildKind::Hook(_) => "hook:",
    ChildKind::Check(_) => "check:",
  };
  let r = grid.draw_text(
    row_area,
    kind_label,
    attrs.clone().fg(Color::BRIGHT_BLACK),
  );
  row_area.x += r.width;
  row_area.width = row_area.width.saturating_sub(r.width);
  let r = grid.draw_text(row_area, child.name.as_str(), attrs);
  row_area.x += r.width;
  row_area.width = row_area.width.saturating_sub(r.width);

  let (text, pill_attrs) = status_pill_for_child(child, attrs);
  draw_right_aligned_pill(grid, row_area, &text, pill_attrs, attrs);
}

fn draw_right_aligned_pill(
  grid: &mut Grid,
  area: Rect,
  text: &str,
  attrs: Attrs,
  base_attrs: Attrs,
) {
  let w = text.width() as u16;
  grid.draw_text(
    Rect {
      x: area.x.max(area.x + area.width - w),
      width: w.min(area.width),
      ..area
    },
    text,
    attrs,
  );
  let remaining = area.width.saturating_sub(w.min(area.width));
  let bg_area = Rect {
    width: remaining,
    ..area
  };
  grid.fill_area(bg_area, ' ', base_attrs);
}

fn status_pill_for_proc<'a>(
  proc: &ProcView,
  mut base: Attrs,
) -> (Cow<'a, str>, Attrs) {
  if matches!(proc.status, TaskStatus::Starting) {
    return (
      Cow::from(" STARTING "),
      base.set_bold(true).fg(Color::BRIGHT_YELLOW),
    );
  }
  if matches!(proc.status, TaskStatus::Unhealthy) {
    return (
      Cow::from(" UNHEALTHY "),
      base.set_bold(true).fg(Color::BRIGHT_RED),
    );
  }
  if proc.is_up() {
    return (
      Cow::from(" UP "),
      base.set_bold(true).fg(Color::BRIGHT_GREEN),
    );
  }
  match proc.exit_code() {
    Some(0) => (Cow::from(" DOWN (0)"), base.fg(Color::BRIGHT_BLUE)),
    Some(code) => (
      Cow::from(format!(" DOWN ({})", code)),
      base.fg(Color::BRIGHT_RED),
    ),
    None => (Cow::from(" DOWN "), base.fg(Color::BRIGHT_BLACK)),
  }
}

fn status_pill_for_child<'a>(
  child: &ProcChild,
  mut base: Attrs,
) -> (Cow<'a, str>, Attrs) {
  match child.status {
    ChildStatus::Idle => (Cow::from(" — "), base.fg(Color::BRIGHT_BLACK)),
    ChildStatus::Running => (
      Cow::from(" RUN "),
      base.set_bold(true).fg(Color::BRIGHT_YELLOW),
    ),
    ChildStatus::LastExit(0) => (
      Cow::from(" ✓ "),
      base.set_bold(true).fg(Color::BRIGHT_GREEN),
    ),
    ChildStatus::LastExit(_) => (
      Cow::from(" ✗ "),
      base.set_bold(true).fg(Color::BRIGHT_RED),
    ),
  }
}

pub fn procs_get_clicked_index(
  area: Rect,
  x: u16,
  y: u16,
  state: &State,
) -> Option<usize> {
  let inner = area.inner(1);
  if procs_check_hit(area, x, y) {
    let index = y - inner.y;
    let scroll = (state.selected() + 1).saturating_sub(inner.height as usize);
    let index = index as usize + scroll;
    if index < state.procs.len() {
      return Some(index);
    }
  }
  None
}

pub fn procs_check_hit(area: Rect, x: u16, y: u16) -> bool {
  area.x < x
    && area.x + area.width > x + 1
    && area.y < y
    && area.y + area.height > y + 1
}
