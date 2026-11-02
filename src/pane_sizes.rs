//! Keeps the panes at the sizes the user chose. egui_dock only stores split
//! fractions, so on its own a bigger window makes the sidebar bigger, and a
//! window squeezed small loses the sizes for good.
//!
//! What keeps its size: the panes beside the tabs (the tabs take the rest),
//! and Places beside other panes (the folder tree, say, takes the rest).
//! Those sizes change only when the user drags a separator, so a window too
//! small for them squeezes the panes for as long as it stays that small.

use egui::{Pos2, Rect, Vec2};
use egui_dock::{Node, NodeIndex, SeparatorStyle, Tree};
use serde::{Deserialize, Serialize};

use crate::dock_tab::{DockTab, Pane};

/// How big a pane starts out along each axis, in points, and the most of the
/// dock it may take; what the panels were before they became panes.
fn default_size(pane: Pane, horizontal: bool) -> Option<(f32, f32)> {
    match (pane, horizontal) {
        (Pane::Places | Pane::Tree, true) => Some((220.0, 0.3)),
        // Enough for the standard folders; the folder tree below gets the rest.
        (Pane::Places, false) => Some((300.0, 0.6)),
        (Pane::Preview, true) => Some((300.0, 0.35)),
        (Pane::Terminal, false) => Some((240.0, 0.4)),
        _ => None,
    }
}

/// The panes in a subtree, or `None` if it also holds tabs.
fn panes_in(tree: &Tree<DockTab>, node: NodeIndex) -> Option<Vec<Pane>> {
    match &tree[node] {
        Node::Empty => Some(Vec::new()),
        Node::Leaf(leaf) => leaf.tabs.iter().map(DockTab::pane).collect(),
        Node::Horizontal(_) | Node::Vertical(_) => {
            let mut panes = panes_in(tree, node.left())?;
            panes.extend(panes_in(tree, node.right())?);
            Some(panes)
        }
    }
}

/// A split with a side that keeps its size.
struct Kept {
    /// The kept side is the left or top one.
    first: bool,
    /// The panes on it, in `Pane::ALL` order.
    panes: Vec<Pane>,
    horizontal: bool,
}

fn kept(tree: &Tree<DockTab>, node: NodeIndex) -> Option<Kept> {
    let horizontal = match &tree[node] {
        Node::Horizontal(_) => true,
        Node::Vertical(_) => false,
        Node::Empty | Node::Leaf(_) => return None,
    };
    let (left, right) = (panes_in(tree, node.left()), panes_in(tree, node.right()));
    let (first, mut panes) = match (left, right) {
        (Some(panes), None) => (true, panes),
        (None, Some(panes)) => (false, panes),
        (Some(a), Some(b)) => match (a.contains(&Pane::Places), b.contains(&Pane::Places)) {
            (true, false) => (true, a),
            (false, true) => (false, b),
            _ => return None,
        },
        (None, None) => return None,
    };
    if panes.is_empty() {
        return None;
    }
    panes.sort_by_key(|&p| p as u8);
    Some(Kept {
        first,
        panes,
        horizontal,
    })
}

fn contains(tree: &Tree<DockTab>, node: NodeIndex, pane: Pane) -> bool {
    match &tree[node] {
        Node::Empty => false,
        Node::Leaf(leaf) => leaf.tabs.iter().any(|t| t.pane() == Some(pane)),
        Node::Horizontal(_) | Node::Vertical(_) => {
            contains(tree, node.left(), pane) || contains(tree, node.right(), pane)
        }
    }
}

fn fraction(tree: &Tree<DockTab>, node: NodeIndex) -> Option<f32> {
    match &tree[node] {
        Node::Horizontal(split) | Node::Vertical(split) => Some(split.fraction),
        Node::Empty | Node::Leaf(_) => None,
    }
}

/// Lays `node` out in `rect` the way egui_dock does, less the separators,
/// calling `f` with each split and its rect, parents first. `f` may change
/// the split's fraction before its children are laid out.
fn walk(
    tree: &mut Tree<DockTab>,
    node: NodeIndex,
    rect: Rect,
    f: &mut impl FnMut(&mut Tree<DockTab>, NodeIndex, Rect),
) {
    if node.0 >= tree.len() {
        return;
    }
    let horizontal = match &tree[node] {
        Node::Horizontal(_) => true,
        Node::Vertical(_) => false,
        Node::Empty | Node::Leaf(_) => return,
    };
    f(tree, node, rect);
    let Some(fraction) = fraction(tree, node) else {
        return;
    };
    let (a, b) = if horizontal {
        rect.split_left_right_at_fraction(fraction)
    } else {
        rect.split_top_bottom_at_fraction(fraction)
    };
    walk(tree, node.left(), a, f);
    walk(tree, node.right(), b, f);
}

/// The fractions egui_dock lets a split of `length` have: it keeps the
/// least of `SeparatorStyle::extra` on each side, or half each if there isn't room.
fn bounds(length: f32) -> (f32, f32) {
    let min = (SeparatorStyle::default().extra / length.max(1.0)).min(1.0);
    (min.min(1.0 - min), min.max(1.0 - min))
}

fn length(rect: Rect, horizontal: bool) -> f32 {
    if horizontal {
        rect.width()
    } else {
        rect.height()
    }
}

/// The node kinds of a tree, to tell when its shape changed.
fn shape(tree: &Tree<DockTab>) -> Vec<u8> {
    tree.iter()
        .map(|node| match node {
            Node::Empty => 0,
            Node::Leaf(_) => 1,
            Node::Horizontal(_) => 2,
            Node::Vertical(_) => 3,
        })
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Chosen {
    panes: Vec<Pane>,
    horizontal: bool,
    size: f32,
}

/// The size chosen for each group of panes along each axis, the most recently
/// chosen last.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PaneSizes(Vec<Chosen>);

/// The fractions before egui_dock showed the dock, to tell what the user dragged.
pub struct Before {
    shape: Vec<u8>,
    fractions: Vec<Option<f32>>,
}

impl PaneSizes {
    fn exact(&self, panes: &[Pane], horizontal: bool) -> Option<f32> {
        self.0
            .iter()
            .find(|c| c.horizontal == horizontal && c.panes == panes)
            .map(|c| c.size)
    }

    /// The size of the group last chosen with any of `panes` in it, so the
    /// sidebar keeps its width as panes join or leave it.
    fn overlapping(&self, panes: &[Pane], horizontal: bool) -> Option<f32> {
        self.0
            .iter()
            .rev()
            .find(|c| c.horizontal == horizontal && c.panes.iter().any(|p| panes.contains(p)))
            .map(|c| c.size)
    }

    fn choose(&mut self, panes: Vec<Pane>, horizontal: bool, size: f32) {
        self.0
            .retain(|c| c.horizontal != horizontal || c.panes != panes);
        self.0.push(Chosen {
            panes,
            horizontal,
            size,
        });
    }

    /// Sets the split fractions of `tree`, laid out in a dock of size `area`,
    /// so the panes get their chosen sizes. A group of panes with none chosen
    /// yet takes a related group's size, or its default size if the split
    /// holds the `new` pane, or else the size it has now.
    pub fn fit(&mut self, tree: &mut Tree<DockTab>, area: Vec2, new: Option<Pane>) {
        if !(area.x > 0.0 && area.y > 0.0 && area.is_finite()) {
            return;
        }
        let rect = Rect::from_min_size(Pos2::ZERO, area);
        walk(tree, NodeIndex::root(), rect, &mut |tree, node, rect| {
            let Some(kept) = kept(tree, node) else {
                return;
            };
            let length = length(rect, kept.horizontal).max(1.0);
            let size = match self.exact(&kept.panes, kept.horizontal) {
                Some(size) => size,
                None => {
                    let whole = if kept.horizontal { area.x } else { area.y };
                    let default = new
                        .filter(|&p| contains(tree, node, p))
                        .and_then(|_| {
                            kept.panes
                                .iter()
                                .filter_map(|&p| default_size(p, kept.horizontal))
                                .reduce(|a, b| (a.0.max(b.0), a.1.max(b.1)))
                        })
                        .map(|(size, max)| size.min(max * whole));
                    let size = self
                        .overlapping(&kept.panes, kept.horizontal)
                        .or(default)
                        .unwrap_or_else(|| {
                            let fraction = fraction(tree, node).unwrap_or(0.5);
                            let share = if kept.first { fraction } else { 1.0 - fraction };
                            share * length
                        });
                    self.choose(kept.panes, kept.horizontal, size);
                    size
                }
            };
            // Squeezed by a window too small for it, the chosen size stays.
            let share = size / length;
            let fraction = if kept.first { share } else { 1.0 - share };
            let (min, max) = bounds(length);
            if let Node::Horizontal(split) | Node::Vertical(split) = &mut tree[node] {
                split.fraction = fraction.clamp(min, max);
            }
        });
    }

    pub fn before(tree: &Tree<DockTab>) -> Before {
        Before {
            shape: shape(tree),
            fractions: (0..tree.len())
                .map(|i| fraction(tree, NodeIndex(i)))
                .collect(),
        }
    }

    /// Takes the sizes the user dragged separators to while egui_dock showed
    /// `tree`, laid out in a dock of size `area`. egui_dock also moves
    /// separators to keep its least size on each side, which isn't the user's
    /// doing.
    pub fn note_drags(&mut self, tree: &mut Tree<DockTab>, before: &Before, area: Vec2) {
        if shape(tree) != before.shape || !(area.x > 0.0 && area.y > 0.0) {
            return;
        }
        let rect = Rect::from_min_size(Pos2::ZERO, area);
        walk(tree, NodeIndex::root(), rect, &mut |tree, node, rect| {
            let (Some(Some(was)), Some(now)) = (before.fractions.get(node.0), fraction(tree, node))
            else {
                return;
            };
            if now == *was {
                return;
            }
            let Some(kept) = kept(tree, node) else {
                return;
            };
            if let Some(drawn) = tree[node].rect() {
                let range = length(drawn, kept.horizontal);
                if range > 0.0 {
                    let (min, max) = bounds(range);
                    if (now - was.clamp(min, max)).abs() < 1e-5 {
                        return;
                    }
                }
            }
            let share = if kept.first { now } else { 1.0 - now };
            self.choose(
                kept.panes,
                kept.horizontal,
                share * length(rect, kept.horizontal),
            );
        });
    }
}
