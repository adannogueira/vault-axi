//! Line-based three-way merge used to reconcile concurrent edits of the same note.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    Clean(String),
    AutoMerged(String),
    Conflict,
}

#[derive(Debug, Clone)]
struct DiffHunk<'a> {
    b_start: usize,
    b_end: usize,
    replacement: Vec<&'a str>,
}

fn compute_lcs_hunks<'a>(base: &[&'a str], target: &[&'a str]) -> Option<Vec<DiffHunk<'a>>> {
    let b_len = base.len();
    let t_len = target.len();
    if b_len.saturating_mul(t_len) > 4_000_000 {
        return None;
    }
    let cols = t_len + 1;
    let mut dp = vec![0u32; (b_len + 1) * cols];
    for i in (0..b_len).rev() {
        for j in (0..t_len).rev() {
            dp[i * cols + j] = if base[i] == target[j] {
                dp[(i + 1) * cols + (j + 1)] + 1
            } else {
                dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
            };
        }
    }

    let mut matches = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < b_len && j < t_len {
        if base[i] == target[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matches.push((b_len, t_len));

    let mut hunks = Vec::new();
    let mut b_prev = 0;
    let mut t_prev = 0;
    for (b_m, t_m) in matches {
        if b_prev < b_m || t_prev < t_m {
            hunks.push(DiffHunk {
                b_start: b_prev,
                b_end: b_m,
                replacement: target[t_prev..t_m].to_vec(),
            });
        }
        b_prev = b_m + 1;
        t_prev = t_m + 1;
    }
    Some(hunks)
}

/// Perform a line-based 3-way merge of `local` and `remote` against `base`.
/// - Non-overlapping edits to different lines/sections or concurrent pure insertions
///   (such as `vault log` appends) are automatically merged into `MergeOutcome::AutoMerged`.
/// - Overlapping incompatible edits to the same base lines return `MergeOutcome::Conflict`.
pub fn merge_three_way(base: &str, local: &str, remote: &str) -> MergeOutcome {
    let b_norm = base.replace("\r\n", "\n");
    let l_norm = local.replace("\r\n", "\n");
    let r_norm = remote.replace("\r\n", "\n");

    if l_norm == r_norm {
        return MergeOutcome::Clean(l_norm);
    }
    if l_norm == b_norm {
        return MergeOutcome::Clean(r_norm);
    }
    if r_norm == b_norm {
        return MergeOutcome::Clean(l_norm);
    }
    if b_norm.is_empty() {
        return MergeOutcome::Conflict;
    }

    let b_lines: Vec<&str> = b_norm.lines().collect();
    let l_lines: Vec<&str> = l_norm.lines().collect();
    let r_lines: Vec<&str> = r_norm.lines().collect();

    let Some(l_hunks) = compute_lcs_hunks(&b_lines, &l_lines) else {
        return MergeOutcome::Conflict;
    };
    let Some(r_hunks) = compute_lcs_hunks(&b_lines, &r_lines) else {
        return MergeOutcome::Conflict;
    };

    let mut out_lines: Vec<&str> = Vec::with_capacity(l_lines.len().max(r_lines.len()) + 8);
    let mut b_pos = 0;
    let mut li = 0;
    let mut ri = 0;

    while li < l_hunks.len() || ri < r_hunks.len() {
        let next_b = match (l_hunks.get(li), r_hunks.get(ri)) {
            (Some(lh), Some(rh)) => lh.b_start.min(rh.b_start),
            (Some(lh), None) => lh.b_start,
            (None, Some(rh)) => rh.b_start,
            (None, None) => b_lines.len(),
        };
        if b_pos < next_b {
            out_lines.extend_from_slice(&b_lines[b_pos..next_b]);
            b_pos = next_b;
        }

        match (l_hunks.get(li), r_hunks.get(ri)) {
            (Some(lh), None) => {
                out_lines.extend_from_slice(&lh.replacement);
                b_pos = lh.b_end;
                li += 1;
            }
            (None, Some(rh)) => {
                out_lines.extend_from_slice(&rh.replacement);
                b_pos = rh.b_end;
                ri += 1;
            }
            (Some(lh), Some(rh)) => {
                let both_pure_insert =
                    lh.b_start == lh.b_end && rh.b_start == rh.b_end && lh.b_start == rh.b_start;
                if both_pure_insert {
                    if lh.replacement == rh.replacement {
                        out_lines.extend_from_slice(&lh.replacement);
                    } else {
                        out_lines.extend_from_slice(&rh.replacement);
                        out_lines.extend_from_slice(&lh.replacement);
                    }
                    li += 1;
                    ri += 1;
                } else if lh.b_end < rh.b_start
                    || (lh.b_end == rh.b_start
                        && !(lh.b_start == lh.b_end && rh.b_start == rh.b_end))
                {
                    out_lines.extend_from_slice(&lh.replacement);
                    b_pos = lh.b_end;
                    li += 1;
                } else if rh.b_end < lh.b_start
                    || (rh.b_end == lh.b_start
                        && !(lh.b_start == lh.b_end && rh.b_start == rh.b_end))
                {
                    out_lines.extend_from_slice(&rh.replacement);
                    b_pos = rh.b_end;
                    ri += 1;
                } else if lh.b_start == rh.b_start
                    && lh.b_end == rh.b_end
                    && lh.replacement == rh.replacement
                {
                    out_lines.extend_from_slice(&lh.replacement);
                    b_pos = lh.b_end;
                    li += 1;
                    ri += 1;
                } else {
                    return MergeOutcome::Conflict;
                }
            }
            (None, None) => break,
        }
    }

    if b_pos < b_lines.len() {
        out_lines.extend_from_slice(&b_lines[b_pos..]);
    }

    let mut merged = out_lines.join("\n");
    if !merged.is_empty() && (l_norm.ends_with('\n') || r_norm.ends_with('\n')) {
        merged.push('\n');
    }
    MergeOutcome::AutoMerged(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str =
        "# Plan\n\n## Objective\nInitial objective\n\n## Build Log\n- `10:00` - Started\n";
    const LOCAL_SECTION: &str =
        "# Plan\n\n## Objective\nUpdated objective on B\n\n## Build Log\n- `10:00` - Started\n";
    const REMOTE_APPEND: &str = "# Plan\n\n## Objective\nInitial objective\n\n## Build Log\n- `10:00` - Started\n- `10:02` - A step\n";

    #[test]
    fn trivial_cases_are_clean() {
        assert_eq!(
            merge_three_way("a\n", "b\n", "b\n"),
            MergeOutcome::Clean("b\n".into())
        );
        assert_eq!(
            merge_three_way("a\n", "a\n", "r\n"),
            MergeOutcome::Clean("r\n".into())
        );
        assert_eq!(
            merge_three_way("a\n", "l\n", "a\n"),
            MergeOutcome::Clean("l\n".into())
        );
        assert_eq!(
            merge_three_way("a\r\n", "a\n", "r\r\n"),
            MergeOutcome::Clean("r\n".into()),
            "CRLF is normalized before comparing"
        );
    }

    #[test]
    fn empty_base_with_divergent_sides_conflicts() {
        assert_eq!(merge_three_way("", "l", "r"), MergeOutcome::Conflict);
    }

    #[test]
    fn non_overlapping_edits_auto_merge() {
        match merge_three_way(BASE, LOCAL_SECTION, REMOTE_APPEND) {
            MergeOutcome::AutoMerged(m) => {
                assert!(m.contains("Updated objective on B"));
                assert!(m.contains("A step"));
                assert!(m.ends_with('\n'));
            }
            other => panic!("expected AutoMerged, got {other:?}"),
        }
    }

    #[test]
    fn concurrent_appends_keep_remote_first() {
        let local_append = format!("{BASE}- `10:03` - B step\n");
        match merge_three_way(BASE, &local_append, REMOTE_APPEND) {
            MergeOutcome::AutoMerged(m) => assert!(m.contains("A step\n- `10:03` - B step")),
            other => panic!("expected AutoMerged, got {other:?}"),
        }
    }

    #[test]
    fn identical_concurrent_edits_merge_once() {
        let edit = "x\nSAME\nz\n";
        let local = "x\nSAME\nz\nlocal tail\n";
        match merge_three_way("x\ny\nz\n", local, edit) {
            MergeOutcome::AutoMerged(m) => assert_eq!(m, "x\nSAME\nz\nlocal tail\n"),
            other => panic!("expected AutoMerged, got {other:?}"),
        }
    }

    #[test]
    fn same_line_edits_conflict() {
        let remote = BASE.replace("Initial objective", "Conflicting objective on A");
        assert_eq!(
            merge_three_way(BASE, LOCAL_SECTION, &remote),
            MergeOutcome::Conflict
        );
    }

    #[test]
    fn oversized_inputs_conflict_instead_of_exhausting_memory() {
        let base: String = (0..2_100).map(|i| format!("b{i}\n")).collect();
        let local: String = (0..2_100).map(|i| format!("l{i}\n")).collect();
        let remote: String = (0..2_100).map(|i| format!("r{i}\n")).collect();
        assert_eq!(
            merge_three_way(&base, &local, &remote),
            MergeOutcome::Conflict
        );
    }
}
