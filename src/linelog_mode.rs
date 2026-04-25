use anyhow::{anyhow, Result};
use linelog::AbstractLineLog;
use std::collections::{BTreeSet, HashMap};

use crate::owned;
use crate::{HunkAttribution, HunkWithCommit};

pub(crate) type StackEntry<'r> = (git2::Commit<'r>, owned::Diff);

pub(crate) fn assign_hunks<'c, 'r, 'p>(
    repo: &git2::Repository,
    stack: &'c [StackEntry<'r>],
    index: &'p owned::Diff,
    logger: &slog::Logger,
) -> Result<HunkAttribution<'c, 'r, 'p>> {
    let mut hunks_with_commit = Vec::new();
    let mut modified_hunks_without_target = 0usize;
    let mut non_modified_patches = 0usize;

    for index_patch in index.iter() {
        let path = index_patch.new_path.as_slice();
        if index_patch.status != git2::Delta::Modified {
            debug!(logger, "skipped non-modified patch";
                    "path" => String::from_utf8_lossy(path).into_owned(),
                    "status" => format!("{:?}", index_patch.status),
            );
            non_modified_patches += 1;
            continue;
        }

        let Some(history) = FileLineLog::build(repo, stack, path)? else {
            debug!(logger, "skipped linelog path without usable linear history";
                   "path" => String::from_utf8_lossy(path).into_owned());
            modified_hunks_without_target += index_patch.hunks.len();
            continue;
        };

        let mut preceding_hunks_offset = 0isize;
        let mut applied_hunks_offset = 0isize;
        for index_hunk in &index_patch.hunks {
            let isolated_hunk = index_hunk
                .clone()
                .shift_added_block(-preceding_hunks_offset);
            preceding_hunks_offset += index_hunk.changed_offset();

            let fixups = history.analyze_hunk(&isolated_hunk);
            if fixups.is_empty() {
                modified_hunks_without_target += 1;
                continue;
            }

            for fixup in fixups {
                let Some(dest_commit) = history.commit_for_original_rev(fixup.rev) else {
                    modified_hunks_without_target += 1;
                    continue;
                };
                let hunk_to_apply = fixup
                    .to_hunk(&isolated_hunk)
                    .shift_both_blocks(applied_hunks_offset);
                applied_hunks_offset += hunk_to_apply.changed_offset();

                hunks_with_commit.push(HunkWithCommit {
                    hunk_to_apply,
                    dest_commit,
                    index_patch,
                });
            }
        }
    }

    Ok(HunkAttribution {
        hunks_with_commit,
        modified_hunks_without_target,
        non_modified_patches,
    })
}

pub(crate) struct RewritePlan<'c, 'r> {
    pub(crate) paths: Vec<PathRewrite<'c, 'r>>,
    pub(crate) modified_hunks_without_target: usize,
    pub(crate) non_modified_patches: usize,
    pub(crate) accepted_hunks: usize,
}

pub(crate) struct PathRewrite<'c, 'r> {
    pub(crate) path: Vec<u8>,
    pub(crate) history: FileLineLog<'c, 'r>,
}

pub(crate) fn plan_rewrite<'c, 'r>(
    repo: &git2::Repository,
    stack: &'c [StackEntry<'r>],
    index: &owned::Diff,
    logger: &slog::Logger,
) -> Result<RewritePlan<'c, 'r>> {
    let mut paths = Vec::new();
    let mut modified_hunks_without_target = 0usize;
    let mut non_modified_patches = 0usize;
    let mut accepted_hunks = 0usize;

    for index_patch in index.iter() {
        let path = index_patch.new_path.as_slice();
        if index_patch.status != git2::Delta::Modified {
            debug!(logger, "skipped non-modified patch";
                    "path" => String::from_utf8_lossy(path).into_owned(),
                    "status" => format!("{:?}", index_patch.status),
            );
            non_modified_patches += 1;
            continue;
        }

        let Some(mut history) = FileLineLog::build(repo, stack, path)? else {
            debug!(logger, "skipped linelog path without usable linear history";
                   "path" => String::from_utf8_lossy(path).into_owned());
            modified_hunks_without_target += index_patch.hunks.len();
            continue;
        };

        let mut preceding_hunks_offset = 0isize;
        let mut edits = Vec::new();
        for index_hunk in &index_patch.hunks {
            let isolated_hunk = index_hunk
                .clone()
                .shift_added_block(-preceding_hunks_offset);
            preceding_hunks_offset += index_hunk.changed_offset();

            let fixups = history.analyze_hunk(&isolated_hunk);
            if fixups.is_empty() {
                modified_hunks_without_target += 1;
                continue;
            }

            accepted_hunks += 1;
            for fixup in fixups {
                edits.push((fixup.added_lines(&isolated_hunk), fixup));
            }
        }

        if !edits.is_empty() {
            history.apply_rewrite_edits(edits)?;
            paths.push(PathRewrite {
                path: path.to_vec(),
                history,
            });
        }
    }

    Ok(RewritePlan {
        paths,
        modified_hunks_without_target,
        non_modified_patches,
        accepted_hunks,
    })
}

pub(crate) struct FileLineLog<'c, 'r> {
    log: AbstractLineLog<Vec<u8>>,
    rev_to_commit: HashMap<usize, &'c git2::Commit<'r>>,
    rev_to_rewrite_rev: HashMap<usize, usize>,
    commit_to_rewrite_rev: HashMap<git2::Oid, usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct LineFixup {
    rev: usize,
    a1: usize,
    a2: usize,
    b1: usize,
    b2: usize,
}

impl<'c, 'r> FileLineLog<'c, 'r> {
    pub(crate) fn build(
        repo: &git2::Repository,
        stack: &'c [StackEntry<'r>],
        path: &[u8],
    ) -> Result<Option<Self>> {
        let Some((oldest_commit, _)) = stack.last() else {
            return Ok(None);
        };

        let base_content = if oldest_commit.parent_count() == 0 {
            Vec::new()
        } else {
            let base_tree = oldest_commit.parent(0)?.tree()?;
            blob_content(repo, &base_tree, path)?.unwrap_or_default()
        };

        let mut log = AbstractLineLog::<Vec<u8>>::default().edit_chunk(
            0,
            0,
            0,
            1,
            split_lines(&base_content),
        );
        let mut rev_to_commit = HashMap::new();
        let mut rev_to_rewrite_rev = HashMap::new();
        let mut commit_to_rewrite_rev = HashMap::new();

        for (idx, (commit, diff)) in stack.iter().rev().enumerate() {
            let original_rev = 3 + (idx * 2);
            let rewrite_rev = original_rev + 1;
            rev_to_commit.insert(original_rev, commit);
            rev_to_rewrite_rev.insert(original_rev, rewrite_rev);
            commit_to_rewrite_rev.insert(commit.id(), rewrite_rev);

            if let Some(old_patch) = diff.by_old(path) {
                if old_patch.new_path.as_slice() != path {
                    return Ok(None);
                }
            }

            let Some(patch) = diff.by_new(path) else {
                continue;
            };

            match patch.status {
                git2::Delta::Added | git2::Delta::Modified => {}
                _ => return Ok(None),
            }
            if patch.status == git2::Delta::Modified && patch.old_path.as_slice() != path {
                return Ok(None);
            }

            for hunk in patch.hunks.iter().rev() {
                let (a1, a2) = old_range(hunk)?;
                let b_lines = hunk.added.lines.iter().cloned().collect();
                let a_rev = log.max_rev();
                log = log.edit_chunk(a_rev, a1, a2, original_rev, b_lines);
            }
        }

        Ok(Some(Self {
            log,
            rev_to_commit,
            rev_to_rewrite_rev,
            commit_to_rewrite_rev,
        }))
    }

    pub(crate) fn analyze_hunk(&self, hunk: &owned::Hunk) -> Vec<LineFixup> {
        let Ok((a1, a2)) = old_range(hunk) else {
            return Vec::new();
        };
        let b1 = 0;
        let b2 = hunk.added.lines.len();
        let annotated = self.log.checkout_lines(self.log.max_rev());
        if a2 > annotated.len() {
            return Vec::new();
        }

        let mut involved: Vec<_> = annotated.iter().skip(a1).take(a2 - a1).cloned().collect();
        if involved.is_empty() && !annotated.is_empty() {
            let mut nearby = BTreeSet::new();
            nearby.insert(a2);
            nearby.insert(a1.saturating_sub(1));
            involved = nearby
                .into_iter()
                .filter_map(|idx| annotated.get(idx))
                .filter(|line| line.rev > 1)
                .cloned()
                .collect();
        }

        let involved_revs: BTreeSet<_> = involved.iter().map(|line| line.rev).collect();
        let mut fixups = Vec::new();
        let single_owner_continuous = match (annotated.get(a1), annotated.get(a2.saturating_sub(1)))
        {
            (Some(start), Some(end)) => {
                self.is_continuous_by_pc(start.pc, end.pc, a1, a2.saturating_sub(1))
            }
            _ => a1 >= a2.saturating_sub(1),
        };
        if involved_revs.len() == 1 && single_owner_continuous {
            let rev = *involved_revs.iter().next().unwrap();
            if rev > 1 {
                fixups.push(LineFixup {
                    rev,
                    a1,
                    a2,
                    b1,
                    b2,
                });
            }
        } else if a2 - a1 == b2 - b1 || b1 == b2 {
            for i in a1..a2 {
                let Some(line) = annotated.get(i) else {
                    return Vec::new();
                };
                if line.rev <= 1 {
                    continue;
                }
                let (nb1, nb2) = if b1 == b2 {
                    (0, 0)
                } else {
                    let nb1 = b1 + i - a1;
                    (nb1, nb1 + 1)
                };
                fixups.push(LineFixup {
                    rev: line.rev,
                    a1: i,
                    a2: i + 1,
                    b1: nb1,
                    b2: nb2,
                });
            }
        }

        self.optimize_fixups(fixups)
    }

    fn is_continuous_by_pc(&self, start_pc: usize, end_pc: usize, a1: usize, a2: usize) -> bool {
        if a1 >= a2 {
            return true;
        }
        let all_lines = self.log.checkout_range_lines(0, self.log.max_rev());
        let Some(start_pos) = all_lines.iter().position(|line| line.pc == start_pc) else {
            return false;
        };
        let Some(end_pos) = all_lines.iter().position(|line| line.pc == end_pc) else {
            return false;
        };
        if start_pos > end_pos {
            return false;
        }

        all_lines
            .iter()
            .skip(start_pos)
            .take(end_pos - start_pos + 1)
            .filter(|line| line.rev != 0)
            .count()
            == a2 - a1 + 1
    }

    fn optimize_fixups(&self, fixups: Vec<LineFixup>) -> Vec<LineFixup> {
        let mut result: Vec<LineFixup> = Vec::new();
        for fixup in fixups {
            if let Some(last) = result.last_mut() {
                let annotated = self.log.checkout_lines(self.log.max_rev());
                let continuous = match (
                    annotated.get(fixup.a1.saturating_sub(1)),
                    annotated.get(fixup.a1),
                ) {
                    (Some(start), Some(end)) => self.is_continuous_by_pc(
                        start.pc,
                        end.pc,
                        fixup.a1.saturating_sub(1),
                        fixup.a1,
                    ),
                    _ => false,
                };
                if fixup.rev == last.rev && fixup.a1 == last.a2 && fixup.b1 == last.b2 && continuous
                {
                    last.a2 = fixup.a2;
                    last.b2 = fixup.b2;
                    continue;
                }
            }
            result.push(fixup);
        }
        result
    }

    fn commit_for_original_rev(&self, rev: usize) -> Option<&'c git2::Commit<'r>> {
        self.rev_to_commit.get(&rev).copied()
    }

    fn rewrite_rev_for_original_rev(&self, rev: usize) -> Option<usize> {
        self.rev_to_rewrite_rev.get(&rev).copied()
    }

    pub(crate) fn content_for_commit(&self, commit_id: git2::Oid) -> Option<Vec<u8>> {
        let rev = self.commit_to_rewrite_rev.get(&commit_id)?;
        Some(join_lines(
            self.log.checkout_lines(*rev).iter().map(|line| &*line.data),
        ))
    }

    fn apply_rewrite_edits(&mut self, mut edits: Vec<(Vec<Vec<u8>>, LineFixup)>) -> Result<()> {
        edits.sort_by_key(|(_, fixup)| fixup.a1);
        for (lines, fixup) in edits.into_iter().rev() {
            let rewrite_rev = self
                .rewrite_rev_for_original_rev(fixup.rev)
                .ok_or_else(|| anyhow!("linelog owner revision has no rewrite revision"))?;
            let a_rev = self.log.max_rev();
            self.log = std::mem::take(&mut self.log).edit_chunk(
                a_rev,
                fixup.a1,
                fixup.a2,
                rewrite_rev,
                lines,
            );
        }
        Ok(())
    }
}

impl LineFixup {
    fn to_hunk(&self, source: &owned::Hunk) -> owned::Hunk {
        let (source_a1, _) = old_range(source).expect("source hunk must have a valid old range");
        let removed_offset = self.a1 - source_a1;
        let removed_len = self.a2 - self.a1;

        let removed_lines = source.removed.lines[removed_offset..removed_offset + removed_len]
            .iter()
            .cloned()
            .collect();
        let added_lines = source.added.lines[self.b1..self.b2]
            .iter()
            .cloned()
            .collect();

        let removed_start = if removed_len == 0 {
            self.a1
        } else {
            self.a1 + 1
        };
        owned::Hunk {
            removed: owned::Block {
                start: removed_start,
                lines: std::rc::Rc::new(removed_lines),
            },
            added: owned::Block {
                start: self.a1 + 1,
                lines: std::rc::Rc::new(added_lines),
            },
        }
    }

    fn added_lines(&self, source: &owned::Hunk) -> Vec<Vec<u8>> {
        source.added.lines[self.b1..self.b2].to_vec()
    }
}

fn old_range(hunk: &owned::Hunk) -> Result<(usize, usize)> {
    let len = hunk.removed.lines.len();
    if len == 0 {
        Ok((hunk.removed.start, hunk.removed.start))
    } else if hunk.removed.start == 0 {
        Err(anyhow!("removed hunk with lines cannot start at zero"))
    } else {
        let a1 = hunk.removed.start - 1;
        Ok((a1, a1 + len))
    }
}

fn split_lines(content: &[u8]) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (idx, byte) in content.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(content[start..=idx].to_vec());
            start = idx + 1;
        }
    }
    if start < content.len() {
        lines.push(content[start..].to_vec());
    }
    lines
}

fn join_lines<I>(lines: I) -> Vec<u8>
where
    I: IntoIterator,
    I::Item: AsRef<[u8]>,
{
    let mut content = Vec::new();
    for line in lines {
        content.extend_from_slice(line.as_ref());
    }
    content
}

pub(crate) fn blob_content(
    repo: &git2::Repository,
    tree: &git2::Tree,
    path: &[u8],
) -> Result<Option<Vec<u8>>> {
    let Some((first, rest)) = split_path(path) else {
        return Ok(None);
    };
    let Some(entry) = tree.get_name_bytes(first) else {
        return Ok(None);
    };
    if rest.is_empty() {
        let blob = repo.find_blob(entry.id())?;
        return Ok(Some(blob.content().to_vec()));
    }

    let subtree = repo.find_tree(entry.id())?;
    blob_content(repo, &subtree, rest)
}

pub(crate) fn split_path(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() {
        return None;
    }
    match path.iter().position(|byte| *byte == b'/') {
        Some(idx) => Some((&path[..idx], &path[idx + 1..])),
        None => Some((path, &[])),
    }
}
