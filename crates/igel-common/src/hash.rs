use std::fs;
use std::io::{self, Read};
use std::path::Path;

#[inline]
pub fn xxhash3(data: &[u8]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(data)
}

#[inline]
pub fn xxhash3_hex(data: &[u8]) -> String {
    format!("{:016x}", xxhash3(data))
}

pub fn xxhash3_file(path: &Path) -> io::Result<u64> {
    let metadata = fs::metadata(path)?;
    let size = metadata.len();

    if size <= 128 * 1024 {
        let data = fs::read(path)?;
        Ok(xxhash3(&data))
    } else {
        let mut file = fs::File::open(path)?;
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.digest())
    }
}

pub fn blake3_file(path: &Path) -> io::Result<String> {
    let data = fs::read(path)?;
    Ok(blake3::hash(&data).to_hex().to_string())
}

#[inline]
pub fn blake3_bytes(data: &[u8]) -> String {
    blake3::hash(data).to_hex().to_string()
}

pub struct HashBuilder {
    blake3: blake3::Hasher,
    xxh3: xxhash_rust::xxh3::Xxh3,
}

impl HashBuilder {
    pub fn new() -> Self {
        Self {
            blake3: blake3::Hasher::new(),
            xxh3: xxhash_rust::xxh3::Xxh3::new(),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        self.blake3.update(data);
        self.xxh3.update(data);
    }

    pub fn finish_xxhash3(&self) -> String {
        format!("{:016x}", self.xxh3.digest())
    }

    pub fn finish_blake3(&self) -> String {
        self.blake3.finalize().to_hex().to_string()
    }

    pub fn finish(&self) -> (String, String) {
        (self.finish_xxhash3(), self.finish_blake3())
    }
}

impl Default for HashBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xxhash3_deterministic() {
        let data = b"hello world";
        assert_eq!(xxhash3(data), xxhash3(data));
    }

    #[test]
    fn test_xxhash3_different_inputs() {
        assert_ne!(xxhash3(b"hello"), xxhash3(b"world"));
    }

    #[test]
    fn test_blake3_deterministic() {
        let data = b"hello world";
        assert_eq!(blake3_bytes(data), blake3_bytes(data));
    }

    #[test]
    fn test_hash_builder() {
        let mut hb = HashBuilder::new();
        hb.update(b"test");
        hb.update(b"data");

        let (xxh, blake) = hb.finish();
        assert_eq!(xxh.len(), 16);
        assert_eq!(blake.len(), 64);

        let mut hb2 = HashBuilder::new();
        hb2.update(b"test");
        hb2.update(b"data");
        assert_eq!(hb.finish(), hb2.finish());
    }

    #[test]
    fn test_xxhash3_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        fs::write(&path, b"file content").unwrap();
        let h1 = xxhash3_file(&path).unwrap();
        let h2 = xxhash3_file(&path).unwrap();
        assert_eq!(h1, h2);
    }
}
