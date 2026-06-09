use std::borrow::Cow;

use crate::console::proc::child::ChildKind;
use crate::console::proc::view::ProcView;
use crate::console::state::{Scope, State};
use crate::kernel::task::TaskStatus;
use crate::term::{Color, Grid, Screen, attrs::Attrs, grid::Rect};

pub fn render_term(area: Rect, grid: &mut Grid, state: &mut State) {
  if area.width < 3 || area.height < 3 {
    return;
  }

  let active = match state.scope {
    Scope::Procs => false,
    Scope::Term | Scope::TermZoom => true,
  };

  if let Some(proc) = state.get_current_proc()
    && matches!(proc.focused_child_kind(), Some(ChildKind::Deps))
  {
    render_deps_view(area, grid, state, active);
    return;
  }

  let Some(proc) = state.get_current_proc() else {
    return;
  };

  let chars = match active {
    true => crate::term::grid::BorderType::Thick,
    false => crate::term::grid::BorderType::Plain,
  }
  .chars();
  grid.draw_block(area, &chars, Attrs::default());

  let handle = proc.focused_vt();
  let Ok(parser) = handle.read() else {
    return;
  };
  let screen = parser.screen();

  let mut top_line = Rect {
    x: area.x + 1,
    y: area.y,
    width: area.width - 2,
    height: 1,
  };
  let r =
    grid.draw_text(top_line, "Terminal", Attrs::default().set_bold(active));
  top_line = top_line.move_left(r.width as i32);
  let title = screen.title();
  if !title.is_empty() {
    let r = grid.draw_text(top_line, " ", Attrs::default());
    top_line = top_line.move_left(r.width as i32);
    let _r =
      grid.draw_text(top_line, title, Attrs::default().fg(Color::BRIGHT_BLACK));
  }

  let inner = area.inner(1);
  render_screen(screen, inner, grid);

  if active && !screen.hide_cursor() {
    let (row, col) = screen.cursor_position();
    grid.cursor_pos = Some(crate::term::grid::Pos {
      col: inner.x + col,
      row: inner.y + row,
    });
    grid.cursor_style = screen.cursor_style();
  }
}

fn render_screen(screen: &Screen, area: Rect, grid: &mut Grid) {
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
      }
    }
  }
}

pub fn term_check_hit(area: Rect, x: u16, y: u16) -> bool {
  area.x <= x
    && area.x + area.width >= x + 1
    && area.y <= y
    && area.y + area.height >= y + 1
}

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
  let header = format!("Dependencies of {}", proc.name());
  let header_row = Rect {
    x: inner.x,
    y: inner.y,
    width: inner.width,
    height: 1,
  };
  grid.draw_text(header_row, &header, Attrs::default().set_bold(true));

  let max_name_len = proc
    .deps
    .iter()
    .map(|n| n.chars().count())
    .max()
    .unwrap_or(0);
  let name_col_w =
    ((max_name_len + 4) as u16).min(inner.width.saturating_sub(20));

  for (i, dep_name) in proc.deps.iter().enumerate() {
    let y = inner.y + 2 + i as u16;
    if y >= inner.y + inner.height {
      break;
    }
    let dep = state.procs.iter().find(|p| p.name() == dep_name);
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
      grid.draw_text(
        status_rect,
        &status_text,
        Attrs::default().fg(status_color),
      );
    }
  }
}

fn dep_row_visuals(
  dep: &ProcView,
) -> (&'static str, Color, Cow<'static, str>, Color) {
  match dep.status {
    TaskStatus::Running => {
      ("✓", Color::BRIGHT_GREEN, Cow::Borrowed("UP"), Color::BRIGHT_GREEN)
    }
    TaskStatus::Starting => (
      "…",
      Color::BRIGHT_YELLOW,
      Cow::Borrowed("STARTING"),
      Color::BRIGHT_YELLOW,
    ),
    TaskStatus::Unhealthy => (
      "✗",
      Color::BRIGHT_RED,
      Cow::Borrowed("UNHEALTHY"),
      Color::BRIGHT_RED,
    ),
    TaskStatus::Completed => (
      "✓",
      Color::BRIGHT_GREEN,
      Cow::Borrowed("DONE"),
      Color::BRIGHT_GREEN,
    ),
    TaskStatus::NotStarted => (
      "—",
      Color::BRIGHT_BLACK,
      Cow::Borrowed("not started"),
      Color::BRIGHT_BLACK,
    ),
    TaskStatus::Exited(0) => (
      "○",
      Color::BRIGHT_BLUE,
      Cow::Borrowed("DOWN (0)"),
      Color::BRIGHT_BLUE,
    ),
    TaskStatus::Exited(code) => (
      "✗",
      Color::BRIGHT_RED,
      Cow::Owned(format!("DOWN ({})", code)),
      Color::BRIGHT_RED,
    ),
  }
}
