# Development status

As of 2026-10-05, izu is an experimental 0.1.0 development build. It is not a
qualified production release. Do not rely on this build as the only copy of
important work.

The original Rust engine, CLI, JSON/MCP interface, private workspace runtime,
prepared environments, native bundles and optional Git adapter are integrated.
All interfaces use the shared engine. The project is Apache-2.0 licensed;
see [NOTICE](../NOTICE) for attribution.

## Recorded local validation

| Environment | Default tests | Fault-suite tests | CLI workflows |
| --- | ---: | ---: | ---: |
| macOS ARM64 | 374 passed | 206 passed | 15 passed |
| Linux ARM64, guest-local ext4 | 382 passed | 208 passed | 15 passed |
| Linux ARM64, Docker Desktop host-shared storage | Earlier stages passed | Prepared-cache import failed | Final acceptance not reached |

Default and fault suites overlap. Private child-process fixture tests are
invoked by their parent tests. The passing workflows exercise CLI, JSON and MCP,
bundle restoration, local Git exchange, conflicts, checks and guarded
integration. Thirty managed jobs used separate workspaces with an observed
maximum of four admitted jobs running at once.

These are historical local results from the validation campaign, not hosted CI
results. Raw logs, failed fixtures and exact source/binary manifests remain in
the maintainer's private `.artifacts` directory and are excluded from Git.
The tested source manifest is
`9399290a774b8e861cac0103e26eb88848eaeb79fc0db77154cdbb4ae1004544`.
Subsequent publication changes update licensing, attribution, metadata and
documentation; they do not change Rust implementation or test code. Fresh CI
results apply to their own recorded commit and environment.

## Open release work

- Prepared-cache imports can fail their file-identity check on the tested
  Docker Desktop shared mount. The cause is unresolved. Passing ext4 results
  do not qualify that shared mount. The identity guard remains enforced, and
  a native-storage-only release boundary has not been adopted.
- Historical MCP first-discovery timeouts and two short-deadline Linux storage
  acquisitions remain unexplained. Later passing runs do not establish their
  cause. Separate MCP output-shutdown and initialization-cost fixes are retained.
- Real agent-host integration and authenticated GitHub acceptance remain
  unverified. Local Git and loopback HTTPS fixtures are not live GitHub evidence.
- Windows is unsupported. Other CPU architectures and Linux ABI baselines
  require implementation or target-specific validation.
- SSH, signed commits, submodules and Git LFS are unsupported; tag handling is
  restricted. See [Git interoperability](git-interop.md) for the complete limits.
- Larger repositories, long-lived storage reclamation, extracted distribution
  packages and release signing still need work. No public binary release is
  qualified by these local results.

Process-crash and injected persistence-failure tests passed their recorded
scopes. A bounded real kernel ENOSPC check used volatile storage with durable
acknowledgement disabled. It does not establish persistent-media exhaustion or
physical power-loss behavior.

## Performance

The final macOS benchmark used 512 files of 4,096 bytes, seven normal samples
and three cooperative repetitions. Median unchanged capture was 379.82 ms,
changed-file capture 385.02 ms and diff 371.25 ms. Corresponding Git operations
measured 13.87 ms, 14.05 ms and 26.41 ms; acknowledgement semantics differ and
OS cache state was uncontrolled. izu is not generally faster in these results.

Focused paired measurements reduced workspace creation by 61.46% and candidate
file checks by 47.10%. Broader performance and scale claims require further
measurement. See [benchmark methodology](benchmark-method.md) and the
[validation procedure](validation.md) for reproduction and evidence limits.
