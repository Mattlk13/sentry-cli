use std::env;
use std::fs;
use std::io;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use flate2::read::GzDecoder;
use log::error;
use sha1_smol::{Digest, Sha1};
use uuid::Uuid;

pub trait SeekRead: Seek + Read {}
impl<T: Seek + Read> SeekRead for T {}

/// Helper for temporary dicts
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Creates a new tempdir
    pub fn create() -> io::Result<Self> {
        let mut path = env::temp_dir();
        path.push(Uuid::new_v4().as_hyphenated().to_string());
        fs::create_dir(&path)?;
        Ok(TempDir { path })
    }

    /// Returns the path to the tempdir
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Helper for temporary file access
#[derive(Debug)]
pub struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// Creates a new tempfile.
    pub fn create() -> io::Result<Self> {
        let mut path = env::temp_dir();
        path.push(Uuid::new_v4().as_hyphenated().to_string());

        let tf = TempFile { path };
        tf.open()?;
        Ok(tf)
    }

    /// Assumes ownership over an existing file and moves it to a temp location.
    pub fn take<P: AsRef<Path>>(path: P) -> io::Result<TempFile> {
        let mut destination = env::temp_dir();
        destination.push(Uuid::new_v4().as_hyphenated().to_string());

        fs::rename(&path, &destination)?;
        Ok(TempFile { path: destination })
    }

    /// Opens the tempfile for reading and writing at the beginning. We create the
    /// file if it doesn't exist yet, but if the file exists, we don't truncate it.
    /// The lack of truncation allows us to re-open and read a temp file from the
    /// beginning. Writing to the file will overwrite the existing content, but existing
    /// data after the written content will remain.
    pub fn open(&self) -> io::Result<fs::File> {
        let mut f = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // Allows us to re-open and read a temp file from the beginning
            .open(&self.path)?;

        f.rewind().ok();
        Ok(f)
    }

    /// Returns the path to the tempfile.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    #[cfg(not(windows))]
    fn drop(&mut self) {
        let result = fs::remove_file(&self.path);
        if let Err(e) = result {
            error!(
                "Failed to delete TempFile {}: {:?}",
                &self.path.display(),
                e
            );
        }
    }

    #[cfg(windows)]
    fn drop(&mut self) {
        // On Windows, we open the file handle to set "FILE_FLAG_DELETE_ON_CLOSE" so that it will be closed
        // when the last open handle to this file is gone.
        use std::os::windows::prelude::*;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_DELETE_ON_CLOSE;
        let result = fs::OpenOptions::new()
            .write(true)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(&self.path);

        if let Err(e) = result {
            error!(
                "Failed to open {} to flag for delete: {:?}",
                &self.path.display(),
                e
            );
        }
    }
}

/// Checks if a path is writable.
#[cfg(not(feature = "managed"))]
pub fn is_writable<P: AsRef<Path>>(path: P) -> bool {
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .map(|_| true)
        .unwrap_or(false)
}

/// Set the mode of a path to 755 if we're on a Unix machine, otherwise
/// don't do anything with the given path.
#[cfg(not(feature = "managed"))]
#[cfg(not(windows))]
pub fn set_executable_mode<P: AsRef<Path>>(path: P) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mut perm = fs::metadata(&path)?.permissions();
    perm.set_mode(0o755);
    fs::set_permissions(&path, perm)?;

    Ok(())
}

/// Returns the SHA1 hash of the given input.
pub fn get_sha1_checksum<R: Read>(rdr: R) -> Result<Digest> {
    let mut sha = Sha1::new();
    let mut buf = [0u8; 16384];
    let mut rdr = io::BufReader::new(rdr);
    loop {
        let read = rdr.read(&mut buf)?;
        if read == 0 {
            break;
        }
        sha.update(&buf[..read]);
    }
    Ok(sha.digest())
}

/// Returns the SHA1 hash for the entire input, as well as each chunk of it. The
/// `chunk_size` must be non-zero.
pub fn get_sha1_checksums(data: &[u8], chunk_size: usize) -> Result<(Digest, Vec<Digest>)> {
    if chunk_size == 0 {
        bail!("Chunk size may not be zero.");
    }

    let mut total_sha = Sha1::new();
    let mut chunks = Vec::new();

    for chunk in data.chunks(chunk_size) {
        let mut chunk_sha = Sha1::new();
        chunk_sha.update(chunk);
        total_sha.update(chunk);
        chunks.push(chunk_sha.digest());
    }

    Ok((total_sha.digest(), chunks))
}

/// Checks if provided slice contains gzipped data.
pub fn is_gzip_compressed(slice: &[u8]) -> bool {
    // Per https://www.ietf.org/rfc/rfc1952.txt
    const GZIP_MAGIC: [u8; 2] = [0x1F, 0x8B];
    slice.starts_with(&GZIP_MAGIC)
}

/// Gets gzip decompressed contents.
pub fn decompress_gzip_content(slice: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(slice);
    let mut decoded = vec![];
    decoder.read_to_end(&mut decoded)?;
    Ok(decoded)
}

#[cfg(windows)]
pub fn path_as_url(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[cfg(not(windows))]
pub fn path_as_url(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempfile_goes_away() -> io::Result<()> {
        let tempfile = TempFile::create()?;
        let path = tempfile.path().to_owned();
        assert!(
            path.exists(),
            "{} should exist after creating Tempfile",
            path.display()
        );

        drop(tempfile);
        assert!(!path.exists(), "File didn't get deleted");

        Ok(())
    }

    #[test]
    fn tempfile_goes_away_with_longer_living_handle() -> io::Result<()> {
        let tempfile = TempFile::create()?;
        let path = tempfile.path().to_owned();
        assert!(
            path.exists(),
            "{} should exist after creating Tempfile",
            path.display()
        );

        // Create a handle to the file that outlives the TempFile object (which means that
        // the `Drop` impl will run before our handle is closed).
        let handle = tempfile.open()?;
        drop(tempfile);

        drop(handle);
        assert!(!path.exists(), "{} didn't get deleted", path.display());

        Ok(())
    }

    #[test]
    fn sha1_checksums_power_of_two() {
        let data = b"this is some binary data for the test";
        let (total_sha, chunks) =
            get_sha1_checksums(data, 16).expect("Method should not fail because 16 is not zero");

        assert_eq!(
            total_sha.to_string(),
            "8e2f54f899107ad16af3f0bc8cc6e39a0fd9299e"
        );

        let chunks_str = chunks.iter().map(|c| c.to_string()).collect::<Vec<_>>();

        assert_eq!(
            chunks_str,
            vec![
                "aba4463482b4960f67a3b49ee5114b5d5e80bc28",
                "048509d362da6a10e180bf18c7c80752e3d4f44f",
                "81d55bec0f2bb3c521dcd40663cd525bb4808054"
            ]
        );
    }

    #[test]
    fn sha1_checksums_not_power_of_two() {
        let data = b"this is some binary data for the test";

        let (total_sha, chunks) =
            get_sha1_checksums(data, 17).expect("Method should not fail because 17 is not zero");

        assert_eq!(
            total_sha.to_string(),
            "8e2f54f899107ad16af3f0bc8cc6e39a0fd9299e"
        );

        let chunks_str = chunks.iter().map(|c| c.to_string()).collect::<Vec<_>>();

        assert_eq!(
            chunks_str,
            vec![
                "d84b7535763d088169943895014c8db840ee80bc",
                "7e65be6f54369a71b98aacf5fccc4daec1da6fe0",
                "665de1f2775ca0b64d3ceda7c1b4bd15e32a73ed"
            ]
        );
    }

    #[test]
    fn sha1_checksums_zero() {
        let data = b"this is some binary data for the test";
        get_sha1_checksums(data, 0).expect_err("Method should fail because 0 is zero");
    }
}
