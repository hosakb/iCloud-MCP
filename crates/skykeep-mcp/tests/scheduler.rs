use skykeep_mcp::scheduler::Schedule;
use std::time::{Duration, Instant};
#[test]
fn failed_passes_wait_for_next_interval_without_busy_retry() {
    let start = Instant::now();
    let mut schedule = Schedule::new(start, Duration::from_secs(300));
    assert!(schedule.due(start));
    assert!(!schedule.due(start + Duration::from_secs(299)));
    assert!(schedule.due(start + Duration::from_secs(300)));
    assert!(!schedule.due(start + Duration::from_secs(300)));
    assert!(schedule.due(start + Duration::from_secs(1200)));
    assert!(!schedule.due(start + Duration::from_secs(1200)));
}
