//! Line-based 3-way merge with git-style conflict markers.
//!
//! Direct port of the Go server's `merge.go`. The LCS is reimplemented rather
//! than pulled from a crate so the hunk-merger's existing test suite continues
//! to anchor exact behavior.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DiffOp {
    Equal,
    Insert,
    Delete,
}

#[derive(Clone, Debug)]
struct DiffEntry {
    op: DiffOp,
    lines: Vec<String>,
}

#[derive(Clone, Debug)]
struct Hunk {
    base_start: usize,
    base_len: usize,
    new_lines: Vec<String>,
}

/// Perform a line-based 3-way merge. `base` is the common ancestor, `ours` is
/// the server's version, `theirs` is the client's version. Returns the merged
/// bytes and whether any conflict markers were inserted.
pub fn three_way_merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> (Vec<u8>, bool) {
    let base_lines = split_lines(std::str::from_utf8(base).unwrap_or(""));
    let our_lines = split_lines(std::str::from_utf8(ours).unwrap_or(""));
    let their_lines = split_lines(std::str::from_utf8(theirs).unwrap_or(""));

    let our_diff = diff_lines(&base_lines, &our_lines);
    let their_diff = diff_lines(&base_lines, &their_lines);

    let our_hunks = to_hunks(&our_diff);
    let their_hunks = to_hunks(&their_diff);

    let (merged, has_conflict) = merge_hunks(&base_lines, &our_hunks, &their_hunks);
    (merged.join("\n").into_bytes(), has_conflict)
}

/// Split text into lines. An empty string produces zero lines (matching the Go
/// special case: `splitLines("") == []`).
fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').map(str::to_owned).collect()
}

/// LCS-based line-level diff.
fn diff_lines(a: &[String], b: &[String]) -> Vec<DiffEntry> {
    let m = a.len();
    let n = b.len();
    let mut dp = vec![vec![0usize; n + 1]; m + 1];

    for i in 1..=m {
        for j in 1..=n {
            if a[i - 1] == b[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else if dp[i - 1][j] >= dp[i][j - 1] {
                dp[i][j] = dp[i - 1][j];
            } else {
                dp[i][j] = dp[i][j - 1];
            }
        }
    }

    // Backtrack, pushing entries onto a stack we reverse at the end.
    let mut stack: Vec<DiffEntry> = Vec::new();
    let (mut i, mut j) = (m, n);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && a[i - 1] == b[j - 1] {
            stack.push(DiffEntry {
                op: DiffOp::Equal,
                lines: vec![a[i - 1].clone()],
            });
            i -= 1;
            j -= 1;
        } else if j > 0 && (i == 0 || dp[i][j - 1] >= dp[i - 1][j]) {
            stack.push(DiffEntry {
                op: DiffOp::Insert,
                lines: vec![b[j - 1].clone()],
            });
            j -= 1;
        } else {
            stack.push(DiffEntry {
                op: DiffOp::Delete,
                lines: vec![a[i - 1].clone()],
            });
            i -= 1;
        }
    }

    stack.reverse();
    compact_diff(stack)
}

/// Merge consecutive entries of the same op.
fn compact_diff(entries: Vec<DiffEntry>) -> Vec<DiffEntry> {
    let mut iter = entries.into_iter();
    let Some(first) = iter.next() else {
        return Vec::new();
    };
    let mut result = Vec::new();
    let mut current = first;
    for entry in iter {
        if entry.op == current.op {
            current.lines.extend(entry.lines);
        } else {
            result.push(std::mem::replace(&mut current, entry));
        }
    }
    result.push(current);
    result
}

/// Group a diff into hunks. A delete immediately followed by an insert collapses
/// into a single replacement hunk.
fn to_hunks(diff: &[DiffEntry]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut base_pos = 0usize;
    let mut i = 0usize;
    while i < diff.len() {
        let entry = &diff[i];
        match entry.op {
            DiffOp::Equal => base_pos += entry.lines.len(),
            DiffOp::Delete => {
                let mut h = Hunk {
                    base_start: base_pos,
                    base_len: entry.lines.len(),
                    new_lines: Vec::new(),
                };
                if i + 1 < diff.len() && diff[i + 1].op == DiffOp::Insert {
                    h.new_lines = diff[i + 1].lines.clone();
                    i += 1;
                }
                base_pos += h.base_len;
                hunks.push(h);
            }
            DiffOp::Insert => {
                hunks.push(Hunk {
                    base_start: base_pos,
                    base_len: 0,
                    new_lines: entry.lines.clone(),
                });
            }
        }
        i += 1;
    }
    hunks
}

/// Walk both sides' hunks in base-order, emitting non-overlapping hunks
/// straight through and producing git-style conflict blocks for overlaps.
fn merge_hunks(base: &[String], our_hunks: &[Hunk], their_hunks: &[Hunk]) -> (Vec<String>, bool) {
    let mut has_conflict = false;
    let mut result: Vec<String> = Vec::new();
    let (mut oi, mut ti) = (0usize, 0usize);
    let mut base_pos = 0usize;

    loop {
        let next_our = our_hunks.get(oi);
        let next_their = their_hunks.get(ti);

        if next_our.is_none() && next_their.is_none() {
            if base_pos < base.len() {
                result.extend_from_slice(&base[base_pos..]);
            }
            break;
        }

        if let (Some(no), Some(nt)) = (next_our, next_their) {
            if hunks_overlap(no, nt) {
                let start = no.base_start.min(nt.base_start);
                if base_pos < start {
                    result.extend_from_slice(&base[base_pos..start]);
                }
                let end = (no.base_start + no.base_len).max(nt.base_start + nt.base_len);

                if same_change(no, nt) {
                    result.extend(no.new_lines.iter().cloned());
                } else {
                    has_conflict = true;
                    result.push("<<<<<<< server".to_string());
                    result.extend(apply_hunk_to_region(base, start, end, no));
                    result.push("=======".to_string());
                    result.extend(apply_hunk_to_region(base, start, end, nt));
                    result.push(">>>>>>> client".to_string());
                }

                base_pos = end;
                oi += 1;
                ti += 1;
                continue;
            }
        }

        // Non-overlapping: apply whichever comes first.
        let (next_hunk, is_ours) = match (next_our, next_their) {
            (Some(no), Some(nt)) => {
                if no.base_start <= nt.base_start {
                    (no, true)
                } else {
                    (nt, false)
                }
            }
            (Some(no), None) => (no, true),
            (None, Some(nt)) => (nt, false),
            (None, None) => unreachable!("checked above"),
        };

        if base_pos < next_hunk.base_start {
            result.extend_from_slice(&base[base_pos..next_hunk.base_start]);
        }
        result.extend(next_hunk.new_lines.iter().cloned());
        base_pos = next_hunk.base_start + next_hunk.base_len;

        if is_ours {
            oi += 1;
        } else {
            ti += 1;
        }
    }

    (result, has_conflict)
}

fn hunks_overlap(a: &Hunk, b: &Hunk) -> bool {
    let a_end = a.base_start + a.base_len;
    let b_end = b.base_start + b.base_len;

    // Two insertions at the same position collide.
    if a.base_len == 0 && b.base_len == 0 && a.base_start == b.base_start {
        return true;
    }

    // Insertion strictly inside the other's range.
    if a.base_len == 0 {
        return a.base_start > b.base_start && a.base_start < b_end;
    }
    if b.base_len == 0 {
        return b.base_start > a.base_start && b.base_start < a_end;
    }

    a.base_start < b_end && b.base_start < a_end
}

fn same_change(a: &Hunk, b: &Hunk) -> bool {
    a.base_start == b.base_start && a.base_len == b.base_len && a.new_lines == b.new_lines
}

/// Reconstruct `[start, end)` of `base` after applying a single hunk.
fn apply_hunk_to_region(base: &[String], start: usize, end: usize, h: &Hunk) -> Vec<String> {
    let mut result = Vec::new();
    if start < h.base_start {
        result.extend_from_slice(&base[start..h.base_start]);
    }
    result.extend(h.new_lines.iter().cloned());
    let hunk_end = h.base_start + h.base_len;
    if hunk_end < end {
        result.extend_from_slice(&base[hunk_end..end]);
    }
    result
}

/// Extract the two sides of a document containing git-style conflict markers.
/// When no markers are present, both sides equal the input.
pub fn parse_conflict_markers(text: &str) -> (String, String, bool) {
    let mut ours_lines: Vec<&str> = Vec::new();
    let mut theirs_lines: Vec<&str> = Vec::new();
    let mut has_conflicts = false;
    let mut in_conflict = false;
    let mut in_ours = false;

    for line in text.split('\n') {
        if line.starts_with("<<<<<<< ") {
            has_conflicts = true;
            in_conflict = true;
            in_ours = true;
            continue;
        }
        if in_conflict && line == "=======" {
            in_ours = false;
            continue;
        }
        if line.starts_with(">>>>>>> ") {
            in_conflict = false;
            continue;
        }
        if in_conflict {
            if in_ours {
                ours_lines.push(line);
            } else {
                theirs_lines.push(line);
            }
        } else {
            ours_lines.push(line);
            theirs_lines.push(line);
        }
    }

    if !has_conflicts {
        return (text.to_string(), text.to_string(), false);
    }
    (ours_lines.join("\n"), theirs_lines.join("\n"), true)
}

/// Return true when the text contains conflict markers.
pub fn has_conflict_markers(text: &str) -> bool {
    text.starts_with("<<<<<<< ") || text.contains("\n<<<<<<< ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn merge_str(base: &str, ours: &str, theirs: &str) -> (String, bool) {
        let (bytes, c) = three_way_merge(base.as_bytes(), ours.as_bytes(), theirs.as_bytes());
        (String::from_utf8(bytes).unwrap(), c)
    }

    #[test]
    fn three_way_merge_no_changes() {
        let base = "line1\nline2\nline3";
        let (merged, conflict) = merge_str(base, base, base);
        assert!(!conflict);
        assert_eq!(merged, base);
    }

    #[test]
    fn three_way_merge_only_ours_changed() {
        let base = "line1\nline2\nline3";
        let ours = "line1\nmodified\nline3";
        let (merged, conflict) = merge_str(base, ours, base);
        assert!(!conflict);
        assert_eq!(merged, ours);
    }

    #[test]
    fn three_way_merge_only_theirs_changed() {
        let base = "line1\nline2\nline3";
        let theirs = "line1\nline2\nmodified";
        let (merged, conflict) = merge_str(base, base, theirs);
        assert!(!conflict);
        assert_eq!(merged, theirs);
    }

    #[test]
    fn three_way_merge_both_changed_different_lines() {
        let base = "line1\nline2\nline3\nline4";
        let ours = "modified1\nline2\nline3\nline4";
        let theirs = "line1\nline2\nline3\nmodified4";
        let (merged, conflict) = merge_str(base, ours, theirs);
        assert!(!conflict);
        assert_eq!(merged, "modified1\nline2\nline3\nmodified4");
    }

    #[test]
    fn three_way_merge_conflict() {
        let base = "line1\nline2\nline3";
        let ours = "line1\nours\nline3";
        let theirs = "line1\ntheirs\nline3";
        let (merged, conflict) = merge_str(base, ours, theirs);
        assert!(conflict);
        assert!(merged.contains("<<<<<<< server"));
        assert!(merged.contains("ours"));
        assert!(merged.contains("======="));
        assert!(merged.contains("theirs"));
        assert!(merged.contains(">>>>>>> client"));
    }

    #[test]
    fn three_way_merge_same_change_on_both_sides() {
        let base = "line1\nline2\nline3";
        let both = "line1\nsame change\nline3";
        let (merged, conflict) = merge_str(base, both, both);
        assert!(!conflict);
        assert_eq!(merged, both);
    }

    #[test]
    fn three_way_merge_ours_adds_lines() {
        let base = "line1\nline3";
        let ours = "line1\nline2\nline3";
        let (merged, conflict) = merge_str(base, ours, base);
        assert!(!conflict);
        assert_eq!(merged, ours);
    }

    #[test]
    fn three_way_merge_theirs_deletes_lines() {
        let base = "line1\nline2\nline3";
        let theirs = "line1\nline3";
        let (merged, conflict) = merge_str(base, base, theirs);
        assert!(!conflict);
        assert_eq!(merged, theirs);
    }

    #[test]
    fn three_way_merge_both_add_at_same_place_conflict() {
        let base = "line1\nline3";
        let ours = "line1\nour insert\nline3";
        let theirs = "line1\ntheir insert\nline3";
        let (merged, conflict) = merge_str(base, ours, theirs);
        assert!(conflict);
        assert!(merged.contains("<<<<<<< server"));
        assert!(merged.contains("our insert"));
        assert!(merged.contains("their insert"));
        assert!(merged.contains(">>>>>>> client"));
    }

    #[test]
    fn three_way_merge_empty_base() {
        let base = "";
        let ours = "new content from server";
        let theirs = "new content from client";
        let (merged, conflict) = merge_str(base, ours, theirs);
        assert!(conflict);
        assert!(merged.contains("<<<<<<< server"));
        assert!(merged.contains(">>>>>>> client"));
    }

    #[test]
    fn three_way_merge_real_world_markdown() {
        let base = "# My Page\n\nSome introductory text.\n\n## Section A\n\nContent of section A.\n\n## Section B\n\nContent of section B.";
        let ours = "# My Page\n\nSome introductory text.\n\n## Section A\n\nUpdated content of section A by server.\n\n## Section B\n\nContent of section B.";
        let theirs = "# My Page\n\nSome introductory text.\n\n## Section A\n\nContent of section A.\n\n## Section B\n\nUpdated content of section B by client.";
        let (merged, conflict) = merge_str(base, ours, theirs);
        assert!(!conflict);
        assert!(merged.contains("Updated content of section A by server."));
        assert!(merged.contains("Updated content of section B by client."));
    }

    #[test]
    fn split_lines_cases() {
        assert_eq!(split_lines(""), Vec::<String>::new());
        assert_eq!(split_lines("hello"), vec!["hello".to_string()]);
        assert_eq!(split_lines("a\nb"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(split_lines("a\n"), vec!["a".to_string(), "".to_string()]);
    }

    #[test]
    fn parse_conflict_markers_extracts_sides() {
        let text =
            "line1\n<<<<<<< server\nserver line\n=======\nclient line\n>>>>>>> client\nline3";
        let (ours, theirs, conflicts) = parse_conflict_markers(text);
        assert!(conflicts);
        assert_eq!(ours, "line1\nserver line\nline3");
        assert_eq!(theirs, "line1\nclient line\nline3");
    }

    #[test]
    fn parse_conflict_markers_no_conflict() {
        let text = "just normal text\nno conflicts here";
        let (ours, theirs, conflicts) = parse_conflict_markers(text);
        assert!(!conflicts);
        assert_eq!(ours, text);
        assert_eq!(theirs, text);
    }

    #[test]
    fn has_conflict_markers_cases() {
        assert!(has_conflict_markers(
            "<<<<<<< server\nfoo\n=======\nbar\n>>>>>>> client"
        ));
        assert!(has_conflict_markers("line1\n<<<<<<< server\nfoo"));
        assert!(!has_conflict_markers("normal text"));
        assert!(!has_conflict_markers(""));
    }

    #[test]
    fn diff_lines_finds_insert() {
        let a = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let b = vec!["a".to_string(), "x".to_string(), "c".to_string()];
        let diff = diff_lines(&a, &b);
        let mut found = false;
        for d in &diff {
            if d.op == DiffOp::Insert {
                assert_eq!(d.lines, vec!["x".to_string()]);
                found = true;
            }
        }
        assert!(found, "should find an insert of 'x'");
    }
}
