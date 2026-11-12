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
}
