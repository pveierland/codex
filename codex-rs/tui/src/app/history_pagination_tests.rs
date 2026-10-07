//! Older pages must preserve newer events and identify repeated hidden prompts precisely.

use super::*;
use crate::app::test_support::make_test_app;
use crate::history_cell::ComputerActivityCell;
use crate::history_cell::FinalMessageSeparator;
use codex_app_server_protocol::TurnItemsView;
use codex_app_server_protocol::UserInput;
use pretty_assertions::assert_eq;

fn turn(id: &str, status: TurnStatus, item_ids: &[&str]) -> Turn {
    Turn {
        id: id.to_string(),
        root_turn_id: None,
        items: item_ids
            .iter()
            .map(|id| ThreadItem::UserMessage {
                id: id.to_string(),
                client_id: None,
                content: Vec::new(),
            })
            .collect(),
        items_view: TurnItemsView::Full,
        status,
        error: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
    }
}

#[test]
fn page_turns_end_after_the_newest_turn_holding_page_items() {
    let turns = vec![
        turn("fetched", TurnStatus::Completed, &["a"]),
        turn("shared", TurnStatus::Completed, &["b", "c"]),
        turn("newer", TurnStatus::Completed, &["d"]),
    ];
    let items = turns[..2]
        .iter()
        .flat_map(|turn| turn.items.clone())
        .take(2)
        .collect::<Vec<_>>();
    assert_eq!(leading_page_turns(&turns, &items), 2);
    assert_eq!(leading_page_turns(&turns, &[]), 0);
}

#[test]
fn review_classification_continues_only_while_a_review_spans_newer_turns() {
    let review = |id: &str, items: Vec<ThreadItem>| Turn {
        items,
        ..turn(id, TurnStatus::Completed, &[])
    };
    let prompt = |id: &str| ThreadItem::UserMessage {
        id: id.to_string(),
        client_id: None,
        content: Vec::new(),
    };
    let entered = ThreadItem::EnteredReviewMode {
        id: "enter".to_string(),
        review: "changes".to_string(),
    };
    let exited = ThreadItem::ExitedReviewMode {
        id: "exit".to_string(),
        review: "changes".to_string(),
    };
    // The page holds only the first turn; the review it opens hides prompts until it exits.
    let turns = vec![
        review("page", vec![entered, prompt("in-page")]),
        review("next", vec![prompt("continues")]),
        review("closing", vec![exited, prompt("after")]),
        review("later", vec![prompt("unaffected")]),
    ];
    assert_eq!(
        hidden_review_item_ids(&turns, /*page_turns*/ 1),
        HashSet::from(["in-page", "continues"])
    );
    assert_eq!(
        hidden_review_item_ids(&turns[1..], /*page_turns*/ 0),
        HashSet::new()
    );
}

fn user_cell(message: &str) -> Arc<dyn HistoryCell> {
    Arc::new(UserHistoryCell {
        spoken: false,
        message: message.to_string(),
        text_elements: Vec::new(),
        local_image_paths: Vec::new(),
        remote_image_urls: Vec::new(),
    })
}

#[test]
fn repeated_prompt_matching_removes_only_the_hidden_occurrence() {
    let cells = vec![user_cell("repeat"), user_cell("other"), user_cell("repeat")];
    let persisted = vec![
        ("unloaded".to_string(), user_cell("repeat")),
        ("hidden".to_string(), user_cell("repeat")),
        ("middle".to_string(), user_cell("other")),
        ("visible".to_string(), user_cell("repeat")),
    ];
    assert_eq!(
        hidden_transcript_indices(&cells, persisted, &HashSet::from(["hidden", "unloaded"])),
        vec![0],
    );
}

#[tokio::test]
async fn hidden_last_item_keeps_turn_groups_and_completion_boundaries() {
    let computer = |id: &str| {
        serde_json::from_value(serde_json::json!({
            "type": "mcpToolCall", "id": id, "server": "cua_repl", "tool": "js",
            "status": "completed", "arguments": {"title": format!("Inspect {id}")},
            "appContext": null, "pluginId": null, "readOnlyHint": null,
            "result": {"content": [], "structuredContent": null},
            "error": null, "durationMs": 10
        }))
        .expect("valid completed computer call")
    };
    for status in [
        TurnStatus::Completed,
        TurnStatus::Failed,
        TurnStatus::Interrupted,
    ] {
        let mut app = make_test_app().await;
        let cwd = app.config.cwd.clone();
        let thread_id = ThreadId::new();
        let expected_completions = usize::from(status == TurnStatus::Completed);
        let mut first = turn("first", status, &[]);
        first.items = vec![
            ThreadItem::EnteredReviewMode {
                id: "enter-review".to_string(),
                review: "current changes".to_string(),
            },
            computer("older-call"),
            ThreadItem::UserMessage {
                id: "hidden".to_string(),
                client_id: None,
                content: vec![UserInput::Text {
                    text: "internal review prompt".to_string(),
                    text_elements: Vec::new(),
                }],
            },
        ];
        first.completed_at = Some(1_700_000_000);
        first.duration_ms = Some(125_000);
        let mut second = turn("second", TurnStatus::InProgress, &[]);
        second.items = vec![computer("newer-call")];
        let turns = vec![first, second];
        let hidden = hidden_review_item_ids(&turns, turns.len());
        assert_eq!(hidden, HashSet::from(["hidden"]));
        let items = turns
            .iter()
            .flat_map(|turn| turn.items.clone())
            .collect::<Vec<_>>();

        let cells = app.project_older_history_cells(
            items.clone(),
            &turns,
            &hidden,
            PageProjection {
                thread_id,
                cwd: &cwd,
                visibility: RawReasoningVisibility::Hidden,
            },
            |_| None,
        );
        let groups = cells
            .iter()
            .filter_map(|cell| cell.as_any().downcast_ref::<ComputerActivityCell>())
            .map(|cell| cell.call_ids().map(str::to_string).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        let completions = cells
            .iter()
            .filter(|cell| cell.as_any().is::<FinalMessageSeparator>())
            .count();
        let visible_users = cells
            .iter()
            .filter(|cell| cell.as_any().is::<UserHistoryCell>())
            .count();
        assert_eq!(
            (groups, completions, visible_users),
            (
                vec![
                    vec!["older-call".to_string()],
                    vec!["newer-call".to_string()]
                ],
                expected_completions,
                0,
            ),
        );

        let repeated = app.project_older_history_cells(
            items,
            &turns,
            &hidden,
            PageProjection {
                thread_id,
                cwd: &cwd,
                visibility: RawReasoningVisibility::Hidden,
            },
            |_| None,
        );
        assert_eq!(
            repeated
                .iter()
                .filter(|cell| cell.as_any().is::<FinalMessageSeparator>())
                .count(),
            0,
        );
    }
}

#[tokio::test]
async fn browsing_waits_for_a_prompt_outside_the_initial_history_window() -> Result<()> {
    let mut app = make_test_app().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    app.scrollback_has_older_history = true;
    app.transcript_cells = vec![
        user_cell(""),
        Arc::new(crate::history_cell::PlainHistoryCell::new(vec![
            "recent answer".into(),
        ])),
    ];
    app.handle_backtrack_esc_key(&mut tui);
    app.handle_backtrack_esc_key(&mut tui);
    assert!(app.backtrack.overlay_preview_active && app.browsing_needs_history());
    app.prepend_older_transcript_cells(vec![
        user_cell(""),
        Arc::new(crate::history_cell::PlainHistoryCell::new(vec![
            "earlier answer".into(),
        ])),
    ]);
    assert!(app.browsing_needs_history());
    app.prepend_older_transcript_cells(vec![
        user_cell(""),
        user_cell("older prompt"),
        user_cell("latest prompt"),
    ]);
    app.apply_backtrack_selection_internal(app.backtrack.nth_user_message);
    assert_eq!(
        (
            app.backtrack.nth_user_message,
            crate::app_backtrack::nth_user_position(
                &app.transcript_cells,
                app.backtrack.nth_user_message
            )
        ),
        (1, Some(2))
    );
    assert!(!app.browsing_needs_history());
    app.apply_backtrack_selection_internal(/*nth_user_message*/ 0);
    let mut review_turn = turn(
        "review",
        TurnStatus::Completed,
        &["older prompt", "latest prompt"],
    );
    for item in &mut review_turn.items {
        if let ThreadItem::UserMessage { id, content, .. } = item {
            *content = vec![UserInput::Text {
                text: id.clone(),
                text_elements: Vec::new(),
            }];
        }
    }
    review_turn.items.insert(
        /*index*/ 0,
        ThreadItem::EnteredReviewMode {
            id: "enter".to_string(),
            review: "review".to_string(),
        },
    );
    review_turn.items.insert(
        /*index*/ 2,
        ThreadItem::ExitedReviewMode {
            id: "exit".to_string(),
            review: "review".to_string(),
        },
    );
    let turns = [review_turn];
    app.remove_hidden_review_cells(
        &mut tui,
        &turns,
        &hidden_review_item_ids(&turns, turns.len()),
        PageProjection {
            thread_id: ThreadId::new(),
            cwd: &app.config.cwd.clone(),
            visibility: RawReasoningVisibility::Hidden,
        },
    );
    assert!(!app.backtrack.overlay_preview_active);
    assert_eq!(
        app.transcript_cells
            .iter()
            .filter_map(|cell| cell.as_any().downcast_ref::<UserHistoryCell>())
            .filter(|cell| !cell.message.is_empty())
            .map(|cell| cell.message.as_str())
            .collect::<Vec<_>>(),
        vec!["latest prompt"]
    );
    tui.set_owned_screen(/*owned*/ false)?;
    Ok(())
}

#[tokio::test]
async fn older_entries_are_dated_by_the_item_that_started_them() {
    let computer = |id: &str| -> ThreadItem {
        serde_json::from_value(serde_json::json!({
            "type": "mcpToolCall", "id": id, "server": "cua_repl", "tool": "js",
            "status": "completed", "arguments": {"title": format!("Inspect {id}")},
            "appContext": null, "pluginId": null, "readOnlyHint": null,
            "result": {"content": [], "structuredContent": null},
            "error": null, "durationMs": 10
        }))
        .expect("valid completed computer call")
    };
    let message = |id: &str| ThreadItem::AgentMessage {
        id: id.to_string(),
        text: format!("message {id}"),
        phase: None,
        memory_citation: None,
        delivery: None,
        questions: None,
    };
    let mut app = make_test_app().await;
    app.history_times = Default::default();
    let cwd = app.config.cwd.clone();
    let mut page_turn = turn("turn", TurnStatus::InProgress, &[]);
    page_turn.started_at = Some(1_000);
    page_turn.items = vec![
        computer("a"),
        computer("b"),
        message("untimed"),
        message("timed"),
    ];
    let items = page_turn.items.clone();
    let times = HashMap::from([("a", 2_000_000), ("b", 2_500_000), ("timed", 3_000_000)]);

    let cells = app.project_older_history_cells(
        items,
        std::slice::from_ref(&page_turn),
        &HashSet::new(),
        PageProjection {
            thread_id: ThreadId::new(),
            cwd: &cwd,
            visibility: RawReasoningVisibility::Hidden,
        },
        |item_id| times.get(item_id).copied(),
    );
    let at = |index| {
        app.history_times
            .time_at(&cells, index)
            .map(|time| time.timestamp())
    };

    assert_eq!(cells.len(), 3);
    // The group takes its first call's time; an untimed item falls back to its turn's start.
    assert_eq!(
        [at(0), at(1), at(2)],
        [Some(2_000), Some(1_000), Some(3_000)]
    );
}
