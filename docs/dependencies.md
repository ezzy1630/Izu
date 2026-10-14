# Dependency and unsafe boundaries

The IZU crates deny unsafe Rust by default. The platform crate has narrowly
scoped Windows FFI exceptions documented in its boundary notes; Windows is on
hold and is not a supported release target. macOS/Linux product paths use safe
Rust APIs. The original native history engine does not link another version
control engine. The optional Git adapter invokes an explicitly selected Git
executable, which is a separate trusted tool outside Cargo's dependency audit.

The locked dependency review on 2026-10-14 used Cargo.lock SHA-256
`5184fbd0de3d658731b229163d8316f989570b0b29879773aeb383abcc11684e`.
Cargo-audit 0.22.2 checked 229 package records against RustSec database commit
`6aed84668c23d7f535077a9fe918bf1fa0f6a5bc` (1,310 advisories, updated
2026-10-13). The current database was fetched; no known vulnerabilities or
informational warnings were reported, with warnings configured as errors and
no ignored advisories. This dated check does not establish future advisory
status, transitive soundness, or safety of installed external tools. Lockfile or
feature changes require another audit and boundary review.

## Reviewed boundaries

| Boundary | Selected implementation and review |
| --- | --- |
| Filesystem and process syscalls | Rustix 1.1.5 provides descriptor-relative operations, owned and borrowed file descriptors, directory iteration, locking, process signals and synchronization. The platform adapter uses these safe interfaces. The reviewed paths preserve descriptor lifetimes, avoid following source links, bound link reads and distinguish kernel synchronization from the Apple full persistence barrier. Linux uses Rustix's target-specific syscall implementation; macOS crosses libc. |
| MCP transport readiness | The CLI also selects Rustix's `event` feature. Its safe `poll` API borrows live descriptors for a bounded readiness set; the reviewed dependency boundary passes their platform representation to Linux `ppoll` or macOS libc `poll`, with checked timeout conversion. The adapter temporarily enables nonblocking pipe I/O and restores the original nonblocking bit on return while preserving other flags. Shared descriptor aliases require the documented exclusive host lease; forced process death cannot promise flag restoration. macOS and Linux subprocess tests exercised output disconnection, stalled output and descriptor-flag restoration. |
| Worker authentication | Getrandom 0.3.4 obtains a 32-byte OS-random nonce for the loopback fallback used only when a private Unix socket pathname cannot be represented. The nonce enters the worker through its inherited input pipe, never arguments, environment or diagnostic output. At most eight pending peers and a fixed 41-byte authentication record are admitted; only a complete matching nonce, protocol marker and worker PID receives the process ticket. Partial prefixes disclose no match result. Existing cancellation/admission deadlines apply. Native subprocess and malformed/partial-peer tests exercise this boundary. Cooperative process ownership is not a security sandbox. |
| Windows FFI (deferred) | File identity borrows a live handle for a checked `FILE_ID_INFO` query; initialization is read only after API success. The dormant Windows directory implementation scopes NT/Win32 unsafe calls in its private `nt` module, validates relative components/reparse behavior and owns returned handles. Its diagnostics do not establish a Windows durable release. See the platform boundary notes and Windows hold in README. |
| Source hashes | SHA2 0.10.9 supplies SHA-256. The observed arm64 native-core build selects `default/std`, without the optional assembly feature. The fixed-block conversion through GenericArray is an unsafe dependency boundary. Native format vectors were independently recomputed. Other architectures can select different backends and need their own build evidence. |
| Git TLS and cryptography | Reqwest 0.12.28 selects Rustls 0.23.45 and Ring 0.17.14 with web PKI roots. Ring's target-selected C and assembly are a reviewed dependency boundary, not an IZU C subsystem. Its packaged build script compiles local packaged sources; a C toolchain is required. Rustls uses Ring's safe key, random and verification interfaces. Cryptographic primitives were not independently re-proven. |
| HTTP policy | The adapter requires HTTPS, disables ambient proxies and redirects, bounds explicit trust anchors and response bodies, and applies connect/read/whole-request deadlines. It does not offer a certificate-verification bypass. Owned TLS fixtures exercise certificate rejection, authorization, cancellation, receiver policy and receipt verification. |
| Compression | Flate2 1.1.10 explicitly selects its Rust backend. OpenSSL, AWS-LC and C zlib are not selected by the reviewed macOS/Linux dependency graphs. Pack decoding has separate compressed, expanded, object-count and delta-depth limits. |
| Cancellation signals | The CLI uses Signal-hook's safe iterator for SIGINT/SIGTERM. Signal handling is performed inside that library's syscall boundary; the product cancels on a normal listener thread, closes the iterator and joins that thread on cleanup. It does not register arbitrary product code as a raw signal handler. |
| Names and input parsing | Serde, Unicode normalization and full case folding operate behind bounded product inputs. The native format pins normalization/case-folding behavior and rejects noncanonical encodings. Safe APIs still depend on their crates' internal unsafe code and Rust's allocator. |

The review includes source inspection, target dependency/feature resolution,
known-advisory checks and the exercised boundary tests. It is not a line-by-line
soundness proof of every transitive crate, libc, the Rust standard library, the
kernel or the selected Git binary. Resource admission and checked arithmetic do
not make allocator exhaustion recoverable in every third-party allocation.

No hand-written assembly or C was added to optimize IZU. Changes that add a new
native dependency, enable a cryptographic provider or introduce product-owned
unsafe code require an explicit boundary review and tests on the affected target.
Keep the dependency graph, audit database identity and built artifact identity
with release evidence; a successful audit alone does not establish runtime
correctness or persistence.
