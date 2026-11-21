use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use serde::{Deserialize, Serialize};

pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 2.5;

const FIT_PADDING_SCREEN: f32 = 72.0;
const GRID_BASE_STEP: f32 = 32.0;
const GRID_MIN_SCREEN_STEP: f32 = 16.0;
const POSITION_LIMIT: f32 = 1_000_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Camera {
    pub center: [f32; 2],
    pub zoom: f32,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            center: [0.0, 0.0],
            zoom: 1.0,
        }
    }
}

impl Camera {
    pub fn center_pos(&self) -> Pos2 {
        pos2(self.center[0], self.center[1])
    }

    pub fn world_to_screen(&self, world: Pos2, canvas: Rect) -> Pos2 {
        canvas.center() + (world - self.center_pos()) * self.zoom
    }

    pub fn screen_to_world(&self, screen: Pos2, canvas: Rect) -> Pos2 {
        self.center_pos() + (screen - canvas.center()) / self.zoom
    }

    pub fn pan_screen(&mut self, screen_delta: Vec2) {
        if !screen_delta.x.is_finite() || !screen_delta.y.is_finite() {
            return;
        }
        self.center[0] -= screen_delta.x / self.zoom;
        self.center[1] -= screen_delta.y / self.zoom;
    }

    pub fn zoom_at(&mut self, pointer: Pos2, canvas: Rect, factor: f32) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let anchor = self.screen_to_world(pointer, canvas);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let offset = pointer - canvas.center();
        self.center[0] = anchor.x - offset.x / self.zoom;
        self.center[1] = anchor.y - offset.y / self.zoom;
    }

    pub fn fit_to_bounds(&mut self, bounds: Rect, canvas: Rect) {
        if !bounds.is_finite() || bounds.width() <= 0.0 || bounds.height() <= 0.0 {
            return;
        }
        let usable = vec2(
            (canvas.width() - FIT_PADDING_SCREEN * 2.0).max(64.0),
            (canvas.height() - FIT_PADDING_SCREEN * 2.0).max(64.0),
        );
        let zoom_x = usable.x / bounds.width();
        let zoom_y = usable.y / bounds.height();
        self.zoom = zoom_x.min(zoom_y).clamp(MIN_ZOOM, 1.0);
        self.center = [bounds.center().x, bounds.center().y];
    }

    pub fn reset_to_origin(&mut self) {
        self.center = [0.0, 0.0];
        self.zoom = 1.0;
    }

    pub fn grid_step(&self) -> f32 {
        let mut step = GRID_BASE_STEP;
        let mut guard = 0;
        while step * self.zoom < GRID_MIN_SCREEN_STEP && guard < 32 {
            step *= 2.0;
            guard += 1;
        }
        step
    }

    pub fn sanitize(&mut self) {
        if !self.center.iter().all(|value| value.is_finite()) {
            self.center = [0.0, 0.0];
        }
        self.center[0] = self.center[0].clamp(-POSITION_LIMIT, POSITION_LIMIT);
        self.center[1] = self.center[1].clamp(-POSITION_LIMIT, POSITION_LIMIT);
        if !self.zoom.is_finite() || self.zoom <= 0.0 {
            self.zoom = 1.0;
        }
        self.zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Rect {
        Rect::from_min_max(pos2(0.0, 60.0), pos2(1280.0, 800.0))
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.01,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn world_screen_round_trip() {
        let camera = Camera {
            center: [120.0, -45.0],
            zoom: 1.75,
        };
        let world = pos2(-240.5, 310.25);
        let screen = camera.world_to_screen(world, canvas());
        let round_trip = camera.screen_to_world(screen, canvas());
        assert_close(round_trip.x, world.x);
        assert_close(round_trip.y, world.y);
    }

    #[test]
    fn zoom_keeps_world_point_under_pointer() {
        let mut camera = Camera {
            center: [40.0, -20.0],
            zoom: 1.0,
        };
        let viewport = canvas();
        let pointer = pos2(910.0, 260.0);
        let before = camera.screen_to_world(pointer, viewport);
        camera.zoom_at(pointer, viewport, 1.8);
        let after = camera.screen_to_world(pointer, viewport);
        assert_close(before.x, after.x);
        assert_close(before.y, after.y);
    }

    #[test]
    fn zoom_is_clamped() {
        let mut camera = Camera::default();
        camera.zoom_at(pos2(640.0, 400.0), canvas(), 1000.0);
        assert_eq!(camera.zoom, MAX_ZOOM);
        camera.zoom_at(pos2(640.0, 400.0), canvas(), 0.00001);
        assert_eq!(camera.zoom, MIN_ZOOM);
    }

    #[test]
    fn panning_follows_pointer() {
        let mut camera = Camera {
            center: [10.0, 20.0],
            zoom: 2.0,
        };
        let viewport = canvas();
        let world = pos2(100.0, 100.0);
        let before = camera.world_to_screen(world, viewport);
        let delta = vec2(48.0, -26.0);
        camera.pan_screen(delta);
        let after = camera.world_to_screen(world, viewport);
        assert_close(after.x - before.x, delta.x);
        assert_close(after.y - before.y, delta.y);
    }

    #[test]
    fn fit_puts_bounds_center_at_view_center() {
        let mut camera = Camera::default();
        let bounds = Rect::from_min_max(pos2(-500.0, -300.0), pos2(500.0, 300.0));
        camera.fit_to_bounds(bounds, canvas());
        assert_close(camera.center[0], 0.0);
        assert_close(camera.center[1], 0.0);
        assert!(camera.zoom <= 1.0);
        assert!(camera.zoom >= MIN_ZOOM);
    }

    #[test]
    fn grid_step_adapts_to_zoom() {
        let camera = Camera {
            center: [0.0, 0.0],
            zoom: 1.0,
        };
        assert_eq!(camera.grid_step(), 32.0);
        let camera = Camera {
            center: [0.0, 0.0],
            zoom: 0.25,
        };
        assert_eq!(camera.grid_step(), 64.0);
    }
}
