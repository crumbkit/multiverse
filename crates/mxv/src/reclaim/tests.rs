use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use multiverse::phase::host::TableEntry;

use super::{
    Facts, MIN_IDLE, Need, Verdict, choose, counted, idle, owned, parse_idle, parse_need, recheck,
    users, walks,
};

const HOUR: Duration = Duration::from_secs(60 * 60);

fn ago(now: SystemTime, hours: u32) -> Option<SystemTime> {
    Some(now - HOUR * hours)
}

fn facts(modified: Option<SystemTime>, touched: Option<SystemTime>) -> Facts {
    Facts {
        pinned: false,
        modified,
        touched,
        users: Vec::new(),
    }
}

fn reason(verdict: Verdict) -> String {
    match verdict {
        Verdict::InUse(reason) => reason,
        Verdict::Idle { why, .. } => panic!("idle: {why}"),
    }
}

#[test]
fn a_workspace_unchanged_and_untouched_for_the_idle_age_is_idle_since_its_last_activity() {
    let now = SystemTime::now();
    let verdict = idle(&facts(ago(now, 5), ago(now, 3)), now, 2 * HOUR);
    let Verdict::Idle { active, why } = verdict else {
        panic!("{verdict:?}");
    };
    assert_eq!(active, ago(now, 3));
    assert!(why.contains("unchanged for 5h"), "{why}");
    assert!(why.contains("no mxv command named it for 3h"), "{why}");
    assert!(
        why.contains("no process outside phase works in it"),
        "{why}"
    );
}

#[test]
fn a_workspace_that_never_changed_and_no_command_touched_is_idle() {
    let now = SystemTime::now();
    let verdict = idle(&facts(None, None), now, 2 * HOUR);
    assert!(
        matches!(verdict, Verdict::Idle { active: None, .. }),
        "{verdict:?}"
    );
}

#[test]
fn a_change_within_the_idle_age_keeps_a_workspace_in_use() {
    let now = SystemTime::now();
    let reason = reason(idle(&facts(ago(now, 1), ago(now, 5)), now, 2 * HOUR));
    assert!(reason.contains("its checkouts changed 1h ago"), "{reason}");
    assert!(reason.contains("idle age of 2h"), "{reason}");
}

#[test]
fn an_mxv_command_within_the_idle_age_keeps_a_workspace_in_use() {
    let now = SystemTime::now();
    let reason = reason(idle(&facts(ago(now, 5), ago(now, 1)), now, 2 * HOUR));
    assert!(
        reason.contains("an mxv command named it 1h ago"),
        "{reason}"
    );
}

#[test]
fn a_time_after_now_keeps_a_workspace_in_use() {
    let now = SystemTime::now();
    let reason = reason(idle(&facts(Some(now + HOUR), None), now, 2 * HOUR));
    assert!(reason.contains("after the current time"), "{reason}");
}

#[test]
fn a_process_outside_phase_that_works_in_it_keeps_a_workspace_in_use() {
    let now = SystemTime::now();
    let mut facts = facts(ago(now, 5), ago(now, 5));
    facts.users = vec![(4242, PathBuf::from("/w/triage/app"))];
    let reason = reason(idle(&facts, now, 2 * HOUR));
    assert!(reason.contains("process 4242"), "{reason}");
    assert!(reason.contains("/w/triage/app"), "{reason}");
}

#[test]
fn a_pinned_workspace_is_in_use_whatever_else_holds() {
    let now = SystemTime::now();
    let mut facts = facts(ago(now, 5), ago(now, 5));
    facts.pinned = true;
    let reason = reason(idle(&facts, now, 2 * HOUR));
    assert!(reason.contains("pinned"), "{reason}");
    assert!(reason.contains("mxv unpin"), "{reason}");
}

#[test]
fn choose_takes_the_least_recently_active_first_until_the_need_is_covered() {
    let now = SystemTime::now();
    let idle = [
        (ago(now, 3), 300),
        (ago(now, 9), 100),
        (None, 50),
        (ago(now, 5), 200),
    ];
    assert_eq!(choose(&idle, 320), [2, 1, 3]);
    assert_eq!(choose(&idle, 50), [2]);
    assert_eq!(choose(&idle, 0), Vec::<usize>::new());
}

#[test]
fn choose_takes_every_workspace_that_frees_memory_when_they_cannot_cover_the_need() {
    let now = SystemTime::now();
    let idle = [(ago(now, 3), 300), (ago(now, 9), 0), (ago(now, 5), 200)];
    assert_eq!(choose(&idle, 10_000), [2, 0]);
}

fn entry(pid: u32, parent: u32, group: u32, dir: &str, resident: u64) -> TableEntry {
    TableEntry::new(pid, parent, group, Some(PathBuf::from(dir)), Some(resident))
}

/// A table of a keeper 100 that runs the app 200 of the group 200, whose
/// child 201 started a group of its own, an agent's shell 300 and its
/// child 301 in the workspace, a process 400 elsewhere, and mxv 500.
fn table() -> Vec<TableEntry> {
    vec![
        entry(100, 1, 100, "/", 10),
        entry(200, 100, 200, "/w/triage/app", 1000),
        entry(201, 200, 201, "/w/triage/app/web", 2000),
        entry(300, 1, 300, "/w/triage", 30),
        entry(301, 300, 300, "/w/triage/app", 40),
        entry(400, 1, 400, "/w/triage-two", 50),
        entry(500, 300, 300, "/w/triage", 60),
    ]
}

#[test]
fn the_processes_that_phase_owns_are_the_keepers_and_apps_and_their_descendants() {
    let table = table();
    let pids: Vec<u32> = owned(&table, &[200]).map(|entry| entry.pid).collect();
    assert_eq!(pids, [100, 200, 201]);
}

#[test]
fn the_users_of_a_workspace_are_the_processes_outside_phase_and_mxv_that_work_in_it() {
    let table = table();
    let found = users(&table, Path::new("/w/triage"), &[200], 500);
    assert_eq!(
        found,
        [
            (300, PathBuf::from("/w/triage")),
            (301, PathBuf::from("/w/triage/app"))
        ]
    );
}

#[test]
fn a_need_names_memory_or_disk_and_a_size() {
    assert_eq!(parse_need("memory=1K"), Ok(Need::Memory(1024)));
    assert_eq!(parse_need("disk=2GiB"), Ok(Need::Disk(2 << 30)));
    assert!(parse_need("cpu=1K").unwrap_err().contains("memory=<SIZE>"));
    assert!(parse_need("memory").is_err());
    assert!(parse_need("memory=lots").is_err());
}

/// The facts of a workspace that mxv measured as idle three hours after
/// it was last named, read again under its lock.
fn still_idle(now: SystemTime) -> Facts {
    facts(ago(now, 5), ago(now, 3))
}

#[test]
fn the_check_under_the_lock_lets_a_workspace_that_is_still_idle_stop() {
    let now = SystemTime::now();
    let facts = still_idle(now);
    assert_eq!(recheck(Some(&facts), facts.touched, now, 2 * HOUR), None);
}

#[test]
fn the_check_under_the_lock_keeps_a_workspace_whose_processes_stopped_after_mxv_measured_it() {
    let now = SystemTime::now();
    let reason = recheck(None, ago(now, 3), now, 2 * HOUR).unwrap();
    assert!(reason.contains("stopped before mxv reached it"), "{reason}");
}

#[test]
fn the_check_under_the_lock_keeps_a_workspace_pinned_after_mxv_measured_it() {
    let now = SystemTime::now();
    let mut facts = still_idle(now);
    facts.pinned = true;
    let reason = recheck(Some(&facts), ago(now, 3), now, 2 * HOUR).unwrap();
    assert!(reason.contains("pinned after mxv measured it"), "{reason}");
}

#[test]
fn the_check_under_the_lock_keeps_a_workspace_that_an_mxv_command_named_after_mxv_measured_it() {
    let now = SystemTime::now();
    let facts = still_idle(now);
    let reason = recheck(Some(&facts), ago(now, 4), now, 2 * HOUR).unwrap();
    assert!(
        reason.contains("named it after mxv measured it"),
        "{reason}"
    );
}

#[test]
fn the_check_under_the_lock_keeps_a_workspace_that_a_process_outside_phase_started_working_in() {
    let now = SystemTime::now();
    let mut facts = still_idle(now);
    facts.users = vec![(4242, PathBuf::from("/work/triage/app"))];
    let reason = recheck(Some(&facts), facts.touched, now, 2 * HOUR).unwrap();
    assert!(reason.contains("process 4242 outside phase"), "{reason}");
    assert!(
        reason.contains("checked again under the workspace's lock"),
        "{reason}"
    );
}

#[test]
fn the_check_under_the_lock_keeps_a_workspace_whose_checkouts_changed_after_mxv_measured_it() {
    let now = SystemTime::now();
    let mut facts = still_idle(now);
    facts.modified = Some(now - Duration::from_secs(30));
    let reason = recheck(Some(&facts), facts.touched, now, 2 * HOUR).unwrap();
    assert!(reason.contains("its checkouts changed"), "{reason}");
    assert!(
        reason.contains("checked again under the workspace's lock"),
        "{reason}"
    );
}

#[test]
fn a_workspace_named_within_the_idle_age_is_kept_without_walking_its_checkouts() {
    let now = SystemTime::now();
    assert!(!walks(&facts(None, ago(now, 1)), now, 2 * HOUR));
    assert!(walks(&facts(None, ago(now, 3)), now, 2 * HOUR));
    assert!(walks(&facts(None, None), now, 2 * HOUR));
    let mut pinned = facts(None, None);
    pinned.pinned = true;
    assert!(!walks(&pinned, now, 2 * HOUR));
    let mut used = facts(None, None);
    used.users = vec![(4242, PathBuf::from("/work/triage/app"))];
    assert!(!walks(&used, now, 2 * HOUR));
}

#[test]
fn container_memory_counts_towards_the_need_only_where_stopping_returns_it_to_the_host() {
    assert_eq!(counted(100, 50, true), 150);
    assert_eq!(counted(100, 50, false), 100);
}

#[test]
fn an_idle_age_under_ten_minutes_is_refused() {
    assert_eq!(MIN_IDLE, Duration::from_secs(10 * 60));
    assert_eq!(parse_idle("10m"), Ok(MIN_IDLE));
    assert_eq!(parse_idle("2h"), Ok(2 * HOUR));
    let refusal = parse_idle("9m").unwrap_err();
    assert!(refusal.contains("10m"), "{refusal}");
    assert!(refusal.contains("stop_idle"), "{refusal}");
    assert!(parse_idle("0s").is_err());
}
