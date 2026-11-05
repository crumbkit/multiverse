//! Tests of the volume-owner step that need no Docker daemon: the choice
//! of the owner, the helpers' arguments and labels, and the reading of
//! what `id` printed. The tests that run the helpers against a real
//! daemon are in the runner's tests.

use std::path::PathBuf;

use super::{HELPER_IMAGE, RunAs, Wanted, chown_args, helper_of, id_args, parse_id, wanted};
use crate::engine::container::names::{Identity, Scope, VolumeKey};
use crate::error::VolumeOwnerProblem;
use crate::name::{AppName, WorkspaceId};
use crate::plan::{AppPlan, RunnerKind};
use crate::record::run::VolumeOwner;
use crate::source::Source;

fn db(owner: Option<&str>, user: Option<&str>) -> AppPlan {
    let mut app = AppPlan::new(
        "db".parse().unwrap(),
        RunnerKind::Container,
        Source::image("postgres:17".parse().unwrap()),
    );
    app.container.owner = owner.map(str::to_owned);
    app.container.user = user.map(str::to_owned);
    app
}

fn owner(text: &str) -> Wanted {
    Wanted::Owner(text.parse::<VolumeOwner>().unwrap())
}

fn scope() -> Scope {
    let app: AppName = "db".parse().unwrap();
    Scope::app(
        Identity {
            id: "0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21".parse().unwrap(),
            run_record: PathBuf::from("/state/runs/0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21.json"),
        },
        &app,
    )
}

#[test]
fn the_volume_owner_is_the_plans_owner_then_the_run_as_user_then_the_images_user() {
    assert_eq!(
        wanted(&db(Some("999:999"), Some("1000:1000")), "70:70"),
        owner("999:999")
    );
    assert_eq!(
        wanted(&db(None, Some("1000:1000")), "70:70"),
        owner("1000:1000")
    );
    assert_eq!(wanted(&db(None, None), "70:70"), owner("70:70"));
    assert_eq!(wanted(&db(None, None), "70"), owner("70"));
}

#[test]
fn keep_and_a_run_as_user_or_image_user_of_root_leave_the_volumes_alone() {
    assert_eq!(
        wanted(&db(Some("keep"), Some("1000:1000")), "70"),
        Wanted::Skip
    );
    for user in ["", "0", "0:0", "00", "root", "root:root"] {
        assert_eq!(wanted(&db(None, Some(user)), "70"), Wanted::Skip, "{user}");
        assert_eq!(wanted(&db(None, None), user), Wanted::Skip, "{user}");
    }
}

#[test]
fn a_plans_owner_of_root_gives_the_volumes_to_root() {
    assert_eq!(
        wanted(&db(Some("0:0"), Some("1000:1000")), "70"),
        owner("0:0")
    );
    assert_eq!(wanted(&db(Some("0"), None), "70"), owner("0"));
}

#[test]
fn a_plans_owner_of_root_with_another_group_still_sets_the_group() {
    assert_eq!(wanted(&db(Some("0:70"), None), ""), owner("0:70"));
}

#[test]
fn a_user_that_is_a_name_is_resolved_in_the_image() {
    assert_eq!(
        wanted(&db(None, None), "postgres"),
        Wanted::Resolve(RunAs {
            user: "postgres".to_owned(),
            from_image: true,
        })
    );
    assert_eq!(
        wanted(&db(None, Some("1000:staff")), "postgres"),
        Wanted::Resolve(RunAs {
            user: "1000:staff".to_owned(),
            from_image: false,
        })
    );
}

#[test]
fn an_error_about_the_images_user_names_its_source_and_leaves_out_its_text() {
    let image_user = "ev\nil\u{1b}[31m";
    let Wanted::Resolve(run_as) = wanted(&db(None, None), image_user) else {
        panic!("the image's user should need resolving");
    };
    for problem in [
        VolumeOwnerProblem::ResolvesToRoot,
        VolumeOwnerProblem::IdFailed { status: 3 },
        VolumeOwnerProblem::IdUnreadable,
    ] {
        let message = run_as
            .clone()
            .error("db".parse().unwrap(), problem)
            .to_string();

        assert!(message.contains("the image's USER"), "{message}");
        assert!(!message.contains('\n'), "{message:?}");
        assert!(!message.contains('\u{1b}'), "{message:?}");
        assert!(!message.contains("[31m"), "{message:?}");
    }
}

#[test]
fn an_error_about_the_plans_run_as_user_quotes_it() {
    let Wanted::Resolve(run_as) = wanted(&db(None, Some("ev\nil")), "") else {
        panic!("the run-as user should need resolving");
    };

    let message = run_as
        .error("db".parse().unwrap(), VolumeOwnerProblem::ResolvesToRoot)
        .to_string();

    assert!(message.contains(r#"the user "ev\nil" that"#), "{message}");
    assert!(!message.contains('\n'), "{message:?}");
}

#[test]
fn the_chown_helper_is_pinned_by_digest_and_runs_with_only_the_chown_capability_and_its_volume() {
    assert!(
        HELPER_IMAGE.starts_with("busybox@sha256:"),
        "{HELPER_IMAGE}"
    );
    assert_eq!(HELPER_IMAGE.len(), "busybox@sha256:".len() + 64);
    let key = VolumeKey::new("data").unwrap();

    let args = chown_args(&scope(), &key, &"1000:1000".parse().unwrap());

    let joined = args.join(" ");
    for wanted in [
        "create --pull never",
        "--network none",
        "--cap-drop ALL --cap-add CHOWN",
        "--security-opt no-new-privileges",
        "--read-only",
        "--user 0:0",
        "--entrypoint chown",
        "--label mxv.role=volume-owner",
        "--label mxv.volume=data",
        "--label mxv.app=db",
        "--label mxv.workspace-id=0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21",
    ] {
        assert!(joined.contains(wanted), "{wanted} in {joined}");
    }
    assert!(!joined.contains("DAC_OVERRIDE"), "{joined}");
    assert!(!joined.contains("-R"), "{joined}");
    let mounts: Vec<&String> = args
        .iter()
        .zip(args.iter().skip(1))
        .filter(|(flag, _)| *flag == "--volume")
        .map(|(_, mount)| mount)
        .collect();
    let volume = scope().volume_name(&key);
    assert_eq!(mounts, [&format!("{volume}:/volume")]);
    assert_eq!(
        args.iter().rev().take(4).collect::<Vec<_>>(),
        ["/volume", "1000:1000", HELPER_IMAGE, "--"]
    );
}

#[test]
fn the_id_helper_runs_in_the_apps_image_as_its_user_with_no_capability_and_no_volume() {
    let args = id_args(&scope(), "postgres:17", "postgres", "-u");

    let joined = args.join(" ");
    for wanted in [
        "--network none",
        "--cap-drop ALL",
        "--security-opt no-new-privileges",
        "--read-only",
        "--user postgres",
        "--entrypoint id",
        "--label mxv.role=volume-owner",
    ] {
        assert!(joined.contains(wanted), "{wanted} in {joined}");
    }
    assert!(!joined.contains("--cap-add"), "{joined}");
    assert!(!joined.contains("--volume"), "{joined}");
    assert!(!joined.contains("mxv.volume"), "{joined}");
    assert_eq!(
        args.iter().rev().take(3).collect::<Vec<_>>(),
        ["-u", "postgres:17", "--"]
    );
}

#[test]
fn what_id_printed_counts_only_when_it_is_one_number_on_one_line() {
    assert_eq!(parse_id("1000\n"), Some(1000));
    assert_eq!(parse_id("0\n"), Some(0));
    assert_eq!(parse_id("1000"), Some(1000));
    for printed in [
        "",
        "\n",
        "password=made-up-1\n",
        "1000\n1000\n",
        " 1000\n",
        "+1000\n",
        "4294967296\n",
        "1000 password=made-up-1\n",
    ] {
        assert_eq!(parse_id(printed), None, "{printed:?}");
    }
}

#[test]
fn a_leftover_helper_is_one_whose_labels_name_the_workspace_the_role_and_the_app() {
    let workspace: WorkspaceId = "0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21".parse().unwrap();
    let app: AppName = "db".parse().unwrap();
    let of = |labels: &str| helper_of(&workspace, Some(&app), labels);
    let own = r#"{"mxv.workspace-id":"0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21","mxv.role":"volume-owner","mxv.app":"db","mxv.volume":"data"}"#;
    assert_eq!(of(own), Some(Some(VolumeKey::new("data").unwrap())));
    let id_helper = r#"{"mxv.workspace-id":"0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21","mxv.role":"volume-owner","mxv.app":"db"}"#;
    assert_eq!(of(id_helper), Some(None));
    for foreign in [
        r#"{"mxv.workspace-id":"0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e22","mxv.role":"volume-owner","mxv.app":"db"}"#,
        r#"{"mxv.workspace-id":"0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21","mxv.role":"volume-owner ","mxv.app":"db"}"#,
        r#"{"mxv.workspace-id":"0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21","mxv.app":"db","mxv.container":"server"}"#,
        r#"{"mxv.workspace-id":"0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21","mxv.role":"volume-owner","mxv.app":"web"}"#,
        "null",
        "not json",
    ] {
        assert_eq!(of(foreign), None, "{foreign}");
    }
    let web = r#"{"mxv.workspace-id":"0b9f3c8e6a1d4f7b9c2e5d8a7b6c4e21","mxv.role":"volume-owner","mxv.app":"web"}"#;
    assert_eq!(helper_of(&workspace, None, web), Some(None));
}
