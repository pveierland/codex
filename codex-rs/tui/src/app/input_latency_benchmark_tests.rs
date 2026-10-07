//! Measure owned-transcript input latency against session length and terminal size.
//!
//! The benchmark is ignored by default because it is timing-sensitive. Redirect stdout to a file
//! so terminal output volume can be measured:
//! `cargo test -p codex-tui --lib input_latency_benchmark -- --ignored --nocapture > /tmp/out`
//!
//! The report is written to `codex-input-latency-benchmark.txt` in the system temporary directory.

use super::*;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::new_user_prompt;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use std::fmt::Write as _;
use std::path::Path;

const SESSION_TURNS: [usize; 3] = [25, 250, 2500];
const SCREEN_SIZES: [Size; 2] = [
    Size {
        width: 80,
        height: 24,
    },
    Size {
        width: 240,
        height: 70,
    },
];
const EVENTS: usize = 60;
/// Notches in one fast wheel flick, and flicks in one long scroll back through history.
const FLICK_NOTCHES: usize = 300;
const FLICKS: usize = 20;

const WORDS: [&str; 24] = [
    "scheduler",
    "queue",
    "worker",
    "backpressure",
    "latency",
    "buffer",
    "retry",
    "budget",
    "frame",
    "socket",
    "parser",
    "cache",
    "index",
    "stream",
    "token",
    "handler",
    "snapshot",
    "cursor",
    "region",
    "layout",
    "commit",
    "branch",
    "signal",
    "channel",
];

/// Deterministic varied prose so consecutive screens differ like a real transcript.
fn prose(seed: usize, words: usize) -> String {
    let mut state = seed as u64 ^ 0x9e37_79b9_7f4a_7c15;
    (0..words)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            WORDS[(state >> 33) as usize % WORDS.len()]
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn answer(turn: usize) -> String {
    let first = prose(turn * 7, /*words*/ 40);
    let second = prose(turn * 7 + 1, /*words*/ 25);
    let item = prose(turn * 7 + 2, /*words*/ 12);
    let ident = prose(turn * 7 + 3, /*words*/ 2).replace(' ', "_");
    let closing = prose(turn * 7 + 4, /*words*/ 30);
    format!(
        "## Step {turn}: {item}\n\n{first}. See `worker::{ident}` for turn {turn}.\n\n\
         - first item explains **why** the {item}\n\
         - second item links to [the docs](https://example.com/docs/{turn})\n\
         - third item mentions `Arc<Mutex<State>>` and {second}\n\n\
         ```rust\n\
         fn {ident}(&mut self, cx: &mut Context<'_>) -> Poll<Option<Item>> {{\n\
         \x20   let item = ready!(self.inner.poll_recv(cx)); // {turn}\n\
         \x20   if let Some(item) = item {{\n\
         \x20       self.metrics.record(item.len() + {turn});\n\
         \x20       return Poll::Ready(Some(item));\n\
         \x20   }}\n\
         \x20   Poll::Ready(None)\n\
         }}\n\
         ```\n\n\
         | column | value |\n|---|---|\n| turn | {turn} |\n| {ident} | done |\n\n{closing}.\n"
    )
}

fn session(cwd: &Path, turns: usize) -> Vec<Arc<dyn HistoryCell>> {
    let mut cells: Vec<Arc<dyn HistoryCell>> = Vec::with_capacity(turns * 2);
    for turn in 0..turns {
        cells.push(Arc::new(new_user_prompt(
            format!("Question {turn}: {}?", prose(turn * 7 + 5, /*words*/ 9)),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )));
        cells.push(Arc::new(AgentMarkdownCell::new(answer(turn), cwd)));
    }
    cells
}

/// Bytes written to stdout so far; zero unless stdout is a regular file.
fn stdout_bytes() -> u64 {
    let _ = std::io::Write::flush(&mut std::io::stdout());
    std::fs::read_to_string("/proc/self/fdinfo/1")
        .ok()
        .and_then(|info| {
            info.lines()
                .find_map(|line| line.strip_prefix("pos:"))
                .and_then(|pos| pos.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn wheel(kind: MouseEventKind, size: Size) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind,
        column: size.width / 2,
        row: 2,
        modifiers: KeyModifiers::NONE,
    })
}

struct Sample {
    micros_per_event: f64,
    bytes_per_event: u64,
}

impl std::fmt::Display for Sample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:>9.1}us {:>8}B",
            self.micros_per_event, self.bytes_per_event
        )
    }
}

/// When the simulated event loop draws while replaying input.
///
/// Real input arrives between frames at any rate, so each scenario names the cadence it models.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DrawCadence {
    /// Draw after each event, as when input arrives slower than frames.
    EachEvent,
    /// Draw once after all events, as when queued input is handled before the next frame.
    AfterBurst,
}

impl DrawCadence {
    /// Whether to draw after handling event number `handled` (1-based) of `total`.
    fn draws_after(self, handled: usize, total: usize) -> bool {
        match self {
            Self::EachEvent => true,
            Self::AfterBurst => handled == total,
        }
    }
}

/// Handle `events` and report the average cost of each drawn frame, including its input.
async fn measure(
    app: &mut App,
    tui: &mut tui::Tui,
    server: &mut AppServerSession,
    size: Size,
    cadence: DrawCadence,
    events: Vec<TuiEvent>,
) -> Result<Sample> {
    let bytes = stdout_bytes();
    let start = Instant::now();
    let total = events.len();
    let mut frames = 0;
    for (index, event) in events.into_iter().enumerate() {
        app.handle_tui_event(tui, server, event).await?;
        if cadence.draws_after(index + 1, total) {
            app.render_owned_transcript(tui, size)?;
            frames += 1;
        }
    }
    let elapsed = start.elapsed();
    Ok(Sample {
        micros_per_event: elapsed.as_secs_f64() * 1e6 / frames as f64,
        bytes_per_event: (stdout_bytes() - bytes) / frames as u64,
    })
}

fn wheel_events(kind: MouseEventKind, size: Size, notches: usize) -> Vec<TuiEvent> {
    (0..notches).map(|_| wheel(kind, size)).collect()
}

fn typing_events() -> Vec<TuiEvent> {
    (0..EVENTS)
        .map(|index| {
            let character = char::from(b'a' + (index % 26) as u8);
            TuiEvent::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        })
        .collect()
}

fn key_events(key: KeyEvent, count: usize) -> Vec<TuiEvent> {
    (0..count).map(|_| TuiEvent::Key(key)).collect()
}

async fn return_to_latest(app: &mut App, tui: &mut tui::Tui, size: Size) -> Result<()> {
    app.transcript_view.jump_to_latest();
    app.render_owned_transcript(tui, size)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "timing benchmark; run explicitly with --ignored"]
async fn input_latency_benchmark() -> Result<()> {
    let (mut app, _events, _operations) = crate::app::tests::make_test_app_with_channels().await;
    let mut server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let cwd = app.config.cwd.to_path_buf();
    let mut long_scroll = Vec::new();
    let mut report =
        String::from("cost per drawn frame including its input: time, terminal bytes\n");
    writeln!(
        report,
        "{:>9} {:>5} | {:>19} | {:>19} | {:>19} | {:>19} | {:>19}",
        "size",
        "turns",
        "idle frame",
        "wheel/notch",
        format!("{EVENTS} notches"),
        "typing/key",
        format!("{EVENTS} keys")
    )?;
    for size in SCREEN_SIZES {
        let mut tui = crate::tui::test_support::make_test_tui_with_size(size)?;
        tui.set_owned_screen(/*owned*/ true)?;
        for turns in SESSION_TURNS {
            app.transcript_view = Default::default();
            app.transcript_cells = session(&cwd, turns);
            app.chat_widget.apply_external_edit(String::new());
            app.render_owned_transcript(&mut tui, size)?;

            let bytes = stdout_bytes();
            let start = Instant::now();
            for _ in 0..EVENTS {
                app.render_owned_transcript(&mut tui, size)?;
            }
            let idle = Sample {
                micros_per_event: start.elapsed().as_secs_f64() * 1e6 / EVENTS as f64,
                bytes_per_event: (stdout_bytes() - bytes) / EVENTS as u64,
            };

            let up = MouseEventKind::ScrollUp;
            let mut scenarios = Vec::new();
            for (cadence, events) in [
                (DrawCadence::EachEvent, wheel_events(up, size, EVENTS)),
                (DrawCadence::AfterBurst, wheel_events(up, size, EVENTS)),
                (DrawCadence::EachEvent, typing_events()),
                (DrawCadence::AfterBurst, typing_events()),
            ] {
                if matches!(events.first(), Some(TuiEvent::Key(_))) {
                    return_to_latest(&mut app, &mut tui, size).await?;
                }
                scenarios
                    .push(measure(&mut app, &mut tui, &mut server, size, cadence, events).await?);
            }
            let [wheel, wheel_burst, typing, typing_burst] = <[Sample; 4]>::try_from(scenarios)
                .map_err(|_| color_eyre::eyre::eyre!("expected four scenarios"))?;
            app.chat_widget.apply_external_edit(String::new());
            writeln!(
                report,
                "{:>4}x{:<4} {:>5} | {idle} | {wheel} | {wheel_burst} | {typing} | {typing_burst}",
                size.width, size.height, turns
            )?;

            // Long scrolls back into history: repeated fast flicks, paging, and the beginning.
            return_to_latest(&mut app, &mut tui, size).await?;
            let mut flick = Sample {
                micros_per_event: 0.0,
                bytes_per_event: 0,
            };
            for _ in 0..FLICKS {
                let sample = measure(
                    &mut app,
                    &mut tui,
                    &mut server,
                    size,
                    DrawCadence::AfterBurst,
                    wheel_events(up, size, FLICK_NOTCHES),
                )
                .await?;
                flick.micros_per_event += sample.micros_per_event / FLICKS as f64;
                flick.bytes_per_event += sample.bytes_per_event / FLICKS as u64;
            }
            return_to_latest(&mut app, &mut tui, size).await?;
            let page_up = measure(
                &mut app,
                &mut tui,
                &mut server,
                size,
                DrawCadence::EachEvent,
                key_events(KeyEvent::from(KeyCode::PageUp), EVENTS),
            )
            .await?;
            return_to_latest(&mut app, &mut tui, size).await?;
            let beginning = measure(
                &mut app,
                &mut tui,
                &mut server,
                size,
                DrawCadence::AfterBurst,
                key_events(KeyEvent::new(KeyCode::Home, KeyModifiers::CONTROL), 1),
            )
            .await?;
            return_to_latest(&mut app, &mut tui, size).await?;
            long_scroll.push(format!(
                "{:>4}x{:<4} {:>5} | {flick} | {page_up} | {beginning}",
                size.width, size.height, turns
            ));
        }
        tui.set_owned_screen(/*owned*/ false)?;
    }
    writeln!(
        report,
        "\n{:>9} {:>5} | {:>19} | {:>19} | {:>19}",
        "size",
        "turns",
        format!("{FLICK_NOTCHES}-notch flick"),
        "page up/key",
        "jump to beginning"
    )?;
    for line in long_scroll {
        writeln!(report, "{line}")?;
    }
    std::fs::write(
        std::env::temp_dir().join("codex-input-latency-benchmark.txt"),
        report,
    )?;
    server.shutdown().await?;
    Ok(())
}
