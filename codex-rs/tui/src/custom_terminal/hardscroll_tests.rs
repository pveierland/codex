use super::*;
use crate::custom_terminal::Terminal;
use crate::test_backend::VT100Backend;
use pretty_assertions::assert_eq;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

const WIDTH: u16 = 24;
const HEIGHT: u16 = 10;

fn buffer(lines: &[String]) -> Buffer {
    let mut buffer = Buffer::empty(Rect::new(/*x*/ 0, /*y*/ 0, WIDTH, HEIGHT));
    for (y, line) in lines.iter().enumerate() {
        buffer.set_line(/*x*/ 0, y as u16, &Line::from(line.as_str()), WIDTH);
    }
    buffer
}

fn transcript(first: usize, header: Option<&str>, footer: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    lines.extend(header.map(str::to_owned));
    let body = usize::from(HEIGHT) - lines.len() - usize::from(footer.is_some());
    lines.extend((first..first + body).map(|row| format!("transcript row {row}")));
    lines.extend(footer.map(str::to_owned));
    lines
}

fn draw(terminal: &mut Terminal<VT100Backend>, lines: &[String]) {
    terminal
        .draw(|frame| {
            for (y, line) in lines.iter().enumerate() {
                let style = if y % 3 == 0 {
                    Style::default().bold()
                } else {
                    Style::default()
                };
                frame.buffer_mut().set_line(
                    /*x*/ 0,
                    y as u16,
                    &Line::from(line.as_str()).style(style),
                    WIDTH,
                );
            }
        })
        .expect("draw");
}

fn screen(terminal: &Terminal<VT100Backend>) -> Vec<String> {
    terminal
        .backend()
        .vt100()
        .screen()
        .rows(/*start*/ 0, WIDTH)
        .map(|row| row.trim_end().to_owned())
        .collect()
}

fn terminal() -> Terminal<VT100Backend> {
    let mut terminal = Terminal::with_options(VT100Backend::new(WIDTH, HEIGHT)).expect("terminal");
    terminal.set_viewport_area(Rect::new(/*x*/ 0, /*y*/ 0, WIDTH, HEIGHT));
    terminal.set_hardscroll(/*enabled*/ true);
    terminal
}

#[test]
fn plans_band_between_fixed_header_and_footer() {
    let previous = buffer(&transcript(
        /*first*/ 10,
        Some("header"),
        Some("composer"),
    ));
    for (first, shift) in [(13, 3), (7, -3), (11, 1)] {
        let current = buffer(&transcript(first, Some("header"), Some("composer")));
        assert_eq!(
            plan(&previous, &current),
            Some(Hardscroll {
                top: 1,
                bottom: HEIGHT - 1,
                shift,
            })
        );
    }
}

#[test]
fn skips_frames_without_enough_shifted_rows() {
    let previous = buffer(&transcript(
        /*first*/ 0,
        /*header*/ None,
        Some("composer"),
    ));
    let mut typed = transcript(/*first*/ 0, /*header*/ None, Some("composer"));
    typed[usize::from(HEIGHT) - 1] = "composer typed".to_owned();
    assert_eq!(plan(&previous, &buffer(&typed)), None);
    // A jump past the whole viewport shares no rows with the previous frame.
    let jumped = buffer(&transcript(
        /*first*/ 100,
        /*header*/ None,
        Some("composer"),
    ));
    assert_eq!(plan(&previous, &jumped), None);
}

#[test]
fn scrolled_frames_match_the_rendered_buffer() {
    let mut terminal = terminal();
    for first in [20, 23, 22, 15, 16, 30, 29, 0] {
        let lines = transcript(first, Some("› pinned prompt"), Some("› composer"));
        draw(&mut terminal, &lines);
        assert_eq!(screen(&terminal), lines, "first visible row {first}");
    }
}

#[test]
fn random_frames_match_the_rendered_buffer() {
    // Deterministic LCG so failures reproduce.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = |bound: usize| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) as usize % bound
    };
    let mut terminal = terminal();
    let mut first = 50usize;
    for step in 0..500 {
        first = (first + next(9)).saturating_sub(4);
        let mut lines = transcript(
            first,
            (next(4) != 0).then_some("header"),
            (next(4) != 0).then_some("footer"),
        );
        // Occasionally edit, blank, or duplicate rows so plans must tolerate imperfect shifts.
        for _ in 0..next(3) {
            let row = next(lines.len());
            lines[row] = match next(3) {
                0 => String::new(),
                1 => format!("edited {step}"),
                _ => lines[next(lines.len())].clone(),
            };
        }
        draw(&mut terminal, &lines);
        assert_eq!(screen(&terminal), lines, "step {step}");
    }
}
