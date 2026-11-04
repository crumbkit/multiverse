use std::path::Path;

use crate::error::Error;
use crate::manifest::Manifest;
use crate::template::{Found, Template, find};
use crate::testing::write;

const APP: &str = "ports = [\"web\"]\n\n[processes.web]\nargv = [\"npm\", \"run\", \"dev\"]\nports = { PORT = \"web\" }\n\n[template]\norigins = [\"github.com/acme/app\"]\n\n[app]\nname = \"app\"\n";

fn templates(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        write(dir.path().join(name), text);
    }
    dir
}

fn search(dir: &Path, url: &str) -> Result<Found, Error> {
    find(
        dir,
        url,
        Path::new("/home/someone/app"),
        Path::new("/nonexistent/mxv"),
    )
}

fn found(dir: &Path, url: &str) -> Result<Option<Template>, Error> {
    search(dir, url).map(|found| found.template)
}

#[test]
fn a_template_matches_the_https_and_ssh_forms_of_its_origin() {
    let dir = templates(&[("app.toml", APP)]);
    for url in [
        "https://github.com/acme/app.git",
        "git@github.com:acme/app",
        "ssh://git@github.com:22/acme/app",
    ] {
        let template = found(dir.path(), url).unwrap().unwrap();
        assert_eq!(template.path, dir.path().join("app.toml"), "{url}");
    }
    assert_eq!(
        found(dir.path(), "https://github.com/acme/other").unwrap(),
        None
    );
}

#[test]
fn the_copied_text_has_no_template_table_and_keeps_the_rest_untouched() {
    let dir = templates(&[("app.toml", APP)]);
    let template = found(dir.path(), "https://github.com/acme/app")
        .unwrap()
        .unwrap();
    assert!(!template.text.contains("[template]"), "{}", template.text);
    assert!(!template.text.contains("origins"), "{}", template.text);
    assert!(
        template.text.contains("[app]\nname = \"app\"\n"),
        "{}",
        template.text
    );
    assert!(
        template.text.starts_with("ports = [\"web\"]\n"),
        "{}",
        template.text
    );
    let manifest = Manifest::parse(&template.text, Path::new("/x/mxv.toml")).unwrap();
    assert_eq!(manifest.ports, ["web"]);
}

#[test]
fn a_missing_directory_holds_no_template_and_other_files_are_ignored() {
    assert_eq!(
        found(
            Path::new("/nonexistent/templates"),
            "https://github.com/acme/app"
        )
        .unwrap(),
        None
    );
    let dir = templates(&[("notes.md", "not a template")]);
    assert_eq!(
        found(dir.path(), "https://github.com/acme/app").unwrap(),
        None
    );
}

#[test]
fn two_templates_that_list_one_origin_fail_as_ambiguous_and_name_both() {
    let other = APP.replace("github.com/acme/app", "git@github.com:acme/app.git");
    let dir = templates(&[("a.toml", APP), ("b.toml", &other)]);
    let error = found(dir.path(), "https://someone:token@github.com/acme/app").unwrap_err();
    let Error::TemplateAmbiguous { origin, templates } = error else {
        panic!("{error:?}");
    };
    assert_eq!(origin, "https://github.com/acme/app");
    assert_eq!(
        templates,
        [dir.path().join("a.toml"), dir.path().join("b.toml")]
    );
}

#[test]
fn a_malformed_template_that_lists_the_origin_fails_as_invalid_and_names_its_file() {
    for (text, reason) in [
        (
            "[template]\norigins = [\"github.com/acme/app\"]\nports = [\"web\"]\n",
            "a key after the header",
        ),
        (
            "template = { origins = [\"github.com/acme/app\"] }\n",
            "an inline table",
        ),
        (
            "[template]\norigins = [\"github.com/acme/app\"]\n[processes.web]\nargv = \"npm\"\n",
            "an invalid manifest",
        ),
    ] {
        let dir = templates(&[("app.toml", text)]);
        let error = found(dir.path(), "https://github.com/acme/app").unwrap_err();
        assert!(
            matches!(&error, Error::TemplateInvalid { path, .. } if *path == dir.path().join("app.toml")),
            "{reason}: {error:?}"
        );
    }
}

#[test]
fn a_malformed_template_that_does_not_list_the_origin_is_skipped_with_a_warning() {
    for (text, reason) in [
        ("ports = [\"web\"]\n", "no [template] table"),
        ("[template]\norigins = []\n", "no origins"),
        (
            "[template]\norigins = \"github.com/acme/app\"\n",
            "origins not a list",
        ),
        ("ports = \n", "not TOML"),
        (
            "[template]\norigins = [\"github.com/acme/other\"]\nports = [\"web\"]\n",
            "another origin",
        ),
    ] {
        let dir = templates(&[("a.toml", text), ("b.toml", APP)]);
        let found = search(dir.path(), "https://github.com/acme/app").unwrap();
        assert_eq!(
            found.template.map(|template| template.path),
            Some(dir.path().join("b.toml")),
            "{reason}"
        );
        assert_eq!(found.skipped.len(), 1, "{reason}: {:?}", found.skipped);
        assert!(
            found.skipped[0].starts_with(&format!(
                "mxv skipped the template {}",
                dir.path().join("a.toml").display()
            )),
            "{reason}: {:?}",
            found.skipped
        );
    }
}

#[test]
fn a_template_that_is_a_symbolic_link_is_never_copied() {
    let dir = templates(&[("real.txt", APP)]);
    std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("app.toml")).unwrap();
    let error = found(dir.path(), "https://github.com/acme/app").unwrap_err();
    assert!(
        matches!(&error, Error::TemplateInvalid { message, .. } if message.contains("symbolic link")),
        "{error:?}"
    );
    let found = search(dir.path(), "https://github.com/acme/other").unwrap();
    assert_eq!(found.template, None);
    assert_eq!(found.skipped.len(), 1, "{:?}", found.skipped);
}

#[test]
fn a_template_under_the_root_of_the_workspaces_is_never_copied() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("app/templates");
    std::fs::create_dir_all(&dir).unwrap();
    write(dir.join("app.toml"), APP);
    let error = find(
        &dir,
        "https://github.com/acme/app",
        Path::new("/home/someone/app"),
        root.path(),
    )
    .unwrap_err();
    assert!(
        matches!(&error, Error::TemplateInvalid { message, .. } if message.contains("root of the workspaces")),
        "{error:?}"
    );
}

#[test]
fn the_copied_text_keeps_the_comments_of_the_template_table() {
    let text = "ports = [\"web\"]\n\n[template]\n# Only for the app.\norigins = [\"github.com/acme/app\"]\n\n# The web server.\n[processes.web]\nargv = [\"true\"]\n";
    let dir = templates(&[("app.toml", text)]);
    let template = found(dir.path(), "https://github.com/acme/app")
        .unwrap()
        .unwrap();
    assert_eq!(
        template.text,
        "ports = [\"web\"]\n\n# Only for the app.\n\n# The web server.\n[processes.web]\nargv = [\"true\"]\n"
    );
}

#[test]
fn digits_after_the_colon_of_an_origin_without_a_scheme_are_a_port() {
    let text = APP.replace("github.com/acme/app", "github.com:443/acme/app");
    let dir = templates(&[("app.toml", &text)]);
    assert!(
        found(dir.path(), "https://github.com/acme/app")
            .unwrap()
            .is_some()
    );
    let scp = APP.replace("github.com/acme/app", "github.com:acme/app");
    let dir = templates(&[("app.toml", &scp)]);
    assert!(
        found(dir.path(), "git@github.com:acme/app")
            .unwrap()
            .is_some()
    );
}
