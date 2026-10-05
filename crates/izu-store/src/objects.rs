use crate::durability::{Identity, check_directory_entry, check_file_entry};
use crate::{
    CHUNK_BYTES, DurableBoundary, Result, Store, StoreError, durable_dir, durable_file, io_error,
};
use izu_model::{
    CancellationToken, FRAME_HEADER_LEN, ObjectHasher, ObjectId, ObjectKind, frame_header,
    parse_frame_header,
};
use izu_platform::Directory;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub(crate) struct TemporaryFile<'a> {
    entry: TemporaryEntry<'a>,
    pub(crate) file: File,
}

pub(crate) struct TemporaryEntry<'a> {
    directory: &'a Directory,
    name: [u8; 40],
    identity: Identity,
}

impl<'a> TemporaryFile<'a> {
    pub(crate) fn new(directory: &'a Directory) -> Result<Self> {
        for _ in 0..8 {
            let mut random = [0_u8; 16];
            getrandom::fill(&mut random).map_err(|_| StoreError::Corrupt {
                kind: "random source",
                reason: "cannot generate temporary name",
            })?;
            let mut name = [0_u8; 40];
            name[..4].copy_from_slice(b"izu-");
            name[36..].copy_from_slice(b".tmp");
            encode_hex(&random, &mut name[4..36]);
            let text = std::str::from_utf8(&name)
                .map_err(|_| StoreError::InvalidPath("temporary name is not ASCII"))?;
            match directory.create_new_file(OsStr::new(text)) {
                Ok(file) => {
                    let identity = Identity::file(&file)?;
                    return Ok(Self {
                        entry: TemporaryEntry {
                            directory,
                            name,
                            identity,
                        },
                        file,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_error("create temporary object", error)),
            }
        }
        Err(StoreError::Corrupt {
            kind: "temporary name",
            reason: "repeated random-name collisions",
        })
    }

    pub(crate) fn name(&self) -> Result<&OsStr> {
        self.entry.name()
    }

    pub(crate) fn check_entry(&self) -> Result<()> {
        self.entry.check()
    }

    pub(crate) fn into_entry(self) -> TemporaryEntry<'a> {
        self.entry
    }
}

impl TemporaryEntry<'_> {
    pub(crate) fn name(&self) -> Result<&OsStr> {
        let name = std::str::from_utf8(&self.name)
            .map_err(|_| StoreError::InvalidPath("temporary name is not ASCII"))?;
        Ok(OsStr::new(name))
    }

    pub(crate) fn check(&self) -> Result<()> {
        let metadata = self
            .directory
            .metadata(self.name()?)
            .map_err(|error| io_error("check temporary entry", error))?;
        if !metadata.is_file() || Identity::metadata(&metadata)? != self.identity {
            return Err(StoreError::Corrupt {
                kind: "temporary entry",
                reason: "entry no longer names the prepared file",
            });
        }
        Ok(())
    }

    pub(crate) fn open(&self) -> Result<File> {
        self.check()?;
        let file = self
            .directory
            .open_read(self.name()?)
            .map_err(|error| io_error("reopen prepared file", error))?;
        if Identity::file(&file)? != self.identity {
            return Err(StoreError::Corrupt {
                kind: "temporary entry",
                reason: "entry changed while reopening prepared file",
            });
        }
        Ok(file)
    }

    pub(crate) fn remove(&self) -> Result<()> {
        self.check()?;
        self.directory
            .remove_file(self.name()?)
            .map_err(|error| io_error("remove prepared temporary entry", error))
    }
}

impl Drop for TemporaryEntry<'_> {
    fn drop(&mut self) {
        // Cleanup failure does not rewrite a publication outcome. Recovery will
        // remove remaining temporary entries while holding the exclusive lease.
        // A random owned name may have been substituted. Unknown replacements
        // are retained, including on an earlier streaming or staging error.
        let _ = self.remove();
    }
}

pub(crate) fn encode_hex(source: &[u8], output: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (pair, byte) in output.as_chunks_mut::<2>().0.iter_mut().zip(source) {
        pair[0] = HEX[usize::from(byte >> 4)];
        pair[1] = HEX[usize::from(byte & 15)];
    }
}

pub(crate) fn object_name(id: ObjectId) -> [u8; 64] {
    let mut name = [0_u8; 64];
    encode_hex(id.as_bytes(), &mut name);
    name
}

pub(crate) fn object_parts(name: &[u8; 64]) -> Result<(&OsStr, &OsStr)> {
    let text = std::str::from_utf8(name)
        .map_err(|_| StoreError::InvalidPath("object name is not ASCII"))?;
    Ok((OsStr::new(&text[..2]), OsStr::new(&text[2..])))
}

pub(crate) struct ObjectHeader {
    pub(crate) kind: ObjectKind,
    pub(crate) payload_len: u64,
}

pub(crate) struct StreamFrame {
    header: [u8; FRAME_HEADER_LEN],
    hasher: ObjectHasher,
    length: u64,
}

impl StreamFrame {
    pub(crate) fn new(kind: ObjectKind, length: u64, store: &Store) -> Result<Self> {
        Ok(Self {
            header: frame_header(kind, length, &store.options.limits)?,
            hasher: ObjectHasher::new(kind, length, &store.options.limits)?,
            length,
        })
    }
}

impl Store {
    pub fn put(
        &self,
        kind: ObjectKind,
        payload: &[u8],
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        let length =
            u64::try_from(payload.len()).map_err(|_| StoreError::LimitExceeded("object length"))?;
        self.put_stream(kind, &mut io::Cursor::new(payload), length, cancel)
    }

    /// Reads exactly the declared length in fixed-size buffers. Extra bytes and
    /// early EOF are both rejected; no complete object is published on failure.
    pub fn put_blob<R: Read>(
        &self,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        self.put_stream(ObjectKind::Blob, reader, length, cancel)
    }

    pub fn put_stream<R: Read>(
        &self,
        kind: ObjectKind,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        self.put_stream_checked(kind, None, reader, length, cancel)
    }

    pub fn import_object<R: Read>(
        &self,
        kind: ObjectKind,
        expected_id: ObjectId,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        self.put_stream_checked(kind, Some(expected_id), reader, length, cancel)
    }

    fn put_stream_checked<R: Read>(
        &self,
        kind: ObjectKind,
        expected_id: Option<ObjectId>,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        cancel.check()?;
        let frame = StreamFrame::new(kind, length, self)?;
        // All temporary writers hold a shared lease. Recovery takes the exclusive
        // lease, so it cannot delete uncommitted in-flight data.
        let _lease = self.acquire_lock("temporary.lock", true, cancel)?;
        let (id, mut temp) = self.prepare_object(reader, frame, cancel)?;
        if expected_id.is_some_and(|expected| expected != id) {
            return Err(StoreError::Model(izu_model::ModelError::HashMismatch));
        }
        cancel.check()?;
        durable_file(&temp.file, &self.options)?;
        self.boundary(DurableBoundary::ObjectFileSynced)?;
        let name = object_name(id);
        let (prefix, suffix) = object_parts(&name)?;
        let shard = self
            .objects
            .ensure_dir(prefix)
            .map_err(|error| io_error("create object shard", error))?;
        // A shard entry must itself be persistent before any referencing HEAD.
        durable_dir(&self.objects, &self.options)?;
        cancel.check()?;
        self.check_strict_layout(&shard, prefix)?;
        temp.check_entry()?;
        let published = match self.temporary.hard_link(temp.name()?, &shard, suffix) {
            Ok(()) => {
                let file = shard
                    .open_read(suffix)
                    .map_err(|error| io_error("open published object", error))?;
                // The source name can change even after its pre-link check.
                // Only the inode whose completed bytes were persisted qualifies.
                if Identity::file(&file)? != Identity::file(&temp.file)? {
                    return Err(StoreError::Corrupt {
                        kind: "object entry",
                        reason: "published entry does not name the prepared file",
                    });
                }
                file
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let mut existing = shard
                    .open_read(suffix)
                    .map_err(|error| io_error("open existing object", error))?;
                self.verify_duplicate(&mut existing, &mut temp.file, id, kind, length, cancel)?;
                // A competing writer may have linked the object but not yet
                // synced it. This writer takes responsibility before its ACK.
                durable_file(&existing, &self.options)?;
                existing
            }
            Err(error) => return Err(io_error("publish immutable object", error)),
        };
        self.boundary(DurableBoundary::ObjectLinked)?;
        self.check_strict_layout(&shard, prefix)?;
        check_file_entry(&shard, suffix, &published)?;
        durable_dir(&shard, &self.options)?;
        self.boundary(DurableBoundary::ObjectDirectorySynced)?;
        drop(temp);
        durable_dir(&self.temporary, &self.options)?;
        // Retain the actual new or verified competing inode through cleanup and
        // the last barrier. A detached descriptor cannot prove the named object.
        self.check_strict_layout(&shard, prefix)?;
        check_file_entry(&shard, suffix, &published)?;
        Ok(id)
    }

    fn check_strict_layout(&self, shard: &Directory, prefix: &OsStr) -> Result<()> {
        Identity::file(self.root.file())?;
        check_directory_entry(&self.root, OsStr::new("objects"), &self.objects)?;
        check_directory_entry(&self.root, OsStr::new("tmp"), &self.temporary)?;
        check_directory_entry(&self.objects, prefix, shard)
    }

    pub(crate) fn verify_duplicate(
        &self,
        existing: &mut File,
        prepared: &mut File,
        id: ObjectId,
        kind: ObjectKind,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let actual = self.verify_file(existing, id, Some(kind), cancel)?;
        if actual.payload_len != length {
            return Err(StoreError::Corrupt {
                kind: "object",
                reason: "identifier collision",
            });
        }
        compare_files(existing, prepared, cancel)
    }

    // Both callers hold the temporary lease. Streaming does not itself ACK data.
    pub(crate) fn prepare_object<R: Read>(
        &self,
        reader: &mut R,
        frame: StreamFrame,
        cancel: &CancellationToken,
    ) -> Result<(ObjectId, TemporaryFile<'_>)> {
        cancel.check()?;
        let StreamFrame {
            header,
            mut hasher,
            length,
        } = frame;
        let mut temp = TemporaryFile::new(&self.temporary)?;
        self.boundary(DurableBoundary::ObjectTempCreated)?;
        temp.file
            .write_all(&header)
            .map_err(|error| io_error("write object header", error))?;
        let mut remaining = length;
        let mut buffer = [0_u8; CHUNK_BYTES];
        while remaining != 0 {
            cancel.check()?;
            let count = usize::try_from(remaining.min(CHUNK_BYTES as u64))
                .map_err(|_| StoreError::LimitExceeded("chunk length"))?;
            let n = match reader.read(&mut buffer[..count]) {
                Ok(n) => n,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error("read blob source", error)),
            };
            if n == 0 {
                return Err(StoreError::InputLengthMismatch);
            }
            temp.file
                .write_all(&buffer[..n])
                .map_err(|error| io_error("write object payload", error))?;
            hasher.update(&buffer[..n])?;
            remaining = remaining
                .checked_sub(n as u64)
                .ok_or(StoreError::InputLengthMismatch)?;
        }
        cancel.check()?;
        let mut extra = [0_u8; 1];
        loop {
            match reader.read(&mut extra) {
                Ok(0) => break,
                Ok(_) => return Err(StoreError::InputLengthMismatch),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    cancel.check()?;
                }
                Err(error) => return Err(io_error("check blob length", error)),
            }
        }
        self.boundary(DurableBoundary::ObjectDataWritten)?;
        let id = hasher.finish()?;
        Ok((id, temp))
    }

    pub fn get(
        &self,
        id: ObjectId,
        expected: ObjectKind,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        cancel.check()?;
        let mut file = self.open_object(id)?;
        let header = self.read_header(&mut file, id, Some(expected))?;
        self.read_payload(&mut file, id, header, cancel)
    }

    /// Reads from the descriptor positioned by `read_header`, retaining the
    /// verified payload so closure traversal can decode and submit the same file.
    pub(crate) fn read_payload(
        &self,
        file: &mut File,
        id: ObjectId,
        header: ObjectHeader,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        cancel.check()?;
        if header.payload_len > self.options.max_in_memory_bytes {
            return Err(StoreError::LimitExceeded("in-memory object"));
        }
        let len = usize::try_from(header.payload_len)
            .map_err(|_| StoreError::LimitExceeded("in-memory object"))?;
        let mut payload = Vec::new();
        payload
            .try_reserve_exact(len)
            .map_err(|_| StoreError::Allocation)?;
        payload.resize(len, 0);
        let mut position = 0_usize;
        let mut hasher = ObjectHasher::new(header.kind, header.payload_len, &self.options.limits)?;
        while position < len {
            cancel.check()?;
            let end = position.checked_add(CHUNK_BYTES).unwrap_or(len).min(len);
            let n = file
                .read(&mut payload[position..end])
                .map_err(|error| io_error("read object payload", error))?;
            if n == 0 {
                return Err(StoreError::Corrupt {
                    kind: "object",
                    reason: "truncated payload",
                });
            }
            hasher.update(&payload[position..position + n])?;
            position = position
                .checked_add(n)
                .ok_or(StoreError::LimitExceeded("object position"))?;
        }
        check_end(file)?;
        if hasher.finish()? != id {
            return Err(StoreError::Corrupt {
                kind: "object",
                reason: "hash mismatch",
            });
        }
        Ok(payload)
    }

    /// Fully verifies the object before writing caller output, with bounded memory.
    /// A caller-output failure may leave partial output; HEAD is never mutated.
    pub fn read_blob<W: Write>(
        &self,
        id: ObjectId,
        writer: &mut W,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        self.read_object_to(id, ObjectKind::Blob, writer, cancel)
    }

    pub fn read_object_to<W: Write>(
        &self,
        id: ObjectId,
        expected: ObjectKind,
        writer: &mut W,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        let mut file = self.open_object(id)?;
        let header = self.verify_file(&mut file, id, Some(expected), cancel)?;
        file.seek(SeekFrom::Start(FRAME_HEADER_LEN as u64))
            .map_err(|error| io_error("rewind blob", error))?;
        let mut remaining = header.payload_len;
        let mut buffer = [0_u8; CHUNK_BYTES];
        let mut hasher = ObjectHasher::new(header.kind, header.payload_len, &self.options.limits)?;
        while remaining != 0 {
            cancel.check()?;
            let count = usize::try_from(remaining.min(CHUNK_BYTES as u64))
                .map_err(|_| StoreError::LimitExceeded("blob chunk"))?;
            let n = file
                .read(&mut buffer[..count])
                .map_err(|error| io_error("read blob", error))?;
            if n == 0 {
                return Err(StoreError::Corrupt {
                    kind: "blob",
                    reason: "changed after verification",
                });
            }
            writer
                .write_all(&buffer[..n])
                .map_err(|error| io_error("write blob output", error))?;
            hasher.update(&buffer[..n])?;
            remaining = remaining
                .checked_sub(n as u64)
                .ok_or(StoreError::LimitExceeded("blob length"))?;
        }
        check_end(&mut file)?;
        if hasher.finish()? != id {
            return Err(StoreError::Corrupt {
                kind: "blob",
                reason: "changed after verification",
            });
        }
        Ok(header.payload_len)
    }

    pub(crate) fn open_object(&self, id: ObjectId) -> Result<File> {
        self.open_object_with_shard(id).map(|(file, _)| file)
    }

    pub(crate) fn open_object_with_shard(&self, id: ObjectId) -> Result<(File, Directory)> {
        let name = object_name(id);
        let (prefix, suffix) = object_parts(&name)?;
        let shard = self
            .objects
            .open_dir(prefix)
            .map_err(|error| io_error("open object shard", error))?;
        let file = shard
            .open_read(suffix)
            .map_err(|error| io_error("open immutable object", error))?;
        Ok((file, shard))
    }

    pub(crate) fn read_header(
        &self,
        file: &mut File,
        id: ObjectId,
        expected: Option<ObjectKind>,
    ) -> Result<ObjectHeader> {
        file.seek(SeekFrom::Start(0))
            .map_err(|error| io_error("rewind object", error))?;
        let mut header = [0_u8; FRAME_HEADER_LEN];
        file.read_exact(&mut header)
            .map_err(|error| io_error("read object header", error))?;
        let (kind, payload_len) = parse_frame_header(&header, &self.options.limits)?;
        if let Some(expected) = expected
            && expected != kind
        {
            return Err(StoreError::UnexpectedKind {
                id,
                expected,
                actual: kind,
            });
        }
        let actual_len = file
            .metadata()
            .map_err(|error| io_error("stat object", error))?
            .len();
        let framed_len = payload_len
            .checked_add(FRAME_HEADER_LEN as u64)
            .ok_or(StoreError::LimitExceeded("framed object length"))?;
        if actual_len != framed_len {
            return Err(StoreError::Corrupt {
                kind: "object",
                reason: "frame length does not match file length",
            });
        }
        Ok(ObjectHeader { kind, payload_len })
    }

    pub(crate) fn verify_file(
        &self,
        file: &mut File,
        id: ObjectId,
        expected: Option<ObjectKind>,
        cancel: &CancellationToken,
    ) -> Result<ObjectHeader> {
        cancel.check()?;
        let header = self.read_header(file, id, expected)?;
        let mut hasher = ObjectHasher::new(header.kind, header.payload_len, &self.options.limits)?;
        let mut remaining = header.payload_len;
        let mut buffer = [0_u8; CHUNK_BYTES];
        while remaining != 0 {
            cancel.check()?;
            let count = usize::try_from(remaining.min(CHUNK_BYTES as u64))
                .map_err(|_| StoreError::LimitExceeded("verification chunk"))?;
            let n = file
                .read(&mut buffer[..count])
                .map_err(|error| io_error("verify object payload", error))?;
            if n == 0 {
                return Err(StoreError::Corrupt {
                    kind: "object",
                    reason: "truncated payload",
                });
            }
            hasher.update(&buffer[..n])?;
            remaining = remaining
                .checked_sub(n as u64)
                .ok_or(StoreError::LimitExceeded("verification length"))?;
        }
        check_end(file)?;
        if hasher.finish()? != id {
            return Err(StoreError::Corrupt {
                kind: "object",
                reason: "hash mismatch",
            });
        }
        Ok(header)
    }
}

fn check_end(file: &mut File) -> Result<()> {
    let mut extra = [0_u8; 1];
    if file
        .read(&mut extra)
        .map_err(|error| io_error("check object EOF", error))?
        != 0
    {
        return Err(StoreError::Corrupt {
            kind: "object",
            reason: "trailing data",
        });
    }
    Ok(())
}

fn compare_files(a: &mut File, b: &mut File, cancel: &CancellationToken) -> Result<()> {
    a.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("rewind existing object", error))?;
    b.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("rewind prepared object", error))?;
    let mut left = [0_u8; CHUNK_BYTES];
    let mut right = [0_u8; CHUNK_BYTES];
    loop {
        cancel.check()?;
        let n = a
            .read(&mut left)
            .map_err(|error| io_error("compare existing object", error))?;
        if n == 0 {
            return check_end(b);
        }
        b.read_exact(&mut right[..n])
            .map_err(|error| io_error("compare prepared object", error))?;
        if left[..n] != right[..n] {
            return Err(StoreError::Corrupt {
                kind: "object",
                reason: "identifier collision",
            });
        }
    }
}
