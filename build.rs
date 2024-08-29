//! The build script for git-z.

use std::{env, io, process::Command};

fn main() {
    define_version_with_git();
}

/// Defines a variable containing the version with the Git revision.
///
/// `VERSION_WITH_GIT` contains at least the cargo version, even when Git is not
/// available. When Git is available, the current Git revision and dirty state
/// is added to the version as a tag, except in the following cases:
///
/// * the build is done from a clean worktree checked out at a tag matching
///   *exactly* the cargo version prefixed by `v`,
/// * the build is done from a cargo checkout (via `cargo install --git`) and
///   the version does not contain `-dev`.
///
/// If Git is not available, but the `FLAKE_REVISION` environment variable is
/// defined, it is used instead to provide the revision from the Nix flake, for
/// development versions only.
///
/// For instance:
///
/// * Cargo version 1.0.0 on tag v1.0.0, clean state => `1.0.0`
/// * Cargo version 1.0.0 on tag v1.0.0, dirty state =>
///   `1.0.0+abcd1234-modified`
/// * Cargo version 1.1.0-dev on any commit, clean state => `1.1.0-dev+abcd1234`
fn define_version_with_git() {
    let cargo_version = env!("CARGO_PKG_VERSION");
    let version = version_with_features(cargo_version);
    let version = version_with_revision(&version);
    println!("cargo:rustc-env=VERSION_WITH_GIT={version}");
}

/// Returns the version with feature flags.
fn version_with_features(cargo_version: &str) -> String {
    if env::var("CARGO_FEATURE_UNSTABLE_PRE_COMMIT").is_ok() {
        format!("{cargo_version}+pre-commit")
    } else {
        String::from(cargo_version)
    }
}

/// Returns the version from cargo with a revision.
fn version_with_revision(cargo_version: &str) -> String {
    if let Some(revision) = revision(cargo_version) {
        format!("{cargo_version}+{revision}")
    } else {
        String::from(cargo_version)
    }
}

/// Gets the revision from the Git or the flake.
fn revision(cargo_version: &str) -> Option<String> {
    revision_from_git(cargo_version)
        .ok()
        .flatten()
        .or_else(|| revision_from_flake(cargo_version))
}

/// Gets the revision from the flake.
fn revision_from_flake(cargo_version: &str) -> Option<String> {
    if is_dev_version(cargo_version) {
        env::var("FLAKE_REVISION").ok()
    } else {
        None
    }
}

/// Gets the revision from Git.
fn revision_from_git(cargo_version: &str) -> io::Result<Option<String>> {
    if git_describe()? == format!("v{cargo_version}")
        || is_cargo_checkout()? && !is_dev_version(cargo_version)
    {
        Ok(None)
    } else {
        Ok(Some(git_revision_and_state()?))
    }
}

/// Returns the result of `git describe --always --dirty=-modified`.
///
/// # Panics
///
/// This function panics if the output of `git describe` is not valid UTF-8.
fn git_describe() -> io::Result<String> {
    let output = Command::new("git")
        .args(["describe", "--always", "--dirty=-modified"])
        .output()?;
    #[allow(clippy::unwrap_used)]
    Ok(String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

/// Returns the current Git revision and its dirtiness.
fn git_revision_and_state() -> io::Result<String> {
    let revision = git_revision()?;
    if git_is_dirty()? && !is_cargo_checkout()? {
        Ok(format!("{revision}-modified"))
    } else {
        Ok(revision)
    }
}

/// Returns the current Git revision.
///
/// # Panics
///
/// This function panics if the output of `git rev-parse` is not valid UTF-8.
fn git_revision() -> io::Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()?;
    #[allow(clippy::unwrap_used)]
    Ok(String::from_utf8(output.stdout).unwrap().trim().to_owned())
}

/// Returns whether the current Git worktree is dirty.
fn git_is_dirty() -> io::Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()?;
    Ok(!output.stdout.is_empty())
}

/// Returns whether the current wortree is a checkout from cargo.
fn is_cargo_checkout() -> io::Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()?;
    Ok(output.stdout == b"?? .cargo-ok\n")
}

/// Returns whether the current version is a development version.
fn is_dev_version(cargo_version: &str) -> bool {
    cargo_version.contains("-dev")
}
