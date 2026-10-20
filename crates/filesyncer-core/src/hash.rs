use crate::{Error, Result};
use std::path::Path;

/// BLAKE3 of a file's contents, as lowercase hex.
pub fn hash_file(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = blake3::Hasher::new();
    // `update_reader` reads in 64 KiB blocks, wide enough for BLAKE3's SIMD to
    // hash many chunks at once; `std::io::copy`'s 8 KiB blocks are not. Memory
    // mapping would be faster still, but a file truncated while mapped kills the
    // process with SIGBUS, and these files may be in use.
    hasher.update_reader(file).map_err(|e| Error::io(path, e))?;
    Ok(to_hex(hasher))
}

/// The lowercase-hex form every hash in the state database takes.
pub fn to_hex(hasher: blake3::Hasher) -> String {
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn identical_contents_hash_identically() {
        let a = write_temp(b"hello world");
        let b = write_temp(b"hello world");
        assert_eq!(hash_file(a.path()).unwrap(), hash_file(b.path()).unwrap());
    }

    #[test]
    fn different_contents_hash_differently() {
        let a = write_temp(b"hello world");
        let b = write_temp(b"hello worlds");
        assert_ne!(hash_file(a.path()).unwrap(), hash_file(b.path()).unwrap());
    }

    #[test]
    fn empty_file_hashes_without_error() {
        let a = write_temp(b"");
        assert_eq!(hash_file(a.path()).unwrap().len(), 64);
    }

    #[test]
    fn missing_file_reports_its_path() {
        let err = hash_file(Path::new("/nonexistent/xyzzy")).unwrap_err();
        assert!(err.to_string().contains("xyzzy"));
    }
}
