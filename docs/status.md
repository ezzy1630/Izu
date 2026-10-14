# Development status

As of 2026-10-14, izu is an experimental 0.1.0 development build. It is not a
qualified production release. Do not rely on this build as the only copy of
important work.

The original Rust engine, CLI, JSON/MCP interface, private workspace runtime,
prepared environments, native bundles and optional Git adapter are integrated.
All interfaces use the shared engine. The project is Apache-2.0 licensed;
see [NOTICE](../NOTICE) for attribution.

## Recorded candidate validation

The latest local candidate, R6, has these recorded results:

| Environment | Default tests | Fault-suite tests | Extracted package workflows |
| --- | ---: | ---: | ---: |
| macOS ARM64, macOS 26.6.2/APFS | 439 passed | 237 passed | 15 passed per extraction, two extractions |
| Linux ARM64, Ubuntu 24.04.5/glibc 2.39, guest-native ext4 | 447 passed | 239 passed | 15 passed per extraction, two extractions |
| macOS x86-64 | Final candidate jobs never started | Unqualified | Unqualified |
| Linux x86-64 | Final candidate jobs never started | Unqualified | Unqualified |
| Windows x86-64 | Deferred | Unsupported | Unsupported |

Default and fault suites overlap. Six default and four fault tests are private
subprocess helpers invoked by parent tests. Both ARM64 environments passed
formatting and strict default/fault Clippy checks. Each environment produced two
byte-identical archives and exercised the executables extracted from both.
Linux execution used a native AArch64 Docker virtual machine, not emulation.
Older macOS, older libc and other Linux distributions remain unqualified.

These are local campaign results, not final hosted CI results. Raw logs, failed
fixtures and exact source/binary manifests remain in the maintainer's private
`.artifacts` directory and are excluded from Git. R6 used Rust 1.99.0 and this
199-file source manifest:
`3d8815df7b030639f5c3e8e981dd962045d28644b108029128a512de9b7e7edb`.
The public development update changes documentation, CI and the disposable
acceptance harness beyond that frozen source; its Rust source and lockfile are
unchanged. Each result applies only to its recorded source, executable and
environment. It does not qualify a later packaged artifact.

The exact macOS packaged executable passed a real Codex CLI campaign with five
independent agents, a concurrency cap of four and queued fifth, correct private
working directory before first write, real commands/edits, conflict and stale
rejection, checked integration, cancellation, restart and execution deadline.
Observed agent processes were absent after cleanup. Cooperative process
management is not a security sandbox. See [agent host integration](agent-host.md).

The same executable passed authenticated disposable GitHub HTTPS clone, fetch,
import, edit, export, exact leases, stale rejection, authentication failure and
independent network content/history/fsck readback. The two-writer race observed
one winner and a client lease-precheck rejection; it did not prove server-side
compare-and-swap contention. Actual SSH, protected-branch enforcement,
repository-scoped read-only denial and visible interrupted-publication gates
remain unrun. Local fixtures are separate evidence.

Both ARM64 packages passed bounded persistent HFS+/ext4 kernel ENOSPC fixtures,
prior acknowledged HEAD preservation, recovery/retry and orderly remount
readback. Separate source-equivalent fault builds exercised process crashes,
concurrent writers, corruption and interrupted reclamation. Neither proves
physical power-loss behavior. The recorded RustSec review found no advisories
or warnings for the candidate lockfile against the October 13 database; it does
not cover the operating system or separately selected Git/SSH tools.

## Scope and remaining release gates

The intended first-release targets are macOS ARM64/x86-64 and Linux ARM64/x86-64
on native persistent storage. This target list is not a support qualification.
Windows is on hold and unsupported. Docker Desktop host-shared repository,
cache and workspace storage is excluded by the approved first-release boundary;
its prepared-cache identity incident remains open. Matching file bytes did not
explain conflicting identities, and no identity checks were bypassed. Use an
owned native Linux volume with an init reaper, transfer captured history with a
native bundle and prepare dependencies there.

The approved Git boundary defers submodules, LFS payload hydration/upload and
Git working-tree conversions. Pointer interchange and attribute bytes are
preserved with warnings. SHA-1 branches/tags, unchanged opaque signature bytes,
modes, symlinks, merge ancestry and supported repository layouts are implemented.
Preserving a signature does not verify it; rewritten content does not retain a
valid old signature. Other restrictions remain explicit in
[Git interoperability](git-interop.md). No complete Git adoption is claimed.
Native metadata-v2 migration requires explicit exact-HEAD approval and retained
backup; tags and selected-object reclamation use the shared engine.

Required reliability evidence remains missing for the historical MCP
first-discovery 15-second timeout and Linux 30 ms HEAD admission failures.
Later successful runs did not explain them. The recovered MCP source passed its
original serial cases, but the failed executable and stalled-child evidence
were not retained. These incidents are distinct from the fixed MCP
output-shutdown and macOS initialization-cost defects. Earlier Linux ARM
30-second controller setup and 120-second close failures also remain
unexplained; visible checkpoints are not durable close acknowledgements.
Diagnostic capture must retain live process, pipe and lock ownership on a
future reproduction rather than declaring the incident resolved by a rerun.

Public macOS Intel CI also exposed a failure in the diagnostic collector itself:
native stack commands remained unfinished when its seven-second owner bound
expired. Raw reports and both failed jobs are retained. A four-second Python
thread watchdog and early startup timestamps collect further evidence, and a
backpressure regression checks bounded stdout/stderr retention, overflow counts
and descendant cleanup. Later passing Intel default runs do not identify the
earlier blocking call. A subsequent failure retained an empty watchdog stream;
startup delay and a later stall remain distinguishable hypotheses, not proven
causes. This collector incident remains open pending startup and stack evidence; it is separate from the original MCP discovery timeout.

An Intel fault-suite admission regression also returned an unlabelled channel
`Timeout`. Its fixture required a gate releaser to wake before a 200 ms request
expired; deliberately delaying release reproduced that fixture failure. Separate
checks now hold the gate through expiration and consume registration time inside
the request before the native wait. The 200 ms request budget and existing native
wait assertion remain unchanged. The original CI log does not identify which
channel timed out, so that particular scheduling cause remains unconfirmed.

Another Intel provenance-test failure lacked the descendant marker after a
negative probe. The fixture used nested Python startup under an artificial
one-second limit and accepted any probe error. It now uses native fixture
commands and checks the specific error for each case. The one-second override for every case, production probe bounds and the
test's four-second return assertion are unchanged. The original probe error was not
retained, so its exact failure cause remains unknown.

Final Intel jobs did not execute because GitHub rejected the private campaign's
jobs for account billing/spending limits. Earlier native CI passes belong to
earlier sources and packages. No alternative final Intel environment was
available. Final SSH/protection and interrupted-publication acceptance remains
incomplete. There is no public binary release or Developer ID/notarization;
macOS candidates have only a linker ad hoc signature and failed Gatekeeper
assessment. No daily-driver installation or system policy change was performed.

## Performance

The original 512-file macOS benchmark measured median unchanged capture at
379.82 ms, changed capture at 385.02 ms and diff at 371.25 ms. Corresponding Git
operations measured 13.87 ms, 14.05 ms and 26.41 ms; acknowledgement semantics
differ and cache state was uncontrolled. Focused measurements reduced workspace
creation by 61.46% and candidate file checks by 47.10%.

A later five-sample, 20,000-file paired experiment measured median unchanged
capture improving from 13.308 s to 9.395 s and diff from 14.164 s to 8.333 s.
Those earlier binaries and results remain separate from the final R6 package.
The final package measured checkpoint p50/p95 at 18.779/55.647 s and diff at
26.777/35.204 s; its earlier baseline run measured 12.190/12.895 s and
11.320/14.731 s. Host activity differed, so no causal speed ratio is claimed.

The final candidate passed 284 scale commands and source/history readbacks.
A 64 MiB file measured checkpoint p50/p95 at 0.648/0.704 s and diff at
0.491/0.527 s; 200 authored commits had log p50/p95 at 0.074/0.077 s.
A 144-package, 6,672-file prepared payload passed nine private views and mutation
isolation: initial import 44.652 s, warm import 0.792/0.985 s and private-view
creation 47.039/51.477 s. Four concurrent views took 115.911 s with sampled
aggregate RSS 49.25 MB and logical/allocated disk 752.721/919.294 MB. This is a
file-payload test, not dependency-installer or relocated-build execution proof.
OS caches were not purged; sampled RSS can miss peaks; APFS allocated bytes do
not establish exclusive device use. See [scale validation](scale-validation.md),
[benchmark methodology](benchmark-method.md) and [validation](validation.md).
