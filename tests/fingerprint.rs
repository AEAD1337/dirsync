use dirsync::sync::fingerprint::hash_file;
use std::fs;
use tempfile::TempDir;

// PARTIAL_THRESHOLD = 1 MB (1_048_576 bytes); files ≤ threshold use full read,
// files > threshold use head (first 512 KB) + tail (last 512 KB) only.

#[test]
fn test_hash_file_error_on_missing_file() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("does_not_exist.bin");
    assert!(
        hash_file(&missing, 100).is_err(),
        "hash_file on a missing path must return Err"
    );
}

#[test]
fn test_hash_file_at_threshold_uses_full_read() {
    // A file exactly at 1 MB is within the full-read path (size <= threshold).
    // Flipping the middle byte must produce a different hash.
    let dir = TempDir::new().unwrap();
    let size: usize = 1024 * 1024;
    let data_a = vec![0u8; size];
    let mut data_b = data_a.clone();
    data_b[size / 2] = 0xFF;

    let pa = dir.path().join("a.bin");
    let pb = dir.path().join("b.bin");
    fs::write(&pa, &data_a).unwrap();
    fs::write(&pb, &data_b).unwrap();

    let ha = hash_file(&pa, size as u64).unwrap();
    let hb = hash_file(&pb, size as u64).unwrap();
    assert_ne!(
        ha, hb,
        "1 MB file (at threshold): middle-byte diff must be detected"
    );
}

#[test]
fn test_hash_file_above_threshold_ignores_middle_byte() {
    // A file 1 byte above the threshold uses the partial (head+tail) path.
    // The head covers bytes 0..512 KB and the tail bytes (size-512 KB)..size.
    // A byte flipped exactly in the middle falls in the gap and must be invisible.
    let dir = TempDir::new().unwrap();
    let size: usize = 1024 * 1024 + 1;
    let data_a = vec![0u8; size];
    let mut data_b = data_a.clone();
    data_b[size / 2] = 0xFF; // middle byte: in the gap between head and tail

    let pa = dir.path().join("a.bin");
    let pb = dir.path().join("b.bin");
    fs::write(&pa, &data_a).unwrap();
    fs::write(&pb, &data_b).unwrap();

    let ha = hash_file(&pa, size as u64).unwrap();
    let hb = hash_file(&pb, size as u64).unwrap();
    assert_eq!(
        ha, hb,
        "file just above 1 MB: middle-byte diff must be invisible to partial hash"
    );
}

#[test]
fn test_small_file_hash_stable() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("small.bin");
    fs::write(&path, b"hello").unwrap();

    let h1 = hash_file(&path, 5).unwrap();
    let h2 = hash_file(&path, 5).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn test_different_content_different_hash() {
    let dir = TempDir::new().unwrap();
    let p1 = dir.path().join("a.bin");
    let p2 = dir.path().join("b.bin");
    fs::write(&p1, b"content A").unwrap();
    fs::write(&p2, b"content B").unwrap();

    let h1 = hash_file(&p1, 9).unwrap();
    let h2 = hash_file(&p2, 9).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn test_large_file_partial_hash() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("large.bin");

    // Create a 10 MB file (above 8 MB threshold → partial hash)
    let data = vec![0xABu8; 10 * 1024 * 1024];
    fs::write(&path, &data).unwrap();

    let h1 = hash_file(&path, data.len() as u64).unwrap();
    let h2 = hash_file(&path, data.len() as u64).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn test_different_sizes_same_head_tail_different_hash() {
    // Two large files with identical head+tail bytes but different total sizes must
    // produce different hashes now that size is folded into the digest (fix 2.5).
    // Before the fix both would hash identically, enabling a false move detection.
    let dir = TempDir::new().unwrap();
    let chunk = 512 * 1024; // 512 KB: exactly PARTIAL_CHUNK

    // File A: 2 MB of zeros
    let size_a: usize = 2 * 1024 * 1024;
    let data_a = vec![0u8; size_a];

    // File B: 3 MB: same first and last 512 KB (zeros), but a different total size.
    let size_b: usize = 3 * 1024 * 1024;
    let mut data_b = vec![0u8; size_b];
    // Middle bytes differ (irrelevant to partial hash - only size matters here).
    for b in &mut data_b[chunk..size_b - chunk] {
        *b = 0xFF;
    }

    let pa = dir.path().join("a.bin");
    let pb = dir.path().join("b.bin");
    fs::write(&pa, &data_a).unwrap();
    fs::write(&pb, &data_b).unwrap();

    let ha = hash_file(&pa, size_a as u64).unwrap();
    let hb = hash_file(&pb, size_b as u64).unwrap();
    assert_ne!(
        ha, hb,
        "files with different sizes must hash differently even when head+tail bytes are identical"
    );
}
