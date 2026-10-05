# Native storage and durability

Status: implemented storage boundary, tested on macOS APFS with Rust 1.99.0. These results cover process termination, corruption, concurrency, and injected I/O errors. They do not demonstrate sudden power-loss behavior or production readiness. The integrator owns independent Linux execution, actual filesystem-full testing, and complete engine workflows. The current IZU checks use a fresh `target-izu` after the original external volume returned and the coordinated product rename; interrupted and prototype build artifacts are not current verification evidence.

## Format and ownership

The store is original Rust code and contains no VCS engine dependency. `izu-platform` wraps general-purpose Rustix filesystem functions; both izu crates forbid unsafe code. Rustix 1.1.5 uses platform FFI internally; the reviewed entrypoints are descriptor-relative open, link, rename, unlink, directory iteration, metadata, and explicit `fsync`, and Apple `fcntl_fullfsync`. Utility dependencies are SHA-256, system randomness, typed errors, and the shared izu model.

All numeric object framing is fixed-width and independent of Rust layout. Object files contain the model's 17-byte version-one header followed by exactly the declared payload. SHA-256 covers the header and payload. The frame magic is the eight bytes `IZUOBJ1\0`; the kind is included in the hash. Earlier unreleased `EZY-STORE`, `EZYHEAD1`, and `EZYOBJ1` prototype formats are explicitly rejected without resetting or implicitly migrating the store. Prototype evidence and binaries retain their historical identifiers. Unknown versions and kinds, truncated or extended files, unexpected kinds, excessive lengths, invalid canonical metadata, and hash mismatches are rejected.

```
FORMAT                   exactly ASCII "IZU-STORE 1\n"
objects/ab/<62 hex>       framed immutable object; full ID is 64 lowercase hex
tmp/izu-<32 hex>.tmp      exclusively created, randomly named staging file
head.lock                permanent inode; process-released publication lock
temporary.lock           permanent inode; process-released staging/recovery lock
HEAD                     72-byte version-one operation selector
```

HEAD is `ASCII "IZUHEAD1"` (8 bytes), operation object ID (32 raw bytes), and SHA-256 of the preceding 40 bytes (32 raw bytes). The selected object must be a valid canonical operation. Absence of HEAD is an empty store, distinct from a corrupt or unsupported HEAD. The checksum is corruption detection, not authentication.

Store directories use mode 0700 and files use mode 0600 at creation. Path components are opened separately through anchored directory descriptors with no-follow flags. Parent traversal and multi-component entry names are rejected. Symlinks are never followed by object, lock, HEAD, or directory operations. Callers may canonicalize a user-selected repository root before passing that explicit target; the store does not silently canonicalize a different target itself.

`Store::init_at(parent, name, options)` creates through a supplied directory descriptor. `Store::init(path, options)` opens the parent and delegates to that same implementation. `Directory::try_clone()` duplicates the descriptor without reopening a pathname, and `Store::root_directory()` exposes the pinned metadata directory for engine-owned locks and intents. These handles keep the original directory identity when its locator is renamed or replaced. They do not prove that an external locator still names that directory: final directory publication and source identity checks remain the caller's responsibility. Platform hard links use anchored entry names; callers must detect source-entry substitution before acknowledging an artifact.

## Immutable object acknowledgement

1. Bound the declared length and obtain the shared temporary-writer lease.
2. Exclusively create a staging file, write the frame, and stream the declared payload through a 64 KiB buffer and SHA-256. Reject early EOF and extra source bytes.
3. Persist the completed file through the platform barrier.
4. Create/open its hash shard and persist the shard's parent entry.
5. Revalidate the pinned native/shard entries and the staging name's original device/inode immediately before hard-linking it to the immutable object name. This is atomic creation and cannot replace an existing entry. Open the resulting name and require it to identify the persisted prepared inode. If it already exists, validate its complete frame, kind, length, hash, and exact bytes; then persist that existing file as well.
6. Retain the actual new or verified competing file descriptor while persisting the object shard directory, cleaning the staging entry, and persisting the temporary directory. Revalidate the native entries, shard, and final object name against those descriptors before returning its ID. Cleanup removes only a staging name that still identifies its original file; an unknown replacement is retained.

A failure may leave an unreferenced finalized object. It cannot overwrite a collision or publish an incomplete file as acknowledged data. Native metadata can be staged before all its references exist; publication verifies the complete typed graph before HEAD changes. All finalized objects are retained, including sources prepared outside a transaction and shared data. There is no destructive GC API in this version.

Blob import/export uses bounded streaming. Import validates an expected content ID before creating the final immutable name. Export fully verifies an object before writing caller output and hashes the second read as well. Output I/O or a concurrent out-of-contract file mutation may still leave partial caller output and returns an error. The destination's atomic publication belongs to its owner.

## Object batch acknowledgement

`Store::begin_object_batch(cancel)` creates an explicit provisional batch. `stage_blob(reader, length, cancel)` streams a blob; `stage(kind, payload, cancel)` stages an already bounded payload. Both return `StagedObjectId`, whose `object_id()` supplies a content address for encoding dependent metadata. That address acknowledges no persistence. Only consuming `finish(cancel)` can return `ObjectBatchReceipt { object_count: u64, payload_bytes: u64 }`. Counts and bytes cover every successfully staged input, including duplicates, rather than unique object names.

The source-capture caller stages its streamed blobs and final encoded tree in one batch, then finishes before returning the capture result. Its outer strict tree put and full-history HEAD publication remain in place. Restoration and existing strict put/import callers keep their original acknowledgement contracts. The batch does not replace HEAD or validate a published operation graph; transaction publication still performs the full typed graph verification.

1. Acquire the shared temporary lease for the batch's whole lifetime and pin the native directory identities. Before reading a source, creating its temporary entry, or growing bookkeeping, check the model's per-object framing limit, `max_scan_objects`, aggregate `max_scan_bytes`, and the prepared-record plus eventual published-identity allocation against `max_in_memory_bytes`. Checked arithmetic and fallible reservation bound growth. Keep only names, identities, kinds, IDs, and lengths between stages; payloads and per-object file descriptors are not retained.
2. Stream through the existing 64 KiB frame/hash path. Preserve the actual `ObjectTempCreated` and `ObjectDataWritten` hook semantics. Check the temporary name against its original inode, kernel-sync its file, check the name again, then close the descriptor. Any staging error poisons the batch; later staging or finishing returns `BatchAborted`.
3. At finish, reopen and fully verify each original prepared frame through its guarded name. Submit the temporary directory, revalidate the layout, and complete a strongest barrier on that directory. No new immutable object name precedes this first ordered barrier.
4. Create/open shards and link without replacement. Open every actual destination and verify its complete frame, kind, length, hash, and exact bytes against the prepared file, including an existing duplicate. Record its device/inode and its exact shard identity. Kernel-sync the actual destination even when a competing writer linked it without an acknowledgement. Remove only unchanged owned temporary names.
5. Submit each distinct shard once, plus the objects, temporary, and root directories. Revalidate the pinned native/shard layout and every recorded final object binding, complete a strongest root-directory barrier, and revalidate those bindings again before returning the batch receipt. The fixed 256-entry shard table and one-at-a-time reopen keep the number of active file descriptors independent of batch size.

Every participant must share the full-barrier anchor's filesystem device, and each persistence call retains volatile-filesystem classification. The first barrier completes staged data before linking; the second completes actual final-file submissions and changed namespace entries. Linux still receives an explicit `fsync` for every participating file and changed directory. There is no stronger-file acknowledgement on stage and no environment bypass or production fault switch.

Drop, cancellation, and finish errors yield no durable batch receipt and leave HEAD unchanged. They may retain unreferenced immutable objects, which recovery must preserve. Owned temporary cleanup runs before releasing the shared lease and retains substituted names. Even an empty batch refuses a durable receipt in `allow_volatile_for_tests` mode.

## Fresh source workspace directory submission

The engine batches directory persistence only for a fresh
fork into a pinned, actually empty source target. It does not derive this
permission from an empty before-tree. Restore and archive adoption retain their
strict staging/link/cleanup barriers and original-inode recovery protocol.

Before installing source, the engine checks the source-root locator,
rejects known volatile filesystems through the existing platform classifier,
and reserves identity bookkeeping with checked arithmetic and fallible
allocation under the configured store memory budget. Records borrow the
validated tree rather than retain payloads or one descriptor per tree entry.
Source and metadata-store devices need not be identical: each source participant
must match the source-root device whose barrier completes this batch.

New directories and final file/symlink names are exclusively created in the
provisional, unregistered target. Caller-selected targets may be visible during
preparation. Regular files keep the shared bounded copy, exact mode and
strongest data barrier. Each original file descriptor remains open through its
barrier and comparisons with the final named entry. Fresh paths never unlink
or clean named source, including on error or drop; zero-length or partial files,
unknown replacements and displaced originals remain available. No recovery path
is invented for this fresh materialization, and no internal hardlink aliases
need cleanup. Per-file atomic visibility changes only within this unregistered
target; product checks cannot launch until a completed workspace is registered.

After installation and directory modes, the engine checks all recorded
entries and ancestors, kernel-submits each distinct source directory and root,
rechecks the layout, completes one strongest source-root barrier, and checks
again. Device changes, observed substitution, cancellation or I/O errors cannot
acknowledge this batch or register the workspace; there is no automatic restart.
Marker persistence, full source readback and existing native operation
publication remain separate subsequent steps.

The classifier rejects known volatile filesystem types; it cannot prove
arbitrary filesystem/device durability. Successful persistence assumes the
filesystem and device honor the requested barriers. Device/inode records detect
observed differing identities, not inode reuse, replacement before initial
capture or arbitrary continuous external mutation. `Directory::create_dir`
uses separate mkdir/open operations; it supplies an anchored observed directory,
not an atomic creation identity witness. Traversal retains a constant number
of descriptors independent of path depth, while each current regular file
remains open through its strongest sync and final-name comparison.

The isolated change passed fresh macOS and Linux engine suites (64 default
tests and 74 fault tests per platform), strict Clippy, and all 15 CLI workflows.
The checks cover source bytes/modes/symlinks, nested Unicode/read-only source,
bookkeeping/nonempty refusal, ENOSPC/cancellation, partial-name retention,
late equal-byte substitutions and bounded SIGKILL/reopen. The device guard
uses fabricated identities rather than a real cross-mount experiment;
SIGKILL is not power-loss evidence. Integrated acceptance and the full
cooperative benchmark remain separate from this per-change proof.

## Transaction and HEAD acknowledgement

`Store::begin(HeadExpectation::{Any, Absent, At(id)}, cancel)` obtains the exclusive permanent HEAD lock, reads the latest operation, and checks the requested expectation. These states never conflate "any HEAD" with "HEAD must be absent." The engine derives changes from the locked latest view and validates its per-ref/workspace expectations. `tx.store().put(...)` takes only the shared temporary lease and does not reacquire the HEAD lock.

`Transaction::publish` consumes the transaction. The operation's parent must equal the locked current HEAD; this protects intervening operation history. Every typed reachable edge is traversed using the model's authoritative reference visitor. Every object's frame, length, hash, canonical metadata, and expected kind are verified, including previously acknowledged history. Missing/wrong-kind transitive references and corrupt old objects are rejected.

Publication batches the strongest barriers while preserving that complete verification:

1. Record the pinned store, objects, and temporary directory identities. Check that every participating file and directory has the same filesystem device as the store and reject an unlinked handle. Classify each descriptor at its persistence call; a known volatile filesystem cannot qualify.
2. Open each object through its original no-follow shard descriptor, verify it completely, and submit that file through explicit kernel `fsync`. Record the exact shard device/inode used for that open. Submit each distinct shard once, plus the objects parent directory. The fixed 256-entry identity table bounds bookkeeping and does not retain 256 open descriptors.
3. Write the prepared HEAD, check its device, and submit its bytes through kernel `fsync`. Revalidate pinned native entries and every recorded shard identity. Issue one strongest barrier on the prepared HEAD before the atomic rename. On macOS this asks the device to complete all preceding closure, directory, and selector submissions.
4. Check cancellation and revalidate the recorded directories, the exact prepared selector bytes, and the temporary name's original identity immediately before atomically replacing HEAD.
5. Once HEAD is visible, require the published name to identify the prepared selector inode and exact bytes, then kernel-sync the destination store directory and source temporary directory. Revalidate after that boundary, issue one strongest barrier on the store directory, and revalidate the layout and actual HEAD binding again before returning a durable receipt. All failures after the rename return the proposed operation's receipt with uncertainty, including a later substitution with another valid selector.

No previously published closure is trusted to skip hashing or submission. The same-device requirement returns `UnsupportedDurability` rather than assuming that a full flush on one filesystem covers another. Recorded shard checks detect a directory substituted at the tested boundaries; arbitrary external mutation of native metadata after the final check is outside the cooperative store contract.

The API has three outcomes:

| Outcome | Meaning |
| --- | --- |
| `Err(StoreError)` | HEAD was not changed by this call. Prepared objects may remain. |
| `Durable(PublishReceipt)` | The persistence protocol completed for the visible operation. |
| `VisibleButUncertain { receipt, error }` | HEAD was replaced, but the call did not acknowledge the completed persistence protocol. The receipt retains the proposed operation and prior HEAD even if a subsequent external replacement now hides that selector. This is never described as rollback. |

Cancellation is checked before publication, at chunk boundaries, during graph traversal, and while waiting for locks. After the atomic HEAD rename, cancellation does not interrupt the required barriers or erase the receipt. Lock waits are bounded by `lock_timeout` (default 10 seconds, maximum 60) with 5 ms polling. Blocking source/sink I/O and filesystem persistence syscalls require an interruptible caller/OS; a token cannot forcibly interrupt an arbitrary `Read` or kernel flush.

HEAD lock requests register through the private `head-admission-v1/` directory before acquiring the unchanged native `head.lock`. A short permanent `gate.lock` orders registration; up to 1,024 permanent reusable claimant files contain fixed, checksummed slot/sequence records. Fresh descriptor leases identify live claims, including the current HEAD holder. Each request waits for all older captured claims, so a later publisher cannot repeatedly overtake a registered waiter. Claimant ownership lasts until native HEAD ownership ends. Cancellation and the original single lock deadline cover registration, predecessor waiting and native acquisition; no phase starts a new timeout. Process death releases kernel leases, and an interrupted record is reused only after an independent exclusive probe proves it is unheld. Unknown coordination entries are retained and refused. The gate is never held while waiting for HEAD, temporary leases, user hooks or persistence barriers. This state is reconstructable coordination, not native history or a durable publication receipt.

New stores create the directory, blank gate and one zero-filled 56-byte claimant before creating FORMAT. Both files and the coordination directory are submitted to the kernel before init's existing strongest device barriers; the durable init acknowledgement includes this preparation without adding another full flush. A free prepared claimant is overwritten with a complete checked ticket under its lease. Reusing an exact-size claimant skips redundant resizing while retaining exact record and EOF readback. Existing stores still initialize missing coordination lazily without a completion flag, so creator death before gate creation does not strand the store. Opening a store with an absent HEAD may initialize this coordination while confirming absence. The original acquisition deadline also covers this lazy setup. Store format remains 1: old and new binaries still serialize on the same native HEAD lock, but ordered admission requires cooperating upgraded participants. Existing publication and recovery barriers are unchanged.

A successful HEAD read validates its pinned selector without acquiring the publication lock. A missing selector is confirmed with one read under shared `head.lock`, since a concurrent atomic replacement reproduced `ENOENT` on the Linux host-backed filesystem. Corruption and other read errors propagate directly. Absence confirmation uses the existing cancellation and lock timeout rules. A transaction reads under its existing exclusive head lock; recovery reads under its existing exclusive temporary lease, which also excludes selector replacement. These guarded reads do not acquire another head lock or reverse publication's lock order.

`bootstrap_archive` is a separate cold-restore boundary. It requires HEAD to be absent under the same exclusive lock, verifies/persists the complete archived operation chain, and never replaces an active store. Only this path may seed an archived operation with an existing parent. Normal publication keeps the parent invariant. The engine owns remapping historical workspace locations, source materialization, and the final staging-directory publication; seeding HEAD alone does not produce a usable restored repository.

Initialization persists its parent directory before success. Opening also persists that parent entry, covering interrupted initialization which left a complete layout before the last barrier. Existing stores are never reinitialized and lock files are never replaced or removed by store APIs. Platform `open_lock` first opens an existing regular entry; only an absent entry triggers exclusive creation. A competing creator's `AlreadyExists` permits one existing-entry reopen. Other failures are returned without broad retries. This replaces concurrent nonexclusive `O_CREAT`, which reproduced `ENOENT` on the external APFS volume despite a linked parent and a resulting lock entry. The two-writer regression checks 128 fresh names for one permanent inode and also covers preserved data, symlink/directory rejection, and an unlinked parent.

## Recovery and limits

`verify` validates every finalized object and then the complete current HEAD closure. `verify_reachable` checks an explicit operation root. A hash-valid uncommitted metadata object may have unresolved references; references from the selected HEAD may not. Scans are bounded by object and logical-payload-byte budgets, checked before streaming an object. Graph queues/maps and in-memory object reads reserve capacity fallibly.

`recover` obtains the exclusive temporary lease. This excludes active blob writers and HEAD publication without reversing transaction lock order. It verifies storage, reissues persistence barriers for the stable HEAD closure and HEAD, then deletes only recognized inactive temporary names. Unknown temporary entries are retained and counted. Immutable objects remain retained. Recovery retains the submitted closure's directory identities through temporary cleanup and revalidates them before and after the strongest persistence barriers and before its final receipt. A detached or substituted shard is refused even if its original descriptor was synchronized. Corruption is returned explicitly; recovery never fabricates missing data or silently selects an earlier operation.

Defaults are: 64 MiB in-memory object limit, 1,000,000 scanned/reachable objects, 64 GiB aggregate logical payload per scan/closure, and the model's independent limits. These limits are configurable and checked. The aggregate limit includes retained history reachable through operation parents; it is a deliberate first-version repository-history limit, not just a per-file limit. Old blobs are included. No unchecked payload allocation or arithmetic is needed for streaming.

Every publication still reads and validates the full reachable history under the publication lock, then performs one kernel submission per object and one per distinct shard. Its cost remains proportional to retained reachable bytes and metadata. Batching removes repeated macOS hardware-flush requests; it does not make full-history work constant or claim general performance superiority. Strict object `put` acknowledgements retain their full persistence protocol. Source capture now uses the separately acknowledged object batch above; repeated strict puts elsewhere may still be costly. An incremental durability frontier remains a separate design.

## Platform contract and evidence

On macOS, strict file/directory persistence calls safe Rustix `fcntl(F_FULLFSYNC)` exactly once. Apple's [fcntl manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fcntl.2.html) specifies that it includes file synchronization and then asks the device to flush buffered data. Apple's [fsync manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html) explains why a kernel-only flush can leave device caches pending. Rust 1.99's [pinned standard-library source](https://github.com/rust-lang/rust/blob/b940084d7eb6a299eb4bfeb8e34901bc051e7ac4/library/std/src/sys/fs/unix.rs#L1180) already maps both `File::sync_all` and `sync_data` to `F_FULLFSYNC` on Apple. The earlier implementation inadvertently combined that strongest call with another full flush. The strict path now issues one; the batched path deliberately uses explicit Rustix `fsync` for kernel submissions and the ordered full barriers described above. Successful requests assume that the filesystem and device honor them; Apple also notes that devices can ignore the strongest request.

Linux strict persistence and kernel submission both use explicit Rustix `fsync` on files and containing directories. A Linux full barrier does not replace submitting every participating file and changed directory. The macOS run does not prove the new optimizer's Linux execution; the integrator owns its independent Linux suite. Windows durable native storage remains unsupported and is rejected before initialization. Microsoft documents [directory handle support](https://learn.microsoft.com/en-us/windows/win32/fileio/obtaining-a-handle-to-a-directory) and [file flushing](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers), but those pages alone do not establish this store's directory-entry persistence and atomic publication requirements. Other platforms are also rejected. A successful barrier cannot validate untrusted, defective, or network storage.

Successful `fsync` on a known volatile filesystem is insufficient. Before initialization creates an entry, and at every file/directory persistence barrier, the store classifies the actual descriptor using safe Rustix `fstatfs`. Linux tmpfs, ramfs, and hugetlbfs are rejected by their [kernel filesystem magic values](https://github.com/torvalds/linux/blob/master/include/uapi/linux/magic.h); macOS types named tmpfs, ramfs, mfs, or devfs are rejected. The error is `UnsupportedDurability`, with no fallback to weaker acknowledgement. Linux documents [ramfs as having no backing store](https://www.kernel.org/doc/html/latest/filesystems/ramfs-rootfs-initramfs.html). This classification rejects known volatile types; it cannot discover every volatile block device beneath an otherwise ordinary filesystem or establish hardware reliability.

Reproducible checks from the desired checkout, using the internal project toolchain and disposable storage:

```sh
scripts/izu-env cargo test -p izu-store -p izu-platform --locked --offline --features izu-store/fault-injection
scripts/izu-env cargo clippy -p izu-store -p izu-platform --all-targets --locked --offline --features izu-store/fault-injection -- -D warnings
scripts/izu-env cargo build -p izu-store --release
# This deliberately fails: a release build must reject the development feature.
scripts/izu-env cargo check -p izu-store --release --features fault-injection
```

Coverage includes immutable collisions, corruption/truncation/version/kind/length rejection, strict streamed output, imports with wrong expected IDs, transitive closure rejection, cold archive bootstrap, symlinks and descriptor anchoring, renamed/replaced staging initialization, lock bounds, cancellation, temporary-writer retention, concurrent subprocess history/ref updates, and SIGKILL at all 13 durable boundaries, full-history submission with deduplicated shard directories, previously acknowledged history corruption, and late shard substitution before and after HEAD visibility. Injected ENOSPC failures cover each object and HEAD boundary and distinguish visible publication accurately. Linux-only policy tests use `/dev/shm` when available to prove that successful synchronization does not bypass volatile-filesystem rejection, normal initialization leaves no new store entry, and normal opening rejects a test fixture. SIGKILL does not simulate power loss. Injected ENOSPC does not substitute for the integrator's actual filesystem-full run. A bounded tmpfs ENOSPC experiment covers actual kernel error handling, not persistent-media disk-full or power-loss behavior.

The 2026-10-03 IZU run passed 5 platform tests, 1 store unit test, 16 storage tests, and 9 failure tests with a fresh `target-izu` and `TMPDIR` on Neural. The 13 process-kill points and eight publication failure/cancellation points run inside those tests. Clippy passed with warnings denied; the normal release library built; release with `fault-injection` failed at the intentional compile-time restriction. The cross-device unit changes the expected device scalar and proves guard logic; it is not a real cross-mount experiment. The late-shard regression first reproduced a false durable receipt with only the final shard revalidation omitted, then passed at four boundaries with the guard restored. Its red log and source hashes remain in `.artifacts/batch/shard-guard-red.{log,json}`. Independent integrated Linux, actual ENOSPC, controller concurrency, and CLI performance results belong to the integrator's evidence.

The integrator independently reran the same 31 store/platform tests successfully from the original external-volume checkout; its raw log is `.artifacts/evidence/izu-store-batch-tests.log`. Its model source matches the final model-owner pin, while the local checks and metadata driver used the earlier renamed dependency pin. Those distinct pins are recorded in `.artifacts/batch/model-pins.json`; they are not silently treated as one binary or dependency snapshot. No additional local refresh or measurement was launched after the integrator froze source for the next review.

The 2026-10-04 source-capture lane refreshed the reviewed root inputs after preserving its prior snapshot, including the CORE05 recovery closure guard. The focused gate now passes 50 tests: 5 platform, 1 store unit, 16 storage, 10 existing failure, 14 batch, and 4 adjacent strict/HEAD substitution tests. Clippy passes for both crates and all targets with warnings denied. Raw logs are `.artifacts/capture-performance/final-store-platform-tests.log` and `final-store-platform-clippy.log`. Batch coverage includes exact mixed objects and duplicates, cross-chunk maximum-length streaming, reader/length errors and poisoned finish, count/byte/bookkeeping checks before reads, descriptor bounds, whole-lifetime recovery exclusion, volatile-mode receipt refusal, independent strict/batch writers with preserved operation history, substituted temporary/native/shard/final names, injected ENOSPC and cancellation at ten boundaries, and subprocess kills followed by reopen/recovery.

The four adjacent tests first reproduced false strict IDs or durable HEAD receipts at `ObjectFileSynced`, `HeadFileSynced`, `ObjectDirectorySynced`, and `HeadDirectorySynced`. The RED log, exact binaries, sources, and retained originals remain in `.artifacts/capture-performance/adjacent-red.log` and `adjacent-red-bindings/`. The guard correction then passed all four; permanent tests hold their owned `TempDir` through the assertions and remove it on return. Early selector failure preserves the old HEAD; late selector failure requires `VisibleButUncertain` with the exact proposed and prior IDs. The source-ready002 freeze separates compiling corrected inputs from the final tested/documented handoff. CORE05 source and its permanent failure test remain unchanged. The focused lane still uses lock SHA `84199c4e17e7cfb81e825f05e2879bb744944cef56e58257c8f1264ae530d5d3`; the later integrated runtime-to-store development edge changes root lock SHA to `4f4c3787fb5274be5d6080e9ad9fafbe1d8da452f5919c5411357b2d808a48f6` with the same 229 package identities, including 216 external packages. Final integrated execution uses the root's coherent inputs.

Fault hooks are available only through the explicitly enabled development `fault-injection` feature; enabling it in an ordinary release build is a compile error. Production builds do not read a fault environment variable. The `store_lab` example requires that feature and supports `init`, `append [--hold-at Boundary|--enospc-at Boundary]`, streamed `blob`, `verify`, `recover`, and metadata-only `benchmark` against owned disposable fixtures.

Batch hooks report their actual persistence semantics: `BatchObjectKernelSynced` follows a staged file's kernel submission; `BatchTemporaryKernelSynced` follows the pre-link temporary-directory submission; `BatchDataSynced` follows the first strongest barrier; `BatchObjectLinked` follows resolution/verification of an actual destination; `BatchFinalObjectKernelSynced` follows that destination's kernel submission; `BatchDirectoriesKernelSynced` follows all final directory submissions; and `BatchDirectorySynced` follows the second strongest barrier before final binding checks. A kernel sync is never relabeled as the strict `ObjectFileSynced` strongest-file hook.

The feature also exposes `StoreOptions::allow_volatile_for_tests`, false by default. This option bypasses only the known-volatile classification to permit an owned kernel ENOSPC fixture; persistence syscalls still execute. When enabled, every successful visible HEAD publication returns `VisibleButUncertain { receipt, error: StoreError::VolatileTestMode }`, even on a persistent filesystem. It never returns `Durable`. Object IDs and successful initialization in this explicit fixture mode are not persistent-storage acknowledgements. The driver accepts a leading `--allow-volatile-for-tests` and prints `VOLATILE_TEST_VISIBLE`, `VOLATILE_TEST_BLOB`, or `INITIALIZED_VOLATILE_TEST`; it refuses persistence benchmarks in that mode. A host-independent test checks that this option cannot accidentally produce a durable publication receipt.

For a deterministic process-crash check, `store_lab append PATH LABEL --hold-at HeadFileSynced` prints and flushes the boundary, then blocks before HEAD replacement; killing it must preserve the previous HEAD. At `HeadReplaced`, the replacement is already visible but no durable receipt was returned; recovery may observe that operation and must retain the earlier acknowledged operation through its parent chain. No production CLI fault switch is installed by these tests.

## Measured publication cost

The external-Neural baseline is preserved in `.artifacts/store-baseline-neural-20261003.json` and the unmodified prototype development driver `.artifacts/bin/store-lab-before-batch-885d15bdcd30`. Its SHA-256 is `885d15bdcd30ac8f8de296b5bc6681abf1f8a8a1f7bf198cdd2fb58640055d24`. Fixtures were on the actual Neural APFS volume, not the earlier internal-volume fixtures. A 100-operation metadata history spent 2,050/2,113/2,414 ms in each measured publication while separate verification took 20/21/20 ms. At 1,000 operations, publication took 41,206/21,457/20,829 ms while verification took 135/126/131 ms. Lock acquisition was below 0.3 ms; strict staging of each new operation took 70–335 ms in the 1,000-operation case. That separation motivated changing repeated strongest barriers while retaining verification.

The workload prepares a metadata-only immutable operation chain, seeds it through the archive boundary, then measures three actual transactions. It is not source capture, a blob-heavy repository, an engine checkpoint, a release CLI benchmark, or a comparison with another VCS. Historical prototype framing is retained inside the frozen binary; new IZU fixtures use the renamed native magic and are not mixed with those old stores. Host activity can vary substantially. Broader performance claims require the integrator's exact binary and workload.

The fresh 100-operation old/new comparison used the two SHA-pinned development drivers on Neural. Old publication samples were 1,836/1,977/2,152 ms; new samples were 58.1/59.4/77.5 ms, a 33.3× ratio of the sample medians. Both retained 103 reachable objects and 21,143 payload bytes. End-to-end transaction samples were 1,953/2,054/2,265 ms before and 135/113/147 ms after. Strict preparation of the whole chain took 13.51 seconds before and 7.75 seconds after; it remains a separate cost.

The new 1,000-operation run measured publication at 163.5/168.5/156.3 ms and end-to-end transaction time at 192/204/194 ms. Separate full verification still took 121/115/114 ms. Its 1,003 objects and 207,443 payload bytes match the preserved earlier baseline. That 1,000-operation before/after comparison uses historical baseline samples, not a fresh paired old run: repeating the old run alone previously took 250 seconds and would have exceeded the integrator's roughly four-minute coordinated slot. New strict chain preparation took 64.18 seconds.

The new driver SHA-256 is `af842b49d53cb8074dd46c4cbd91c8b839245952d2faba3ae9431f6f79268cbc`, built from the stable store/platform snapshot and the earlier renamed model dependency pin recorded in `.artifacts/batch/metadata-comparison.json`. The integrator later reported an engine test run overlapping the intended quiet slot; possible fixture I/O is recorded and raw samples are retained. These three-sample development measurements demonstrate this workload's barrier cost change; they do not establish a statistical bound, fully isolated timing, the unchanged 30-controller gate, or the release CLI's source-capture performance.

## Native source-capture diagnostic

The integrator separately ran a controlled paired native CLI diagnostic on 512 files of 4,096 bytes, seed 1729, using the same script, generated fixture, cache labels, and unchanged 30-second command deadline. All three lanes were idle during the after run. Before and after binaries stayed unchanged during their runs, every attempted native command succeeded, and source bytes were preserved. Warm startup samples were milliseconds; repeated strict blob persistence was the actionable capture bottleneck. The after run integrates object-batch source-ready001 with the engine capture callback, before the adjacent namespace correction above.

| Native command | Before seconds | After seconds |
| --- | ---: | ---: |
| First full capture | 21.081863 | 1.540570 |
| No-op capture, median of three | 15.131799 | 1.262494 |
| One changed file | 14.582193 | 1.278445 |
| Diff | 16.035405 | 1.424741 |

The frozen before debug CLI SHA-256 is `efea313cd4112881ce78023560ca6653b755659170f7dd396a242df24e6f5afb`; the after debug CLI is `62d551987110b8142da178bfa5753f1fdef0d795165694c46d7311f4fc124f5d`. Root receipts, exact frozen source/build inputs, and raw logs are under `.artifacts/capture-profile/{before,after}/`. The report SHAs are respectively `3c48846a3cac4aa3ac1ec853883e724690dc8a7a928e276a9497f74802835da4` and `12fe0ac3aa61ca16d954fc452552060737aeab1d9dc1d5be5f901b208d7b6e24`; the lane's readonly extraction is `.artifacts/capture-performance/paired-cli-parent-readback.json`. OS cache state was uncontrolled, and these are a few debug native samples. They are diagnostic evidence, not final namespace-corrected release timing, all-interface acceptance, statistical bounds, or the 30-controller gate. Those checks remain the integrator's responsibility.
