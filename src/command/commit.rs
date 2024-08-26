// git-z - A Git extension to go beyond.
// Copyright (C) 2023-2024 Jean-Philippe Cugnet <jean-philippe@cugnet.eu>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, version 3 of the License.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! The `commit` subcommand.

use std::{fs, path::PathBuf, process::Command};

use clap::Parser;
use eyre::{bail, ensure, eyre, Context as _, Result};
use indexmap::IndexMap;
use inquire::{validator::Validation, Confirm, CustomUserError, Select, Text};
use itertools::Itertools;
use regex::Regex;
use serde::Serialize;
use tera::{Context, Tera};
use thiserror::Error;

use crate::{
    command::helpers::load_config,
    commit_cache::{CommitCache, WizardState},
    config::{Config, Scopes, Ticket},
};

use super::helpers::ensure_in_git_worktree;

#[cfg(feature = "unstable-pre-commit")]
use std::env;

#[cfg(feature = "unstable-pre-commit")]
use is_executable::IsExecutable;

#[cfg(feature = "unstable-pre-commit")]
use crate::warning;

/// The size of a page in the terminal.
const PAGE_SIZE: usize = 15;

/// The commit command.
#[derive(Debug, Parser)]
pub struct Commit {
    /// Print the commit message instead of calling `git commit`.
    #[arg(long)]
    print_only: bool,
    /// Do not run the pre-commit hook.
    #[cfg(feature = "unstable-pre-commit")]
    #[arg(long, short = 'n')]
    no_verify: bool,
    /// Extra arguments to be passed to `git commit`.
    #[arg(last = true)]
    extra_args: Vec<String>,
}

/// Usage errors of `git z commit`.
#[derive(Debug, Error)]
pub enum CommitError {
    /// The pre-commit hook has failed.
    #[cfg(feature = "unstable-pre-commit")]
    #[error("The pre-commit hook has failed")]
    PreCommitFailed,
    /// The commit template is invalid.
    #[error("Failed to parse the commit template")]
    Template(#[from] tera::Error),
    /// Git has returned an error.
    #[error("Git has returned an error")]
    Git {
        /// The status code returned by Git.
        status_code: Option<i32>,
    },
}

/// A conventional commit message.
#[derive(Debug, Serialize)]
struct CommitMessage {
    /// The type of commit.
    r#type: String,
    /// The optional scope of the commit.
    scope: Option<String>,
    /// The short commit description.
    description: String,
    /// The optional breaking change description.
    breaking_change: Option<String>,
    /// The optional linked ticket.
    ticket: Option<String>,
}

impl super::Command for Commit {
    fn run(&self) -> Result<()> {
        ensure_in_git_worktree()?;

        let config = load_config()?;

        #[cfg(feature = "unstable-pre-commit")]
        if !self.no_verify {
            run_pre_commit_hook()?;
        }

        let commit_message = make_commit_message(&config)?;

        if self.print_only {
            println!("{commit_message}");
        } else {
            let mut git_commit = Command::new("git");

            git_commit.arg("commit");
            #[cfg(feature = "unstable-pre-commit")]
            git_commit.arg("--no-verify");
            git_commit
                .args(&self.extra_args)
                .args(["-em", &commit_message]);

            let status = git_commit.status()?;

            if !status.success() {
                bail!(CommitError::Git {
                    status_code: status.code()
                });
            }
        }

        CommitCache::discard()?;
        Ok(())
    }
}

impl CommitMessage {
    /// Runs the wizard to build a commit message from user input.
    fn run_wizard(config: &Config, cache: &mut CommitCache) -> Result<Self> {
        let commit_message = Self {
            r#type: ask_type(config, cache)?,
            scope: ask_scope(config, cache)?,
            description: ask_description(cache)?,
            breaking_change: ask_breaking_change(cache)?,
            ticket: ask_ticket(config, cache)?,
        };

        cache.mark_wizard_as_completed()?;

        Ok(commit_message)
    }

    /// Builds a dummy commit message.
    fn dummy() -> Self {
        Self {
            r#type: String::from("dummy"),
            scope: Some(String::from("dummy")),
            description: String::from("dummy commit"),
            breaking_change: Some(String::from("Dummy breaking change.")),
            ticket: Some(String::from("#0")),
        }
    }
}

/// Runs the pre-commit hook if it exists.
#[cfg(feature = "unstable-pre-commit")]
fn run_pre_commit_hook() -> Result<()> {
    let pre_commit = pre_commit()?;

    if pre_commit.exists() {
        if pre_commit.is_executable() {
            let status = Command::new(pre_commit).status()?;

            if !status.success() {
                bail!(CommitError::PreCommitFailed);
            }
        } else {
            let path = pre_commit
                .strip_prefix(env::current_dir()?)
                .unwrap_or(&pre_commit)
                .display();

            warning!(
                "The `{path}` hook was ignored because it is not set as \
                executable."
            );
        }
    }

    Ok(())
}

/// Makes a commit message.
fn make_commit_message(config: &Config) -> Result<String> {
    let mut cache = CommitCache::load()?;

    match cache.wizard_state {
        WizardState::NotStarted | WizardState::Ongoing => {
            make_message_from_wizard(config, &mut cache)
        }
        WizardState::Completed => {
            if let Some(message) = last_commit_message()? {
                let do_reuse_message = ask_reuse_message()?;

                if do_reuse_message {
                    Ok(message)
                } else {
                    cache.reset()?;
                    make_message_from_wizard(config, &mut cache)
                }
            } else {
                cache.mark_wizard_as_ongoing()?;
                make_message_from_wizard(config, &mut cache)
            }
        }
    }
}

/// Makes a commit message by running the wizard.
fn make_message_from_wizard(
    config: &Config,
    cache: &mut CommitCache,
) -> Result<String> {
    let tera = build_and_check_template(config)?;

    if cache.wizard_state == WizardState::Ongoing {
        let do_reuse_answers = ask_reuse_answers()?;
        if !do_reuse_answers {
            cache.reset()?;
        }
    }

    let commit_message = CommitMessage::run_wizard(config, cache)?;
    let context = Context::from_serialize(commit_message)?;
    let message = tera.render("templates.commit", &context)?;

    Ok(message)
}

/// Loads the commit template and checks for errors.
fn build_and_check_template(config: &Config) -> Result<Tera> {
    let mut tera = Tera::default();

    tera.add_raw_template("templates.commit", &config.templates.commit)
        .map_err(CommitError::Template)?;

    // Render a dummy commit to catch early any variable error.
    tera.render(
        "templates.commit",
        &Context::from_serialize(CommitMessage::dummy())?,
    )
    .map_err(CommitError::Template)?;

    Ok(tera)
}

/// Asks the user whether to reuse the commit message from an aborted run.
fn ask_reuse_message() -> Result<bool> {
    Ok(Confirm::new(
        "A previous run has been aborted. Do you want to reuse your commit \
            message?",
    )
    .with_help_message(
        "This will use your last commit message without running the wizard.",
    )
    .with_default(true)
    .prompt()?)
}

/// Asks the user whether to reuse answers from an aborted run.
fn ask_reuse_answers() -> Result<bool> {
    Ok(Confirm::new(
        "A previous run has been aborted. Do you want to reuse your answers?",
    )
    .with_help_message(
        "The wizard will be run as usual with your answers pre-selected.",
    )
    .with_default(true)
    .prompt()?)
}

/// Asks the user which type of commit they wants.
fn ask_type(config: &Config, cache: &mut CommitCache) -> Result<String> {
    let cached = cache.r#type().unwrap_or_default();
    let cursor = config.types.get_index_of(cached).unwrap_or_default();

    let choice = Select::new("Commit type", format_types(&config.types))
        .with_starting_cursor(cursor)
        .with_page_size(PAGE_SIZE)
        .with_formatter(&|choice| remove_type_description(choice.value))
        .prompt()?;
    let r#type = remove_type_description(&choice);

    cache.set_type(&r#type)?;

    Ok(r#type)
}

/// Asks the user to which scope the changes are applicable.
fn ask_scope(
    config: &Config,
    cache: &mut CommitCache,
) -> Result<Option<String>> {
    let scope = match &config.scopes {
        None => None,

        Some(Scopes::Any) => Text::new("Scope")
            .with_initial_value(cache.scope().unwrap_or_default())
            .with_help_message("Press ESC or leave empty to omit the scope.")
            .prompt_skippable()?
            .filter(|s| !s.is_empty()),

        Some(Scopes::List { list }) => {
            let cached = cache.scope().unwrap_or_default();
            let cursor =
                list.iter().position(|s| s == cached).unwrap_or_default();

            let help_message = "↑↓ to move, enter to select, type to \
                filter, ESC to leave empty, update `git-z.toml` to add new \
                scopes";

            Select::new("Scope", list.clone())
                .with_starting_cursor(cursor)
                .with_help_message(help_message)
                .with_page_size(PAGE_SIZE)
                .prompt_skippable()?
        }
    };

    cache.set_scope(scope.as_deref())?;

    Ok(scope)
}

/// Asks the user for a commit description.
fn ask_description(cache: &mut CommitCache) -> Result<String> {
    let placeholder =
        "describe your change with a short description (5-50 characters)";
    let message = "You will be able to add a long description to your \
        commit in an editor later.";

    let description = Text::new("Short description")
        .with_placeholder(placeholder)
        .with_initial_value(cache.description().unwrap_or_default())
        .with_help_message(message)
        .with_validator(validate_description)
        .prompt()?;

    cache.set_description(&description)?;

    Ok(description)
}

/// Asks the user for an optional breaking change description.
fn ask_breaking_change(cache: &mut CommitCache) -> Result<Option<String>> {
    let breaking_change = Text::new("BREAKING CHANGE")
        .with_placeholder("Summary of the breaking change.")
        .with_initial_value(cache.breaking_change().unwrap_or_default())
        .with_help_message(
            "Press ESC or leave empty if there are no breaking changes.",
        )
        .prompt_skippable()?
        .filter(|s| !s.is_empty());

    cache.set_breaking_change(breaking_change.as_deref())?;

    Ok(breaking_change)
}

/// Optionally asks the user for a ticket reference.
fn ask_ticket(
    config: &Config,
    cache: &mut CommitCache,
) -> Result<Option<String>> {
    let ticket = match &config.ticket {
        None => None,
        Some(Ticket { required, prefixes }) => {
            let placeholder = ticket_placeholder(prefixes)?;
            let cached_answer = cache.ticket();
            let ticket_from_branch = get_ticket_from_branch(prefixes)?;

            let initial_value = cached_answer.unwrap_or_else(|| {
                ticket_from_branch.as_deref().unwrap_or_default()
            });

            let prompt = Text::new("Issue / ticket number")
                .with_placeholder(&placeholder)
                .with_initial_value(initial_value)
                .with_validator(validate_ticket);

            if *required {
                Some(prompt.prompt()?)
            } else {
                prompt
                    .with_help_message(
                        "Press ESC to omit the ticket reference.",
                    )
                    .prompt_skippable()?
            }
        }
    };

    cache.set_ticket(ticket.as_deref())?;

    Ok(ticket)
}

/// Tries to extract a ticket number from the name of the current Git branch.
// NOTE(allow): This function cannot actually panic. See the notes below.
#[allow(clippy::missing_panics_doc)]
fn get_ticket_from_branch(prefixes: &[String]) -> Result<Option<String>> {
    // Replace `#` with an empty string in the regex, as we want to match
    // branches like `feature/23-name` when `#` is a valid prefix like for
    // GitHub or GitLab issues.
    let regex = ticket_regex(prefixes).replace('#', "");

    let ticket = Regex::new(&regex)
        .wrap_err("Impossible to build a regex from the list of prefixes")?
        .captures(&get_current_branch()?)
        .map(|captures| {
            // NOTE(indexing): Capture group 0 always corresponds to an implicit
            // unnamed group that includes the entire match.
            #[allow(clippy::indexing_slicing)]
            captures[0].to_owned()
        })
        .map(|ticket| {
            // NOTE(unwrap): This regex is known to be valid.
            #[allow(clippy::unwrap_used)]
            let regex = &Regex::new(r"^\d+$").unwrap();

            // If one of the valid prefixes is `#` and the matched ticket ID is
            // only made of numbers, we are in the GitHub / GitLab style, so
            // let’s add a `#` as a prefix to the ticket ID.
            if prefixes.contains(&String::from("#")) && regex.is_match(&ticket)
            {
                format!("#{ticket}")
            } else {
                ticket
            }
        });

    Ok(ticket)
}

/// Gets the name of the current Git branch.
fn get_current_branch() -> Result<String> {
    let git_branch = Command::new("git")
        .args(["branch", "--show-current"])
        .output()?;

    ensure!(
        git_branch.status.success(),
        "Failed to run `git branch --show-current`"
    );

    Ok(String::from_utf8(git_branch.stdout)?)
}

/// Formats the list of types and their description.
fn format_types(types: &IndexMap<String, String>) -> Vec<String> {
    let Some(max_type_len) = types.keys().map(String::len).max() else {
        return vec![];
    };

    types
        .iter()
        .map(|(ty, doc)| {
            let padding = " ".repeat(max_type_len - ty.len());
            format!("{ty}{padding}  {doc}")
        })
        .collect()
}

/// Removes the type description from the choice.
// NOTE(allow): This function cannot actually panic. See the notes below.
#[allow(clippy::missing_panics_doc)]
fn remove_type_description(choice: &str) -> String {
    // NOTE(unwrap): Even an empty string will contain at list one split, so the
    // only call to next will always return Some(value).
    #[allow(clippy::unwrap_used)]
    choice.split(' ').next().unwrap().to_owned()
}

/// Validates the commit description.
// NOTE(allow): This function cannot actually panic. See the notes below.
#[allow(clippy::missing_panics_doc)]
// NOTE(allow): The signature of the function is imposed by Inquire.
#[allow(clippy::unnecessary_wraps)]
fn validate_description(
    description: &str,
) -> Result<Validation, CustomUserError> {
    // NOTE(unwrap): We know from the first condition that description.len() >
    // 0, so there is at least one character in the string. Hence,
    // description.chars().next() in the third condition will always return
    // Some(value).
    #[allow(clippy::unwrap_used)]
    if description.len() < 5 {
        Ok(Validation::Invalid(
            "The description must be longer than 5 characters".into(),
        ))
    } else if description.len() > 50 {
        Ok(Validation::Invalid(
            "The description must not be longer than 50 characters".into(),
        ))
    } else if description.chars().next().unwrap().is_uppercase() {
        Ok(Validation::Invalid(
            "The description must start in lowercase".into(),
        ))
    } else {
        Ok(Validation::Valid)
    }
}

/// Validates the ticket reference.
fn validate_ticket(ticket: &str) -> Result<Validation, CustomUserError> {
    let config = Config::load()?;
    let prefixes = &config
        .ticket
        .ok_or(eyre!("no ticket prefix list"))?
        .prefixes;

    let regex = ticket_regex(prefixes);
    let placeholder = ticket_placeholder(prefixes)?;

    if Regex::new(&format!("^{regex}$"))?.is_match(ticket) {
        Ok(Validation::Valid)
    } else {
        Ok(Validation::Invalid(
            format!(
                "The issue / ticket number must be in the form {placeholder}"
            )
            .into(),
        ))
    }
}

/// Builds a regex to match valid tickets from the list of valid prefixes.
fn ticket_regex(prefixes: &[String]) -> String {
    let prefixes = prefixes.join("|");
    format!("(?:{prefixes})\\d+")
}

/// Builds the ticket placeholder from the list of valid prefixes.
fn ticket_placeholder(prefixes: &[String]) -> Result<String> {
    prefixes
        .iter()
        .map(|prefix| format!("{prefix}XXX"))
        .reduce(|acc, prefix| format!("{acc} or {prefix}"))
        .ok_or(eyre!("empty ticket prefix list"))
}

/// Returns the last commit message if it exists.
fn last_commit_message() -> Result<Option<String>> {
    let commit_editmsg = commit_editmsg()?;

    let remove_commented_lines =
        |s: &str| s.lines().filter(|line| !line.starts_with('#')).join("\n");

    let maybe_message = commit_editmsg
        .exists()
        .then(|| fs::read_to_string(commit_editmsg))
        .transpose()?
        .as_deref()
        .map(remove_commented_lines)
        .filter(|s| !s.trim().is_empty());

    Ok(maybe_message)
}

/// Returns the path to the `COMMIT_EDITMSG` file.
fn commit_editmsg() -> Result<PathBuf> {
    Ok(git_dir()?.join("COMMIT_EDITMSG"))
}

/// Returns the path to the pre-commit hook.
#[cfg(feature = "unstable-pre-commit")]
fn pre_commit() -> Result<PathBuf> {
    Ok(git_dir()?.join("hooks").join("pre-commit"))
}

/// Returns the path of the Git directory.
fn git_dir() -> Result<PathBuf> {
    let git_rev_parse = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()?;

    ensure!(
        git_rev_parse.status.success(),
        "Failed to run `git rev-parse --git-dir`"
    );

    let git_dir = String::from_utf8(git_rev_parse.stdout)?;
    Ok(PathBuf::from(git_dir.trim()))
}
