# Native scale validation

`scripts/scale-validation.py` runs the exact supplied executable on bounded,
owned generated-source fixtures. It supplements [benchmark-method.md](benchmark-method.md).
Expected executable SHA-256 and a frozen candidate identity are mandatory.
Before/after executable hashes, candidate/compiler/build identity, platform,
filesystem, script hash, argv/cwd, exit, timeout, raw samples and command outputs
are recorded beneath a new directory inside the project `.artifacts/local`.
The frozen driver is retained beside the report.

The default campaign exercises:

- 5,000 and 20,000 code-like files of 4,096 bytes each: first checkpoint, five
  unchanged checkpoints, five unchanged diffs, one changed-file checkpoint/diff and
  native object verification. An authored baseline is established before unchanged diffs. Independent source-byte hashes must match.
- One 64 MiB generated text file using the same CLI operations.
- 200 authored commits in a 32-file tree, each changing one generated file,
  followed by repeated history traversal and checkpoint.
- One repeated managed private change and four concurrently launched changes on
  a 512-file tree. Primary source must remain unchanged. These managed launches
  do not establish cooperative checked integration.

Generated payload is bounded at 256 MiB per tree. Commands have a declared
120-second default deadline. A failed operation stops that case and retains its
fixture and command evidence; independent cases continue. Successful fixtures
are removed after readbacks. Reports never overwrite prior evidence. Project
source, remotes, global Git configuration and credentials are untouched.

Wall timings include executable startup, process observation and memory-sampling
overhead. Generation, independent hashing and allocation walks happen outside
target timing. First invocation and repeated warm samples are separate. OS
caches and unrelated activity are uncontrolled. Python deadline polling can add
observation delay, especially to short commands. Percentiles use nearest rank;
five samples make p95 the maximum. Failed latency is not successful performance.

The memory sampler invokes `ps` about every 50 ms and sums resident bytes across
owned process groups and observed descendants. Short peaks and reparented
children can be missed; shared pages can be counted repeatedly, and kernel
memory is absent. Its sampled aggregate maximum is not a system peak statistic
or exclusive physical-memory allocation. The original `baseline-facade-r1`
campaign uses the earlier group-only sampler, excluding managed children that
create separate groups. Its native rows observe the invoked CLI process group.
Do not claim complete aggregate managed-workflow RSS from that campaign.

Logical lengths and allocated `st_blocks * 512` are separate. APFS sharing and
compression mean summed allocation is not exclusive device usage. Neither
measures I/O bytes. Allocation walks do not run during concurrent calls.

Example, from the intended main checkout:

```sh
python3 scripts/scale-validation.py \
  --executable "$PWD/.artifacts/local/CANDIDATE/bin/izu" \
  --expected-sha256 EXACT_BINARY_SHA256 \
  --candidate-identity "$PWD/.artifacts/local/CANDIDATE/identity.json" \
  --project-root "$PWD" \
  --output "$PWD/.artifacts/local/scale-validation/UNIQUE_RUN" \
  --samples 5 --history 200
```

Use `--cases tree5000 tree20000` for explicitly scoped capture/diff evidence.
Exit 0 requires every requested case and final binary readback to pass. Exit 2
retains partial and failed evidence. Retain the frozen source and recorded build
provenance: candidate identity alone does not prove how the binary was built.

The historical `baseline-facade-r1` driver checkpointed its generated files
without authoring a baseline. Its diff rows therefore compare all added source
against the initial empty authored head. They are full uncommitted diffs, despite
the historical `warm_diff` phase label. At 20,000 files the CLI refused to encode
that result within its output byte limit. The current driver authors a baseline
before measuring unchanged diff, and also measures a changed-file diff. Keep
these workloads distinct when comparing results.

## Duplicate publication optimization evidence

The local 2026-10-06 comparison used release builds from one frozen source,
with only `crates/izu-store/src/batch.rs` changed in the candidate. Existing
objects are opened before trying a no-overwrite hard link. Missing names still
use the no-overwrite link and handle concurrent publication. Duplicate byte
verification, inode checks, fault boundaries, kernel syncs and full namespace
persistence barriers remain in place.

Both runs passed 512, 5,000 and 20,000-file source and semantic readbacks with
five samples per repeated operation. The source-bound store suite passed,
including duplicate corruption, replacement and concurrent history checks;
new tests cover duplicate inode/barrier retention, replacement after identity
recording and four simultaneous duplicate batches.

| Files | Operation | Baseline p50 seconds | Candidate p50 seconds | Reduction |
| --- | --- | ---: | ---: | ---: |
| 512 | Unchanged capture | 0.400 | 0.297 | 25.8% |
| 512 | Empty diff | 0.349 | 0.242 | 30.5% |
| 5000 | Unchanged capture | 2.964 | 2.253 | 24.0% |
| 5000 | Empty diff | 2.844 | 2.021 | 29.0% |
| 20000 | Unchanged capture | 13.308 | 9.395 | 29.4% |
| 20000 | Empty diff | 14.164 | 8.333 | 41.2% |

Private receipts are under `.artifacts/local/scale-validation/`: the
`frozen-baseline-rerun-r1` and `frozen-batch-candidate-r1` reports retain raw
commands, outputs and samples; `frozen-batch-comparison-r1.json` binds the two
executable hashes and common driver hash. Build provenance is under
`.artifacts/local/verification/perf-frozen-batch-build-r1`; store checks are
`perf-duplicates-resume-r1` and `perf-duplicates-resume-r3`.

The runs were serial, candidate then baseline, with uncontrolled caches and
host activity. These are generated-fixture observations, not a general speedup
guarantee or full release qualification. The comparison does not measure I/O
bytes and does not establish changed performance for large-file, long-history
or managed-agent workloads. No duplicate validation or durability was skipped.
