# Lifecycle

This document lists each state that multiverse keeps across commands, across the crates `mxv`, `multiverse` and `phase`. It covers state enums that are stored, and lifecycles that exist only as a layout on disk. It describes the code at this commit.

File paths are relative to `crates/`.

Each table has these columns:

- **Transition** names the states before and after, and the event that moves them.
- **Owner** names the function that performs the transition and its file.
- **Crash point** says where a process death, a kill or a failed step can happen.
- **State left** says what that crash leaves on disk or in the process table.
- **Recovery** says what the next command does, or says that nothing recovers it.
- **Fault test** names a test that causes that fault and then checks the outcome, with its file. The cell says whether the test kills a process, fails a step through a seam, or plants the state and kills nothing. "None" means no such test exists.

Writes to records use `store::replace_file` (`multiverse/src/store.rs`), which writes a temporary file, syncs it, renames it over the target and syncs the directory. A crash during that call leaves the old record or the new one. Locks are `flock` locks, which the operating system releases when the holder dies, so a crash leaves no lock held.

## Workspace creation

The workspace has no state enum. Its state is the record at `$XDG_STATE_HOME/mxv/workspaces/<name>.json` and the directory at `<root>/<name>/`. A checkout with `commit: null` in a clone workspace is unfinished (`Workspace::is_unfinished`, `multiverse/src/record.rs`).

States are absent, recorded with no commits, cloning, and finished.

Only the library reaches the two `Workspaces::make` rows below. mxv always calls `with_mirrors` (`mxv/src/verbs.rs`), so `create_repos` returns through `create_clone`. A worktree workspace reaches mxv only as a version 1 record or a restored worktree archive.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Absent to recorded, on `mxv new` | `Workspaces::create_clone` through `Store::create` (`multiverse/src/workspaces.rs`, `multiverse/src/store.rs`) | The process dies after the record is written and before any directory exists. | A record names checkouts with no commit, and no directory exists. | `mxv remove` removes it. The same `mxv add` resumes one checkout at a time. | None |
| Recorded to cloning | `Workspaces::make_clones` and `make_clone_at` (`multiverse/src/workspaces.rs`) | A step fails, such as a fetch from an unreachable origin. | `Workspaces::undo` deletes the checkouts, the directory and the record, on a best-effort basis. | The next `mxv new` starts clean. | `a_failed_mirror_fetch_fails_with_its_origin_and_leaves_no_record_or_directory` in `multiverse/src/workspaces/clone_tests.rs` and `a_failed_second_clone_deletes_the_first_and_the_record` in `multiverse/src/workspaces/multi_tests.rs` fail the fetch by deleting the origin. |
| Recorded to cloning | `make_clone_at` | The process dies during the clone. No `undo` runs. | The record has no commit for that checkout, and a partial directory exists. | `mxv new` with that name fails because the name is taken. `up`, `restart` and `clean` refuse with `mxv::checkout::incomplete`. `mxv remove` or the same `mxv add` recovers it. | `an_add_that_stopped_partway_is_resumed_by_the_same_add_and_refuses_any_other` in `mxv/tests/multirepo.rs` plants the state and kills nothing. |
| Cloning to finished | `Workspaces::write_agents`, `write_marker`, `write_manifest`, `Store::update` (`multiverse/src/workspaces.rs`) | A step fails after the clones exist. `write_marker` writes the marker `.deadband-workspace` (see [Workspace marker](#workspace-marker)) and then runs step `marker` of the `checkpoint` seam. | `undo` deletes the manifest and the marker that the run wrote along with the rest. | The next `mxv new` starts clean. | `a_failed_creation_deletes_the_manifest_it_wrote_with_the_rest` in `multiverse/src/workspaces/clone_tests.rs` fails the fetch by deleting the origin, which is earlier than this step. `a_failed_marker_write_during_new_takes_the_undo_path` in `multiverse/src/workspaces/tests.rs` fails step `marker` of a worktree workspace through the seam, which `make_clones` shares. |
| Worktree workspace, recorded to made | `Workspaces::make` (`multiverse/src/workspaces.rs`) | A step fails, such as a hook that exits with an error. | `undo` removes the checkout, the branch, the directory and the record. | The next `create` through the library starts clean. | `a_checkout_that_fails_partway_leaves_no_checkout_branch_directory_or_record` in `multiverse/src/workspaces/tests.rs` fails the step with a failing hook. `a_failed_fetch_leaves_no_record_and_no_directory` in the same file fails the fetch. |
| Worktree workspace, recorded to made | `Workspaces::make` | The process dies with the worktree locked as initializing. | The record exists and git holds a worktree locked with the reason `initializing`. | A removal unlocks and removes it. | `a_checkout_locked_while_initializing_is_unlocked_and_removed` in `multiverse/src/workspaces/tests.rs` plants the lock and kills nothing. |
| Finished, ignored directories copied | `clone_into` (`mxv/src/verbs.rs`) | The copy fails. | The workspace is finished without those directories. | The command reports a warning, and no later command retries the copy. | None |

## Workspace marker

mxv keeps an empty regular file named `.deadband-workspace`, with mode 0644, in each workspace's directory (`multiverse/src/marker.rs`). deadband walks up from a job's directory to the nearest directory that holds a regular file of that name and shows that directory's name as the job's workspace, so a job that a person runs by hand in a checkout, such as `deadband run -- cargo test`, shows its workspace. Every build and task that mxv itself admits names its workspace in `Asked.workspace` (`multiverse/src/runs/phased/build.rs`, `mxv/src/admission.rs`), so mxv's own jobs do not depend on the marker.

The marker has no stored state. Each verb reads the directory. Its states are absent, present (a regular file of the name) and blocked (an entry of the name that is not a regular file, such as a directory or a link). mxv never replaces an entry of the name, and it deletes only an empty regular file. `.deadband-workspace` is in `RESERVED_FOLDERS` (`multiverse/src/workspaces.rs`), so no checkout's folder can take the name.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Absent to present, on `mxv new` | `Workspaces::write_marker` from `make_clones` or `make` (`multiverse/src/workspaces.rs`), through `marker::ensure` and `store::create_empty` (`multiverse/src/store.rs`) | The write fails, or step `marker` of the `checkpoint` seam fails. | `Workspaces::undo` deletes the marker, the directory, the checkouts and the record. | The next `mxv new` starts clean. | `a_failed_marker_write_during_new_takes_the_undo_path` in `multiverse/src/workspaces/tests.rs` fails the step through the seam. |
| Absent to present, on `mxv add` | `marker::ensure` from `Workspaces::finish_add` | The write fails. | `undo_add` restores the record, `AGENTS.md` and the manifest, and deletes the clone. A marker that the run wrote stays, because the workspace stays. | The earlier record is back. | `add_writes_the_marker_that_a_workspace_made_before_markers_lacks` in `multiverse/src/workspaces/multi_tests.rs` plants an absent marker and kills nothing. None fails the write. |
| Absent to present, on `mxv restore` | `Workspaces::write_marker` from `Workspaces::rebuild` (`multiverse/src/archive/restore.rs`), before step `record` | The write fails, or step `marker` fails. | `unmake` removes what the run made, and the archive is whole. | The next `mxv restore` starts again. | The restore step tests fail step `marker` through the seam. |
| Absent to present, on `up`, `restart`, `run` and `exec` | `Context::mark` (`mxv/src/processes.rs`), called while the verb holds the workspace's lock | The write fails, such as on a directory that mxv cannot write. | No marker. The verb prints a warning that names the path and carries on. | The next such verb writes it. | `a_failed_marker_write_warns_and_up_continues` in `mxv/tests/processes.rs` makes the directory read-only. |
| Present to absent | A person deletes the marker | None | Absent | The next `up`, `restart`, `run` or `exec` writes it. | `mxv_up_writes_a_missing_marker_and_leaves_a_directory_of_that_name_alone` in `mxv/tests/processes.rs` plants the state and kills nothing. |
| Blocked | A directory or link of the marker's name exists | None | The entry stays untouched, and deadband ignores it. | `up`, `restart`, `run` and `exec` print a warning that names the path. Archive copies it as an extra root entry, and `mxv remove` keeps the directory. | `mxv_up_writes_a_missing_marker_and_leaves_a_directory_of_that_name_alone` in `mxv/tests/processes.rs` and `ensure_leaves_a_directory_or_a_link_of_the_markers_name_alone` in `multiverse/src/marker/tests.rs` plant the state and kill nothing. |
| Present, during archive | `root::scan` (`multiverse/src/archive/root.rs`) counts an empty marker as mxv's own, and `Workspaces::remove` deletes it | None | The archive does not hold it. A marker that holds bytes is the person's file, and the archive copies it. | `mxv restore` writes it again. | `archive_does_not_count_the_marker_as_an_extra_entry_and_restore_writes_it_again` and `a_marker_that_holds_bytes_is_archived_as_the_persons_file_and_comes_back` in `multiverse/src/archive/tests.rs` kill nothing. |

## Adding a checkout

`Leftover` (`multiverse/src/workspaces.rs`) classifies what an earlier run left. It is computed on each `add` and never stored. The stored state is the same unfinished checkout as in creation.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Finished to unfinished, on `mxv add` | `Workspaces::add` through `Store::update` (`multiverse/src/workspaces.rs`) | The process dies after the record is written and before the clone starts. | The record names a checkout with no commit, and no directory exists. | The same `add` classifies it as `Leftover::Absent` and resumes. | `the_same_add_resumes_a_checkout_that_a_stopped_add_left_without_a_directory` in `multiverse/src/workspaces/multi_tests.rs` plants the state and kills nothing. |
| Unfinished, partial clone | `Workspaces::leftover`, `make_clone` | The process dies during the clone. | The directory holds a partial clone. | The same `add` deletes a partial clone that holds nothing of its own and clones again. | `the_same_add_deletes_a_partial_clone_and_clones_again` in `multiverse/src/workspaces/multi_tests.rs` plants the state and kills nothing. |
| Unfinished, partial clone that holds work | `Workspaces::leftover` | The clone directory holds a commit, an untracked file or an ignored file. | The directory stays. | The same `add` returns `AddOutcome::ResumeBlocked`. The person must move or delete the work. | `the_same_add_refuses_to_delete_a_partial_clone_that_holds_a_commit_of_its_own`, `the_same_add_refuses_to_delete_a_partial_clone_that_holds_an_untracked_file` and `the_same_add_refuses_to_delete_a_partial_clone_that_holds_an_ignored_file` in `multiverse/src/workspaces/multi_tests.rs` plant the state and kill nothing. |
| Unfinished, another `add` arrives | `Workspaces::add` | A different `add` runs while a checkout is unfinished. | Nothing changes. | The command returns `AddOutcome::Incomplete` and names the checkout. | `another_add_refuses_while_a_checkout_is_unfinished_and_names_it` in `multiverse/src/workspaces/multi_tests.rs` plants the state and kills nothing. |
| Unfinished to finished | `Workspaces::finish_add` (`multiverse/src/workspaces.rs`) | A step fails after the clone. | `undo_add` restores the record, `AGENTS.md` and the manifest, and deletes the clone unless the clone holds work. | The earlier record is back. | `a_failed_add_deletes_the_new_clone_and_restores_the_record`, `a_resumed_add_that_fails_restores_the_earlier_record_and_deletes_the_partial_clone`, `a_resumed_add_that_fails_keeps_a_finished_clone_that_holds_work_and_says_so` and `a_resumed_add_that_fails_deletes_a_finished_clone_without_work_and_restores_the_record` in `multiverse/src/workspaces/multi_tests.rs` fail the step by deleting the origin or with a broken manifest. |
| Unfinished to finished | `Workspaces::finish_add` | The process dies after the manifest is replaced and before the final `Store::update`. | The manifest is new and the record has no commit for the checkout. | The same `add` classifies the clone as `Leftover::Finished` and completes it. | `the_same_add_keeps_a_clone_that_reached_the_workspace_branch_and_records_its_commit` in `multiverse/src/workspaces/multi_tests.rs` plants the state and kills nothing. |

## Removing a workspace

`RemovalPlan` and `RemovalOutcome` are per-call results. The state is the record, the checkouts, and the names `.<folder>.mxv-removing` that hold a checkout during its deletion.

`mxv remove` (`verbs::remove`, `mxv/src/verbs.rs`) calls `stop_all`, which takes and releases the workspace lock, and then calls `Workspaces::remove` with no workspace lock held. A concurrent `mxv up` can therefore start between the stop and the removal. `Workspaces::remove` replans first and returns `RemovalOutcome::Changed` when `loses_more` finds a worse plan.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Record and clone to moved aside | `Workspaces::remove`, `delete_clone` (`multiverse/src/workspaces.rs`) | The process dies after the rename and during the deletion. | The record stays, the checkout path is empty and `.<folder>.mxv-removing` holds what is left. | A second `mxv remove` reports the checkout as missing and deletes the rest. | `a_clone_whose_deletion_was_interrupted_is_finished_by_a_second_removal` in `multiverse/src/workspaces/clone_tests.rs` plants the state and kills nothing. |
| Worktree removed | `Workspaces::remove`, `Backend::remove_checkout` (`multiverse/src/git.rs`) | `git worktree remove` stops partway and has deleted the `.git` file. | The record stays and the checkout has files and no `.git`. | The next `plan_removal` returns `RemovalPlan::Dirty`. `--force` finishes it. | `a_worktree_that_an_interrupted_removal_left_without_its_git_file_is_finished_when_forced` in `multiverse/src/workspaces/tests.rs` fails the step through a scripted backend. |
| A checkout's directory is gone | `plan_removal` (`multiverse/src/workspaces.rs`) | The directory was deleted outside mxv. | The record names a path that does not exist. | `mxv remove` removes the record. | `a_workspace_whose_checkout_is_gone_is_removed_and_the_repository_forgets_the_checkout` in `multiverse/src/workspaces/tests.rs` and `a_clone_whose_directory_is_gone_is_removed_with_its_record` in `multiverse/src/workspaces/clone_tests.rs` plant the state and kill nothing. |
| Checkouts deleted to directory removed | `Workspaces::remove` (`multiverse/src/workspaces.rs`) deletes the workspace manifest, `AGENTS.md` and an empty marker `.deadband-workspace`, then the directory | Another entry, such as a marker that holds bytes or a directory of the marker's name, stays. | The directory stays with that entry, and the removal reports it as kept. | Nothing needs recovering. The person deletes the entry. | `removing_a_workspace_deletes_its_marker_with_its_directory` and `removing_a_workspace_keeps_a_directory_of_the_markers_name` in `multiverse/src/workspaces/tests.rs` plant the state and kill nothing. |
| Checkouts deleted to record deleted | `Workspaces::remove`, `Store::delete` | The process dies after the checkouts are deleted and before `Store::delete`. | The record names missing checkouts. | A second `mxv remove` finishes it. | `a_workspace_whose_checkout_is_gone_is_removed_and_the_repository_forgets_the_checkout` in `multiverse/src/workspaces/tests.rs` and `a_clone_whose_directory_is_gone_is_removed_with_its_record` in `multiverse/src/workspaces/clone_tests.rs` plant the state and kill nothing. |
| Record deleted to run files forgotten | `verbs::remove` calls `processes::forget`, then `delete_volumes` (`mxv/src/verbs.rs`, `mxv/src/processes.rs`) | The process dies after `Store::delete` and before `forget` or `delete_volumes`. | The run directory, the phase run record, the logs and the docker volumes remain, with no workspace record. | Nothing recovers it. No mxv command can name those files once the record is gone. | None |

## Archiving a workspace

`ArchiveState` (`multiverse/src/archive/record.rs`) has the states `Retiring` and `Archived`, and is stored in `$XDG_STATE_HOME/mxv/archives/<name>/archive.json`. `Workspaces::archive` (`multiverse/src/archive/retire.rs`) writes `Retiring`. `Workspaces::finish_archive` in the same file is the only function that writes `Archived`. `Workspaces::subject` and `admit` (`multiverse/src/archive/admit.rs`) read the state: `Retiring` with no workspace record returns `ArchivePlanned::Finish`.

The `Err` of the `checkpoint` seam stops `Workspaces::archive` and runs no undo, so the two tests below leave the state that a kill at that point would leave. They name the step labels in the table. They are `a_worktree_archive_stopped_after_each_step_finishes_when_it_runs_again` and `a_clone_archive_stopped_after_each_step_finishes_when_it_runs_again` in `multiverse/src/archive/tests.rs`. Below they are called the step tests. They fail the step through the seam and kill nothing.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Live, work in progress committed | `commit_wip` (`multiverse/src/archive/retire.rs`) | Step `wip`, after the commit. | The workspace is live and holds a new commit. | The next `mxv archive` plans again. | `a_wip_archive_stopped_after_its_commit_finishes_when_it_runs_again` in `multiverse/src/archive/tests.rs` fails the step through the seam. |
| Live to `Retiring` | `Workspaces::archive`, `Archives::write` (`multiverse/src/archive/record.rs`) | Step `record`. | The record says `Retiring` and the workspace is whole. | `admit` plans again and overwrites the record. | The step tests. `a_rerun_after_head_moved_pins_the_new_commit_and_finishes_the_archive` in `multiverse/src/archive/tests.rs` also fails this step through the seam and then moves `HEAD`. |
| Pins written | `Workspaces::pin` (`multiverse/src/archive/retire.rs`) | Step `pins`. | References exist under `refs/mxv/archives/<id>/`. | A rerun writes the same pins. | The step tests. `a_run_stopped_after_the_pins_leaves_no_second_pin_when_it_runs_again` in `multiverse/src/archive/tests/edges.rs` fails the step through the seam. |
| Root files copied | `root::copy_in` (`multiverse/src/archive/root.rs`) | Step `root`. | The archive holds the root files. | A rerun copies them again. | The step tests. `a_rerun_copies_a_root_file_again_after_a_second_checkout_stopped_the_first_run` in `multiverse/src/archive/tests.rs` fails a later step through the seam. |
| Checkout quarantined | `Workspaces::retire` through `store::move_aside` (`multiverse/src/archive/retire.rs`) | Steps `before-move:<folder>` and `moved:<folder>`. The second leaves the checkout at `.<folder>.mxv-archiving`. | The checkout is at its own name, or at the quarantine name. | `put_back` moves a quarantined checkout back on the next `admit` with `Mode::Change`. | The step tests. |
| Quarantine compared | `Workspaces::retire` | A file changed after the move, or the comparison failed. | `retire` moves the checkout back, and the archive refuses. | The person resolves the change and runs `mxv archive` again. | `a_file_written_while_a_clone_is_moved_aside_puts_the_clone_back_and_refuses` and `a_file_written_while_a_worktree_is_moved_aside_puts_the_worktree_back_and_refuses` in `multiverse/src/archive/tests.rs` write the file from the seam. `an_error_after_the_rename_puts_the_checkout_back_under_its_own_name` in the same file writes a broken `HEAD` from the seam, so the comparison fails. |
| Checkout deleted | `Workspaces::retire`, `delete_directory` | Steps `cleared:<folder>` and `retired:<folder>`. | The checkout is at `.<folder>.mxv-removing` or is gone. Its commit is pinned. | The next `mxv archive` finishes. | The step tests. |
| Root originals compared | `root::differences` (`multiverse/src/archive/root.rs`) | A root file changed after the copy. This check runs after `retire` has deleted every checkout. | The checkouts are already deleted. The record stays `Retiring`, and the root file keeps the edit. | A rerun copies the edited file and finishes. | `a_root_file_edited_during_retirement_refuses_and_the_rerun_keeps_the_edit` in `multiverse/src/archive/tests.rs` edits the file from the seam. |
| Root originals deleted | `root::delete_originals` | Step `delete`. | The originals are gone and the archive holds copies. | The next `mxv archive` finishes. | The step tests. |
| Workspace record removed | `Workspaces::finish_removal`, which calls `Workspaces::remove` (`multiverse/src/archive/retire.rs`) | Step `removed`. | No workspace record exists and the archive says `Retiring`. | `subject` returns `ArchivePlanned::Finish`. | The step tests. |
| `Retiring` to `Archived` | `archive::finish` (`mxv/src/archive.rs`): `processes::forget`, `delete_volumes`, then `Workspaces::finish_archive` | The process dies at any point in `finish`. `forget` runs before `finish_archive`. | The record stays `Retiring`, with some run files or volumes already deleted. | `mxv archive -w <name>` runs `finish` again. `mxv restore` and `mxv remove --archived` refuse until then, with `mxv::restore::unfinished` and `mxv::archive::unfinished`. `mxv list` shows a problem row. | `an_unfinished_archive_is_a_degraded_row_that_names_the_command_that_finishes_it` in `mxv/tests/archive.rs` plants the state and kills nothing. |

## Restoring a workspace

The stored state is the `Archived` record and the live record that replaces it. `RestorePlanned` and `RestoreOutcome` are per-call results. `Workspaces::restore` is in `multiverse/src/archive/restore.rs`.

A failed step calls `Workspaces::unmake`, so the `Err` of the `checkpoint` seam tests the undo path, and it does not model a kill. Below, the restore step tests are `a_worktree_restore_stopped_after_each_step_leaves_the_archive_whole` and `a_clone_restore_stopped_after_each_step_leaves_the_archive_whole` in `multiverse/src/archive/tests.rs`.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| `Archived`, mirror ensured | `ensure_mirror` | Step `mirrors`, or a fetch fails into a mirror that did not exist. | The archive is whole. `ensure_mirror` deletes a mirror that this run made when its fetch fails. | The next `mxv restore` starts again. | The restore step tests fail the step through the seam. `an_unreachable_origin_leaves_no_empty_mirror_behind` in `multiverse/src/archive/tests.rs` fails the fetch. |
| Rebuilding | `Workspaces::rebuild` | Steps `directory`, `checkout:<folder>`, `root` and `marker` fail. Step `marker` follows `write_marker`, which writes the marker `.deadband-workspace` after the root files, because the archive leaves an empty marker out. | `unmake` removes what the run made, the marker included, and the archive is whole. | The next `mxv restore` starts again. | The restore step tests fail the step through the seam. |
| Rebuilding | `Workspaces::rebuild` | The process dies during the rebuild. `unmake` does not run. | The workspace directory exists and has no record. | The next `mxv restore` returns `RestoreRefusal::NameTaken` with `record: false`. The person must delete the directory by hand. No command cleans it. | `a_restore_refuses_a_path_that_something_else_holds` in `multiverse/src/archive/tests.rs` plants a directory, kills nothing and checks only the refusal. |
| Rebuilt to recorded | `Runs::restored_ports` calls the writer under `.ports.lock` (`multiverse/src/runs.rs`) | Step `record`, after the workspace record is written. No `unmake` runs. | The workspace is live and the `Archived` record remains. | `mxv restore` returns `NameTaken` with `record: true`. | `a_restore_stopped_after_its_record_keeps_the_archive_and_refuses_the_name` in `multiverse/src/archive/tests.rs` fails the step through the seam. |
| Recorded to archive deleted | `Workspaces::delete_pins`, `Archives::delete` | The process dies after `delete_pins` and before `Archives::delete`. | The workspace is live, and the `Archived` record names pins that are gone. | `mxv remove --archived` deletes the record. `mxv restore` refuses with `NameTaken`. | None |

## Deleting an archive

`archive::remove` (`mxv/src/archive.rs`) refuses a `Retiring` record before it takes the workspace lock, and it does not check that again under the lock. With `--delete-volumes` it refuses volumes that a live workspace uses (`refuse_live_volumes`), before the lock and again under it. It deletes the volumes first because the archive holds the only id that names them, and then calls `Workspaces::remove_archive` (`multiverse/src/archive/restore.rs`).

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| `Archived` to volumes deleted | `archive::remove` | The process dies after the volumes are deleted. | The archive stays and its volumes are gone. | `mxv remove --archived` runs again and finishes. | None |
| Volumes deleted to pins deleted to archive deleted | `Workspaces::remove_archive`: `delete_pins`, then `Archives::delete` | The process dies between the two. | The record stays and some pins are gone. | `mxv remove --archived` runs again and finishes. | None |

## Mirrors

A mirror has no state enum. It is a bare repository at `$XDG_DATA_HOME/mxv/mirrors/<origin key>`, guarded by `<mirror>.lock` (`Workspaces::lock_mirror`, `multiverse/src/workspaces.rs`). States are absent, prepared and fetched.

No removal deletes a mirror. The one deletion in the code is in `ensure_mirror`, for a mirror whose first fetch failed during a restore.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Absent to prepared | `Backend::prepare_mirror` (`multiverse/src/git.rs`) | The process dies during `git init`. | A directory exists with no `HEAD`, or with one. | `prepare_mirror` runs `git init` again when `HEAD` is missing. | None |
| Prepared to fetched | `Backend::fetch_mirror` (`multiverse/src/git.rs`) | The fetch fails during `mxv new` or `mxv add`. | The prepared mirror stays. `make_clone_at` does not delete it. | The next fetch reuses it. | `a_failed_mirror_fetch_fails_with_its_origin_and_leaves_no_record_or_directory` in `multiverse/src/workspaces/clone_tests.rs` deletes the origin and checks the workspace. It does not check the mirror. |
| Prepared to fetched | `Backend::fetch_mirror` | The fetch is killed or misses `FETCH_DEADLINE`. | git may leave its own lock files in the mirror. | git reports them on the next fetch, and the person removes them. | None |

## Run records and process entries

`EntryState` (`phase/src/record/run.rs`) has the states `Planned`, `Running`, `Stopping` and `Stopped`. phase stores it for each keeper, host process, container, network and volume in `$XDG_STATE_HOME/mxv/phase/runs/<id>.json`, and writes each state before the step that the next state describes. A host entry also stores `completed`, which the oneshot table below covers. `Journal` (`phase/src/record/store.rs`) performs every write under the lifecycle lock.

Two writers own host entry states. The engine owns them during `up`, `restart` and `down`, through `Journal`. The keeper's `Recorder` (`phase/src/engine/keeper/recorder.rs`) owns them afterwards and writes them asynchronously, so a restart or an end can change an entry at any time. A `Recorder` note applies only while the entry names the keeper's own process (`Note::applies_to`). A note that cannot be written is logged and dropped.

`Phase::down` and `Phase::sweep` (`phase/src/phase.rs`) both stop what a record names without a plan, because the record stores how to stop each process. `down` leaves a workspace volume `Running`, and `RunRecord::all_stopped` ignores workspace volumes, so `down` still removes the record. The only caller of `sweep` in the product is `Runs::forget` (`multiverse/src/runs/phased.rs`), which `mxv remove` and `mxv archive` reach. No command runs `sweep` by itself.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| No record to `Planned` | `Journal::plan` | The process dies after the write and before any launch. | Every object in the plan is `Planned` with no process. | The next `up` launches. `Journal::forget_unstarted` removes entries that `up` never tried. | None |
| `Planned` to keeper launched | `start_process`, `Journal::launching`, `launch` (`phase/src/engine/run.rs`) | `mxv` dies after the keeper starts its app and before `Journal::started`. | The keeper and the app run. The keeper queued `Note::Started` before its handshake (`phase/src/engine/keeper/supervise.rs`), and its writer waits for the lifecycle lock, which the operating system releases when `mxv` dies. The writer then records the app's process entry as `Running`. The keeper's own entry stays `Planned` with no process, because the writer writes only the process entry. | The next `up` races that write for the lock. When the writer wins, `recorded` returns `Recorded::Ours`, `up` reports the app as already running, and `sweep` can stop it. When `up` wins, it finds nothing and launches a second keeper, and only a held port stops it. The first keeper's `Note::Started` then does not apply, because the entry names another process, so the first app stays unnamed. | None |
| Handshake | `launch`, `HostRunner::launch` (`phase/src/engine/host.rs`), `engine/keeper/spawn.rs` | The handshake misses its deadline. | The engine sends the keeper `TERM`, waits the grace and kill wait, then kills it. | `up` reports `phase::keeper::unstartable`. | `a_keeper_whose_handshake_misses_the_deadline_is_unstartable_and_leaves_no_app_behind` in `phase/src/engine/keeper/tests.rs` uses a keeper that answers late. |
| Handshake | `engine/keeper/spawn.rs` | The keeper's start of the app takes longer than that wait. `phase/src/engine/keeper.rs` documents this window. | The engine kills the keeper before it can stop the app. The app runs with no keeper, and the record does not name it. | Nothing recovers it. | None |
| Launched to `Running` | `Journal::started` | The write fails after a launch. | The record cannot name the process. | `up` fails and stops every process it started. | `a_record_that_cannot_be_written_after_a_launch_fails_up_and_stops_every_process_it_started` in `phase/src/engine/run/tests.rs` fails the write through the test host. |
| `Running`, waiting for readiness, cancelled through the library | `await_ready` in `start_process` | A library caller cancels the `Cancel` it passed to `Phase::up`. mxv never does this. | The processes that this `up` started are stopped. | The next `up` starts them. | `a_cancelled_up_stops_what_it_started_and_reports_the_rest_as_not_started` and `a_cancel_during_a_readiness_wait_stops_every_process_this_up_started` in `phase/src/engine/run/tests.rs` cancel through the seam. |
| `Running`, waiting for readiness, `mxv` interrupted. **Gap:** no rollback runs on this path. | `Runs::up` (`multiverse/src/runs/phased.rs`) passes `&Cancel::new()` to `Phase::up`, and nothing cancels that value. | `SIGINT` arrives after the builds end. `Held::end` (`mxv/src/admission.rs`) has stopped cancelling by then, so the signal ends `mxv`. | The keepers and their apps keep running, because each keeper runs in its own session. The processes this `up` started are not stopped. | The next `up` reports them as already running, and `down` stops them. | None checks what becomes of the processes. `an_interrupt_after_the_builds_ends_mxv_while_it_waits_for_a_process_to_be_ready` in `mxv/tests/processes.rs` sends `SIGINT` and asserts only that `mxv` ended with that signal. |
| `Running`, an app fails to start | `Runs::failure`, `Runs::roll_back_phase` (`multiverse/src/runs/phased.rs`) | A later app fails. `roll_back_phase` stops only the apps whose outcome is `AppOutcome::Started`. | Apps this `up` restarted keep running. | The report names them. | `a_failed_up_stops_the_processes_that_it_started_before_the_failure` and `a_failed_up_leaves_a_process_it_restarted_running_and_says_it_restarted_it` in `mxv/tests/processes.rs` start a process that exits. |
| `Running`, `mxv` dies | `Journal::started` (`phase/src/record/store.rs`) wrote the state, and the keeper in `phase/src/engine/keeper/supervise.rs` runs on its own. | The `mxv` command dies after `Journal::started`. | The record says `Running`, and the keeper and app continue. | The next `up` reports `AppOutcome::AlreadyRunning`. | None kills `mxv` at this point. `up_counts_a_group_whose_leader_has_gone_as_running_while_its_keeper_runs` in `phase/src/engine/run/tests.rs` plants a related state through the test host. |
| `Running`, process has gone | `recorded` (`phase/src/engine/run.rs`) | The host process and keeper ended while no command ran. | The record says `Running`. | `up` starts it again. | `up_starts_a_recorded_process_again_once_it_has_gone` in `phase/src/engine/run/tests.rs` plants the state and kills nothing. |
| `Running`, group leader has gone without a keeper | `recorded`, `Identity` (`phase/src/record/process.rs`) | The keeper and the leader were killed and a group member survives. | The record names a group that phase cannot prove is its own. | `up` starts nothing in its place and fails. phase never signals that group. | `up_fails_as_unstoppable_and_starts_nothing_more_when_phase_leaves_a_group_it_cannot_prove` in `mxv/tests/processes.rs` kills the keeper and leader with `SIGKILL`. `up_starts_nothing_in_place_of_a_group_whose_leader_has_gone_and_reports_it_left` in `phase/src/engine/run/tests.rs` and `a_group_whose_leader_has_gone_without_a_keeper_is_unknown_and_never_signalled` in `phase/src/record/sweep/tests.rs` plant the state. |
| `Running`, pid reused | `sweep`, `Identity` | The machine reused the pid, or rebooted. | The record names a process that is not phase's. | `sweep` reports it and sends no signal. | `sweep_leaves_a_process_whose_start_time_differs_and_reports_the_pid_as_reused` and `a_process_recorded_in_an_earlier_boot_is_gone_even_when_its_pid_and_start_match` in `phase/src/record/sweep/tests.rs` plant the state. |
| `Running` to `Stopping` | `stop_target`, `Journal::stopping` (`phase/src/engine/run/stop.rs`) | The process dies after the write and before the stop. | The record says `Stopping` and the process runs. | `down` runs again. `up` reports a running keeper as already running. | `up_reports_a_running_keeper_as_already_running_even_when_its_process_entry_reads_stopping` in `phase/src/engine/run/tests.rs` plants the state. |
| `Stopping` to `Stopped` | `stop_target`, `Journal::stopped` | The process survives `KILL`. | The record keeps `Stopping`. | `down` and `sweep` report it as unstoppable. | `a_process_that_survives_kill_is_unstoppable_and_stays_recorded_as_stopping` in `phase/src/record/sweep/tests.rs` plants a surviving process through the test host. `down_keeps_a_process_that_survives_kill_and_notes_its_group` in `phase/src/engine/run/tests.rs` does the same for `down`. |
| `Running` to `Stopped`, with a known end | `Recorder`, `Journal::ending`, `Journal::stopped_after` | The app ends while no command runs. | The record holds the end. | `status` reports the exit. | `status_reports_a_process_that_crashed_as_exited_with_its_code` and `status_reports_a_process_whose_group_sigkill_ended_as_exited_with_signal_9` in `mxv/tests/processes.rs` let the app end or kill its group. |
| `Stopped` to `Planned` | `Journal::launching`, `Journal::containers_launching` | The process dies after the write and before the launch. | The entry says `Planned` with no process. | The next `up` launches. | None |
| Container `Planned` to `Running` | `start_containers` (`phase/src/engine/run.rs`), `Journal::started` | The process dies after the container starts and before its id is recorded. | The record has no id for a running container. | `sweep` finds it by name. | `sweep_finds_a_container_that_started_before_its_id_was_recorded_by_its_name` in `phase/src/record/sweep/tests.rs` plants the state. |
| Volume `Planned` to `Running` | `start_containers` (`phase/src/engine/run.rs`), `Journal::started` | The process dies after docker creates the volume and before the write. | The entry stays `Planned`, and the volume exists. | The next `up` finds the volume by name through `ensure_volume` (`phase/src/engine/container/objects.rs`), reuses it and records it as `Running`. `sweep` keeps a workspace volume. | None |
| Network or run volume `Running` to `Stopping` to `Stopped`. Only a test runner plans these objects. `app::objects` (`phase/src/engine/container/app.rs`) plans no network and only workspace volumes. | `docker_object` (`phase/src/record/sweep.rs`) is the only writer of these states for them. `down_app` (`phase/src/engine/run/stop.rs`) stops host processes and containers only. | `down` runs and does not touch them. | The entries stay `Running`, `RunRecord::all_stopped` stays false, and `Journal::finish` keeps the record until a `sweep`, which only `mxv remove` and `mxv archive` reach. | The next `up` keeps the entries, because `RunRecord::merge` adds only missing ones. | Not a crash point. |
| Network or run volume `Stopping` to `Stopped` | `docker_object` | The process dies between `Journal::stopping` and the removal. | The entry says `Stopping`, and the object exists. | A later `sweep` finds it by name and removes it. The next `up` keeps the entry. | None |
| Container start fails | `start_containers` | Docker fails partway. | The record names what docker created and removed. | The next `up` or `down` reads it. | `a_failed_container_start_records_every_container_it_created_and_those_it_removed_as_stopped` and `a_container_start_that_fails_before_it_removes_anything_keeps_the_recorded_ids` in `phase/src/engine/run/tests.rs` fail the runner through the seam. |
| Volume owner helpers | `sweep` | `up` was interrupted while helpers ran. | Helper containers remain. | `sweep` removes them. | `sweep_removes_the_volume_owner_helpers_that_an_interrupted_up_left_behind` in `phase/src/record/sweep/tests.rs` plants the state. |
| All `Stopped` to no record | `Journal::finish` | The write of the removal fails. | The record stays. | `down` or `sweep` runs again. | `a_directory_that_cannot_be_synced_fails_a_write_and_a_removal` in `phase/src/record/store/tests.rs` fails the sync. `finishing_removes_the_record_only_when_everything_is_stopped` in the same file checks a partly stopped record. |
| Any write to the record | `Journal` | The directory sync fails after the rename. | The new record is on disk. | The journal keeps the new record. | `a_write_whose_directory_sync_fails_after_the_rename_keeps_the_new_record_in_the_journal` in `phase/src/record/store/tests.rs` fails the sync. |

## Oneshot completion

`HostEntry.completed` (`phase/src/record/run.rs`) stores the plan hash of a oneshot that exited with status 0 since its last launch. It is empty or set. `mxv-provision` is such a oneshot, so every manifest with services uses it. A later `up` reads it to decide whether to run the oneshot again (`phase/src/engine/run/oneshot.rs`).

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Empty to set | `RunRecord::set_ended`, reached from `up` when it watched the exit, or from the keeper's `Recorder`, first through `Journal::ending` with the state `Stopping` and again through `Journal::stopped_after` with the state `Stopped`. Only an entry in `Stopped` counts as complete for a later `up` (`completed_from_plan`) | `up` stops waiting while the oneshot runs. | The keeper records the exit of 0 once it holds the lifecycle lock. | The next `up` leaves the oneshot alone under the rules in `oneshot.rs`. | `a_oneshot_that_up_stopped_waiting_for_completes_when_its_keeper_records_an_exit_of_zero` in `phase/src/engine/run/tests/oneshot.rs` and `a_real_oneshot_that_up_stopped_waiting_for_completes_once_its_keeper_records_the_exit` in `phase/src/phase/tests/oneshot.rs` stop the wait and kill nothing. |
| Set to empty, on a launch | `Journal::launching` | The process dies after the write and before the launch. | The entry says `Planned` and the completion is gone. | The next `up` runs the oneshot again. | None |
| Set to empty, on a `down` that selects it | `Journal::clear_completion`, from `phase/src/engine/run/stop.rs` | The process dies before the write. | The completion stays. | `down` runs again. | None |

## Keepers

The keeper has no state enum. Its life is the control flow of `phase/src/engine/keeper/supervise.rs`. `KeeperExit` (`phase/src/engine/keeper.rs`) is its exit code. The keeper's states are spawned, handshaken, supervising, in backoff, stopping and exited. It communicates through its standard input and output and through `TERM` and `INT`, and it has no control socket.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Supervising, keeper killed | `supervise.rs` | The keeper receives `SIGKILL`. | The app keeps running and logging, with no restart policy applied. | `up` counts the app by its record. `sweep` stops it when the keeper was phase's own. | `killing_the_keeper_leaves_its_app_running_and_appending_to_its_log` in `phase/src/engine/keeper/tests.rs` kills the keeper. `a_keeper_that_is_phases_own_lets_sweep_stop_its_apps_group_after_the_leader_has_gone` in `phase/src/record/sweep/tests.rs` plants the state. |
| Supervising to backoff to supervising | `supervise.rs`, `Recorder` | The app crashes. | The keeper restarts it and records the new process. | The next `status` reads the record. | `a_keeper_records_and_restarts_an_app_that_crashes_after_the_engine_stopped_reading` in `phase/src/engine/keeper/tests.rs` makes the app exit. |
| Stopping | `supervise.rs` | The app ignores its stop signal. | The keeper sends `KILL` after the grace. | The keeper exits. | `a_keeper_kills_an_app_that_ignores_its_stop_signal_once_the_grace_passes` in `phase/src/engine/keeper/tests.rs` uses such an app. |
| Backoff to exited | `supervise.rs` | A stop request arrives during a backoff. | The app stays stopped. | The keeper exits with no restart. | `a_stop_request_during_a_backoff_leaves_the_app_stopped` in `phase/src/engine/keeper/tests.rs` sends the request in a backoff. |
| `Recorder` write | `Recorder`, `Note::applies_to` | Another operation has already changed the entry. | The entry stays as that operation wrote it. | Nothing needs recovery. | `a_keepers_end_and_stop_skip_an_entry_that_another_operation_already_stopped` and `a_keeper_leaves_a_record_entry_that_names_another_process_unchanged` in `phase/src/engine/keeper/tests.rs` plant the entry. |
| `Recorder` write | `Recorder` | The write fails. | The note is dropped and written to the keeper's diagnostic log, and the record is stale. | The next `up` or `status` reads the process table. | None |

## Port ledger

Ports are the `ports` map of the workspace record. A port is free or held. `.ports.lock` guards allocation (`Runs::assign_ports`, `multiverse/src/runs.rs`).

Four functions change the map. `Runs::assign_ports` allocates and drops. `ports_after_add` (`multiverse/src/workspaces.rs`) keeps and renames ports during `add`, and takes no port lock, because it allocates nothing. `Runs::restored_ports` replaces ports that are taken during a restore. `Runs::reset_ports` clears named ports and calls `assign_ports`.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Free to held | `Runs::assign_ports` | The process dies after allocation and before `Store::update`. | Nothing is written. The lock is released. | The next `up` or `mxv run` allocates again. | None |
| Held to free | `Runs::assign_ports`, when the manifest no longer names a port and no process holds it | The process dies before the write. | The old map stays. | The next `up` drops it. | None |
| Held, renamed during `add` | `ports_after_add`, `Workspaces::finish_add` | The step fails. | `undo_add` restores the record. | The earlier map is back. | None. `a_failed_add_deletes_the_new_clone_and_restores_the_record` in `multiverse/src/workspaces/multi_tests.rs` fails earlier, before `ports_after_add` runs, and asserts nothing about ports. |
| Held, replaced during restore | `Runs::restored_ports` | The process dies during the call. | No record exists yet, and the rebuild table above applies. | The rebuild table above applies. | None |

A port is free when no workspace record holds it and a bind test passes. A workspace whose record survives keeps its ports whether or not it runs.

## Reclaim mark and pin

`Workspace.reclaimed` and `Workspace.pinned` are stored in the workspace record. `Reclaimed` holds the units that ran when reclaim stopped them.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Running to reclaimed | `processes::reclaim_stop` (`mxv/src/processes.rs`) | The process dies after `stop_every` and before `Store::update`. | The units are stopped and no mark exists. | `mxv up` without names starts every unit of the manifest. The set that ran is lost. | None |
| Reclaimed to running | `Context::forget_reclaim` (`mxv/src/processes.rs`) | The process dies after the units start and before the write. | The mark stays. | The next `up` without names starts the marked set again. | None |
| Pin set or cleared | `processes::set_pinned` (`mxv/src/processes.rs`) | The write is atomic. | The old or new value is on disk. | Nothing needs recovery. | None |

## Permission grants

A grant is the file `<phase state>/grants/<repository folder>-<16 hex digits>.json`, with the schema `phase/grants/v1`. Its states are absent, recorded and wider, which is recorded again with a larger set. No enum is stored, and the file's presence and content are the state. `grants::grant` (`phase/src/grants.rs`) holds a `flock` on `<file>.lock`, reads the file again under it, takes the union, and writes through phase's own temporary file, sync, rename and directory sync. Nothing else changes a grant. `mxv remove`, `mxv archive` and `mxv restore` do not touch it, because it belongs to the repository.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Absent to recorded, or recorded to wider, on a person's yes | `Phase::grant`, called from `processes::ask_first` (`mxv/src/processes.rs`) before the workspace lock | The process dies before the rename. | The old grant, or none. | The next `up` asks again. | None |
| Same | Same | The process dies after the rename and before the build. | The new grant, with nothing built, stopped or started. | The next `up` finds no growth and starts without asking. | `a_grant_given_for_one_workspace_lets_another_workspace_of_the_repository_start` in `mxv/tests/processes.rs` plants the grant through the library and kills nothing. |
| Two grants at once | Two prompts answered yes, each through `Phase::grant` | Either holder dies. | One or both additions are on disk. | The missing addition asks again. | `two_concurrent_grants_keep_both_additions` in `phase/src/grants/tests.rs` runs two threads through the lock and kills nothing. |
| Any state to unreadable | Nothing writes it. `Phase::granted` detects a corrupt file or one from a newer phase. | Not applicable. | The file stays as it is. | `up`, `restart` and `exec` refuse with `phase::grants::unreadable` or `phase::grants::newer` until a person repairs or moves the file. | `a_corrupt_file_fails_with_its_own_code` and `a_newer_file_fails_with_its_own_code` in `phase/src/grants/tests.rs` plant the file and kill nothing. |
| Recorded to absent | A person moves the file away. | Not applicable. | Absent. | The next `up` asks about every selected host unit. | `a_scope_without_a_file_has_no_grants_and_asks_about_every_unit` in `phase/src/grants/tests.rs` plants the state and kills nothing. |

The check on `up` and `restart` stores nothing of its own. At a terminal it compares, asks and writes the grant on yes before the workspace lock. `Runs::build` then compares again under the lock, before any build step, and returns `BuildEnd::NotGranted` on growth, so a crash between the two leaves at most a grant and nothing built, stopped or started. `BuildsPassed`, which only `Runs::build` makes, is the only way into `Runs::up` and `processes::replace`. `a_unit_whose_permissions_no_person_granted_runs_no_build_and_asks_for_no_place` in `multiverse/src/runs/phased/build/tests.rs` and `a_manifest_that_widens_a_running_unit_is_refused_without_a_terminal_and_changes_nothing` in `mxv/tests/sandbox.rs` check that order.

`mxv down` with names adds no stored state. `Runs::down` computes the kept set from the status rows and the run record's `apps[].depends` while mxv holds the workspace lock, before `Phase::down`. A crash before `Phase::down` stops nothing, and a crash during it leaves the kept units running while the run record's `Stopping` row applies to the rest. `forget_reclaim` receives only the units that stopped, so a kept unit stays in the reclaim mark. `down_keeps_a_named_unit_that_a_running_unit_needs_and_stops_one_that_nothing_needs` in `mxv/tests/processes.rs` checks the kept set and kills nothing.

## Clean

`mxv clean` keeps no state, and `mxv reclaim --need disk` runs the same clean through `clean::run`. `clean_one` (`mxv/src/clean.rs`) holds the workspace lock, and `Workspaces::clean` (`multiverse/src/disk.rs`) measures again and calls `undeletable`. Before the pick, `hold_manifest_paths` keeps each path that only a repository's `mxv.toml` declares disposable, unless `clean --yes --disposable`, `reclaim --disposable` or a confirmed prompt lets the run delete it.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Disposable directory to deleted | `clean_one`, `disk::tree::remove` (`multiverse/src/disk/tree.rs`) | Deletion fails or the process dies partway. | The directory is partly deleted. | The next `mxv clean` or `mxv reclaim --need disk` deletes the rest. | `a_path_that_cannot_be_deleted_in_full_fails_its_workspace_and_names_the_entry` in `mxv/tests/disk.rs` fails the step with an unwritable directory. |

## `mxv run`

`mxv run` keeps no state of its own. `Runs::prepare_task` (`multiverse/src/runs/phased/task.rs`) gives the workspace its ports under the workspace lock, as `up` does, and the lock ends before the task starts. The task runs through `Phase::exec`, which writes nothing to the run record, so `status`, `down` and `sweep` cannot find a running task. `archive`, `clean` and `reclaim` see it through the process table, as a process inside the workspace.

| Transition | Owner | Crash point | State left | Recovery | Fault test |
| --- | --- | --- | --- | --- | --- |
| Task running | `PreparedTask::run` (`multiverse/src/runs/phased/task.rs`) | `SIGINT` reaches `mxv` alone. | The task runs on and `mxv` waits for it. | `mxv` exits with the task's code. | `sigint_to_mxv_alone_leaves_the_task_running_and_mxv_ends_with_its_exit_code` in `mxv/tests/run.rs` sends the signal. |
| Task running | `PreparedTask::run` | `SIGTERM` or `SIGKILL` ends `mxv`. | The task runs on, and deadband frees its place, so the machine's job limiter no longer counts it. | Nothing recovers it. No record names the task. | None |

## Reports and configuration values

These enums have no stored state. Each is one of the following.

Computed afresh on each read:

- `multiverse::ProcessState` (`multiverse/src/runs.rs`) is mapped from phase's state on each status read.
- `phase::ProcessState` and `WorkspaceState` (`phase/src/status.rs`) are derived from the run record and the process table.
- `ContainerState` (`phase/src/engine/container/objects.rs`) is parsed from docker's answer.
- `Identity` (`phase/src/record/process.rs`) is computed when a process is examined.
- `CleanState` (`mxv/src/clean.rs`), `ReclaimState` and `Verdict` (`mxv/src/reclaim.rs`), and `BranchState`, `StateName` and `DirectoryFate` (`mxv/src/rows.rs`) are report rows.
- `Leftover` (`multiverse/src/workspaces.rs`) is computed on each `add`.

Results of one call:

- `UpOutcome` and `DownOutcome` (`multiverse/src/runs.rs`).
- `NewOutcome`, `AddOutcome`, `Fate`, `BranchFate`, `RemovalOutcome`, `RemovalPlan`, `CloneRefusal`, `Entry` and `Making` (`multiverse/src/workspaces.rs`).
- `Created` (`multiverse/src/store.rs`).
- `ArchivePlanned`, `ArchiveRefusal` (`multiverse/src/archive/admit.rs`), `ArchiveOutcome` (`multiverse/src/archive/retire.rs`), `RestorePlanned`, `RestoreRefusal` and `RestoreOutcome` (`multiverse/src/archive/restore.rs`).
- `SweepOutcome` (`phase/src/record.rs`), `AppOutcome` (`phase/src/report.rs`), and `AppStep`, `Step`, `Recorded` and `Begun` (`phase/src/engine/run.rs`).
- `KeeperExit` (`phase/src/engine/keeper.rs`) is an exit code.

Configuration values with no transitions:

- `Mode` (`multiverse/src/workspaces.rs`) selects a dry run or a change.
- `Need` (`mxv/src/reclaim.rs`) is parsed from the command line.
- `WorkspaceKind` (`multiverse/src/record.rs`) is set at creation and never changes.
- `Exit`, `DependencyRun` and `VolumeLifetime` (`phase/src/record/run.rs`) are stored and have no transitions.
