//! Time window and normalized coordinates for the clip timeline.

const MIN_AXIS_SECONDS: f64 = 0.01;
const MIN_ZOOM_SPAN_SECONDS: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Window {
    pub(super) start: f64,
    span: f64,
}

impl Window {
    fn clamped(start: f64, span: f64, duration: f64) -> Self {
        let span = span.clamp(duration.min(MIN_ZOOM_SPAN_SECONDS), duration);
        Self { start: start.clamp(0.0, duration - span), span }
    }

    pub(super) fn end(self) -> f64 {
        self.start + self.span
    }

    pub(super) fn time_at(self, fraction: f64) -> f64 {
        self.start + fraction.clamp(0.0, 1.0) * self.span
    }

    pub(super) fn fraction_at(self, time: f64) -> f64 {
        ((time - self.start) / self.span).clamp(0.0, 1.0)
    }
}

#[derive(Default)]
pub(super) struct TimelineViewport {
    // An unfixed window follows the duration as the clip loads.
    window: Option<Window>,
}

impl TimelineViewport {
    fn axis_duration(duration: f64) -> f64 {
        if duration.is_finite() {
            duration.max(MIN_AXIS_SECONDS)
        } else {
            MIN_AXIS_SECONDS
        }
    }

    pub(super) fn visible(&self, duration: f64) -> Window {
        let duration = Self::axis_duration(duration);
        let window = self.window.unwrap_or(Window { start: 0.0, span: duration });
        Window::clamped(window.start, window.span, duration)
    }

    /// Keep the given time at its horizontal fraction, unless a clip edge
    /// prevents it. A centered fraction anchors button zoom at the playhead.
    pub(super) fn zoom(&mut self, duration: f64, factor: f64, anchor: f64, fraction: f64) {
        if factor.is_nan() || factor < 0.0 || !anchor.is_finite() || !fraction.is_finite() {
            return;
        }
        let duration = Self::axis_duration(duration);
        let current = self.visible(duration);
        let span = (current.span * factor).clamp(duration.min(MIN_ZOOM_SPAN_SECONDS), duration);
        let start = anchor.clamp(0.0, duration) - fraction.clamp(0.0, 1.0) * span;
        self.update(current, Window::clamped(start, span, duration));
    }

    pub(super) fn pan(&mut self, duration: f64, fraction: f64) {
        if fraction != 0.0 && fraction.is_finite() {
            let duration = Self::axis_duration(duration);
            let current = self.visible(duration);
            let start = current.start + fraction * current.span;
            self.update(current, Window::clamped(start, current.span, duration));
        }
    }

    fn update(&mut self, previous: Window, next: Window) {
        if next != previous {
            self.window = Some(next);
        }
    }

    pub(super) fn fit(&mut self) {
        self.window = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "{actual} != {expected}");
    }

    #[test]
    fn screen_fractions_round_trip_and_clamp() {
        let mut view = TimelineViewport::default();
        view.zoom(120.0, 0.25, 60.0, 0.5);
        let window = view.visible(120.0);
        assert_eq!(window, Window { start: 45.0, span: 30.0 });
        for fraction in [0.0, 0.21, 0.5, 1.0] {
            assert_close(window.fraction_at(window.time_at(fraction)), fraction);
        }
        assert_eq!(window.time_at(-1.0), 45.0);
        assert_eq!(window.time_at(1.5), 75.0);
        assert_eq!(window.fraction_at(1.0), 0.0);
        assert_eq!(window.fraction_at(100.0), 1.0);
    }

    #[test]
    fn cursor_zoom_preserves_the_anchor_fraction_until_an_edge() {
        let mut view = TimelineViewport::default();
        let before = view.visible(100.0);
        let anchor = before.time_at(0.78);
        view.zoom(100.0, 0.5, anchor, 0.78);
        let after = view.visible(100.0);
        assert_close(after.span, 50.0);
        assert_close(after.time_at(0.78), anchor);
        view.zoom(100.0, 0.5, after.end(), 0.0);
        assert_eq!(view.visible(100.0), Window { start: 75.0, span: 25.0 });
    }

    #[test]
    fn buttons_center_the_playhead_and_clamp_at_both_ends() {
        let mut view = TimelineViewport::default();
        view.zoom(100.0, 0.5, 97.0, 0.5);
        assert_eq!(view.visible(100.0), Window { start: 50.0, span: 50.0 });
        view.zoom(100.0, 0.5, 1.0, 0.5);
        assert_eq!(view.visible(100.0), Window { start: 0.0, span: 25.0 });
        view.zoom(100.0, 0.5, 60.0, 0.5);
        assert_eq!(view.visible(100.0).time_at(0.5), 60.0);
        view.zoom(100.0, 100.0, 60.0, 0.5);
        assert_eq!(view.visible(100.0), Window { start: 0.0, span: 100.0 });
    }

    #[test]
    fn zoom_and_pan_respect_short_clips_and_unloaded_media() {
        let mut view = TimelineViewport::default();
        for duration in [0.02, 0.4, 0.999] {
            view.zoom(duration, 0.001, 0.0, 0.5);
            view.pan(duration, 100.0);
            assert_eq!(view.visible(duration), Window { start: 0.0, span: duration });
        }
        // No-effect gestures on the initial fit must not pin the duration.
        assert_eq!(view.visible(12.5), Window { start: 0.0, span: 12.5 });
        for duration in [0.0, -1.0, f64::NAN] {
            let safe = view.visible(duration);
            assert_eq!(safe, Window { start: 0.0, span: MIN_AXIS_SECONDS });
            assert_close(safe.fraction_at(safe.time_at(0.5)), 0.5);
        }
    }

    #[test]
    fn long_clips_zoom_to_one_second_pan_to_edges_and_refit() {
        let duration = 24.0 * 60.0 * 60.0;
        let mut view = TimelineViewport::default();
        view.zoom(duration, 0.000001, 10.0, 0.5);
        assert_eq!(view.visible(duration), Window { start: 9.5, span: 1.0 });
        view.pan(duration, -100.0);
        assert_eq!(view.visible(duration), Window { start: 0.0, span: 1.0 });
        view.pan(duration, duration);
        assert_eq!(view.visible(duration).end(), duration);

        assert_eq!(view.visible(3.0), Window { start: 2.0, span: 1.0 });
        view.fit();
        assert_eq!(view.visible(3.0), Window { start: 0.0, span: 3.0 });
        assert_eq!(view.visible(33.0), Window { start: 0.0, span: 33.0 });
    }
}
