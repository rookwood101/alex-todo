# 100 — Canvas: nesting, arrows, snooze, done

**Goal:** render the layout tree and make it interactive: containers, chips, arrows, snooze
opacity, done animation, drag-to-reparent, link handles. No delete button.

**Depends on:** 070 (layout), 090 (composer). **Parallel with:** 110.
**Owns:** `src/canvas.rs`, small additions under `src/ui/**`. Coordinate any `src/app.rs` change
with 170.

**Read first:** this index; `src/canvas.rs` (camera, painting, input); `src/layout.rs`;
`src/theme.rs`.

## Steps

1. Convert `model::Board` into `layout::LayoutNote`/`LayoutEdge` (or add a small adapter), call
   `layout::solve`. Paint containers first (header + clipped child area), then leaves; convert
   world rects with the existing `Camera`.
2. Note chrome: first line as title, due chip (formatted in the board zone), priority dot,
   snooze icon. **Due/reminder never change opacity.** Snoozed → opacity 0.35 until
   `snooze_until`; done notes are omitted (the app animates them out on completion).
3. Arrows: draw `layout::edge_geometry` beziers under the notes with arrowheads; hit-test for
   selection. A selected note shows a small link handle; drag handle → target → emit
   `Command::Link`; click an arrow + Del → `Command::Unlink`.
4. Drag: header drag starts a move. Root notes pin at the drop position (`Command::Pin`);
   child notes show a drop ghost and reparent when released over a container
   (`Command::SetParent`), otherwise unparent to root. Highlight the target container. Reject
   self/descendant drops.
5. Keep: wheel zoom, right/middle pan, click-to-edit text, selected outline. Do not add any
   canvas-local delete affordance; the toolbar button is 170's job.
6. Headless tests: drag a root note pins it; drag a child onto a container reparents; drop on
   itself is rejected; snoozed note paints at 0.35; done note is not painted; arrow hit-test
   selects; Del on an arrow emits Unlink.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- Existing camera/zoom/pan tests still pass.
- No panics on missing parents/edges or a cycle in the read model.
- The edit-in-place `TextEdit` still works at every zoom level.

## Out of scope

Undo implementation/toast (050/170), composer (090), chat (130), free resize, multi-select,
marquee. Keep the rendering under ~16 ms for 200 visible notes.
