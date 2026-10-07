//! Project persisted items into the same rich cells as completed live output.
//! Tool grouping spans hidden reasoning but stops at visible content and turn boundaries.
//! Only one computer or exploration group can be pending at a time.

use std::sync::Arc;

use crate::app_server_session::AppServerSession;
use crate::app_server_session::HistoryHydrationScope;
use crate::git_action_directives::parse_assistant_markdown;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::PrefixedWrappedHistoryCell;
use crate::history_cell::ReasoningSummaryCell;
use crate::history_cell::UserHistoryCell;
use crate::history_cell::split_reasoning_summary_parts;
use crate::inline_visualization::InlineVisualizationContext;
use crate::legacy_core::config::Config;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserInput;
use codex_protocol::ThreadId;
use codex_protocol::items::UserMessageItem;
use codex_utils_absolute_path::AbsolutePathBuf;
use ratatui::style::Stylize as _;

mod activity_pages;
mod computer_groups;
mod exploration_groups;
mod other_items;
pub(crate) mod tools;

pub(crate) use activity_pages::fold_trailing_activity_details;
pub(crate) use activity_pages::is_hidden_activity_detail;
#[allow(
    unused_imports,
    reason = "Used by later layers of the TUI refresh stack."
)]
pub(crate) use computer_groups::join_computer_groups;
pub(crate) use computer_groups::older_computer_group;
pub(crate) use exploration_groups::join_exploration_groups;
pub(crate) use exploration_groups::older_exploration_group;

pub(crate) type TranscriptCells = Vec<Arc<dyn HistoryCell>>;

enum PendingActivity {
    Computer(crate::history_cell::ComputerActivityCell),
    Exploration(crate::exec_cell::ExecCell),
}

impl PendingActivity {
    fn flush(pending: &mut Option<Self>, cells: &mut TranscriptCells) {
        let cell: Arc<dyn HistoryCell> = match pending.take() {
            Some(Self::Computer(group)) => Arc::new(group),
            Some(Self::Exploration(group)) => Arc::new(group),
            None => return,
        };
        cells.push(cell);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RawReasoningVisibility {
    Hidden,
    Visible,
}

pub(crate) async fn load_session_transcript(
    app_server: &mut AppServerSession,
    thread_id: ThreadId,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
) -> std::io::Result<TranscriptCells> {
    let mut thread = app_server
        .thread_read(thread_id, /*include_turns*/ false)
        .await
        .map_err(std::io::Error::other)?;
    app_server
        .hydrate_initial_thread_history(
            &mut thread,
            /*turn_cursor*/ None,
            /*item_cursor*/ None,
            /*config*/ None,
            /*local_settings*/ None,
            HistoryHydrationScope::Complete,
        )
        .await
        .map_err(std::io::Error::other)?;
    Ok(thread_to_transcript_cells(
        thread,
        raw_reasoning_visibility,
        config,
    ))
}

pub(crate) fn thread_to_transcript_cells(
    thread: Thread,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
) -> TranscriptCells {
    let cwd = thread.cwd;
    let thread_id = ThreadId::from_string(&thread.id).ok();
    let mut cells = thread
        .turns
        .into_iter()
        .flat_map(|turn| {
            thread_items_to_transcript_cells(
                thread_id,
                &cwd,
                turn.items,
                raw_reasoning_visibility,
                config,
            )
        })
        .collect::<TranscriptCells>();
    if cells.is_empty() {
        cells.push(Arc::new(PlainHistoryCell::new(vec![
            "No transcript content available".italic().dim().into(),
        ])));
    }
    cells
}

pub(crate) fn thread_items_to_transcript_cells(
    thread_id: Option<ThreadId>,
    cwd: &AbsolutePathBuf,
    items: impl IntoIterator<Item = ThreadItem>,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
) -> TranscriptCells {
    project_items(
        thread_id,
        cwd,
        items,
        raw_reasoning_visibility,
        config,
        /*sources*/ None,
    )
}

/// Like [`thread_items_to_transcript_cells`], also returning for each cell the index of the item
/// that started it, so callers can attribute per-item metadata such as times to grouped cells.
pub(crate) fn thread_items_to_transcript_cells_with_sources(
    thread_id: Option<ThreadId>,
    cwd: &AbsolutePathBuf,
    items: impl IntoIterator<Item = ThreadItem>,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
) -> (TranscriptCells, Vec<usize>) {
    let mut sources = Vec::new();
    let cells = project_items(
        thread_id,
        cwd,
        items,
        raw_reasoning_visibility,
        config,
        Some(&mut sources),
    );
    (cells, sources)
}

fn project_items(
    thread_id: Option<ThreadId>,
    cwd: &AbsolutePathBuf,
    items: impl IntoIterator<Item = ThreadItem>,
    raw_reasoning_visibility: RawReasoningVisibility,
    config: Option<&Config>,
    mut sources: Option<&mut Vec<usize>>,
) -> TranscriptCells {
    let inline_visualization_context = config.and_then(|config| {
        thread_id.and_then(|thread_id| InlineVisualizationContext::from_config(config, thread_id))
    });
    let mut cells: TranscriptCells = Vec::new();
    let mut pending = None;
    // The item that started the pending group.
    let mut pending_start = 0;
    for (index, item) in items.into_iter().enumerate() {
        let before = cells.len();
        let had_pending = pending.is_some();
        project_item(
            item,
            cwd,
            raw_reasoning_visibility,
            &inline_visualization_context,
            &mut pending,
            &mut cells,
        );
        // Every branch flushes a pending group before pushing anything else, so when cells were
        // added the first one is the earlier group.
        let flushed = had_pending && cells.len() > before;
        if let Some(sources) = sources.as_deref_mut() {
            for position in before..cells.len() {
                sources.push(if flushed && position == before {
                    pending_start
                } else {
                    index
                });
            }
        }
        if pending.is_some() && (!had_pending || flushed) {
            pending_start = index;
        }
    }
    let before = cells.len();
    PendingActivity::flush(&mut pending, &mut cells);
    if let Some(sources) = sources {
        sources.extend((before..cells.len()).map(|_| pending_start));
    }
    cells
}

fn project_item(
    item: ThreadItem,
    cwd: &AbsolutePathBuf,
    raw_reasoning_visibility: RawReasoningVisibility,
    inline_visualization_context: &Option<InlineVisualizationContext>,
    pending: &mut Option<PendingActivity>,
    cells: &mut TranscriptCells,
) {
    if matches!(item, ThreadItem::Reasoning { .. })
        && let Some(group) = pending
    {
        for cell in item_to_cells(
            item,
            cwd,
            raw_reasoning_visibility,
            inline_visualization_context.clone(),
        ) {
            match group {
                PendingActivity::Computer(group) => group.group.push_detail(cell),
                PendingActivity::Exploration(group) => group.group.push_detail(cell),
            }
        }
        return;
    }
    if let Some(cell) = tools::historical_tool_fallback(&item) {
        PendingActivity::flush(pending, cells);
        cells.push(Arc::new(cell));
        return;
    }
    match item {
        item @ ThreadItem::McpToolCall { .. } => {
            let Some(call) = tools::McpHistory::from_item(item) else {
                PendingActivity::flush(pending, cells);
                return;
            };
            if call.invocation.is_computer_activity() {
                match pending {
                    Some(PendingActivity::Computer(group)) => {
                        computer_groups::append(group, call);
                    }
                    Some(PendingActivity::Exploration(_)) | None => {
                        PendingActivity::flush(pending, cells);
                        let mut group = crate::history_cell::ComputerActivityCell::default();
                        computer_groups::append(&mut group, call);
                        *pending = Some(PendingActivity::Computer(group));
                    }
                }
            } else {
                PendingActivity::flush(pending, cells);
                let cell = call.into_cell();
                cells.push(Arc::new(cell));
            }
        }
        item @ ThreadItem::CommandExecution { .. } => {
            if matches!(pending, Some(PendingActivity::Computer(_))) {
                PendingActivity::flush(pending, cells);
            }
            if let Some(command) = tools::CommandHistory::from_item(item) {
                let newer = command.into_cell();
                if let Some(PendingActivity::Exploration(group)) = pending {
                    if let Err(newer) = group.append_completed(newer) {
                        PendingActivity::flush(pending, cells);
                        *pending = Some(PendingActivity::Exploration(newer));
                    }
                } else {
                    *pending = Some(PendingActivity::Exploration(newer));
                }
                if let Some(PendingActivity::Exploration(group)) = pending
                    && group.should_flush()
                {
                    PendingActivity::flush(pending, cells);
                }
            }
        }
        item => {
            let projected = item_to_cells(
                item,
                cwd,
                raw_reasoning_visibility,
                inline_visualization_context.clone(),
            );
            if !projected.is_empty() {
                PendingActivity::flush(pending, cells);
                cells.extend(projected);
            }
        }
    }
}

/// Project one item without changing the active widget or its turn lifecycle.
fn item_to_cells(
    item: ThreadItem,
    cwd: &AbsolutePathBuf,
    raw_reasoning_visibility: RawReasoningVisibility,
    inline_visualization_context: Option<InlineVisualizationContext>,
) -> TranscriptCells {
    let mut cells: TranscriptCells = Vec::new();
    match item {
        ThreadItem::UserMessage {
            id,
            client_id,
            content,
        } => {
            if content.iter().any(|input| {
                matches!(
                    input,
                    UserInput::Audio { .. } | UserInput::LocalAudio { .. }
                )
            }) {
                tracing::warn!(
                    user_message_id = id,
                    "audio user inputs are not supported by the TUI and will be omitted"
                );
            }
            let item = UserMessageItem {
                id,
                client_id,
                content: content
                    .into_iter()
                    .map(codex_app_server_protocol::UserInput::into_core)
                    .collect(),
            };
            let message = item.message();
            let reply_text = crate::async_question_reply::display_text(&message);
            let text_elements = if reply_text.is_some() {
                Vec::new()
            } else {
                item.text_elements()
            };
            cells.push(Arc::new(UserHistoryCell {
                spoken: false,
                message: reply_text.unwrap_or(message),
                text_elements,
                local_image_paths: item.local_image_paths(),
                remote_image_urls: item.image_urls(),
            }));
        }
        ThreadItem::AgentMessage { text, .. } => {
            let parsed = parse_assistant_markdown(&text, cwd.as_path());
            if !parsed.visible_markdown.trim().is_empty() {
                cells.push(Arc::new(AgentMarkdownCell::new_with_inline_visualizations(
                    parsed.visible_markdown,
                    cwd.as_path(),
                    inline_visualization_context,
                )));
            }
        }
        ThreadItem::FunctionCallOutput {
            name,
            namespace,
            output,
            ..
        } => {
            if let Some((source_thread_id, prompt)) =
                crate::dynamic_tools::parse_delegated_tool_output(
                    &name,
                    namespace.as_deref(),
                    &output,
                )
            {
                cells.push(Arc::new(PrefixedWrappedHistoryCell::new(
                    format!("Sent by Codex from task {source_thread_id}\n{prompt}"),
                    "• ".dim(),
                    "  ",
                )));
            }
        }
        ThreadItem::Plan { text, .. } => {
            if !text.trim().is_empty() {
                cells.push(Arc::new(crate::history_cell::new_proposed_plan(
                    text,
                    cwd.as_path(),
                )));
            }
        }
        ThreadItem::Reasoning {
            id,
            summary,
            content,
        } => {
            let (header, mut text) = split_reasoning_summary_parts(&summary);
            if matches!(raw_reasoning_visibility, RawReasoningVisibility::Visible)
                && !content.is_empty()
            {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&content.join("\n\n"));
            }
            if !text.trim().is_empty() {
                let mut cell = ReasoningSummaryCell::new(
                    header,
                    text,
                    cwd.as_path(),
                    /*transcript_only*/ true,
                );
                cell.set_source_item_id(id);
                cells.push(Arc::new(cell));
            }
        }
        item @ ThreadItem::CommandExecution { .. } => {
            if let Some(command) = tools::CommandHistory::from_item(item) {
                cells.push(Arc::new(command.into_cell()));
            }
        }
        other => cells.extend(other_items::cells(other, cwd)),
    }
    cells
}
