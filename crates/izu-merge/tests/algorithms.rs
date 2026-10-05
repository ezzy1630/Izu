use izu_merge::{
    ConflictKind, LimitKind, MergeChunk, MergeError, MergeResult, Options, apply_patch, diff,
    merge, render_unified,
};
use std::sync::atomic::AtomicBool;

fn clean(base: &[u8], ours: &[u8], theirs: &[u8], expected: &[u8]) {
    assert_eq!(
        merge(Some(base), Some(ours), Some(theirs), &Options::default()).unwrap(),
        MergeResult::Clean(Some(expected.to_vec()))
    );
}
#[test]
fn identity_one_sided_and_nonoverlap() {
    clean(b"a\nb\nc\n", b"a\nb\nc\n", b"a\nb\nc\n", b"a\nb\nc\n");
    clean(b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nc\n", b"A\nb\nc\n");
    clean(b"a\nb\nc\n", b"A\nb\nc\n", b"a\nb\nC\n", b"A\nb\nC\n");
    clean(b"a\nb\nc\n", b"A\nb\nc\n", b"A\nb\nc\n", b"A\nb\nc\n");
    clean(b"a\nb\nc\n", b"b\nc\n", b"a\nb\nC\n", b"b\nC\n");
    clean(
        b"a\nb\nc\n",
        b"x\na\nb\nc\n",
        b"a\nb\nc\ny\n",
        b"x\na\nb\nc\ny\n",
    );
}
#[test]
fn exact_conflict_chunks_preserve_context_and_alternatives() {
    assert_eq!(
        merge(
            Some(b"a\nb\nc\n"),
            Some(b"a\nours\nc\n"),
            Some(b"a\ntheirs\nc\n"),
            &Options::default()
        )
        .unwrap(),
        MergeResult::Conflicted {
            chunks: vec![
                MergeChunk::Resolved(b"a\n"),
                MergeChunk::Conflict {
                    kind: ConflictKind::Content,
                    base: Some(b"b\n"),
                    ours: Some(b"ours\n"),
                    theirs: Some(b"theirs\n")
                },
                MergeChunk::Resolved(b"c\n")
            ]
        }
    );
}
#[test]
fn absence_empty_binary_and_delete_modify_are_distinct() {
    let o = Options::default();
    assert_eq!(
        merge(None, Some(b""), None, &o).unwrap(),
        MergeResult::Clean(Some(vec![]))
    );
    assert_eq!(
        merge(Some(b"a"), None, Some(b"a"), &o).unwrap(),
        MergeResult::Clean(None)
    );
    assert_eq!(
        merge(Some(b"a"), None, None, &o).unwrap(),
        MergeResult::Clean(None)
    );
    for (base, ours, theirs, kind) in [
        (
            Some(b"a".as_slice()),
            None,
            Some(b"b".as_slice()),
            ConflictKind::DeleteModify,
        ),
        (
            None,
            Some(b"a".as_slice()),
            Some(b"b".as_slice()),
            ConflictKind::AddAdd,
        ),
        (
            Some(b"a\0".as_slice()),
            Some(b"b\0".as_slice()),
            Some(b"c\0".as_slice()),
            ConflictKind::Binary,
        ),
    ] {
        assert_eq!(
            merge(base, ours, theirs, &o).unwrap(),
            MergeResult::Conflicted {
                chunks: vec![MergeChunk::Conflict {
                    kind,
                    base,
                    ours,
                    theirs
                }]
            }
        );
    }
    clean(b"a\0", b"a\0", b"b\0", b"b\0");
}
#[test]
fn unicode_long_lines_and_missing_newlines() {
    let a = "你好\n🙂 café".as_bytes();
    let b = "你好\n🙂 tea\n".as_bytes();
    let p = diff(a, b, &Options::default()).unwrap();
    assert_eq!(apply_patch(a, &p, &Options::default()).unwrap(), b);
    let text = render_unified(&p, b"a/file", b"b/file", 3, &Options::default()).unwrap();
    assert_eq!(text, "--- a/file\n+++ b/file\n@@ -1,2 +1,2 @@\n 你好\n-🙂 café\n\\ No newline at end of file\n+🙂 tea\n".as_bytes());
    let mut a = vec![b'x'; 100_000];
    a.push(b'\n');
    let mut b = a.clone();
    b[50_000] = b'y';
    let p = diff(&a, &b, &Options::default()).unwrap();
    assert_eq!(apply_patch(&a, &p, &Options::default()).unwrap(), b);
}
fn strings(max: usize) -> Vec<Vec<u8>> {
    let mut result = vec![vec![]];
    for n in 1..=max {
        for bits in 0..(1usize << n) {
            let mut s = Vec::new();
            for i in 0..n {
                s.push(if bits & (1 << i) == 0 { b'a' } else { b'b' });
                s.push(b'\n');
            }
            result.push(s);
        }
    }
    result
}
#[test]
fn exhaustive_repeated_lines_reconstruct_and_shortest_distance() {
    let inputs = strings(6);
    let o = Options::default();
    for a in &inputs {
        for b in &inputs {
            let p = diff(a, b, &o).unwrap();
            assert_eq!(apply_patch(a, &p, &o).unwrap(), *b);
            let distance: usize = p
                .changes()
                .iter()
                .map(|h| h.before_lines.len() + h.after_lines.len())
                .sum();
            // Independent tiny oracle only in tests, not the production algorithm.
            let al = a.as_chunks::<2>().0;
            let bl = b.as_chunks::<2>().0;
            let mut matrix = vec![vec![0; bl.len() + 1]; al.len() + 1];
            for (i, row) in matrix.iter_mut().enumerate() {
                row[0] = i;
            }
            for (j, cell) in matrix[0].iter_mut().enumerate() {
                *cell = j;
            }
            for i in 1..=al.len() {
                for j in 1..=bl.len() {
                    matrix[i][j] = if al[i - 1] == bl[j - 1] {
                        matrix[i - 1][j - 1]
                    } else {
                        (matrix[i - 1][j] + 1).min(matrix[i][j - 1] + 1)
                    };
                }
            }
            assert_eq!(distance, matrix[al.len()][bl.len()], "a={a:?}, b={b:?}");
        }
    }
}
#[test]
fn merge_side_swap_and_one_sided_properties() {
    let inputs = strings(3);
    let o = Options::default();
    for base in &inputs {
        for ours in &inputs {
            assert_eq!(
                merge(Some(base), Some(ours), Some(base), &o).unwrap(),
                MergeResult::Clean(Some(ours.clone()))
            );
            for theirs in &inputs {
                let left = merge(Some(base), Some(ours), Some(theirs), &o).unwrap();
                let right = merge(Some(base), Some(theirs), Some(ours), &o).unwrap();
                match (left, right) {
                    (MergeResult::Clean(a), MergeResult::Clean(b)) => assert_eq!(a, b),
                    (
                        MergeResult::Conflicted { chunks: a },
                        MergeResult::Conflicted { chunks: b },
                    ) => {
                        let swapped: Vec<_> = a
                            .into_iter()
                            .map(|c| match c {
                                MergeChunk::Resolved(x) => MergeChunk::Resolved(x),
                                MergeChunk::Conflict {
                                    kind,
                                    base,
                                    ours,
                                    theirs,
                                } => MergeChunk::Conflict {
                                    kind,
                                    base,
                                    ours: theirs,
                                    theirs: ours,
                                },
                            })
                            .collect();
                        assert_eq!(swapped, b);
                    }
                    pair => panic!("asymmetric merge {pair:?}"),
                }
            }
        }
    }
}
#[test]
fn limits_cancellation_invalid_source_and_labels() {
    let cancelled = AtomicBool::new(true);
    let o = Options {
        cancellation: Some(&cancelled),
        ..Options::default()
    };
    assert_eq!(diff(b"", b"", &o).unwrap_err(), MergeError::Cancelled);
    assert_eq!(
        merge(None, None, None, &o).unwrap_err(),
        MergeError::Cancelled
    );
    for (kind, limit) in [
        (LimitKind::InputBytes, 0),
        (LimitKind::Lines, 0),
        (LimitKind::EditDistance, 0),
        (LimitKind::Work, 0),
        (LimitKind::AllocationBytes, 0),
    ] {
        let mut o = Options::default();
        match kind {
            LimitKind::InputBytes => o.limits.max_input_bytes = limit,
            LimitKind::Lines => o.limits.max_lines = limit,
            LimitKind::EditDistance => o.limits.max_edit_distance = limit,
            LimitKind::Work => o.limits.max_work = limit,
            LimitKind::AllocationBytes => o.limits.max_allocation_bytes = limit,
            _ => unreachable!(),
        }
        assert_eq!(
            diff(b"a\n", b"b\n", &o).unwrap_err(),
            MergeError::Limit(kind)
        );
    }
    let o = Options::default();
    let p = diff(b"a", b"b", &o).unwrap();
    assert_eq!(
        apply_patch(b"c", &p, &o).unwrap_err(),
        MergeError::SourceMismatch
    );
    assert_eq!(
        render_unified(&p, b"bad\nlabel", b"good", 3, &o).unwrap_err(),
        MergeError::InvalidLabel
    );
    let mut low = o;
    low.limits.max_output_bytes = 0;
    assert_eq!(
        apply_patch(b"a", &p, &low).unwrap_err(),
        MergeError::Limit(LimitKind::OutputBytes)
    );
}
#[test]
fn adversarial_changes_are_rejected_with_a_typed_budget_error() {
    let a = b"a\n".repeat(10_000);
    let b = b"b\n".repeat(10_000);
    let mut o = Options::default();
    o.limits.max_edit_distance = 64;
    assert_eq!(
        diff(&a, &b, &o).unwrap_err(),
        MergeError::Limit(LimitKind::EditDistance)
    );
}

#[test]
fn caller_classified_binary_never_uses_line_merging() {
    use izu_merge::merge_binary;
    let o = Options::default();
    let (base, ours, theirs) = (
        b"a\nb\n".as_slice(),
        b"A\nb\n".as_slice(),
        b"a\nB\n".as_slice(),
    );
    assert_eq!(
        merge_binary(Some(base), Some(ours), Some(theirs), &o).unwrap(),
        MergeResult::Conflicted {
            chunks: vec![MergeChunk::Conflict {
                kind: ConflictKind::Binary,
                base: Some(base),
                ours: Some(ours),
                theirs: Some(theirs)
            }]
        }
    );
    assert_eq!(
        merge_binary(Some(base), Some(ours), Some(base), &o).unwrap(),
        MergeResult::Clean(Some(ours.to_vec()))
    );
}
#[test]
fn unified_zero_context_insert_delete_and_hunk_grouping() {
    let o = Options::default();
    for (a, b, expected) in [
        (
            b"".as_slice(),
            b"a\n".as_slice(),
            b"--- old\n+++ new\n@@ -0,0 +1,1 @@\n+a\n".as_slice(),
        ),
        (
            b"a\n".as_slice(),
            b"".as_slice(),
            b"--- old\n+++ new\n@@ -1,1 +0,0 @@\n-a\n".as_slice(),
        ),
        (
            b"a\nb\nc\n".as_slice(),
            b"A\nb\nC\n".as_slice(),
            b"--- old\n+++ new\n@@ -1,1 +1,1 @@\n-a\n+A\n@@ -3,1 +3,1 @@\n-c\n+C\n".as_slice(),
        ),
    ] {
        let p = diff(a, b, &o).unwrap();
        assert_eq!(render_unified(&p, b"old", b"new", 0, &o).unwrap(), expected);
    }
    let p = diff(b"a\nb\nc\n", b"A\nb\nC\n", &o).unwrap();
    assert_eq!(
        render_unified(&p, b"old", b"new", 1, &o).unwrap(),
        b"--- old\n+++ new\n@@ -1,3 +1,3 @@\n-a\n+A\n b\n-c\n+C\n"
    );
}
#[test]
fn patch_reconstruction_for_arbitrary_bytes() {
    // Deterministic generator deliberately includes NUL, Unicode-invalid bytes,
    // LF/CR and inputs without final newlines. No text decoding is performed.
    let mut seed = 17u64;
    let mut next = || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (seed >> 32) as u8
    };
    let mut values = vec![vec![]];
    for _ in 0..200 {
        let len = usize::from(next());
        let mut value = Vec::new();
        for _ in 0..len {
            let b = next();
            value.push(if b % 5 == 0 { b'\n' } else { b });
        }
        values.push(value);
    }
    for pair in values.windows(2) {
        let p = diff(&pair[0], &pair[1], &Options::default()).unwrap();
        assert_eq!(
            apply_patch(&pair[0], &p, &Options::default()).unwrap(),
            pair[1]
        );
    }
}
#[test]
fn cancellation_is_observed_during_expensive_computation() {
    use std::{sync::Arc, time::Duration};
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = cancelled.clone();
    let a = b"a\n".repeat(1_000_000);
    let b = b"b\n".repeat(1_000_000);
    let worker = std::thread::spawn(move || {
        let mut o = Options {
            cancellation: Some(&cancelled),
            ..Options::default()
        };
        o.limits.max_edit_distance = 20_000;
        o.limits.max_work = usize::MAX;
        diff(&a, &b, &o).map(|_| ())
    });
    std::thread::sleep(Duration::from_millis(1));
    signal.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(worker.join().unwrap(), Err(MergeError::Cancelled));
}

#[test]
fn large_pure_additions_and_deletions_need_no_edit_search() {
    let middle = b"inserted\n".repeat(20_000);
    let mut padded = b"prefix\n".to_vec();
    padded.extend_from_slice(&middle);
    padded.extend_from_slice(b"suffix\n");
    let shared = b"prefix\nsuffix\n";
    let mut o = Options::default();
    o.limits.max_edit_distance = 0;
    // Whole-file add/delete and a large edit surrounded by retained context
    // have a known exact shortest path without any frontier search.
    for (a, b) in [
        (b"".as_slice(), middle.as_slice()),
        (middle.as_slice(), b"".as_slice()),
        (shared.as_slice(), padded.as_slice()),
        (padded.as_slice(), shared.as_slice()),
    ] {
        let patch = diff(a, b, &o).unwrap();
        assert_eq!(patch.changes().len(), 1);
        assert_eq!(apply_patch(a, &patch, &o).unwrap(), b);
        assert_eq!(
            patch.changes()[0].before_lines.len() + patch.changes()[0].after_lines.len(),
            20_000
        );
    }
}

#[test]
fn exact_one_sided_middle_still_obeys_resource_budgets() {
    let middle = b"line\n".repeat(20_000);
    let mut o = Options::default();
    o.limits.max_edit_distance = 0;
    o.limits.max_work = middle.len();
    assert_eq!(
        diff(b"", &middle, &o).unwrap_err(),
        MergeError::Limit(LimitKind::Work)
    );
    o.limits.max_work = usize::MAX;
    o.limits.max_allocation_bytes = 0;
    assert_eq!(
        diff(&middle, b"", &o).unwrap_err(),
        MergeError::Limit(LimitKind::AllocationBytes)
    );
    // Permit line indexing but leave no space for the replacement hunk itself.
    o.limits.max_allocation_bytes = 20_000 * std::mem::size_of::<usize>();
    assert_eq!(
        diff(b"", &middle, &o).unwrap_err(),
        MergeError::Limit(LimitKind::AllocationBytes)
    );
}
#[test]
fn large_insertion_combines_with_an_independent_edit() {
    let mut ours = b"prefix\n".to_vec();
    ours.extend_from_slice(&b"added\n".repeat(20_000));
    ours.extend_from_slice(b"spacer\nsuffix\n");
    let mut expected = b"prefix\n".to_vec();
    expected.extend_from_slice(&b"added\n".repeat(20_000));
    expected.extend_from_slice(b"spacer\nSUFFIX\n");
    clean(
        b"prefix\nspacer\nsuffix\n",
        &ours,
        b"prefix\nspacer\nSUFFIX\n",
        &expected,
    );
}
