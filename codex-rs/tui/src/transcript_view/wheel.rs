//! Accelerate fast wheel flicks so long scrolls take a few notches instead of one per row.
//!
//! Each notch adds one unit of momentum, and momentum halves every [`HALF_LIFE`]. Slow,
//! deliberate notches never build enough momentum to leave the base step, while a quick run in
//! one direction grows the step, up to half the viewport, so a flick lands far away.
//!
//! Notch timing is only known from arrival, which a laggy link such as SSH distorts: a stall
//! splits one flick into clumps, and can bunch slow notches together. Decaying momentum rather
//! than resetting it on a gap keeps a jittered flick fast, and bunched slow notches only reach
//! mild acceleration because the first [`PRECISE_NOTCHES`] never grow. Reversing direction always
//! starts over at the base step.
//!
//! An opposite notch stops scrolling instead of adding to it. Wheel input queued behind a slow
//! frame or a slow link is applied before the next frame, and a long upward scroll can wait on
//! older history pages, so motion can still be pending when the user reverses. The reversal
//! returns to the position on screen and drops any rows waiting for history; the next notch
//! scrolls normally.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use super::Anchor;
use super::Position;
use super::TranscriptView;
use crate::history_cell::HistoryCell;

/// Time for wheel momentum to halve once notches stop arriving.
const HALF_LIFE: Duration = Duration::from_millis(/*millis*/ 150);
/// Momentum that keeps the base step before it starts to grow.
const PRECISE_NOTCHES: f64 = 3.0;
/// Growth of the step, in base steps, for each unit of momentum beyond the precise notches.
const GROWTH_PER_NOTCH: f64 = 0.5;

/// The run of wheel notches in progress.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Flick {
    run: Option<Run>,
}

#[derive(Clone, Copy, Debug)]
struct Run {
    upward: bool,
    momentum: f64,
    last: Instant,
}

impl Flick {
    /// Record a notch at `now` and return its step as a multiple of the base step.
    ///
    /// `max_multiplier` bounds a single notch, normally to half the viewport.
    pub(super) fn notch(&mut self, upward: bool, now: Instant, max_multiplier: f64) -> f64 {
        let carried = match self.run {
            Some(run) if run.upward == upward => {
                let half_lives =
                    now.saturating_duration_since(run.last).as_secs_f64() / HALF_LIFE.as_secs_f64();
                run.momentum * 0.5_f64.powf(half_lives)
            }
            _ => 0.0,
        };
        let momentum = carried + 1.0;
        self.run = Some(Run {
            upward,
            momentum,
            last: now,
        });
        let growth = (momentum - PRECISE_NOTCHES).max(0.0) * GROWTH_PER_NOTCH;
        (1.0 + growth).min(max_multiplier.max(1.0))
    }
}

/// Wheel motion applied since the last frame, which the user has not seen yet.
#[derive(Clone, Copy, Debug)]
pub(super) struct UnseenScroll {
    upward: bool,
    origin: Position,
}

impl TranscriptView {
    /// Stop pending motion against an opposite notch, or remember where unseen motion began.
    ///
    /// Returns false when the notch was spent stopping earlier motion.
    pub(super) fn begin_wheel_notch(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        upward: bool,
    ) -> bool {
        match self.unseen_wheel {
            Some(unseen) if unseen.upward != upward => {
                self.unseen_wheel = None;
                self.restore_wheel_origin(cells, unseen.origin);
            }
            Some(_) => return true,
            // Rows waiting on older history always continue upward.
            None if !upward && self.pending_history_rows > 0 => {}
            None => {
                self.unseen_wheel = Some(UnseenScroll {
                    upward,
                    origin: self.position,
                });
                return true;
            }
        }
        self.pending_history_rows = 0;
        self.pending_mouse_scroll = 0.0;
        self.wheel_flick = Flick::default();
        false
    }

    fn restore_wheel_origin(&mut self, cells: &[Arc<dyn HistoryCell>], origin: Position) {
        match (origin, self.position) {
            (Position::Latest, _) => {
                self.release_live_reading();
                self.position = Position::Latest;
            }
            (Position::Reading(anchor), Position::Reading(_)) => {
                // Older pages may have arrived since, so locate the entry again.
                let index = self.resolve(cells, anchor);
                self.position = Position::Reading(Anchor {
                    key: self.entry_key(cells, index),
                    index,
                    ..anchor
                });
            }
            // Reaching the latest output already ended the reading position.
            (Position::Reading(_), Position::Latest) => {}
        }
    }
}

#[cfg(test)]
#[path = "wheel_tests.rs"]
mod tests;
