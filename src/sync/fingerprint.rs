use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const PARTIAL_THRESHOLD: u64 = 1024 * 1024; // 1 MB
const PARTIAL_CHUNK: u64 = 512 * 1024; // 0.5 MB

pub fn hash_file(path: &Path, size: u64) -> Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    hasher.update(size.to_le_bytes());

    if size <= PARTIAL_THRESHOLD {
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)?;
        hasher.update(&buf);
    } else {
        // First 0.5 MB
        let mut head = vec![0u8; PARTIAL_CHUNK as usize];
        file.read_exact(&mut head)?;
        hasher.update(&head);

        // Last 0.5 MB
        file.seek(SeekFrom::End(-(PARTIAL_CHUNK as i64)))?;
        let mut tail = vec![0u8; PARTIAL_CHUNK as usize];
        file.read_exact(&mut tail)?;
        hasher.update(&tail);
    }

    Ok(hasher.finalize().into())
}
