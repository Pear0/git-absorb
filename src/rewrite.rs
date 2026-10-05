use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::process::Command;

use crate::linelog_mode::{self, StackEntry};
use crate::owned;
use crate::{announce_rewrote_commit, Config};

pub(crate) struct RewriteOutcome {
    pub(crate) modified_hunks_without_target: usize,
    pub(crate) non_modified_patches: usize,
}

// Capture before stack discovery so publication can detect both a moved branch
// and a switched HEAD, including switches to another branch at the same OID.
#[derive(Debug, PartialEq)]
pub(crate) struct HeadState {
    symbolic_target: Option<String>,
    oid: git2::Oid,
}

impl HeadState {
    pub(crate) fn capture(repo: &git2::Repository) -> Result<Self> {
        let head = repo.find_reference("HEAD")?;
        let symbolic_target = match head.symbolic_target_bytes() {
            Some(name) => Some(std::str::from_utf8(name)?.to_owned()),
            None => None,
        };
        Ok(Self {
            symbolic_target,
            oid: head.peel_to_commit()?.id(),
        })
    }
}

pub(crate) fn run(
    repo: &git2::Repository,
    expected_head: &HeadState,
    stack: &[StackEntry],
    index: &owned::Diff,
    config: &Config,
    we_added_everything_to_index: bool,
    logger: &slog::Logger,
) -> Result<RewriteOutcome> {
    if HeadState::capture(repo)? != *expected_head
        || stack
            .first()
            .is_some_and(|(commit, _)| commit.id() != expected_head.oid)
    {
        return Err(anyhow!(
            "HEAD changed while preparing the rewrite; retry git-absorb"
        ));
    }
    let old_head = repo.find_commit(expected_head.oid)?;
    let old_head_id = old_head.id();
    let plan = linelog_mode::plan_rewrite(repo, stack, index, logger)?;
    let outcome = RewriteOutcome {
        modified_hunks_without_target: plan.modified_hunks_without_target,
        non_modified_patches: plan.non_modified_patches,
    };

    if plan.accepted_hunks == 0 {
        if we_added_everything_to_index && !config.dry_run {
            let mut index = repo.index()?;
            index.read_tree(&old_head.tree()?)?;
            index.write()?;
        }
        return Ok(outcome);
    }

    let committer = repo
        .signature()
        .or_else(|_| git2::Signature::now("nobody", "nobody@example.com"))?;
    let mut replacements = HashMap::new();
    let mut events = Vec::new();

    for (commit, _) in stack.iter().rev() {
        let old_parent_ids: Vec<_> = commit.parents().map(|parent| parent.id()).collect();
        let parent_ids: Vec<_> = old_parent_ids
            .iter()
            .map(|oid| replacements.get(oid).copied().unwrap_or(*oid))
            .collect();
        let parent_changed = parent_ids != old_parent_ids;

        let Some((tree, changed_files)) =
            rewritten_tree(repo, commit, &plan.paths, parent_changed)?
        else {
            replacements.insert(commit.id(), commit.id());
            continue;
        };

        let original_tree = commit.tree()?;
        if tree.id() == original_tree.id() && !parent_changed {
            replacements.insert(commit.id(), commit.id());
            continue;
        }

        let new_oid = write_replacement_commit(repo, commit, &committer, &tree, &parent_ids)?;
        replacements.insert(commit.id(), new_oid);
        events.push((commit.id(), new_oid, changed_files));
    }

    let new_head_id = replacements
        .get(&old_head_id)
        .copied()
        .unwrap_or(old_head_id);
    if config.dry_run {
        if new_head_id != old_head_id {
            let comparison = stack_range_diff(repo, old_head_id, new_head_id)?;
            info!(
                logger,
                "Stack range-diff (before -> after):\n{}",
                String::from_utf8_lossy(&comparison)
            );
        }
        return Ok(outcome);
    }

    move_head(repo, expected_head, new_head_id)?;

    if we_added_everything_to_index {
        let new_head_tree = repo.find_commit(new_head_id)?.tree()?;
        let mut index = repo.index()?;
        index.read_tree(&new_head_tree)?;
        index.write()?;
    }

    for (old_oid, new_oid, changed_files) in events {
        announce_rewrote_commit(logger, old_oid, new_oid, changed_files);
    }

    Ok(outcome)
}

/// Compare only the rewritten suffix. Symmetric ranges also include a rewritten
/// root commit, without needing a synthetic base or any temporary refs.
fn stack_range_diff(
    repo: &git2::Repository,
    before: git2::Oid,
    after: git2::Oid,
) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("--no-pager")
        .arg("--git-dir")
        .arg(repo.path())
        // These stacks are rewrites of the same commits. Prefer pairing even
        // small patches whose entire contents changed over delete/add summaries.
        .args([
            "range-diff",
            "--creation-factor=1000",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
        ])
        .arg(format!("{after}..{before}"))
        .arg(format!("{before}..{after}"))
        .arg("--")
        .output()
        .context("could not run git range-diff for the rewrite preview")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git range-diff failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn rewritten_tree<'repo>(
    repo: &'repo git2::Repository,
    commit: &git2::Commit<'repo>,
    paths: &[linelog_mode::PathRewrite],
    parent_changed: bool,
) -> Result<Option<(git2::Tree<'repo>, usize)>> {
    let original_tree = commit.tree()?;
    let mut tree = original_tree.clone();
    let mut changed_files = 0usize;

    for path_plan in paths {
        let Some(content) = path_plan.history.content_for_commit(commit.id()) else {
            continue;
        };
        let before = linelog_mode::blob_content(repo, &tree, &path_plan.path)?;
        if before.is_none() && content.is_empty() {
            continue;
        }
        if before.as_deref() == Some(content.as_slice()) {
            continue;
        }
        tree = replace_path_content(repo, &tree, &path_plan.path, &content)?;
        changed_files += 1;
    }

    if changed_files == 0 && !parent_changed {
        Ok(None)
    } else {
        Ok(Some((tree, changed_files)))
    }
}

fn write_replacement_commit(
    repo: &git2::Repository,
    original: &git2::Commit,
    committer: &git2::Signature,
    tree: &git2::Tree,
    parent_ids: &[git2::Oid],
) -> Result<git2::Oid> {
    let mut content = Vec::new();
    content.extend_from_slice(format!("tree {}\n", tree.id()).as_bytes());
    for parent_id in parent_ids {
        content.extend_from_slice(format!("parent {}\n", parent_id).as_bytes());
    }
    append_signature(&mut content, b"author", &original.author());
    append_signature(&mut content, b"committer", committer);
    if let Some(encoding) = original.message_encoding() {
        content.extend_from_slice(b"encoding ");
        content.extend_from_slice(encoding.as_bytes());
        content.push(b'\n');
    }
    content.push(b'\n');
    content.extend_from_slice(original.message_raw_bytes());

    let odb = repo.odb()?;
    Ok(odb.write(git2::ObjectType::Commit, &content)?)
}

fn append_signature(content: &mut Vec<u8>, header: &[u8], signature: &git2::Signature) {
    content.extend_from_slice(header);
    content.push(b' ');
    content.extend_from_slice(signature.name_bytes());
    content.extend_from_slice(b" <");
    content.extend_from_slice(signature.email_bytes());
    content.extend_from_slice(b"> ");
    content.extend_from_slice(signature.when().seconds().to_string().as_bytes());
    content.push(b' ');

    let offset = signature.when().offset_minutes();
    let sign = if offset < 0 { '-' } else { '+' };
    let offset = offset.abs();
    content.extend_from_slice(format!("{sign}{:02}{:02}\n", offset / 60, offset % 60).as_bytes());
}

fn move_head(repo: &git2::Repository, expected: &HeadState, new_head_id: git2::Oid) -> Result<()> {
    let mut transaction = repo.transaction()?;
    transaction.lock_ref("HEAD")?;
    let target = expected.symbolic_target.as_deref().unwrap_or("HEAD");
    if target != "HEAD" {
        transaction.lock_ref(target)?;
    }
    transaction.lock_ref("PRE_ABSORB_HEAD")?;
    // Compare only after acquiring locks; keep them until publication completes.
    if HeadState::capture(repo)? != *expected {
        return Err(anyhow!("HEAD changed during the rewrite; retry git-absorb"));
    }
    transaction.set_target("PRE_ABSORB_HEAD", expected.oid, None, "git-absorb backup")?;
    transaction.set_target(target, new_head_id, None, "git-absorb rewrite")?;
    transaction.commit()?;
    Ok(())
}

fn replace_path_content<'repo>(
    repo: &'repo git2::Repository,
    base: &git2::Tree,
    path: &[u8],
    content: &[u8],
) -> Result<git2::Tree<'repo>> {
    let Some((first, rest)) = linelog_mode::split_path(path) else {
        return Err(anyhow!("cannot rewrite an empty path"));
    };

    let mut builder = repo.treebuilder(Some(base))?;
    if rest.is_empty() {
        let mode = builder
            .get(first)?
            .map(|entry| entry.filemode())
            .unwrap_or(0o100644);
        let blob = repo.blob(content)?;
        builder.insert(first, blob, mode)?;
        return Ok(repo.find_tree(builder.write()?)?);
    }

    let (subtree, submode) = match builder.get(first)? {
        Some(entry) => (repo.find_tree(entry.id())?, entry.filemode()),
        None => {
            let empty = repo.treebuilder(None)?.write()?;
            (repo.find_tree(empty)?, 0o040000)
        }
    };
    let new_subtree = replace_path_content(repo, &subtree, rest, content)?;
    builder.insert(first, new_subtree.id(), submode)?;
    Ok(repo.find_tree(builder.write()?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::repo_utils;

    #[test]
    fn publication_rejects_concurrent_commit_or_branch_switch() {
        for switch in [false, true] {
            let (ctx, _) = repo_utils::prepare_repo();
            let expected = HeadState::capture(&ctx.repo).unwrap();
            let original = ctx.repo.find_commit(expected.oid).unwrap();
            if switch {
                ctx.repo.branch("other", &original, false).unwrap();
                ctx.repo.set_head("refs/heads/other").unwrap();
            } else {
                repo_utils::empty_commit(&ctx.repo, "HEAD", "concurrent", &[&original]);
            }
            let concurrent = HeadState::capture(&ctx.repo).unwrap();
            let err = move_head(&ctx.repo, &expected, original.id()).unwrap_err();
            assert!(err.to_string().contains("HEAD changed"));
            assert_eq!(HeadState::capture(&ctx.repo).unwrap(), concurrent);
            assert!(ctx.repo.find_reference("PRE_ABSORB_HEAD").is_err());
        }
    }

    #[test]
    fn publication_updates_attached_and_detached_heads_and_backup() {
        for detached in [false, true] {
            let (ctx, _) = repo_utils::prepare_repo();
            let original = ctx.repo.head().unwrap().peel_to_commit().unwrap();
            if detached {
                ctx.repo.set_head_detached(original.id()).unwrap();
            }
            let expected = HeadState::capture(&ctx.repo).unwrap();
            let sig = ctx.repo.signature().unwrap();
            let replacement = ctx
                .repo
                .commit(
                    None,
                    &sig,
                    &sig,
                    "replacement",
                    &original.tree().unwrap(),
                    &[&original],
                )
                .unwrap();
            move_head(&ctx.repo, &expected, replacement).unwrap();
            assert_eq!(ctx.repo.head().unwrap().target(), Some(replacement));
            assert_eq!(ctx.repo.head_detached().unwrap(), detached);
            assert_eq!(
                ctx.repo.find_reference("PRE_ABSORB_HEAD").unwrap().target(),
                Some(original.id())
            );
        }
    }
}
