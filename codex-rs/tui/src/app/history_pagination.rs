//! Load bounded history pages into the transcript before updating its owned or inline viewport.

use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

use super::*;
use crate::app_server_session::HISTORY_ITEM_PAGE_LIMIT;
use crate::app_server_session::thread_items_page_params;
use crate::history_cell::SessionHeaderHistoryCell;
use crate::history_cell::SessionInfoCell;
use crate::history_cell::UserHistoryCell;
use crate::pager_overlay::TranscriptHistoryState;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells;
use crate::thread_transcript::thread_items_to_transcript_cells_with_sources;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ThreadItemsListResponse;

#[path = "history_completion.rs"]
mod completion;

/// The thread, working directory, and reasoning visibility an older page's items render with.
#[derive(Clone, Copy)]
struct PageProjection<'a> {
    thread_id: ThreadId,
    cwd: &'a AbsolutePathBuf,
    visibility: RawReasoningVisibility,
}

impl App {
    /// Start one bounded page request shared by scrollback refill and the transcript overlay.
    pub(crate) fn request_older_history_page(
        &self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) -> bool {
        let Some(cursor) = app_server.begin_older_history_page(thread_id) else {
            return false;
        };
        tracing::debug!(
            %thread_id,
            %cursor,
            overlay = self.overlay.is_some(),
            "loading older transcript history page"
        );
        let request_id = app_server.next_request_id();
        let request_handle = app_server.request_handle();
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = request_handle
                .request_typed::<ThreadItemsListResponse>(ClientRequest::ThreadItemsList {
                    request_id,
                    params: thread_items_page_params(
                        thread_id,
                        /*turn_id*/ None,
                        Some(cursor.clone()),
                        HISTORY_ITEM_PAGE_LIMIT,
                    ),
                })
                .await
                .map_err(|err| err.to_string());
            app_event_tx.send(AppEvent::OlderThreadHistoryLoaded {
                thread_id,
                cursor,
                result,
            });
        });
        true
    }

    pub(super) async fn handle_older_history_page(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        cursor: &str,
        result: Result<ThreadItemsListResponse, String>,
    ) -> Result<()> {
        if !app_server.is_older_history_page_pending(thread_id, cursor) {
            return Ok(());
        }
        if self.chat_widget.thread_id() != Some(thread_id)
            || (tui.is_owned_screen() && !self.scrollback_has_older_history)
        {
            app_server.cancel_older_history_page(thread_id, cursor);
            return Ok(());
        }
        let page = result.map_err(|err| color_eyre::eyre::eyre!(err))?;
        let Some(store) = self
            .thread_event_channels
            .get(&thread_id)
            .map(|channel| Arc::clone(&channel.store))
        else {
            app_server.cancel_older_history_page(thread_id, cursor);
            return Ok(());
        };
        // Fetch without holding the store, then apply the page to the stored turns in place.
        // Copying every loaded turn per page made each page slower than the last.
        let known_turn_ids = {
            let store = store.lock().await;
            store.turns.iter().map(|turn| turn.id.clone()).collect()
        };
        let page = app_server
            .fetch_older_history_page(thread_id, cursor, page, known_turn_ids)
            .await?;
        let mut store = store.lock().await;
        let cwd = store
            .session
            .as_ref()
            .map_or_else(|| self.config.cwd.clone(), |session| session.cwd.clone());
        let items = page.apply(&mut store.turns);
        let turns = &store.turns;
        let page_turns = leading_page_turns(turns, &items);
        let hidden_item_ids = hidden_review_item_ids(turns, page_turns);
        let projection = PageProjection {
            thread_id,
            cwd: &cwd,
            visibility: if self.config.show_raw_agent_reasoning {
                RawReasoningVisibility::Visible
            } else {
                RawReasoningVisibility::Hidden
            },
        };
        let width = tui.terminal.last_known_screen_size.width;
        // Only prompts that were already rendered can need removal.
        let page_item_ids = items.iter().map(ThreadItem::id).collect::<HashSet<_>>();
        let revealed_item_ids = hidden_item_ids
            .iter()
            .copied()
            .filter(|id| !page_item_ids.contains(id))
            .collect::<HashSet<_>>();
        self.remove_hidden_review_cells(tui, turns, &revealed_item_ids, projection);
        let cells = self.project_older_history_cells(
            items,
            &turns[..page_turns],
            &hidden_item_ids,
            projection,
            |item_id| app_server.history_item_time(thread_id, item_id),
        );
        let inserted = self.prepend_older_transcript_cells(cells);
        self.transcript_view
            .history_loaded(&self.transcript_cells, inserted.clone());
        if !inserted.is_empty() {
            self.join_older_activity_group(inserted.end, turns);
        }
        drop(store);
        self.scrollback_has_older_history = app_server.has_older_history(thread_id);
        if self.backtrack.overlay_preview_active
            && self.backtrack.nth_user_message == usize::MAX
            && !self.scrollback_has_older_history
        {
            self.cancel_transcript_browsing(tui);
            self.chat_widget.add_info_message(
                "No previous message to edit.".to_string(),
                /*hint*/ None,
            );
        }

        if tui.is_owned_screen() {
            self.finish_owned_history_page(tui, app_server, thread_id);
            return Ok(());
        }
        self.finish_inline_history_page(tui, app_server, thread_id, width);
        Ok(())
    }

    /// Remove prompts revealed as internal review input by an older page's review marker.
    fn remove_hidden_review_cells(
        &mut self,
        tui: &mut tui::Tui,
        turns: &[Turn],
        hidden_item_ids: &HashSet<&str>,
        projection: PageProjection<'_>,
    ) {
        if hidden_item_ids.is_empty() {
            return;
        }
        let user_items = turns
            .iter()
            .flat_map(|turn| turn.items.iter())
            .filter(|item| matches!(item, ThreadItem::UserMessage { .. }))
            .map(|item| (item.id().to_string(), item.clone()))
            .collect::<Vec<_>>();
        let projected_user_cells = thread_items_to_transcript_cells(
            Some(projection.thread_id),
            projection.cwd,
            user_items.iter().map(|(_, item)| item.clone()),
            projection.visibility,
            Some(&self.config),
        );
        let persisted_user_cells = user_items
            .into_iter()
            .zip(projected_user_cells)
            .map(|((item_id, _), cell)| (item_id, cell))
            .collect();
        let indices = hidden_transcript_indices(
            &self.transcript_cells,
            persisted_user_cells,
            hidden_item_ids,
        );
        if indices.is_empty() {
            return;
        }
        if self.backtrack.overlay_preview_active {
            let selected_index = crate::app_backtrack::nth_user_position(
                &self.transcript_cells,
                self.backtrack.nth_user_message,
            );
            if selected_index.is_some_and(|selected| indices.contains(&selected)) {
                self.cancel_transcript_browsing(tui);
            } else {
                let removed_visible_users = indices
                    .iter()
                    .filter(|&&index| {
                        selected_index.is_some_and(|selected| index < selected)
                            && crate::app_backtrack::user_count(std::slice::from_ref(
                                &self.transcript_cells[index],
                            )) != 0
                    })
                    .count();
                self.backtrack.nth_user_message = self
                    .backtrack
                    .nth_user_message
                    .saturating_sub(removed_visible_users);
            }
        }
        for index in indices {
            self.transcript_cells.remove(index);
        }
        self.native_history.retain(&self.transcript_cells);
        self.transcript_view.restart_search();
        self.transcript_view
            .history_loaded(&self.transcript_cells, 0..0);
        if let Some(Overlay::Transcript(overlay)) = self.overlay.as_mut() {
            overlay.replace_cells(self.transcript_cells.clone());
        }
    }

    /// Project successful turn completion metadata after the corresponding page's final item.
    fn project_older_history_cells(
        &mut self,
        items: Vec<ThreadItem>,
        turns: &[Turn],
        hidden_item_ids: &HashSet<&str>,
        projection: PageProjection<'_>,
        item_time_ms: impl Fn(&str) -> Option<i64>,
    ) -> Vec<Arc<dyn HistoryCell>> {
        let turn_starts: HashMap<&str, i64> = turns
            .iter()
            .filter_map(|turn| turn.started_at.map(|started_at| (turn, started_at)))
            .flat_map(|(turn, started_at)| {
                turn.items.iter().map(move |item| (item.id(), started_at))
            })
            .collect();
        let mut cells = Vec::new();
        for (items, completed_turn) in completion::group_completed_turn_items(items, turns) {
            let started_at = items
                .first()
                .and_then(|item| turn_starts.get(item.id()))
                .and_then(|started_at| history_times::local_time(*started_at));
            // Internal prompts stay invisible, but still separate adjacent tool groups.
            for visible in items.split(|item| hidden_item_ids.contains(item.id())) {
                let (visible_cells, sources) = thread_items_to_transcript_cells_with_sources(
                    Some(projection.thread_id),
                    projection.cwd,
                    visible.iter().cloned(),
                    projection.visibility,
                    Some(&self.config),
                );
                // Date each entry by the item that started it, or by its turn's start.
                for (cell, source) in visible_cells.iter().zip(sources) {
                    if let Some(at) = item_time_ms(visible[source].id())
                        .and_then(history_times::local_time_ms)
                        .or(started_at)
                    {
                        self.history_times.record(cell, at);
                    }
                }
                cells.extend(visible_cells);
            }
            if let Some(turn) = completed_turn
                && let Some(completion) = self
                    .chat_widget
                    .completion_cell(turn, Some(ReplayKind::ResumeInitialMessages))
            {
                let completion: Arc<dyn HistoryCell> = Arc::new(completion);
                if let Some(completed_at) = turn
                    .completed_at
                    .or(turn.started_at)
                    .and_then(history_times::local_time)
                {
                    self.history_times.record(&completion, completed_at);
                }
                cells.push(completion);
            }
        }
        cells
    }

    /// Insert each page once, keeping session headers and any backtrack selection in place.
    fn prepend_older_transcript_cells(&mut self, cells: Vec<Arc<dyn HistoryCell>>) -> Range<usize> {
        if self.backtrack.overlay_preview_active {
            let added_prompts = crate::app_backtrack::user_count(&cells);
            self.backtrack.nth_user_message = if self.backtrack.nth_user_message == usize::MAX {
                added_prompts.checked_sub(1).unwrap_or(usize::MAX)
            } else {
                self.backtrack
                    .nth_user_message
                    .saturating_add(added_prompts)
            };
        }
        let index = if let Some(Overlay::Transcript(overlay)) = self.overlay.as_mut() {
            overlay.prepend(cells.clone())
        } else {
            self.transcript_cells
                .iter()
                .rposition(|cell| {
                    cell.as_any().is::<SessionInfoCell>()
                        || cell.as_any().is::<SessionHeaderHistoryCell>()
                })
                .map_or(/*default*/ 0, |index| index.saturating_add(/*rhs*/ 1))
        };
        let inserted = index..index + cells.len();
        self.transcript_cells.splice(index..index, cells);
        inserted
    }

    /// Continue explicit history traversal without rewriting native scrollback.
    fn finish_owned_history_page(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
    ) {
        let previous = self.transcript_view.history;
        self.transcript_view.history = if self.scrollback_has_older_history {
            TranscriptHistoryState::Partial
        } else {
            TranscriptHistoryState::Complete
        };
        let continue_to_start = previous == TranscriptHistoryState::LoadingBeginning;
        if continue_to_start && !self.scrollback_has_older_history {
            self.transcript_view
                .jump_to_entry(&self.transcript_cells, /*index*/ 0);
        }
        if self.scrollback_has_older_history
            && (continue_to_start
                || self.browsing_needs_history()
                || self.transcript_view.needs_history(&self.transcript_cells))
            && self.request_older_history_page(app_server, thread_id)
        {
            self.transcript_view.history = if continue_to_start {
                TranscriptHistoryState::LoadingBeginning
            } else {
                TranscriptHistoryState::LoadingOlder
            };
        }
        if self.backtrack.overlay_preview_active {
            self.apply_backtrack_selection_internal(self.backtrack.nth_user_message);
        }
        tui.frame_requester().schedule_frame();
    }

    /// Preserve the legacy overlay and inline scrollback refill behavior.
    fn finish_inline_history_page(
        &mut self,
        tui: &mut tui::Tui,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        width: u16,
    ) {
        let mut continue_to_start = false;
        if let Some(Overlay::Transcript(overlay)) = self.overlay.as_mut() {
            let previous_state = overlay.set_history_state(if self.scrollback_has_older_history {
                TranscriptHistoryState::Partial
            } else {
                TranscriptHistoryState::Complete
            });
            continue_to_start = previous_state == TranscriptHistoryState::LoadingBeginning
                && self.scrollback_has_older_history;
        } else {
            let wrap_width = self.chat_widget.history_wrap_width(width);
            let rendered_rows = self
                .render_transcript_lines_for_reflow(wrap_width)
                .lines
                .len();
            self.schedule_immediate_resize_reflow(tui);
            if self.scrollback_history_needs_top_up(rendered_rows)
                && self.request_older_history_page(app_server, thread_id)
            {
                return;
            }
        }
        if continue_to_start
            && self.request_older_history_page(app_server, thread_id)
            && let Some(Overlay::Transcript(overlay)) = self.overlay.as_mut()
        {
            overlay.set_history_state(TranscriptHistoryState::LoadingBeginning);
        }
        if self.backtrack.overlay_preview_active {
            self.apply_backtrack_selection_internal(self.backtrack.nth_user_message);
        }
        tui.frame_requester().schedule_frame();
    }
}

/// Count the leading turns holding a page's items; older pages only reach the oldest turns.
fn leading_page_turns(turns: &[Turn], items: &[ThreadItem]) -> usize {
    let mut remaining = items.iter().map(ThreadItem::id).collect::<HashSet<_>>();
    let mut end = 0;
    for (index, turn) in turns.iter().enumerate() {
        if remaining.is_empty() {
            break;
        }
        let before = remaining.len();
        for item in &turn.items {
            remaining.remove(item.id());
        }
        if remaining.len() < before {
            end = index + 1;
        }
    }
    end
}

/// Classify internal review prompts across page boundaries before rendering any older items.
///
/// Classification flows forward from the oldest turn. Past the first `page_turns` turns and the
/// one after them, later turns keep their classification unless a review continues into them,
/// so the scan stops there instead of revisiting the whole loaded thread for every page.
fn hidden_review_item_ids(turns: &[Turn], page_turns: usize) -> HashSet<&str> {
    let mut hidden = HashSet::new();
    let mut review_mode = false;
    for (index, turn) in turns.iter().enumerate() {
        if index > page_turns && !review_mode {
            break;
        }
        let hidden_nested_review_turn = index
            .checked_sub(/*rhs*/ 1)
            .and_then(|previous| turns.get(previous))
            .is_some_and(|previous| {
                crate::app_backtrack::is_hidden_nested_review_turn(previous, turn)
            });
        for item in &turn.items {
            match item {
                ThreadItem::EnteredReviewMode { .. } => review_mode = true,
                ThreadItem::ExitedReviewMode { .. } => review_mode = false,
                ThreadItem::UserMessage { .. } if review_mode || hidden_nested_review_turn => {
                    hidden.insert(item.id());
                }
                _ => {}
            }
        }
    }
    hidden
}

/// Match repeated user messages from newest to oldest so identical visible prompts stay distinct.
fn hidden_transcript_indices(
    cells: &[Arc<dyn HistoryCell>],
    mut persisted_user_cells: Vec<(String, Arc<dyn HistoryCell>)>,
    hidden_item_ids: &HashSet<&str>,
) -> Vec<usize> {
    let mut indices = Vec::new();
    for (index, cell) in cells.iter().enumerate().rev() {
        let Some(user_cell) = cell.as_any().downcast_ref::<UserHistoryCell>() else {
            continue;
        };
        let Some(position) = persisted_user_cells.iter().rposition(|(_, projected)| {
            projected
                .as_any()
                .downcast_ref::<UserHistoryCell>()
                .is_some_and(|projected| {
                    projected.message == user_cell.message
                        && projected.text_elements == user_cell.text_elements
                        && projected.local_image_paths == user_cell.local_image_paths
                        && projected.remote_image_urls == user_cell.remote_image_urls
                })
        }) else {
            continue;
        };
        if hidden_item_ids.contains(persisted_user_cells[position].0.as_str()) {
            indices.push(index);
        }
        persisted_user_cells.truncate(position);
    }
    indices
}

#[cfg(test)]
#[path = "history_pagination_tests.rs"]
mod tests;
