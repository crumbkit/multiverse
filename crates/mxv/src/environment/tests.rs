use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::{Bases, Environment, RootSource, VARIABLES};
use crate::failures::MxvFailure;

/// Base directories under a temporary home directory.
struct Home {
    dir: TempDir,
}

impl Home {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn bases(&self) -> Bases {
        let home = self.path();
        Bases {
            config: home.join(".config"),
            state: home.join(".local/state"),
            data: home.join(".local/share"),
            home,
        }
    }

    /// Writes `text` as the config file.
    fn config(&self, text: &str) -> PathBuf {
        let path = self.path().join(".config/mxv/config.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::testing::write(&path, text);
        path
    }

    fn environment(&self, root: Option<&str>) -> Environment {
        Environment::read(
            |name| {
                (name == "MXV_ROOT")
                    .then(|| root.map(OsString::from))
                    .flatten()
            },
            Some(self.bases()),
        )
    }
}

#[test]
fn the_snapshot_reads_mxv_root_and_the_catalog_names_every_variable_that_mxv_depends_on() {
    let mut read = Vec::new();
    let _ = Environment::read(
        |name| {
            read.push(name.to_owned());
            None
        },
        None,
    );
    assert_eq!(read, ["MXV_ROOT"]);
    let catalog: Vec<&str> = VARIABLES.iter().map(|variable| variable.name).collect();
    assert_eq!(
        catalog,
        [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_STATE_HOME",
            "MXV_ROOT"
        ]
    );
}

#[test]
fn the_records_live_in_the_state_directory() {
    let home = Home::new();
    assert_eq!(
        home.environment(None).records().unwrap(),
        home.path().join(".local/state/mxv/workspaces")
    );
}

#[test]
fn the_config_file_lives_in_the_config_directory() {
    let home = Home::new();
    assert_eq!(
        home.environment(None).config_file().unwrap(),
        home.path().join(".config/mxv/config.toml")
    );
}

#[test]
fn without_mxv_root_or_a_config_file_the_root_is_in_the_data_directory() {
    let home = Home::new();
    let root = home.environment(None).root().unwrap();
    assert_eq!(root.path, home.path().join(".local/share/mxv/workspaces"));
    assert_eq!(root.source, RootSource::Default);
}

#[test]
fn a_config_file_without_a_root_leaves_the_default_root() {
    let home = Home::new();
    home.config("# no root yet\n");
    let root = home.environment(None).root().unwrap();
    assert_eq!(root.source, RootSource::Default);
}

#[test]
fn the_config_file_root_wins_over_the_default() {
    let home = Home::new();
    home.config("root = \"~/work\"\n");
    let root = home.environment(None).root().unwrap();
    assert_eq!(root.path, home.path().join("work"));
    assert_eq!(root.source, RootSource::Config);
}

#[test]
fn mxv_root_wins_over_the_config_file() {
    let home = Home::new();
    home.config("root = \"~/work\"\n");
    let root = home.environment(Some("/srv/mxv")).root().unwrap();
    assert_eq!(root.path, PathBuf::from("/srv/mxv"));
    assert_eq!(root.source, RootSource::Env);
}

#[test]
fn an_empty_mxv_root_counts_as_unset() {
    let home = Home::new();
    let root = home.environment(Some("")).root().unwrap();
    assert_eq!(root.source, RootSource::Default);
}

#[test]
fn mxv_root_wins_over_a_config_file_that_is_invalid() {
    let home = Home::new();
    home.config("roots = 1\n");
    let root = home.environment(Some("/srv/mxv")).root().unwrap();
    assert_eq!(root.source, RootSource::Env);
}

#[test]
fn an_invalid_config_file_is_refused_with_its_path_and_line() {
    let home = Home::new();
    let path = home.config("root = \"/srv\"\ncolour = \"blue\"\n");
    match home.environment(None).root() {
        Err(MxvFailure::BadConfig {
            path: found, line, ..
        }) => {
            assert_eq!(found, path);
            assert_eq!(line, Some(2));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_relative_mxv_root_is_refused_without_blaming_home() {
    let home = Home::new();
    let error = home.environment(Some("work")).root().unwrap_err();
    assert!(
        matches!(&error, MxvFailure::RelativeRoot { value } if value == Path::new("work")),
        "{error:?}"
    );
    assert!(!error.to_string().contains("HOME"), "{error}");
}

#[test]
fn without_a_home_directory_the_records_and_the_default_root_are_refused() {
    let environment = Environment::read(|_| None, None);
    assert!(matches!(environment.records(), Err(MxvFailure::NoHome)));
    assert!(matches!(environment.root(), Err(MxvFailure::NoHome)));
    let environment = Environment::read(|_| Some(OsString::from("/srv/mxv")), None);
    assert_eq!(environment.root().unwrap().path, PathBuf::from("/srv/mxv"));
}
