use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};

use crate::linelog_mode::{self, StackEntry};
use crate::owned;
use crate::{announce_rewrote_commit, announce_would_rewrite_commit, Config};

pub(crate) struct RewriteOutcome {
    pub(crate) modified_hunks_without_target: usize,
    pub(crate) non_modified_patches: usize,
}

pub(crate) fn run(
    repo: &git2::Repository,
    stack: &[StackEntry],
    index: &owned::Diff,
    config: &Config,
    we_added_everything_to_index: bool,
    logger: &slog::Logger,
) -> Result<RewriteOutcome> {
    let old_head = repo.head()?.peel_to_commit()?;
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

    if config.dry_run {
        let mut changed_commits = HashSet::new();
        for (commit, _) in stack.iter().rev() {
            let parent_changed = commit
                .parents()
                .any(|parent| changed_commits.contains(&parent.id()));
            if commit_changed(repo, commit, &plan.paths, parent_changed)?.is_some() {
                changed_commits.insert(commit.id());
                announce_would_rewrite_commit(logger, commit.id());
            }
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
    repo.reference("PRE_ABSORB_HEAD", old_head_id, true, "")?;
    move_head(repo, new_head_id)?;

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

fn commit_changed(
    repo: &git2::Repository,
    commit: &git2::Commit,
    paths: &[linelog_mode::PathRewrite],
    parent_changed: bool,
) -> Result<Option<usize>> {
    rewritten_tree(repo, commit, paths, parent_changed)
        .map(|changed| changed.map(|(_, files)| files))
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

fn move_head(repo: &git2::Repository, new_head_id: git2::Oid) -> Result<()> {
    let head = repo.head()?;
    if head.is_branch() {
        let name = head
            .name()
            .ok_or_else(|| anyhow!("current branch name is not valid UTF-8"))?;
        repo.reference(name, new_head_id, true, "git-absorb rewrite")?;
        repo.set_head(name)?;
    } else {
        repo.set_head_detached(new_head_id)?;
    }
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
