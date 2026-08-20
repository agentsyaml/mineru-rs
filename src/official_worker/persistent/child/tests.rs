use super::DiagnosticRing;
use super::replay::{PERSISTENT_RECENT_REQUEST_CAPACITY, RecentRequestIds};
use crate::official_worker::STDERR_CAP;

#[test]
fn diagnostic_ring_bounds_and_resets() {
    let mut ring = DiagnosticRing::new();
    ring.push(&vec![b'x'; STDERR_CAP + 1]);

    let diagnostic = ring.take();
    assert!(diagnostic.len() <= STDERR_CAP);
    assert!(diagnostic.ends_with(b" [truncated]"));
    assert!(ring.take().is_empty());

    ring.push(b"fresh diagnostic");
    assert_eq!(ring.take(), b"fresh diagnostic");
    assert!(ring.take().is_empty());
}

#[test]
fn recent_request_ids_reject_duplicates_within_window() {
    let mut recent = RecentRequestIds::new();
    assert!(recent.insert("request-1".into()));
    assert!(!recent.insert("request-1".into()));
    assert_eq!(recent.len(), 1);
}

#[test]
fn recent_request_ids_remain_bounded() {
    let mut recent = RecentRequestIds::new();
    for id in 0..(PERSISTENT_RECENT_REQUEST_CAPACITY + 10) {
        assert!(recent.insert(format!("request-{id}")));
        assert!(recent.len() <= PERSISTENT_RECENT_REQUEST_CAPACITY);
    }
    assert_eq!(recent.len(), PERSISTENT_RECENT_REQUEST_CAPACITY);
}

#[test]
fn recent_request_ids_release_evicted_ids() {
    let mut recent = RecentRequestIds::new();
    for id in 0..PERSISTENT_RECENT_REQUEST_CAPACITY {
        assert!(recent.insert(format!("request-{id}")));
    }
    assert!(recent.contains("request-0"));

    assert!(recent.insert("request-new".into()));
    assert!(!recent.contains("request-0"));
    assert!(recent.insert("request-0".into()));
}
