use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;

use crumbkit::Failure as _;
use multiverse::phase::{Grant, Grants, Growth, Permissions};
use multiverse::{Manifest, Reclaimed, Runs, Store, Workspace, WorkspaceName, WorkspaceState};

use super::{Context, answer, ask_person, question, selection};
use crate::failures::WORKSPACE_NOT_FOUND;
use crate::testing::write;

/// Returns what a new sandboxed unit `api` that writes `target`, and a new
/// unit `worker` that runs outside the sandbox, ask for.
fn growth() -> Growth {
    let mut api = Permissions::default();
    api.write.insert("target".to_owned());
    Grants::default().growth(&BTreeMap::from([
        ("api".parse().unwrap(), Grant::Sandboxed(api)),
        ("worker".parse().unwrap(), Grant::Unsandboxed),
    ]))
}

/// Returns what `answer` reports for the reply `reply`, and what it wrote.
fn answered(reply: &str) -> (bool, String) {
    let mut written = Vec::new();
    let granted = answer("Grant them? [y/N] ", &mut written, &mut reply.as_bytes());
    (granted, String::from_utf8(written).unwrap())
}

#[test]
fn the_question_names_the_repository_and_each_unit_in_plain_words_and_holds_no_hash() {
    let text = question("multiverse", &[PathBuf::from("/src/multiverse")], &growth());

    assert_eq!(
        text,
        "Repository multiverse (/src/multiverse) asks for these permissions:\n\
         \x20 api, a new unit:\n\
         \x20   write to target in its checkout\n\
         \x20 worker, a new unit:\n\
         \x20   run outside the sandbox, with everything your account can reach\n\
         Grant them? [y/N] "
    );
    assert!(
        !text
            .split_whitespace()
            .any(|word| word.len() >= 16 && word.chars().all(|c| c.is_ascii_hexdigit())),
        "{text}"
    );
}

#[test]
fn yes_in_any_case_grants_and_writes_the_question() {
    assert_eq!(answered("y\n"), (true, "Grant them? [y/N] ".to_owned()));
    assert!(answered("  YES \n").0);
}

#[test]
fn any_other_reply_or_no_reply_grants_nothing() {
    assert!(!answered("n\n").0);
    assert!(!answered("\n").0);
    assert!(!answered("").0);
    assert!(!answered("sure\n").0);
}

/// Returns a workspace that `mxv reclaim` marked as running `reclaimed`,
/// and a manifest that declares the units `store`, `api` and `web`.
fn reclaimed(reclaimed: &[&str]) -> (Workspace, Manifest) {
    let mut workspace = Workspace::new(
        "triage".parse().unwrap(),
        PathBuf::from("/nowhere/triage"),
        Vec::new(),
    );
    workspace.reclaimed = Some(Reclaimed::new(
        reclaimed.iter().map(|name| (*name).to_owned()).collect(),
    ));
    let manifest = Manifest::parse(
        "[processes.store]\nargv = [\"true\"]\n\n\
         [processes.api]\nargv = [\"true\"]\n\n\
         [processes.web]\nargv = [\"true\"]\n",
        Path::new("mxv.toml"),
    )
    .unwrap();
    (workspace, manifest)
}

#[test]
fn an_empty_selection_after_reclaim_is_the_marked_units_that_the_manifest_still_declares() {
    let (workspace, manifest) = reclaimed(&["api", "gone"]);

    assert_eq!(selection(&workspace, &manifest, &[]), ["api"]);
}

#[test]
fn named_units_are_the_selection_whatever_the_reclaim_mark_holds() {
    let (workspace, manifest) = reclaimed(&["api"]);

    assert_eq!(
        selection(&workspace, &manifest, &["web".to_owned()]),
        ["web"]
    );
}

#[test]
fn an_empty_selection_without_a_reclaim_mark_stays_empty_so_it_means_every_unit() {
    let (mut workspace, manifest) = reclaimed(&[]);
    workspace.reclaimed = None;

    assert!(selection(&workspace, &manifest, &[]).is_empty());
}

/// A manifest whose one unit runs outside the sandbox, so `up` asks a
/// person to grant it.
const UNSANDBOXED: &str = "[processes.api]\nargv = [\"true\"]\n";

/// A state directory that holds the record of the live workspace `triage`,
/// whose directory holds [`UNSANDBOXED`] as its manifest.
struct Place {
    /// The temporary directory.
    dir: tempfile::TempDir,
}

impl Place {
    fn new() -> Self {
        let place = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        place.create();
        place
    }

    fn name() -> WorkspaceName {
        "triage".parse().unwrap()
    }

    fn store(&self) -> Store {
        Store::new(self.dir.path().join("workspaces"))
    }

    fn runs(&self) -> Runs {
        Runs::new(self.dir.path())
    }

    /// The path of the workspace's record.
    fn record(&self) -> PathBuf {
        self.store().path(&Self::name())
    }

    /// Creates the live workspace `triage`, with a new stable id, and
    /// returns that id as its record holds it.
    fn create(&self) -> String {
        let path = self.dir.path().join("triage");
        fs::create_dir_all(&path).unwrap();
        write(path.join("mxv.toml"), UNSANDBOXED);
        let mut workspace = Workspace::new(Self::name(), path, Vec::new());
        let store = self.store();
        store.create(&mut workspace).unwrap();
        store.advance(&mut workspace, WorkspaceState::Live).unwrap();
        workspace.id.unwrap().to_string()
    }

    /// Rewrites the workspace's record as an earlier mxv wrote it, without
    /// a stable id.
    fn without_id(&self) {
        let path = self.record();
        let mut record: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        record.as_object_mut().unwrap().remove("id");
        record["version"] = 2.into();
        write(&path, record.to_string());
    }

    /// Returns the context of a command that read the workspace's record
    /// now.
    fn context(&self) -> Context {
        Context {
            store: self.store(),
            runs: self.runs(),
            workspace: self.store().read(&Self::name()).unwrap().unwrap(),
            processes: Vec::new(),
        }
    }
}

/// Runs `ask_person` for `context` on another thread while this thread
/// holds the workspace's lock and runs `meanwhile`, and returns the code
/// of its failure, or `None`, with what it wrote to the person.
fn ask_after_wait(
    place: &Place,
    context: Context,
    meanwhile: impl FnOnce(),
) -> (Option<String>, String) {
    let held = place.runs().lock(&Place::name()).unwrap();
    let asker = thread::spawn(move || {
        let mut prompt = Vec::new();
        let asked = ask_person(&mut prompt, &mut "n\n".as_bytes(), &context, |_| Vec::new());
        (
            asked
                .err()
                .map(|error| error.failure().code().as_str().to_owned()),
            String::from_utf8(prompt).unwrap(),
        )
    });
    meanwhile();
    drop(held);
    asker.join().unwrap()
}

#[test]
fn a_question_whose_workspace_was_removed_while_it_waited_for_the_lock_is_not_found() {
    let place = Place::new();
    place.without_id();
    let context = place.context();

    let (code, prompt) = ask_after_wait(&place, context, || {
        fs::remove_file(place.record()).unwrap();
    });

    assert_eq!(code, Some(WORKSPACE_NOT_FOUND.as_str().to_owned()));
    assert_eq!(prompt, "");
    assert!(!place.record().exists());
}

#[test]
fn a_question_whose_workspace_stayed_while_it_waited_for_the_lock_saves_the_id_and_asks() {
    let place = Place::new();
    place.without_id();
    let context = place.context();

    let (code, prompt) = ask_after_wait(&place, context, || {});

    assert_eq!(code.as_deref(), Some("mxv::permissions::not_granted"));
    assert!(prompt.ends_with("Grant them? [y/N] "), "{prompt}");
    let saved = place.store().read(&Place::name()).unwrap().unwrap();
    assert!(saved.id.is_some());
}

/// Runs [`Context::lock`] for `context` on another thread while this
/// thread holds the workspace's lock and runs `meanwhile`, and returns
/// the code and the message of its failure, or the workspace it read.
fn lock_after_wait(
    place: &Place,
    mut context: Context,
    meanwhile: impl FnOnce(),
) -> Result<Workspace, (String, String)> {
    let held = place.runs().lock(&Place::name()).unwrap();
    let locker = thread::spawn(move || match context.lock() {
        Ok(_lock) => Ok(context.workspace),
        Err(failure) => Err((failure.code().as_str().to_owned(), failure.to_string())),
    });
    meanwhile();
    drop(held);
    locker.join().unwrap()
}

#[test]
fn a_command_whose_workspace_was_replaced_while_it_waited_for_the_lock_is_not_found_and_leaves_the_new_one()
 {
    let place = Place::new();
    let context = place.context();
    let mut replacement = String::new();
    let mut before = Vec::new();

    let locked = lock_after_wait(&place, context, || {
        // An `mxv archive` deletes the record, and an `mxv new` of the same
        // name creates another workspace with another stable id.
        fs::remove_file(place.record()).unwrap();
        replacement = place.create();
        before = fs::read(place.record()).unwrap();
    });

    let Err((code, message)) = locked else {
        panic!("the command took the replaced workspace: {locked:?}");
    };
    assert_eq!(code, WORKSPACE_NOT_FOUND.as_str().to_owned());
    assert!(message.contains(&replacement), "{message}");
    assert!(message.contains("run the command again"), "{message}");
    assert_eq!(fs::read(place.record()).unwrap(), before);
}

#[test]
fn a_command_whose_workspace_gained_a_stable_id_while_it_waited_for_the_lock_takes_it() {
    let place = Place::new();
    place.without_id();
    let context = place.context();
    assert_eq!(context.workspace.id, None);

    let locked = lock_after_wait(&place, context, || {
        // Another `mxv up` saves the id under the lock.
        let mut record = place.store().read(&Place::name()).unwrap().unwrap();
        place.store().update(&mut record).unwrap();
    });

    let workspace = locked.unwrap();
    let saved = place.store().read(&Place::name()).unwrap().unwrap();
    assert!(saved.id.is_some());
    assert_eq!(workspace.id, saved.id);
}
