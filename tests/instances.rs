//! The cross-instance registry: two dirsync processes must never mirror into
//! each other's folders. Several registries in one test process stand in for
//! separate processes: each holds its own OS file lock, exactly as a second
//! process would.

use dirsync::instances::Registry;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Canonical endpoint paths under a scratch root, so overlap checks compare
/// the same forms the entrypoints hand in.
fn dirs(root: &Path, names: &[&str]) -> Vec<PathBuf> {
    names
        .iter()
        .map(|n| {
            let p = root.join(n);
            std::fs::create_dir_all(&p).unwrap();
            p.canonicalize().unwrap()
        })
        .collect()
}

#[test]
fn disjoint_pairs_can_run_side_by_side() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "c", "d"]);
    let first = Registry::open(reg.path()).unwrap();
    let second = Registry::open(reg.path()).unwrap();

    first.claim(&d[0], &d[1]).unwrap();

    // The use case: A -> B in one window, C -> D in another.
    second.claim(&d[2], &d[3]).unwrap();
}

#[test]
fn the_same_dst_is_refused() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "c"]);
    let first = Registry::open(reg.path()).unwrap();
    let second = Registry::open(reg.path()).unwrap();
    first.claim(&d[0], &d[1]).unwrap();

    // Two mirrors into one folder delete each other's files as orphans.
    let err = second.claim(&d[2], &d[1]).unwrap_err();

    let msg = err.to_string();
    assert!(msg.contains("another dirsync"), "{msg}");
    assert!(msg.contains(&dirsync::paths::display_path(&d[1])), "{msg}");
}

#[test]
fn a_dst_nested_in_either_direction_is_refused() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "b/inner", "c", "e"]);
    let first = Registry::open(reg.path()).unwrap();
    let second = Registry::open(reg.path()).unwrap();
    first.claim(&d[0], &d[1]).unwrap();

    assert!(second.claim(&d[3], &d[2]).is_err(), "DST inside their DST");
    first.claim(&d[0], &d[2]).unwrap();
    assert!(second.claim(&d[3], &d[1]).is_err(), "DST around their DST");
    assert!(second.claim(&d[3], &d[4]).is_ok());
}

#[test]
fn writing_into_what_another_instance_reads_is_refused() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "c"]);
    let first = Registry::open(reg.path()).unwrap();
    let second = Registry::open(reg.path()).unwrap();
    first.claim(&d[0], &d[1]).unwrap();

    // My DST is their SRC: I would change the tree they are mirroring.
    assert!(second.claim(&d[2], &d[0]).is_err());
    // My SRC is their DST: I would read a tree while they rewrite it.
    assert!(second.claim(&d[1], &d[2]).is_err());
}

#[test]
fn two_instances_may_read_the_same_src() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "c"]);
    let first = Registry::open(reg.path()).unwrap();
    let second = Registry::open(reg.path()).unwrap();
    first.claim(&d[0], &d[1]).unwrap();

    // Mirroring one source to two backups only reads it twice.
    second.claim(&d[0], &d[2]).unwrap();
}

#[test]
fn a_new_claim_replaces_the_instances_previous_one() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "c", "d"]);
    let first = Registry::open(reg.path()).unwrap();
    let second = Registry::open(reg.path()).unwrap();
    first.claim(&d[0], &d[1]).unwrap();

    // The GUI window moved on to another pair: B is free again.
    first.claim(&d[2], &d[3]).unwrap();

    second.claim(&d[0], &d[1]).unwrap();
}

#[test]
fn an_instance_never_conflicts_with_itself() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b"]);
    let only = Registry::open(reg.path()).unwrap();

    only.claim(&d[0], &d[1]).unwrap();
    // Preview, then run: the same pair claimed twice.
    only.claim(&d[0], &d[1]).unwrap();
}

#[test]
fn a_released_or_dropped_instance_frees_its_pair() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "c"]);
    let second = Registry::open(reg.path()).unwrap();

    let first = Registry::open(reg.path()).unwrap();
    first.claim(&d[0], &d[1]).unwrap();
    first.release();
    second.claim(&d[2], &d[1]).unwrap();
    second.release();

    let third = Registry::open(reg.path()).unwrap();
    third.claim(&d[0], &d[1]).unwrap();
    drop(third);
    second.claim(&d[2], &d[1]).unwrap();
}

#[test]
fn an_entry_whose_owner_died_is_stale_and_removed() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(t.path(), &["a", "b", "c"]);
    // What a crashed process leaves behind: its entry, but no lock holder.
    let crashed = Registry::open(reg.path()).unwrap();
    crashed.claim(&d[0], &d[1]).unwrap();
    crashed.forget_without_cleanup();

    let second = Registry::open(reg.path()).unwrap();
    second.claim(&d[2], &d[1]).unwrap();

    let leftovers: Vec<_> = std::fs::read_dir(reg.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".json"))
        .collect();
    assert_eq!(leftovers.len(), 1, "stale entry not cleaned: {leftovers:?}");
}

#[test]
fn simultaneous_claims_of_one_dst_let_exactly_one_win() {
    let reg = TempDir::new().unwrap();
    let t = TempDir::new().unwrap();
    let d = dirs(
        t.path(),
        &["dst", "s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7"],
    );
    let registries: Vec<Registry> = (0..8)
        .map(|_| Registry::open(reg.path()).unwrap())
        .collect();

    let wins = std::thread::scope(|s| {
        let handles: Vec<_> = registries
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let (src, dst) = (&d[i + 1], &d[0]);
                s.spawn(move || r.claim(src, dst).is_ok())
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count()
    });

    assert_eq!(wins, 1);
}
