//! Reuse terminal rows when a frame shifts content vertically.
//!
//! Scrolling a transcript changes nearly every row, so a cell diff would repaint the whole
//! viewport and terminal throughput would bound scrolling speed. When enough changed rows equal
//! previous rows at one constant offset, scroll that band inside the terminal and shift the
//! previous buffer the same way. The ordinary diff then paints only exposed or still-different
//! rows. Row hashes only choose the band; the diff against the shifted buffer keeps output exact.

use std::hash::Hash;
use std::hash::Hasher;
use std::io;
use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::buffer::CellDiffOption;

/// Scrolling costs a few escape sequences, so require a clear saving before using it.
const MIN_SAVED_ROWS: usize = 3;

/// One terminal scroll of rows `top..bottom` (viewport-relative, `bottom` exclusive).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Hardscroll {
    pub(super) top: u16,
    pub(super) bottom: u16,
    /// Positive moves content up (new rows appear at the bottom); negative moves it down.
    pub(super) shift: i32,
}

/// Fast, non-cryptographic row fingerprint. Collisions only cost a suboptimal plan.
struct RowHasher(u64);

impl Hasher for RowHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.write_u64(u64::from_le_bytes(word));
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.0 = (self.0.rotate_left(5) ^ value).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }

    fn write_u8(&mut self, value: u8) {
        self.write_u64(u64::from(value));
    }

    fn write_u16(&mut self, value: u16) {
        self.write_u64(u64::from(value));
    }

    fn write_u32(&mut self, value: u32) {
        self.write_u64(u64::from(value));
    }

    fn write_usize(&mut self, value: usize) {
        self.write_u64(value as u64);
    }
}

struct Row {
    hash: u64,
    blank: bool,
    /// Skip cells belong to out-of-band renderers (for example images); never move them.
    pinned: bool,
}

fn rows(buffer: &Buffer) -> Vec<Row> {
    let width = usize::from(buffer.area.width);
    if width == 0 {
        return Vec::new();
    }
    buffer
        .content
        .chunks(width)
        .map(|cells| {
            let mut hasher = RowHasher(0);
            let mut blank = true;
            let mut pinned = false;
            for cell in cells {
                cell.hash(&mut hasher);
                blank &= cell.symbol() == " "
                    && cell.bg == ratatui::style::Color::Reset
                    && cell.modifier.is_empty();
                #[allow(deprecated)]
                let skip = cell.skip;
                pinned |= skip || cell.diff_option == CellDiffOption::Skip;
            }
            Row {
                hash: hasher.finish(),
                blank,
                pinned,
            }
        })
        .collect()
}

/// Choose the terminal scroll that saves the most row repaints, if any saves enough.
pub(super) fn plan(previous: &Buffer, current: &Buffer) -> Option<Hardscroll> {
    if previous.area != current.area || current.area.height < 4 {
        return None;
    }
    let before = rows(previous);
    let after = rows(current);
    let height = after.len();
    let stale = |y: usize| before[y].hash != after[y].hash;
    // Only nonblank rows are expensive to repaint.
    let changed = (0..height).filter(|&y| stale(y) && !after[y].blank).count();
    if changed < MIN_SAVED_ROWS {
        return None;
    }

    let mut best: Option<(usize, Hardscroll)> = None;
    for distance in 1..height - 1 {
        for direction in [1isize, -1] {
            let shift = direction * distance as isize;
            // Kadane over destination rows: gain for rows the scroll delivers for free,
            // loss for rows that were already correct and would be disturbed.
            let mut run_start = 0usize;
            let mut run_score = 0isize;
            let mut found: Option<(isize, usize, usize)> = None;
            for (y, after_row) in after.iter().enumerate().take(height) {
                let source = y as isize + shift;
                if source < 0 || source >= height as isize {
                    run_score = 0;
                    run_start = y + 1;
                    continue;
                }
                let source = source as usize;
                if before[source].pinned || after_row.pinned {
                    run_score = 0;
                    run_start = y + 1;
                    continue;
                }
                let delivered = before[source].hash == after_row.hash;
                let value = match (delivered, stale(y), after_row.blank) {
                    (true, true, false) => 1,
                    (false, false, false) => -1,
                    _ => 0,
                };
                if run_score <= 0 {
                    run_start = y;
                    run_score = 0;
                }
                run_score += value;
                if run_score > 0 && found.is_none_or(|(score, _, _)| run_score > score) {
                    found = Some((run_score, run_start, y));
                }
            }
            let Some((_, first, last)) = found else {
                continue;
            };
            // The terminal region spans destinations and their sources; the rows the content
            // leaves behind are cleared by the terminal.
            let (top, bottom) = if shift > 0 {
                (first, last + distance + 1)
            } else {
                (first - distance, last + 1)
            };
            if (top..bottom).any(|y| before[y].pinned || after[y].pinned) {
                continue;
            }
            let mut saved = 0isize;
            for (y, after_row) in after.iter().enumerate().take(bottom).skip(top) {
                let source = y as isize + shift;
                let delivered = (top as isize..bottom as isize).contains(&source)
                    && before[source as usize].hash == after_row.hash;
                let correct_before = !stale(y);
                let correct_after = if delivered {
                    true
                } else if (top as isize..bottom as isize).contains(&source) {
                    false
                } else {
                    // Exposed rows become blank.
                    after_row.blank
                };
                if after_row.blank {
                    continue;
                }
                saved += isize::from(correct_after) - isize::from(correct_before);
            }
            let Ok(saved) = usize::try_from(saved) else {
                continue;
            };
            if saved >= MIN_SAVED_ROWS && best.is_none_or(|(score, _)| saved > score) {
                best = Some((
                    saved,
                    Hardscroll {
                        top: top as u16,
                        bottom: bottom as u16,
                        shift: shift as i32,
                    },
                ));
            }
        }
    }
    best.map(|(_, plan)| plan)
}

/// Scroll the terminal rows. `origin_y` is the viewport's screen row.
///
/// A line feed at the bottom margin and reverse index at the top margin scroll the region on
/// every VT100-compatible terminal (history insertion relies on the same sequences), unlike
/// SU/SD. After an SGR reset, exposed rows take the default background.
pub(super) fn write(writer: &mut impl Write, plan: Hardscroll, origin_y: u16) -> io::Result<()> {
    let top = origin_y + plan.top;
    let bottom = origin_y + plan.bottom - 1;
    queue!(
        writer,
        Print("\x1b[0m"),
        Print(format!("\x1b[{};{}r", top + 1, bottom + 1))
    )?;
    let (row, step) = if plan.shift > 0 {
        (bottom, "\n")
    } else {
        (top, "\x1bM")
    };
    queue!(writer, MoveTo(/*x*/ 0, row))?;
    queue!(
        writer,
        Print(step.repeat(plan.shift.unsigned_abs() as usize))
    )?;
    queue!(writer, Print("\x1b[r"))?;
    Ok(())
}

/// Mirror the terminal scroll in the buffer that models the screen.
pub(super) fn apply(buffer: &mut Buffer, plan: Hardscroll) {
    let width = usize::from(buffer.area.width);
    let top = usize::from(plan.top) * width;
    let bottom = usize::from(plan.bottom) * width;
    let region = &mut buffer.content[top..bottom];
    let moved = plan.shift.unsigned_abs() as usize * width;
    if plan.shift > 0 {
        region.rotate_left(moved);
        let exposed = region.len() - moved;
        region[exposed..].fill(Cell::EMPTY);
    } else {
        region.rotate_right(moved);
        region[..moved].fill(Cell::EMPTY);
    }
}

#[cfg(test)]
#[path = "hardscroll_tests.rs"]
mod tests;
