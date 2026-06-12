use unicode_width::UnicodeWidthStr;

use crate::console::proc::view::ProcView;
use crate::term::grid::Rect;

const COLUMN_GAP: u16 = 7;
const NODE_PADDING: u16 = 4;

pub struct NodeBox {
  pub proc_idx: usize,
  pub rect: Rect,
}

impl NodeBox {
  pub fn center_col(&self) -> i32 {
    self.rect.x as i32 + self.rect.width as i32 / 2
  }

  pub fn center_row(&self) -> i32 {
    self.rect.y as i32
  }
}

pub struct Edge {
  pub from: usize,
  pub to: usize,
}

pub struct GraphLayout {
  pub nodes: Vec<NodeBox>,
  pub edges: Vec<Edge>,
  pub content_width: u16,
  pub content_height: u16,
}

impl GraphLayout {
  pub fn node_for_proc(&self, proc_idx: usize) -> Option<&NodeBox> {
    self.nodes.iter().find(|n| n.proc_idx == proc_idx)
  }
}

pub fn node_width(name: &str) -> u16 {
  name.width() as u16 + NODE_PADDING
}

pub fn layout_graph(procs: &[ProcView]) -> GraphLayout {
  let names: Vec<&str> = procs.iter().map(|p| p.name()).collect();
  let deps_of = resolve_deps(procs);
  layout_from(&names, &deps_of)
}

pub(crate) fn layout_from(
  names: &[&str],
  deps_of: &[Vec<usize>],
) -> GraphLayout {
  let n = names.len();
  if n == 0 {
    return GraphLayout {
      nodes: Vec::new(),
      edges: Vec::new(),
      content_width: 0,
      content_height: 0,
    };
  }

  let mut dependents_of = vec![Vec::new(); n];
  let mut indegree = vec![0u32; n];
  let mut edges = Vec::new();
  for (i, deps) in deps_of.iter().enumerate() {
    for &d in deps {
      dependents_of[d].push(i);
      indegree[i] += 1;
      edges.push(Edge { from: d, to: i });
    }
  }

  let layers = compute_layers(&deps_of);
  let max_layer = layers.iter().copied().max().unwrap_or(0);

  let mut col_width = vec![0u16; max_layer as usize + 1];
  for i in 0..n {
    let w = node_width(names[i]);
    let layer = layers[i] as usize;
    col_width[layer] = col_width[layer].max(w);
  }
  let mut col_x = vec![0u16; max_layer as usize + 1];
  for layer in 1..=max_layer as usize {
    col_x[layer] = col_x[layer - 1] + col_width[layer - 1] + COLUMN_GAP;
  }

  let order = visit_order(n, &indegree, &dependents_of);

  let mut nodes = Vec::with_capacity(n);
  let mut row: u16 = 0;
  let mut prev_component: Option<usize> = None;
  let components = component_ids(n, &deps_of);
  for &i in &order {
    if let Some(prev) = prev_component {
      if components[i] != prev {
        row += 1;
      }
    }
    prev_component = Some(components[i]);

    let layer = layers[i] as usize;
    nodes.push(NodeBox {
      proc_idx: i,
      rect: Rect {
        x: col_x[layer],
        y: row,
        width: node_width(names[i]),
        height: 1,
      },
    });
    row += 1;
  }

  let content_width = col_x
    .last()
    .copied()
    .unwrap_or(0)
    + col_width.last().copied().unwrap_or(0);

  GraphLayout {
    nodes,
    edges,
    content_width,
    content_height: row,
  }
}

fn resolve_deps(procs: &[ProcView]) -> Vec<Vec<usize>> {
  procs
    .iter()
    .map(|p| {
      p.deps
        .iter()
        .filter_map(|name| {
          procs.iter().position(|q| q.name() == name)
        })
        .collect()
    })
    .collect()
}

fn compute_layers(deps_of: &[Vec<usize>]) -> Vec<u16> {
  let n = deps_of.len();
  let mut memo = vec![None; n];
  let mut on_stack = vec![false; n];
  let mut layers = vec![0u16; n];
  for i in 0..n {
    layers[i] = layer_of(i, deps_of, &mut memo, &mut on_stack);
  }
  layers
}

fn layer_of(
  i: usize,
  deps_of: &[Vec<usize>],
  memo: &mut [Option<u16>],
  on_stack: &mut [bool],
) -> u16 {
  if let Some(v) = memo[i] {
    return v;
  }
  if on_stack[i] {
    return 0;
  }
  on_stack[i] = true;
  let mut layer = 0;
  for &d in &deps_of[i] {
    layer = layer.max(layer_of(d, deps_of, memo, on_stack) + 1);
  }
  on_stack[i] = false;
  memo[i] = Some(layer);
  layer
}

fn visit_order(
  n: usize,
  indegree: &[u32],
  dependents_of: &[Vec<usize>],
) -> Vec<usize> {
  let mut visited = vec![false; n];
  let mut order = Vec::with_capacity(n);
  let mut stack = Vec::new();

  for root in 0..n {
    if indegree[root] != 0 || visited[root] {
      continue;
    }
    stack.push(root);
    while let Some(node) = stack.pop() {
      if visited[node] {
        continue;
      }
      visited[node] = true;
      order.push(node);
      let mut next: Vec<usize> = dependents_of[node]
        .iter()
        .copied()
        .filter(|&d| !visited[d])
        .collect();
      next.sort_unstable_by(|a, b| b.cmp(a));
      stack.extend(next);
    }
  }

  for i in 0..n {
    if !visited[i] {
      visited[i] = true;
      order.push(i);
    }
  }
  order
}

fn component_ids(n: usize, deps_of: &[Vec<usize>]) -> Vec<usize> {
  let mut parent: Vec<usize> = (0..n).collect();
  for i in 0..n {
    for &d in &deps_of[i] {
      union(&mut parent, i, d);
    }
  }
  let mut label = vec![0usize; n];
  let mut next_label = 0;
  let mut seen = std::collections::HashMap::new();
  for i in 0..n {
    let root = find(&mut parent, i);
    let id = *seen.entry(root).or_insert_with(|| {
      let id = next_label;
      next_label += 1;
      id
    });
    label[i] = id;
  }
  label
}

fn find(parent: &mut [usize], i: usize) -> usize {
  let mut root = i;
  while parent[root] != root {
    root = parent[root];
  }
  let mut cur = i;
  while parent[cur] != root {
    let next = parent[cur];
    parent[cur] = root;
    cur = next;
  }
  root
}

fn union(parent: &mut [usize], a: usize, b: usize) {
  let ra = find(parent, a);
  let rb = find(parent, b);
  if ra != rb {
    parent[ra.max(rb)] = ra.min(rb);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn rows(g: &GraphLayout) -> Vec<(usize, u16, u16)> {
    g.nodes.iter().map(|n| (n.proc_idx, n.rect.x, n.rect.y)).collect()
  }

  #[test]
  fn chain_lays_out_left_to_right() {
    let names = ["db", "api", "web"];
    let deps = vec![vec![], vec![0], vec![1]];
    let g = layout_from(&names, &deps);

    let x: Vec<u16> = (0..3)
      .map(|i| g.node_for_proc(i).unwrap().rect.x)
      .collect();
    assert!(x[0] < x[1] && x[1] < x[2]);
  }

  #[test]
  fn independent_procs_get_separate_rows() {
    let names = ["a", "b"];
    let deps = vec![vec![], vec![]];
    let g = layout_from(&names, &deps);

    let y0 = g.node_for_proc(0).unwrap().rect.y;
    let y1 = g.node_for_proc(1).unwrap().rect.y;
    assert_ne!(y0, y1);
    assert!(y1 > y0 + 1);
  }

  #[test]
  fn diamond_places_dependent_after_both_deps() {
    let names = ["root", "left", "right", "join"];
    let deps = vec![vec![], vec![0], vec![0], vec![1, 2]];
    let g = layout_from(&names, &deps);

    let join_x = g.node_for_proc(3).unwrap().rect.x;
    let left_x = g.node_for_proc(1).unwrap().rect.x;
    let right_x = g.node_for_proc(2).unwrap().rect.x;
    assert!(join_x > left_x && join_x > right_x);
    assert_eq!(g.edges.len(), 4);
  }

  #[test]
  fn cycle_does_not_hang() {
    let names = ["a", "b"];
    let deps = vec![vec![1], vec![0]];
    let g = layout_from(&names, &deps);
    assert_eq!(g.nodes.len(), 2);
  }
}
