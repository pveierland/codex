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
const MAX_CACHED_LAYOUT_BYTES: usize = 8 * 1024 * 1024;

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
        (true, Some(ActivityDisclosure::OutputLines(_))) => "− Show less".to_owned(),
        (true, Some(ActivityDisclosure::Generic) | None) => "− Hide details".to_owned(),
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

fn cell_content_layout(
    cell: &dyn HistoryCell,
    width: u16,
    mode: HistoryRenderMode,
    presentation: CellPresentation,
    shortcut: Option<ShortcutHint>,
) -> TextLayout {
    let mut layout = if presentation.disclosure {
        activity_layout(
            ActivityTranscriptLines {
                activity: if presentation.expanded {
                    cell.expanded_hyperlink_lines(width)
                } else {
                    cell.compact_hyperlink_lines(width)
                },
                auxiliary: Vec::new(),
                disclosure: cell.activity_disclosure(width),
            },
            width,
            presentation.expanded,
            shortcut,
        )
    } else {
        TextLayout::new(
            crate::history_cell::fullscreen_session_lines(cell, width, presentation.detailed, mode),
            width,
        )
    };
    if let Some(protocol) = presentation.image_protocol
        && let Some(path) = cell.image_path()
    {
        use crate::transcript_image::PreviewKind;
        let kind = if cell
            .as_any()
            .is::<crate::history_cell::ImageGenerationHistoryCell>()
        {
            PreviewKind::Generated
        } else {
            PreviewKind::Inspected
        };
        layout.image =
            crate::transcript_image::TranscriptImage::load(path.as_path(), width, protocol, kind)
                .map(Arc::new);
        layout.image_left_aligned = kind == PreviewKind::Generated;
    }
    layout
}

fn is_rich_user_prompt(cell: &dyn HistoryCell, mode: HistoryRenderMode) -> bool {
    mode == HistoryRenderMode::Rich && cell.as_any().is::<crate::history_cell::UserHistoryCell>()
}

#[derive(Default)]
pub(super) struct LayoutCache {
    /// One entry per source cell, keyed by its allocation; the weak reference keeps the key from
    /// being reused while the entry exists.
    entries: HashMap<usize, CachedLayout>,
    frame: u64,
    /// Increases on every lookup, ordering entries for least-recently-used eviction.
    clock: u64,
    retained_bytes: usize,
    render_state: Option<RenderState>,
}

#[derive(PartialEq, Eq)]
struct RenderState {
    theme: u64,
    document_links: bool,
    foreground: Option<(u8, u8, u8)>,
    background: Option<(u8, u8, u8)>,
    color_level: crate::terminal_palette::StdoutColorLevel,
    cell_size: Option<(u32, u32)>,
}

struct CachedLayout {
    /// Holds the source allocation so its address, the entry's key, is not reused.
    source: Weak<dyn HistoryCell>,
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
    summarized: bool,
    separated: bool,
    shares_prompt_background: bool,
    turn_tip_space: bool,
    expanded: bool,
    disclosure: bool,
    detailed: bool,
    image_protocol: Option<crate::pets::ImageProtocol>,
}

impl TranscriptView {
    fn content_presentation(
        &mut self,
        cell: &dyn HistoryCell,
        full_content: bool,
        mode: HistoryRenderMode,
    ) -> CellPresentation {
        let ids = cell.activity_ids();
        let disclosure = !self.detailed && mode == HistoryRenderMode::Rich && !ids.is_empty();
        let manually_expanded = self.disclosure.is_expanded(&ids);
        let reveal_image = ids.is_empty() || manually_expanded || self.detailed;
        if manually_expanded {
            self.disclosure.expanded.extend(ids);
        }
        CellPresentation {
            summarized: false,
            separated: false,
            shares_prompt_background: false,
            turn_tip_space: false,
            expanded: disclosure && (full_content || manually_expanded),
            disclosure,
            detailed: full_content,
            image_protocol: self
                .image_protocol
                .filter(|_| mode == HistoryRenderMode::Rich && reveal_image),
        }
    }

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
        let mut presentation = self.content_presentation(cell.as_ref(), full_content, mode);
        let mut turn_header = None;
        if !full_content && let Some(row) = self.turn_row(cells, index) {
            let ids = row.ids();
            presentation.expanded = self.disclosure.is_expanded(ids);
            presentation.summarized = true;
            match &row {
                turn_details::TurnRow::Prompt { label, .. } => {
                    let arrow = if presentation.expanded { "▾" } else { "▸" };
                    turn_header = Some(format!("{arrow} {label}"));
                }
                turn_details::TurnRow::Tools(_) => {
                    if presentation.expanded {
                        self.disclosure.expanded.extend(ids.iter().cloned());
                    }
                    return Some(self.cache.get(cell, width, presentation, || {
                        row.layout(width, presentation.expanded, Vec::new())
                    }));
                }
                _ => return Some(Arc::new(TextLayout::new(Vec::new(), width))),
            }
        }
        let shortcut = self
            .disclosure
            .keymap
            .primary_hint(KeymapContext::Global, "open_transcript");
        let shares_prompt_background = is_rich_user_prompt(cell.as_ref(), mode)
            && (0..index)
                .rev()
                .find(|&previous_index| {
                    let previous = &cells[previous_index];
                    let key = EntryKey::cell(previous);
                    if let Some(layout) = self.search.match_layout(key).or_else(|| {
                        self.snapshot()
                            .and_then(|snapshot| snapshot.pinned.get(&key))
                            .cloned()
                    }) {
                        return layout.row_count() > 0;
                    }
                    if !full_content && let Some(row) = self.turn_row(cells, previous_index) {
                        return !row.ids().is_empty();
                    }
                    let previous_presentation =
                        self.content_presentation(previous.as_ref(), self.detailed, mode);
                    self.cache
                        .has_visible_content(previous, width, previous_presentation, || {
                            cell_content_layout(
                                previous.as_ref(),
                                width,
                                mode,
                                previous_presentation,
                                shortcut,
                            )
                        })
                })
                .is_some_and(|previous| is_rich_user_prompt(cells[previous].as_ref(), mode));
        presentation.separated =
            index > 0 && !cell.is_stream_continuation() && !shares_prompt_background;
        presentation.shares_prompt_background = shares_prompt_background;
        presentation.turn_tip_space = self.turn_tip_key == Some(EntryKey::cell(cell));
        Some(self.cache.get(cell, width, presentation, || {
            let mut layout =
                cell_content_layout(cell.as_ref(), width, mode, presentation, shortcut);
            if shares_prompt_background {
                layout = layout.without_leading_blank_row();
            }
            if let Some(label) = turn_header {
                layout = layout.with_turn_disclosure(label);
            }
            layout
        }))
    }
}

impl LayoutCache {
    pub(super) fn has_mutable_source(&self) -> bool {
        self.entries.values().any(|entry| {
            entry
                .source
                .upgrade()
                .is_some_and(|cell| !cell.has_stable_transcript_height())
        })
    }

    pub(super) fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(/*rhs*/ 1);
        let state = RenderState {
            theme: crate::render::highlight::syntax_theme_revision(),
            document_links: crate::markdown_render::preferences::document_links_enabled(),
            foreground: crate::terminal_palette::default_fg(),
            background: crate::terminal_palette::default_bg(),
            color_level: crate::terminal_palette::stdout_color_level(),
            cell_size: crate::transcript_image::terminal_cell_size(),
        };
        if self.render_state.as_ref() != Some(&state) {
            self.clear();
            self.render_state = Some(state);
        }
    }

    /// The entry cached for `cell` at `width` that is still current, marked as just used.
    fn current_entry(
        &mut self,
        cell: &Arc<dyn HistoryCell>,
        width: u16,
        matches: impl FnOnce(&CellPresentation) -> bool,
    ) -> Option<&CachedLayout> {
        let animation_tick = cell.transcript_animation_tick();
        self.clock += 1;
        let entry = self.entries.get_mut(&cache_key(cell))?;
        if entry.width != width
            || !matches(&entry.presentation)
            || !(entry.rendered_frame == self.frame
                || (cell.has_stable_transcript_height() && entry.animation_tick == animation_tick))
        {
            return None;
        }
        entry.last_used = self.clock;
        Some(entry)
    }

    pub(super) fn get(
        &mut self,
        cell: &Arc<dyn HistoryCell>,
        width: u16,
        presentation: CellPresentation,
        render: impl FnOnce() -> TextLayout,
    ) -> Arc<TextLayout> {
        if let Some(entry) = self.current_entry(cell, width, |cached| *cached == presentation) {
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
        self.retained_bytes += retained_bytes(&layout);
        if let Some(replaced) = self.entries.insert(
            cache_key(cell),
            CachedLayout {
                source: Arc::downgrade(cell),
                width,
                presentation,
                layout: Arc::clone(&layout),
                rendered_frame: self.frame,
                animation_tick: cell.transcript_animation_tick(),
                last_used: self.clock,
            },
        ) {
            self.retained_bytes -= retained_bytes(&replaced.layout);
        }
        self.evict();
        layout
    }

    fn has_visible_content(
        &mut self,
        cell: &Arc<dyn HistoryCell>,
        width: u16,
        presentation: CellPresentation,
        render: impl FnOnce() -> TextLayout,
    ) -> bool {
        if let Some(entry) = self.current_entry(cell, width, |cached| {
            cached.expanded == presentation.expanded
                && cached.disclosure == presentation.disclosure
                && cached.detailed == presentation.detailed
                && cached.image_protocol == presentation.image_protocol
        }) {
            return entry.layout.row_count() > 0;
        }
        self.get(cell, width, presentation, render).row_count() > 0
    }

    pub(super) fn clear_summaries(&mut self) {
        let mut released = 0;
        self.entries.retain(|_, entry| {
            let keep = !entry.presentation.summarized;
            if !keep {
                released += retained_bytes(&entry.layout);
            }
            keep
        });
        self.retained_bytes -= released;
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.retained_bytes = 0;
    }

    /// Drop least recently used entries, a quarter of the limit at a time so eviction stays
    /// rare. Retain the newest entry even when oversized rather than repeatedly laying it out.
    fn evict(&mut self) {
        if self.entries.len() <= MAX_CACHED_ENTRIES
            && self.retained_bytes <= MAX_CACHED_LAYOUT_BYTES
        {
            return;
        }
        let mut by_age = self
            .entries
            .iter()
            .map(|(key, entry)| (entry.last_used, *key))
            .collect::<Vec<_>>();
        by_age.sort_unstable();
        let keep_entries = MAX_CACHED_ENTRIES - MAX_CACHED_ENTRIES / 4;
        let keep_bytes = MAX_CACHED_LAYOUT_BYTES - MAX_CACHED_LAYOUT_BYTES / 4;
        for (_, key) in &by_age[..by_age.len() - 1] {
            if self.entries.len() <= keep_entries && self.retained_bytes <= keep_bytes {
                break;
            }
            if let Some(entry) = self.entries.remove(key) {
                self.retained_bytes -= retained_bytes(&entry.layout);
            }
        }
    }
}

/// Memory a cached layout keeps alive: its text and any decoded image.
fn retained_bytes(layout: &TextLayout) -> usize {
    layout.text().len() + layout.image.as_ref().map_or(0, |image| image.pixels.len())
}

#[cfg(test)]
#[path = "layout_tests.rs"]
mod tests;
