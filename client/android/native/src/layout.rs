//! Where the immersive screens go (ADR 0009). Pure, so it is tested on the
//! host.

/// Screen aspect ratio (width / height).
pub const ASPECT: f32 = 16.0 / 9.0;
/// Bounds the user can adjust within.
pub const RADIUS_RANGE: (f32, f32) = (1.0, 3.5);
pub const WIDTH_RANGE: (f32, f32) = (0.8, 3.5);

/// Screen arrangement: curved screens on a cylinder around the viewer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// Distance from the eyes to the screens' surface, metres.
    pub radius: f32,
    /// Width of one screen along the arc, metres.
    pub width: f32,
    /// Gap between neighbouring screens, radians.
    pub gap: f32,
}

impl Default for Layout {
    /// ≈ a 65" TV at 1.7 m.
    fn default() -> Self {
        Self {
            radius: 1.7,
            width: 1.6,
            gap: 0.05,
        }
    }
}

impl Layout {
    /// Angle one screen spans, radians.
    pub fn arc(&self) -> f32 {
        self.width / self.radius
    }

    pub fn height(&self) -> f32 {
        self.width / ASPECT
    }

    /// Yaw (radians, positive = left) of screen `index` of `count`, laid out
    /// left to right and centred on the forward direction.
    pub fn yaw(&self, index: usize, count: usize) -> f32 {
        let arc = self.arc();
        let total = count as f32 * arc + count.saturating_sub(1) as f32 * self.gap;
        total / 2.0 - arc / 2.0 - index as f32 * (arc + self.gap)
    }

    /// Where a ray from `origin` along `dir` (OpenXR LOCAL space: -Z forward,
    /// +Y up) hits screen `index` of `count`: `(index, u, v)` with u, v in
    /// 0..1 from the screen's top-left corner. `None` when it misses.
    pub fn hit(&self, origin: [f32; 3], dir: [f32; 3], count: usize) -> Option<(usize, f32, f32)> {
        let [ox, oy, oz] = origin;
        let [dx, dy, dz] = dir;
        // Vertical cylinder through the origin: solve |o + t·d|xz = radius.
        let a = dx * dx + dz * dz;
        if a < 1e-9 {
            return None; // pointing straight up/down
        }
        let b = 2.0 * (ox * dx + oz * dz);
        let c = ox * ox + oz * oz - self.radius * self.radius;
        let disc = b * b - 4.0 * a * c;
        if disc < 0.0 {
            return None;
        }
        let t = (-b + disc.sqrt()) / (2.0 * a);
        if t <= 0.0 {
            return None;
        }
        let (px, py, pz) = (ox + t * dx, oy + t * dy, oz + t * dz);
        let yaw = (-px).atan2(-pz);
        let arc = self.arc();
        let v = 0.5 - py / self.height();
        if !(0.0..=1.0).contains(&v) {
            return None;
        }
        (0..count).find_map(|i| {
            let u = (self.yaw(i, count) + arc / 2.0 - yaw) / arc;
            (0.0..=1.0).contains(&u).then_some((i, u, v))
        })
    }

    /// Apply a thumbstick for `dt` seconds: y > 0 pushes the screens away,
    /// x > 0 makes them larger. Dead zone 0.3; ~0.8 m/s at full deflection.
    pub fn adjust(&mut self, stick_x: f32, stick_y: f32, dt: f32) -> bool {
        let rate = 0.8 * dt;
        let mut changed = false;
        if stick_y.abs() > 0.3 {
            let r = (self.radius + stick_y * rate).clamp(RADIUS_RANGE.0, RADIUS_RANGE.1);
            changed |= r != self.radius;
            self.radius = r;
        }
        if stick_x.abs() > 0.3 {
            let w = (self.width + stick_x * rate).clamp(WIDTH_RANGE.0, WIDTH_RANGE.1);
            changed |= w != self.width;
            self.width = w;
        }
        changed
    }

    /// `"radius width"` for the settings file.
    pub fn to_line(&self) -> String {
        format!("{} {}", self.radius, self.width)
    }

    /// Parse [`to_line`](Self::to_line); out-of-range values are clamped,
    /// garbage falls back to the default.
    pub fn from_line(line: &str) -> Self {
        let mut it = line.split_whitespace().map(str::parse::<f32>);
        match (it.next(), it.next()) {
            (Some(Ok(r)), Some(Ok(w))) if r.is_finite() && w.is_finite() => Self {
                radius: r.clamp(RADIUS_RANGE.0, RADIUS_RANGE.1),
                width: w.clamp(WIDTH_RANGE.0, WIDTH_RANGE.1),
                ..Self::default()
            },
            _ => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn screens_are_centred_and_ordered_left_to_right() {
        let l = Layout::default();
        assert!(
            l.yaw(0, 1).abs() < 1e-6,
            "a single screen is straight ahead"
        );
        let (a, b) = (l.yaw(0, 2), l.yaw(1, 2));
        assert!(a > 0.0 && b < 0.0 && (a + b).abs() < 1e-6, "{a} {b}");
        assert!((a - b - (l.arc() + l.gap)).abs() < 1e-6);
        assert!(l.yaw(1, 3).abs() < 1e-6, "middle of three is ahead");
    }

    #[test]
    fn looking_straight_ahead_hits_the_centre_of_a_single_screen() {
        let (i, u, v) = Layout::default()
            .hit([0.0; 3], [0.0, 0.0, -1.0], 1)
            .unwrap();
        assert_eq!(i, 0);
        assert!(close(u, 0.5) && close(v, 0.5), "{u} {v}");
    }

    #[test]
    fn left_and_up_map_to_the_top_left() {
        let (_, u, v) = Layout::default()
            .hit([0.0; 3], [-0.2, 0.1, -1.0], 1)
            .unwrap();
        assert!(u < 0.5 && v < 0.5, "{u} {v}");
    }

    #[test]
    fn two_screens_split_left_and_right() {
        let l = Layout::default();
        let yaw0 = l.yaw(0, 2);
        let (i, u, _) = l.hit([0.0; 3], [-yaw0.sin(), 0.0, -yaw0.cos()], 2).unwrap();
        assert_eq!(i, 0);
        assert!(close(u, 0.5));
        assert_eq!(l.hit([0.0; 3], [0.0, 0.0, -1.0], 3).unwrap().0, 1);
    }

    #[test]
    fn misses_return_none() {
        let l = Layout::default();
        assert!(l.hit([0.0; 3], [0.0, 0.0, 1.0], 1).is_none(), "behind");
        assert!(l.hit([0.0; 3], [0.0, 1.0, 0.0], 1).is_none(), "straight up");
        assert!(l.hit([0.0; 3], [0.0, 0.9, -1.0], 1).is_none(), "above");
    }

    #[test]
    fn a_ray_from_a_hand_off_centre_still_hits() {
        let (_, u, v) = Layout::default()
            .hit([0.2, -0.3, -0.2], [0.0, 0.0, -1.0], 1)
            .unwrap();
        assert!(u > 0.5 && v > 0.5, "{u} {v}");
    }

    #[test]
    fn hit_testing_follows_the_adjusted_layout() {
        let mut l = Layout::default();
        // Bigger screens: a ray that missed the edge now lands inside.
        let edge = [-(0.55f32).sin(), 0.0, -(0.55f32).cos()];
        assert!(l.hit([0.0; 3], edge, 1).is_none());
        for _ in 0..100 {
            l.adjust(1.0, 0.0, 0.05);
        }
        assert!(l.hit([0.0; 3], edge, 1).is_some());
    }

    #[test]
    fn adjust_respects_dead_zone_and_bounds() {
        let mut l = Layout::default();
        assert!(!l.adjust(0.2, -0.2, 1.0), "inside the dead zone");
        for _ in 0..1000 {
            l.adjust(-1.0, -1.0, 0.1);
        }
        assert_eq!((l.radius, l.width), (RADIUS_RANGE.0, WIDTH_RANGE.0));
        for _ in 0..1000 {
            l.adjust(1.0, 1.0, 0.1);
        }
        assert_eq!((l.radius, l.width), (RADIUS_RANGE.1, WIDTH_RANGE.1));
    }

    #[test]
    fn settings_roundtrip_and_reject_garbage() {
        let l = Layout {
            radius: 2.25,
            width: 2.0,
            ..Layout::default()
        };
        assert_eq!(Layout::from_line(&l.to_line()), l);
        assert_eq!(Layout::from_line("nonsense"), Layout::default());
        assert_eq!(Layout::from_line("NaN 2"), Layout::default());
        assert_eq!(Layout::from_line("99 99").radius, RADIUS_RANGE.1);
    }
}
