//! Width-specific layouts retained only for recently displayed conversation entries.
//!
//! Mutable cells refresh each frame, then share that frame's layout across measurement and paint.
//! Stable cells also invalidate when animation ticks, syntax themes, or terminal colors change.

use crate::history_cell::ActivityDisclosure;
use crate::key_hint::ShortcutHint;
use crate::keymap::KeymapContext;
use std::collections::HashMap;
use std::sync::Weak;

use super::*;

/// Enough entries for a tall screen of short entries plus the rows scrolled past, so a frame
/// never evicts layouts it is about to reuse.
const MAX_CACHED_ENTRIES: usize = 1024;
const MAX_CACHED_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// The selected activity presentation and any live content that follows it.
pub(crate) struct ActivityTranscriptLines {
    pub(crate) activity: Vec<HyperlinkLine>,
    pub(crate) auxiliary: Vec<HyperlinkLine>,
    pub(crate) disclosure: Option<ActivityDisclosure>,
}

/// Keep disclosure controls outside selectable source without rendering hidden details.
pub(super) fn activity_layout(
    mut lines: ActivityTranscriptLines,
    width: u16,
    expanded: bool,
    shortcut: Option<ShortcutHint>,
) -> TextLayout {
    let has_details = !lines.activity.is_empty() && (expanded || lines.disclosure.is_some());
    let source_offset = (!lines.auxiliary.is_empty())
        .then(|| TextLayout::new(lines.activity.clone(), width).text().len());
    lines.activity.extend(lines.auxiliary);
    let layout = TextLayout::new(lines.activity, width);
    if !has_details {
        return layout;
    }
    let label = match (expanded, lines.disclosure) {
        (true, _) => "− Show less".to_owned(),
        (false, Some(ActivityDisclosure::OutputLines(count))) => {
            let noun = if count == 1 { "line" } else { "lines" };
            let mut label = format!("+ {count} {noun}");
            if let Some(shortcut) = shortcut {
                let hint = format!(" ({} to expand)", shortcut.display_label());
                let indent = usize::from(width / 4).min(/*other*/ 4);
                if Line::from(format!("{label}{hint}")).width() + indent <= usize::from(width) {
                    label.push_str(&hint);
                }
            }
            label
        }
        (false, Some(ActivityDisclosure::Generic) | None) => "+ Show details".to_owned(),
    };
    match source_offset {
        Some(offset) => layout.with_disclosure_control_at(label, offset),
        None => layout.with_disclosure_control(label),
    }
}

#[derive(Default)]
pub(super) struct LayoutCache {
    /// One entry per source cell, keyed by its allocation; the weak reference keeps the key from
    /// being reused while the entry exists.
    entries: HashMap<usize, CachedLayout>,
    frame: u64,
    /// Increases on every lookup, ordering entries for least-recently-used eviction.
    clock: u64,
    text_bytes: usize,
    render_state: Option<RenderState>,
}

#[derive(PartialEq, Eq)]
struct RenderState {
    theme: u64,
    foreground: Option<(u8, u8, u8)>,
    background: Option<(u8, u8, u8)>,
    color_level: crate::terminal_palette::StdoutColorLevel,
}

struct CachedLayout {
    /// Holds the source allocation so its address, the entry's key, is not reused.
    _source: Weak<dyn HistoryCell>,
    width: u16,
    presentation: CellPresentation,
    layout: Arc<TextLayout>,
    rendered_frame: u64,
    animation_tick: Option<u64>,
    last_used: u64,
}

fn cache_key(cell: &Arc<dyn HistoryCell>) -> usize {
    Arc::as_ptr(cell).cast::<()>() as usize
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct CellPresentation {
    separated: bool,
    turn_tip_space: bool,
    expanded: bool,
    disclosure: bool,
    detailed: bool,
}

impl TranscriptView {
    pub(super) fn layout(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        index: usize,
    ) -> Option<Arc<TextLayout>> {
        let snapshot = self.snapshot_cells();
        let cells = snapshot.as_deref().unwrap_or(cells);
        if index > cells.len() {
            return None;
        }
        let key = self.entry_key(cells, index);
        if let Some(layout) = self.search.match_layout(key) {
            return Some(layout);
        }
        self.base_layout(cells, index)
    }

    /// Resolve manual disclosure and retained revisions, without Find's temporary expansion.
    /// Call with the snapshot's cells, if present.
    pub(super) fn base_layout(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        index: usize,
    ) -> Option<Arc<TextLayout>> {
        let key = self.entry_key(cells, index);
        if let Some(layout) = self
            .snapshot()
            .and_then(|snapshot| snapshot.pinned.get(&key))
        {
            return Some(Arc::clone(layout));
        }
        if index == cells.len() && self.snapshot().is_some() {
            return None;
        }
        self.current_layout(cells, index)
    }

    /// Resolve the current revision without a reader's retained snapshot.
    pub(super) fn current_layout(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        index: usize,
    ) -> Option<Arc<TextLayout>> {
        self.entry_layout(cells, index, self.detailed)
    }

    pub(super) fn entry_layout(
        &mut self,
        cells: &[Arc<dyn HistoryCell>],
        index: usize,
        full_content: bool,
    ) -> Option<Arc<TextLayout>> {
        let Some(cell) = cells.get(index) else {
            if index != cells.len() {
                return None;
            }
            let live = self.live.as_ref()?;
            if cells.is_empty() || self.live_continuation {
                return Some(Arc::clone(live));
            }
            let width = self.area.width.max(/*other*/ 1);
            return Some(Arc::clone(self.live_separated.get_or_insert_with(|| {
                Arc::new(live.rewrap(width).with_leading_separator())
            })));
        };
        let width = self.area.width.max(/*other*/ 1);
        // Session information belongs before all history, including pages not loaded yet.
        // Pinned displayed revisions above remain visible; hidden headers never enter the cache.
        if self.history.has_unloaded_history()
            && cell.as_any().is::<crate::history_cell::SessionInfoCell>()
        {
            return Some(Arc::new(TextLayout::new(Vec::new(), width)));
        }
        let mode = self.mode;
        let ids = cell.activity_ids();
        let disclosure = !self.detailed && mode == HistoryRenderMode::Rich && !ids.is_empty();
        let manually_expanded = self.disclosure.is_expanded(&ids);
        let expanded = disclosure && (full_content || manually_expanded);
        if manually_expanded {
            self.disclosure.expanded.extend(ids);
        }
        let separated = index > 0 && !cell.is_stream_continuation();
        let presentation = CellPresentation {
            separated,
            turn_tip_space: self.turn_tip_key == Some(EntryKey::cell(cell)),
            expanded,
            disclosure,
            detailed: full_content,
        };
        let shortcut = self
            .disclosure
            .keymap
            .primary_hint(KeymapContext::Global, "open_transcript");
        Some(self.cache.get(cell, width, presentation, || {
            if disclosure {
                activity_layout(
                    ActivityTranscriptLines {
                        activity: if expanded {
                            cell.expanded_hyperlink_lines(width)
                        } else {
                            cell.compact_hyperlink_lines(width)
                        },
                        auxiliary: Vec::new(),
                        disclosure: cell.activity_disclosure(width),
                    },
                    width,
                    expanded,
                    shortcut,
                )
            } else {
                TextLayout::new(
                    crate::history_cell::fullscreen_session_lines(
                        cell.as_ref(),
                        width,
                        full_content,
                        mode,
                    ),
                    width,
                )
            }
        }))
    }
}

impl LayoutCache {
    pub(super) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(/*rhs*/ 1);
        let state = RenderState {
            theme: crate::render::highlight::syntax_theme_revision(),
            foreground: crate::terminal_palette::default_fg(),
            background: crate::terminal_palette::default_bg(),
            color_level: crate::terminal_palette::stdout_color_level(),
        };
        if self.render_state.as_ref() != Some(&state) {
            self.clear();
            self.render_state = Some(state);
        }
    }

    pub(super) fn get(
        &mut self,
        cell: &Arc<dyn HistoryCell>,
        width: u16,
        presentation: CellPresentation,
        render: impl FnOnce() -> TextLayout,
    ) -> Arc<TextLayout> {
        let key = cache_key(cell);
        let animation_tick = cell.transcript_animation_tick();
        self.clock += 1;
        if let Some(entry) = self.entries.get_mut(&key)
            && entry.width == width
            && entry.presentation == presentation
            && (entry.rendered_frame == self.frame
                || (cell.has_stable_transcript_height() && entry.animation_tick == animation_tick))
        {
            entry.last_used = self.clock;
            return Arc::clone(&entry.layout);
        }
        let layout = render();
        let layout = if presentation.separated {
            layout.with_leading_separator()
        } else {
            layout
        };
        let layout = Arc::new(if presentation.turn_tip_space {
            layout.with_leading_spacer()
        } else {
            layout
        });
        self.text_bytes += layout.text().len();
        if let Some(replaced) = self.entries.insert(
            key,
            CachedLayout {
                _source: Arc::downgrade(cell),
                width,
                presentation,
                layout: Arc::clone(&layout),
                rendered_frame: self.frame,
                animation_tick,
                last_used: self.clock,
            },
        ) {
            self.text_bytes -= replaced.layout.text().len();
        }
        self.evict();
        layout
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.text_bytes = 0;
    }

    /// Drop least recently used entries, a quarter of the limit at a time so eviction stays
    /// rare. Retain the newest entry even when oversized rather than repeatedly laying it out.
    fn evict(&mut self) {
        if self.entries.len() <= MAX_CACHED_ENTRIES && self.text_bytes <= MAX_CACHED_TEXT_BYTES {
            return;
        }
        let mut by_age = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.last_used, *key))
            .collect::<Vec<_>>();
        by_age.sort_unstable();
        let keep_entries = MAX_CACHED_ENTRIES - MAX_CACHED_ENTRIES / 4;
        let keep_bytes = MAX_CACHED_TEXT_BYTES - MAX_CACHED_TEXT_BYTES / 4;
        for (_, key) in &by_age[..by_age.len() - 1] {
            if self.entries.len() <= keep_entries && self.text_bytes <= keep_bytes {
                break;
            }
            if let Some(entry) = self.entries.remove(key) {
                self.text_bytes -= entry.layout.text().len();
            }
        }
    }
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
