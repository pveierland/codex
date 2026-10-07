use super::*;
use crate::history_cell::PlainHistoryCell;
use pretty_assertions::assert_eq;

fn cell(text: &'static str) -> Arc<dyn HistoryCell> {
    Arc::new(PlainHistoryCell::new(vec![text.into()]))
}

fn at(unix_seconds: i64) -> DateTime<Local> {
    local_time(unix_seconds).expect("valid time")
}

#[test]
fn replayed_cells_take_the_turn_time_and_live_cells_the_present() {
    let mut times = HistoryTimes::default();
    let replayed = cell("replayed");
    let live = cell("live");
    times.set_replay_clock(Some(at(1_000)));
    times.record_inserted(&replayed);
    times.set_replay_clock(None);
    let before = Local::now();
    times.record_inserted(&live);

    let cells = [replayed, live];
    assert_eq!(times.time_at(&cells, 0), Some(at(1_000)));
    assert!(times.time_at(&cells, 1).is_some_and(|time| time >= before));
}

#[test]
fn untimed_cells_take_the_nearest_earlier_time() {
    let mut times = HistoryTimes::default();
    let cells = [cell("timed"), cell("notice"), cell("later")];
    times.record(&cells[0], at(1_000));

    assert_eq!(times.time_at(&cells, 1), Some(at(1_000)));
    assert_eq!(times.time_at(&cells, 2), Some(at(1_000)));
    // The live entry past the committed cells uses the latest committed time.
    assert_eq!(times.time_at(&cells, 3), Some(at(1_000)));
    assert_eq!(times.time_at(&[cell("untimed")], 0), None);
}

#[test]
fn test_apps_record_no_times() {
    let mut times = HistoryTimes::disabled();
    let live = cell("live");
    times.record_inserted(&live);

    assert_eq!(times.time_at(&[live], 0), None);
}

#[test]
fn replacement_cells_inherit_their_time() {
    let mut times = HistoryTimes::default();
    let streamed = cell("streamed");
    let consolidated = cell("consolidated");
    times.record(&streamed, at(2_000));
    times.inherit(&streamed, &consolidated);

    assert_eq!(times.time_at(&[consolidated], 0), Some(at(2_000)));
}

#[test]
fn dropped_cells_are_swept_without_losing_live_times() {
    let mut times = HistoryTimes::default();
    let kept = cell("kept");
    times.record(&kept, at(3_000));
    for _ in 0..MIN_PRUNE_LEN * 3 {
        times.record(&cell("dropped"), at(4_000));
    }

    assert!(times.times.len() <= MIN_PRUNE_LEN * 2);
    assert_eq!(times.time_at(&[kept], 0), Some(at(3_000)));
}
