#![forbid(unsafe_code)]
//! Original bounded, byte-preserving line diff and conservative three-way merge.

use std::{
    fmt,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_input_bytes: usize,
    pub max_lines: usize,
    /// Maximum Myers search depth. Exact one-sided middles need no search.
    pub max_edit_distance: usize,
    pub max_work: usize,
    pub max_allocation_bytes: usize,
    pub max_output_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_input_bytes: 64 * 1024 * 1024,
            max_lines: 1_000_000,
            max_edit_distance: 2048,
            max_work: 100_000_000,
            max_allocation_bytes: 128 * 1024 * 1024,
            max_output_bytes: 128 * 1024 * 1024,
        }
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Options<'a> {
    pub limits: Limits,
    pub cancellation: Option<&'a AtomicBool>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitKind {
    InputBytes,
    Lines,
    EditDistance,
    Work,
    AllocationBytes,
    OutputBytes,
    Arithmetic,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeError {
    Limit(LimitKind),
    Cancelled,
    Allocation,
    SourceMismatch,
    InvalidLabel,
}
impl fmt::Display for MergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limit(kind) => write!(f, "merge resource limit: {kind:?}"),
            Self::Cancelled => f.write_str("merge cancelled"),
            Self::Allocation => f.write_str("merge allocation failed"),
            Self::SourceMismatch => f.write_str("patch source does not match"),
            Self::InvalidLabel => f.write_str("diff labels must not contain CR, LF or NUL"),
        }
    }
}
impl std::error::Error for MergeError {}

/// Byte and line ranges of one contiguous replacement. Ranges are zero-based.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub before: Range<usize>,
    pub after: Range<usize>,
    pub before_lines: Range<usize>,
    pub after_lines: Range<usize>,
}
/// Constructed only by `diff`; callers cannot substitute an unrelated source.
#[derive(Debug)]
pub struct Diff<'a> {
    before: &'a [u8],
    after: &'a [u8],
    changes: Vec<Change>,
}
impl<'a> Diff<'a> {
    pub fn before(&self) -> &'a [u8] {
        self.before
    }
    pub fn after(&self) -> &'a [u8] {
        self.after
    }
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictKind {
    Content,
    Binary,
    DeleteModify,
    AddAdd,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeChunk<'a> {
    Resolved(&'a [u8]),
    Conflict {
        kind: ConflictKind,
        base: Option<&'a [u8]>,
        ours: Option<&'a [u8]>,
        theirs: Option<&'a [u8]>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeResult<'a> {
    /// `None` means the file is absent, distinct from an empty file.
    Clean(Option<Vec<u8>>),
    /// Ordered chunks; conflict slices retain all exact original alternatives.
    Conflicted { chunks: Vec<MergeChunk<'a>> },
}

struct Budget<'a> {
    options: &'a Options<'a>,
    work: usize,
    allocation: usize,
}
impl<'a> Budget<'a> {
    fn new(options: &'a Options<'a>, inputs: &[&[u8]]) -> Result<Self, MergeError> {
        let mut this = Self {
            options,
            work: 0,
            allocation: 0,
        };
        this.tick(0)?;
        let size = inputs.iter().try_fold(0usize, |n, b| add(n, b.len()))?;
        if size > options.limits.max_input_bytes {
            return Err(MergeError::Limit(LimitKind::InputBytes));
        }
        Ok(this)
    }
    fn tick(&mut self, work: usize) -> Result<(), MergeError> {
        if self
            .options
            .cancellation
            .is_some_and(|c| c.load(Ordering::Relaxed))
        {
            return Err(MergeError::Cancelled);
        }
        self.work = add(self.work, work)?;
        if self.work > self.options.limits.max_work {
            return Err(MergeError::Limit(LimitKind::Work));
        }
        Ok(())
    }
    fn reserve<T>(&mut self, v: &mut Vec<T>, extra: usize) -> Result<(), MergeError> {
        self.tick(0)?;
        let needed = add(v.len(), extra)?;
        if needed <= v.capacity() {
            return Ok(());
        }
        let bytes = needed
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(MergeError::Limit(LimitKind::Arithmetic))?;
        // Account the full new allocation, including allocations replaced during growth.
        self.allocation = add(self.allocation, bytes)?;
        if self.allocation > self.options.limits.max_allocation_bytes {
            return Err(MergeError::Limit(LimitKind::AllocationBytes));
        }
        v.try_reserve_exact(extra)
            .map_err(|_| MergeError::Allocation)
    }
    fn append(&mut self, out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), MergeError> {
        self.tick(bytes.len())?;
        let needed = add(out.len(), bytes.len())?;
        if needed > self.options.limits.max_output_bytes {
            return Err(MergeError::Limit(LimitKind::OutputBytes));
        }
        if needed > out.capacity() {
            let doubled = out.capacity().checked_mul(2).unwrap_or(needed);
            let capacity = needed
                .max(doubled)
                .min(self.options.limits.max_output_bytes);
            self.reserve(out, capacity - out.len())?;
        }
        out.extend_from_slice(bytes);
        Ok(())
    }
    fn equal(&mut self, a: &[u8], b: &[u8]) -> Result<bool, MergeError> {
        if a.len() != b.len() {
            self.tick(1)?;
            return Ok(false);
        }
        for (aa, bb) in a.chunks(4096).zip(b.chunks(4096)) {
            self.tick(aa.len())?;
            if aa != bb {
                return Ok(false);
            }
        }
        self.tick(0)?;
        Ok(true)
    }
}
fn add(a: usize, b: usize) -> Result<usize, MergeError> {
    a.checked_add(b)
        .ok_or(MergeError::Limit(LimitKind::Arithmetic))
}

struct Lines<'a> {
    bytes: &'a [u8],
    ends: Vec<usize>,
}
impl<'a> Lines<'a> {
    fn new(bytes: &'a [u8], budget: &mut Budget<'_>) -> Result<Self, MergeError> {
        let mut count = 0usize;
        for block in bytes.chunks(4096) {
            budget.tick(block.len())?;
            count = add(count, block.iter().filter(|&&b| b == b'\n').count())?;
            if count > budget.options.limits.max_lines {
                return Err(MergeError::Limit(LimitKind::Lines));
            }
        }
        if bytes.last().is_some_and(|b| *b != b'\n') {
            count = add(count, 1)?;
        }
        if count > budget.options.limits.max_lines {
            return Err(MergeError::Limit(LimitKind::Lines));
        }
        let mut ends = Vec::new();
        budget.reserve(&mut ends, count)?;
        for (i, block) in bytes.chunks(4096).enumerate() {
            budget.tick(block.len())?;
            let offset = i * 4096;
            for (j, b) in block.iter().enumerate() {
                if *b == b'\n' {
                    ends.push(offset + j + 1);
                }
            }
        }
        if bytes.last().is_some_and(|b| *b != b'\n') {
            ends.push(bytes.len());
        }
        Ok(Self { bytes, ends })
    }
    fn len(&self) -> usize {
        self.ends.len()
    }
    fn offset(&self, line: usize) -> usize {
        if line == 0 { 0 } else { self.ends[line - 1] }
    }
    fn line(&self, line: usize) -> &'a [u8] {
        &self.bytes[self.offset(line)..self.ends[line]]
    }
    fn range(&self, lines: Range<usize>) -> Range<usize> {
        self.offset(lines.start)..self.offset(lines.end)
    }
}
#[derive(Clone, Copy)]
enum Step {
    Equal,
    Delete,
    Insert,
}

pub fn diff<'a>(
    before: &'a [u8],
    after: &'a [u8],
    options: &Options<'_>,
) -> Result<Diff<'a>, MergeError> {
    let mut budget = Budget::new(options, &[before, after])?;
    diff_inner(before, after, &mut budget)
}
fn diff_inner<'a>(
    before: &'a [u8],
    after: &'a [u8],
    budget: &mut Budget<'_>,
) -> Result<Diff<'a>, MergeError> {
    let a = Lines::new(before, budget)?;
    let b = Lines::new(after, budget)?;
    let mut prefix = 0;
    while prefix < a.len().min(b.len()) && budget.equal(a.line(prefix), b.line(prefix))? {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < (a.len() - prefix).min(b.len() - prefix)
        && budget.equal(a.line(a.len() - suffix - 1), b.line(b.len() - suffix - 1))?
    {
        suffix += 1;
    }
    let n = a.len() - prefix - suffix;
    let m = b.len() - prefix - suffix;
    if n == 0 && m == 0 {
        return Ok(Diff {
            before,
            after,
            changes: Vec::new(),
        });
    }
    // With one empty middle, every remaining line must be inserted/deleted.
    // The unique exact replacement range is already known, so no frontier
    // search or per-line edit path is needed, regardless of search depth.
    if n == 0 || m == 0 {
        budget.tick(1)?;
        let mut changes = Vec::new();
        budget.reserve(&mut changes, 1)?;
        changes.push(change(
            &a,
            &b,
            prefix..add(prefix, n)?,
            prefix..add(prefix, m)?,
        ));
        return Ok(Diff {
            before,
            after,
            changes,
        });
    }
    let max_d = add(n, m)?.min(budget.options.limits.max_edit_distance);
    let mut trace: Vec<Vec<usize>> = Vec::new();
    budget.reserve(&mut trace, add(max_d, 1)?)?;
    let mut distance = None;
    for d in 0..=max_d {
        let mut row = Vec::new();
        budget.reserve(&mut row, add(d, 1)?)?;
        for i in 0..=d {
            budget.tick(1)?;
            let mut x = if d == 0 {
                0
            } else {
                let previous = &trace[d - 1];
                if i == 0 || (i < d && previous[i - 1] < previous[i]) {
                    previous[i]
                } else {
                    add(previous[i - 1], 1)?
                }
            };
            // k = 2*i-d; calculate y=x-k without signed conversion.
            let positive = i >= d - i;
            let delta = if positive { i - (d - i) } else { (d - i) - i };
            let y_start = if positive {
                x.checked_sub(delta)
            } else {
                x.checked_add(delta)
            };
            let Some(mut y) = y_start else {
                row.push(x);
                continue;
            };
            while x < n && y < m && budget.equal(a.line(prefix + x), b.line(prefix + y))? {
                x += 1;
                y += 1;
            }
            row.push(x);
            if x == n && y == m {
                distance = Some(d);
                break;
            }
        }
        trace.push(row);
        if distance.is_some() {
            break;
        }
    }
    let d_end = distance.ok_or(MergeError::Limit(LimitKind::EditDistance))?;
    let mut steps = Vec::new();
    budget.reserve(&mut steps, add(n, m)?)?;
    let (mut x, mut y) = (n, m);
    for d in (1..=d_end).rev() {
        budget.tick(1)?;
        // i=(x-y+d)/2, expressed without signed arithmetic.
        let i = if x >= y {
            add(x - y, d)? / 2
        } else {
            (d - (y - x)) / 2
        };
        let previous = &trace[d - 1];
        let down = i == 0 || (i < d && previous[i - 1] < previous[i]);
        let pi = if down { i } else { i - 1 };
        let px = previous[pi];
        let pd = d - 1;
        let py = if pi >= pd - pi {
            px - (pi - (pd - pi))
        } else {
            add(px, (pd - pi) - pi)?
        };
        while x > px && y > py {
            budget.tick(1)?;
            steps.push(Step::Equal);
            x -= 1;
            y -= 1;
        }
        if down {
            steps.push(Step::Insert);
            y -= 1;
        } else {
            steps.push(Step::Delete);
            x -= 1;
        }
    }
    while x > 0 && y > 0 {
        budget.tick(1)?;
        steps.push(Step::Equal);
        x -= 1;
        y -= 1;
    }
    steps.reverse();
    let mut changes = Vec::new();
    budget.reserve(&mut changes, add(d_end, 1)?)?;
    let (mut ai, mut bi) = (prefix, prefix);
    let mut start = None;
    for step in steps {
        budget.tick(1)?;
        match step {
            Step::Equal => {
                if let Some((sa, sb)) = start.take() {
                    changes.push(change(&a, &b, sa..ai, sb..bi));
                }
                ai += 1;
                bi += 1;
            }
            Step::Delete => {
                start.get_or_insert((ai, bi));
                ai += 1;
            }
            Step::Insert => {
                start.get_or_insert((ai, bi));
                bi += 1;
            }
        }
    }
    if let Some((sa, sb)) = start {
        changes.push(change(&a, &b, sa..ai, sb..bi));
    }
    Ok(Diff {
        before,
        after,
        changes,
    })
}
fn change(a: &Lines<'_>, b: &Lines<'_>, aa: Range<usize>, bb: Range<usize>) -> Change {
    Change {
        before: a.range(aa.clone()),
        after: b.range(bb.clone()),
        before_lines: aa,
        after_lines: bb,
    }
}

/// Apply exact replacement ranges; rejects a source different from the diff source.
pub fn apply_patch(
    source: &[u8],
    patch: &Diff<'_>,
    options: &Options<'_>,
) -> Result<Vec<u8>, MergeError> {
    let mut budget = Budget::new(options, &[source, patch.after])?;
    if !budget.equal(source, patch.before)? {
        return Err(MergeError::SourceMismatch);
    }
    let mut out = Vec::new();
    let mut position = 0;
    for h in &patch.changes {
        budget.append(&mut out, &source[position..h.before.start])?;
        budget.append(&mut out, &patch.after[h.after.clone()])?;
        position = h.before.end;
    }
    budget.append(&mut out, &source[position..])?;
    Ok(out)
}

fn whole_conflict<'a>(
    kind: ConflictKind,
    base: Option<&'a [u8]>,
    ours: Option<&'a [u8]>,
    theirs: Option<&'a [u8]>,
    budget: &mut Budget<'_>,
) -> Result<MergeResult<'a>, MergeError> {
    let mut chunks = Vec::new();
    budget.reserve(&mut chunks, 1)?;
    chunks.push(MergeChunk::Conflict {
        kind,
        base,
        ours,
        theirs,
    });
    Ok(MergeResult::Conflicted { chunks })
}
fn option_equal(
    a: Option<&[u8]>,
    b: Option<&[u8]>,
    budget: &mut Budget<'_>,
) -> Result<bool, MergeError> {
    match (a, b) {
        (None, None) => Ok(true),
        (Some(a), Some(b)) => budget.equal(a, b),
        _ => {
            budget.tick(1)?;
            Ok(false)
        }
    }
}
fn clean(
    bytes: Option<&[u8]>,
    budget: &mut Budget<'_>,
) -> Result<MergeResult<'static>, MergeError> {
    match bytes {
        None => Ok(MergeResult::Clean(None)),
        Some(bytes) => {
            let mut out = Vec::new();
            budget.append(&mut out, bytes)?;
            Ok(MergeResult::Clean(Some(out)))
        }
    }
}
/// Merge caller-classified binary data, including symlink targets. Only equal
/// alternatives and one-sided changes resolve; divergent bytes remain explicit.
pub fn merge_binary<'a>(
    base: Option<&'a [u8]>,
    ours: Option<&'a [u8]>,
    theirs: Option<&'a [u8]>,
    options: &Options<'_>,
) -> Result<MergeResult<'a>, MergeError> {
    let mut budget = Budget::new(
        options,
        &[
            base.unwrap_or_default(),
            ours.unwrap_or_default(),
            theirs.unwrap_or_default(),
        ],
    )?;
    if option_equal(ours, theirs, &mut budget)? {
        return clean(ours, &mut budget);
    }
    if option_equal(base, ours, &mut budget)? {
        return clean(theirs, &mut budget);
    }
    if option_equal(base, theirs, &mut budget)? {
        return clean(ours, &mut budget);
    }
    let kind = if ours.is_none() || theirs.is_none() {
        ConflictKind::DeleteModify
    } else {
        ConflictKind::Binary
    };
    whole_conflict(kind, base, ours, theirs, &mut budget)
}
/// Conservative line merge. NUL marks binary content; callers must separately merge entry type/mode.
pub fn merge<'a>(
    base: Option<&'a [u8]>,
    ours: Option<&'a [u8]>,
    theirs: Option<&'a [u8]>,
    options: &Options<'_>,
) -> Result<MergeResult<'a>, MergeError> {
    let mut budget = Budget::new(
        options,
        &[
            base.unwrap_or_default(),
            ours.unwrap_or_default(),
            theirs.unwrap_or_default(),
        ],
    )?;
    if option_equal(ours, theirs, &mut budget)? {
        return clean(ours, &mut budget);
    }
    if option_equal(base, ours, &mut budget)? {
        return clean(theirs, &mut budget);
    }
    if option_equal(base, theirs, &mut budget)? {
        return clean(ours, &mut budget);
    }
    let (base, ours, theirs) = match (base, ours, theirs) {
        (None, ours, theirs) => {
            return whole_conflict(ConflictKind::AddAdd, None, ours, theirs, &mut budget);
        }
        (base, None, theirs) => {
            return whole_conflict(ConflictKind::DeleteModify, base, None, theirs, &mut budget);
        }
        (base, ours, None) => {
            return whole_conflict(ConflictKind::DeleteModify, base, ours, None, &mut budget);
        }
        (Some(base), Some(ours), Some(theirs)) => (base, ours, theirs),
    };
    for bytes in [base, ours, theirs] {
        for block in bytes.chunks(4096) {
            budget.tick(block.len())?;
            if block.contains(&0) {
                return whole_conflict(
                    ConflictKind::Binary,
                    Some(base),
                    Some(ours),
                    Some(theirs),
                    &mut budget,
                );
            }
        }
    }
    let od = diff_inner(base, ours, &mut budget)?;
    let td = diff_inner(base, theirs, &mut budget)?;
    let mut chunks = Vec::new();
    budget.reserve(
        &mut chunks,
        add(
            add(od.changes.len(), td.changes.len())?
                .checked_mul(2)
                .ok_or(MergeError::Limit(LimitKind::Arithmetic))?,
            1,
        )?,
    )?;
    let (mut oi, mut ti, mut bc, mut oc, mut tc) = (0, 0, 0, 0, 0);
    let mut conflicted = false;
    while oi < od.changes.len() || ti < td.changes.len() {
        budget.tick(1)?;
        let start = match (od.changes.get(oi), td.changes.get(ti)) {
            (Some(o), Some(t)) => o.before.start.min(t.before.start),
            (Some(o), None) => o.before.start,
            (None, Some(t)) => t.before.start,
            (None, None) => break,
        };
        if start > bc {
            chunks.push(MergeChunk::Resolved(&base[bc..start]));
        }
        oc = add(oc, start - bc)?;
        tc = add(tc, start - bc)?;
        let (os, ts) = (oi, ti);
        let mut region = start..start;
        loop {
            let mut advanced = false;
            if let Some(o) = od.changes.get(oi)
                && ((oi == os && ti == ts && o.before.start == start)
                    || overlaps(&region, &o.before))
            {
                region.end = region.end.max(o.before.end);
                oi += 1;
                advanced = true;
            }
            if let Some(t) = td.changes.get(ti)
                && ((oi == os && ti == ts && t.before.start == start)
                    || overlaps(&region, &t.before))
            {
                region.end = region.end.max(t.before.end);
                ti += 1;
                advanced = true;
            }
            if !advanced {
                break;
            }
            budget.tick(1)?;
        }
        let oe = if oi > os {
            let h = &od.changes[oi - 1];
            add(h.after.end, region.end - h.before.end)?
        } else {
            add(oc, region.end - start)?
        };
        let te = if ti > ts {
            let h = &td.changes[ti - 1];
            add(h.after.end, region.end - h.before.end)?
        } else {
            add(tc, region.end - start)?
        };
        let ob = &ours[oc..oe];
        let tb = &theirs[tc..te];
        if oi == os {
            chunks.push(MergeChunk::Resolved(tb));
        } else if ti == ts || budget.equal(ob, tb)? {
            chunks.push(MergeChunk::Resolved(ob));
        } else {
            conflicted = true;
            chunks.push(MergeChunk::Conflict {
                kind: ConflictKind::Content,
                base: Some(&base[region.clone()]),
                ours: Some(ob),
                theirs: Some(tb),
            });
        }
        bc = region.end;
        oc = oe;
        tc = te;
    }
    if bc < base.len() {
        chunks.push(MergeChunk::Resolved(&base[bc..]));
    }
    if conflicted {
        Ok(MergeResult::Conflicted { chunks })
    } else {
        let mut out = Vec::new();
        for chunk in chunks {
            if let MergeChunk::Resolved(bytes) = chunk {
                budget.append(&mut out, bytes)?;
            }
        }
        Ok(MergeResult::Clean(Some(out)))
    }
}
fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    if a.is_empty() {
        b.start <= a.start && a.start <= b.end
    } else if b.is_empty() {
        a.start <= b.start && b.start <= a.end
    } else {
        a.start < b.end && b.start < a.end
    }
}

/// Unified byte rendering, including conventional missing-final-newline markers.
/// File labels are supplied by the caller; no terminal escaping is performed.
pub fn render_unified(
    patch: &Diff<'_>,
    before_label: &[u8],
    after_label: &[u8],
    context_lines: usize,
    options: &Options<'_>,
) -> Result<Vec<u8>, MergeError> {
    let mut budget = Budget::new(
        options,
        &[patch.before, patch.after, before_label, after_label],
    )?;
    for label in [before_label, after_label] {
        for block in label.chunks(4096) {
            budget.tick(block.len())?;
            if block.iter().any(|b| matches!(b, 0 | b'\n' | b'\r')) {
                return Err(MergeError::InvalidLabel);
            }
        }
    }
    let mut out = Vec::new();
    if patch.is_empty() {
        return Ok(out);
    }
    budget.append(&mut out, b"--- ")?;
    budget.append(&mut out, before_label)?;
    budget.append(&mut out, b"\n+++ ")?;
    budget.append(&mut out, after_label)?;
    budget.append(&mut out, b"\n")?;
    let a = Lines::new(patch.before, &mut budget)?;
    let b = Lines::new(patch.after, &mut budget)?;
    let mut i = 0;
    while i < patch.changes.len() {
        budget.tick(1)?;
        let first = &patch.changes[i];
        let ast = first.before_lines.start.saturating_sub(context_lines);
        let bst = first.after_lines.start.saturating_sub(context_lines);
        let mut aend = add(first.before_lines.end, context_lines)?.min(a.len());
        let mut bend = add(first.after_lines.end, context_lines)?.min(b.len());
        let mut j = i + 1;
        while let Some(next) = patch.changes.get(j) {
            budget.tick(1)?;
            if next.before_lines.start.saturating_sub(context_lines) > aend {
                break;
            }
            aend = add(next.before_lines.end, context_lines)?.min(a.len());
            bend = add(next.after_lines.end, context_lines)?.min(b.len());
            j += 1;
        }
        budget.append(&mut out, b"@@ -")?;
        number(
            &mut out,
            if aend == ast { ast } else { ast + 1 },
            &mut budget,
        )?;
        budget.append(&mut out, b",")?;
        number(&mut out, aend - ast, &mut budget)?;
        budget.append(&mut out, b" +")?;
        number(
            &mut out,
            if bend == bst { bst } else { bst + 1 },
            &mut budget,
        )?;
        budget.append(&mut out, b",")?;
        number(&mut out, bend - bst, &mut budget)?;
        budget.append(&mut out, b" @@\n")?;
        let mut cursor = ast;
        for h in &patch.changes[i..j] {
            for line in cursor..h.before_lines.start {
                render_line(&mut out, b' ', a.line(line), &mut budget)?;
            }
            for line in h.before_lines.clone() {
                render_line(&mut out, b'-', a.line(line), &mut budget)?;
            }
            for line in h.after_lines.clone() {
                render_line(&mut out, b'+', b.line(line), &mut budget)?;
            }
            cursor = h.before_lines.end;
        }
        for line in cursor..aend {
            render_line(&mut out, b' ', a.line(line), &mut budget)?;
        }
        i = j;
    }
    Ok(out)
}
fn number(out: &mut Vec<u8>, mut n: usize, budget: &mut Budget<'_>) -> Result<(), MergeError> {
    let mut digits = [0u8; 40];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    budget.append(out, &digits[i..])
}
fn render_line(
    out: &mut Vec<u8>,
    prefix: u8,
    line: &[u8],
    budget: &mut Budget<'_>,
) -> Result<(), MergeError> {
    budget.append(out, &[prefix])?;
    budget.append(out, line)?;
    if line.last() != Some(&b'\n') {
        budget.append(out, b"\n\\ No newline at end of file\n")?;
    }
    Ok(())
}
