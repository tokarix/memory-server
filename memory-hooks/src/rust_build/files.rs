//! Bounded, read-only input observations, including missing-file evidence.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use rustix::fs::{Mode, OFlags, Stat, fstat, fstatfs, openat};
use sha2::{Digest, Sha256};

use super::{Failure, paths};

/// Maximum number of distinct config/manifest/selector input paths.
pub const MAX_FILES: usize = 64;
/// Maximum bytes in one regular input file.
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// Maximum combined bytes retained across the operation's read set.
pub const MAX_TOTAL_BYTES: usize = 1024 * 1024;
/// Maximum recursive TOML/workspace processing depth.
pub const MAX_DEPTH: usize = 16;

/// Private input contents and identity. Deliberately has no Debug/Serialize.
#[derive(Clone, Eq, PartialEq)]
pub struct Observation {
    bytes: Option<Vec<u8>>,
    identity: [u8; 32],
}

impl Observation {
    /// Construct evidence from a provider's regular-file identity and contents.
    /// This is also the deterministic test-provider boundary.
    ///
    /// # Errors
    /// Oversized contents cannot be accepted, even from a scripted provider.
    pub fn present(bytes: Vec<u8>, identity: [u8; 32]) -> Result<Self, Failure> {
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Failure::Limit);
        }
        Ok(Self {
            bytes: Some(bytes),
            identity,
        })
    }

    /// Construct absence evidence tied to the observed existing path prefix.
    #[must_use]
    pub const fn missing(identity: [u8; 32]) -> Self {
        Self {
            bytes: None,
            identity,
        }
    }
}

/// Filesystem-read-only boundary. No process, write or discovery methods exist.
/// The production gate chooses its provider; clients cannot select one.
pub trait ReadProvider {
    /// Observe a bounded regular file or confirmed missing path, before deadline.
    ///
    /// # Errors
    /// Inaccessibility, nonregular inputs and unsupported indirections deny.
    fn observe(&self, path: &Path, deadline: Instant) -> Result<Observation, Failure>;
}

/// Real Linux provider. Symlink components and pseudo-filesystems are denied.
/// A checked regular descriptor is reopened through the kernel's own fd link;
/// no untrusted device/FIFO or arbitrary proc path is opened for reading.
pub struct Filesystem;

fn tick(deadline: Instant) -> Result<(), Failure> {
    if Instant::now() >= deadline {
        return Err(Failure::Deadline);
    }
    Ok(())
}

fn stamp(stat: &Stat, hash: &mut Sha256) {
    for value in [
        stat.st_dev,
        stat.st_ino,
        u64::from(stat.st_mode),
        stat.st_size.cast_unsigned(),
        stat.st_mtime.cast_unsigned(),
        stat.st_mtime_nsec,
        stat.st_ctime.cast_unsigned(),
        stat.st_ctime_nsec,
    ] {
        hash.update(value.to_be_bytes());
    }
}

fn check_filesystem(file: &File) -> Result<(), Failure> {
    let filesystem = fstatfs(file).map_err(|_| Failure::Read)?;
    if matches!(
        filesystem.f_type,
        libc::PROC_SUPER_MAGIC | libc::SYSFS_MAGIC | libc::DEBUGFS_MAGIC | libc::TRACEFS_MAGIC
    ) {
        return Err(Failure::Read);
    }
    Ok(())
}

impl ReadProvider for Filesystem {
    fn observe(&self, path: &Path, deadline: Instant) -> Result<Observation, Failure> {
        paths::validate(path)?;
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(Failure::Read);
        }
        tick(deadline)?;
        let mut current = File::from(
            openat(
                rustix::fs::CWD,
                "/",
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Failure::Read)?,
        );
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-rust-input-identity-v1\0");
        let mut parts = path
            .components()
            .filter(|part| !matches!(part, Component::RootDir))
            .peekable();
        for _ in 0..128 {
            tick(deadline)?;
            check_filesystem(&current)?;
            stamp(&fstat(&current).map_err(|_| Failure::Read)?, &mut hash);
            let Some(part) = parts.next() else {
                return Err(Failure::Read);
            };
            let final_file = parts.peek().is_none();
            let mut flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            if !final_file {
                flags |= OFlags::DIRECTORY;
            }
            current = match openat(&current, part.as_os_str(), flags, Mode::empty()) {
                Ok(descriptor) => File::from(descriptor),
                Err(rustix::io::Errno::NOENT) => {
                    return Ok(Observation::missing(hash.finalize().into()));
                }
                Err(_) => return Err(Failure::Read),
            };
            if final_file {
                return read_regular(&current, hash, deadline);
            }
        }
        Err(Failure::Limit)
    }
}

fn read_regular(
    descriptor: &File,
    mut identity: Sha256,
    deadline: Instant,
) -> Result<Observation, Failure> {
    tick(deadline)?;
    check_filesystem(descriptor)?;
    let before = fstat(descriptor).map_err(|_| Failure::Read)?;
    if before.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(Failure::Read);
    }
    if before.st_size < 0 || before.st_size.cast_unsigned() > MAX_FILE_BYTES as u64 {
        return Err(Failure::Limit);
    }
    let mut file = File::open(format!("/proc/self/fd/{}", descriptor.as_raw_fd()))
        .map_err(|_| Failure::Read)?;
    let mut reopened = Sha256::new();
    stamp(&fstat(&file).map_err(|_| Failure::Read)?, &mut reopened);
    let mut expected = Sha256::new();
    stamp(&before, &mut expected);
    if reopened.finalize() != expected.clone().finalize() {
        return Err(Failure::Changed);
    }
    let mut bytes = Vec::new();
    let mut block = [0_u8; 8192];
    loop {
        tick(deadline)?;
        let count = file.read(&mut block).map_err(|_| Failure::Read)?;
        if count == 0 {
            break;
        }
        if bytes.len() + count > MAX_FILE_BYTES {
            return Err(Failure::Limit);
        }
        bytes.extend_from_slice(&block[..count]);
    }
    let mut after = Sha256::new();
    stamp(&fstat(&file).map_err(|_| Failure::Read)?, &mut after);
    if after.finalize() != expected.finalize() {
        return Err(Failure::Changed);
    }
    stamp(&before, &mut identity);
    Observation::present(bytes, identity.finalize().into())
}

/// Private complete read set; it binds contents and absences to one deadline.
pub struct ReadSet<'a, P: ReadProvider> {
    provider: &'a P,
    deadline: Instant,
    files: BTreeMap<PathBuf, Observation>,
    total_bytes: usize,
}

impl<'a, P: ReadProvider> ReadSet<'a, P> {
    /// Start one operation. A caller must not extend the deadline on retry.
    #[must_use]
    pub fn new(provider: &'a P, deadline: Instant) -> Self {
        Self {
            provider,
            deadline,
            files: BTreeMap::new(),
            total_bytes: 0,
        }
    }

    /// Retain regular-file contents or absence evidence, deduplicating by path.
    ///
    /// # Errors
    /// Any read failure, overflow or late completion denies without truncation.
    pub fn read(&mut self, path: &Path) -> Result<Option<&[u8]>, Failure> {
        paths::validate(path)?;
        tick(self.deadline)?;
        if !self.files.contains_key(path) {
            if self.files.len() >= MAX_FILES {
                return Err(Failure::Limit);
            }
            let observation = self.provider.observe(path, self.deadline)?;
            tick(self.deadline)?;
            let count = observation.bytes.as_ref().map_or(0, Vec::len);
            if count > MAX_FILE_BYTES || self.total_bytes + count > MAX_TOTAL_BYTES {
                return Err(Failure::Limit);
            }
            self.total_bytes += count;
            self.files.insert(path.to_owned(), observation);
        }
        Ok(self.files.get(path).and_then(|item| item.bytes.as_deref()))
    }

    /// Check the shared deadline during non-I/O configuration processing.
    ///
    /// # Errors
    /// Late or cancelled-by-deadline analysis cannot complete successfully.
    pub fn checkpoint(&self) -> Result<(), Failure> {
        tick(self.deadline)
    }

    /// Reobserve every input immediately before the final gate decision.
    /// Includes ignored lower-precedence files and both candidate filenames.
    ///
    /// # Errors
    /// Changes, newly created files, replacement, failed reads and lateness deny.
    pub fn recheck(&self) -> Result<(), Failure> {
        for (path, expected) in &self.files {
            tick(self.deadline)?;
            if self.provider.observe(path, self.deadline)? != *expected {
                return Err(Failure::Changed);
            }
        }
        tick(self.deadline)
    }

    /// Domain-separated digest of private paths, identities, contents/absences.
    /// Hashes are not encryption; low-entropy inputs remain guessable.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-rust-read-set-v1\0");
        for (path, item) in &self.files {
            let bytes = path.as_os_str().as_encoded_bytes();
            hash.update(bytes.len().to_be_bytes());
            hash.update(bytes);
            hash.update(item.identity);
            hash.update([u8::from(item.bytes.is_some())]);
            if let Some(bytes) = &item.bytes {
                hash.update(bytes.len().to_be_bytes());
                hash.update(bytes);
            }
        }
        hash.finalize().into()
    }
}

#[cfg(test)]
pub(super) mod fixtures {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::{Failure, Observation, ReadProvider};

    #[derive(Default)]
    pub struct Scripted {
        pub files: RefCell<BTreeMap<PathBuf, Observation>>,
        pub calls: RefCell<Vec<PathBuf>>,
    }

    impl Scripted {
        pub fn put(&self, path: &str, text: &str) {
            self.files.borrow_mut().insert(
                PathBuf::from(path),
                Observation::present(text.as_bytes().to_vec(), [1; 32]).unwrap(),
            );
        }
    }

    impl ReadProvider for Scripted {
        fn observe(&self, path: &Path, _deadline: Instant) -> Result<Observation, Failure> {
            self.calls.borrow_mut().push(path.to_owned());
            Ok(self
                .files
                .borrow()
                .get(path)
                .cloned()
                .unwrap_or_else(|| Observation::missing([0; 32])))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use rustix::fs::{Mode, mkfifoat};

    use super::fixtures::Scripted;
    use super::{
        Failure, Filesystem, MAX_FILE_BYTES, MAX_FILES, Observation, ReadProvider, ReadSet,
    };

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    #[test]
    fn retained_absences_content_and_identity_are_rechecked() {
        let provider = Scripted::default();
        provider.put("/work/config", "secret-sentinel");
        let mut reads = ReadSet::new(&provider, deadline());
        assert_eq!(
            reads.read(Path::new("/work/config")).unwrap(),
            Some(b"secret-sentinel".as_slice())
        );
        assert_eq!(reads.read(Path::new("/work/config.toml")).unwrap(), None);
        let digest = reads.digest();
        reads.recheck().unwrap();
        provider.put("/work/config.toml", "new higher precedence");
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        provider
            .files
            .borrow_mut()
            .remove(Path::new("/work/config.toml"));
        provider.files.borrow_mut().insert(
            Path::new("/work/config").to_owned(),
            Observation::present(b"secret-sentinel".to_vec(), [2; 32]).unwrap(),
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        provider.put("/work/config", "replacement-content");
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        assert_eq!(reads.digest(), digest);
        assert!(!format!("{:?}", reads.recheck()).contains("secret-sentinel"));
    }

    #[test]
    fn every_bound_and_late_completion_fails_without_prefix_acceptance() {
        let provider = Scripted::default();
        let mut reads = ReadSet::new(&provider, deadline());
        for index in 0..MAX_FILES {
            assert_eq!(
                reads.read(Path::new(&format!("/work/{index}"))).unwrap(),
                None
            );
        }
        assert_eq!(reads.read(Path::new("/work/overflow")), Err(Failure::Limit));
        let mut reads = ReadSet::new(&provider, deadline());
        for index in 0..5 {
            provider.files.borrow_mut().insert(
                Path::new(&format!("/work/{index}")).to_owned(),
                Observation::present(vec![b'x'; MAX_FILE_BYTES], [1; 32]).unwrap(),
            );
        }
        for index in 0..4 {
            reads.read(Path::new(&format!("/work/{index}"))).unwrap();
        }
        assert_eq!(reads.read(Path::new("/work/4")), Err(Failure::Limit));
        assert!(matches!(
            Observation::present(vec![0; MAX_FILE_BYTES + 1], [1; 32]),
            Err(Failure::Limit)
        ));
        let mut expired = ReadSet::new(&provider, Instant::now());
        assert_eq!(expired.read(Path::new("/work/0")), Err(Failure::Deadline));
        assert_eq!(expired.recheck(), Err(Failure::Deadline));

        let mut reads = ReadSet::new(&Late, Instant::now() + Duration::from_millis(1));
        assert_eq!(reads.read(Path::new("/work/file")), Err(Failure::Deadline));
    }

    #[test]
    fn real_provider_reads_only_checked_regular_files_and_detects_creation() {
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("config");
        fs::write(&file, b"secret-sentinel").unwrap();
        let provider = Filesystem;
        let mut reads = ReadSet::new(&provider, deadline());
        assert_eq!(
            reads.read(&file).unwrap(),
            Some(b"secret-sentinel".as_slice())
        );
        reads.read(&fixture.path().join("missing/config")).unwrap();
        reads.recheck().unwrap();
        fs::create_dir(fixture.path().join("missing")).unwrap();
        assert_eq!(reads.recheck(), Err(Failure::Changed));

        let link = fixture.path().join("link");
        symlink(&file, &link).unwrap();
        assert!(matches!(
            provider.observe(&link, deadline()),
            Err(Failure::Read)
        ));
        assert!(matches!(
            provider.observe(fixture.path(), deadline()),
            Err(Failure::Read)
        ));
        let fifo = fixture.path().join("fifo");
        mkfifoat(rustix::fs::CWD, &fifo, Mode::RUSR | Mode::WUSR).unwrap();
        assert!(matches!(
            provider.observe(&fifo, deadline()),
            Err(Failure::Read)
        ));
        assert!(matches!(
            provider.observe(Path::new("/proc/self/stat"), deadline()),
            Err(Failure::Read)
        ));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o0)).unwrap();
        // CI runs as root with DAC override; mode bits alone do not prove
        // inaccessibility. Compare with the effective reader's actual access.
        match fs::File::open(&file) {
            Ok(_) => {
                let observation = provider.observe(&file, deadline()).unwrap();
                assert_eq!(
                    observation.bytes.as_deref(),
                    Some(b"secret-sentinel".as_slice())
                );
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
                assert!(matches!(
                    provider.observe(&file, deadline()),
                    Err(Failure::Read)
                ));
            }
        }
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&file, vec![0; MAX_FILE_BYTES + 1]).unwrap();
        assert!(matches!(
            provider.observe(&file, deadline()),
            Err(Failure::Limit)
        ));
        assert!(!fixture.path().join("marker").exists());
    }

    struct Late;
    impl ReadProvider for Late {
        fn observe(&self, _path: &Path, deadline: Instant) -> Result<Observation, Failure> {
            std::thread::sleep(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
            );
            Ok(Observation::missing([0; 32]))
        }
    }
}
