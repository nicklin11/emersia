//! Where the immersive screens go (ADR 0009). Pure, so it is tested on the
//! host.

/// Distance from the eyes to the screens' surface, metres.
pub const RADIUS: f32 = 1.7;
/// Width of one screen along the arc, metres (≈ a 65" TV at this distance).
pub const SCREEN_WIDTH: f32 = 1.6;
/// Gap between neighbouring screens, radians.
pub const GAP: f32 = 0.05;

/// Yaw (radians, positive = left) of screen `index` of `count`, laid out
/// left to right and centred on the forward direction.
pub fn screen_yaw(index: usize, count: usize) -> f32 {
    let arc = SCREEN_WIDTH / RADIUS;
    let total = count as f32 * arc + count.saturating_sub(1) as f32 * GAP;
    total / 2.0 - arc / 2.0 - index as f32 * (arc + GAP)
}

/// Screen aspect ratio (width / height).
pub const ASPECT: f32 = 16.0 / 9.0;

/// Where a ray from `origin` along `dir` (OpenXR LOCAL space: -Z forward,
/// +Y up) hits screen `index` of `count`: `(index, u, v)` with u, v in 0..1
/// from the screen's top-left corner. `None` when it misses every screen.
pub fn hit(origin: [f32; 3], dir: [f32; 3], count: usize) -> Option<(usize, f32, f32)> {
    let [ox, oy, oz] = origin;
    let [dx, dy, dz] = dir;
    // Vertical cylinder through the origin with RADIUS: solve |o + t·d|xz = R.
    let a = dx * dx + dz * dz;
    if a < 1e-9 {
        return None; // pointing straight up/down
    }
    let b = 2.0 * (ox * dx + oz * dz);
    let c = ox * ox + oz * oz - RADIUS * RADIUS;
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let t = (-b + disc.sqrt()) / (2.0 * a);
    if t <= 0.0 {
        return None;
    }
    let (px, py, pz) = (ox + t * dx, oy + t * dy, oz + t * dz);
    // Yaw of the hit (positive = left), as screen_yaw measures it.
    let yaw = (-px).atan2(-pz);
    let arc = SCREEN_WIDTH / RADIUS;
    let height = SCREEN_WIDTH / ASPECT;
    let v = 0.5 - py / height;
    if !(0.0..=1.0).contains(&v) {
        return None;
    }
    (0..count).find_map(|i| {
        let u = (screen_yaw(i, count) + arc / 2.0 - yaw) / arc;
        (0.0..=1.0).contains(&u).then_some((i, u, v))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screens_are_centred_and_ordered_left_to_right() {
        assert!(
            screen_yaw(0, 1).abs() < 1e-6,
            "a single screen is straight ahead"
        );
        let (l, r) = (screen_yaw(0, 2), screen_yaw(1, 2));
        assert!(l > 0.0 && r < 0.0 && (l + r).abs() < 1e-6, "{l} {r}");
        assert!((l - r - (SCREEN_WIDTH / RADIUS + GAP)).abs() < 1e-6);
        assert!(screen_yaw(1, 3).abs() < 1e-6, "middle of three is ahead");
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn looking_straight_ahead_hits_the_centre_of_a_single_screen() {
        let (i, u, v) = hit([0.0; 3], [0.0, 0.0, -1.0], 1).unwrap();
        assert_eq!(i, 0);
        assert!(close(u, 0.5) && close(v, 0.5), "{u} {v}");
    }

    #[test]
    fn left_and_up_map_to_the_top_left() {
        // Slightly left and up of centre.
        let (_, u, v) = hit([0.0; 3], [-0.2, 0.1, -1.0], 1).unwrap();
        assert!(u < 0.5 && v < 0.5, "{u} {v}");
    }

    #[test]
    fn two_screens_split_left_and_right() {
        let yaw0 = screen_yaw(0, 2);
        let dir0 = [-yaw0.sin(), 0.0, -yaw0.cos()];
        let (i, u, _) = hit([0.0; 3], dir0, 2).unwrap();
        assert_eq!(i, 0);
        assert!(close(u, 0.5));
        let (i, _, _) = hit([0.0; 3], [0.0, 0.0, -1.0], 3).unwrap();
        assert_eq!(i, 1, "middle of three is ahead");
    }

    #[test]
    fn misses_return_none() {
        assert!(hit([0.0; 3], [0.0, 0.0, 1.0], 1).is_none(), "behind");
        assert!(hit([0.0; 3], [0.0, 1.0, 0.0], 1).is_none(), "straight up");
        assert!(
            hit([0.0; 3], [0.0, 0.9, -1.0], 1).is_none(),
            "above the screen"
        );
    }

    #[test]
    fn a_ray_from_a_hand_off_centre_still_hits() {
        // Right hand, lower than the eyes, pointing forward.
        let (_, u, v) = hit([0.2, -0.3, -0.2], [0.0, 0.0, -1.0], 1).unwrap();
        assert!(u > 0.5 && v > 0.5, "{u} {v}");
    }
}
