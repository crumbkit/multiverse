use std::path::Path;

use crate::error::Error;
use crate::manifest::Manifest;
use crate::template::find;
use crate::testing::write;

const APP: &str = "ports = [\"web\"]\n\n[processes.web]\nargv = [\"npm\", \"run\", \"dev\"]\nports = { PORT = \"web\" }\n\n[template]\norigins = [\"github.com/acme/app\"]\n\n[app]\nname = \"app\"\n";

fn templates(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, text) in files {
        write(dir.path().join(name), text);
    }
    dir
}

fn found(dir: &Path, url: &str) -> Result<Option<crate::template::Template>, Error> {
    find(dir, url, Path::new("/home/someone/app"))
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
fn a_malformed_template_fails_as_invalid_and_names_its_file() {
    for (text, reason) in [
        ("ports = [\"web\"]\n", "no [template] table"),
        ("[template]\norigins = []\n", "no origins"),
        (
            "[template]\norigins = \"github.com/acme/app\"\n",
            "origins not a list",
        ),
        (
            "[template]\norigins = [\"github.com/acme/app\"]\nports = [\"web\"]\n",
            "a key after the header",
        ),
        (
            "template = { origins = [\"github.com/acme/app\"] }\n",
            "an inline table",
        ),
        ("ports = \n", "not TOML"),
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
