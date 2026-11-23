//! Recursive tiling layout (task 070).
//!
//! This module is pure geometry. It never paints, never touches `egui`, and has
//! no dependency on the database or the application; it turns a flat list of
//! notes into world-space [`Rect`]s. Task 100 paints those rectangles and task
//! 170 maps the persistence model into [`LayoutNote`]/[`LayoutEdge`].
//!
//! # Rules
//!
//! * A note with no visible children is a leaf, [`LEAF_WIDTH`] x
//!   [`LEAF_HEIGHT`].
//! * A note with children is a container. It has a [`CONTAINER_HEADER`] strip
//!   and lays its children out in a row-major grid with
//!   `cols = ceil(sqrt(n))`. A container is
//!   `padding * 2 + sum(col_width + gap)` wide and
//!   `header + sum(row_height + gap)` high (one max width per column and one
//!   max height per row, so differently sized children never overlap).
//! * Children are ordered open first, then done, then by `sort`, then `id`.
//! * Root notes are grouped into columns: one column per group, ordered by the
//!   `(GroupId, sort)` list supplied by the caller, followed by an Inbox column
//!   for notes with no group. Groups that appear on notes but not in that list
//!   are appended by id so no note is silently dropped.
//! * `done` notes are laid out after their open siblings; `hidden` notes (the
//!   caller's "Hide done" filter) are omitted; `snoozed` notes are laid out
//!   normally.
//! * Pinned root notes use their stored `x`/`y` and are excluded from the
//!   column flow. Pinning a non-root note has no effect (child-level pinning is
//!   unsupported).
//! * A note whose parent is missing, or was omitted because it is hidden, becomes
//!   a root. Cyclic parents are skipped defensively. Recursion stops after
//!   [`DEPTH_CAP`] container levels; a note at that depth is drawn as a leaf and
//!   its children are not placed.
//!
//! All arithmetic is finite for finite input. Positions that arrive as `NaN`
//! or infinity are clamped to `0.0`, and malformed graphs never panic.

use std::collections::{HashMap, HashSet};

/// Identifier for a note.
///
/// The board model uses the same width; callers convert from SQLite's `i64`
/// rowids when they build [`LayoutNote`]s.
pub type NoteId = u64;

/// Identifier for a group.
pub type GroupId = u64;

/// Width of a leaf note, in world units.
pub const LEAF_WIDTH: f32 = 260.0;
/// Height of a leaf note, in world units.
pub const LEAF_HEIGHT: f32 = 160.0;
/// Height of a container's title strip.
pub const CONTAINER_HEADER: f32 = 36.0;
/// Padding between a container's edge and its child grid (horizontally).
pub const CONTAINER_PADDING: f32 = 12.0;
/// Gap between siblings, rows, columns and stacked root notes.
pub const CONTAINER_GAP: f32 = 12.0;
/// Maximum number of nested container levels.
pub const DEPTH_CAP: usize = 8;

/// A point in board world space.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    /// Create a point.
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Linear interpolation from `self` towards `other`, with `t` in `0..=1`.
    pub fn lerp(self, other: Point, t: f32) -> Point {
        Point::new(
            self.x + (other.x - self.x) * t,
            self.y + (other.y - self.y) * t,
        )
    }
}

/// An axis-aligned rectangle in board world space with a top-left origin.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    /// Create a rectangle from its top-left corner and size.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// The `x` coordinate of the right edge.
    pub const fn right(self) -> f32 {
        self.x + self.w
    }

    /// The `y` coordinate of the bottom edge.
    pub const fn bottom(self) -> f32 {
        self.y + self.h
    }

    /// The centre of the rectangle.
    pub fn center(self) -> Point {
        Point::new(self.x + self.w * 0.5, self.y + self.h * 0.5)
    }

    /// Whether every coordinate is finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.w.is_finite() && self.h.is_finite()
    }

    /// Whether `point` lies inside or on the border.
    pub fn contains_point(self, point: Point) -> bool {
        point.x >= self.x
            && point.y >= self.y
            && point.x <= self.right()
            && point.y <= self.bottom()
    }

    /// Whether `other` lies entirely inside or on the border.
    pub fn contains_rect(self, other: Rect) -> bool {
        other.x >= self.x
            && other.y >= self.y
            && other.right() <= self.right()
            && other.bottom() <= self.bottom()
    }

    /// Whether the two rectangles share interior area.
    ///
    /// Rectangles that only touch edges are not considered overlapping.
    pub fn overlaps(self, other: Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// The smallest rectangle containing both inputs.
    pub fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(x, y, right - x, bottom - y)
    }
}

/// One note as the layout algorithm sees it.
///
/// This mirrors the `notes` columns `parent_id`, `group_id`, `done`, `snoozed`,
/// `pinned`, `pos_x`, `pos_y` and `sort_order`. Positions are top-left corner
/// coordinates and are only read for pinned root notes.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutNote {
    /// Stable note id.
    pub id: NoteId,
    /// Parent container, if the note is nested.
    pub parent: Option<NoteId>,
    /// Group whose root column the note belongs to; `None` is the Inbox.
    pub group: Option<GroupId>,
    /// Completed notes sort after their open siblings.
    pub done: bool,
    /// Hidden notes are omitted from the layout.
    pub hidden: bool,
    /// Snoozed notes are still laid out; snoozing only dims them in the UI.
    pub snoozed: bool,
    /// Pinned root notes keep their stored `x`/`y`.
    pub pinned: bool,
    /// Stored world `x`, used only when `pinned` is true on a root note.
    pub x: f32,
    /// Stored world `y`, used only when `pinned` is true on a root note.
    pub y: f32,
    /// Manual ordering key; smaller values sort first, ties break on `id`.
    pub sort: i64,
}

impl LayoutNote {
    /// Create an unpinned, ungrouped root note with default flags and position.
    pub fn new(id: NoteId) -> Self {
        Self {
            id,
            parent: None,
            group: None,
            done: false,
            hidden: false,
            snoozed: false,
            pinned: false,
            x: 0.0,
            y: 0.0,
            sort: 0,
        }
    }
}

/// A directed link between two notes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayoutEdge {
    pub from: NoteId,
    pub to: NoteId,
}

/// The rectangles produced for one board.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// World rectangle per visible note.
    pub rects: HashMap<NoteId, Rect>,
    /// Notes that were laid out as containers (they have visible children).
    pub containers: HashSet<NoteId>,
}

impl Layout {
    /// Compute the layout for `notes` and the caller-supplied group order.
    pub fn compute(notes: &[LayoutNote], groups: &[(GroupId, i64)]) -> Self {
        compute(notes, groups)
    }

    /// The rectangle for `id`, if the note is visible.
    pub fn rect(&self, id: NoteId) -> Option<Rect> {
        self.rects.get(&id).copied()
    }

    /// Whether `id` was laid out as a container.
    pub fn is_container(&self, id: NoteId) -> bool {
        self.containers.contains(&id)
    }

    /// The union of every visible rectangle, or `None` for an empty board.
    pub fn bounds(&self) -> Option<Rect> {
        self.rects.values().copied().reduce(Rect::union)
    }
}

/// Compute the layout for `notes` and the caller-supplied group order.
///
/// `groups` is a `(group id, sort)` list; it decides the left-to-right order of
/// the group columns. The Inbox column (`group: None`) always comes last.
/// Pinned roots are placed before the flow and do not consume column space.
pub fn compute(notes: &[LayoutNote], groups: &[(GroupId, i64)]) -> Layout {
    let mut planner = Planner::build(notes);
    let mut layout = Layout::default();

    let mut group_order: Vec<(GroupId, i64)> = Vec::with_capacity(groups.len());
    let mut known_groups = HashSet::new();
    for &(id, sort) in groups {
        if known_groups.insert(id) {
            group_order.push((id, sort));
        }
    }
    group_order.sort_by_key(|&(id, sort)| (sort, id));

    let mut by_group: HashMap<Option<GroupId>, Vec<NoteId>> = HashMap::new();
    let mut pinned = Vec::new();
    for &id in &planner.roots {
        let Some(note) = planner.note(id) else {
            continue;
        };
        if note.pinned {
            pinned.push(id);
        } else {
            by_group.entry(note.group).or_default().push(id);
        }
    }

    pinned.sort_by_key(|id| planner.sort_key(*id));
    let mut visiting = HashSet::new();
    for id in pinned {
        let (x, y) = planner.note(id).map_or((0.0, 0.0), |note| {
            (finite_or(note.x, 0.0), finite_or(note.y, 0.0))
        });
        let size = planner.measure(id, 0, &mut visiting);
        planner.place(id, Rect::new(x, y, size.w, size.h), &mut layout);
    }

    let mut columns: Vec<Column> = Vec::new();
    for &(group, _) in &group_order {
        if let Some(mut ids) = by_group.remove(&Some(group)) {
            sort_notes(&planner, &mut ids);
            columns.push((Some(group), ids));
        }
    }

    let mut extra_groups: Vec<GroupId> = by_group.keys().filter_map(|group| *group).collect();
    extra_groups.sort_unstable();
    for group in extra_groups {
        if let Some(mut ids) = by_group.remove(&Some(group)) {
            sort_notes(&planner, &mut ids);
            columns.push((Some(group), ids));
        }
    }

    if let Some(mut ids) = by_group.remove(&None) {
        sort_notes(&planner, &mut ids);
        columns.push((None, ids));
    }

    let mut column_x = 0.0;
    for (_, ids) in columns {
        let mut column_width = 0.0_f32;
        let mut y = 0.0;
        for id in ids {
            let size = planner.measure(id, 0, &mut visiting);
            column_width = column_width.max(size.w);
            planner.place(id, Rect::new(column_x, y, size.w, size.h), &mut layout);
            y += size.h + CONTAINER_GAP;
        }
        column_x += column_width + CONTAINER_GAP;
    }

    layout
}

/// Bezier anchors for a directed edge.
///
/// The curve starts and ends exactly on the two rectangles' borders and uses
/// control points one third and two thirds along the chord.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeGeometry {
    pub start: Point,
    pub end: Point,
    pub control1: Point,
    pub control2: Point,
}

/// Compute edge anchors from the laid-out rectangles.
///
/// Returns `None` when either endpoint has no rectangle or a rectangle has a
/// non-finite coordinate. Coincident centres and zero-sized rectangles still
/// produce finite geometry.
pub fn edge_geometry(
    from: NoteId,
    to: NoteId,
    rects: &HashMap<NoteId, Rect>,
) -> Option<EdgeGeometry> {
    let from_rect = rects.get(&from).copied()?;
    let to_rect = rects.get(&to).copied()?;
    if !from_rect.is_finite() || !to_rect.is_finite() {
        return None;
    }

    let from_center = from_rect.center();
    let to_center = to_rect.center();
    let mut direction = Point::new(to_center.x - from_center.x, to_center.y - from_center.y);
    if direction.x.abs() <= f32::EPSILON && direction.y.abs() <= f32::EPSILON {
        // Coincident centres: a deterministic horizontal anchor keeps the
        // result finite instead of dividing by zero.
        direction = Point::new(1.0, 0.0);
    }

    let start = boundary_point(from_rect, direction);
    let end = boundary_point(to_rect, Point::new(-direction.x, -direction.y));
    Some(EdgeGeometry {
        start,
        end,
        control1: start.lerp(end, 1.0 / 3.0),
        control2: start.lerp(end, 2.0 / 3.0),
    })
}

/// A `(group, root ids)` column while the layout is being assembled.
type Column = (Option<GroupId>, Vec<NoteId>);

/// A measured node.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Size {
    w: f32,
    h: f32,
}

impl Size {
    const fn leaf() -> Self {
        Self {
            w: LEAF_WIDTH,
            h: LEAF_HEIGHT,
        }
    }
}

/// The measured child grid of one container.
struct Grid {
    cols: usize,
    col_widths: Vec<f32>,
    row_heights: Vec<f32>,
    width: f32,
    height: f32,
}

impl Grid {
    fn new(sizes: &[Size]) -> Self {
        let count = sizes.len();
        let mut cols: usize = 1;
        while cols.saturating_mul(cols) < count {
            cols += 1;
        }
        let rows = count.div_ceil(cols);

        let mut col_widths = vec![0.0_f32; cols];
        let mut row_heights = vec![0.0_f32; rows];
        for (index, size) in sizes.iter().enumerate() {
            let col = index % cols;
            let row = index / cols;
            col_widths[col] = col_widths[col].max(size.w);
            row_heights[row] = row_heights[row].max(size.h);
        }

        let width =
            CONTAINER_PADDING * 2.0 + col_widths.iter().sum::<f32>() + cols as f32 * CONTAINER_GAP;
        let height =
            CONTAINER_HEADER + row_heights.iter().sum::<f32>() + rows as f32 * CONTAINER_GAP;

        Self {
            cols,
            col_widths,
            row_heights,
            width,
            height,
        }
    }

    fn child_offset(&self, index: usize) -> (f32, f32) {
        let col = index % self.cols;
        let row = index / self.cols;
        let x = CONTAINER_PADDING
            + self.col_widths[..col].iter().sum::<f32>()
            + col as f32 * CONTAINER_GAP;
        let y = CONTAINER_HEADER
            + self.row_heights[..row].iter().sum::<f32>()
            + row as f32 * CONTAINER_GAP;
        (x, y)
    }
}

/// Indexed note graph plus the measured sizes and container flags.
struct Planner<'a> {
    notes: &'a [LayoutNote],
    index: HashMap<NoteId, usize>,
    children: HashMap<NoteId, Vec<NoteId>>,
    roots: Vec<NoteId>,
    sizes: HashMap<NoteId, Size>,
    containers: HashSet<NoteId>,
}

impl<'a> Planner<'a> {
    fn build(notes: &'a [LayoutNote]) -> Self {
        let mut index: HashMap<NoteId, usize> = HashMap::new();
        for (position, note) in notes.iter().enumerate() {
            if note.hidden {
                continue;
            }
            // First occurrence wins, so duplicate ids cannot corrupt the graph.
            index.entry(note.id).or_insert(position);
        }

        let mut children: HashMap<NoteId, Vec<NoteId>> = HashMap::new();
        let mut roots = Vec::new();
        for (position, note) in notes.iter().enumerate() {
            if note.hidden || index.get(&note.id) != Some(&position) {
                continue;
            }
            match note.parent {
                None => roots.push(note.id),
                Some(parent) if parent == note.id => {
                    // A self-parent is a cycle: skip it rather than recurse.
                }
                Some(parent) => {
                    if index.contains_key(&parent) {
                        children.entry(parent).or_default().push(note.id);
                    } else {
                        // Missing or hidden parent: promote to a root so the note
                        // stays visible.
                        roots.push(note.id);
                    }
                }
            }
        }

        let mut parent_ids: Vec<NoteId> = children.keys().copied().collect();
        parent_ids.sort_unstable();
        for parent in parent_ids {
            if let Some(list) = children.get_mut(&parent) {
                list.sort_by_key(|id| {
                    index.get(id).map_or((false, 0, *id), |&position| {
                        (notes[position].done, notes[position].sort, *id)
                    })
                });
            }
        }

        Self {
            notes,
            index,
            children,
            roots,
            sizes: HashMap::new(),
            containers: HashSet::new(),
        }
    }

    fn note(&self, id: NoteId) -> Option<&LayoutNote> {
        self.index
            .get(&id)
            .and_then(|&position| self.notes.get(position))
    }

    fn sort_key(&self, id: NoteId) -> (bool, i64, NoteId) {
        self.note(id)
            .map_or((false, 0, id), |note| (note.done, note.sort, id))
    }

    /// Bottom-up sizing with a depth cap and a defensive cycle guard.
    fn measure(&mut self, id: NoteId, depth: usize, visiting: &mut HashSet<NoteId>) -> Size {
        if let Some(size) = self.sizes.get(&id) {
            return *size;
        }

        let size = if depth >= DEPTH_CAP || !self.children.contains_key(&id) {
            Size::leaf()
        } else if !visiting.insert(id) {
            // Already on the stack: this is a cycle, so stop and treat it as a
            // leaf.
            Size::leaf()
        } else {
            let child_ids = self.children.get(&id).cloned().unwrap_or_default();
            let child_sizes: Vec<Size> = child_ids
                .iter()
                .map(|child| self.measure(*child, depth + 1, visiting))
                .collect();
            visiting.remove(&id);
            if child_sizes.is_empty() {
                Size::leaf()
            } else {
                let grid = Grid::new(&child_sizes);
                self.containers.insert(id);
                Size {
                    w: grid.width,
                    h: grid.height,
                }
            }
        };

        self.sizes.insert(id, size);
        size
    }

    /// Place `rect` for `id` and recursively place its measured children.
    fn place(&self, id: NoteId, rect: Rect, layout: &mut Layout) {
        if layout.rects.contains_key(&id) {
            // A second visit can only come from malformed input; first wins.
            return;
        }
        layout.rects.insert(id, rect);
        if !self.containers.contains(&id) {
            return;
        }
        layout.containers.insert(id);

        let Some(child_ids) = self.children.get(&id) else {
            return;
        };
        let child_sizes: Vec<Size> = child_ids
            .iter()
            .map(|child| self.sizes.get(child).copied().unwrap_or_else(Size::leaf))
            .collect();
        if child_sizes.is_empty() {
            return;
        }

        let grid = Grid::new(&child_sizes);
        for (index, child) in child_ids.iter().enumerate() {
            let (offset_x, offset_y) = grid.child_offset(index);
            let size = child_sizes[index];
            let child_rect = Rect::new(rect.x + offset_x, rect.y + offset_y, size.w, size.h);
            self.place(*child, child_rect, layout);
        }
    }
}

fn sort_notes(planner: &Planner<'_>, ids: &mut [NoteId]) {
    ids.sort_by_key(|id| planner.sort_key(*id));
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

/// Intersect the ray from `rect`'s centre in `direction` with its border.
fn boundary_point(rect: Rect, direction: Point) -> Point {
    let center = rect.center();
    let half_width = rect.w * 0.5;
    let half_height = rect.h * 0.5;

    let scale_x = if direction.x.abs() > f32::EPSILON && half_width > 0.0 {
        half_width / direction.x.abs()
    } else {
        f32::INFINITY
    };
    let scale_y = if direction.y.abs() > f32::EPSILON && half_height > 0.0 {
        half_height / direction.y.abs()
    } else {
        f32::INFINITY
    };
    let scale = scale_x.min(scale_y);
    if !scale.is_finite() || scale <= 0.0 {
        return center;
    }
    Point::new(
        center.x + direction.x * scale,
        center.y + direction.y * scale,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_parent(id: NoteId, parent: NoteId) -> LayoutNote {
        let mut note = LayoutNote::new(id);
        note.parent = Some(parent);
        note
    }

    /// A root container (id 1) with `count` leaf children (ids 2..).
    fn leaf_grid(count: usize) -> (Vec<LayoutNote>, Layout) {
        let mut notes = vec![LayoutNote::new(1)];
        for index in 0..count {
            let id = index as NoteId + 2;
            let mut note = with_parent(id, 1);
            note.sort = index as i64;
            notes.push(note);
        }
        let layout = Layout::compute(&notes, &[]);
        (notes, layout)
    }

    fn on_border(rect: Rect, point: Point) -> bool {
        let eps = 0.05;
        let inside = point.x >= rect.x - eps
            && point.x <= rect.right() + eps
            && point.y >= rect.y - eps
            && point.y <= rect.bottom() + eps;
        let edge = (point.x - rect.x).abs() <= eps
            || (point.x - rect.right()).abs() <= eps
            || (point.y - rect.y).abs() <= eps
            || (point.y - rect.bottom()).abs() <= eps;
        inside && edge
    }

    #[test]
    fn sibling_grids_do_not_overlap() {
        for count in [1_usize, 2, 3, 5, 9, 20] {
            let (_, layout) = leaf_grid(count);
            let parent = layout.rects[&1];
            assert!(layout.containers.contains(&1));
            assert_eq!(layout.rects.len(), count + 1);

            let mut cols = 1_usize;
            while cols * cols < count {
                cols += 1;
            }
            let rows = count.div_ceil(cols);
            let expected_width =
                CONTAINER_PADDING * 2.0 + cols as f32 * (LEAF_WIDTH + CONTAINER_GAP);
            let expected_height = CONTAINER_HEADER + rows as f32 * (LEAF_HEIGHT + CONTAINER_GAP);
            assert!(
                (parent.w - expected_width).abs() < 0.01,
                "container width for {count} children"
            );
            assert!(
                (parent.h - expected_height).abs() < 0.01,
                "container height for {count} children"
            );

            let children: Vec<Rect> = (0..count)
                .map(|index| layout.rects[&(index as NoteId + 2)])
                .collect();
            for (index, child) in children.iter().enumerate() {
                assert!(
                    parent.contains_rect(*child),
                    "child {index} escapes a {count}-child container"
                );
                for sibling in &children[index + 1..] {
                    assert!(
                        !child.overlaps(*sibling),
                        "siblings overlap in a {count}-child container"
                    );
                    let gap_x = (sibling.x - child.right()).max(child.x - sibling.right());
                    let gap_y = (sibling.y - child.bottom()).max(child.y - sibling.bottom());
                    assert!(
                        gap_x >= CONTAINER_GAP - 0.01 || gap_y >= CONTAINER_GAP - 0.01,
                        "siblings are too close in a {count}-child container"
                    );
                }
            }

            let bounds = layout.bounds().expect("a non-empty layout has bounds");
            assert!(bounds.contains_rect(parent));
        }
    }

    #[test]
    fn two_nesting_levels_tile_inside_each_other() {
        let mut notes = vec![LayoutNote::new(1), with_parent(2, 1)];
        for id in 3..=6 {
            notes.push(with_parent(id, 2));
        }
        let layout = Layout::compute(&notes, &[]);

        assert!(layout.containers.contains(&1));
        assert!(layout.containers.contains(&2));
        let outer = layout.rects[&1];
        let inner = layout.rects[&2];
        assert!(outer.contains_rect(inner));

        let grandchildren = [
            layout.rects[&3],
            layout.rects[&4],
            layout.rects[&5],
            layout.rects[&6],
        ];
        for (index, rect) in grandchildren.iter().enumerate() {
            assert!(inner.contains_rect(*rect));
            for other in &grandchildren[index + 1..] {
                assert!(!rect.overlaps(*other));
            }
        }
        assert!(outer.is_finite() && inner.is_finite());
    }

    #[test]
    fn groups_then_inbox_form_ordered_columns() {
        let groups = [(30_u64, 2_i64), (10, 0), (20, 1)];
        let mut notes = Vec::new();
        for (index, (group, _)) in groups.iter().enumerate() {
            let mut note = LayoutNote::new(index as NoteId + 1);
            note.group = Some(*group);
            notes.push(note);
        }
        notes.push(LayoutNote::new(4));
        let mut second_in_g10 = LayoutNote::new(5);
        second_in_g10.group = Some(10);
        second_in_g10.sort = 1;
        notes.push(second_in_g10);

        let layout = Layout::compute(&notes, &groups);
        let g10 = layout.rects[&2];
        let g20 = layout.rects[&3];
        let g30 = layout.rects[&1];
        let inbox = layout.rects[&4];

        assert_eq!(g10.x, 0.0);
        assert!(g10.x < g20.x);
        assert!(g20.x < g30.x);
        assert!(g30.x < inbox.x);
        assert_eq!(g10.y, 0.0);
        assert_eq!(g20.y, 0.0);
        assert_eq!(g30.y, 0.0);
        assert_eq!(inbox.y, 0.0);

        let stacked = layout.rects[&5];
        assert_eq!(stacked.x, g10.x);
        assert_eq!(stacked.y, LEAF_HEIGHT + CONTAINER_GAP);
        assert!(!g10.overlaps(stacked));
    }

    #[test]
    fn unlisted_groups_are_appended_before_inbox() {
        let groups = [(1_u64, 0_i64)];
        let mut listed = LayoutNote::new(1);
        listed.group = Some(1);
        let mut extra = LayoutNote::new(2);
        extra.group = Some(9);
        let inbox = LayoutNote::new(3);

        let layout = Layout::compute(&[listed, extra, inbox], &groups);
        assert_eq!(layout.rects[&1].x, 0.0);
        assert!(layout.rects[&1].x < layout.rects[&2].x);
        assert!(layout.rects[&2].x < layout.rects[&3].x);
    }

    #[test]
    fn pinned_roots_keep_their_position_and_leave_the_flow() {
        let mut first = LayoutNote::new(1);
        first.sort = 0;
        let mut pinned = LayoutNote::new(2);
        pinned.sort = 1;
        pinned.pinned = true;
        pinned.x = 900.0;
        pinned.y = 400.0;
        let mut last = LayoutNote::new(3);
        last.sort = 2;

        let layout = Layout::compute(&[first, pinned, last], &[]);
        let pinned_rect = layout.rects[&2];
        assert_eq!((pinned_rect.x, pinned_rect.y), (900.0, 400.0));
        assert_eq!(layout.rects[&1].y, 0.0);
        assert_eq!(layout.rects[&3].y, LEAF_HEIGHT + CONTAINER_GAP);
    }

    #[test]
    fn done_notes_sort_last_and_keep_their_children() {
        let mut done = LayoutNote::new(1);
        done.done = true;
        let child = with_parent(2, 1);
        let open = LayoutNote::new(3);

        let layout = Layout::compute(&[done, child, open], &[]);
        assert_eq!(layout.rects.len(), 3);
        assert!(
            layout.is_container(1),
            "a done container keeps its children"
        );
        assert_eq!(layout.rects[&3].y, 0.0, "the open note sorts first");
        assert_eq!(layout.rects[&1].y, LEAF_HEIGHT + CONTAINER_GAP);
    }

    #[test]
    fn hidden_notes_are_omitted() {
        let mut done = LayoutNote::new(1);
        done.hidden = true;
        let promoted = with_parent(2, 1);

        let layout = Layout::compute(&[done, promoted, LayoutNote::new(3)], &[]);
        assert!(!layout.rects.contains_key(&1));
        assert_eq!(layout.rects.len(), 2);
        assert!(layout.rects.contains_key(&2));
        assert!(layout.rects.contains_key(&3));
        assert_eq!(layout.rects[&2].x, 0.0);
        assert_eq!(layout.rects[&3].y, LEAF_HEIGHT + CONTAINER_GAP);
    }

    #[test]
    fn snoozed_notes_are_laid_out() {
        let mut snoozed = LayoutNote::new(1);
        snoozed.snoozed = true;

        let layout = Layout::compute(&[snoozed, LayoutNote::new(2)], &[]);
        assert!(layout.rects.contains_key(&1));
        assert!(layout.rects.contains_key(&2));
        assert_eq!(layout.rects[&1].y, 0.0);
        assert_eq!(layout.rects[&2].y, LEAF_HEIGHT + CONTAINER_GAP);
    }

    #[test]
    fn cyclic_parents_are_skipped_without_panicking() {
        let mut first = LayoutNote::new(1);
        first.parent = Some(2);
        let mut second = LayoutNote::new(2);
        second.parent = Some(1);
        let mut self_parent = LayoutNote::new(3);
        self_parent.parent = Some(3);

        let layout = Layout::compute(&[first, second, self_parent, LayoutNote::new(4)], &[]);
        assert_eq!(layout.rects.len(), 1);
        assert!(layout.rects.contains_key(&4));
        assert!(layout.containers.is_empty());
    }

    #[test]
    fn missing_parent_becomes_a_root() {
        let mut orphan = LayoutNote::new(5);
        orphan.parent = Some(99);

        let layout = Layout::compute(&[orphan], &[]);
        assert_eq!(
            layout.rects[&5],
            Rect::new(0.0, 0.0, LEAF_WIDTH, LEAF_HEIGHT)
        );
    }

    #[test]
    fn nesting_stops_at_the_depth_cap() {
        let mut notes = Vec::new();
        for index in 0..(DEPTH_CAP + 4) {
            let id = index as NoteId + 1;
            let mut note = LayoutNote::new(id);
            if index > 0 {
                note.parent = Some(id - 1);
            }
            notes.push(note);
        }

        let layout = Layout::compute(&notes, &[]);
        assert_eq!(layout.rects.len(), DEPTH_CAP + 1);
        assert_eq!(layout.containers.len(), DEPTH_CAP);
        assert!(layout.rects.values().all(|rect| rect.is_finite()));
    }

    #[test]
    fn edge_geometry_is_finite_and_anchored_on_borders() {
        let mut notes = vec![LayoutNote::new(1)];
        for id in 2..=4 {
            notes.push(with_parent(id, 1));
        }
        let layout = Layout::compute(&notes, &[]);

        for (from, to) in [(1_u64, 2_u64), (2, 3), (3, 4), (4, 1)] {
            let geometry = edge_geometry(from, to, &layout.rects).expect("edge anchors");
            for point in [
                geometry.start,
                geometry.end,
                geometry.control1,
                geometry.control2,
            ] {
                assert!(point.x.is_finite() && point.y.is_finite());
            }
            assert!(on_border(layout.rects[&from], geometry.start));
            assert!(on_border(layout.rects[&to], geometry.end));

            let one_third = geometry.start.lerp(geometry.end, 1.0 / 3.0);
            let two_thirds = geometry.start.lerp(geometry.end, 2.0 / 3.0);
            assert!((geometry.control1.x - one_third.x).abs() < 0.001);
            assert!((geometry.control2.y - two_thirds.y).abs() < 0.001);
        }

        assert!(edge_geometry(1, 99, &layout.rects).is_none());
    }

    #[test]
    fn edge_geometry_is_finite_for_degenerate_rects() {
        let mut rects = HashMap::new();
        rects.insert(1, Rect::new(0.0, 0.0, 100.0, 60.0));
        rects.insert(2, Rect::new(0.0, 0.0, 100.0, 60.0));
        rects.insert(3, Rect::new(200.0, 0.0, 0.0, 0.0));

        for (from, to) in [(1, 2), (1, 3)] {
            let geometry = edge_geometry(from, to, &rects).expect("degenerate anchors");
            for point in [
                geometry.start,
                geometry.end,
                geometry.control1,
                geometry.control2,
            ] {
                assert!(point.x.is_finite() && point.y.is_finite());
            }
        }
    }

    #[test]
    fn layout_is_stable_across_repeated_calls() {
        let (notes, first) = leaf_grid(9);
        let second = Layout::compute(&notes, &[]);
        assert_eq!(first.rects, second.rects);
        assert_eq!(first.containers, second.containers);
    }
}
