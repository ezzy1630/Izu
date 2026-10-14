# Filesystem platform boundary

`FileIdentity::of(&File)` compares live handles without resolving a path again.
Unix uses device/inode. Windows uses `GetFileInformationByHandleEx(FileIdInfo)`:
volume serial plus the complete 128-bit file identifier. It preserves an API
failure, including unsupported filesystems, instead of guessing from timestamps
or file lengths. This in-memory key has no serialized representation and does
not establish user ownership, path safety, or durable persistence. Keep the
object handles alive while comparing identities; identifiers may be reused
after deletion.

The Windows implementation has two unsafe expressions in one private module:
the handle query and reading its output after success. The borrowed `File` owns
the handle throughout the call. `MaybeUninit<FILE_ID_INFO>` provides aligned
storage; the checked API byte count is exactly its size. The information class
selects that output structure. A zero return preserves `last_os_error` before
reading the output. No pointer or handle is retained, closed, or transferred.
The dependency is general-purpose `windows-sys` 0.61.2, limited to Foundation
and Storage/FileSystem bindings. The rest of the crate denies unsafe code.

API references checked against Rust 1.99 and the Microsoft API documentation:

- [Rust Windows MetadataExt](https://doc.rust-lang.org/std/os/windows/fs/trait.MetadataExt.html):
  the stable metadata API does not expose volume/file IDs; those methods require
  the experimental `windows_by_handle` feature.
- [GetFileInformationByHandleEx](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getfileinformationbyhandleex)
  specifies the output buffer/class contract and failure handling.
- [FILE_ID_INFO](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_id_info)
  describes comparing volume serial and the full file identifier.

## Native acceptance still required

Run `cargo test --locked -p izu-platform --test identity` on a native Windows
x86-64 host using the pinned Rust toolchain. The tests require a writable local
filesystem with hard-link support and exercise cloned handles, hard links,
equal-content distinct files, and rename plus replacement of the original path.
Record OS/architecture, filesystem, exact source tree, compiler, command and
binary hashes. Cross compilation cannot establish runtime correctness.

The identity addition does **not** implement Windows `Directory` operations:
they continue to return `Unsupported`. Full Windows repository acceptance must
first implement anchored handle-relative traversal, reject reparse points,
validate Windows components (including alternate streams and reserved names),
perform atomic no-replacement publication, and establish a reviewed persistence
barrier for objects, state and directory entries. Test namespace replacement,
junction/reparse races, concurrent writers, corruption, disk-full and crashes
at each publication point with fresh fixtures. An error after visible state
replacement must retain the existing uncertain-acknowledgement contract.

Windows subprocess support is also incomplete. The Unix process-group runner
cannot establish Windows descendant ownership. The Windows implementation must
assign a suspended worker to its owned Job Object before starting any target,
prevent unobserved startup escape, handle parent disconnect, and bound pipe I/O
and termination. Job termination alone must not substitute for observed writer
quiescence. Test large concurrent I/O, background writers, timeout/cancellation,
parent loss, output caps, and failed job assignment on the native host. See
[Microsoft Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects).

Run actual source capture, history mutation/reopen, corruption/crash recovery,
Git exchange and process-backed validation through the exact freshly built CLI
on each requested native OS/architecture after these platform gates pass.
