//! The names that the library writes for a program that links it: its
//! host profile.
//!
//! The library writes the name of the program that uses it into what it
//! creates for a person: the manifest file a checkout declares its apps
//! in, the prefix of the unit names a manifest may not use, the git
//! references of archives and of the copies of `origin`, the commits an
//! archive writes, the directories beside a workspace during a removal or
//! an archive, the labels of docker objects, the proxy's domain and the
//! text of a workspace's `AGENTS.md`. A [`Host`] names each of these.
//! [`Host::mxv`] holds mxv's names, which every type uses unless it is
//! given another profile, and [`Host::named`] derives every name from one
//! name, such as `example`.
//!
//! A host lists, prunes, sweeps, restores and deletes only the references,
//! directories and docker objects that carry its own names, so two hosts
//! that share a mirror or a docker daemon never touch each other's.

use std::borrow::Cow;
use std::fmt;

use phase::LabelPrefix;

use crate::record::Workspace;

#[cfg(test)]
mod tests;

/// The most bytes of a host's name. The longest docker label key that a
/// host's name starts is the name, `.` and `workspace-id`, 45 bytes, far
/// below docker's limit of 255.
pub const MOST_HOST_NAME_BYTES: usize = LabelPrefix::MOST_NAME_BYTES;

/// The most bytes of a host's manifest file name.
pub const MOST_MANIFEST_FILE_BYTES: usize = 64;

/// The most bytes of one label of a proxy domain, as DNS allows.
const MOST_DOMAIN_LABEL_BYTES: usize = 63;

/// The most bytes of a proxy domain, as DNS allows.
const MOST_DOMAIN_BYTES: usize = 253;

/// The name of a host, such as `mxv`: 1 to [`MOST_HOST_NAME_BYTES`] bytes
/// of lowercase ASCII letters, digits and `-`, starting with a letter. The
/// constructor checks it, so the name can stand in a git reference, a file
/// name and a docker label key as it is.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HostName(Cow<'static, str>);

impl HostName {
    /// Returns the host name `text`.
    ///
    /// # Errors
    ///
    /// Returns [`BadHost`] for the field `name` when `text` breaks the
    /// rule of the type.
    pub fn new(text: &str) -> Result<Self, BadHost> {
        Self::with_labels(text).map(|(name, _)| name)
    }

    /// Returns the host name `text` and the label prefix `<text>.`. A host
    /// name follows the rule of a [`LabelPrefix`]'s name, so phase's check
    /// is the only copy of the rule.
    fn with_labels(text: &str) -> Result<(Self, LabelPrefix), BadHost> {
        let labels = LabelPrefix::new(text).map_err(|_| {
            BadHost::new(
                "name",
                text,
                format!(
                    "a host name is 1 to {MOST_HOST_NAME_BYTES} bytes of lowercase letters, digits and `-`, starting with a letter"
                ),
            )
        })?;
        Ok((Self(Cow::Owned(text.to_owned())), labels))
    }

    /// Returns the name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A host profile that a constructor refused: the field, the text and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadHost {
    /// The field that holds the text, such as `name`.
    field: &'static str,
    /// The text that was refused.
    text: String,
    /// Why the text was refused.
    reason: String,
}

impl BadHost {
    /// Returns the refusal of `text` for `field`, because of `reason`.
    fn new(field: &'static str, text: &str, reason: String) -> Self {
        Self {
            field,
            text: text.to_owned(),
            reason,
        }
    }

    /// Returns the field whose text was refused: `name`, `manifest file`
    /// or `proxy domain`.
    #[must_use]
    pub const fn field(&self) -> &'static str {
        self.field
    }
}

impl fmt::Display for BadHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "the host profile's {} `{}` is refused: {}",
            self.field, self.text, self.reason
        )
    }
}

impl std::error::Error for BadHost {}

/// The names that the library writes for one program, as the module docs
/// describe. Build one with [`Host::mxv`] or [`Host::named`], and give it
/// to [`crate::Workspaces::with_host`], [`crate::Archives::with_host`],
/// [`crate::Runs::with_host`] and [`crate::Proxy::with_host`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Host {
    /// The host's name.
    name: HostName,
    /// The manifest file name, `<name>.toml` unless overridden.
    manifest_file: Cow<'static, str>,
    /// The proxy's domain, `<name>.localhost` unless overridden.
    proxy_domain: Cow<'static, str>,
    /// The prefix of the docker labels, `<name>.`.
    labels: LabelPrefix,
    /// The text of a workspace's `AGENTS.md`.
    agents_text: fn(&Workspace, &[String]) -> String,
}

impl Default for Host {
    fn default() -> Self {
        Self::mxv()
    }
}

impl Host {
    /// Returns mxv's profile, whose names are those of mxv 0.5.0 and
    /// earlier: `mxv.toml`, `mxv-`, `refs/mxv/`, `mxv.`, `mxv.localhost`
    /// and mxv's own `AGENTS.md` text.
    #[must_use]
    pub const fn mxv() -> Self {
        Self {
            name: HostName(Cow::Borrowed("mxv")),
            manifest_file: Cow::Borrowed("mxv.toml"),
            proxy_domain: Cow::Borrowed("mxv.localhost"),
            labels: LabelPrefix::mxv(),
            agents_text: crate::record::agents_text,
        }
    }

    /// Returns the profile whose every name is derived from `name`: the
    /// manifest file `<name>.toml`, the unit prefix `<name>-`, the
    /// references below `refs/<name>/`, the labels `<name>.`, the proxy
    /// domain `<name>.localhost`, and an `AGENTS.md` text that names no
    /// program's commands.
    ///
    /// # Errors
    ///
    /// Returns [`BadHost`] when `name` is not a [`HostName`].
    pub fn named(name: &str) -> Result<Self, BadHost> {
        let (name, labels) = HostName::with_labels(name)?;
        Ok(Self {
            manifest_file: Cow::Owned(format!("{name}.toml")),
            proxy_domain: Cow::Owned(format!("{name}.localhost")),
            labels,
            agents_text: generic_agents_text,
            name,
        })
    }

    /// Returns this profile with `file` as the name of the manifest file of
    /// a checkout and of a workspace.
    ///
    /// # Errors
    ///
    /// Returns [`BadHost`] when `file` is not one path component of at most
    /// [`MOST_MANIFEST_FILE_BYTES`] bytes of ASCII letters, digits, `.`,
    /// `-` and `_` that ends in `.toml` and has a name before it.
    pub fn with_manifest_file(mut self, file: &str) -> Result<Self, BadHost> {
        let valid = file.len() <= MOST_MANIFEST_FILE_BYTES
            && file.strip_suffix(".toml").is_some_and(|stem| {
                !stem.is_empty()
                    && !stem.starts_with('.')
                    && stem.chars().all(|character| {
                        character.is_ascii_alphanumeric() || "._-".contains(character)
                    })
            });
        if !valid {
            return Err(BadHost::new(
                "manifest file",
                file,
                format!(
                    "a manifest file is one file name of at most {MOST_MANIFEST_FILE_BYTES} bytes that ends in `.toml`"
                ),
            ));
        }
        self.manifest_file = Cow::Owned(file.to_owned());
        Ok(self)
    }

    /// Returns this profile with `domain` as the domain below which the
    /// proxy serves each workspace's routes.
    ///
    /// # Errors
    ///
    /// Returns [`BadHost`] when `domain` is not a DNS name below
    /// `localhost`, whose labels are 1 to 63 bytes of lowercase letters,
    /// digits and `-` that neither start nor end with `-`. Only names below
    /// `localhost` reach the loopback address without setup.
    pub fn with_proxy_domain(mut self, domain: &str) -> Result<Self, BadHost> {
        let label = |label: &str| {
            (1..=MOST_DOMAIN_LABEL_BYTES).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|character| {
                    character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
                })
        };
        let valid = domain.len() <= MOST_DOMAIN_BYTES
            && domain
                .strip_suffix(".localhost")
                .is_some_and(|below| below.split('.').all(label));
        if !valid {
            return Err(BadHost::new(
                "proxy domain",
                domain,
                "a proxy domain is a DNS name below `localhost`".to_owned(),
            ));
        }
        self.proxy_domain = Cow::Owned(domain.to_owned());
        Ok(self)
    }

    /// Returns this profile with `text` as the function that writes the
    /// text of a workspace's `AGENTS.md` from its record and the origin URL
    /// of each checkout, as [`crate::agents_text`] does for mxv.
    #[must_use]
    pub fn with_agents_text(mut self, text: fn(&Workspace, &[String]) -> String) -> Self {
        self.agents_text = text;
        self
    }

    /// Returns the host's name.
    #[must_use]
    pub const fn name(&self) -> &HostName {
        &self.name
    }

    /// Returns the name of the manifest file, such as `mxv.toml`.
    #[must_use]
    pub fn manifest_file(&self) -> &str {
        &self.manifest_file
    }

    /// Returns the prefix of the unit names that a manifest may not use,
    /// such as `mxv-`.
    #[must_use]
    pub fn unit_prefix(&self) -> String {
        format!("{}-", self.name)
    }

    /// Returns the namespace of the host's git references, such as
    /// `refs/mxv/`.
    #[must_use]
    pub fn ref_namespace(&self) -> String {
        format!("refs/{}/", self.name)
    }

    /// Returns the namespace of the references that pin an archive's
    /// commits in a mirror, such as `refs/mxv/archives/`.
    #[must_use]
    pub fn pin_namespace(&self) -> String {
        format!("refs/{}/archives/", self.name)
    }

    /// Returns the namespace of the references that keep a restored
    /// commit in the person's repository, such as `refs/mxv/kept/`.
    #[must_use]
    pub fn kept_namespace(&self) -> String {
        format!("refs/{}/kept/", self.name)
    }

    /// Returns the namespace of a clone's copies of its mirror's heads and
    /// tags, such as `refs/mxv/origin/`.
    #[must_use]
    pub fn origin_namespace(&self) -> String {
        format!("refs/{}/origin/", self.name)
    }

    /// Returns the prefix of the labels of docker objects, such as `mxv.`.
    #[must_use]
    pub const fn labels(&self) -> &LabelPrefix {
        &self.labels
    }

    /// Returns the proxy's domain, such as `mxv.localhost`.
    #[must_use]
    pub fn proxy_domain(&self) -> &str {
        &self.proxy_domain
    }

    /// Returns the name and email of the author of an archive's commit of
    /// uncommitted work when git has no identity of the person, such as
    /// `mxv` and `mxv@localhost.invalid`.
    #[must_use]
    pub fn wip_author(&self) -> (&str, String) {
        (
            self.name.as_str(),
            format!("{}@localhost.invalid", self.name),
        )
    }

    /// Returns the message of an archive's commit of uncommitted work, such
    /// as `wip: archived by mxv`.
    #[must_use]
    pub fn wip_message(&self) -> String {
        format!("wip: archived by {}", self.name)
    }

    /// Returns the suffix of a checkout's directory while a removal holds
    /// it aside, such as `.mxv-removing`.
    #[must_use]
    pub fn aside_suffix(&self) -> String {
        format!(".{}-removing", self.name)
    }

    /// Returns the suffix of a checkout's directory while an archive holds
    /// it in quarantine, such as `.mxv-archiving`.
    #[must_use]
    pub fn quarantine_suffix(&self) -> String {
        format!(".{}-archiving", self.name)
    }

    /// Returns the text of the `AGENTS.md` of `workspace`, whose checkouts'
    /// origin URLs are `origins`.
    #[must_use]
    pub fn agents_text(&self, workspace: &Workspace, origins: &[String]) -> String {
        (self.agents_text)(workspace, origins)
    }
}

/// Returns the `AGENTS.md` text of a profile that [`Host::named`] made,
/// which names the workspace and its checkouts and no program's commands.
fn generic_agents_text(workspace: &Workspace, origins: &[String]) -> String {
    let mut text = format!(
        "# Workspace {}\n\
         \n\
         This directory is a task workspace. Work inside the checkouts below,\n\
         and commit and push from each of them.\n\
         \n\
         ## Checkouts\n\
         \n",
        workspace.name
    );
    for (index, checkout) in workspace.checkouts.iter().enumerate() {
        text.push_str(&format!(
            "- `{}/` is on the branch `{}`, which started from `{}`.\n",
            checkout.name, checkout.branch, checkout.base,
        ));
        if let Some(origin) = origins.get(index) {
            text.push_str(&format!("  Its origin is `{origin}`.\n"));
        }
    }
    text.push_str("\nThis file sits above the checkouts, so git does not see it.\n");
    text
}

/// The names of a host that reading a manifest needs: the manifest file,
/// the prefix of the unit names a manifest may not use, and the proxy's
/// domain, below which no `hosts` entry may lie. A [`Workspace`] carries
/// the names of the host that read it, so its manifest methods read the
/// host's file.
///
/// Every two `ManifestNames` compare equal. A [`Workspace`] derives its
/// equality, and its record never stores these names, so the same record
/// read by stores of two hosts compares equal, as a confirmed record and a
/// fresh read of it must.
#[derive(Clone, Debug)]
pub(crate) struct ManifestNames {
    /// The manifest file name, such as `mxv.toml`.
    pub(crate) file: Cow<'static, str>,
    /// The prefix of reserved unit names, such as `mxv-`.
    pub(crate) unit_prefix: Cow<'static, str>,
    /// The proxy's domain, such as `mxv.localhost`.
    pub(crate) domain: Cow<'static, str>,
}

impl ManifestNames {
    /// mxv's names.
    pub(crate) const MXV: Self = Self {
        file: Cow::Borrowed("mxv.toml"),
        unit_prefix: Cow::Borrowed("mxv-"),
        domain: Cow::Borrowed("mxv.localhost"),
    };
}

impl PartialEq for ManifestNames {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for ManifestNames {}

impl Default for ManifestNames {
    fn default() -> Self {
        Self::MXV
    }
}

impl Host {
    /// Returns the names that reading a manifest needs.
    pub(crate) fn manifest_names(&self) -> ManifestNames {
        ManifestNames {
            file: self.manifest_file.clone(),
            unit_prefix: Cow::Owned(self.unit_prefix()),
            domain: self.proxy_domain.clone(),
        }
    }
}
