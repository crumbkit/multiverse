use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{
    LineSink, LogLine, LogNote, LogRequest, LogStream, MOST_TAIL_BYTES, log_path, logs, tail,
};
use crate::error::{Operation, PhaseError};
use crate::record::process::{Processes, System};
use crate::record::run::{EntryState, RunRecord};
use crate::record::store::RunStore;
use crate::record::testing::{
    FakeProcesses, Spawned, app, empty_record, host_at, process, workspace, write_file,
};

/// A sink that keeps each line, and breaks once it holds `most` of them.
struct Kept {
    /// The stream and text of each line.
    lines: Vec<(Option<LogStream>, String)>,
    /// The lines after which the sink breaks.
    most: usize,
    /// The notes that the sink received.
    notes: Vec<LogNote>,
}

impl Kept {
    fn new(most: usize) -> Self {
        Self {
            lines: Vec::new(),
            most,
            notes: Vec::new(),
        }
    }

    fn texts(&self) -> Vec<&str> {
        self.lines.iter().map(|(_, text)| text.as_str()).collect()
    }
}

impl LineSink for Kept {
    fn line(&mut self, line: &LogLine<'_>) -> ControlFlow<()> {
        self.lines.push((line.stream, line.text.to_owned()));
        if self.lines.len() >= self.most {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }

    fn note(&mut self, note: LogNote) {
        self.notes.push(note);
    }
}

fn server_log(state: &Path) -> PathBuf {
    log_path(state, &workspace(), &app("api"), &process("server"))
}

fn write_log(state: &Path, text: &str) {
    let path = server_log(state);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_file(&path, text);
}

fn write_record(state: &Path, record: &RunRecord) {
    std::fs::create_dir_all(state.join("runs")).unwrap();
    write_file(
        &RunStore::new(state).path(&workspace()),
        &serde_json::to_string(record).unwrap(),
    );
}

/// Returns a record whose `api/server` process runs as `host`.
fn running(host: crate::record::run::HostProcess) -> RunRecord {
    let mut record = empty_record().host_process(&app("api"), &process("server"));
    record.processes[0].state = EntryState::Running;
    record.processes[0].host = Some(host);
    record
}

fn request(follow: bool) -> LogRequest {
    let mut request = LogRequest::new(app("api"), process("server"));
    request.follow = follow;
    request
}

fn read(
    state: &Path,
    request: &LogRequest,
    sink: &mut Kept,
    processes: &dyn Processes,
) -> Result<(), PhaseError> {
    logs(
        &RunStore::new(state),
        state,
        &workspace(),
        request,
        sink,
        processes,
    )
}

#[test]
fn a_log_lives_under_the_state_directory_by_workspace_app_and_process() {
    assert_eq!(
        server_log(Path::new("/state")),
        Path::new("/state/logs")
            .join(workspace().as_str())
            .join("api.server.log")
    );
}

#[test]
fn a_tail_returns_the_last_lines_and_drops_a_line_cut_by_its_bound() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.log");
    write_file(&path, "one\ntwo\nthree\n");
    assert_eq!(tail(&path, 2), ["two", "three"]);
    assert_eq!(tail(&path, 10), ["one", "two", "three"]);
    assert!(tail(&dir.path().join("missing.log"), 2).is_empty());

    let long = "x".repeat(usize::try_from(MOST_TAIL_BYTES).unwrap());
    write_file(&path, &format!("{long}\nlast\n"));
    assert_eq!(tail(&path, 10), ["last"]);
    // A read whose bound falls just after a newline starts a whole line,
    // which it keeps.
    let whole = "y".repeat(usize::try_from(MOST_TAIL_BYTES).unwrap() - "\nlast\n".len());
    write_file(&path, &format!("first\n{whole}\nlast\n"));
    assert_eq!(tail(&path, 10), [whole.as_str(), "last"]);
}

#[test]
fn a_log_is_read_from_its_start_and_its_lines_name_no_stream() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "one\ntwo\nunfinished");
    let mut sink = Kept::new(100);

    read(
        state.path(),
        &request(false),
        &mut sink,
        &FakeProcesses::default(),
    )
    .unwrap();

    assert_eq!(sink.texts(), ["one", "two"]);
    assert!(sink.lines.iter().all(|(stream, _)| stream.is_none()));
}

#[test]
fn a_sink_that_breaks_ends_the_read() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "one\ntwo\nthree\n");
    let mut sink = Kept::new(1);

    read(
        state.path(),
        &request(true),
        &mut sink,
        &FakeProcesses::default(),
    )
    .unwrap();

    assert_eq!(sink.texts(), ["one"]);
}

#[test]
fn a_process_with_neither_a_log_nor_a_record_entry_is_unknown() {
    let state = tempfile::tempdir().unwrap();

    let error = read(
        state.path(),
        &request(false),
        &mut Kept::new(1),
        &FakeProcesses::default(),
    )
    .unwrap_err();

    assert!(matches!(error, PhaseError::UnknownTarget { target } if target == "api/server"));
}

#[test]
fn a_recorded_process_without_a_log_yet_has_no_lines() {
    let state = tempfile::tempdir().unwrap();
    write_record(
        state.path(),
        &empty_record().host_process(&app("api"), &process("server")),
    );
    let mut sink = Kept::new(1);

    read(
        state.path(),
        &request(true),
        &mut sink,
        &FakeProcesses::default(),
    )
    .unwrap();

    assert!(sink.lines.is_empty());
}

#[test]
fn a_container_that_has_not_started_has_no_lines_and_a_since_is_not_built_yet() {
    let state = tempfile::tempdir().unwrap();
    write_record(
        state.path(),
        &empty_record().container(
            &app("api"),
            &process("server"),
            "api-server".parse().unwrap(),
        ),
    );
    let mut sink = Kept::new(1);
    read(
        state.path(),
        &request(false),
        &mut sink,
        &FakeProcesses::default(),
    )
    .unwrap();
    assert!(sink.lines.is_empty());
    assert_eq!(sink.notes, [LogNote::NotStarted]);

    let mut since = request(false);
    since.since = Some(Duration::from_secs(300));
    assert!(matches!(
        read(
            state.path(),
            &since,
            &mut Kept::new(1),
            &FakeProcesses::default()
        ),
        Err(PhaseError::NotBuilt {
            operation: Operation::Logs
        })
    ));
}

#[test]
fn a_request_for_the_last_lines_passes_only_those() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "one\ntwo\nthree\n");
    let mut sink = Kept::new(100);
    let mut last = request(false);
    last.lines = Some(2);

    read(state.path(), &last, &mut sink, &FakeProcesses::default()).unwrap();

    assert_eq!(sink.texts(), ["two", "three"]);
}

#[test]
fn a_followed_log_ends_once_its_process_has_gone_and_its_last_lines_are_read() {
    let state = tempfile::tempdir().unwrap();
    let log = server_log(state.path());
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    let mut writer = Spawned::shell(&format!(
        "echo first >> {log}; echo ready; sleep 0.3; echo last >> {log}; exit 0",
        log = log.display()
    ));
    write_record(state.path(), &running(writer.host()));
    let mut sink = Kept::new(100);

    let reading = std::thread::scope(|scope| {
        let reader = scope.spawn(|| read(state.path(), &request(true), &mut sink, &System));
        writer.wait_for_exit();
        reader.join().unwrap()
    });

    reading.unwrap();
    assert_eq!(sink.texts(), ["first", "last"]);
}

#[test]
fn a_followed_log_of_a_process_that_the_table_shows_as_gone_ends_at_once() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "only\n");
    write_record(state.path(), &running(host_at(4242, 2)));
    let mut sink = Kept::new(100);

    read(
        state.path(),
        &request(true),
        &mut sink,
        &FakeProcesses::default(),
    )
    .unwrap();

    assert_eq!(sink.texts(), ["only"]);
}

#[test]
fn a_followed_tail_passes_an_unfinished_last_line_once_it_is_finished_and_never_split() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "one\nab");
    let log = server_log(state.path());
    let mut writer = Spawned::shell(&format!(
        "echo ready; sleep 0.3; printf 'cd\\n' >> {log}; exit 0",
        log = log.display()
    ));
    write_record(state.path(), &running(writer.host()));
    let mut sink = Kept::new(100);
    let mut last = request(true);
    last.lines = Some(5);

    let reading = std::thread::scope(|scope| {
        let reader = scope.spawn(|| read(state.path(), &last, &mut sink, &System));
        writer.wait_for_exit();
        reader.join().unwrap()
    });

    reading.unwrap();
    assert_eq!(sink.texts(), ["one", "abcd"]);
}

#[test]
fn a_tail_without_follow_counts_an_unfinished_last_line_as_a_line() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "one\ntwo\nab");
    let mut sink = Kept::new(100);
    let mut last = request(false);
    last.lines = Some(2);

    read(state.path(), &last, &mut sink, &FakeProcesses::default()).unwrap();

    assert_eq!(sink.texts(), ["two", "ab"]);
}

#[test]
fn a_follow_from_no_last_lines_starts_after_an_unfinished_line_and_passes_it_whole() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "one\nab");
    let log = server_log(state.path());
    let mut writer = Spawned::shell(&format!(
        "echo ready; sleep 0.3; printf 'cd\\nthree\\n' >> {log}; exit 0",
        log = log.display()
    ));
    write_record(state.path(), &running(writer.host()));
    let mut sink = Kept::new(100);
    let mut none = request(true);
    none.lines = Some(0);

    let reading = std::thread::scope(|scope| {
        let reader = scope.spawn(|| read(state.path(), &none, &mut sink, &System));
        writer.wait_for_exit();
        reader.join().unwrap()
    });

    reading.unwrap();
    assert_eq!(sink.texts(), ["abcd", "three"]);
}

#[test]
fn no_last_lines_without_follow_passes_nothing_even_with_an_unfinished_line() {
    let state = tempfile::tempdir().unwrap();
    write_log(state.path(), "one\nab");
    let mut sink = Kept::new(100);
    let mut none = request(false);
    none.lines = Some(0);

    read(state.path(), &none, &mut sink, &FakeProcesses::default()).unwrap();

    assert!(sink.texts().is_empty());
    assert!(super::tail(&server_log(state.path()), 0).is_empty());
}

#[test]
fn a_container_that_phase_stopped_has_no_lines_and_a_note_that_says_so() {
    let state = tempfile::tempdir().unwrap();
    let mut record = empty_record().container(
        &app("api"),
        &process("server"),
        "api-server".parse().unwrap(),
    );
    record
        .set_state(
            &crate::record::run::ObjectKey::Container(app("api"), process("server")),
            EntryState::Stopped,
        )
        .unwrap();
    write_record(state.path(), &record);
    let mut sink = Kept::new(1);

    read(
        state.path(),
        &request(true),
        &mut sink,
        &FakeProcesses::default(),
    )
    .unwrap();

    assert!(sink.lines.is_empty());
    assert_eq!(sink.notes, [LogNote::Stopped]);
}

#[test]
fn each_note_has_a_message_that_names_no_value() {
    for note in [
        LogNote::ContainerGone,
        LogNote::NotStarted,
        LogNote::Stopped,
        LogNote::EntryUnusable,
    ] {
        assert!(!note.message().is_empty());
        assert_eq!(note.to_string(), note.message());
    }
}
