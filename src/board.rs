use std::collections::HashSet;

use eframe::egui::{Rect, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::theme;

pub type NoteId = u64;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: NoteId,
    pub pos: [f32; 2],
    pub size: [f32; 2],
    pub text: String,
    pub color: u8,
}

impl Note {
    pub fn new(id: NoteId, pos: [f32; 2], color: u8, text: impl Into<String>) -> Self {
        Self {
            id,
            pos,
            size: [theme::NOTE_WIDTH, theme::NOTE_HEIGHT],
            text: text.into(),
            color,
        }
    }

    pub fn rect(&self) -> Rect {
        Rect::from_min_size(
            pos2(self.pos[0], self.pos[1]),
            vec2(self.size[0], self.size[1]),
        )
    }

    pub fn palette(&self) -> theme::NotePalette {
        theme::note_palette(self.color)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub notes: Vec<Note>,
    pub next_id: NoteId,
}

impl Default for Board {
    fn default() -> Self {
        Self {
            notes: Vec::new(),
            next_id: 1,
        }
    }
}

impl Board {
    pub fn seeded() -> Self {
        let notes = vec![
            Note::new(
                1,
                [-470.0, -140.0],
                0,
                "Welcome to Rustboard\n\nDrag this header to move me.\nClick the note text to edit.",
            ),
            Note::new(
                2,
                [-135.0, -225.0],
                1,
                "Navigate\n\nScroll to zoom.\nRight or middle drag to pan.\n\nYour board saves automatically.",
            ),
            Note::new(
                3,
                [-325.0, 105.0],
                2,
                "Make it yours\n\n- Add notes\n- Reshape ideas\n- Find your flow",
            ),
            Note::new(
                4,
                [175.0, -25.0],
                3,
                "Shortcut\n\nCtrl+N adds a note.\nDelete removes the selected note when you're not typing.",
            ),
        ];
        Self { notes, next_id: 5 }
    }

    pub fn add_note(&mut self, center: [f32; 2], cascade: [f32; 2]) -> NoteId {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let color = (id as usize % theme::NOTE_PALETTE.len()) as u8;
        let pos = [
            finite_or(center[0] + cascade[0] - theme::NOTE_WIDTH * 0.5, 0.0),
            finite_or(center[1] + cascade[1] - theme::NOTE_HEIGHT * 0.5, 0.0),
        ];
        self.notes.push(Note::new(id, pos, color, ""));
        id
    }

    pub fn remove(&mut self, id: NoteId) -> bool {
        let before = self.notes.len();
        self.notes.retain(|note| note.id != id);
        self.notes.len() != before
    }

    pub fn bring_to_front(&mut self, id: NoteId) {
        if let Some(index) = self.notes.iter().position(|note| note.id == id)
            && index + 1 != self.notes.len()
        {
            let note = self.notes.remove(index);
            self.notes.push(note);
        }
    }

    pub fn bounds(&self) -> Option<Rect> {
        self.notes
            .iter()
            .map(Note::rect)
            .reduce(|union, rect| union.union(rect))
    }

    /// Restore guard for state coming from disk or a future schema version.
    pub fn sanitize(&mut self) {
        let mut next_id = self.next_id.max(1);
        let mut seen = HashSet::new();
        let mut clean = Vec::with_capacity(self.notes.len());

        for mut note in self.notes.drain(..) {
            if !note.pos.iter().all(|value| value.is_finite()) {
                continue;
            }
            if !note
                .size
                .iter()
                .all(|value| value.is_finite() && *value > 0.0)
            {
                note.size = [theme::NOTE_WIDTH, theme::NOTE_HEIGHT];
            }
            note.size[0] = note.size[0].clamp(48.0, 2000.0);
            note.size[1] = note.size[1].clamp(48.0, 2000.0);
            note.pos[0] = note.pos[0].clamp(-1_000_000.0, 1_000_000.0);
            note.pos[1] = note.pos[1].clamp(-1_000_000.0, 1_000_000.0);

            if note.id == 0 || seen.contains(&note.id) {
                while note.id == 0 || seen.contains(&note.id) {
                    note.id = next_id;
                    next_id = next_id.saturating_add(1);
                }
            }
            seen.insert(note.id);
            next_id = next_id.max(note.id.saturating_add(1));

            note.color = (note.color as usize % theme::NOTE_PALETTE.len()) as u8;
            clean.push(note);
        }

        self.notes = clean;
        self.next_id = next_id.max(1);
    }
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_board_has_unique_notes() {
        let board = Board::seeded();
        let ids: HashSet<_> = board.notes.iter().map(|note| note.id).collect();
        assert_eq!(ids.len(), board.notes.len());
        assert!(board.notes.iter().all(|note| !note.text.is_empty()));
        assert!(board.next_id > ids.iter().copied().max().unwrap_or(0));
    }

    #[test]
    fn add_remove_and_reorder_notes() {
        let mut board = Board::default();
        let first = board.add_note([0.0, 0.0], [0.0, 0.0]);
        let second = board.add_note([0.0, 0.0], [24.0, 18.0]);

        assert_ne!(first, second);
        assert_eq!(board.notes.len(), 2);
        assert_eq!(board.notes.last().map(|note| note.id), Some(second));

        board.bring_to_front(first);
        assert_eq!(board.notes.last().map(|note| note.id), Some(first));

        assert!(board.remove(first));
        assert!(!board.remove(first));
        assert_eq!(board.notes.len(), 1);
        assert_eq!(board.bounds(), Some(board.notes[0].rect()));
    }

    #[test]
    fn bounds_unions_every_note() {
        let mut board = Board::default();
        board.add_note([0.0, 0.0], [0.0, 0.0]);
        board.add_note([500.0, 300.0], [0.0, 0.0]);
        let bounds = board.bounds().expect("two notes have bounds");
        assert_eq!(bounds.min.x, -theme::NOTE_WIDTH * 0.5);
        assert!(bounds.max.x > 500.0);
    }

    #[test]
    fn sanitize_repairs_bad_restored_state() {
        let mut board = Board {
            notes: vec![
                Note {
                    id: 1,
                    pos: [f32::NAN, 0.0],
                    size: [theme::NOTE_WIDTH, theme::NOTE_HEIGHT],
                    text: "drop me".to_owned(),
                    color: 0,
                },
                Note {
                    id: 0,
                    pos: [0.0, 0.0],
                    size: [-10.0, f32::INFINITY],
                    text: "fix me".to_owned(),
                    color: 99,
                },
                Note {
                    id: 2,
                    pos: [10.0, 10.0],
                    size: [theme::NOTE_WIDTH, theme::NOTE_HEIGHT],
                    text: "duplicate".to_owned(),
                    color: 2,
                },
                Note {
                    id: 2,
                    pos: [20.0, 20.0],
                    size: [theme::NOTE_WIDTH, theme::NOTE_HEIGHT],
                    text: "also duplicate".to_owned(),
                    color: 3,
                },
            ],
            next_id: 2,
        };
        board.sanitize();

        assert_eq!(board.notes.len(), 3);
        let ids: HashSet<_> = board.notes.iter().map(|note| note.id).collect();
        assert_eq!(ids.len(), 3);
        assert!(board.notes.iter().all(|note| note.rect().is_finite()));
        assert!(
            board
                .notes
                .iter()
                .all(|note| note.palette().fill != theme::CANVAS_BG)
        );
        assert!(board.next_id > 3);
    }

    #[test]
    fn board_round_trips_through_ron() {
        let board = Board::seeded();
        let encoded = ron::to_string(&board).expect("serialize board");
        let decoded: Board = ron::from_str(&encoded).expect("deserialize board");
        assert_eq!(board, decoded);
    }
}
