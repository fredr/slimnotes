//! Scrolling geometry: the overlay scrollbar (and the drag mapping that moves
//! it), plus the autoscroll ramp used when a text drag leaves the viewport.
//!
//! Pure functions over (total height, viewport height, scroll offset, track
//! length), so the painted thumb and the drag that scrolls by it can never
//! disagree.
//!
//! `viewport` and `track` are deliberately separate. The scrollable viewport is
//! the *inset* text area, but the track runs the full window height so the thumb
//! can reach the very top and bottom edges; the thumb's proportion comes from the
//! viewport, its travel from the track. Thumb offsets are relative to the top of
//! the track (i.e. the top of the window).
//!
//! `total` comes from the layout's exact `total_height` — virtualization
//! measures every line's height even when it defers shaping, so the thumb is
//! correctly sized from the first frame and doesn't drift as you scroll.

use std::time::Duration;

use gpui::{px, Bounds, Pixels, Point};

/// Visible thumb width.
pub(crate) const SCROLLBAR_W: f32 = 8.0;
/// Gap between the thumb and the right edge of the window.
pub(crate) const SCROLLBAR_MARGIN: f32 = 2.0;
/// Width of the clickable strip along the window's right edge. Wider than the
/// thumb so it stays easy to grab, the usual overlay-scrollbar affordance.
pub(crate) const SCROLLBAR_HIT_W: f32 = 14.0;
/// Floor on the thumb length, so it stays grabbable in a very long document.
pub(crate) const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// Where the thumb sits for a given scroll offset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ScrollbarGeom {
    /// Track length — the full window height.
    pub(crate) track: Pixels,
    /// Thumb top, relative to the top of the track.
    pub(crate) thumb_top: Pixels,
    pub(crate) thumb_len: Pixels,
}

/// The furthest the content can scroll.
pub(crate) fn max_scroll(total: Pixels, viewport: Pixels) -> Pixels {
    px((f32::from(total) - f32::from(viewport)).max(0.))
}

/// Whether a window-space point falls in the scrollbar's grab strip along the
/// window's right edge. Shared by every handler that must decide "scrollbar or
/// text?", so they can't disagree. `bounds` is the element's full rect.
pub(crate) fn in_scrollbar_strip(bounds: &Bounds<Pixels>, pos: Point<Pixels>) -> bool {
    bounds.contains(&pos)
        && pos.x >= bounds.origin.x + bounds.size.width - px(SCROLLBAR_HIT_W)
}

/// Thumb length: the visible fraction of the document, scaled to the track.
/// Shared by the geometry and its inverse so the two can't drift apart.
fn thumb_len(total: f32, viewport: f32, track: f32) -> f32 {
    (viewport / total * track).clamp(SCROLLBAR_MIN_THUMB.min(track), track)
}

/// Thumb geometry, or `None` when there is nothing to scroll (the document fits,
/// or the viewport/track is degenerate) — in which case no scrollbar is drawn.
pub(crate) fn scrollbar_geom(
    total: Pixels,
    viewport: Pixels,
    scroll_top: Pixels,
    track: Pixels,
) -> Option<ScrollbarGeom> {
    let (t, v, k) = (f32::from(total), f32::from(viewport), f32::from(track));
    if v <= 0. || k <= 0. || t <= v {
        return None;
    }
    let len = thumb_len(t, v, k);
    let travel = k - len;
    let frac = (f32::from(scroll_top) / (t - v)).clamp(0., 1.);
    Some(ScrollbarGeom {
        track: px(k),
        thumb_top: px(frac * travel),
        thumb_len: px(len),
    })
}

/// The scroll offset that places the thumb's top at `thumb_top` (measured from
/// the top of the track) — the inverse of [`scrollbar_geom`], so dragging the
/// thumb N px moves the content by `N * (total - viewport)/travel`. Clamped to
/// the scrollable range.
pub(crate) fn scroll_for_thumb_top(
    total: Pixels,
    viewport: Pixels,
    thumb_top: Pixels,
    track: Pixels,
) -> Pixels {
    let (t, v, k) = (f32::from(total), f32::from(viewport), f32::from(track));
    if v <= 0. || k <= 0. || t <= v {
        return px(0.);
    }
    let travel = k - thumb_len(t, v, k);
    if travel <= 0. {
        // The thumb fills the track (a track shorter than the minimum thumb),
        // so its position can't express an offset.
        return px(0.);
    }
    px((f32::from(thumb_top) / travel).clamp(0., 1.) * (t - v))
}

/// How often the drag-autoscroll loop ticks (~60Hz), which is what makes the
/// `AUTOSCROLL_*` steps below a speed rather than a distance.
pub(crate) const AUTOSCROLL_TICK: Duration = Duration::from_millis(16);
/// How far past the viewport edge the pointer must be for autoscroll to reach
/// full speed.
const AUTOSCROLL_RAMP: f32 = 160.0;
/// Step just past the edge — slow enough to place the end of a selection
/// precisely (~90 px/s).
const AUTOSCROLL_MIN: f32 = 1.5;
/// Step at full ramp (~1440 px/s), fast enough to cross a long note.
const AUTOSCROLL_MAX: f32 = 24.0;

/// How far to scroll in one autoscroll tick while drag-selecting, given the
/// pointer's y in *content-box* coordinates: negative to scroll up (the pointer
/// is above the viewport), positive to scroll down, zero while it is inside.
///
/// A function of position only, never of movement: that is precisely what lets
/// the caller keep scrolling while the pointer is held still outside the text,
/// which is the behaviour that makes selecting past the visible page possible.
/// The speed ramps with the overshoot so a small overshoot creeps and a big one
/// travels.
pub(crate) fn drag_scroll_step(y: Pixels, viewport: Pixels) -> Pixels {
    let (y, v) = (f32::from(y), f32::from(viewport));
    let over = if y < 0. {
        y
    } else if y > v {
        y - v
    } else {
        return px(0.);
    };
    let t = (over.abs() / AUTOSCROLL_RAMP).min(1.);
    px(over.signum() * (AUTOSCROLL_MIN + (AUTOSCROLL_MAX - AUTOSCROLL_MIN) * t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(p: Pixels) -> f32 {
        f32::from(p)
    }

    #[test]
    fn hidden_when_nothing_to_scroll() {
        // Document shorter than, or exactly equal to, the viewport.
        assert!(scrollbar_geom(px(300.), px(500.), px(0.), px(500.)).is_none());
        assert!(scrollbar_geom(px(500.), px(500.), px(0.), px(500.)).is_none());
        // Degenerate viewport.
        assert!(scrollbar_geom(px(1000.), px(0.), px(0.), px(500.)).is_none());
        assert!(scrollbar_geom(px(1000.), px(-10.), px(0.), px(500.)).is_none());
        // Degenerate track.
        assert!(scrollbar_geom(px(1000.), px(500.), px(0.), px(0.)).is_none());
    }

    #[test]
    fn thumb_is_proportional_and_tracks_scroll() {
        // Viewport is a quarter of the document, so the thumb is a quarter of
        // the track: 500/2000 * 500 = 125.
        let g = scrollbar_geom(px(2000.), px(500.), px(0.), px(500.)).unwrap();
        assert_eq!(f(g.track), 500.);
        assert_eq!(f(g.thumb_len), 125.);
        assert_eq!(f(g.thumb_top), 0.); // at the top

        let travel = 500. - 125.;
        // Half way down.
        let g = scrollbar_geom(px(2000.), px(500.), px(750.), px(500.)).unwrap();
        assert!((f(g.thumb_top) - travel / 2.).abs() < 0.01);
        // At the bottom the thumb is flush with the end of the track.
        let g = scrollbar_geom(px(2000.), px(500.), px(1500.), px(500.)).unwrap();
        assert!((f(g.thumb_top) - travel).abs() < 0.01);
        assert!(f(g.thumb_top) + f(g.thumb_len) <= 500.001);
    }

    #[test]
    fn track_is_independent_of_the_viewport() {
        // The real case: the text area is inset by CONTENT_PAD on both sides, so
        // the track (full window) is taller than the scrollable viewport.
        let (total, viewport, track) = (px(2000.), px(500.), px(564.));
        let g = scrollbar_geom(total, viewport, px(0.), track).unwrap();
        assert_eq!(f(g.track), 564.);
        // Proportion is viewport/total, scaled to the track: 500/2000 * 564.
        assert!((f(g.thumb_len) - 141.).abs() < 0.01);
        assert_eq!(f(g.thumb_top), 0.);
        // At max scroll the thumb is flush with the bottom of the *track*, i.e.
        // the bottom of the window — not the bottom of the inset content box.
        let g = scrollbar_geom(total, viewport, max_scroll(total, viewport), track).unwrap();
        assert!((f(g.thumb_top) + f(g.thumb_len) - 564.).abs() < 0.01);
        // And the drag mapping still round-trips against the taller track.
        let back = scroll_for_thumb_top(total, viewport, g.thumb_top, track);
        assert!((f(back) - f(max_scroll(total, viewport))).abs() < 0.01);
    }

    #[test]
    fn scroll_offset_is_clamped_into_the_track() {
        // Past either end, the thumb stays inside the track.
        let g = scrollbar_geom(px(2000.), px(500.), px(9999.), px(500.)).unwrap();
        assert!((f(g.thumb_top) - (500. - f(g.thumb_len))).abs() < 0.01);
        let g = scrollbar_geom(px(2000.), px(500.), px(-100.), px(500.)).unwrap();
        assert_eq!(f(g.thumb_top), 0.);
    }

    #[test]
    fn thumb_length_has_a_floor() {
        // 500/100000 * 500 = 2.5px would be unusable, so it clamps.
        let g = scrollbar_geom(px(100_000.), px(500.), px(0.), px(500.)).unwrap();
        assert_eq!(f(g.thumb_len), SCROLLBAR_MIN_THUMB);
        // Still reaches the bottom of the track at max scroll.
        let g = scrollbar_geom(px(100_000.), px(500.), px(99_500.), px(500.)).unwrap();
        assert!((f(g.thumb_top) + f(g.thumb_len) - 500.).abs() < 0.01);
        // A track smaller than the floor clamps to the track instead.
        let g = scrollbar_geom(px(1000.), px(10.), px(0.), px(10.)).unwrap();
        assert_eq!(f(g.thumb_len), 10.);
    }

    #[test]
    fn drag_mapping_is_the_exact_inverse() {
        let (total, viewport, track) = (px(2000.), px(500.), px(500.));
        for scroll in [0., 100., 375., 750., 1200., 1500.] {
            let g = scrollbar_geom(total, viewport, px(scroll), track).unwrap();
            let back = scroll_for_thumb_top(total, viewport, g.thumb_top, track);
            assert!(
                (f(back) - scroll).abs() < 0.01,
                "round trip failed for {scroll}: got {}",
                f(back)
            );
        }
    }

    #[test]
    fn track_click_jumps_to_that_position() {
        // A click on the track scrolls to where it points (the handler centres
        // the thumb on the click, i.e. `scroll_for_thumb_top(click - len/2)`),
        // rather than paging by a viewport.
        let (total, viewport, track) = (px(100_000.), px(500.), px(564.));
        let g = scrollbar_geom(total, viewport, px(0.), track).unwrap();
        let jump = |click: f32| {
            f(scroll_for_thumb_top(
                total,
                viewport,
                px(click - f(g.thumb_len) / 2.),
                track,
            ))
        };
        let m = f(max_scroll(total, viewport));
        // Clicking at the very top/bottom pins to the ends...
        assert_eq!(jump(0.), 0.);
        assert_eq!(jump(564.), m);
        // ...and the middle lands mid-document, NOT one viewport away (which is
        // what the old paging behaviour would have given).
        let mid = jump(282.);
        assert!((mid - m / 2.).abs() < 1.0, "middle click gave {mid}, want ~{}", m / 2.);
        assert!(mid > 500., "a mid-track click must not merely page by a screen");
    }

    #[test]
    fn drag_mapping_clamps_at_both_ends() {
        let (total, viewport, track) = (px(2000.), px(500.), px(500.));
        assert_eq!(f(scroll_for_thumb_top(total, viewport, px(-50.), track)), 0.);
        // Dragging past the bottom pins to max scroll, never beyond.
        let m = f(max_scroll(total, viewport));
        assert_eq!(f(scroll_for_thumb_top(total, viewport, px(9999.), track)), m);
        assert_eq!(m, 1500.);
        // Nothing to scroll -> no offset.
        assert_eq!(f(scroll_for_thumb_top(px(300.), px(500.), px(10.), track)), 0.);
        // Degenerate track (thumb fills it) can't express an offset.
        assert_eq!(f(scroll_for_thumb_top(px(1000.), px(10.), px(5.), px(10.))), 0.);
    }

    #[test]
    fn grab_strip_is_the_right_edge_only() {
        use gpui::{size, Bounds};
        // Element at (100, 50), 800x600 — so the strip is x >= 886.
        let b = Bounds { origin: gpui::point(px(100.), px(50.)), size: size(px(800.), px(600.)) };
        let at = |x: f32, y: f32| in_scrollbar_strip(&b, gpui::point(px(x), px(y)));
        assert!(at(895., 300.)); // inside the strip
        assert!(at(886., 300.)); // exactly on its left edge
        assert!(!at(885., 300.)); // just left of it: text territory
        assert!(!at(400., 300.)); // middle of the text
        // Outside the content box entirely, even at a strip-like x.
        assert!(!at(895., 10.));
        assert!(!at(895., 700.));
        assert!(!at(950., 300.));
    }

    #[test]
    fn no_autoscroll_while_the_pointer_is_inside() {
        let v = px(500.);
        for y in [0., 1., 250., 499., 500.] {
            assert_eq!(f(drag_scroll_step(px(y), v)), 0., "y={y} should not scroll");
        }
    }

    #[test]
    fn autoscroll_direction_follows_the_edge_crossed() {
        let v = px(500.);
        // Above the text: scroll up (towards the start of the document).
        assert!(f(drag_scroll_step(px(-1.), v)) < 0.);
        assert!(f(drag_scroll_step(px(-300.), v)) < 0.);
        // Below it: scroll down.
        assert!(f(drag_scroll_step(px(501.), v)) > 0.);
        assert!(f(drag_scroll_step(px(900.), v)) > 0.);
    }

    #[test]
    fn autoscroll_ramps_with_the_overshoot_and_caps() {
        let v = px(500.);
        // A hair past the edge creeps, so the selection end is placeable.
        assert!((f(drag_scroll_step(px(500.5), v)) - AUTOSCROLL_MIN).abs() < 0.2);
        // Monotonic in the overshoot...
        let mut prev = 0.;
        for over in [1., 20., 60., 100., 159.] {
            let s = f(drag_scroll_step(px(500. + over), v));
            assert!(s > prev, "not monotonic at {over}: {s} <= {prev}");
            prev = s;
        }
        // ...and capped, so a pointer flung far off screen doesn't teleport.
        assert_eq!(f(drag_scroll_step(px(500. + AUTOSCROLL_RAMP), v)), AUTOSCROLL_MAX);
        assert_eq!(f(drag_scroll_step(px(99_999.), v)), AUTOSCROLL_MAX);
        assert_eq!(f(drag_scroll_step(px(-99_999.), v)), -AUTOSCROLL_MAX);
    }

    #[test]
    fn autoscroll_depends_only_on_position() {
        // The property the held-still case relies on: the same pointer position
        // always yields the same step, so the loop keeps scrolling with no
        // further mouse movement.
        let v = px(500.);
        let a = drag_scroll_step(px(560.), v);
        let b = drag_scroll_step(px(560.), v);
        assert_eq!(a, b);
        assert!(f(a) > 0.);
    }

    #[test]
    fn max_scroll_is_never_negative() {
        assert_eq!(f(max_scroll(px(2000.), px(500.))), 1500.);
        assert_eq!(f(max_scroll(px(300.), px(500.))), 0.);
        assert_eq!(f(max_scroll(px(500.), px(500.))), 0.);
    }

    #[test]
    fn dragging_moves_content_one_to_one_with_the_thumb() {
        // The whole point of the inverse: N px of thumb travel must equal
        // N * (total - viewport)/travel px of content.
        let (total, viewport, track) = (px(4000.), px(1000.), px(1064.));
        let g = scrollbar_geom(total, viewport, px(0.), track).unwrap();
        let travel = f(g.track) - f(g.thumb_len);
        let a = scroll_for_thumb_top(total, viewport, px(0.), track);
        let b = scroll_for_thumb_top(total, viewport, px(travel / 2.), track);
        let ratio = (f(b) - f(a)) / (travel / 2.);
        let expected = f(max_scroll(total, viewport)) / travel;
        assert!((ratio - expected).abs() < 0.001, "ratio {ratio} != {expected}");
    }
}
