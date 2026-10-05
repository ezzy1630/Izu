# Dependency and unsafe boundaries

The IZU crates deny unsafe Rust. The product uses an original native history
engine; it does not link another version-control engine. The optional Git
adapter invokes a selected Git executable for interoperability. That executable
is a separate trusted tool and is not covered by Cargo's dependency audit.

The locked dependency review on 2026-10-04 used Cargo.lock SHA-256
`b807a9faa2fbe1e667427c2ec296d801794a150719a693f65ef18c6e694054b7`.
Cargo-audit checked 229 package records against RustSec database commit
`ef6173cbc5c50ec8166f9a5b28f07834144373ee` (1,290 advisories, updated
2026-10-03): no known vulnerabilities or informational warnings were reported.
This is a dated result, not a guarantee about future advisories. Changes to the
lockfile require a fresh audit and review of newly selected features.

## Reviewed boundaries

| Boundary | Selected implementation and review |
| --- | --- |
| Filesystem and process syscalls | Rustix 1.1.5 provides descriptor-relative operations, owned and borrowed file descriptors, directory iteration, locking, process signals and synchronization. The platform adapter uses these safe interfaces. The reviewed paths preserve descriptor lifetimes, avoid following source links, bound link reads and distinguish kernel synchronization from the Apple full persistence barrier. Linux uses Rustix's target-specific syscall implementation; macOS crosses libc. |
| MCP transport readiness | The CLI also selects Rustix's `event` feature. Its safe `poll` API borrows live descriptors for a bounded readiness set; the reviewed dependency boundary passes their platform representation to Linux `ppoll` or macOS libc `poll`, with checked timeout conversion. The adapter temporarily enables nonblocking pipe I/O and restores the original nonblocking bit on return while preserving other flags. Shared descriptor aliases require the documented exclusive host lease; forced process death cannot promise flag restoration. macOS and Linux subprocess tests exercised output disconnection, stalled output and descriptor-flag restoration. |
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
