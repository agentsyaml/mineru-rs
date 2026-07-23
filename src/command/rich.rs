use super::{CommandCallback, CommandEvent, CommandScope, plain};
use crate::{ProgressEvent, sanitize_event_text};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

const TEXT_CAP: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Policy {
    pub(super) rich: bool,
    pub(super) color: bool,
}

impl Policy {
    pub(super) fn select(stderr_tty: bool, env: &super::Environment) -> Self {
        let term_dumb = env
            .os("TERM")
            .and_then(|value| value.into_string().ok())
            .is_some_and(|value| value == "dumb");
        let no_color = env.os("NO_COLOR").is_some_and(|value| !value.is_empty());
        Self {
            rich: stderr_tty && !term_dumb && env.os("CI").is_none(),
            color: !no_color,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Terminal {
    Completed,
    Failed,
}

#[derive(Clone, Copy)]
enum Activity {
    Completed,
    Warning,
    Failed,
}

pub(super) struct Renderer {
    state: Mutex<State>,
    level: plain::LogLevel,
}

struct State {
    multi: MultiProgress,
    overview: ProgressBar,
    active: BTreeMap<CommandScope, ProgressBar>,
    activity: Option<ProgressBar>,
    terminal: BTreeMap<CommandScope, Terminal>,
    planned_documents: usize,
    planned_tasks: usize,
    completed: usize,
    failed: usize,
    warnings: usize,
    finished: bool,
    color: bool,
    steady: bool,
    show_progress: bool,
}

impl Renderer {
    pub(super) fn stderr(level: plain::LogLevel, color: bool) -> Arc<Self> {
        Self::new(ProgressDrawTarget::stderr_with_hz(20), level, color, true)
    }

    fn new(
        target: ProgressDrawTarget,
        level: plain::LogLevel,
        color: bool,
        steady: bool,
    ) -> Arc<Self> {
        let multi = MultiProgress::with_draw_target(target);
        let show_progress = level.admits(plain::Severity::Info);
        let overview = if show_progress {
            let overview = multi.add(ProgressBar::new_spinner());
            overview.set_style(spinner_style(color));
            overview.set_prefix("MinerU");
            overview.set_message("Preparing parsing plan");
            if steady {
                overview.enable_steady_tick(Duration::from_millis(100));
            }
            overview
        } else {
            ProgressBar::hidden()
        };
        Arc::new(Self {
            state: Mutex::new(State {
                multi,
                overview,
                active: BTreeMap::new(),
                activity: None,
                terminal: BTreeMap::new(),
                planned_documents: 0,
                planned_tasks: 0,
                completed: 0,
                failed: 0,
                warnings: 0,
                finished: false,
                color,
                steady,
                show_progress,
            }),
            level,
        })
    }

    pub(super) fn callback(self: &Arc<Self>) -> CommandCallback {
        let renderer = Arc::clone(self);
        Arc::new(move |event| renderer.handle(event))
    }

    pub(super) fn warning_callback(self: &Arc<Self>) -> super::direct::WarningCallback {
        let renderer = Arc::clone(self);
        Arc::new(move |source, message| renderer.warning(source, message))
    }

    pub(super) fn handle(&self, event: CommandEvent) {
        let mut state = self.lock();
        if state.finished {
            return;
        }
        match event {
            CommandEvent::RunPlanned {
                documents,
                api_tasks,
            } => {
                state.planned_documents = documents;
                state.planned_tasks = api_tasks;
                plan_overview(&state);
                update_overview(&state);
            }
            CommandEvent::Progress { scope, event } => {
                let visible = self.level.admits(plain::event_severity(&event));
                handle_progress(&mut state, scope, event, visible);
            }
            CommandEvent::RunCompleted => {
                finish(&mut state, true, self.level.admits(plain::Severity::Info))
            }
            CommandEvent::RunFailed { .. } => {
                finish(&mut state, false, self.level.admits(plain::Severity::Error))
            }
        }
    }

    pub(super) fn warning(&self, source: &str, message: &str) {
        if !self.level.admits(plain::Severity::Warning) {
            return;
        }
        let mut state = self.lock();
        if state.finished {
            return;
        }
        state.warnings = state.warnings.saturating_add(1);
        print_status(
            &state,
            "! Warning",
            "1;33",
            &format!("{} · {}", clean(source), clean(message)),
        );
        update_activity(&mut state, Activity::Warning, &clean(source));
        update_overview(&state);
    }

    pub(super) fn fail(&self, message: &str) {
        if !self.level.admits(plain::Severity::Error) {
            return;
        }
        let state = self.lock();
        if !state.finished {
            print_status(&state, "✗ Failed", "1;31", &clean(message));
        }
    }

    pub(super) fn finish(&self) {
        let mut state = self.lock();
        cleanup(&mut state);
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cleanup(state);
    }
}

fn handle_progress(state: &mut State, scope: CommandScope, event: ProgressEvent, visible: bool) {
    if state.terminal.contains_key(&scope) {
        return;
    }
    match event {
        ProgressEvent::DocumentCompleted { document } => {
            terminal(state, scope, Terminal::Completed, &document, None);
        }
        ProgressEvent::ApiCompleted { label } => {
            terminal(state, scope, Terminal::Completed, &label, None);
        }
        ProgressEvent::DocumentFailed { document, message } => {
            terminal(
                state,
                scope,
                Terminal::Failed,
                &document,
                visible.then_some(message.as_str()),
            );
        }
        ProgressEvent::ApiFailed { label, message } => {
            terminal(
                state,
                scope,
                Terminal::Failed,
                &label,
                visible.then_some(message.as_str()),
            );
        }
        ProgressEvent::OfficeWarning { document, message } => {
            if !visible {
                return;
            }
            state.warnings = state.warnings.saturating_add(1);
            print_status(
                state,
                "! Warning",
                "1;33",
                &format!("{} · {}", clean(&document), clean(&message)),
            );
            update_activity(state, Activity::Warning, &clean(&document));
            if state.show_progress {
                update_item(state, scope, &document, "warning", None);
            }
            update_overview(state);
        }
        ProgressEvent::ApiWarning { label, message } => {
            if !visible {
                return;
            }
            state.warnings = state.warnings.saturating_add(1);
            print_status(
                state,
                "! Warning",
                "1;33",
                &format!("{} · {}", clean(&label), clean(&message)),
            );
            update_activity(state, Activity::Warning, &clean(&label));
            if state.show_progress {
                update_item(state, scope, &label, "warning", None);
            }
            update_overview(state);
        }
        ProgressEvent::DocumentStarted { document } => {
            if visible {
                update_item(state, scope, &document, "starting", None)
            }
        }
        ProgressEvent::DocumentPrepared { document } => {
            if visible {
                update_item(state, scope, &document, "prepared · parsing", None)
            }
        }
        ProgressEvent::DocumentPageCompleted {
            document,
            completed,
            total,
            ..
        } => {
            if visible {
                update_item(
                    state,
                    scope,
                    &document,
                    "parsing pages",
                    Some((completed, total)),
                )
            }
        }
        ProgressEvent::ApiSubmitted { label } => {
            if visible {
                update_item(state, scope, &label, "submitted", None)
            }
        }
        ProgressEvent::ApiPending {
            label,
            queued_ahead,
        } => {
            if visible {
                let stage = match queued_ahead {
                    Some(count) => format!("waiting · {count} ahead"),
                    None => "waiting".into(),
                };
                update_item(state, scope, &label, &stage, None)
            }
        }
        ProgressEvent::ApiProcessing { label } => {
            if visible {
                update_item(state, scope, &label, "processing", None)
            }
        }
        ProgressEvent::ApiDownloading { label } => {
            if visible {
                update_item(state, scope, &label, "downloading results", None)
            }
        }
        ProgressEvent::ApiExtracting { label } => {
            if visible {
                update_item(state, scope, &label, "extracting output", None)
            }
        }
        _ => {}
    }
}

fn update_item(
    state: &mut State,
    scope: CommandScope,
    label: &str,
    stage: &str,
    pages: Option<(usize, usize)>,
) {
    if !state.active.contains_key(&scope) {
        let position = 1 + state
            .active
            .keys()
            .filter(|existing| **existing < scope)
            .count();
        let bar = state.multi.insert(position, ProgressBar::new_spinner());
        bar.set_style(spinner_style(state.color));
        bar.set_prefix(scope_kind(scope));
        if state.steady {
            bar.enable_steady_tick(Duration::from_millis(100));
        }
        state.active.insert(scope, bar);
    }
    let bar = state.active.get(&scope).expect("active item inserted");
    if let Some((completed, total)) = pages {
        bar.set_style(bar_style(state.color));
        bar.set_prefix(clean(label));
        bar.set_length(total as u64);
        bar.set_position(completed.min(total) as u64);
    }
    bar.set_message(format!("{} · {stage}", clean(label)));
}

fn terminal(
    state: &mut State,
    scope: CommandScope,
    result: Terminal,
    label: &str,
    message: Option<&str>,
) {
    if state.terminal.contains_key(&scope) {
        return;
    }
    state.terminal.insert(scope, result);
    if let Some(bar) = state.active.remove(&scope) {
        bar.disable_steady_tick();
        bar.finish_and_clear();
        state.multi.remove(&bar);
    }
    match result {
        Terminal::Completed => {
            state.completed = state.completed.saturating_add(1);
            update_activity(
                state,
                Activity::Completed,
                &format!("{} · {}", scope_kind(scope), clean(label)),
            );
        }
        Terminal::Failed => {
            state.failed = state.failed.saturating_add(1);
            if let Some(message) = message {
                print_status(
                    state,
                    "✗ Failed",
                    "1;31",
                    &format!("{} · {}", clean(label), clean(message)),
                );
            }
            update_activity(
                state,
                Activity::Failed,
                &format!("{} · {}", scope_kind(scope), clean(label)),
            );
        }
    }
    update_overview(state);
}

fn plan_overview(state: &State) {
    if !state.show_progress {
        return;
    }
    let total = state.planned_documents.saturating_add(state.planned_tasks);
    if total == 0 {
        state.overview.set_message("No parsing work planned");
        return;
    }
    state.overview.disable_steady_tick();
    state.overview.set_style(overall_style(state.color));
    state.overview.set_length(total as u64);
    state
        .overview
        .set_prefix(match (state.planned_documents, state.planned_tasks) {
            (documents, 0) => format!(
                "MinerU · {documents} doc{}",
                if documents == 1 { "" } else { "s" }
            ),
            (0, tasks) => format!("MinerU · {tasks} task{}", if tasks == 1 { "" } else { "s" }),
            _ => format!("MinerU · {total} items"),
        });
}

fn update_overview(state: &State) {
    if !state.show_progress {
        return;
    }
    let total = state.planned_documents.saturating_add(state.planned_tasks);
    if total == 0 {
        return;
    }
    state
        .overview
        .set_position(state.completed.saturating_add(state.failed).min(total) as u64);
}

fn finish(state: &mut State, success: bool, visible: bool) {
    if state.finished {
        return;
    }
    cleanup_active(state);
    clear_overview(state);
    let _ = state.multi.clear();
    if visible {
        print_status(
            state,
            if success {
                "✓ Parsing complete"
            } else {
                "✗ Parsing failed"
            },
            if success { "1;32" } else { "1;31" },
            &format!(
                "{} completed · {} failed · {} warning{}",
                state.completed,
                state.failed,
                state.warnings,
                if state.warnings == 1 { "" } else { "s" }
            ),
        );
    }
    state.finished = true;
}

fn cleanup(state: &mut State) {
    if state.finished {
        return;
    }
    cleanup_active(state);
    clear_overview(state);
    let _ = state.multi.clear();
    state.finished = true;
}

fn cleanup_active(state: &mut State) {
    for (_, bar) in std::mem::take(&mut state.active) {
        bar.disable_steady_tick();
        bar.finish_and_clear();
        state.multi.remove(&bar);
    }
    if let Some(activity) = state.activity.take() {
        activity.finish_and_clear();
        state.multi.remove(&activity);
    }
}

fn clear_overview(state: &State) {
    if state.show_progress {
        state.overview.disable_steady_tick();
        state.overview.finish_and_clear();
        state.multi.remove(&state.overview);
    }
}

fn spinner_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{spinner:.cyan} {prefix:.bold}  {elapsed_precise}  {wide_msg}"
    } else {
        "{spinner} {prefix}  {elapsed_precise}  {wide_msg}"
    })
    .unwrap()
    .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
}

fn bar_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{prefix:.bold}  [{wide_bar:.cyan/blue}] {pos}/{len} {percent:>3}%"
    } else {
        "{prefix}  [{wide_bar}] {pos}/{len} {percent:>3}%"
    })
    .unwrap()
    .progress_chars("█▓░")
}

fn overall_style(color: bool) -> ProgressStyle {
    ProgressStyle::with_template(if color {
        "{prefix:.bold.cyan} {wide_bar:.cyan/blue} {pos}/{len} {percent:>3}%"
    } else {
        "{prefix} {wide_bar} {pos}/{len} {percent:>3}%"
    })
    .unwrap()
    .progress_chars("━╸─")
}

fn scope_kind(scope: CommandScope) -> &'static str {
    match scope {
        CommandScope::Document(_) => "Document",
        CommandScope::ApiTask(_) => "API",
    }
}

fn update_activity(state: &mut State, activity: Activity, message: &str) {
    if !state.show_progress {
        return;
    }
    let bar = state.activity.get_or_insert_with(|| {
        let bar = state.multi.add(ProgressBar::new(0));
        bar.set_style(activity_style(activity, state.color));
        bar
    });
    bar.set_style(activity_style(activity, state.color));
    bar.set_prefix(match activity {
        Activity::Completed => "✓ Completed",
        Activity::Warning => "! Warning",
        Activity::Failed => "✗ Failed",
    });
    bar.set_message(message.to_owned());
}

fn activity_style(activity: Activity, color: bool) -> ProgressStyle {
    let template = match (activity, color) {
        (Activity::Completed, true) => "  {prefix:.bold.green}  {wide_msg}",
        (Activity::Warning, true) => "  {prefix:.bold.yellow}  {wide_msg}",
        (Activity::Failed, true) => "  {prefix:.bold.red}  {wide_msg}",
        _ => "  {prefix}  {wide_msg}",
    };
    ProgressStyle::with_template(template).unwrap()
}

fn print_status(state: &State, status: &str, ansi: &str, message: &str) {
    let status = if state.color {
        format!("\x1b[{ansi}m{status}\x1b[0m")
    } else {
        status.to_owned()
    };
    let _ = state.multi.println(format!("{status}  {message}"));
}

fn clean(value: &str) -> String {
    sanitize_event_text(value, TEXT_CAP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::{ApiTaskId, CommandEvent, DocumentId};
    use indicatif::TermLike;
    use std::{
        ffi::OsString,
        fmt, io,
        sync::{Arc, Mutex},
    };

    #[derive(Default)]
    struct Screen {
        lines: Vec<String>,
        cursor: usize,
        history: Vec<String>,
        max_rows: usize,
        clears: usize,
    }

    #[derive(Clone)]
    struct TestTerm(Arc<Mutex<Screen>>, u16);

    impl Default for TestTerm {
        fn default() -> Self {
            Self(Arc::default(), 32)
        }
    }

    impl fmt::Debug for TestTerm {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("TestTerm")
        }
    }

    impl TestTerm {
        fn with_width(width: u16) -> Self {
            Self(Arc::default(), width)
        }

        fn update(screen: &mut Screen) {
            screen.max_rows = screen
                .max_rows
                .max(screen.lines.iter().filter(|line| !line.is_empty()).count());
        }

        fn text(&self) -> String {
            let screen = self.0.lock().unwrap();
            screen
                .history
                .iter()
                .chain(screen.lines.iter())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    impl TermLike for TestTerm {
        fn width(&self) -> u16 {
            self.1
        }

        fn height(&self) -> u16 {
            8
        }

        fn move_cursor_up(&self, n: usize) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            screen.cursor = screen.cursor.saturating_sub(n);
            Ok(())
        }

        fn move_cursor_down(&self, n: usize) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            screen.cursor = screen.cursor.saturating_add(n);
            Ok(())
        }

        fn move_cursor_right(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn move_cursor_left(&self, _n: usize) -> io::Result<()> {
            Ok(())
        }

        fn write_line(&self, value: &str) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            let cursor = screen.cursor;
            if screen.lines.len() <= cursor {
                screen.lines.resize(cursor + 1, String::new());
            }
            screen.lines[cursor].push_str(value);
            screen.history.push(value.to_owned());
            screen.cursor += 1;
            Self::update(&mut screen);
            Ok(())
        }

        fn write_str(&self, value: &str) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            let cursor = screen.cursor;
            if screen.lines.len() <= cursor {
                screen.lines.resize(cursor + 1, String::new());
            }
            screen.lines[cursor].push_str(value);
            screen.history.push(value.to_owned());
            Self::update(&mut screen);
            Ok(())
        }

        fn clear_line(&self) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            let cursor = screen.cursor;
            if screen.lines.len() <= cursor {
                screen.lines.resize(cursor + 1, String::new());
            }
            screen.lines[cursor].clear();
            screen.clears += 1;
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    fn renderer(term: TestTerm) -> Arc<Renderer> {
        Renderer::new(
            ProgressDrawTarget::term_like(Box::new(term)),
            plain::LogLevel::Info,
            false,
            false,
        )
    }

    #[test]
    fn state_machine_is_bounded_sanitized_and_terminal_is_irreversible() {
        let term = TestTerm::default();
        let renderer = renderer(term.clone());
        renderer.handle(CommandEvent::RunPlanned {
            documents: 2,
            api_tasks: 0,
        });
        for (id, label) in [(1, "one\x1b[31m"), (2, "two")] {
            renderer.handle(CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(id)),
                event: ProgressEvent::DocumentStarted {
                    document: label.into(),
                },
            });
        }
        renderer.handle(CommandEvent::Progress {
            scope: CommandScope::Document(DocumentId(1)),
            event: ProgressEvent::DocumentPageCompleted {
                document: "one".into(),
                page_index: 1,
                completed: 1,
                total: 2,
            },
        });
        renderer.handle(CommandEvent::Progress {
            scope: CommandScope::Document(DocumentId(1)),
            event: ProgressEvent::DocumentFailed {
                document: "one".into(),
                message: "bad\x1b[2J".into(),
            },
        });
        renderer.handle(CommandEvent::Progress {
            scope: CommandScope::Document(DocumentId(1)),
            event: ProgressEvent::DocumentCompleted {
                document: "one".into(),
            },
        });
        for _ in 0..2 {
            renderer.handle(CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(2)),
                event: ProgressEvent::DocumentCompleted {
                    document: "two".into(),
                },
            });
        }
        renderer.handle(CommandEvent::RunFailed {
            message: "overall".into(),
        });
        renderer.handle(CommandEvent::RunCompleted);

        let state = renderer.lock();
        assert_eq!((state.completed, state.failed), (1, 1));
        assert!(state.active.is_empty() && state.finished);
        drop(state);
        let text = term.text();
        assert!(!text.contains('\x1b'));
        assert!(!text.contains("\x1b[31m") && !text.contains("\x1b[2J"));
        assert!(
            text.contains("\\u{1B}") && text.contains("Parsing failed"),
            "{text}"
        );
        assert!(!text.contains("run completed"));
        assert!(term.0.lock().unwrap().max_rows <= 4);
    }

    #[test]
    fn renders_determinate_document_progress_and_success() {
        let term = TestTerm::with_width(80);
        let renderer = renderer(term.clone());
        renderer.handle(CommandEvent::RunPlanned {
            documents: 1,
            api_tasks: 0,
        });
        for event in [
            ProgressEvent::DocumentStarted {
                document: "report.pdf".into(),
            },
            ProgressEvent::DocumentPrepared {
                document: "report.pdf".into(),
            },
            ProgressEvent::DocumentPageCompleted {
                document: "report.pdf".into(),
                page_index: 0,
                completed: 1,
                total: 2,
            },
        ] {
            renderer.handle(CommandEvent::Progress {
                scope: CommandScope::Document(DocumentId(1)),
                event,
            });
        }
        let progress = term.text();
        for expected in [
            "MinerU · 1 doc",
            "report.pdf · starting",
            "prepared · parsing",
            "1/2",
            "50%",
            "█",
        ] {
            assert!(
                progress.contains(expected),
                "missing {expected:?}: {progress}"
            );
        }

        renderer.handle(CommandEvent::Progress {
            scope: CommandScope::Document(DocumentId(1)),
            event: ProgressEvent::DocumentCompleted {
                document: "report.pdf".into(),
            },
        });
        renderer.handle(CommandEvent::RunCompleted);
        let final_output = term.text();
        assert!(final_output.contains("✓ Completed  Document · report.pdf"));
        assert!(final_output.contains("1/1 100%"));
        assert!(final_output.contains("✓ Parsing complete  1 completed · 0 failed · 0 warnings"));
    }

    #[test]
    fn api_phases_and_warning_remain_informative() {
        let term = TestTerm::with_width(96);
        let renderer = renderer(term.clone());
        let scope = CommandScope::ApiTask(ApiTaskId(1));
        renderer.handle(CommandEvent::RunPlanned {
            documents: 0,
            api_tasks: 1,
        });
        for event in [
            ProgressEvent::ApiSubmitted {
                label: "task#1 [quarterly-report]".into(),
            },
            ProgressEvent::ApiPending {
                label: "task#1 [quarterly-report]".into(),
                queued_ahead: Some(3),
            },
            ProgressEvent::ApiProcessing {
                label: "task#1 [quarterly-report]".into(),
            },
            ProgressEvent::ApiDownloading {
                label: "task#1 [quarterly-report]".into(),
            },
            ProgressEvent::ApiExtracting {
                label: "task#1 [quarterly-report]".into(),
            },
            ProgressEvent::ApiWarning {
                label: "task#1 [quarterly-report]".into(),
                message: "preview unavailable".into(),
            },
        ] {
            renderer.handle(CommandEvent::Progress { scope, event });
        }
        let text = term.text();
        for expected in [
            "MinerU · 1 task",
            "submitted",
            "waiting · 3 ahead",
            "processing",
            "downloading results",
            "extracting output",
            "! Warning  task#1 [quarterly-report] · preview unavailable",
        ] {
            assert!(text.contains(expected), "missing {expected:?}: {text}");
        }
    }

    #[test]
    fn narrow_live_rows_are_clipped_instead_of_wrapped() {
        let term = TestTerm::with_width(24);
        let renderer = renderer(term.clone());
        renderer.handle(CommandEvent::RunPlanned {
            documents: 1,
            api_tasks: 0,
        });
        renderer.handle(CommandEvent::Progress {
            scope: CommandScope::Document(DocumentId(1)),
            event: ProgressEvent::DocumentStarted {
                document: "a-very-long-document-name.pdf".into(),
            },
        });
        let text = term.text();
        assert!(
            !text.contains("a-very-long-document-name.pdf"),
            "the narrow live row should be clipped rather than wrapped: {text}"
        );
        let screen = term.0.lock().unwrap();
        assert!(
            screen
                .history
                .iter()
                .flat_map(|write| write.lines())
                .all(|line| { line.chars().count() <= usize::from(term.1) })
        );
    }

    #[test]
    fn finish_and_drop_clear_all_active_rows() {
        let term = TestTerm::default();
        {
            let renderer = renderer(term.clone());
            renderer.handle(CommandEvent::RunPlanned {
                documents: 0,
                api_tasks: 1,
            });
            renderer.handle(CommandEvent::Progress {
                scope: CommandScope::ApiTask(ApiTaskId(1)),
                event: ProgressEvent::ApiSubmitted {
                    label: "task".into(),
                },
            });
        }
        let screen = term.0.lock().unwrap();
        assert!(screen.clears > 0);
        assert!(screen.lines.iter().all(String::is_empty));
    }

    #[test]
    fn policy_uses_tty_term_ci_and_no_color_independently() {
        fn env(values: &[(&'static str, &str)]) -> super::super::Environment {
            super::super::Environment(Arc::new(
                values
                    .iter()
                    .map(|&(name, value)| (name, OsString::from(value)))
                    .collect(),
            ))
        }
        assert_eq!(
            Policy::select(true, &env(&[])),
            Policy {
                rich: true,
                color: true
            }
        );
        assert!(!Policy::select(false, &env(&[])).rich);
        assert!(!Policy::select(true, &env(&[("TERM", "dumb")])).rich);
        assert!(!Policy::select(true, &env(&[("CI", "0")])).rich);
        assert_eq!(
            Policy::select(true, &env(&[("NO_COLOR", "1")])),
            Policy {
                rich: true,
                color: false
            }
        );
    }
}
