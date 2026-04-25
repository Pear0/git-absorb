#[macro_use]
extern crate slog;
use anyhow::{anyhow, Result};

mod commute;
mod config;
mod linelog_mode;
mod owned;
mod rewrite;
mod stack;

use git2::DiffStats;
use std::io::Write;
use std::path::Path;

pub struct Config<'a> {
    pub dry_run: bool,
    pub no_limit: bool,
    pub force_author: bool,
    pub force_detach: bool,
    pub base: Option<&'a str>,
    pub and_rebase: bool,
    pub rebase_options: &'a Vec<&'a str>,
    pub whole_file: bool,
    pub linelog: bool,
    pub rewrite: bool,
    pub one_fixup_per_commit: bool,
    pub squash: bool,
    pub message: Option<&'a str>,
}

pub fn run(logger: &slog::Logger, config: &Config) -> Result<()> {
    let repo = git2::Repository::open_from_env()?;
    debug!(logger, "repository found"; "path" => repo.path().to_str());

    run_with_repo(logger, config, &repo)
}

fn run_with_repo(logger: &slog::Logger, config: &Config, repo: &git2::Repository) -> Result<()> {
    let config = config::unify(config, repo);
    validate_config(&config)?;

    let mut we_added_everything_to_index = false;
    if nothing_left_in_index(repo)? {
        if config::auto_stage_if_nothing_staged(repo) {
            // no matter from what subdirectory we're executing,
            // "." will still refer to the root workdir.
            let pathspec = ["."];
            let mut index = repo.index()?;
            index.add_all(pathspec.iter(), git2::IndexAddOption::DEFAULT, None)?;
            index.write()?;

            if nothing_left_in_index(repo)? {
                announce(logger, Announcement::NothingStagedAfterAutoStaging);
                return Ok(());
            }

            we_added_everything_to_index = true;
        } else {
            announce(logger, Announcement::NothingStaged);
            return Ok(());
        }
    }

    let (stack, stack_end_reason) = stack::working_stack(
        repo,
        config.no_limit,
        config.base,
        config.force_author,
        config.force_detach,
        logger,
    )?;

    let mut diff_options = Some({
        let mut ret = git2::DiffOptions::new();
        ret.context_lines(0)
            .id_abbrev(40)
            .ignore_filemode(true)
            .ignore_submodules(true);
        ret
    });

    let (stack, summary_counts): (Vec<_>, _) = {
        let mut diffs = Vec::with_capacity(stack.len());
        for commit in &stack {
            let diff = owned::Diff::new(
                &repo.diff_tree_to_tree(
                    if commit.parents().len() == 0 {
                        None
                    } else {
                        Some(commit.parent(0)?.tree()?)
                    }
                    .as_ref(),
                    Some(&commit.tree()?),
                    diff_options.as_mut(),
                )?,
            )?;
            trace!(logger, "parsed commit diff";
                   "commit" => commit.id().to_string(),
                   "diff" => format!("{:?}", diff),
            );
            diffs.push(diff);
        }

        let summary_counts = stack::summary_counts(&stack);
        (stack.into_iter().zip(diffs).collect(), summary_counts)
    };

    let mut head_tree = repo.head()?.peel_to_tree()?;
    let index = owned::Diff::new(&repo.diff_tree_to_index(
        Some(&head_tree),
        None,
        diff_options.as_mut(),
    )?)?;
    trace!(logger, "parsed index";
           "index" => format!("{:?}", index),
    );

    if config.rewrite {
        let head_commit = repo.head()?.peel_to_commit()?;
        let outcome = rewrite::run(
            repo,
            &stack,
            &index,
            &config,
            we_added_everything_to_index,
            logger,
        )?;
        announce_unabsorbed(
            logger,
            repo,
            &config,
            &stack,
            stack_end_reason,
            &head_commit,
            index.len(),
            outcome.modified_hunks_without_target,
            outcome.non_modified_patches,
            we_added_everything_to_index,
        );
        return Ok(());
    }

    let signature = repo
        .signature()
        .or_else(|_| git2::Signature::now("nobody", "nobody@example.com"))?;
    let mut head_commit = repo.head()?.peel_to_commit()?;

    let HunkAttribution {
        hunks_with_commit,
        modified_hunks_without_target,
        non_modified_patches,
    } = if config.linelog {
        linelog_mode::assign_hunks(repo, &stack, &index, logger)?
    } else {
        assign_hunks_by_commute(&stack, &index, &config, logger)?
    };

    let target_always_sha: bool = config::fixup_target_always_sha(repo);

    if !config.dry_run {
        repo.reference("PRE_ABSORB_HEAD", head_commit.id(), true, "")?;
    }

    // * apply all hunks that are going to be fixed up into `dest_commit`
    // * commit the fixup
    // * repeat for all `dest_commit`s
    //
    // the `.zip` here will gives us something similar to `.windows`, but with
    // an extra iteration for the last element (otherwise we would have to
    // special case the last element and commit it separately)
    for (current, next) in hunks_with_commit
        .iter()
        .zip(hunks_with_commit.iter().skip(1).map(Some).chain([None]))
    {
        let new_head_tree = apply_hunk_to_tree(
            repo,
            &head_tree,
            &current.hunk_to_apply,
            &current.index_patch.old_path,
        )?;

        // whether there are no more hunks to apply to `dest_commit`
        let commit_fixup = next.map_or(true, |next| {
            // if the next hunk is for a different commit -- commit what we have so far
            !config.one_fixup_per_commit || next.dest_commit.id() != current.dest_commit.id()
        });
        if commit_fixup {
            // TODO: the git2 api only supports utf8 commit messages,
            // so it's okay to use strings instead of bytes here
            // https://docs.rs/git2/0.7.5/src/git2/repo.rs.html#998
            // https://libgit2.org/libgit2/#HEAD/group/commit/git_commit_create
            let dest_commit_id = current.dest_commit.id().to_string();
            let dest_commit_locator = match target_always_sha {
                true => &dest_commit_id,
                false => current
                    .dest_commit
                    .summary()
                    .filter(|&msg| summary_counts[msg] == 1)
                    .unwrap_or(&dest_commit_id),
            };
            let diff = repo
                .diff_tree_to_tree(Some(&head_commit.tree()?), Some(&new_head_tree), None)?
                .stats()?;
            if !config.dry_run {
                head_tree = new_head_tree;
                let verb = if config.squash { "squash" } else { "fixup" };
                let mut message = format!("{}! {}\n", verb, dest_commit_locator);
                if let Some(m) = config.message.filter(|m| !m.is_empty()) {
                    message.push('\n');
                    message.push_str(m);
                    message.push('\n');
                };
                head_commit = repo.find_commit(repo.commit(
                    Some("HEAD"),
                    &signature,
                    &signature,
                    &message,
                    &head_tree,
                    &[&head_commit],
                )?)?;
                announce(
                    logger,
                    Announcement::Committed(&head_commit, dest_commit_locator, &diff),
                );
            } else {
                announce(
                    logger,
                    Announcement::WouldHaveCommitted(dest_commit_locator, &diff),
                );
            }
        } else {
            // we didn't commit anything, but we applied a hunk
            head_tree = new_head_tree;
        }
    }

    if we_added_everything_to_index {
        // now that the fixup commits have been created,
        // we should unstage the remaining changes from the index.

        let mut index = repo.index()?;
        index.read_tree(&head_tree)?;
        index.write()?;
    }

    announce_unabsorbed(
        logger,
        repo,
        &config,
        &stack,
        stack_end_reason,
        &head_commit,
        index.len(),
        modified_hunks_without_target,
        non_modified_patches,
        we_added_everything_to_index,
    );

    if !hunks_with_commit.is_empty() {
        use std::process::Command;
        // unwrap() is safe here, as we exit early if the stack is empty
        let last_commit_in_stack = &stack.last().unwrap().0;
        // The stack isn't supposed to have any merge commits, per the check in working_stack()
        let number_of_parents = last_commit_in_stack.parents().len();
        assert!(number_of_parents <= 1);

        let rebase_root = if number_of_parents == 0 {
            "--root"
        } else {
            // Use a range that is guaranteed to include all the commits we might have
            // committed "fixup!" commits for.
            &*last_commit_in_stack.parent(0)?.id().to_string()
        };

        let rebase_args = [
            "rebase",
            "--interactive",
            "--autosquash",
            "--autostash",
            rebase_root,
        ];

        if config.and_rebase {
            let mut command = Command::new("git");

            // We'd generally expect to be run from within the repository, but just in case,
            // try to have git run rebase from the repository root.
            // This simplifies writing tests that execute from within git-absorb's source directory
            // but operate on temporary repositories created elsewhere.
            // (The tests could explicitly change directories, but then must be serialized.)
            let repo_path = repo.workdir().and_then(Path::to_str);
            match repo_path {
                Some(path) => {
                    command.args(["-C", path]);
                }
                _ => {
                    announce(logger, Announcement::CouldNotFindRepositoryPath);
                }
            }

            command.args(rebase_args);

            for arg in config.rebase_options {
                command.arg(arg);
            }

            if config.dry_run {
                announce(logger, Announcement::WouldHaveRebased(&command));
            } else {
                debug!(logger, "running git rebase"; "command" => format!("{:?}", command));
                // Don't check that we have successfully absorbed everything, nor git's
                // exit code -- as git will print helpful messages on its own.
                command.status().expect("could not run git rebase");
            }
        } else if !config.dry_run {
            announce(logger, Announcement::HowToSquash(rebase_args.join(" ")));
        }
    }

    Ok(())
}

fn validate_config(config: &Config) -> Result<()> {
    if config.rewrite {
        if config.and_rebase {
            return Err(anyhow!("--rewrite cannot be combined with --and-rebase"));
        }
        if !config.rebase_options.is_empty() {
            return Err(anyhow!("--rewrite cannot be combined with rebase options"));
        }
        if config.whole_file {
            return Err(anyhow!("--rewrite cannot be combined with --whole-file"));
        }
        if config.one_fixup_per_commit {
            return Err(anyhow!(
                "--rewrite cannot be combined with --one-fixup-per-commit"
            ));
        }
        if config.squash {
            return Err(anyhow!("--rewrite cannot be combined with --squash"));
        }
        if config.message.is_some() {
            return Err(anyhow!("--rewrite cannot be combined with --message"));
        }
        return Ok(());
    }

    if !config.rebase_options.is_empty() && !config.and_rebase {
        return Err(anyhow!(
            "REBASE_OPTIONS were specified without --and-rebase flag"
        ));
    }
    if config.linelog && config.whole_file {
        return Err(anyhow!(
            "--linelog cannot be combined with --whole-file because they use incompatible hunk attribution modes"
        ));
    }
    Ok(())
}

fn announce_unabsorbed<'r>(
    logger: &slog::Logger,
    repo: &git2::Repository,
    config: &Config,
    stack: &[(git2::Commit<'r>, owned::Diff)],
    stack_end_reason: stack::StackEndReason,
    head_commit: &git2::Commit,
    index_len: usize,
    modified_hunks_without_target: usize,
    non_modified_patches: usize,
    we_added_everything_to_index: bool,
) {
    if non_modified_patches == index_len {
        announce(logger, Announcement::NoFileModifications);
        return;
    }

    // So long as there was a patch that had the possibility of fixing up
    // a commit, warn about the presence of patches that will commute with
    // everything.
    // Users that auto-stage changes may be accustomed to having untracked files
    // in their workspace that are not absorbed, so don't warn them.
    if non_modified_patches > 0 && !we_added_everything_to_index {
        announce(logger, Announcement::NonFileModifications);
    }

    if modified_hunks_without_target > 0 {
        announce(logger, Announcement::FileModificationsWithoutTarget);

        match stack_end_reason {
            stack::StackEndReason::ReachedRoot => {
                announce(logger, Announcement::CannotFixUpPastFirstCommit);
            }
            stack::StackEndReason::ReachedMergeCommit => {
                let commit = match stack.last() {
                    Some(commit) => &commit.0,
                    None => head_commit,
                };
                announce(logger, Announcement::CannotFixUpPastMerge(commit));
            }
            stack::StackEndReason::ReachedAnotherAuthor => {
                let commit = match stack.last() {
                    Some(commit) => &commit.0,
                    None => head_commit,
                };
                announce(logger, Announcement::WillNotFixUpPastAnotherAuthor(commit));
            }
            stack::StackEndReason::ReachedLimit => {
                announce(
                    logger,
                    Announcement::WillNotFixUpPastStackLimit(config::max_stack(repo)),
                );
            }
            stack::StackEndReason::CommitsHiddenByBase => {
                announce(
                    logger,
                    Announcement::CommitsHiddenByBase(config.base.unwrap()),
                );
            }
            stack::StackEndReason::CommitsHiddenByBranches => {
                announce(logger, Announcement::CommitsHiddenByBranches);
            }
        }
    }
}

pub(crate) struct HunkAttribution<'c, 'r, 'p> {
    pub(crate) hunks_with_commit: Vec<HunkWithCommit<'c, 'r, 'p>>,
    pub(crate) modified_hunks_without_target: usize,
    pub(crate) non_modified_patches: usize,
}

pub(crate) struct HunkWithCommit<'c, 'r, 'p> {
    pub(crate) hunk_to_apply: owned::Hunk,
    pub(crate) dest_commit: &'c git2::Commit<'r>,
    pub(crate) index_patch: &'p owned::Patch,
}

fn assign_hunks_by_commute<'c, 'r, 'p>(
    stack: &'c [(git2::Commit<'r>, owned::Diff)],
    index: &'p owned::Diff,
    config: &Config,
    logger: &slog::Logger,
) -> Result<HunkAttribution<'c, 'r, 'p>> {
    let mut hunks_with_commit = vec![];

    let mut modified_hunks_without_target = 0usize;
    let mut non_modified_patches = 0usize;
    'patch: for index_patch in index.iter() {
        let old_path = index_patch.new_path.as_slice();
        if index_patch.status != git2::Delta::Modified {
            debug!(logger, "skipped non-modified patch";
                    "path" => String::from_utf8_lossy(old_path).into_owned(),
                    "status" => format!("{:?}", index_patch.status),
            );
            non_modified_patches += 1;
            continue 'patch;
        }

        let mut preceding_hunks_offset = 0isize;
        let mut applied_hunks_offset = 0isize;
        'hunk: for index_hunk in &index_patch.hunks {
            debug!(logger, "next hunk";
                   "header" => index_hunk.header(),
                   "path" => String::from_utf8_lossy(old_path).into_owned(),
            );

            // Isolate this index hunk from preceding index hunks, then shift it
            // again by hunks already committed into the synthetic HEAD tree.
            let isolated_hunk = index_hunk
                .clone()
                .shift_added_block(-preceding_hunks_offset);
            let hunk_to_apply = isolated_hunk
                .clone()
                .shift_both_blocks(applied_hunks_offset);
            let hunk_offset = index_hunk.changed_offset();

            debug!(logger, "";
                "to apply" => hunk_to_apply.header(),
                "to commute" => isolated_hunk.header(),
                "preceding hunks" => format!("{}/{}", applied_hunks_offset, preceding_hunks_offset),
            );

            preceding_hunks_offset += hunk_offset;

            // find the newest commit that the hunk cannot commute with
            let mut dest_commit = None;
            let mut commuted_old_path = old_path;
            let mut commuted_index_hunk = isolated_hunk;

            'commit: for (commit, diff) in stack {
                let c_logger = logger.new(o!(
                    "commit" => commit.id().to_string(),
                ));
                let next_patch = match diff.by_new(commuted_old_path) {
                    Some(patch) => patch,
                    // this commit doesn't touch the hunk's file, so
                    // they trivially commute, and the next commit
                    // should be considered
                    None => {
                        debug!(c_logger, "skipped commit with no path");
                        continue 'commit;
                    }
                };

                // sometimes we just forget some change (eg: intializing some object) that
                // happens in a completely unrelated place with the current hunks. In those
                // cases, might be helpful to just match the first commit touching the same
                // file as the current hunk. Use this option with care!
                if config.whole_file {
                    debug!(
                        c_logger,
                        "Commit touches the hunk file and match whole file is enabled"
                    );
                    dest_commit = Some(commit);
                    break 'commit;
                }

                if next_patch.status == git2::Delta::Added {
                    debug!(c_logger, "found noncommutative commit by add");
                    dest_commit = Some(commit);
                    break 'commit;
                }
                if commuted_old_path != next_patch.old_path.as_slice() {
                    debug!(c_logger, "changed commute path";
                           "path" => String::from_utf8_lossy(&next_patch.old_path).into_owned(),
                    );
                    commuted_old_path = next_patch.old_path.as_slice();
                }
                commuted_index_hunk = match commute::commute_diff_before(
                    &commuted_index_hunk,
                    &next_patch.hunks,
                ) {
                    Some(hunk) => {
                        debug!(c_logger, "commuted hunk with commit";
                               "offset" => (hunk.added.start as i64) - (commuted_index_hunk.added.start as i64),
                        );
                        hunk
                    }
                    // this commit contains a hunk that cannot
                    // commute with the hunk being absorbed
                    None => {
                        debug!(c_logger, "found noncommutative commit by conflict");
                        dest_commit = Some(commit);
                        break 'commit;
                    }
                };
            }
            let dest_commit = match dest_commit {
                Some(commit) => commit,
                // the hunk commutes with every commit in the stack,
                // so there is no commit to absorb it into
                None => {
                    modified_hunks_without_target += 1;
                    continue 'hunk;
                }
            };

            hunks_with_commit.push(HunkWithCommit {
                hunk_to_apply,
                dest_commit,
                index_patch,
            });

            applied_hunks_offset += hunk_offset;
        }
    }

    Ok(HunkAttribution {
        hunks_with_commit,
        modified_hunks_without_target,
        non_modified_patches,
    })
}

fn apply_hunk_to_tree<'repo>(
    repo: &'repo git2::Repository,
    base: &git2::Tree,
    hunk: &owned::Hunk,
    path: &[u8],
) -> Result<git2::Tree<'repo>> {
    let mut treebuilder = repo.treebuilder(Some(base))?;

    // recurse into nested tree if applicable
    if let Some(slash) = path.iter().position(|&x| x == b'/') {
        let (first, rest) = path.split_at(slash);
        let rest = &rest[1..];

        let (subtree, submode) = {
            let entry = treebuilder
                .get(first)?
                .ok_or_else(|| anyhow!("couldn't find tree entry in tree for path"))?;
            (repo.find_tree(entry.id())?, entry.filemode())
        };
        // TODO: loop instead of recursing to avoid potential stack overflow
        let result_subtree = apply_hunk_to_tree(repo, &subtree, hunk, rest)?;

        treebuilder.insert(first, result_subtree.id(), submode)?;
        return Ok(repo.find_tree(treebuilder.write()?)?);
    }

    let (blob, mode) = {
        let entry = treebuilder
            .get(path)?
            .ok_or_else(|| anyhow!("couldn't find blob entry in tree for path"))?;
        (repo.find_blob(entry.id())?, entry.filemode())
    };

    // TODO: convert path to OsStr and pass it during blob_writer
    // creation, to get gitattributes handling (note that converting
    // &[u8] to &std::path::Path is only possible on unixy platforms)
    let mut blobwriter = repo.blob_writer(None)?;
    let old_content = blob.content();
    let (old_start, _, _, _) = hunk.anchors();

    // first, write the lines from the old content that are above the
    // hunk
    let old_content = {
        let (pre, post) = split_lines_after(old_content, old_start);
        blobwriter.write_all(pre)?;
        post
    };
    // next, write the added side of the hunk
    for line in &*hunk.added.lines {
        blobwriter.write_all(line)?;
    }
    // if this hunk removed lines from the old content, those must be
    // skipped
    let (_, old_content) = split_lines_after(old_content, hunk.removed.lines.len());
    // finally, write the remaining lines of the old content
    blobwriter.write_all(old_content)?;

    treebuilder.insert(path, blobwriter.commit()?, mode)?;
    Ok(repo.find_tree(treebuilder.write()?)?)
}

/// Return slices for lines [1..n] and [n+1; ...]
fn split_lines_after(content: &[u8], n: usize) -> (&[u8], &[u8]) {
    let split_index = if n > 0 {
        memchr::Memchr::new(b'\n', content)
            .fuse() // TODO: is fuse necessary here?
            .nth(n - 1) // the position of '\n' ending the `n`-th line
            .map(|x| x + 1)
            .unwrap_or_else(|| content.len())
    } else {
        0
    };
    content.split_at(split_index)
}

fn nothing_left_in_index(repo: &git2::Repository) -> Result<bool> {
    let stats = index_stats(repo)?;
    let nothing = stats.files_changed() == 0 && stats.insertions() == 0 && stats.deletions() == 0;
    Ok(nothing)
}

fn index_stats(repo: &git2::Repository) -> Result<git2::DiffStats> {
    let head = repo.head()?.peel_to_tree()?;
    let diff = repo.diff_tree_to_index(Some(&head), Some(&repo.index()?), None)?;
    let stats = diff.stats()?;
    Ok(stats)
}

// Messages that will be shown to users during normal operations (not debug messages).
enum Announcement<'r> {
    Committed(&'r git2::Commit<'r>, &'r str, &'r git2::DiffStats),
    WouldHaveCommitted(&'r str, &'r git2::DiffStats),
    RewroteCommit(String, String, usize),
    WouldRewriteCommit(String),
    WouldHaveRebased(&'r std::process::Command),
    HowToSquash(String),
    NothingStagedAfterAutoStaging,
    NothingStaged,
    NoFileModifications,
    NonFileModifications,
    FileModificationsWithoutTarget,
    CannotFixUpPastFirstCommit,
    CannotFixUpPastMerge(&'r git2::Commit<'r>),
    WillNotFixUpPastAnotherAuthor(&'r git2::Commit<'r>),
    WillNotFixUpPastStackLimit(usize),
    CommitsHiddenByBase(&'r str),
    CommitsHiddenByBranches,
    CouldNotFindRepositoryPath,
}

fn announce(logger: &slog::Logger, announcement: Announcement) {
    match announcement {
        Announcement::Committed(commit, destination, diff) => {
            let commit_short_id = commit.as_object().short_id().unwrap();
            let commit_short_id = commit_short_id
                .as_str()
                .expect("the commit short id is always a valid ASCII string");
            let change_header = format_change_header(diff);

            info!(
                logger,
                "committed";
                "fixup" => destination,
                "commit" => commit_short_id,
                "header" => change_header,
            );
        }
        Announcement::WouldHaveCommitted(fixup, diff) => info!(
            logger,
            "would have committed";
            "fixup" => fixup,
            "header" => format_change_header(diff),
        ),
        Announcement::RewroteCommit(old, new, changed_files) => info!(
            logger,
            "rewrote commit";
            "old" => old,
            "new" => new,
            "changed_files" => changed_files,
        ),
        Announcement::WouldRewriteCommit(commit) => info!(
            logger,
            "would rewrite commit";
            "commit" => commit,
        ),
        Announcement::WouldHaveRebased(command) => info!(
            logger, "would have run git rebase"; "command" => format!("{:?}", command)
        ),
        Announcement::HowToSquash(rebase_args) => info!(
            logger,
            "To squash the new commits, rebase:";
            "command" => format!("git {}", rebase_args),
        ),
        Announcement::NothingStagedAfterAutoStaging => warn!(
            logger,
            "No changes staged, even after auto-staging. Try adding something to the index.",
        ),
        Announcement::NothingStaged => warn!(
            logger,
            "No changes staged. Try adding something to the index or set {} = true.",
            config::AUTO_STAGE_IF_NOTHING_STAGED_CONFIG_NAME
        ),
        Announcement::NoFileModifications => warn!(
            logger,
            "No changes were in-place file modifications. \
                Added, removed, or renamed files cannot be automatically absorbed."
        ),
        Announcement::NonFileModifications => warn!(
            logger,
            "Some changes were not in-place file modifications. \
                Added, removed, or renamed files cannot be automatically absorbed."
        ),
        Announcement::FileModificationsWithoutTarget => warn!(
            logger,
            "Some file modifications did not have an available commit to fix up. \
                You will have to manually create fixup commits."
        ),
        Announcement::CannotFixUpPastFirstCommit => warn!(
            logger,
            "Cannot fix up past the first commit in the repository."
        ),
        Announcement::CannotFixUpPastMerge(commit) => warn!(
            logger,
            "Cannot fix up past a merge commit";
            "commit" => commit.id().to_string()
        ),
        Announcement::WillNotFixUpPastAnotherAuthor(commit) => warn!(
            logger,
            "Will not fix up past commits by another author. Use --force-author to override";
            "commit" => commit.id().to_string()
        ),
        Announcement::WillNotFixUpPastStackLimit(max_stack_limit) => warn!(
            logger,
            "Will not fix up past maximum stack limit. Use --base or configure {} to override",
            config::MAX_STACK_CONFIG_NAME;
            "limit" => max_stack_limit,
        ),
        Announcement::CommitsHiddenByBase(base) => warn!(
            logger,
            "Will not fix up past specified base commit. \
            Consider using --base to specify a different base commit";
            "base" => base,
        ),
        Announcement::CommitsHiddenByBranches => warn!(
            logger,
            "Will not fix up commits reachable by other branches. \
                Use --base to specify a base commit."
        ),
        Announcement::CouldNotFindRepositoryPath => warn!(
            logger,
            "Could not determine repository path for rebase. Running in current directory."
        ),
    }
}

pub(crate) fn announce_rewrote_commit(
    logger: &slog::Logger,
    old: git2::Oid,
    new: git2::Oid,
    changed_files: usize,
) {
    announce(
        logger,
        Announcement::RewroteCommit(short_oid(old), short_oid(new), changed_files),
    );
}

pub(crate) fn announce_would_rewrite_commit(logger: &slog::Logger, commit: git2::Oid) {
    announce(logger, Announcement::WouldRewriteCommit(short_oid(commit)));
}

fn short_oid(oid: git2::Oid) -> String {
    oid.to_string().chars().take(7).collect()
}

fn format_change_header(diff: &DiffStats) -> String {
    let insertions = diff.insertions();
    let deletions = diff.deletions();

    let mut header = String::new();
    if insertions > 0 {
        header.push_str(&format!(
            "{} {}(+)",
            insertions,
            if insertions == 1 {
                "insertion"
            } else {
                "insertions"
            }
        ));
    }
    if deletions > 0 {
        if !header.is_empty() {
            header.push_str(", ");
        }
        header.push_str(&format!(
            "{} {}(-)",
            deletions,
            if deletions == 1 {
                "deletion"
            } else {
                "deletions"
            }
        ));
    }
    header
}

#[cfg(test)]
mod tests {
    use git2::message_trailers_strs;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use tests::repo_utils::add;

    use super::*;
    mod log_utils;
    pub mod repo_utils;

    #[test]
    fn no_commits_in_repo() {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_opts(
            dir.path(),
            git2::RepositoryInitOptions::new().initial_head("master"),
        )
        .unwrap();
        let capturing_logger = log_utils::CapturingLogger::new();
        let result = run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &repo);
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .starts_with("reference 'refs/heads/master' not found"));
    }

    #[test]
    fn multiple_fixups_per_commit() {
        let ctx = repo_utils::prepare_and_stage();

        let actual_pre_absorb_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap().id();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 3);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        let pre_absorb_ref_commit = ctx.repo.refname_to_id("PRE_ABSORB_HEAD").unwrap();
        assert_eq!(pre_absorb_ref_commit, actual_pre_absorb_commit);

        assert_eq!(
            extract_commit_messages(&ctx.repo),
            vec![
                "fixup! Initial commit.\n",
                "fixup! Initial commit.\n",
                "Initial commit.",
            ]
        );

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "INFO",
                    "msg": "committed",
                    "fixup": "Initial commit.",
                    "header": "1 insertion(+)",
                }),
                &json!({
                    "level": "INFO",
                    "msg": "committed",
                    "fixup": "Initial commit.",
                    "header": "2 insertions(+)",
                }),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn one_deletion() {
        let (ctx, file_path) = repo_utils::prepare_repo();
        std::fs::write(
            ctx.join(&file_path),
            br#"
line
line
"#,
        )
        .unwrap();
        add(&ctx.repo, &file_path);

        let actual_pre_absorb_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap().id();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 2);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        let pre_absorb_ref_commit = ctx.repo.refname_to_id("PRE_ABSORB_HEAD").unwrap();
        assert_eq!(pre_absorb_ref_commit, actual_pre_absorb_commit);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "INFO",
                    "msg": "committed",
                    "fixup": "Initial commit.",
                    "header": "3 deletions(-)",
                }),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn one_insertion_and_one_deletion() {
        let (ctx, file_path) = repo_utils::prepare_repo();
        std::fs::write(
            ctx.join(&file_path),
            br#"
line
line

even more
lines
"#,
        )
        .unwrap();
        add(&ctx.repo, &file_path);

        let actual_pre_absorb_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap().id();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 2);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        let pre_absorb_ref_commit = ctx.repo.refname_to_id("PRE_ABSORB_HEAD").unwrap();
        assert_eq!(pre_absorb_ref_commit, actual_pre_absorb_commit);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "INFO",
                    "msg": "committed",
                    "fixup": "Initial commit.",
                    "header": "1 insertion(+), 1 deletion(-)",
                }),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn exceed_stack_limit_with_modified_hunk() {
        let (ctx, file_path) = repo_utils::prepare_repo();

        let parent_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        repo_utils::empty_commit_chain(&ctx.repo, "HEAD", &[&parent_commit], config::MAX_STACK);
        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            config::MAX_STACK + 1,
            "Wrong number of commits."
        );

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": format!(
                        "Will not fix up past maximum stack limit. \
                        Use --base or configure {} to override",
                        config::MAX_STACK_CONFIG_NAME
                    ),
                    "limit": config::MAX_STACK,
                }),
            ],
        );
    }

    #[test]
    fn exceed_stack_limit_with_non_modified_patch() {
        // non-modified patches commute with everything, and
        // have special handling above, so make sure we test with one
        let (ctx, _) = repo_utils::prepare_repo();
        let parent_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        repo_utils::empty_commit_chain(&ctx.repo, "HEAD", &[&parent_commit], config::MAX_STACK);
        let a_new_file_path = PathBuf::from("a_whole_new_file.txt");
        std::fs::write(ctx.join(&a_new_file_path), "contents").unwrap();
        repo_utils::stage_file_changes(&ctx, &a_new_file_path);
        let another_new_file_path = PathBuf::from("another_whole_new_file.txt");
        std::fs::write(ctx.join(&another_new_file_path), "contents").unwrap();
        repo_utils::stage_file_changes(&ctx, &another_new_file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            config::MAX_STACK + 1,
            "Wrong number of commits."
        );

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![&json!({
                "level": "WARN",
                "msg": "No changes were in-place file modifications. \
                       Added, removed, or renamed files cannot be automatically absorbed.",
            })],
        );
    }

    #[test]
    fn exceed_stack_limit_with_modified_patch_and_non_modified_hunks() {
        // non-modified patches commute with everything, and
        // have special handling above. Test with both modified hunks (a patch is made of hunks)
        // and non-modified patches to ensure we don't confuse the messaging to the user.
        let (ctx, file_path) = repo_utils::prepare_repo();
        let new_file_path = PathBuf::from("a_whole_new_file.txt");
        let parent_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        repo_utils::empty_commit_chain(&ctx.repo, "HEAD", &[&parent_commit], config::MAX_STACK);
        std::fs::write(ctx.join(&new_file_path), "contents").unwrap();
        repo_utils::stage_file_changes(&ctx, &new_file_path);
        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(
            revwalk.count(),
            config::MAX_STACK + 1,
            "Wrong number of commits."
        );

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some changes were not in-place file modifications. \
                           Added, removed, or renamed files cannot be automatically absorbed.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": format!(
                        "Will not fix up past maximum stack limit. \
                        Use --base or configure {} to override",
                        config::MAX_STACK_CONFIG_NAME
                    ),
                }),
            ],
        );
    }

    #[test]
    fn no_stack_limit_exceeds_stack_limit() {
        let (ctx, initial_fp) = repo_utils::prepare_repo();
        let parent_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        repo_utils::empty_commit_chain(&ctx.repo, "HEAD", &[&parent_commit], config::MAX_STACK);

        repo_utils::stage_file_changes(&ctx, &initial_fp);

        let config = Config {
            no_limit: true,
            // to have a predictable number of commits for unit test
            one_fixup_per_commit: true,
            ..DEFAULT_CONFIG
        };

        // run 'git-absorb'
        let capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        assert_eq!(
            revwalk.count(),
            // initial + 10 empty + fixup
            config::MAX_STACK + 2,
            "Wrong number of commits."
        );

        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn reached_root() {
        let (ctx, _) = repo_utils::prepare_repo();
        let file_path = PathBuf::from("a_whole_new_file.txt");
        std::fs::write(ctx.join(&file_path), "contents").unwrap();
        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1, "Wrong number of commits.");

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![&json!({
                "level": "WARN",
                "msg": "No changes were in-place file modifications. \
                       Added, removed, or renamed files cannot be automatically absorbed."
            })],
        );
    }

    #[test]
    fn user_defined_base_hides_target_commit() {
        let ctx = repo_utils::prepare_and_stage();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            base: Some("HEAD"),
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1, "Wrong number of commits.");

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Will not fix up past specified base commit. \
                           Consider using --base to specify a different base commit",
                    "base": "HEAD",
                }),
            ],
        );
    }

    #[test]
    fn merge_commit_found() {
        let (ctx, file_path) = repo_utils::prepare_repo();
        repo_utils::merge_commit(
            &ctx.repo,
            &[&ctx.repo.head().unwrap().peel_to_commit().unwrap()],
        );
        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 4, "Wrong number of commits.");

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Cannot fix up past a merge commit",
                }),
            ],
        );
    }

    #[test]
    fn merge_commit_before_target_commit() {
        let (ctx, file_path) = repo_utils::prepare_repo();
        let merge_commit = repo_utils::merge_commit(
            &ctx.repo,
            &[&ctx.repo.head().unwrap().peel_to_commit().unwrap()],
        );

        std::fs::write(&ctx.join(&file_path), "new content").unwrap();
        let tree = repo_utils::add(&ctx.repo, &file_path);
        repo_utils::commit(
            &ctx.repo,
            "HEAD",
            "Change after merge",
            &tree,
            &[&merge_commit],
        );

        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 6, "Wrong number of commits.");

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed",}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": format!(
                        "git rebase --interactive --autosquash --autostash {}",
                        merge_commit.id()),
                }),
            ],
        );
    }

    #[test]
    fn first_hidden_commit_is_merge() {
        let (ctx, file_path) = repo_utils::prepare_repo();
        let merge_commit = repo_utils::merge_commit(
            &ctx.repo,
            &[&ctx.repo.head().unwrap().peel_to_commit().unwrap()],
        );
        repo_utils::empty_commit(&ctx.repo, "HEAD", "empty commit", &[&merge_commit]);
        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let base_id = merge_commit.id().to_string();
        let config = Config {
            base: Some(&base_id),
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 5, "Wrong number of commits.");

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Cannot fix up past a merge commit",
                }),
            ],
        );
    }

    #[test]
    fn first_hidden_commit_is_by_another_author() {
        let (ctx, file_path) = repo_utils::prepare_repo();
        let first_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        ctx.repo
            .branch("some-branch", &first_commit, false)
            .unwrap();
        repo_utils::become_author(&ctx.repo, "nobody2", "nobody2@example.com");
        repo_utils::empty_commit(&ctx.repo, "HEAD", "empty commit", &[&first_commit]);
        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 2, "Wrong number of commits.");

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Will not fix up past commits by another author. \
                           Use --force-author to override",
                }),
            ],
        );
    }

    #[test]
    fn first_hidden_commit_is_regular_commit() {
        let (ctx, file_path) = repo_utils::prepare_repo();
        let first_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        ctx.repo
            .branch("some-branch", &first_commit, false)
            .unwrap();
        repo_utils::empty_commit(&ctx.repo, "HEAD", "empty commit", &[&first_commit]);
        repo_utils::stage_file_changes(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 2, "Wrong number of commits.");

        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Will not fix up commits reachable by other branches. \
                           Use --base to specify a base commit.",
                }),
            ],
        );
    }

    #[test]
    fn one_fixup_per_commit() {
        let ctx = repo_utils::prepare_and_stage();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            one_fixup_per_commit: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 2);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "INFO",
                    "msg": "committed",
                    "fixup": "Initial commit.",
                    "header": "3 insertions(+)",
                }),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn another_author() {
        let ctx = repo_utils::prepare_and_stage();

        repo_utils::become_author(&ctx.repo, "nobody2", "nobody2@example.com");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);
        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Will not fix up past commits by another author. \
                           Use --force-author to override"
                }),
            ],
        );
    }

    #[test]
    fn another_author_with_force_author_flag() {
        let ctx = repo_utils::prepare_and_stage();

        repo_utils::become_author(&ctx.repo, "nobody2", "nobody2@example.com");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            force_author: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 3);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn another_author_with_force_author_config() {
        let ctx = repo_utils::prepare_and_stage();

        repo_utils::become_author(&ctx.repo, "nobody2", "nobody2@example.com");

        repo_utils::set_config_flag(&ctx.repo, "absorb.forceAuthor");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 3);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn detached_head() {
        let ctx = repo_utils::prepare_and_stage();
        repo_utils::detach_head(&ctx.repo);

        // run 'git-absorb'
        let capturing_logger = log_utils::CapturingLogger::new();
        let result = run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo);
        assert_eq!(
            result.err().unwrap().to_string(),
            "HEAD is not a branch, use --force-detach to override"
        );

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);
        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);
    }

    #[test]
    fn detached_head_pointing_at_branch_with_force_detach_flag() {
        let ctx = repo_utils::prepare_and_stage();
        repo_utils::detach_head(&ctx.repo);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            force_detach: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();
        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        assert_eq!(revwalk.count(), 1); // nothing was committed
        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "HEAD is not a branch, but --force-detach used to continue."}),
                &json!({
                    "level": "WARN",
                    "msg": "Some file modifications did not have an available commit to fix up. \
                           You will have to manually create fixup commits.",
                }),
                &json!({
                    "level": "WARN",
                    "msg": "Will not fix up commits reachable by other branches. \
                    Use --base to specify a base commit."
                }),
            ],
        );
    }

    #[test]
    fn detached_head_with_force_detach_flag() {
        let ctx = repo_utils::prepare_and_stage();
        repo_utils::detach_head(&ctx.repo);
        repo_utils::delete_branch(&ctx.repo, "master");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            force_detach: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();
        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        assert_eq!(revwalk.count(), 3);
        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "HEAD is not a branch, but --force-detach used to continue.",
                }),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn detached_head_with_force_detach_config() {
        let ctx = repo_utils::prepare_and_stage();
        repo_utils::detach_head(&ctx.repo);
        repo_utils::delete_branch(&ctx.repo, "master");

        repo_utils::set_config_flag(&ctx.repo, "absorb.forceDetach");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();
        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        assert_eq!(revwalk.count(), 3);
        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "WARN",
                    "msg": "HEAD is not a branch, but --force-detach used to continue.",
                }),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn and_rebase_flag() {
        let ctx = repo_utils::prepare_and_stage();
        repo_utils::set_config_option(&ctx.repo, "core.editor", "true");
        repo_utils::set_config_option(&ctx.repo, "advice.waitingForEditor", "false");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            and_rebase: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        assert_eq!(revwalk.count(), 1);
        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
            ],
        );
    }

    #[test]
    fn and_rebase_flag_with_rebase_options() {
        let ctx = repo_utils::prepare_and_stage();
        repo_utils::set_config_option(&ctx.repo, "core.editor", "true");
        repo_utils::set_config_option(&ctx.repo, "advice.waitingForEditor", "false");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            and_rebase: true,
            rebase_options: &vec!["--signoff"],
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);

        let trailers = message_trailers_strs(
            ctx.repo
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .message()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            trailers
                .iter()
                .filter(|trailer| trailer.0 == "Signed-off-by")
                .count(),
            1
        );

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
            ],
        );
    }

    #[test]
    fn rebase_options_without_and_rebase_flag() {
        let ctx = repo_utils::prepare_and_stage();

        // run 'git-absorb'
        let capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            rebase_options: &vec!["--some-option"],
            ..DEFAULT_CONFIG
        };
        let result = run_with_repo(&capturing_logger.logger, &config, &ctx.repo);

        assert_eq!(
            result.err().unwrap().to_string(),
            "REBASE_OPTIONS were specified without --and-rebase flag"
        );

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);
        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);
    }

    #[test]
    fn squash_flag() {
        let ctx = repo_utils::prepare_and_stage();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            squash: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        assert_eq!(
            extract_commit_messages(&ctx.repo),
            vec![
                "squash! Initial commit.\n",
                "squash! Initial commit.\n",
                "Initial commit.",
            ]
        );

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn run_with_squash_config_option() {
        let ctx = repo_utils::prepare_and_stage();

        repo_utils::set_config_flag(&ctx.repo, "absorb.createSquashCommits");

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        assert_eq!(
            extract_commit_messages(&ctx.repo),
            vec![
                "squash! Initial commit.\n",
                "squash! Initial commit.\n",
                "Initial commit.",
            ]
        );

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn dry_run_flag() {
        let ctx = repo_utils::prepare_and_stage();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            dry_run: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);
        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        let pre_absorb_ref_commit = ctx.repo.references_glob("PRE_ABSORB_HEAD").unwrap().last();
        assert!(pre_absorb_ref_commit.is_none());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({
                    "level": "INFO",
                    "msg": "would have committed",
                    "fixup": "Initial commit.",
                    "header": "1 insertion(+)",
                }),
                &json!({
                    "level": "INFO",
                    "msg": "would have committed",
                    "fixup": "Initial commit.",
                    "header": "2 insertions(+)",
                }),
            ],
        );
    }

    #[test]
    fn dry_run_flag_with_and_rebase_flag() {
        let (ctx, path) = repo_utils::prepare_repo();
        repo_utils::set_config_option(&ctx.repo, "core.editor", "true");

        // create a fixup commit that 'git rebase' will act on if called
        let tree = repo_utils::stage_file_changes(&ctx, &path);
        let head_commit = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        let fixup_message = format!("fixup! {}\n", head_commit.id());
        repo_utils::commit(&ctx.repo, "HEAD", &fixup_message, &tree, &[&head_commit]);

        // stage one more change so 'git-absorb' won't exit early
        repo_utils::stage_file_changes(&ctx, &path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            and_rebase: true,
            dry_run: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 2); // git rebase wasn't called so both commits persist
        let is_something_in_index = !nothing_left_in_index(&ctx.repo).unwrap();
        assert!(is_something_in_index);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "would have committed",}),
                &json!({"level": "INFO", "msg": "would have committed",}),
                &json!({"level": "INFO", "msg": "would have run git rebase",}),
            ],
        );
    }

    fn autostage_common(ctx: &repo_utils::Context, file_path: &PathBuf) -> (PathBuf, PathBuf) {
        // 1 modification w/o staging
        let path = ctx.join(file_path);
        let contents = std::fs::read_to_string(&path).unwrap();
        let modifications = format!("{contents}\nnew_line2");
        std::fs::write(&path, &modifications).unwrap();

        // 1 extra file
        let fp2 = PathBuf::from("unrel.txt");
        std::fs::write(ctx.join(&fp2), "foo").unwrap();

        (path, fp2)
    }

    #[test]
    fn autostage_if_index_was_empty() {
        let (ctx, file_path) = repo_utils::prepare_repo();

        // requires enabled config var
        ctx.repo
            .config()
            .unwrap()
            .set_bool(config::AUTO_STAGE_IF_NOTHING_STAGED_CONFIG_NAME, true)
            .unwrap();

        autostage_common(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 2);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn do_not_autostage_if_index_was_not_empty() {
        let (ctx, file_path) = repo_utils::prepare_repo();

        // enable config var
        ctx.repo
            .config()
            .unwrap()
            .set_bool(config::AUTO_STAGE_IF_NOTHING_STAGED_CONFIG_NAME, true)
            .unwrap();

        let (_, fp2) = autostage_common(&ctx, &file_path);
        // we stage the extra file - should stay in index
        repo_utils::add(&ctx.repo, &fp2);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);

        assert_eq!(index_stats(&ctx.repo).unwrap().files_changed(), 1);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![&json!({
                    "level": "WARN",
                    "msg": "No changes were in-place file modifications. \
                           Added, removed, or renamed files cannot be automatically absorbed."
            })],
        );
    }

    #[test]
    fn do_not_autostage_if_not_enabled_by_config_var() {
        let (ctx, file_path) = repo_utils::prepare_repo();

        // disable config var
        ctx.repo
            .config()
            .unwrap()
            .set_bool(config::AUTO_STAGE_IF_NOTHING_STAGED_CONFIG_NAME, false)
            .unwrap();

        autostage_common(&ctx, &file_path);

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![&json!({
                "level": "WARN",
                "msg": format!(
                    "No changes staged. \
                    Try adding something to the index or set {} = true.",
                    config::AUTO_STAGE_IF_NOTHING_STAGED_CONFIG_NAME,
                ),
            })],
        );
    }

    #[test]
    fn autostage_if_index_was_empty_and_no_changes() {
        let (ctx, _file_path) = repo_utils::prepare_repo();

        // requires enabled config var
        ctx.repo
            .config()
            .unwrap()
            .set_bool(config::AUTO_STAGE_IF_NOTHING_STAGED_CONFIG_NAME, true)
            .unwrap();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();
        assert_eq!(revwalk.count(), 1);

        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![&json!({
                    "level": "WARN",
                    "msg": "No changes staged, even after auto-staging. \
                           Try adding something to the index."})],
        );
    }

    #[test]
    fn fixup_message_always_commit_sha_if_configured() {
        let ctx = repo_utils::prepare_and_stage();

        ctx.repo
            .config()
            .unwrap()
            .set_bool(config::FIXUP_TARGET_ALWAYS_SHA_CONFIG_NAME, true)
            .unwrap();

        // run 'git-absorb'
        let mut capturing_logger = log_utils::CapturingLogger::new();
        run_with_repo(&capturing_logger.logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();
        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        let oids: Vec<git2::Oid> = revwalk.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(oids.len(), 3);

        let commit = ctx.repo.find_commit(oids[0]).unwrap();
        let actual_msg = commit.summary().unwrap();
        let expected_msg = format!("fixup! {}", oids.last().unwrap());
        assert_eq!(actual_msg, expected_msg);

        log_utils::assert_log_messages_are(
            capturing_logger.visible_logs(),
            vec![
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({"level": "INFO", "msg": "committed"}),
                &json!({
                    "level": "INFO",
                    "msg": "To squash the new commits, rebase:",
                    "command": "git rebase --interactive --autosquash --autostash --root",
                }),
            ],
        );
    }

    #[test]
    fn fixup_message_option_left_out_sets_only_summary() {
        let ctx = repo_utils::prepare_and_stage();

        // run 'git-absorb'
        let drain = slog::Discard;
        let logger = slog::Logger::root(drain, o!());
        run_with_repo(&logger, &DEFAULT_CONFIG, &ctx.repo).unwrap();
        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        let oids: Vec<git2::Oid> = revwalk.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(oids.len(), 3);

        let fixup_commit = ctx.repo.find_commit(oids[0]).unwrap();
        let fixed_up_commit = ctx.repo.find_commit(*oids.last().unwrap()).unwrap();
        let actual_msg = fixup_commit.message().unwrap();
        let expected_msg = fixed_up_commit.message().unwrap();
        let expected_msg = format!("fixup! {}\n", expected_msg);
        assert_eq!(actual_msg, expected_msg);
    }

    #[test]
    fn fixup_message_option_provided_sets_message() {
        let ctx = repo_utils::prepare_and_stage();

        // run 'git-absorb'
        let drain = slog::Discard;
        let logger = slog::Logger::root(drain, o!());
        let fixup_message_body = "git-absorb is my favorite git tool!";
        let config = Config {
            message: Some(fixup_message_body),
            ..DEFAULT_CONFIG
        };
        run_with_repo(&logger, &config, &ctx.repo).unwrap();
        assert!(nothing_left_in_index(&ctx.repo).unwrap());

        let mut revwalk = ctx.repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        let oids: Vec<git2::Oid> = revwalk.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(oids.len(), 3);

        let fixup_commit = ctx.repo.find_commit(oids[0]).unwrap();
        let fixed_up_commit = ctx.repo.find_commit(*oids.last().unwrap()).unwrap();
        let actual_msg = fixup_commit.message().unwrap();
        let expected_msg = fixed_up_commit.message().unwrap();
        let expected_msg = format!("fixup! {}\n\n{}\n", expected_msg, fixup_message_body);
        assert_eq!(actual_msg, expected_msg);
    }

    #[test]
    fn hg_linelog_maps_separated_edits_to_original_commits() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        stage_linelog_worktree(&ctx, &path, "a2c");

        let targets = absorb_linelog_stack(&ctx, &base);

        assert_eq!(targets, vec!["commit 1", "commit 3"]);
        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn hg_linelog_maps_middle_line_deletion_to_original_commit() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        stage_linelog_worktree(&ctx, &path, "13");

        let targets = absorb_linelog_stack(&ctx, &base);

        assert_eq!(targets, vec!["commit 2"]);
        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn hg_linelog_maps_file_boundary_insertions_to_neighboring_commits() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        stage_linelog_worktree(&ctx, &path, "a123c");

        let targets = absorb_linelog_stack(&ctx, &base);

        assert_eq!(targets, vec!["commit 1", "commit 3"]);
        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn hg_linelog_rejects_ambiguous_interior_insertion() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        stage_linelog_worktree(&ctx, &path, "1a23");

        let targets = absorb_linelog_stack(&ctx, &base);

        assert!(targets.is_empty());
        assert!(!nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn hg_linelog_rejects_non_one_to_one_replacement() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        stage_linelog_worktree(&ctx, &path, "abcd");

        let targets = absorb_linelog_stack(&ctx, &base);

        assert!(targets.is_empty());
        assert!(!nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn linelog_mode_can_be_enabled_by_config() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        repo_utils::set_config_flag(&ctx.repo, config::USE_LINELOG_CONFIG_NAME);
        stage_linelog_worktree(&ctx, &path, "a2c");

        let targets = absorb_stack(&ctx, &base, DEFAULT_CONFIG);

        assert_eq!(targets, vec!["commit 1", "commit 3"]);
        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn linelog_mode_rejects_whole_file_mode() {
        let (ctx, _path, base) = prepare_linelog_stack(&["", "1"]);
        let config = Config {
            base: Some(&base),
            linelog: true,
            whole_file: true,
            ..DEFAULT_CONFIG
        };

        let capturing_logger = log_utils::CapturingLogger::new();
        let err = run_with_repo(&capturing_logger.logger, &config, &ctx.repo)
            .err()
            .unwrap()
            .to_string();

        assert!(err.contains("--linelog cannot be combined with --whole-file"));
    }

    #[test]
    fn rewrite_absorbs_separated_edits_into_stack() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        stage_linelog_worktree(&ctx, &path, "a2c");

        rewrite_linelog_stack(&ctx, &base, DEFAULT_CONFIG);

        assert_eq!(
            stack_file_contents(&ctx.repo, &base, &path),
            vec!["a", "a2", "a2c"]
        );
        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn rewrite_absorbs_middle_line_deletion_into_owner_commit() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        stage_linelog_worktree(&ctx, &path, "13");

        rewrite_linelog_stack(&ctx, &base, DEFAULT_CONFIG);

        assert_eq!(
            stack_file_contents(&ctx.repo, &base, &path),
            vec!["1", "1", "13"]
        );
        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn rewrite_leaves_ambiguous_interior_insertion_staged() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        let old_head = ctx.repo.head().unwrap().target().unwrap();
        stage_linelog_worktree(&ctx, &path, "1a23");

        rewrite_linelog_stack(&ctx, &base, DEFAULT_CONFIG);

        assert_eq!(ctx.repo.head().unwrap().target().unwrap(), old_head);
        assert!(!nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn rewrite_leaves_non_one_to_one_replacement_staged() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        let old_head = ctx.repo.head().unwrap().target().unwrap();
        stage_linelog_worktree(&ctx, &path, "abcd");

        rewrite_linelog_stack(&ctx, &base, DEFAULT_CONFIG);

        assert_eq!(ctx.repo.head().unwrap().target().unwrap(), old_head);
        assert!(!nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn rewrite_leaves_non_modified_patch_staged() {
        let (ctx, _path, base) = prepare_linelog_stack(&["", "1"]);
        std::fs::write(ctx.join(Path::new("new-file.txt")), "new\n").unwrap();
        repo_utils::add(&ctx.repo, Path::new("new-file.txt"));

        rewrite_linelog_stack(&ctx, &base, DEFAULT_CONFIG);

        assert!(!nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn rewrite_autostage_resets_index_to_rewritten_head() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        std::fs::write(ctx.join(&path), chars_as_lines("a2c")).unwrap();
        repo_utils::set_config_flag(&ctx.repo, config::AUTO_STAGE_IF_NOTHING_STAGED_CONFIG_NAME);

        rewrite_linelog_stack(&ctx, &base, DEFAULT_CONFIG);

        assert_eq!(
            stack_file_contents(&ctx.repo, &base, &path),
            vec!["a", "a2", "a2c"]
        );
        assert!(nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn rewrite_dry_run_moves_no_refs_and_writes_no_pre_absorb_head() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        let old_head = ctx.repo.head().unwrap().target().unwrap();
        stage_linelog_worktree(&ctx, &path, "a2c");

        rewrite_linelog_stack(
            &ctx,
            &base,
            Config {
                dry_run: true,
                ..DEFAULT_CONFIG
            },
        );

        assert_eq!(ctx.repo.head().unwrap().target().unwrap(), old_head);
        assert!(ctx.repo.find_reference("PRE_ABSORB_HEAD").is_err());
        assert!(!nothing_left_in_index(&ctx.repo).unwrap());
    }

    #[test]
    fn rewrite_dry_run_reports_only_commits_that_would_change() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123", "1234"]);
        stage_linelog_worktree(&ctx, &path, "123z");

        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            base: Some(&base),
            rewrite: true,
            dry_run: true,
            ..DEFAULT_CONFIG
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        let would_rewrite_count = capturing_logger
            .visible_logs()
            .into_iter()
            .filter(|log| log["msg"] == "would rewrite commit")
            .count();
        assert_eq!(would_rewrite_count, 1);
    }

    #[test]
    fn rewrite_rejects_fixup_only_flags() {
        let (ctx, _path, base) = prepare_linelog_stack(&["", "1"]);
        let rebase_options = vec!["--keep-empty"];
        let cases = vec![
            (
                Config {
                    rewrite: true,
                    and_rebase: true,
                    ..DEFAULT_CONFIG
                },
                "--and-rebase",
            ),
            (
                Config {
                    rewrite: true,
                    rebase_options: &rebase_options,
                    ..DEFAULT_CONFIG
                },
                "rebase options",
            ),
            (
                Config {
                    rewrite: true,
                    whole_file: true,
                    ..DEFAULT_CONFIG
                },
                "--whole-file",
            ),
            (
                Config {
                    rewrite: true,
                    one_fixup_per_commit: true,
                    ..DEFAULT_CONFIG
                },
                "--one-fixup-per-commit",
            ),
            (
                Config {
                    rewrite: true,
                    squash: true,
                    ..DEFAULT_CONFIG
                },
                "--squash",
            ),
            (
                Config {
                    rewrite: true,
                    message: Some("body"),
                    ..DEFAULT_CONFIG
                },
                "--message",
            ),
        ];

        for (config, expected) in cases {
            let capturing_logger = log_utils::CapturingLogger::new();
            let config = Config {
                base: Some(&base),
                ..config
            };
            let err = run_with_repo(&capturing_logger.logger, &config, &ctx.repo)
                .err()
                .unwrap()
                .to_string();
            assert!(err.contains(expected), "{err}");
        }
    }

    #[test]
    fn rewrite_preserves_author_message_and_updates_parent_links() {
        let (ctx, path, base) = prepare_linelog_stack(&["", "1", "12", "123"]);
        let old_head = ctx.repo.head().unwrap().target().unwrap();
        repo_utils::become_author(&ctx.repo, "rewriter", "rewriter@example.com");
        stage_linelog_worktree(&ctx, &path, "a2c");

        rewrite_linelog_stack(
            &ctx,
            &base,
            Config {
                force_author: true,
                ..DEFAULT_CONFIG
            },
        );

        let commits = stack_commits_oldest_to_newest(&ctx.repo, &base);
        assert_eq!(commits.len(), 3);
        for (idx, commit) in commits.iter().enumerate() {
            assert_eq!(commit.message().unwrap(), format!("commit {}", idx + 1));
            assert_eq!(commit.author().name(), Some("nobody"));
            assert_eq!(commit.committer().name(), Some("rewriter"));
            if idx > 0 {
                assert_eq!(commit.parent(0).unwrap().id(), commits[idx - 1].id());
            }
        }
        assert_eq!(
            ctx.repo
                .find_reference("PRE_ABSORB_HEAD")
                .unwrap()
                .target()
                .unwrap(),
            old_head
        );
    }

    #[test]
    fn rewrite_preserves_non_utf8_commit_message_bytes() {
        let (ctx, path, base) = prepare_linelog_stack(&[""]);
        let message = b"bad-\xff-message\n";

        std::fs::write(ctx.join(&path), chars_as_lines("1")).unwrap();
        let tree = repo_utils::add(&ctx.repo, &path);
        let parent = ctx
            .repo
            .find_commit(git2::Oid::from_str(&base).unwrap())
            .unwrap();
        let commit = raw_commit(
            &ctx.repo,
            tree.id(),
            &[parent.id()],
            b"author nobody <nobody@example.com> 0 +0000\n",
            b"committer nobody <nobody@example.com> 0 +0000\n",
            message,
        );
        ctx.repo
            .reference("refs/heads/master", commit, true, "")
            .unwrap();

        stage_linelog_worktree(&ctx, &path, "a");
        rewrite_linelog_stack(&ctx, &base, DEFAULT_CONFIG);

        let new_head = ctx.repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(new_head.message_raw_bytes(), message);
    }

    // These cases are ported from Mercurial's
    // tests/test-absorb-filefixupstate.py in changeset 5111d11b8719. The
    // character strings model file contents, with each character expanded to
    // one line so the tests can describe line ownership compactly.
    fn prepare_linelog_stack(contents: &[&str]) -> (repo_utils::Context, PathBuf, String) {
        assert!(!contents.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init_opts(
            dir.path(),
            git2::RepositoryInitOptions::new().initial_head("master"),
        )
        .unwrap();
        repo_utils::become_author(&repo, "nobody", "nobody@example.com");

        let path = PathBuf::from("linelog.txt");
        std::fs::write(dir.path().join(&path), chars_as_lines(contents[0])).unwrap();

        let base_commit = {
            let tree = repo_utils::add(&repo, &path);
            repo_utils::commit(&repo, "HEAD", "base", &tree, &[])
        };
        let base = base_commit.id().to_string();

        {
            let mut parent = base_commit;
            for (idx, content) in contents.iter().enumerate().skip(1) {
                std::fs::write(dir.path().join(&path), chars_as_lines(content)).unwrap();
                let tree = repo_utils::add(&repo, &path);
                parent =
                    repo_utils::commit(&repo, "HEAD", &format!("commit {idx}"), &tree, &[&parent]);
            }
        }

        (repo_utils::Context { repo, dir }, path, base)
    }

    fn stage_linelog_worktree(ctx: &repo_utils::Context, path: &Path, content: &str) {
        std::fs::write(ctx.join(path), chars_as_lines(content)).unwrap();
        repo_utils::add(&ctx.repo, path);
    }

    fn absorb_linelog_stack(ctx: &repo_utils::Context, base: &str) -> Vec<String> {
        absorb_stack(
            ctx,
            base,
            Config {
                linelog: true,
                ..DEFAULT_CONFIG
            },
        )
    }

    fn rewrite_linelog_stack<'a>(ctx: &repo_utils::Context, base: &'a str, config: Config<'a>) {
        let capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            base: Some(base),
            rewrite: true,
            ..config
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();
    }

    fn stack_file_contents(repo: &git2::Repository, base: &str, path: &Path) -> Vec<String> {
        stack_commits_oldest_to_newest(repo, base)
            .into_iter()
            .map(|commit| commit_file_as_chars(repo, &commit, path))
            .collect()
    }

    fn stack_commits_oldest_to_newest<'repo>(
        repo: &'repo git2::Repository,
        base: &str,
    ) -> Vec<git2::Commit<'repo>> {
        let base = git2::Oid::from_str(base).unwrap();
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        let mut oids = Vec::new();
        for oid in revwalk {
            let oid = oid.unwrap();
            if oid == base {
                break;
            }
            oids.push(oid);
        }
        oids.reverse();
        oids.into_iter()
            .map(|oid| repo.find_commit(oid).unwrap())
            .collect()
    }

    fn commit_file_as_chars(repo: &git2::Repository, commit: &git2::Commit, path: &Path) -> String {
        let tree = commit.tree().unwrap();
        let entry = tree.get_path(path).unwrap();
        let blob = entry.to_object(repo).unwrap().peel_to_blob().unwrap();
        std::str::from_utf8(blob.content())
            .unwrap()
            .lines()
            .collect()
    }

    fn raw_commit(
        repo: &git2::Repository,
        tree: git2::Oid,
        parents: &[git2::Oid],
        author: &[u8],
        committer: &[u8],
        message: &[u8],
    ) -> git2::Oid {
        let mut content = Vec::new();
        content.extend_from_slice(format!("tree {tree}\n").as_bytes());
        for parent in parents {
            content.extend_from_slice(format!("parent {parent}\n").as_bytes());
        }
        content.extend_from_slice(author);
        content.extend_from_slice(committer);
        content.push(b'\n');
        content.extend_from_slice(message);
        repo.odb()
            .unwrap()
            .write(git2::ObjectType::Commit, &content)
            .unwrap()
    }

    fn absorb_stack<'a>(
        ctx: &repo_utils::Context,
        base: &'a str,
        config: Config<'a>,
    ) -> Vec<String> {
        let mut capturing_logger = log_utils::CapturingLogger::new();
        let config = Config {
            base: Some(base),
            ..config
        };
        run_with_repo(&capturing_logger.logger, &config, &ctx.repo).unwrap();

        capturing_logger
            .visible_logs()
            .into_iter()
            .filter(|log| log["msg"] == "committed")
            .map(|log| log["fixup"].as_str().unwrap().to_owned())
            .collect()
    }

    fn chars_as_lines(content: &str) -> String {
        content.chars().flat_map(|ch| [ch, '\n']).collect()
    }

    /// Perform a revwalk from HEAD, extracting the commit messages.
    fn extract_commit_messages(repo: &git2::Repository) -> Vec<String> {
        let mut revwalk = repo.revwalk().unwrap();
        revwalk.push_head().unwrap();

        let mut messages = Vec::new();

        for oid in revwalk {
            let commit = repo.find_commit(oid.unwrap()).unwrap();
            if let Some(message) = commit.message() {
                messages.push(message.to_string());
            }
        }

        messages
    }

    const DEFAULT_CONFIG: Config = Config {
        dry_run: false,
        no_limit: false,
        force_author: false,
        force_detach: false,
        base: None,
        and_rebase: false,
        rebase_options: &Vec::new(),
        whole_file: false,
        linelog: false,
        rewrite: false,
        one_fixup_per_commit: false,
        squash: false,
        message: None,
    };
}
