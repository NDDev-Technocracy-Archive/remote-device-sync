//! Explicit native X11 sample; run only in an owned, static Xvfb display.
use super::*;
use std::{collections::BTreeMap, fs};

fn counters() -> (u64, u64, u64) {
    let text = fs::read_to_string("/proc/thread-self/stat").unwrap();
    let fields = text
        .rsplit_once(") ")
        .unwrap()
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let cpu = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    let text = fs::read_to_string("/proc/thread-self/status").unwrap();
    let switches = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.ends_with("ctxt_switches")
                .then(|| (key, value.trim().parse::<u64>().unwrap()))
        })
        .collect::<BTreeMap<_, _>>();
    (
        cpu,
        switches["voluntary_ctxt_switches"],
        switches["nonvoluntary_ctxt_switches"],
    )
}

#[test]
#[ignore = "requires an owned static 640x480 Xvfb display; explicit 20-second native resource sample"]
fn native_static_capture_resource_sample() {
    assert!(
        !crate::wayland_session(),
        "the fixture must own a real X11 test server"
    );
    let mut producer =
        x11::X11Producer::new(0, Duration::from_millis(33), Some(0), Some((640, 480))).unwrap();
    let controls = ProducerControls::new(4_000_000);
    let clock = SessionClock::default();
    assert!(producer.produce(0, &controls, &clock).is_some());
    let before = counters();
    let started = Instant::now();
    let mut encoded = 0;
    for seq in 1..=20 {
        let frame = producer.produce(seq, &controls, &clock).unwrap();
        assert_eq!((frame.header.width, frame.header.height), (640, 480));
        encoded += usize::from(!frame.payload.is_empty());
    }
    let elapsed = started.elapsed().as_millis();
    let after = counters();
    assert!(
        (19_000..=25_000).contains(&elapsed),
        "native unchanged-picture cadence changed: {elapsed} ms"
    );
    println!(
        "native_idle_sample elapsed_ms={elapsed} calls=20 encoded={encoded} thread_cpu_ticks={} voluntary_switches={} involuntary_switches={}",
        after.0 - before.0,
        after.1 - before.1,
        after.2 - before.2
    );
}
