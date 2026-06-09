use std::borrow::Cow;

use crate::kernel::task::TaskStatus;
use crate::mprocs::{
  proc::{
    CopyMode, Pos,
    children::ChildKind,
    view::{ProcView, ProcViewFrame},
  },
  state::{Scope, State},
};
use crate::term::{Color, Grid, Screen, attrs::Attrs, grid::Rect};

pub fn render_term(area: Rect, grid: &mut Grid, state: &mut State) {
  if area.width < 3 || area.height < 3 {
    return;
  }

  let active = match state.scope {
    Scope::Procs => false,
    Scope::Term | Scope::TermZoom => true,
  };

  // If the focused row in the sidebar is a synthetic `deps` child, the
  // right pane shows a composed list of dep statuses rather than any
  // captured VT. Resolve that here before the main render path.
  if let Some(proc) = state.get_current_proc() {
    if let Some(child_idx) = proc.focused_child {
      if let Some(child) = proc.children.get(child_idx) {
        if matches!(child.kind, ChildKind::Deps) {
          render_deps_view(area, grid, state, active);
          return;
        }
      }
    }
  }

  if let Some(proc) = state.get_current_proc() {
    let chars = match active {
      true => crate::term::grid::BorderType::Thick,
      false => crate::term::grid::BorderType::Plain,
    }
    .chars();
    grid.draw_block(area, &chars, crate::term::attrs::Attrs::default());

    let mut top_line = Rect {
      x: area.x + 1,
      y: area.y,
      width: area.width - 2,
      height: 1,
    };
    let r =
      grid.draw_text(top_line, "Terminal", Attrs::default().set_bold(active));
    top_line = top_line.move_left(r.width as i32);
    match proc.copy_mode() {
      CopyMode::None(_) => (),
      CopyMode::Active(_, _, _) => {
        let r = grid.draw_text(top_line, " ", Attrs::default());
        top_line = top_line.move_left(r.width as i32);
        let r = grid.draw_text(
          top_line,
          "COPY MODE",
          Attrs::default()
            .fg(Color::BLACK)
            .bg(Color::YELLOW)
            .set_bold(true),
        );
        top_line = top_line.move_left(r.width as i32);
      }
    };

    match &proc.lock_view() {
      ProcViewFrame::Empty => (),
      ProcViewFrame::Vt(vt) => {
        let title = vt.screen().title();
        if !title.is_empty() {
          let r = grid.draw_text(top_line, " ", Attrs::default());
          top_line = top_line.move_left(r.width as i32);
          let r = grid.draw_text(
            top_line,
            title,
            Attrs::default().fg(Color::BRIGHT_BLACK),
          );
          top_line = top_line.move_left(r.width as i32);
          let _r = grid.draw_text(top_line, " ", Attrs::default());
        }
        let (screen, cursor) = match proc.copy_mode() {
          CopyMode::None(_) => {
            let screen = vt.screen();
            let cursor = if screen.hide_cursor() {
              None
            } else {
              let cursor = screen.cursor_position();
              Some((area.x + 1 + cursor.1, area.y + 1 + cursor.0))
            };
            (screen, cursor)
          }
          CopyMode::Active(screen, start, end) => {
            let pos = end.as_ref().unwrap_or(start);
            let y = area.y as i32 + 1 + (pos.y + screen.scrollback() as i32);
            let cursor = if y >= 0 {
              Some((area.x + 1 + pos.x as u16, y as u16))
            } else {
              None
            };
            (screen, cursor)
          }
        };

        render_screen(screen, proc.copy_mode(), area.inner(1), grid);

        if active {
          if let Some(cursor) = cursor {
            grid.cursor_pos = Some(crate::term::grid::Pos {
              col: cursor.0,
              row: cursor.1,
            });
            grid.cursor_style = vt.screen().cursor_style();
          }
        }
      }
    }
  }
}

fn render_screen(
  screen: &Screen,
  copy_mode: &CopyMode,
  area: Rect,
  grid: &mut Grid,
) {
  for row in 0..area.height {
    for col in 0..area.width {
      let to_cell = if let Some(cell) =
        grid.drawing_cell_mut(crate::term::grid::Pos {
          col: area.x + col,
          row: area.y + row,
        }) {
        cell
      } else {
        continue;
      };
      if let Some(cell) = screen.cell(row, col) {
        *to_cell = cell.clone();
        if !cell.has_contents() {
          to_cell.set_str(" ");
        }

        let copy_mode = match copy_mode {
          CopyMode::None(_) => None,
          CopyMode::Active(_, start, end) => {
            Some((start, end.as_ref().unwrap_or(start)))
          }
        };
        if let Some((start, end)) = copy_mode {
          if Pos::within(
            start,
            end,
            &Pos {
              y: (row as i32) - screen.scrollback() as i32,
              x: col as i32,
            },
          ) {
            to_cell.set_attrs(
              Attrs::default()
                .fg(crate::term::Color::BLACK)
                .bg(crate::term::Color::CYAN),
            );
          }
        }
      } else {
        // Out of bounds — the VT is smaller than the displayed area
        // (typical for the small per-hook/per-check child VTs when the
        // term pane is taller than CHILD_VT_HEIGHT). Render blank.
        to_cell.set_str(" ");
      }
    }
  }

  let scrollback = screen.scrollback();
  if scrollback > 0 {
    let str = format!(" -{} ", scrollback);
    let width = str.len() as u16;
    let x = area.x + area.width - width;
    let y = area.y;
    grid.draw_text(
      Rect::new(x, y, width, 1),
      str.as_str(),
      Attrs::default().fg(Color::BLACK).bg(Color::BRIGHT_YELLOW),
    );
  }
}

pub fn term_check_hit(area: Rect, x: u16, y: u16) -> bool {
  area.x <= x
    && area.x + area.width >= x + 1
    && area.y <= y
    && area.y + area.height >= y + 1
}

/// Right-pane view shown when the synthetic `deps` child is focused. A
/// one-line header + one row per declared dep with its live status,
/// looked up against `state.procs`.
fn render_deps_view(area: Rect, grid: &mut Grid, state: &State, active: bool) {
  let chars = match active {
    true => crate::term::grid::BorderType::Thick,
    false => crate::term::grid::BorderType::Plain,
  }
  .chars();
  grid.draw_block(area, &chars, Attrs::default());

  let top_line = Rect {
    x: area.x + 1,
    y: area.y,
    width: area.width - 2,
    height: 1,
  };
  grid.draw_text(top_line, "Dependencies", Attrs::default().set_bold(active));

  let proc = match state.get_current_proc() {
    Some(p) => p,
    None => return,
  };

  let inner = area.inner(1);
  // Header: "Dependencies of <proc>"
  let header = format!("Dependencies of {}", proc.name());
  let header_row = Rect {
    x: inner.x,
    y: inner.y,
    width: inner.width,
    height: 1,
  };
  grid.draw_text(header_row, &header, Attrs::default().set_bold(true));

  // Compute column widths once: name column is max(name length) + a bit
  // of padding, capped at half the inner width so the pill always fits.
  let max_name_len = proc
    .cfg
    .deps
    .iter()
    .map(|n| n.chars().count())
    .max()
    .unwrap_or(0);
  let name_col_w =
    ((max_name_len + 4) as u16).min(inner.width.saturating_sub(20));

  for (i, dep_name) in proc.cfg.deps.iter().enumerate() {
    let y = inner.y + 2 + i as u16;
    if y >= inner.y + inner.height {
      break;
    }
    let dep = state.procs.iter().find(|p| p.cfg.name == *dep_name);
    let (symbol, sym_color, status_text, status_color) = match dep {
      None => (
        "?",
        Color::BRIGHT_RED,
        Cow::Borrowed("not found"),
        Color::BRIGHT_RED,
      ),
      Some(dep) => dep_row_visuals(dep),
    };

    let sym_rect = Rect {
      x: inner.x,
      y,
      width: 3,
      height: 1,
    };
    grid.draw_text(
      sym_rect,
      &format!(" {} ", symbol),
      Attrs::default().fg(sym_color).set_bold(true),
    );

    let name_rect = Rect {
      x: inner.x + 3,
      y,
      width: name_col_w,
      height: 1,
    };
    grid.draw_text(name_rect, dep_name.as_str(), Attrs::default());

    let status_x = inner.x + 3 + name_col_w + 1;
    if status_x < inner.x + inner.width {
      let status_rect = Rect {
        x: status_x,
        y,
        width: (inner.x + inner.width).saturating_sub(status_x),
        height: 1,
      };
      grid.draw_text(status_rect, &status_text, Attrs::default().fg(status_color));
    }
  }
}

fn dep_row_visuals(
  dep: &ProcView,
) -> (&'static str, Color, std::borrow::Cow<'static, str>, Color) {
  use std::borrow::Cow;
  match dep.status {
    TaskStatus::Running => ("✓", Color::BRIGHT_GREEN, Cow::Borrowed("UP"), Color::BRIGHT_GREEN),
    TaskStatus::Starting => {
      ("…", Color::BRIGHT_YELLOW, Cow::Borrowed("STARTING"), Color::BRIGHT_YELLOW)
    }
    TaskStatus::Unhealthy => {
      ("✗", Color::BRIGHT_RED, Cow::Borrowed("UNHEALTHY"), Color::BRIGHT_RED)
    }
    TaskStatus::NotStarted => (
      "—",
      Color::BRIGHT_BLACK,
      Cow::Borrowed("not started"),
      Color::BRIGHT_BLACK,
    ),
    TaskStatus::Exited(0) => {
      ("○", Color::BRIGHT_BLUE, Cow::Borrowed("DOWN (0)"), Color::BRIGHT_BLUE)
    }
    TaskStatus::Exited(code) => (
      "✗",
      Color::BRIGHT_RED,
      Cow::Owned(format!("DOWN ({})", code)),
      Color::BRIGHT_RED,
    ),
  }
}
