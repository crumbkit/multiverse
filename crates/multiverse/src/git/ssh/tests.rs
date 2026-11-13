use std::ffi::OsStr;
use std::path::Path;

use super::{git_command, refused};
use crate::error::{Error, SshRefusal};

const ORIGIN: &str = "ssh://git@example.invalid/acme/app.git";

#[test]
fn a_permission_denied_line_from_ssh_is_an_authentication_failure() {
    let stderr = "git@example.invalid: Permission denied (publickey).\n\
                  fatal: Could not read from remote repository.\n\n\
                  Please make sure you have the correct access rights\n\
                  and the repository exists.\n";
    let error = refused(ORIGIN, stderr).unwrap();
    let Error::SshRefused {
        remote,
        host,
        user,
        port,
        refusal,
        message,
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(refusal, SshRefusal::AuthenticationFailed);
    assert_eq!(remote, ORIGIN);
    assert_eq!(host, "example.invalid");
    assert_eq!(user.as_deref(), Some("git"));
    assert_eq!(port, None);
    assert_eq!(
        message,
        "git@example.invalid: Permission denied (publickey)."
    );
}

#[test]
fn a_host_key_verification_line_is_an_unverified_host_key() {
    let stderr = "Host key verification failed.\r\n\
                  fatal: Could not read from remote repository.\n";
    let error = refused("deploy@Git.Example.com:team/app.git", stderr).unwrap();
    let Error::SshRefused {
        host,
        user,
        refusal,
        message,
        ..
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(refusal, SshRefusal::HostKeyUnverified);
    assert_eq!(host, "git.example.com");
    assert_eq!(user.as_deref(), Some("deploy"));
    assert_eq!(message, "Host key verification failed.");
}

#[test]
fn a_changed_host_key_warning_is_a_changed_host_key_and_not_an_unverified_one() {
    let stderr = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                  @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\n\
                  @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\n\
                  IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\n\
                  Host key for [git.example.com]:2222 has changed and you have requested strict checking.\n\
                  Host key verification failed.\n\
                  fatal: Could not read from remote repository.\n";
    let error = refused("ssh://git@git.example.com:2222/team/app.git", stderr).unwrap();
    let Error::SshRefused {
        host,
        port,
        refusal,
        message,
        ..
    } = error
    else {
        panic!("{error:?}");
    };
    assert_eq!(refusal, SshRefusal::HostKeyChanged);
    assert_eq!(host, "git.example.com");
    assert_eq!(port, Some(2222));
    assert_eq!(message, "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!");
}

#[test]
fn a_fetch_failure_without_an_ssh_line_is_not_an_ssh_refusal() {
    let stderr = "ssh: Could not resolve hostname example.invalid: nodename nor servname provided, or not known\n\
                  fatal: Could not read from remote repository.\n";
    assert!(refused(ORIGIN, stderr).is_none());
}

#[test]
fn an_ssh_line_from_a_remote_that_is_not_ssh_is_not_an_ssh_refusal() {
    let stderr = "remote: Permission denied (publickey).\n\
                  fatal: unable to access 'https://example.invalid/acme/app.git/': The requested URL returned error: 403\n";
    assert!(refused("https://example.invalid/acme/app.git", stderr).is_none());
    assert!(refused("/srv/git/app.git", "Host key verification failed.\n").is_none());
    assert!(refused("file:///srv/git/app.git", "Host key verification failed.\n").is_none());
}

#[test]
fn an_ssh_refusal_keeps_the_user_of_the_remote_and_drops_its_password() {
    let error = refused(
        "ssh://git:hunter2@example.invalid/acme/app.git",
        "Host key verification failed.\n",
    )
    .unwrap();
    let Error::SshRefused { remote, .. } = error else {
        panic!("{error:?}");
    };
    assert_eq!(remote, ORIGIN);
}

#[test]
fn every_git_command_carries_the_settings_that_stop_ssh_asking() {
    let command = git_command(Path::new("/srv/app"));
    let envs: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
    for (name, value) in [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("SSH_ASKPASS_REQUIRE", "force"),
        ("SSH_ASKPASS", "false"),
        ("LC_ALL", "C"),
    ] {
        assert!(
            envs.contains(&(OsStr::new(name), Some(OsStr::new(value)))),
            "{name}={value} is missing from {envs:?}"
        );
    }
    assert_eq!(command.get_program(), "git");
    let args: Vec<&OsStr> = command.get_args().collect();
    assert_eq!(args, [OsStr::new("-C"), OsStr::new("/srv/app")]);
}
