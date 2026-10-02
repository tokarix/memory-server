//! Bounded, read-only input observations, including missing-file evidence.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::time::Instant;

use rustix::fs::{AtFlags, Mode, OFlags, Stat, StatxFlags, fstat, fstatfs, openat, statx};
use sha2::{Digest, Sha256};

use super::{Failure, paths};

/// Maximum retained file/executable/directory input observations. Different
/// directory protocols at one path each consume an observation slot.
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

    /// Observe a bounded directory listing for deterministic member globs.
    /// The default denies; read-only providers never implicitly gain listing.
    ///
    /// # Errors
    /// Unsupported, changed, inaccessible or ambiguous directory inputs deny.
    fn directory(&self, _path: &Path, _deadline: Instant) -> Result<Observation, Failure> {
        Err(Failure::Read)
    }

    /// Observe an output directory, including confirmed absence. Present entries
    /// must be real directories or regular files on the directory's own mount.
    /// Symlinks, special files and mounted individual output files deny: storage
    /// v1 assesses directories and cannot certify their individual file backing.
    ///
    /// # Errors
    /// Unsupported providers, indirections, missing mount IDs or failed reads deny.
    fn output_directory(&self, _path: &Path, _deadline: Instant) -> Result<Observation, Failure> {
        Err(Failure::Read)
    }

    /// Probe an exact executable search path without reading or executing it.
    /// Present evidence means a regular executable file; checked trust/content
    /// identity remains the protected contract's responsibility. Unsupported
    /// providers deny, and devices/FIFOs/symlinks are never treated as absence.
    ///
    /// # Errors
    /// Unknown path components, inaccessibility and unsupported file kinds deny.
    fn executable(&self, _path: &Path, _deadline: Instant) -> Result<Observation, Failure> {
        Err(Failure::Read)
    }
}

/// A private basename and file kind from a retained directory observation.
pub struct DirectoryEntry {
    name: String,
    directory: bool,
}

impl DirectoryEntry {
    /// Literal UTF-8 basename; no separators or control characters are accepted.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Whether the entry is an actual directory, rather than a symlink.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.directory
    }
}

fn directory_bytes(entries: &[(String, bool)]) -> Result<Vec<u8>, Failure> {
    if entries.len() > 512 {
        return Err(Failure::Limit);
    }
    let mut bytes = Vec::new();
    let mut previous = None;
    for (name, directory) in entries {
        if name.is_empty()
            || name.len() > 4096
            || name.contains('/')
            || name.chars().any(char::is_control)
            || matches!(name.as_str(), "." | "..")
            || previous.is_some_and(|previous: &str| previous >= name.as_str())
        {
            return Err(Failure::Read);
        }
        bytes.push(if *directory { b'd' } else { b'f' });
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(b'\n');
        if bytes.len() > MAX_FILE_BYTES {
            return Err(Failure::Limit);
        }
        previous = Some(name.as_str());
    }
    Ok(bytes)
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

fn stamp_directory_identity(stat: &Stat, hash: &mut Sha256) {
    // Path-prefix identity does not include unrelated sibling timestamps.
    // Relevant absence changes are detected by walking the exact path again;
    // glob directories additionally retain their full listing and final stamp.
    for value in [stat.st_dev, stat.st_ino, u64::from(stat.st_mode)] {
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
        observe_file(path, deadline, false)
    }

    fn executable(&self, path: &Path, deadline: Instant) -> Result<Observation, Failure> {
        observe_file(path, deadline, true)
    }

    fn directory(&self, path: &Path, deadline: Instant) -> Result<Observation, Failure> {
        observe_directory(path, deadline, false)
    }

    fn output_directory(&self, path: &Path, deadline: Instant) -> Result<Observation, Failure> {
        observe_directory(path, deadline, true)
    }
}

fn observe_file(path: &Path, deadline: Instant, executable: bool) -> Result<Observation, Failure> {
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
        stamp_directory_identity(&fstat(&current).map_err(|_| Failure::Read)?, &mut hash);
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
            if executable {
                check_filesystem(&current)?;
                let stat = fstat(&current).map_err(|_| Failure::Read)?;
                if stat.st_mode & libc::S_IFMT != libc::S_IFREG || stat.st_mode & 0o111 == 0 {
                    return Err(Failure::Executable);
                }
                stamp(&stat, &mut hash);
                tick(deadline)?;
                return Observation::present(Vec::new(), hash.finalize().into());
            }
            return read_regular(&current, hash, deadline);
        }
    }
    Err(Failure::Limit)
}

fn mount_id(file: &File) -> Result<u64, Failure> {
    let stat =
        statx(file, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID).map_err(|_| Failure::Read)?;
    if stat.stx_mask & StatxFlags::MNT_ID.bits() == 0 || stat.stx_mnt_id == 0 {
        return Err(Failure::Read);
    }
    Ok(stat.stx_mnt_id)
}

fn output_kind(mode: u32, mount: u64, parent_mount: u64) -> Result<bool, Failure> {
    match mode & libc::S_IFMT {
        libc::S_IFDIR => Ok(true),
        libc::S_IFREG if mount == parent_mount => Ok(false),
        _ => Err(Failure::Read),
    }
}

fn output_entry(
    parent: &File,
    name: &str,
    parent_mount: u64,
    deadline: Instant,
    identity: &mut Sha256,
) -> Result<bool, Failure> {
    tick(deadline)?;
    let file = File::from(
        openat(
            parent,
            name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Failure::Read)?,
    );
    let stat = fstat(&file).map_err(|_| Failure::Read)?;
    let mount = mount_id(&file)?;
    let directory = output_kind(stat.st_mode, mount, parent_mount)?;
    if directory {
        stamp_directory_identity(&stat, identity);
    } else {
        stamp(&stat, identity);
    }
    identity.update(name.len().to_be_bytes());
    identity.update(name.as_bytes());
    identity.update(mount.to_be_bytes());
    Ok(directory)
}

fn observe_directory(path: &Path, deadline: Instant, output: bool) -> Result<Observation, Failure> {
    paths::validate(path)?;
    if !path.is_absolute() {
        return Err(Failure::Read);
    }
    let mut current = File::from(
        openat(
            rustix::fs::CWD,
            "/",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Failure::Read)?,
    );
    let mut identity = Sha256::new();
    identity.update(if output {
        b"memory-hooks-rust-output-directory-v1\0".as_slice()
    } else {
        b"memory-hooks-rust-directory-v1\0"
    });
    for (index, part) in path.components().enumerate() {
        tick(deadline)?;
        if index >= 128 {
            return Err(Failure::Limit);
        }
        check_filesystem(&current)?;
        stamp_directory_identity(&fstat(&current).map_err(|_| Failure::Read)?, &mut identity);
        if matches!(part, Component::RootDir) {
            continue;
        }
        current = match openat(
            &current,
            part.as_os_str(),
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(file) => File::from(file),
            Err(rustix::io::Errno::NOENT) => {
                return Ok(Observation::missing(identity.finalize().into()));
            }
            Err(_) => return Err(Failure::Read),
        };
    }
    check_filesystem(&current)?;
    let before = fstat(&current).map_err(|_| Failure::Read)?;
    let mut entries = Vec::new();
    if output {
        identity.update(mount_id(&current)?.to_be_bytes());
    }
    // This kernel fd link refers only to the checked directory descriptor.
    for entry in std::fs::read_dir(format!("/proc/self/fd/{}", current.as_raw_fd()))
        .map_err(|_| Failure::Read)?
    {
        tick(deadline)?;
        let entry = entry.map_err(|_| Failure::Read)?;
        let name = entry.file_name().into_string().map_err(|_| Failure::Read)?;
        let directory = entry.file_type().map_err(|_| Failure::Read)?.is_dir();
        entries.push((name, directory));
        if entries.len() > 512 {
            return Err(Failure::Limit);
        }
    }
    entries.sort_unstable();
    if output {
        let mount = mount_id(&current)?;
        for (name, directory) in &mut entries {
            *directory = output_entry(&current, name, mount, deadline, &mut identity)?;
        }
    }
    let mut original = Sha256::new();
    stamp(&before, &mut original);
    let mut after = Sha256::new();
    stamp(&fstat(&current).map_err(|_| Failure::Read)?, &mut after);
    if original.finalize() != after.finalize() {
        return Err(Failure::Changed);
    }
    stamp(&before, &mut identity);
    tick(deadline)?;
    Observation::present(directory_bytes(&entries)?, identity.finalize().into())
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
    files: BTreeMap<(PathBuf, ReadKind), Observation>,
    total_bytes: usize,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
enum ReadKind {
    File,
    Directory,
    Executable,
    OutputDirectory,
}

fn decode_directory(bytes: &[u8]) -> Result<Vec<DirectoryEntry>, Failure> {
    let text = std::str::from_utf8(bytes).map_err(|_| Failure::Read)?;
    if !text.is_empty() && !text.ends_with('\n') {
        return Err(Failure::Read);
    }
    if text.lines().count() > 512 {
        return Err(Failure::Limit);
    }
    let entries: Vec<_> = text
        .lines()
        .map(|line| {
            let kind = line.as_bytes().first().ok_or(Failure::Read)?;
            if !matches!(kind, b'd' | b'f') {
                return Err(Failure::Read);
            }
            Ok((
                line.get(1..).ok_or(Failure::Read)?.to_owned(),
                *kind == b'd',
            ))
        })
        .collect::<Result<_, Failure>>()?;
    directory_bytes(&entries)?;
    Ok(entries
        .into_iter()
        .map(|(name, directory)| DirectoryEntry { name, directory })
        .collect())
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
        self.retain(path, ReadKind::File)
    }

    /// Retain a directory listing in the same file/byte/deadline budget.
    /// Directory replacements and new glob matches enter the final recheck.
    ///
    /// # Errors
    /// Missing, nonregular, malformed, unsorted or oversized listings deny.
    pub fn directory(&mut self, path: &Path) -> Result<Vec<DirectoryEntry>, Failure> {
        let bytes = self
            .retain(path, ReadKind::Directory)?
            .ok_or(Failure::Read)?;
        decode_directory(bytes)
    }

    /// Retain a safe output listing or complete absence in the shared budget.
    ///
    /// # Errors
    /// Unsupported file backing/kinds, failed probes, ambiguity and overflow deny.
    pub fn output_directory(
        &mut self,
        path: &Path,
    ) -> Result<Option<Vec<DirectoryEntry>>, Failure> {
        self.retain(path, ReadKind::OutputDirectory)?
            .map(decode_directory)
            .transpose()
    }

    /// Retain present/absent executable search evidence in the shared budget.
    ///
    /// # Errors
    /// Failed probes, changed read kinds, overflow and late completion deny.
    pub fn executable(&mut self, path: &Path) -> Result<bool, Failure> {
        Ok(self.retain(path, ReadKind::Executable)?.is_some())
    }

    fn retain(&mut self, path: &Path, kind: ReadKind) -> Result<Option<&[u8]>, Failure> {
        paths::validate(path)?;
        tick(self.deadline)?;
        let key = (path.to_owned(), kind);
        if self.files.keys().any(|(observed_path, observed_kind)| {
            observed_path == path
                && *observed_kind != kind
                && !matches!(
                    (*observed_kind, kind),
                    (ReadKind::Directory, ReadKind::OutputDirectory)
                        | (ReadKind::OutputDirectory, ReadKind::Directory)
                )
        }) {
            return Err(Failure::Read);
        }
        if !self.files.contains_key(&key) {
            if self.files.len() >= MAX_FILES {
                return Err(Failure::Limit);
            }
            let observation = match kind {
                ReadKind::Directory => self.provider.directory(path, self.deadline)?,
                ReadKind::File => self.provider.observe(path, self.deadline)?,
                ReadKind::Executable => self.provider.executable(path, self.deadline)?,
                ReadKind::OutputDirectory => self.provider.output_directory(path, self.deadline)?,
            };
            tick(self.deadline)?;
            let count = observation.bytes.as_ref().map_or(0, Vec::len);
            if count > MAX_FILE_BYTES || self.total_bytes + count > MAX_TOTAL_BYTES {
                return Err(Failure::Limit);
            }
            self.total_bytes += count;
            self.files.insert(key.clone(), observation);
        }
        let item = self.files.get(&key).ok_or(Failure::Read)?;
        Ok(item.bytes.as_deref())
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
        for ((path, kind), expected) in &self.files {
            tick(self.deadline)?;
            let observed = match kind {
                ReadKind::Directory => self.provider.directory(path, self.deadline)?,
                ReadKind::File => self.provider.observe(path, self.deadline)?,
                ReadKind::Executable => self.provider.executable(path, self.deadline)?,
                ReadKind::OutputDirectory => self.provider.output_directory(path, self.deadline)?,
            };
            if observed != *expected {
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
        hash.update(b"memory-hooks-rust-read-set-v4\0");
        for ((path, kind), item) in &self.files {
            let bytes = path.as_os_str().as_encoded_bytes();
            hash.update(bytes.len().to_be_bytes());
            hash.update(bytes);
            hash.update([match kind {
                ReadKind::File => 0,
                ReadKind::Directory => 1,
                ReadKind::Executable => 2,
                ReadKind::OutputDirectory => 3,
            }]);
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
        pub directories: RefCell<BTreeMap<PathBuf, Observation>>,
    }

    impl Scripted {
        pub fn list(&self, path: &str, entries: &[(&str, bool)]) {
            let mut entries: Vec<_> = entries
                .iter()
                .map(|(name, directory)| ((*name).to_owned(), *directory))
                .collect();
            entries.sort_unstable();
            self.directories.borrow_mut().insert(
                PathBuf::from(path),
                Observation::present(super::directory_bytes(&entries).unwrap(), [1; 32]).unwrap(),
            );
        }
        pub fn put(&self, path: &str, text: &str) {
            self.files.borrow_mut().insert(
                PathBuf::from(path),
                Observation::present(text.as_bytes().to_vec(), [1; 32]).unwrap(),
            );
        }
    }

    impl ReadProvider for Scripted {
        fn output_directory(
            &self,
            path: &Path,
            _deadline: Instant,
        ) -> Result<Observation, Failure> {
            self.calls.borrow_mut().push(path.to_owned());
            Ok(self
                .directories
                .borrow()
                .get(path)
                .cloned()
                .unwrap_or_else(|| Observation::missing([0; 32])))
        }
        fn executable(&self, path: &Path, deadline: Instant) -> Result<Observation, Failure> {
            self.observe(path, deadline)
        }
        fn directory(&self, path: &Path, _deadline: Instant) -> Result<Observation, Failure> {
            self.calls.borrow_mut().push(path.to_owned());
            self.directories
                .borrow()
                .get(path)
                .cloned()
                .ok_or(Failure::Read)
        }
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
    fn output_file_kind_and_mount_proofs_do_not_inherit_directory_backing() {
        assert_eq!(super::output_kind(libc::S_IFDIR | 0o700, 2, 1), Ok(true));
        assert_eq!(super::output_kind(libc::S_IFREG | 0o600, 1, 1), Ok(false));
        assert_eq!(
            super::output_kind(libc::S_IFREG | 0o600, 2, 1),
            Err(Failure::Read)
        );
        for kind in [
            libc::S_IFLNK,
            libc::S_IFIFO,
            libc::S_IFCHR,
            libc::S_IFBLK,
            libc::S_IFSOCK,
        ] {
            assert_eq!(super::output_kind(kind | 0o600, 1, 1), Err(Failure::Read));
        }
    }

    #[test]
    fn output_probes_do_not_read_large_files_follow_symlinks_or_open_fifos() {
        let end = deadline();
        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let directory = tempfile::tempdir_in(base).unwrap();
        fs::write(
            directory.path().join("large"),
            vec![b'X'; MAX_FILE_BYTES + 1],
        )
        .unwrap();
        fs::create_dir(directory.path().join("nested")).unwrap();
        let mut reads = ReadSet::new(&Filesystem, deadline());
        let entries = reads.output_directory(directory.path()).unwrap().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.name() == "nested" && entry.is_directory())
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.name() == "large" && !entry.is_directory())
        );
        reads.recheck().unwrap();
        let missing = directory.path().join("future").join("output");
        assert!(reads.output_directory(&missing).unwrap().is_none());
        fs::create_dir_all(&missing).unwrap();
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        symlink(
            directory.path().join("nested"),
            directory.path().join("alias"),
        )
        .unwrap();
        assert_eq!(
            Filesystem
                .output_directory(directory.path(), deadline())
                .err(),
            Some(Failure::Read)
        );
        fs::remove_file(directory.path().join("alias")).unwrap();
        mkfifoat(
            rustix::fs::CWD,
            directory.path().join("fifo"),
            Mode::from_bits_truncate(0o600),
        )
        .unwrap();
        assert_eq!(
            Filesystem
                .output_directory(directory.path(), deadline())
                .err(),
            Some(Failure::Read)
        );
        assert!(Instant::now() < end);
    }

    #[test]
    fn source_and_output_directory_observations_coexist_without_kind_substitution() {
        let provider = Scripted::default();
        provider.list("/work", &[("package", true)]);
        let mut reads = ReadSet::new(&provider, deadline());
        assert_eq!(reads.directory(Path::new("/work")).unwrap().len(), 1);
        assert_eq!(
            reads
                .output_directory(Path::new("/work"))
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(reads.read(Path::new("/work")), Err(Failure::Read));
        reads.recheck().unwrap();
        provider.list("/work", &[("package", true), ("new", true)]);
        assert_eq!(reads.recheck(), Err(Failure::Changed));
    }

    #[test]
    fn executable_probe_is_metadata_only_bounded_and_rechecks_absence() {
        let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
        let directory = tempfile::tempdir_in(base).unwrap();
        let executable = directory.path().join("compiler");
        fs::write(&executable, vec![b'X'; MAX_FILE_BYTES + 1]).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut reads = ReadSet::new(&Filesystem, deadline());
        assert!(reads.executable(&executable).unwrap());
        reads.recheck().unwrap();
        // Probe never reads the oversized file as config or launches it.
        assert_eq!(
            Filesystem.observe(&executable, deadline()).err(),
            Some(Failure::Limit)
        );
        assert!(!directory.path().join("marker").exists());
        let absent = directory.path().join("earlier-compiler");
        assert!(!reads.executable(&absent).unwrap());
        fs::write(&absent, "SECRET_SENTINEL").unwrap();
        fs::set_permissions(&absent, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        for (name, mode) in [("plain", 0o600), ("fifo", 0o700)] {
            let path = directory.path().join(name);
            if name == "fifo" {
                mkfifoat(rustix::fs::CWD, &path, Mode::from_bits_truncate(mode)).unwrap();
            } else {
                fs::write(&path, "SECRET_SENTINEL").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            }
            assert!(Filesystem.executable(&path, deadline()).is_err());
        }
        let alias = directory.path().join("alias");
        symlink(&executable, &alias).unwrap();
        assert!(Filesystem.executable(&alias, deadline()).is_err());
        assert!(
            Filesystem
                .executable(&directory.path().join("alias/../compiler"), deadline())
                .is_err()
        );
    }

    #[test]
    fn directory_inputs_share_bounds_and_recheck_new_or_replaced_members() {
        let provider = Scripted::default();
        provider.list("/work/crates", &[("a", true), ("sentinel", false)]);
        let mut reads = ReadSet::new(&provider, deadline());
        let entries = reads.directory(Path::new("/work/crates")).unwrap();
        assert_eq!(entries[0].name(), "a");
        assert!(entries[0].is_directory());
        assert!(!entries[1].is_directory());
        assert_eq!(reads.read(Path::new("/work/crates")), Err(Failure::Read));
        reads.recheck().unwrap();
        provider.list(
            "/work/crates",
            &[("a", true), ("new", true), ("sentinel", false)],
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        let mut reads = ReadSet::new(&provider, deadline());
        for index in 0..MAX_FILES {
            reads.read(Path::new(&format!("/work/{index}"))).unwrap();
        }
        assert!(matches!(
            reads.directory(Path::new("/work/crates")),
            Err(Failure::Limit)
        ));
        for bytes in [
            b"db\nda\n".as_slice(),
            b"da/escape\n",
            b"sa\n",
            b"da",
            b"d..\n",
        ] {
            provider.directories.borrow_mut().insert(
                Path::new("/work/crates").to_owned(),
                Observation::present(bytes.to_vec(), [0; 32]).unwrap(),
            );
            let mut reads = ReadSet::new(&provider, deadline());
            assert!(matches!(
                reads.directory(Path::new("/work/crates")),
                Err(Failure::Read)
            ));
        }
    }

    #[test]
    fn real_directory_listing_is_bounded_and_never_follows_a_member_symlink() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join("a")).unwrap();
        symlink(fixture.path().join("a"), fixture.path().join("b")).unwrap();
        let provider = Filesystem;
        let mut reads = ReadSet::new(&provider, deadline());
        let entries = reads.directory(fixture.path()).unwrap();
        assert_eq!(entries[0].name(), "a");
        assert!(entries[0].is_directory());
        assert_eq!(entries[1].name(), "b");
        assert!(!entries[1].is_directory());
        reads.recheck().unwrap();
        fs::create_dir(fixture.path().join("new")).unwrap();
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        assert!(matches!(
            provider.directory(&fixture.path().join("b"), deadline()),
            Err(Failure::Read)
        ));
        assert!(matches!(
            provider.directory(Path::new("/proc/self"), deadline()),
            Err(Failure::Read)
        ));
        assert!(!fixture.path().join("marker").exists());
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
        fs::write(fixture.path().join("unrelated"), b"unrelated sibling").unwrap();
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
