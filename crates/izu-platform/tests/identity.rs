#![cfg(any(unix, windows))]

use izu_platform::FileIdentity;
use std::fs::{self, File};
use std::io::Write;

#[test]
fn open_identity_tracks_object_across_rename_and_path_replacement()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let original = fixture.path().join("original");
    let held = File::create_new(&original)?;
    let identity = FileIdentity::of(&held)?;
    assert_eq!(identity, FileIdentity::of(&held.try_clone()?)?);
    let moved = fixture.path().join("moved");
    fs::rename(&original, &moved)?;
    assert_eq!(identity, FileIdentity::of(&File::open(&moved)?)?);
    let replacement = File::create_new(&original)?;
    assert_ne!(identity, FileIdentity::of(&replacement)?);
    assert_eq!(identity, FileIdentity::of(&held)?);
    Ok(())
}

#[test]
fn hard_links_share_identity_but_equal_contents_do_not() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let first = fixture.path().join("first");
    let mut held = File::create_new(&first)?;
    held.write_all(b"same content")?;
    let link = fixture.path().join("link");
    fs::hard_link(&first, &link)?;
    let other = fixture.path().join("other");
    fs::write(&other, b"same content")?;
    let identity = FileIdentity::of(&held)?;
    assert_eq!(identity, FileIdentity::of(&File::open(&link)?)?);
    assert_ne!(identity, FileIdentity::of(&File::open(&other)?)?);
    Ok(())
}
