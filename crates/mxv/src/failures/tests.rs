use crumbkit::{Call, Failure as _, Recovery};
use multiverse::SshRefusal;

use super::{
    MxvFailure, RESTORE_CHANGED, SSH_AUTHENTICATION_FAILED, SSH_HOST_KEY_CHANGED,
    SSH_HOST_KEY_UNVERIFIED, WORKSPACE_NOT_FOUND,
};

#[test]
fn a_record_that_is_gone_at_a_write_reads_as_a_workspace_that_is_not_found() {
    let failure = MxvFailure::from(multiverse::Error::RecordMissing {
        name: "triage".parse().unwrap(),
    });

    assert_eq!(failure.code(), &WORKSPACE_NOT_FOUND);
    assert_eq!(failure.to_string(), "no workspace is named triage");
    let Recovery::Run(call) = failure.recovery() else {
        panic!("{failure:?} offers no call");
    };
    let expected = Call::new(&["list"]);
    assert_eq!(format!("{call:?}"), format!("{expected:?}"));
}

#[test]
fn a_restore_whose_records_changed_after_the_question_offers_its_dry_run() {
    let failure = MxvFailure::RestoreChanged {
        name: "triage".to_owned(),
    };

    assert_eq!(failure.code(), &RESTORE_CHANGED);
    assert_eq!(
        failure.to_string(),
        "the records of triage changed after mxv asked, so mxv deleted nothing"
    );
    let Recovery::Run(call) = failure.recovery() else {
        panic!("{failure:?} offers no call");
    };
    let expected = Call::typed(vec![
        "restore".to_owned(),
        "triage".to_owned(),
        "--dry-run".to_owned(),
    ]);
    assert_eq!(format!("{call:?}"), format!("{expected:?}"));
}

/// Returns the failure of an ssh refusal of `refusal` by the host
/// `git.example.com` on port `port`.
fn ssh_refused(refusal: SshRefusal, port: Option<u16>) -> MxvFailure {
    MxvFailure::from(multiverse::Error::SshRefused {
        remote: "ssh://git@git.example.com/acme/app.git".to_owned(),
        login: "git@git.example.com".to_owned(),
        port,
        refusal,
        message: "Host key verification failed.".to_owned(),
    })
}

/// Returns the advice of `failure`'s recovery.
fn hint(failure: &MxvFailure) -> String {
    let Recovery::Hint(text) = failure.recovery() else {
        panic!("{failure:?} offers no advice");
    };
    text
}

#[test]
fn each_ssh_refusal_has_its_own_code_and_names_the_remote_and_the_host() {
    for (refusal, code) in [
        (SshRefusal::AuthenticationFailed, &SSH_AUTHENTICATION_FAILED),
        (SshRefusal::HostKeyUnverified, &SSH_HOST_KEY_UNVERIFIED),
        (SshRefusal::HostKeyChanged, &SSH_HOST_KEY_CHANGED),
    ] {
        let failure = ssh_refused(refusal, None);
        assert_eq!(failure.code(), code);
        let fields: Vec<String> = failure
            .fields()
            .iter()
            .map(|field| format!("{field:?}"))
            .collect();
        let fields = fields.join("\n");
        for value in [
            "ssh://git@git.example.com/acme/app.git",
            "git.example.com",
            "Host key verification failed.",
        ] {
            assert!(fields.contains(value), "{value} is missing from {fields}");
        }
        assert!(hint(&failure).contains("git.example.com"), "{failure:?}");
    }
    let unverified = hint(&ssh_refused(SshRefusal::HostKeyUnverified, Some(2222)));
    assert!(
        unverified.contains("`ssh -p 2222 git@git.example.com`"),
        "{unverified}"
    );
    let changed = hint(&ssh_refused(SshRefusal::HostKeyChanged, Some(2222)));
    assert!(
        changed.contains("`ssh-keygen -R '[git.example.com]:2222'`"),
        "{changed}"
    );
}

#[test]
fn the_recovery_of_a_changed_host_key_never_tells_the_person_to_accept_it() {
    let changed = hint(&ssh_refused(SshRefusal::HostKeyChanged, None));
    assert!(!changed.contains("yes"), "{changed}");
    assert!(changed.contains("find out why"), "{changed}");
    assert!(
        changed.contains("`ssh-keygen -R git.example.com`"),
        "{changed}"
    );
    let unverified = hint(&ssh_refused(SshRefusal::HostKeyUnverified, None));
    assert!(
        unverified.contains("compare the fingerprint"),
        "{unverified}"
    );
}

#[test]
fn a_record_whose_state_changed_at_a_write_asks_for_the_command_again() {
    let failure = MxvFailure::from(multiverse::Error::StateChanged {
        name: "triage".parse().unwrap(),
    });

    assert_eq!(failure.code(), &super::WORKSPACE_STATE_CHANGED);
    let Recovery::Hint(hint) = failure.recovery() else {
        panic!("{failure:?} offers no hint");
    };
    assert!(hint.contains("run the command again"), "{hint}");
}

#[test]
fn a_workspace_that_mxv_new_did_not_finish_names_mxv_remove_and_then_mxv_new() {
    let failure = MxvFailure::Creating {
        name: "triage".to_owned(),
    };

    assert_eq!(failure.code(), &super::WORKSPACE_CREATING);
    let Recovery::Hint(hint) = failure.recovery() else {
        panic!("{failure:?} offers no hint");
    };
    assert!(hint.contains("`mxv remove triage`"), "{hint}");
    assert!(hint.contains("`mxv new`"), "{hint}");
}

/// Returns the code and the hint of the failure of a workspace without a
/// stable id beside an archive record, which is `readable` or not.
fn archive_unfinished(readable: bool) -> (&'static crumbkit::Code, String) {
    let failure = MxvFailure::from(multiverse::Error::ArchiveUnfinished {
        name: "triage".parse().unwrap(),
        readable,
    });
    let Recovery::Hint(hint) = failure.recovery() else {
        panic!("{failure:?} offers no hint");
    };
    (failure.code(), hint)
}

#[test]
fn a_workspace_without_an_id_beside_an_older_unfinished_archive_names_mxv_archive() {
    let (code, hint) = archive_unfinished(true);
    assert_eq!(
        (code.area(), code.reason()),
        ("workspace", "archive_unfinished")
    );
    assert!(hint.contains("`mxv archive triage`"), "{hint}");
}

#[test]
fn a_workspace_without_an_id_beside_an_unreadable_archive_record_names_mxv_remove() {
    let (code, hint) = archive_unfinished(false);
    assert_eq!(
        (code.area(), code.reason()),
        ("workspace", "archive_unfinished")
    );
    assert!(hint.contains("`mxv remove triage`"), "{hint}");
    assert!(!hint.contains("`mxv archive triage`"), "{hint}");
}
