use mineru::ProgressEvent::*;
use mineru::command::plain::{EventSink, LogLevel};
use std::{
    ffi::OsString,
    io::Write,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);
impl Write for Buffer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}
fn sink(tty: bool, level: LogLevel) -> (Arc<EventSink<Buffer>>, Buffer) {
    let b = Buffer::default();
    (Arc::new(EventSink::new(b.clone(), tty, level)), b)
}

#[test]
fn non_tty_all_events_snapshot() {
    let (s, b) = sink(false, LogLevel::Info);
    for e in [
        ServerStarted {
            address: "a".into(),
        },
        ServerStopped,
        RequestAccepted { label: "r".into() },
        RequestRejected {
            message: "x".into(),
        },
        RequestCompleted { label: "r".into() },
        RequestFailed {
            label: "r".into(),
            message: "x".into(),
        },
        DocumentStarted {
            document: "d".into(),
        },
        DocumentPrepared {
            document: "d".into(),
        },
        DocumentPageCompleted {
            document: "d".into(),
            page_index: 2,
            completed: 3,
            total: 4,
        },
        DocumentCompleted {
            document: "d".into(),
        },
        DocumentFailed {
            document: "d".into(),
            message: "x".into(),
        },
        OfficeWarning {
            document: "d".into(),
            message: "x".into(),
        },
        ApiSubmitted { label: "a".into() },
        ApiPending {
            label: "a".into(),
            queued_ahead: Some(2),
        },
        ApiPending {
            label: "a".into(),
            queued_ahead: None,
        },
        ApiProcessing { label: "a".into() },
        ApiDownloading { label: "a".into() },
        ApiExtracting { label: "a".into() },
        ApiWarning {
            label: "a".into(),
            message: "x".into(),
        },
        ApiCompleted { label: "a".into() },
        ApiFailed {
            label: "a".into(),
            message: "x".into(),
        },
    ] {
        s.event(e);
    }
    s.warning("route-env", "x");
    s.fail("x");
    s.finish();
    assert_eq!(
        b.text(),
        "server started: a\nserver stopped: server\nrequest accepted: r\nrequest rejected: x\nrequest completed: r\nrequest failed: r: x\ndocument started: d\ndocument prepared: d\ndocument page completed: d: page=2 completed=3/4\ndocument completed: d\ndocument failed: d: x\noffice warning: d: x\napi submitted: a\napi pending: a: queued-ahead=2\napi pending: a: queued-ahead=none\napi processing: a\napi downloading: a\napi extracting: a\napi warning: a: x\napi completed: a\napi failed: a: x\nwarning: route-env: x\nfailed: x\n"
    );
}

#[test]
fn tty_retains_warnings_and_finishes_once() {
    let (s, b) = sink(true, LogLevel::Info);
    s.warning_callback()("office", "bad");
    s.event(DocumentCompleted {
        document: "d".into(),
    });
    s.fail("no");
    s.finish();
    s.finish();
    s.event(ServerStopped);
    assert_eq!(
        b.text(),
        "\rwarning: office: bad | warnings=1 last-warning=bad\x1b[K\rdocument completed: d | warnings=1 last-warning=bad\x1b[K\rfailed: no | warnings=1 last-warning=bad\x1b[K\n"
    );
}

#[test]
fn levels_filter_before_state() {
    let (s, b) = sink(true, LogLevel::Success);
    s.event(DocumentStarted {
        document: "d".into(),
    });
    s.warning("w", "x");
    s.fail("e");
    s.finish();
    assert_eq!(
        b.text(),
        "\rwarning: w: x | warnings=1 last-warning=x\x1b[K\rfailed: e | warnings=1 last-warning=x\x1b[K\n"
    );
    let (s, b) = sink(true, LogLevel::Critical);
    s.fail("e");
    s.finish();
    assert_eq!(b.text(), "");
}

#[test]
fn parses_levels() {
    let _: fn() -> Result<LogLevel, &'static str> = LogLevel::from_env;
    for (text, level) in [
        ("trace", LogLevel::Trace),
        ("debug", LogLevel::Debug),
        ("info", LogLevel::Info),
        ("success", LogLevel::Success),
        ("warning", LogLevel::Warning),
        ("error", LogLevel::Error),
        ("critical", LogLevel::Critical),
    ] {
        assert_eq!(LogLevel::parse(Some(OsString::from(text))), Ok(level));
        assert_eq!(
            LogLevel::parse(Some(OsString::from(text.to_ascii_uppercase()))),
            Ok(level)
        );
    }
    assert_eq!(LogLevel::parse(None), Ok(LogLevel::Info));
    assert_eq!(
        LogLevel::parse(Some(OsString::from("wat"))),
        Err("invalid MINERU_LOG_LEVEL")
    );
    #[cfg(unix)]
    assert_eq!(
        LogLevel::parse(Some(std::os::unix::ffi::OsStringExt::from_vec(vec![255]))),
        Err("invalid MINERU_LOG_LEVEL")
    );
}

#[test]
fn sink_sanitizes() {
    let (s, b) = sink(false, LogLevel::Info);
    s.fail("Bearer secret https://example.test/a\r\n\t\0\u{1}€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€€");
    let text = b.text();
    let line = text.trim_end_matches('\n');
    assert!(
        !line.contains("secret")
            && !line.contains("example.test")
            && !line.chars().any(char::is_control)
            && text.len() <= 521
            && line.contains("\\r\\n\\t\\0\\u{1}")
    );
}

#[test]
fn callbacks_do_not_interleave() {
    let (s, b) = sink(false, LogLevel::Info);
    let cb = s.callback();
    let mut threads = Vec::new();
    for n in 0..20 {
        let cb = cb.clone();
        threads.push(std::thread::spawn(move || {
            cb(RequestAccepted {
                label: n.to_string(),
            })
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    let text = b.text();
    let mut lines: Vec<_> = text.lines().collect();
    lines.sort();
    assert_eq!(lines.len(), 20);
    for n in 0..20 {
        assert!(
            lines
                .iter()
                .any(|line| *line == format!("request accepted: {n}"))
        );
    }
}
