# Workflow measurement

Measure the real executable before optimizing and retain an unchanged baseline artifact. Supply the exact binary path and source checkout for every before/after run. A report retains raw wall-clock samples and nearest-rank p50/p95; no elapsed-time threshold is a functional test. Target-process timing includes executable startup. A separate launcher timing includes worker setup and cleanup; concurrent writer spans use launcher observations and include queue batches. Allocation walks and provenance hashing occur outside target timing. Concurrent writer runs disable allocation walks so measurement work does not distort overlap or queue spans; sequential setup/integration calls retain allocation observations.

```sh
cargo build --locked --release -p izu-cli -p izu-lab
mkdir -p .artifacts/benchmarks .artifacts/tmp
executable="$PWD/target/release/izu"
digest="$(shasum -a 256 "$executable" | cut -d ' ' -f 1)"
compiler="$(rustup which rustc)"
compiler_digest="$(shasum -a 256 "$compiler" | cut -d ' ' -f 1)"
TMPDIR="$PWD/.artifacts/tmp" target/release/izu-lab \
 --executable "$executable" --expected-sha256 "$digest" \
 --source-root "$PWD" --output "$PWD/.artifacts/benchmarks/baseline.json" \
 --scratch-dir "$PWD/.artifacts/tmp" benchmark \
 --git /usr/bin/git --samples 7 --cooperative-samples 3 --files 512 --bytes-per-file 4096 --seed 1729 \
 --rustc "$compiler" --expected-rustc-sha256 "$compiler_digest"
```

Choose a new output filename for each run. If `CARGO_TARGET_DIR` is set, adjust
the executable paths to match it. Keep a source snapshot and build record with
each binary; a matching source digest alone does not prove how it was built.

The default tree has 512 generated files, 2 MiB logical generated payload plus ignore metadata, with one eighth binary and seven eighths code-like text. Seed, file count and bytes per file are configurable. The harness caps a fixture at 100000 files and 256 MiB generated payload, validates multiplication before allocating, and removes owned fixtures after readbacks; the report records each cleanup outcome. Both implementations receive the same generated tree and changing text. Synthetic identity is configured only in the disposable Git repository; no global Git configuration or actual remote is changed.

Current measured operations are startup, init, first full capture, repeated no-op capture, one changed-file capture and diff. Git capture uses `git add -A`, while izu uses durable checkpoint semantics; the raw operation vectors are recorded because these are different acknowledgement contracts. Git preload index and untracked cache are enabled, fsmonitor is disabled, and all Git state is local. These timings compare index capture rather than claiming Git commit equivalence. The 1/8/30 writer cases use optimized Git worktrees and izu sibling workspaces from the same generated baseline. They admit at most four concurrent writer workflows, queue the remaining logical writers in batches, integrate into one target, and independently hash every original text/binary file and each unique merged change. Each count defaults to three independently materialized fixtures and retains all raw whole-writer wall spans plus p50/p95. Git spans include every writer’s `add -A` and commit; izu spans include full authored commit capture. Queue delay between batches is included. Accepted changes count only independently validated merged source additions across successful samples. The startup/capture sample count and cooperative sample count are separately configurable. Full command records also retain setup and integration timings. Izu additionally runs exact-candidate checks before guarded land; Git merge has a different check/durability contract. Standalone independent repositories are not substituted for cooperative writers. Workspace-copy payload is capped at 128 MiB per case; oversized variants remain explicitly blocked.

First invocation/capture and repeated warm runs are labeled separately. OS caches are uncontrolled: the harness does not evict machine-wide caches and does not call first invocation “cold disk.” Failed invocation latency remains in the report but cannot establish successful workflow performance. A workflow stops after its first failed sample and preserves its command/output and failure reason.

The `merge` row creates independent fixtures at the configured tree size. Two private writers alter separated lines of one 80-line file from a shared revision. It measures the real CLI's divergent candidate preparation, including process startup and durable candidate publication. Exact checks, guarded land, both revision parents, and independent final source hashes prove that both edits and every generated input survived; their command records remain separate validation phases. This is a durable CLI merge workflow, not a pure algorithm microbenchmark. Its four source views are capped at 128 MiB of generated payload.

Dependency rows use a small real offline Rust build: `rustc` compiles a local dependency to an rlib, links an application against that rlib, and executes the application. No Cargo registry, package resolution or network access is involved. Supply both `--rustc` and `--expected-rustc-sha256`; an absent pair leaves these rows explicitly blocked. A partial pair or invalid/mismatched digest fails before any fixture or product command. The canonical selected compiler is hashed before fixtures, around managed builds/checks, and after the benchmark. Its actual `-vV`, host and byte identity are recorded. The identity pins that compiler executable; standard-library and system-linker bytes are not attested.

Each dependency sample has its own repository/cache and uses the managed runtime for actual builds. The cold row begins with absent dependency and output paths, builds them, imports the stopped prepared roots as a verified immutable artifact, writes a native environment binding, and runs a candidate check against its exact result and verified starting files. The warm row materializes that artifact into a second private workspace, independently matches the rlib and prepared executable bytes, and rebuilds/executes the application without compiling the dependency. It also runs a fresh environment-bound candidate check; successful warm reuse counts zero accepted source changes. The changed row changes both dependency source and declared lockfile, requires different source/key/binding and compiled outputs, rejects a check using the old binding as `stale_expectation`, rejects its land as `missing_check`, and checks/lands a fresh candidate with the new binding. Old prepared/cache bytes and primary source are independently read back unchanged.

Dependency latency is the sum of the explicitly measured CLI phases described in `workflow.timing_scope`. Cold/changed totals include managed compilation, import, bind, candidate preparation and environment-bound check; warm totals include materialization, managed application rebuild, candidate preparation and environment-bound check. Private workspace setup, authored input commits, cache/status readbacks, negative refusals, land and independent hashing remain recorded outside those totals. All command records use the existing deadline, output caps, process cleanup, optional RSS wrapper and allocation observations. Recipes, exact argv, artifact byte hashes, binding/key/manifest/source identities, phase-to-run indexes and actual starting-file receipts remain in `workflow.samples`. Warm outputs are writable after the receipt; the receipt proves their starting contents, not an immutable execution environment. These tiny fixtures prove the measured flow, not performance of arbitrary application dependencies.

Physical allocated bytes use POSIX `st_blocks * 512`, including repository metadata, directory allocations and file data, without following links. They are filesystem allocation, not bytes read or written; APFS clone/compression sharing may mean sums do not equal exclusive physical device consumption. Read/write I/O bytes remain explicitly unavailable. An unwrapped latency pass reports RSS as unavailable; a separate `--measure-rss` pass uses `/usr/bin/time -l` on macOS or `-v` on Linux and records the real reported peak in bytes. Its wall latency includes measurement-wrapper startup, so do not combine its samples with unwrapped timings. The wrapper and descendants live inside the owned process group. No logical `du` value is presented as physical allocation. System `time` measurements require that utility on the supplied platform. Truncated output or timeout makes peak RSS unavailable; a missing numeric field is not zero. Polling resident memory would miss short-lived peaks. The reported maximum is the system child-process resource statistic, not concurrent whole-machine RSS or a sum of writer peaks.

Do not select an optimization from one noisy sample. Identify the dominant operation from the baseline, inspect its actual work, change the smallest relevant path, and repeat on the same fixture/seed/sample count/filesystem/build profile. Report p50/p95, raw samples, artifact/source digests, actual accepted source changes and allocation. Also state where durability, concurrency, output volume or semantics differ. A faster failed or weaker operation is not a gain.

The standalone `git-baseline` accepts `--samples 3`, `--scratch-dir` and `--source-root` and records the same repeated 1/8/30 logical writer measurements without claiming any izu result. Earlier one-sample commit-only reports are preliminary evidence and cannot be compared to the full writer spans.

Repeated operation samples stop at the first failed invocation and retain that failed sample. Requested sample counts and observed raw counts are distinct; a truncated series cannot establish a successful latency distribution. This avoids spending more time repeating an operation that already fails the selected deadline. The harness worker executable hash is checked again at the end.

## Observed capture optimization

A local macOS arm64 diagnostic on 2026-10-04 UTC compared frozen debug builds
before and after batching object persistence. Both used the same 512-file,
4,096-byte, seed-1729 fixture on the same filesystem and an unchanged 30-second
command deadline. Other task lanes were idle during each run; OS caches and
unrelated applications were not controlled.

| Native workflow | Before | After | Samples per build |
| --- | ---: | ---: | ---: |
| First full checkpoint | 21.082 s | 1.541 s | 1 |
| Unchanged checkpoint, median | 15.132 s | 1.262 s | 3 |
| One changed-file checkpoint | 14.582 s | 1.278 s | 1 |
| Diff | 16.035 s | 1.425 s | 1 |

Every command succeeded; independent source-byte and executable-hash readbacks
matched. The unchanged-checkpoint median improved about 12 times. These are
native diagnostic measurements, not release-build or concurrent-workflow
results. The after build also includes a runtime close-identity correction and
documentation changes; these native commands do not launch managed jobs.

Before executable SHA-256:
`efea313cd4112881ce78023560ca6653b755659170f7dd396a242df24e6f5afb`.
After executable SHA-256:
`62d551987110b8142da178bfa5753f1fdef0d795165694c46d7311f4fc124f5d`.
Local raw samples, source snapshots, build records and the comparison are kept
under `.artifacts/capture-profile/`. These generated artifacts are excluded
from version control; retain them separately when reviewing the measurement.
