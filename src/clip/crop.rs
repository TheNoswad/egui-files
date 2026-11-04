//! The rectangle kept of a video's picture, and how dragging changes it.

use serde::{Deserialize, Serialize};

use super::ffmpeg::Turn;

/// The smallest a side can be dragged to, of the picture's.
const MIN_SIDE: f32 = 0.02;

/// A part of the picture as shown, in fractions of its width and height.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Crop {
    pub const FULL: Self = Self {
        x: 0.0,
        y: 0.0,
        w: 1.0,
        h: 1.0,
    };

    /// The largest rectangle of this shape, in the middle. `ratio` is its
    /// width over its height, both as fractions of the picture's.
    pub fn centered(ratio: f32) -> Self {
        let (w, h) = if ratio >= 1.0 {
            (1.0, 1.0 / ratio)
        } else {
            (ratio, 1.0)
        };
        Self {
            x: (1.0 - w) / 2.0,
            y: (1.0 - h) / 2.0,
            w,
            h,
        }
    }

    /// The pixels of a `width` by `height` picture it covers: width, height,
    /// x and y. All even, as most encoders need, and inside the picture.
    pub fn pixels(&self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let even = |value: f32| (value / 2.0).round() as u32 * 2;
        let side = |fraction: f32, whole: u32| {
            even(fraction * whole as f32)
                .max(2)
                .min(whole & !1)
                .max(2.min(whole))
        };
        let (w, h) = (side(self.w, width), side(self.h, height));
        let x = even(self.x * width as f32).min(width.saturating_sub(w));
        let y = even(self.y * height as f32).min(height.saturating_sub(h));
        (w, h, x, y)
    }
}

/// How the picture is shown once turned and mirrored, in the order an export
/// does them: turned, then mirrored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub turn: Turn,
    pub flip_horizontal: bool,
    pub flip_vertical: bool,
}

impl View {
    /// Where on the shown picture a point of the original goes, each in
    /// fractions of the picture's width and height.
    pub fn to_view(self, (x, y): (f32, f32)) -> (f32, f32) {
        let (x, y) = match self.turn {
            Turn::None => (x, y),
            Turn::Quarter => (1.0 - y, x),
            Turn::Half => (1.0 - x, 1.0 - y),
            Turn::ThreeQuarters => (y, 1.0 - x),
        };
        (
            if self.flip_horizontal { 1.0 - x } else { x },
            if self.flip_vertical { 1.0 - y } else { y },
        )
    }

    /// Which point of the original a point of the shown picture is.
    pub fn to_source(self, (x, y): (f32, f32)) -> (f32, f32) {
        let x = if self.flip_horizontal { 1.0 - x } else { x };
        let y = if self.flip_vertical { 1.0 - y } else { y };
        match self.turn {
            Turn::None => (x, y),
            Turn::Quarter => (y, 1.0 - x),
            Turn::Half => (1.0 - x, 1.0 - y),
            Turn::ThreeQuarters => (1.0 - y, x),
        }
    }

    /// Its width and height trade places.
    pub fn swaps_sides(&self) -> bool {
        self.turn.swaps_sides()
    }

    /// The way a part of the crop looks to go, once shown like this.
    fn shown_grab(&self, grab: Grab) -> Grab {
        let (x, y) = grab.direction();
        let (x, y) = self.to_view((0.5 + x * 0.5, 0.5 + y * 0.5));
        Grab::from_direction(x * 2.0 - 1.0, y * 2.0 - 1.0)
    }

    /// The pointer for a part of the crop, as it looks on screen.
    pub fn cursor(&self, grab: Grab) -> egui::CursorIcon {
        self.shown_grab(grab).cursor()
    }
}

/// What a press on the rectangle takes hold of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grab {
    Move,
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}

impl Grab {
    /// What's at `(x, y)` in fractions of the picture, within a reach of each
    /// edge. With a shape to keep, there are only the corners to resize by.
    pub fn at(crop: &Crop, (x, y): (f32, f32), reach: (f32, f32), locked: bool) -> Option<Self> {
        let (left, right) = (crop.x, crop.x + crop.w);
        let (top, bottom) = (crop.y, crop.y + crop.h);
        let (reach_x, reach_y) = reach;
        let inside_x = x >= left - reach_x && x <= right + reach_x;
        let inside_y = y >= top - reach_y && y <= bottom + reach_y;
        if !inside_x || !inside_y {
            return None;
        }
        // The closer edge, for a rectangle smaller than the reach.
        let west = (x - left).abs() <= reach_x && (x - left).abs() <= (x - right).abs();
        let east = !west && (x - right).abs() <= reach_x;
        let north = (y - top).abs() <= reach_y && (y - top).abs() <= (y - bottom).abs();
        let south = !north && (y - bottom).abs() <= reach_y;
        Some(match (north, south, west, east) {
            (true, _, true, _) => Self::NorthWest,
            (true, _, _, true) => Self::NorthEast,
            (_, true, true, _) => Self::SouthWest,
            (_, true, _, true) => Self::SouthEast,
            _ if locked => Self::Move,
            (true, ..) => Self::North,
            (_, true, ..) => Self::South,
            (_, _, true, _) => Self::West,
            (_, _, _, true) => Self::East,
            _ => Self::Move,
        })
    }

    /// Which way out of the rectangle this part faces.
    fn direction(self) -> (f32, f32) {
        match self {
            Self::Move => (0.0, 0.0),
            Self::North => (0.0, -1.0),
            Self::South => (0.0, 1.0),
            Self::East => (1.0, 0.0),
            Self::West => (-1.0, 0.0),
            Self::NorthEast => (1.0, -1.0),
            Self::NorthWest => (-1.0, -1.0),
            Self::SouthEast => (1.0, 1.0),
            Self::SouthWest => (-1.0, 1.0),
        }
    }

    fn from_direction(x: f32, y: f32) -> Self {
        match (x.round() as i32, y.round() as i32) {
            (0, -1) => Self::North,
            (0, 1) => Self::South,
            (1, 0) => Self::East,
            (-1, 0) => Self::West,
            (1, -1) => Self::NorthEast,
            (-1, -1) => Self::NorthWest,
            (1, 1) => Self::SouthEast,
            (-1, 1) => Self::SouthWest,
            _ => Self::Move,
        }
    }

    pub fn cursor(self) -> egui::CursorIcon {
        use egui::CursorIcon as C;
        match self {
            Self::Move => C::Move,
            Self::North | Self::South => C::ResizeVertical,
            Self::East | Self::West => C::ResizeHorizontal,
            Self::NorthWest | Self::SouthEast => C::ResizeNwSe,
            Self::NorthEast | Self::SouthWest => C::ResizeNeSw,
        }
    }
}

/// `start` after dragging what `grab` took by `(dx, dy)`, in fractions of the
/// picture. With a `ratio`, it keeps that shape (see [`Crop::centered`]).
pub fn dragged(start: Crop, grab: Grab, (dx, dy): (f32, f32), ratio: Option<f32>) -> Crop {
    if grab == Grab::Move {
        return Crop {
            x: (start.x + dx).clamp(0.0, 1.0 - start.w),
            y: (start.y + dy).clamp(0.0, 1.0 - start.h),
            ..start
        };
    }
    let west = matches!(grab, Grab::West | Grab::NorthWest | Grab::SouthWest);
    let east = matches!(grab, Grab::East | Grab::NorthEast | Grab::SouthEast);
    let north = matches!(grab, Grab::North | Grab::NorthEast | Grab::NorthWest);
    let south = matches!(grab, Grab::South | Grab::SouthEast | Grab::SouthWest);
    if let Some(ratio) = ratio {
        return dragged_keeping_shape(start, (dx, west, east), (dy, north), south, ratio);
    }
    let (mut left, mut right) = (start.x, start.x + start.w);
    let (mut top, mut bottom) = (start.y, start.y + start.h);
    if west {
        left = (left + dx).clamp(0.0, right - MIN_SIDE);
    }
    if east {
        right = (right + dx).clamp(left + MIN_SIDE, 1.0);
    }
    if north {
        top = (top + dy).clamp(0.0, bottom - MIN_SIDE);
    }
    if south {
        bottom = (bottom + dy).clamp(top + MIN_SIDE, 1.0);
    }
    Crop {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    }
}

/// A corner's drag with its opposite corner held still: the rectangle follows
/// whichever way the pointer has gone further, within the picture.
fn dragged_keeping_shape(
    start: Crop,
    (dx, west, _east): (f32, bool, bool),
    (dy, north): (f32, bool),
    _south: bool,
    ratio: f32,
) -> Crop {
    let wider = if west { -dx } else { dx };
    let taller = if north { -dy } else { dy };
    let w = (start.w + wider).max((start.h + taller) * ratio);
    // Room from the held corner to the picture's edges.
    let room_x = if west {
        start.x + start.w
    } else {
        1.0 - start.x
    };
    let room_y = if north {
        start.y + start.h
    } else {
        1.0 - start.y
    };
    let smallest = MIN_SIDE.max(MIN_SIDE * ratio);
    let w = w.min(room_x).min(room_y * ratio).max(smallest);
    let h = w / ratio;
    Crop {
        x: if west { start.x + start.w - w } else { start.x },
        y: if north {
            start.y + start.h - h
        } else {
            start.y
        },
        w,
        h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REACH: (f32, f32) = (0.02, 0.02);
    const BOX: Crop = Crop {
        x: 0.2,
        y: 0.2,
        w: 0.4,
        h: 0.4,
    };

    fn close(a: Crop, b: Crop) -> bool {
        (a.x - b.x).abs() < 1e-5
            && (a.y - b.y).abs() < 1e-5
            && (a.w - b.w).abs() < 1e-5
            && (a.h - b.h).abs() < 1e-5
    }

    fn views() -> Vec<View> {
        let mut all = Vec::new();
        for turn in [Turn::None, Turn::Quarter, Turn::Half, Turn::ThreeQuarters] {
            for flip_horizontal in [false, true] {
                for flip_vertical in [false, true] {
                    all.push(View {
                        turn,
                        flip_horizontal,
                        flip_vertical,
                    });
                }
            }
        }
        all
    }

    #[test]
    fn a_view_undoes_itself() {
        for view in views() {
            let point = (0.25, 0.1);
            let (x, y) = view.to_source(view.to_view(point));
            assert!(
                (x - point.0).abs() < 1e-5 && (y - point.1).abs() < 1e-5,
                "{view:?}"
            );
        }
    }

    #[test]
    fn a_view_shows_what_the_export_makes() {
        // Matches `crops_turns_and_changes_speed`, which reads the exports'
        // pixels: where a video's top left corner ends up.
        let corner = |turn, flip_horizontal, flip_vertical| {
            View {
                turn,
                flip_horizontal,
                flip_vertical,
            }
            .to_view((0.0, 0.0))
        };
        assert_eq!(corner(Turn::None, false, false), (0.0, 0.0));
        assert_eq!(corner(Turn::Quarter, false, false), (1.0, 0.0));
        assert_eq!(corner(Turn::ThreeQuarters, false, false), (0.0, 1.0));
        assert_eq!(corner(Turn::Half, false, false), (1.0, 1.0));
        assert_eq!(corner(Turn::None, true, false), (1.0, 0.0));
        assert_eq!(corner(Turn::None, false, true), (0.0, 1.0));
        // Mirrored after turning: right after the turn, then back to the left.
        assert_eq!(corner(Turn::Quarter, true, false), (0.0, 0.0));
    }

    #[test]
    fn pointers_follow_the_picture() {
        let turned = View {
            turn: Turn::Quarter,
            flip_horizontal: false,
            flip_vertical: false,
        };
        // The original's top edge is on the right, once turned clockwise.
        assert_eq!(turned.shown_grab(Grab::North), Grab::East);
        assert_eq!(turned.shown_grab(Grab::NorthWest), Grab::NorthEast);
        assert_eq!(turned.shown_grab(Grab::Move), Grab::Move);
        let mirrored = View {
            turn: Turn::None,
            flip_horizontal: true,
            flip_vertical: false,
        };
        assert_eq!(mirrored.shown_grab(Grab::West), Grab::East);
    }

    #[test]
    fn a_shape_is_as_large_as_it_can_be_in_the_middle() {
        assert_eq!(Crop::centered(1.0), Crop::FULL);
        let wide = Crop::centered(2.0);
        assert_eq!((wide.w, wide.h, wide.y), (1.0, 0.5, 0.25));
        let tall = Crop::centered(0.5);
        assert_eq!((tall.w, tall.h, tall.x), (0.5, 1.0, 0.25));
    }

    #[test]
    fn pixels_are_even_and_inside() {
        assert_eq!(Crop::FULL.pixels(1920, 1080), (1920, 1080, 0, 0));
        // An odd picture can't be kept whole in even pixels.
        assert_eq!(Crop::FULL.pixels(641, 361), (640, 360, 0, 0));
        let (w, h, x, y) = Crop {
            x: 0.333,
            y: 0.111,
            w: 0.333,
            h: 0.777,
        }
        .pixels(1921, 1081);
        assert!(w % 2 == 0 && h % 2 == 0 && x % 2 == 0 && y % 2 == 0);
        assert!(x + w <= 1921 && y + h <= 1081);
        // Never nothing.
        let tiny = Crop {
            x: 0.999,
            y: 0.999,
            w: 0.0001,
            h: 0.0001,
        };
        let (w, h, x, y) = tiny.pixels(100, 100);
        assert_eq!((w, h), (2, 2));
        assert!(x + w <= 100 && y + h <= 100);
    }

    #[test]
    fn a_press_takes_what_it_is_near() {
        let at = |x, y| Grab::at(&BOX, (x, y), REACH, false);
        assert_eq!(at(0.4, 0.4), Some(Grab::Move));
        assert_eq!(at(0.2, 0.4), Some(Grab::West));
        assert_eq!(at(0.6, 0.4), Some(Grab::East));
        assert_eq!(at(0.4, 0.2), Some(Grab::North));
        assert_eq!(at(0.4, 0.6), Some(Grab::South));
        assert_eq!(at(0.2, 0.2), Some(Grab::NorthWest));
        assert_eq!(at(0.6, 0.6), Some(Grab::SouthEast));
        assert_eq!(at(0.9, 0.9), None);
        // Edges don't resize a kept shape; corners do.
        let locked = |x, y| Grab::at(&BOX, (x, y), REACH, true);
        assert_eq!(locked(0.2, 0.4), Some(Grab::Move));
        assert_eq!(locked(0.6, 0.6), Some(Grab::SouthEast));
    }

    #[test]
    fn dragging_stays_in_the_picture() {
        let moved = dragged(BOX, Grab::Move, (0.1, -0.1), None);
        assert!(close(
            moved,
            Crop {
                x: 0.3,
                y: 0.1,
                ..BOX
            }
        ));
        let far = dragged(BOX, Grab::Move, (9.0, -9.0), None);
        assert!(close(
            far,
            Crop {
                x: 0.6,
                y: 0.0,
                ..BOX
            }
        ));

        let east = dragged(BOX, Grab::East, (0.1, 0.5), None);
        assert!(close(east, Crop { w: 0.5, ..BOX }), "only that edge");
        let west = dragged(BOX, Grab::West, (-9.0, 0.0), None);
        assert!(close(
            west,
            Crop {
                x: 0.0,
                w: 0.6,
                ..BOX
            }
        ));
        // An edge can't pass the one across from it.
        let crossed = dragged(BOX, Grab::East, (-9.0, 0.0), None);
        assert!((crossed.w - MIN_SIDE).abs() < 1e-5 && crossed.x == BOX.x);
    }

    #[test]
    fn a_kept_shape_stays_that_shape() {
        let ratio = 2.0;
        let start = Crop {
            x: 0.2,
            y: 0.2,
            w: 0.4,
            h: 0.2,
        };
        let grown = dragged(start, Grab::SouthEast, (0.2, 0.0), Some(ratio));
        assert!(close(
            grown,
            Crop {
                w: 0.6,
                h: 0.3,
                ..start
            }
        ));
        // The opposite corner stays where it was.
        let west = dragged(start, Grab::NorthWest, (-0.1, 0.0), Some(ratio));
        assert!((west.x + west.w - 0.6).abs() < 1e-5);
        assert!((west.y + west.h - 0.4).abs() < 1e-5);
        assert!((west.w / west.h - ratio).abs() < 1e-4);
        // Out of room, it stops at the picture's edge, still the same shape.
        let huge = dragged(start, Grab::SouthEast, (9.0, 9.0), Some(ratio));
        assert!(huge.x + huge.w <= 1.0 + 1e-5 && huge.y + huge.h <= 1.0 + 1e-5);
        assert!((huge.w / huge.h - ratio).abs() < 1e-4);
        // And it can't vanish.
        let small = dragged(start, Grab::SouthEast, (-9.0, -9.0), Some(ratio));
        assert!(small.w > 0.0 && small.h > 0.0);
    }
}
