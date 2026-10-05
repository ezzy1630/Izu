# Byte diff and three-way merge

`izu-merge` contains original safe Rust algorithms. It has no dependencies and
uses no Git, Jujutsu, or other VCS engine implementation. All operations are pure
in-memory functions over byte slices. The engine owns tree paths, object loading,
entry modes/types and persistence.

## Interfaces

- `diff(before, after, &Options) -> Result<Diff, MergeError>` returns contiguous
  replacement `Change`s with zero-based byte and line ranges. `Diff` retains
  borrowed input slices and exposes `changes()`, `before()`, and `after()`.
- `apply_patch(source, &Diff, &Options)` reconstructs the target from replacement
  ranges. It rejects a different source with `SourceMismatch`.
- `render_unified(&Diff, old_label, new_label, context_lines, &Options)` renders
  conventional unified diff bytes. It retains exact line bytes and emits missing
  final newline markers. Labels reject CR, LF and NUL. Callers remain responsible
  for terminal escaping and presentation of binary bytes.
- `merge(base, ours, theirs, &Options)` returns `MergeResult::Clean(Option<Vec<u8>>)`
  or `MergeResult::Conflicted { chunks }`. `None` means absence and differs from
  `Some(b"")`. Ordered resolved chunks surround conflicts whose `base`, `ours`
  and `theirs` fields retain exact original bytes and missing alternatives.
- `merge_binary` takes the same arguments and always treats bytes as indivisible.
  Use it for caller-classified binary content and symlink targets.

All equal alternatives resolve. A side unchanged from base accepts the other
side, including additions and deletions. Divergent add/add and delete/modify
operations conflict explicitly. Text merges combine nonoverlapping line changes;
identical overlapping replacements resolve. Differing overlapping replacements
remain unresolved with `Content` conflicts. NUL-containing data automatically
uses binary conflict behavior; this is a conservative heuristic, not complete
binary format recognition. Caller classification through `merge_binary` avoids
all line merging for any binary format.

Entry type and executable-mode merging is intentionally separate. The engine
must retain mode/type conflicts and must not use successful byte merging to
resolve divergent regular-file/symlink types. Text merging makes no claim of
semantic correctness for programming languages or structured file formats.

## Algorithm and bounds

Lines are byte ranges ending in LF, including LF itself; a final unterminated
line remains a distinct exact slice. No Unicode decoding, newline normalization,
or lossy conversion occurs. Prefix/suffix matching removes common lines. The
An empty trimmed middle on either side returns one exact insertion/deletion hunk
directly after line indexing and prefix/suffix matching. Its shortest path is
known without search, including large additions or deletions inside shared
context. General divergent middles use a Myers frontier, storing only reachable diagonals at each distance,
then backtracks the shortest insertion/deletion path. Line-level work is
`O((N+M)D)` and trace memory is `O(D²)`, rather than an `N*M` table. Byte comparisons
are additionally charged by bytes examined. Repeated ambiguous lines have a
deterministic deletion/insertion tie break; they do not promise human-intuitive
alignment.

`Limits` defaults are 64 MiB total input bytes per operation, 1,000,000 lines per
indexed input, Myers search depth 2,048 per diff, 100,000,000 work units, 128 MiB cumulative
requested allocations, and 128 MiB output. Merge shares one work/allocation
budget across both diffs. `max_edit_distance` bounds search depth, rather than
forbidding a known exact insertion/deletion with more lines. The one-sided middle
path still obeys input, line, work, allocation and cancellation budgets; general
divergent inputs never receive an approximate fallback success. Line scans,
comparisons, frontier states, backtracking,
merge grouping and output copies consume work. Arithmetic is checked at resource
boundaries. Vectors use `try_reserve_exact`; allocation failure returns
`Allocation`. Allocation accounting charges the full requested allocation on
every growth, including retired allocations; allocator metadata and allocator
rounding are outside this budget. Borrowed caller input memory is outside the
allocation budget and inside the input limit.

`Options::cancellation` borrows an `AtomicBool`; cancellation is checked before
work and during scans/comparisons in blocks of at most 4,096 bytes. Cancellation
and exhausted limits return typed errors and never a partial successful patch or
merge. Allocator calls and operating-system scheduling are not deadline-bound;
work limits bound algorithmic work, not wall-clock time. Explicit output
allocation is bounded; caller cloning of public result values is outside the
operation budget.

Insertion at the same base point conflicts if the alternatives differ. An
insertion exactly on a replacement boundary is conservatively grouped with that
replacement. Adjacent nonempty replacements remain independent. Merge is line
based and may therefore report conflict for independent edits within one line.

## Verification and measurements

Run the isolated task toolchain with a private target directory:

```sh
scripts/izu-env \
  cargo test --locked --offline -p izu-merge
```

Tests include exhaustive patch reconstruction and comparison to an independent
small shortest-distance oracle for 16,129 repeated-line pairs, three-way side-swap
and one-sided properties, random byte reconstruction, conflict chunk identity,
absence/deletion, binary data, Unicode, long lines, missing newline rendering,
hunk grouping, 20,000-line whole/contextual additions and deletions with zero
search budget, exact-path resource limits, a large insertion combined with an
independent edit, and cancellation during computation.
The small quadratic oracle exists only in tests. Fresh renamed-source test logs
and hashes are under `.artifacts/izu-process-final`; the merge library SHA256 is
`f7f0378dcbb1b792ae70f58f5a0b4cfadb2909da2c0d99b1b3740b4b292c3d4a`.
Renaming the crate did not change this pure library's bytes; the manifest and
test binaries are identified separately in that evidence.
All 15 merge tests passed with the renamed manifest in Mac and Linux debug and
release runs; Mac strict Clippy also passed in both build modes.

`cargo run -p izu-merge --release --example kernels` measures focused local kernels
using `black_box`. The measurements below are historical observations from the
pre-rename `ezy` source. Initial pre-optimization measurements in the merge lane on
2026-10-03, Rust 1.99, 480,000-byte/20,000-line inputs, 100 iterations: identical
diff 667 us/op, one edit 625 us/op, independent two-sided merge 1,360 us/op.
A 10,000-line repeated divergent pair stopped at an edit-distance limit of 256 in
375 us. These are local kernel observations, not repository-scale or superiority
claims. No assembly/C or speculative optimization was introduced.

The measured one-sided-middle correction used the same 480,000-byte/20,000-line
input and 20 iterations on 2026-10-03. Before the correction, pure additions and
deletions spent 8,062 and 8,096 us/op respectively, then incorrectly returned
`Limit(EditDistance)` in all 20 iterations. After direct exact hunk construction,
they succeeded in every iteration at 283 and 284 us/op respectively. The general
repeated-divergence case continued to return `Limit(EditDistance)` (313 us in this
rerun). Other kernel timing variation is not attributed to this change.
