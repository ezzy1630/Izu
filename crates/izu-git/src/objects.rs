use crate::{Error, GitObjectId, Result};
use sha1::{Digest, Sha1};

#[derive(Clone, Debug)]
pub(crate) struct Commit {
    pub tree: GitObjectId,
    pub parents: Vec<GitObjectId>,
    pub author_name: String,
    pub author_email: String,
    pub author_seconds: i64,
    pub message: String,
    pub raw: Vec<u8>,
    pub unsupported: Vec<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct TreeEntry {
    pub mode: u32,
    pub name: String,
    pub object: GitObjectId,
}

pub(crate) fn object_id(kind: &str, bytes: &[u8]) -> GitObjectId {
    let mut hash = Sha1::new();
    hash.update(format!("{kind} {}\0", bytes.len()).as_bytes());
    hash.update(bytes);
    GitObjectId::from_bytes(hash.finalize().into())
}

pub(crate) fn commit(id: GitObjectId, raw: Vec<u8>) -> Result<Commit> {
    let fail = |reason: &str| Error::InvalidObject {
        object: id.to_string(),
        reason: reason.into(),
    };
    let boundary = raw
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .ok_or_else(|| fail("commit has no header/message separator"))?;
    let headers = std::str::from_utf8(&raw[..boundary])
        .map_err(|_| fail("non-UTF8 commit headers are unsupported"))?;
    let message = std::str::from_utf8(&raw[boundary + 2..])
        .map_err(|_| fail("non-UTF8 commit messages are unsupported"))?
        .to_owned();
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = false;
    let mut unsupported = Vec::new();
    for line in headers.split('\n') {
        if line.starts_with(' ') {
            continue;
        }
        let (name, value) = line
            .split_once(' ')
            .ok_or_else(|| fail("malformed commit header"))?;
        match name {
            "tree" if tree.is_none() => tree = Some(value.parse()?),
            "parent" => {
                if parents.len() >= 1024 {
                    return Err(Error::Limit("commit parent count"));
                }
                parents.push(value.parse()?);
            }
            "author" if author.is_none() => {
                author = Some(signature(value).map_err(|reason| fail(&reason))?)
            }
            "committer" if !committer => {
                signature(value).map_err(|reason| fail(&reason))?;
                committer = true;
            }
            "gpgsig" | "gpgsig-sha256" | "mergetag" => {
                unsupported.push(format!("signed commit or merge tag ({name})"))
            }
            "encoding" if value.eq_ignore_ascii_case("utf-8") => {}
            "encoding" => unsupported.push(format!("commit encoding {value}")),
            "tree" | "author" | "committer" => {
                return Err(fail("duplicate singleton commit header"));
            }
            _ => unsupported.push(format!("unknown commit header {name}")),
        }
    }
    let tree = tree.ok_or_else(|| fail("missing tree"))?;
    let (author_name, author_email, author_seconds) =
        author.ok_or_else(|| fail("missing author"))?;
    if !committer {
        return Err(fail("missing committer"));
    }
    Ok(Commit {
        tree,
        parents,
        author_name,
        author_email,
        author_seconds,
        message,
        raw,
        unsupported,
    })
}

fn signature(value: &str) -> std::result::Result<(String, String, i64), String> {
    let (identity, rest) = value
        .rsplit_once("> ")
        .ok_or_else(|| "malformed commit identity".to_owned())?;
    let (name, email) = identity
        .rsplit_once(" <")
        .ok_or_else(|| "malformed commit identity".to_owned())?;
    let (seconds, timezone) = rest
        .split_once(' ')
        .ok_or_else(|| "malformed commit date".to_owned())?;
    let seconds = seconds
        .parse::<i64>()
        .map_err(|_| "commit timestamp is out of range".to_owned())?;
    seconds
        .checked_mul(1000)
        .ok_or_else(|| "commit timestamp cannot map to native milliseconds".to_owned())?;
    if timezone.len() != 5
        || !matches!(timezone.as_bytes()[0], b'+' | b'-')
        || !timezone.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err("invalid commit timezone".into());
    }
    let hours = timezone[1..3]
        .parse::<u8>()
        .map_err(|_| "invalid commit timezone".to_owned())?;
    let minutes = timezone[3..5]
        .parse::<u8>()
        .map_err(|_| "invalid commit timezone".to_owned())?;
    if hours > 23 || minutes > 59 {
        return Err("invalid commit timezone".into());
    }
    if name.is_empty()
        || email.is_empty()
        || name.contains(['<', '>', '\0', '\r', '\n'])
        || email.contains(['<', '>', '\0', '\r', '\n'])
    {
        return Err("unsupported commit identity".into());
    }
    Ok((name.to_owned(), email.to_owned(), seconds))
}

pub(crate) fn tree(id: GitObjectId, bytes: &[u8], max_entries: usize) -> Result<Vec<TreeEntry>> {
    let fail = |reason: &str| Error::InvalidObject {
        object: id.to_string(),
        reason: reason.into(),
    };
    let mut entries = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if entries.len() >= max_entries {
            return Err(Error::Limit("tree entries"));
        }
        let space = bytes[cursor..]
            .iter()
            .position(|byte| *byte == b' ')
            .map(|offset| cursor + offset)
            .ok_or_else(|| fail("tree mode has no separator"))?;
        let mode_bytes =
            std::str::from_utf8(&bytes[cursor..space]).map_err(|_| fail("invalid tree mode"))?;
        let mode = u32::from_str_radix(mode_bytes, 8).map_err(|_| fail("invalid tree mode"))?;
        let end = bytes[space + 1..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| space + 1 + offset)
            .ok_or_else(|| fail("tree entry has no terminator"))?;
        let name = std::str::from_utf8(&bytes[space + 1..end])
            .map_err(|_| fail("non-UTF8 source paths are unsupported"))?
            .to_owned();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains('/')
            || name.eq_ignore_ascii_case(".git")
            || name.eq_ignore_ascii_case(".izu")
        {
            return Err(fail("unsafe source path component"));
        }
        let object_end = end
            .checked_add(21)
            .ok_or(Error::Limit("tree object offset"))?;
        let object_slice = bytes
            .get(end + 1..object_end)
            .ok_or_else(|| fail("truncated tree object ID"))?;
        let object_bytes: [u8; 20] = object_slice
            .try_into()
            .map_err(|_| fail("truncated tree object ID"))?;
        let object = GitObjectId::from_bytes(object_bytes);
        if entries.iter().any(|entry: &TreeEntry| entry.name == name) {
            return Err(fail("duplicate tree entry"));
        }
        entries.push(TreeEntry { mode, name, object });
        cursor = object_end;
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_tree_path_and_truncation() {
        let id = object_id("tree", b"");
        for name in ["..", ".git", ".izu", "a/b"] {
            let mut bytes = format!("100644 {name}\0").into_bytes();
            bytes.extend_from_slice(&[0; 20]);
            assert!(tree(id, &bytes, 10).is_err());
        }
        assert!(tree(id, b"100644 file\0short", 10).is_err());
    }
    #[test]
    fn rejects_invalid_commit_identity_and_keeps_message_bytes() {
        let source = format!(
            "tree {}\nauthor A <a@b> 123 +0530\ncommitter B <b@c> 124 -0800\n\nexact\n",
            object_id("tree", b"")
        );
        let parsed = commit(
            object_id("commit", source.as_bytes()),
            source.as_bytes().to_vec(),
        )
        .expect("fixture");
        assert_eq!(parsed.message, "exact\n");
        assert_eq!(parsed.raw, source.as_bytes());
        let invalid = source.replace("123 +0530", "123 +9999");
        assert!(
            commit(
                object_id("commit", invalid.as_bytes()),
                invalid.into_bytes()
            )
            .is_err()
        );
    }
}
