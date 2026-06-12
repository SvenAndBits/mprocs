use std::collections::HashMap;

use crate::console::dashboard::layout::{GraphLayout, layout_graph};
use crate::console::proc::view::ProcView;
use crate::console::state::{Scope, State};
use crate::kernel::task::TaskStatus;
use crate::term::grid::{BorderType, Pos, Rect};
use crate::term::{Color, Grid, attrs::Attrs};

pub fn render_dashboard(area: Rect, grid: &mut Grid, state: &State) {
  if area.width < 3 || area.height < 3 {
    return;
  }

  let active = matches!(state.scope, Scope::Term | Scope::TermZoom);
  let chars = if active {
    BorderType::Thick
  } else {
    BorderType::Plain
  }
  .chars();
  grid.draw_block(area, &chars, Attrs::default());

  let title_area = Rect {
    x: area.x + 1,
    y: area.y,
    width: area.width - 2,
    height: 1,
  };
  grid.draw_text(title_area, "Dashboard", Attrs::default().set_bold(active));

  let inner = area.inner(1);
  if state.procs.is_empty() {
    grid.draw_text(
      Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
      },
      "No processes",
      Attrs::default().fg(Color::BRIGHT_BLACK),
    );
    return;
  }

  let graph = layout_graph(&state.procs);
  let scroll_col = state.dashboard.scroll_col;
  let scroll_row = state.dashboard.scroll_row;

  draw_edges(grid, inner, &graph, scroll_col, scroll_row);
  draw_nodes(grid, inner, &graph, state, scroll_col, scroll_row);
}

fn draw_nodes(
  grid: &mut Grid,
  inner: Rect,
  graph: &GraphLayout,
  state: &State,
  scroll_col: i32,
  scroll_row: i32,
) {
  for node in &graph.nodes {
    let Some(proc) = state.procs.get(node.proc_idx) else {
      continue;
    };
    let screen_y = inner.y as i32 + node.rect.y as i32 - scroll_row;
    if screen_y < inner.y as i32 || screen_y >= (inner.y + inner.height) as i32
    {
      continue;
    }
    let screen_x = inner.x as i32 + node.rect.x as i32 - scroll_col;

    let focused = state.dashboard.focused_node == Some(node.proc_idx);
    let (glyph, color) = status_visual(proc, &state.procs);
    let label = format!(" {} {} ", glyph, proc.name());

    let base = if focused {
      Attrs::default().bg(Color::Idx(240)).set_bold(true)
    } else {
      Attrs::default()
    };

    for (offset, g) in label.chars().enumerate() {
      let col = screen_x + offset as i32;
      if col < inner.x as i32 || col >= (inner.x + inner.width) as i32 {
        continue;
      }
      let attrs = if offset == 1 {
        base.clone().fg(color)
      } else {
        base.clone()
      };
      if let Some(cell) = grid.drawing_cell_mut(Pos {
        col: col as u16,
        row: screen_y as u16,
      }) {
        cell.set(g, attrs);
      }
    }
  }
}

const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

fn draw_edges(
  grid: &mut Grid,
  inner: Rect,
  graph: &GraphLayout,
  scroll_col: i32,
  scroll_row: i32,
) {
  let mut segments: HashMap<(i32, i32), u8> = HashMap::new();
  let mut add = |col: i32, row: i32, bits: u8| {
    *segments.entry((col, row)).or_insert(0) |= bits;
  };

  for edge in &graph.edges {
    let (Some(from), Some(to)) =
      (graph.node_for_proc(edge.from), graph.node_for_proc(edge.to))
    else {
      continue;
    };
    let src_col = from.rect.x as i32 + from.rect.width as i32;
    let src_row = from.rect.y as i32;
    let tgt_col = to.rect.x as i32;
    let tgt_row = to.rect.y as i32;
    let channel = (tgt_col - 2).max(src_col);

    if src_row == tgt_row {
      for col in src_col..tgt_col {
        add(col, src_row, LEFT | RIGHT);
      }
      continue;
    }

    for col in src_col..channel {
      add(col, src_row, LEFT | RIGHT);
    }
    let down = tgt_row > src_row;
    add(channel, src_row, LEFT | if down { DOWN } else { UP });
    let (lo, hi) = (src_row.min(tgt_row), src_row.max(tgt_row));
    for row in lo + 1..hi {
      add(channel, row, UP | DOWN);
    }
    add(channel, tgt_row, RIGHT | if down { UP } else { DOWN });
    for col in channel + 1..tgt_col {
      add(col, tgt_row, LEFT | RIGHT);
    }
  }

  let attrs = Attrs::default().fg(Color::BRIGHT_BLACK);
  for ((col, row), bits) in &segments {
    let sx = inner.x as i32 + col - scroll_col;
    let sy = inner.y as i32 + row - scroll_row;
    if sx < inner.x as i32 || sx >= (inner.x + inner.width) as i32 {
      continue;
    }
    if sy < inner.y as i32 || sy >= (inner.y + inner.height) as i32 {
      continue;
    }
    if let Some(cell) = grid.drawing_cell_mut(Pos {
      col: sx as u16,
      row: sy as u16,
    }) {
      cell.set(junction(*bits), attrs);
    }
  }
}

fn junction(bits: u8) -> char {
  match bits {
    b if b == LEFT | RIGHT => '─',
    b if b == UP | DOWN => '│',
    b if b == DOWN | RIGHT => '┌',
    b if b == DOWN | LEFT => '┐',
    b if b == UP | RIGHT => '└',
    b if b == UP | LEFT => '┘',
    b if b == UP | DOWN | RIGHT => '├',
    b if b == UP | DOWN | LEFT => '┤',
    b if b == LEFT | RIGHT | DOWN => '┬',
    b if b == LEFT | RIGHT | UP => '┴',
    b if b == UP | DOWN | LEFT | RIGHT => '┼',
    b if b & (UP | DOWN) != 0 => '│',
    _ => '─',
  }
}

fn status_visual(proc: &ProcView, all_procs: &[ProcView]) -> (char, Color) {
  match proc.status {
    TaskStatus::Starting => ('◐', Color::BRIGHT_YELLOW),
    TaskStatus::Unhealthy => ('●', Color::BRIGHT_RED),
    TaskStatus::Completed => ('✓', Color::BRIGHT_GREEN),
    TaskStatus::Running => ('●', Color::BRIGHT_GREEN),
    TaskStatus::Exited(0) => ('○', Color::BRIGHT_BLUE),
    TaskStatus::Exited(_) => ('✗', Color::BRIGHT_RED),
    TaskStatus::NotStarted if waiting_for_deps(proc, all_procs) => {
      ('○', Color::BRIGHT_YELLOW)
    }
    TaskStatus::NotStarted => ('○', Color::BRIGHT_BLACK),
  }
}

fn waiting_for_deps(proc: &ProcView, all_procs: &[ProcView]) -> bool {
  if proc.deps.is_empty() {
    return false;
  }
  proc.deps.iter().any(|dep_name| {
    match all_procs.iter().find(|p| p.name() == dep_name) {
      Some(dep) => {
        !matches!(dep.status, TaskStatus::Running | TaskStatus::Completed)
      }
      None => true,
    }
  })
}

pub fn node_at(
  area: Rect,
  state: &State,
  x: u16,
  y: u16,
) -> Option<usize> {
  if state.procs.is_empty() {
    return None;
  }
  let inner = area.inner(1);
  let graph = layout_graph(&state.procs);
  let scroll_col = state.dashboard.scroll_col;
  let scroll_row = state.dashboard.scroll_row;
  for node in &graph.nodes {
    let screen_x = inner.x as i32 + node.rect.x as i32 - scroll_col;
    let screen_y = inner.y as i32 + node.rect.y as i32 - scroll_row;
    if y as i32 == screen_y
      && x as i32 >= screen_x
      && (x as i32) < screen_x + node.rect.width as i32
    {
      return Some(node.proc_idx);
    }
  }
  None
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::console::dashboard::layout::layout_from;
  use crate::term::Size;
  use crate::term::grid::Pos;

  fn render_to_string(
    names: &[&str],
    deps: &[Vec<usize>],
    w: u16,
    h: u16,
  ) -> String {
    let graph = layout_from(names, deps);
    let mut grid = Grid::new(Size { width: w, height: h }, 0);
    let inner = Rect::new(0, 0, w, h);
    draw_edges(&mut grid, inner, &graph, 0, 0);
    for node in &graph.nodes {
      let label = format!(" ○ {} ", names[node.proc_idx]);
      grid.draw_text(node.rect, &label, Attrs::default());
    }
    let mut out = String::new();
    for row in 0..h {
      for col in 0..w {
        let ch = grid
          .drawing_cell(Pos { col, row })
          .map(|c| c.contents().to_string())
          .filter(|s| !s.is_empty())
          .unwrap_or_else(|| " ".to_string());
        out.push_str(&ch);
      }
      out.push('\n');
    }
    out
  }

  #[test]
  fn graph_renders_connected_lines() {
    let names = ["db", "api", "web", "worker", "logger", "cache"];
    let deps = vec![
      vec![],
      vec![0],
      vec![1],
      vec![0],
      vec![],
      vec![0, 1],
    ];
    let art = render_to_string(&names, &deps, 60, 12);
    println!("\n{}", art);
    assert!(art.contains('○'));
    assert!(art.contains('─') && art.contains('│'));
  }
}
