use super::*;
use pretty_assertions::assert_eq;

const DELIBERATE: Duration = Duration::from_millis(/*millis*/ 250);

fn steps(flick: &mut Flick, start: Instant, gap: Duration, upward: bool, notches: u32) -> Vec<f64> {
    (0..notches)
        .map(|index| flick.notch(upward, start + gap * index, /*max_multiplier*/ 100.0))
        .collect()
}

#[test]
fn bursts_grow_after_a_few_precise_notches() {
    let mut flick = Flick::default();
    assert_eq!(
        steps(
            &mut flick,
            Instant::now(),
            Duration::ZERO,
            /*upward*/ true,
            /*notches*/ 6
        ),
        [1.0, 1.0, 1.0, 1.5, 2.0, 2.5]
    );
}

#[test]
fn deliberate_notches_stay_precise() {
    let mut flick = Flick::default();
    let steps = steps(
        &mut flick,
        Instant::now(),
        DELIBERATE,
        /*upward*/ true,
        /*notches*/ 20,
    );
    assert_eq!(steps, vec![1.0; 20]);
}

#[test]
fn a_lag_spike_slows_a_flick_without_resetting_it() {
    let mut flick = Flick::default();
    let start = Instant::now();
    let burst = steps(
        &mut flick,
        start,
        Duration::ZERO,
        /*upward*/ true,
        /*notches*/ 10,
    );
    let after_stall = flick.notch(
        /*upward*/ true,
        start + Duration::from_millis(/*millis*/ 200),
        /*max_multiplier*/ 100.0,
    );
    assert!(
        1.0 < after_stall && after_stall < burst[9],
        "{after_stall} after {burst:?}"
    );
}

#[test]
fn reversals_start_over() {
    let mut flick = Flick::default();
    let start = Instant::now();
    steps(
        &mut flick,
        start,
        Duration::ZERO,
        /*upward*/ true,
        /*notches*/ 10,
    );
    assert_eq!(
        flick.notch(/*upward*/ false, start, /*max_multiplier*/ 100.0),
        1.0
    );
}

#[test]
fn one_notch_is_bounded() {
    let mut flick = Flick::default();
    let start = Instant::now();
    steps(
        &mut flick,
        start,
        Duration::ZERO,
        /*upward*/ true,
        /*notches*/ 40,
    );
    assert_eq!(
        flick.notch(/*upward*/ true, start, /*max_multiplier*/ 12.0),
        12.0
    );
}
