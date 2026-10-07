//! When each transcript entry happened, for the timestamp shown while reading history.
//!
//! Cells carry no time of their own, so the app records one as each enters the transcript: the
//! present for live output, or the turn's recorded time while replaying or paging in history.
//! Entries are keyed by cell identity, so the many places that splice or regroup cells need not
//! keep a parallel list in step. Cells without a time, such as notices, take the time of the
//! nearest earlier entry.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;

use chrono::DateTime;
use chrono::Local;
use chrono::TimeZone;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Widget;

use crate::history_cell::HistoryCell;

/// Earlier entries searched for a time before giving up, bounding the per-frame cost.
const MAX_LOOKBACK: usize = 256;
/// Entries recorded before cells dropped since are first swept.
const MIN_PRUNE_LEN: usize = 1024;

#[derive(Debug)]
pub(crate) struct HistoryTimes {
    times: HashMap<usize, Recorded>,
    /// The replayed turn's time while history is being replayed.
    replay_clock: Option<DateTime<Local>>,
    prune_at: usize,
    /// Test apps record nothing, so transcript snapshots do not depend on the clock or zone.
    enabled: bool,
}

#[derive(Debug)]
struct Recorded {
    /// Keeps the allocation, and so the key, from being reused by a later cell.
    cell: Weak<dyn HistoryCell>,
    at: DateTime<Local>,
}

impl Default for HistoryTimes {
    fn default() -> Self {
        Self {
            times: HashMap::new(),
            replay_clock: None,
            prune_at: MIN_PRUNE_LEN,
            enabled: true,
        }
    }
}

fn key(cell: &Arc<dyn HistoryCell>) -> usize {
    Arc::as_ptr(cell).cast::<()>() as usize
}

/// Convert recorded Unix seconds to local time.
pub(crate) fn local_time(unix_seconds: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(unix_seconds, /*nsecs*/ 0).single()
}

/// Convert recorded Unix milliseconds to local time.
pub(crate) fn local_time_ms(unix_millis: i64) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(unix_millis).single()
}

impl HistoryTimes {
    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Date cells inserted from now on with `clock` instead of the present, until cleared.
    pub(crate) fn set_replay_clock(&mut self, clock: Option<DateTime<Local>>) {
        self.replay_clock = clock;
    }

    /// Record a cell entering the transcript now, or at the replayed turn's time.
    pub(crate) fn record_inserted(&mut self, cell: &Arc<dyn HistoryCell>) {
        let at = self.replay_clock.unwrap_or_else(Local::now);
        self.record(cell, at);
    }

    pub(crate) fn record(&mut self, cell: &Arc<dyn HistoryCell>, at: DateTime<Local>) {
        if !self.enabled {
            return;
        }
        if self.times.len() >= self.prune_at {
            self.times
                .retain(|_, recorded| recorded.cell.strong_count() > 0);
            self.prune_at = (self.times.len() * 2).max(MIN_PRUNE_LEN);
        }
        self.times.insert(
            key(cell),
            Recorded {
                cell: Arc::downgrade(cell),
                at,
            },
        );
    }

    /// Give a cell that replaces `from`, such as a consolidated message, the same time.
    pub(crate) fn inherit(&mut self, from: &Arc<dyn HistoryCell>, to: &Arc<dyn HistoryCell>) {
        if let Some(at) = self.get(from) {
            self.record(to, at);
        }
    }

    fn get(&self, cell: &Arc<dyn HistoryCell>) -> Option<DateTime<Local>> {
        self.times.get(&key(cell)).map(|recorded| recorded.at)
    }

    /// The time of the entry at `index`, or of the nearest earlier entry with one.
    pub(crate) fn time_at(
        &self,
        cells: &[Arc<dyn HistoryCell>],
        index: usize,
    ) -> Option<DateTime<Local>> {
        let end = index.checked_add(1)?.min(cells.len());
        cells[..end]
            .iter()
            .rev()
            .take(MAX_LOOKBACK)
            .find_map(|cell| self.get(cell))
    }
}

/// Show `time` in the top-right corner of `area`, over the transcript being read.
pub(crate) fn render_reading_time(time: DateTime<Local>, area: Rect, buf: &mut Buffer) {
    let label = format!(" {} ", time.format("%Y-%m-%dT%H:%M:%S%:z"));
    let Ok(width) = u16::try_from(label.len()) else {
        return;
    };
    if area.height == 0 || area.width < width {
        return;
    }
    let label_area = Rect::new(area.right() - width, area.y, width, /*height*/ 1);
    Clear.render(label_area, buf);
    Span::from(label).dim().render(label_area, buf);
}

#[cfg(test)]
#[path = "history_times_tests.rs"]
mod tests;
