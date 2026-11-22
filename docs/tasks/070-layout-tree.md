# 070 — Recursive tiling layout

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** compute every note's rectangle from the board graph: group columns at the root, nested
notes tiled inside containers, pinned root notes respected. Pure functions, no rendering.

**Depends on:** 030. **Parallel with:** 040–060.
**Owns:** `src/layout.rs`, `src/layout` module declaration. Do not edit `canvas.rs`.

**Read first:** the `notes.parent_id/group_id/layout_mode/pos_x/pos_y/sort_order`
columns; `src/theme.rs` for sizes.

## Steps

1. Define inputs locally so this task does not wait for 040: `LayoutNote { id, parent, group,
   done, snoozed, pinned, x, y, sort }` and `LayoutEdge { from, to }`.
   `Layout { rects: HashMap<NoteId, Rect>, containers: HashSet<NoteId> }` plus
   `EdgeGeometry { start, end, control1, control2 }` and `edge_geometry(from, to, rects)`.
   170 maps `model::Board` into these inputs.
2. Constants in `layout.rs`: leaf size 260×160, container header 36, padding 12, gap 12, depth
   cap 8. Children are ordered by `sort_order` then id.
3. Bottom-up sizing: a leaf is 260×160; a container is `header + rows*(child_h+gap)` by
   `padding*2 + cols*(child_w+gap)`, where `cols = ceil(sqrt(n))` and row-major placement.
   Recurse, with a visited set that skips cycles defensively.
4. Root placement: group columns in `groups.sort_order`, then an **Inbox** column for
   `group_id IS NULL`; stack root notes with the gap. Root notes with `layout_mode='pinned'`
   use `pos_x/pos_y` and are excluded from the column flow. Done notes are omitted; snoozed
   notes are included.
5. Edge anchors: intersect the line between rect centers with each rect boundary; return two
   control points at 1/3 and 2/3 for the bezier. Deterministic.
6. Tests: 1/2/3/5/9/20 children; two nesting levels; three groups + Inbox; pinned root;
   done omitted; snoozed included; no overlapping siblings; cycle input skipped; edge geometry
   is finite and anchored on the borders; layout is stable across repeated calls.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- No `egui` painter calls in `layout.rs`; it only returns geometry.
- `cargo test` covers the no-overlap property for every count above.

## Out of scope

Animation, rendering, drag interactions (100), zoom/camera transforms (the canvas converts world
rects to screen). Free-form resizing and child-level pinning are deliberately unsupported.
