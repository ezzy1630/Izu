# Historical failures and public CI

Inspected on 2026-10-17. Local diagnosis and fixes are not a new hosted CI result
or a public binary release.

## Linux HEAD timeouts

The original failing storage suite used a host directory bind-mounted into
Docker Desktop as its temporary filesystem. Two tests used a 30 ms acquisition
budget: `lock_wait_is_bounded_and_recovers_on_drop` and
`head_absence_is_confirmed_without_relocking_guarded_readers`. That budget covers
queue metadata, lock acquisition and subsequent identity validation, not just
waiting for another holder.

A preserved original test executable (SHA-256
`37e98acd13e14bfd82a7434c74e5872300564fd4a6f26874ec7997548b99b644`)
was compared in the same Linux container against shared virtiofs and native ext4:

| Experiment | Shared host filesystem | Native ext4 |
| --- | --- | --- |
| Eight serial runs of the full 22-test suite | 8 passed | 8 passed |
| Eight runs with four concurrent suites | 2 failed | 8 passed |
| Sixteen runs with eight concurrent suites | 14 failed | 16 passed |

Only the two historical 30 ms tests failed in the unmodified binary comparison.
A diagnostic rebuild of the frozen original source added failure-phase labels,
a retry counter and stacks after the timeout decision. Sixteen runs at eight
concurrent suites failed 15 times on shared storage and passed all 16 on ext4.
Captured failures include the first uncontended transaction and initial HEAD
read, with zero wait retries. One trace expires at the final budget check after
the kernel lock succeeded. Filesystem operations and scheduling can consume the
whole deadline even when no other process owns the repository lock.

This reproduces and explains the historical failure *pattern*. The original
failed process had no stack capture, so it does not establish which individual
system call was slow in that run. It is not evidence of lost history or a lock
being retained after drop. No timeout was increased and no safety check removed.
The existing Linux qualification helper already uses an owned native volume
without host bind mounts; continue qualifying and running Linux repositories on
native storage. This comparison does not certify arbitrary shared filesystems
or establish physical power-loss durability.

A subsequent public macOS Intel CI run reproduced `LockTimeout("head.lock")`
in `lock_wait_is_bounded_and_recovers_on_drop` on native storage. The failure
was outside the assertion for the deliberately blocked contender: setup or
post-drop acquisition also used the 30 ms budget. This shows that shared storage
is one way to expose the fixture assumption, not a prerequisite for it. The
historical receipts still cannot identify their exact slow operation.

The lock tests now use a normal-budget holder for setup and successful acquisition,
while the contender retains its 30 ms budget and one-second return assertion.
A deterministic fault-injection test consumes the admission budget with no other
holder, requires the timeout, and verifies that its claim is released. Production
deadlines and persistence checks are unchanged. The corresponding Intel artifact
upload failed independently after five GitHub artifact-service request timeouts;
that was neither a billing refusal nor missing diagnostic files.

## macOS cancellation and a concurrent fork

Public ARM64 CI reported a cancelled check whose leader was killed and reaped,
but whose process group remained present at the unchanged five-second deadline.
The runtime refused close and retained the failed attempt, writer intent and files.
A local reproduction populated an owned group and forked one final child during
shutdown. It produced the same cleanup failure, and a process-state capture
showed a live `sleep` child in that group with its parent already reaped.

The regression failed before the fix on attempt 3. Cleanup now observes leader
exit without reaping, performs a final group sweep while the identity is still
owned, then reaps and requires independently observed group absence. The 32-case
regression and a separate 500-case fork probe passed after the change. No real
signals follow reaping, no deadline is increased, and escaped or still-present
groups remain uncertain. The original CI run did not retain its process table,
so the reproduction establishes this concrete failure mechanism without claiming
to identify the exact surviving process in that earlier run.

Runtime test failures now capture only their recorded groups' process identifiers
and state and preserve their synthetic fixtures in CI artifacts.

## MCP first discovery

The failed serial test timed out waiting 15 seconds for the initial discovery
response. Its stderr was discarded and no raw partial response, process stack,
pipe ownership or exact failed executable hash was retained. Discovery describes
the server; it does not enter repository HEAD admission, so the Linux incident
is not a demonstrated explanation for this failure.

The recovered original source previously passed all three original serial MCP
tests when rebuilt. A preserved original-context executable now passed 64 first
request sessions: 32 serial and 32 at concurrency four, half with an incomplete
following frame to keep the input reader occupied. The slowest response was
0.579 seconds. Its hash is
`3cd25a851fbb1555db904ed804206640009120687d47886ab898b214fcb4ef1b`;
the failed receipt cannot prove that these were the exact failed bytes.

The historical cause remains unresolved. Passing reruns are not a fix. Current
first-discovery coverage also sends a partial following frame, requires the
initial reply before completing that frame or closing stdin, then completes the
next exchange. All 17 current MCP tests passed locally, including output failure,
backpressure, cancellation and before-cleanup diagnostics. Current
tests retain raw failure evidence and capture child/parent stacks and pipe
ownership before cleanup at the unchanged deadline. A recurrence with that
capture is the next useful evidence; more passing soak counts cannot substitute
for it. Fixed output-shutdown and initialization-cost issues remain separately
attributed defects.

A later Intel CI run exposed a diagnostic harness error, not a discovery failure:
the controlled command had timed out and been reaped, but a sibling native stack
probe was still running when the seven-second collector bound expired. The owner
stopped the group and marked the command interrupted, yet left the top-level
capture status as `running`. A real sleeping-child regression reproduces that
stale state. The owner now records `interrupted`; the command test still requires
its controlled terminal result, bounded output, every native launch and descendant
absence. It no longer requires unrelated probes to finish before an intentionally
bounded collector stops. The 15-second response and seven-second collector
limits are unchanged; incomplete controlled evidence still fails the test.

During that work, one local EOF/backpressure test exceeded its five-second exit
bound with the server still alive. A subsequent six-round instrumented run passed,
which does not explain the failure. Its raw observations are retained; the EOF
failure path now captures live stacks and pipe ownership before cleanup, just as
the response-timeout path does. This observation remains under investigation.


## Use the public GitHub repository

The intended source CI target is [ezzy1630/Izu](https://github.com/ezzy1630/Izu).
Actions is enabled. GitHub documents that [standard hosted runners are free for
public repositories](https://docs.github.com/en/actions/concepts/billing-and-usage),
and [protected branches are available on GitHub Free for public repositories](https://docs.github.com/en/repositories/configuring-branches-and-merges-in-your-repository/managing-protected-branches/about-protected-branches).
The disposable private acceptance repository's billing/plan failures do not
require buying a plan or moving normal public source CI elsewhere.

The last inspected [main run, 38652771812](https://github.com/ezzy1630/Izu/actions/runs/38652771812),
at `c749fe2e285fe7ff4baa566a4ee3b2dd753cdd78`, passed all three validation jobs
and two package jobs. The macOS ARM64 package job failed with `missing or
ambiguous rustc host`. This was an executed code failure, not a billing refusal.

A deterministic real-child regression exposes a race in the compiler probe:
`select()` can return empty just before the child writes its final output and
exits; polling after that snapshot then exits the loop without reading the
output. The worker now observes child exit before taking the readiness snapshot
and drains ready data before completion. The original implementation fails the
regression; the fixed implementation passes the seven adjacent identity tests
and actual native compiler selection. Limits and process-group cleanup remain
in place. The old CI run did not retain raw compiler output, so the race explains
the failure mode but cannot be proven to be that run's exact cause.

Compiler probes are now persisted before identity parsing. CI uploads explicitly
include hidden files only under the existing evidence paths, so `.artifacts`
contents are retained on failure. The public repo's main branch was unprotected
and had no rulesets when inspected. Publishing the reviewed source, observing a
fresh six-job run and configuring its required checks remain separate actions;
none are claimed complete by local verification.
