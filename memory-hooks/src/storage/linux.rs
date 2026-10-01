//! Local Linux kernel observations and conservative backing classification.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{
    Access, AtFlags, Mode, OFlags, StatxFlags, accessat, fstat, fstatfs, major, minor, openat,
    readlinkat_raw, statx,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::attestation::StorageExecution;
use super::evidence::hash;
use super::path::{Resolved, normalized};
use super::{
    BackingClass, Budget, Candidate, CandidateRole, ConstraintKind, Entry, Evidence,
    FilesystemClass, ObjectIdentity, Outcome, ProbeError, ProbeResult, Provider, Reason,
    StorageRequirement,
};

pub(super) const MOUNT_BYTES: usize = 1024 * 1024;
pub(super) const MOUNT_ROWS: usize = 4096;
pub(super) const ROW_BYTES: usize = 16 * 1024;
pub(super) const BACKING_DEPTH: usize = 16;
pub(super) const BACKING_NODES: usize = 128;
const RAMFS_MAGIC: rustix::fs::FsWord = 0x8584_58f6;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Mount {
    pub(super) id: u64,
    parent: u64,
    major: u32,
    minor: u32,
    root: PathBuf,
    pub(super) mountpoint: PathBuf,
    options: Vec<u8>,
    super_options: Vec<u8>,
    filesystem: Vec<u8>,
    row_hash: String,
}

#[derive(Clone, Eq, PartialEq, Serialize)]
struct Execution {
    namespace: ObjectIdentity,
    root: ObjectIdentity,
    boot: String,
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct Snapshot {
    execution: Execution,
    pub(super) mounts: BTreeMap<u64, Mount>,
}

#[cfg(test)]
impl Snapshot {
    pub(super) fn fixture(bytes: &[u8]) -> Self {
        Self {
            execution: Execution {
                namespace: ObjectIdentity {
                    device: 9,
                    inode: 10,
                    mode: 0,
                },
                root: ObjectIdentity {
                    device: 1,
                    inode: 1,
                    mode: libc::S_IFDIR,
                },
                boot: "00000000-0000-0000-0000-000000000001".to_owned(),
            },
            mounts: mountinfo(bytes).expect("fixture mountinfo"),
        }
    }
}

enum Observed {
    File(PathBuf, Vec<u8>),
    Directory(PathBuf, Vec<OsString>),
    Canonical(PathBuf, PathBuf),
    Absent(PathBuf),
    Object(PathBuf, ObjectIdentity),
}

pub(super) struct Linux {
    root: Arc<OwnedFd>,
    namespace: Arc<OwnedFd>,
    observed: Vec<Observed>,
    nodes: usize,
    btrfs_sets: Vec<BtrfsSet>,
}

#[derive(Clone)]
struct BtrfsSet {
    handle: Arc<OwnedFd>,
    devices: Vec<(u32, u32)>,
}

pub(super) trait Kernel: Provider {
    fn backing_hash(&mut self, budget: &mut Budget) -> ProbeResult<String>;
    fn correlate(
        &mut self,
        snapshot: &Snapshot,
        resolved: &Resolved<Self::Handle>,
        budget: &mut Budget,
    ) -> ProbeResult<(Mount, FilesystemClass)>;
    fn backing(
        &mut self,
        handle: &Self::Handle,
        mount: &Mount,
        filesystem: FilesystemClass,
        budget: &mut Budget,
    ) -> ProbeResult<BackingClass>;
    fn mount_identity(&mut self, handle: &Self::Handle, budget: &mut Budget) -> ProbeResult<u64>;
    fn recheck(&mut self, initial: &Snapshot, budget: &mut Budget) -> ProbeResult<()>;
}

fn bounded_read(path: &Path, limit: usize, budget: &mut Budget) -> ProbeResult<Vec<u8>> {
    budget.tick()?;
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    budget.tick()?;
    file.take((limit + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(ProbeError::new(Reason::Limit));
    }
    Ok(bytes)
}

fn bounded_directory(path: &Path, budget: &mut Budget) -> ProbeResult<Vec<OsString>> {
    budget.tick()?;
    let mut names = Vec::new();
    for entry in fs::read_dir(path)? {
        budget.tick()?;
        let name = entry?.file_name();
        if name.as_bytes().len() > 256 || names.len() >= BACKING_NODES {
            return Err(ProbeError::new(Reason::Limit));
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn object(handle: &OwnedFd, budget: &mut Budget) -> ProbeResult<ObjectIdentity> {
    budget.tick()?;
    let stat = fstat(handle)?;
    Ok(ObjectIdentity {
        device: stat.st_dev,
        inode: stat.st_ino,
        mode: stat.st_mode,
    })
}

fn path_identity(path: &Path, budget: &mut Budget) -> ProbeResult<ObjectIdentity> {
    budget.tick()?;
    let metadata = fs::symlink_metadata(path)?;
    Ok(ObjectIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        mode: metadata.mode(),
    })
}

impl Linux {
    pub(super) fn new(budget: &mut Budget) -> ProbeResult<Self> {
        let credentials = bounded_read(Path::new("/proc/thread-self/status"), ROW_BYTES, budget)?;
        check_fs_credentials(&credentials)?;
        budget.tick()?;
        let root = openat(
            rustix::fs::CWD,
            "/proc/thread-self/root",
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        budget.tick()?;
        let namespace = openat(
            rustix::fs::CWD,
            "/proc/thread-self/ns/mnt",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(Self {
            root: Arc::new(root),
            namespace: Arc::new(namespace),
            observed: Vec::new(),
            nodes: 0,
            btrfs_sets: Vec::new(),
        })
    }

    pub(super) fn snapshot(&mut self, budget: &mut Budget) -> ProbeResult<Snapshot> {
        let boot = bounded_read(Path::new("/proc/sys/kernel/random/boot_id"), 64, budget)?;
        let boot = std::str::from_utf8(&boot)
            .map_err(|_| ProbeError::new(Reason::MountEvidence))?
            .trim();
        let boot =
            uuid::Uuid::parse_str(boot).map_err(|_| ProbeError::new(Reason::MountEvidence))?;
        if boot.is_nil() {
            return Err(ProbeError::new(Reason::MountEvidence));
        }
        let execution = Execution {
            namespace: object(&self.namespace, budget)?,
            root: object(&self.root, budget)?,
            boot: boot.to_string(),
        };
        let bytes = bounded_read(
            Path::new("/proc/thread-self/mountinfo"),
            MOUNT_BYTES,
            budget,
        )?;
        let mounts = mountinfo(&bytes)?;
        Ok(Snapshot { execution, mounts })
    }

    pub(super) fn recheck(&mut self, initial: &Snapshot, budget: &mut Budget) -> ProbeResult<()> {
        let mut current = Self::new(budget)?;
        if current.snapshot(budget)? != *initial {
            return Err(ProbeError::new(Reason::Changed));
        }
        for observed in &self.observed {
            let same = match observed {
                Observed::File(path, bytes) => bounded_read(path, ROW_BYTES, budget)? == *bytes,
                Observed::Directory(path, names) => bounded_directory(path, budget)? == *names,
                Observed::Canonical(path, target) => {
                    budget.tick()?;
                    fs::canonicalize(path)? == *target
                }
                Observed::Absent(path) => {
                    matches!(path_identity(path, budget), Err(error) if error.code == Some(libc::ENOENT))
                }
                Observed::Object(path, identity) => path_identity(path, budget)? == *identity,
            };
            if !same {
                return Err(ProbeError::new(Reason::Changed));
            }
        }
        for set in self.btrfs_sets.clone() {
            if self.btrfs_devices(&set.handle, budget)? != set.devices {
                return Err(ProbeError::new(Reason::Changed));
            }
        }
        // Held descriptors must also still describe the original identities.
        if object(&self.root, budget)? != initial.execution.root
            || object(&self.namespace, budget)? != initial.execution.namespace
        {
            return Err(ProbeError::new(Reason::Changed));
        }
        Ok(())
    }

    fn read(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<u8>> {
        let before = match path_identity(path, budget) {
            Ok(identity) => identity,
            Err(error) if error.code == Some(libc::ENOENT) => {
                self.observed.push(Observed::Absent(path.to_owned()));
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let bytes = bounded_read(path, ROW_BYTES, budget)?;
        if path_identity(path, budget)? != before {
            return Err(ProbeError::new(Reason::Changed));
        }
        self.observed
            .push(Observed::Object(path.to_owned(), before));
        self.observed
            .push(Observed::File(path.to_owned(), bytes.clone()));
        Ok(bytes)
    }

    fn directory(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<OsString>> {
        let before = path_identity(path, budget)?;
        let names = bounded_directory(path, budget)?;
        if path_identity(path, budget)? != before {
            return Err(ProbeError::new(Reason::Changed));
        }
        self.observed
            .push(Observed::Object(path.to_owned(), before));
        self.observed
            .push(Observed::Directory(path.to_owned(), names.clone()));
        Ok(names)
    }

    fn canonical(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<PathBuf> {
        let before = match path_identity(path, budget) {
            Ok(identity) => identity,
            Err(error) if error.code == Some(libc::ENOENT) => {
                self.observed.push(Observed::Absent(path.to_owned()));
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        budget.tick()?;
        let target = fs::canonicalize(path)?;
        if target.as_os_str().as_bytes().len() > crate::limits::PATH_BYTES {
            return Err(ProbeError::new(Reason::Limit));
        }
        self.observed
            .push(Observed::Canonical(path.to_owned(), target.clone()));
        if path_identity(path, budget)? != before {
            return Err(ProbeError::new(Reason::Changed));
        }
        self.observed
            .push(Observed::Object(path.to_owned(), before));
        Ok(target)
    }

    fn mount_id(handle: &OwnedFd, budget: &mut Budget) -> ProbeResult<u64> {
        budget.tick()?;
        match statx(handle, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID) {
            Ok(stat) if stat.stx_mask & StatxFlags::MNT_ID.bits() != 0 => {
                return Ok(stat.stx_mnt_id);
            }
            Ok(_)
            | Err(
                rustix::io::Errno::NOSYS | rustix::io::Errno::INVAL | rustix::io::Errno::OPNOTSUPP,
            ) => {}
            Err(error) => return Err(error.into()),
        }
        let bytes = bounded_read(
            &PathBuf::from(format!("/proc/thread-self/fdinfo/{}", handle.as_raw_fd())),
            ROW_BYTES,
            budget,
        )?;
        fdinfo_mount_id(&bytes)
    }

    fn correlate<'a>(
        snapshot: &'a Snapshot,
        resolved: &Resolved<Arc<OwnedFd>>,
        budget: &mut Budget,
    ) -> ProbeResult<(&'a Mount, FilesystemClass)> {
        let mount_id = Self::mount_id(&resolved.handle, budget)?;
        budget.tick()?;
        let stat = fstat(&*resolved.handle)?;
        budget.tick()?;
        let filesystem = fstatfs(&*resolved.handle)?;
        correlate_mount(
            snapshot,
            mount_id,
            (major(stat.st_dev), minor(stat.st_dev)),
            filesystem.f_type,
            &resolved.effective,
        )
    }

    fn device(
        &mut self,
        device: (u32, u32),
        depth: usize,
        active: &mut BTreeSet<(u32, u32)>,
        budget: &mut Budget,
    ) -> ProbeResult<BackingClass> {
        let previous = self.nodes;
        let (result, nodes) = {
            let mut walker = Backing {
                provider: self,
                nodes: previous,
            };
            let result = walker.device(device, depth, active, budget);
            (result, walker.nodes)
        };
        self.nodes = nodes;
        result
    }

    fn btrfs_devices(
        &mut self,
        handle: &OwnedFd,
        budget: &mut Budget,
    ) -> ProbeResult<Vec<(u32, u32)>> {
        budget.tick()?;
        let directory = openat(
            handle,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let mut info = [0_u64; 128];
        budget.tick()?;
        // SAFETY: Linux BTRFS_IOC_FS_INFO writes a 1024-byte, aligned UAPI
        // buffer. The read-only directory descriptor and buffer remain alive.
        let status = unsafe { libc::ioctl(directory.as_raw_fd(), 0x8400_941f, info.as_mut_ptr()) };
        if status != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut fsid = [0_u8; 16];
        fsid[..8].copy_from_slice(&info[2].to_ne_bytes());
        fsid[8..].copy_from_slice(&info[3].to_ne_bytes());
        let uuid = uuid::Uuid::from_bytes(fsid);
        btrfs_inventory(self, uuid, info[1], budget)
    }
}

trait Topology {
    fn read(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<u8>>;
    fn directory(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<OsString>>;
    fn canonical(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<PathBuf>;
}

fn btrfs_inventory<P: Topology>(
    provider: &mut P,
    filesystem_id: uuid::Uuid,
    count: u64,
    budget: &mut Budget,
) -> ProbeResult<Vec<(u32, u32)>> {
    if filesystem_id.is_nil() || count == 0 || count > BACKING_NODES as u64 {
        return Err(ProbeError::new(Reason::UnknownBacking));
    }
    let directory = PathBuf::from(format!("/sys/fs/btrfs/{filesystem_id}/devices"));
    let names = provider.directory(&directory, budget)?;
    if u64::try_from(names.len()).ok() != Some(count) {
        return Err(ProbeError::new(Reason::UnknownBacking));
    }
    let mut devices = BTreeSet::new();
    for name in names {
        devices.insert(parse_device(
            &provider.read(&directory.join(name).join("dev"), budget)?,
        )?);
    }
    if devices.len() as u64 != count {
        return Err(ProbeError::new(Reason::UnknownBacking));
    }
    Ok(devices.into_iter().collect())
}

fn correlate_mount<'a>(
    snapshot: &'a Snapshot,
    mount_id: u64,
    device: (u32, u32),
    magic: rustix::fs::FsWord,
    effective: &Path,
) -> ProbeResult<(&'a Mount, FilesystemClass)> {
    let row = snapshot
        .mounts
        .get(&mount_id)
        .ok_or(ProbeError::new(Reason::MountEvidence))?;
    let class = coherent_filesystem(&row.filesystem, magic)?;
    if device != (row.major, row.minor) || !effective.starts_with(&row.mountpoint) {
        return Err(ProbeError::new(Reason::MountEvidence));
    }
    Ok((row, class))
}

impl Topology for Linux {
    fn read(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<u8>> {
        Linux::read(self, path, budget)
    }
    fn directory(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<Vec<OsString>> {
        Linux::directory(self, path, budget)
    }
    fn canonical(&mut self, path: &Path, budget: &mut Budget) -> ProbeResult<PathBuf> {
        Linux::canonical(self, path, budget)
    }
}

struct Backing<'a, P> {
    provider: &'a mut P,
    nodes: usize,
}
impl<P: Topology> Backing<'_, P> {
    fn device(
        &mut self,
        device: (u32, u32),
        depth: usize,
        active: &mut BTreeSet<(u32, u32)>,
        budget: &mut Budget,
    ) -> ProbeResult<BackingClass> {
        budget.tick()?;
        self.nodes += 1;
        if depth > BACKING_DEPTH || self.nodes > BACKING_NODES {
            return Err(ProbeError::new(Reason::Limit));
        }
        if !active.insert(device) {
            return Ok(BackingClass::Unknown);
        }
        let result = self.device_inner(device, depth, active, budget);
        active.remove(&device);
        result
    }

    fn device_inner(
        &mut self,
        device: (u32, u32),
        depth: usize,
        active: &mut BTreeSet<(u32, u32)>,
        budget: &mut Budget,
    ) -> ProbeResult<BackingClass> {
        let sys = PathBuf::from(format!("/sys/dev/block/{}:{}", device.0, device.1));
        let target = self.provider.canonical(&sys, budget)?;
        if !target.starts_with("/sys/devices") {
            return Ok(BackingClass::Unknown);
        }
        if parse_device(&self.provider.read(&target.join("dev"), budget)?)? != device {
            return Err(ProbeError::new(Reason::MountEvidence));
        }
        let name = target
            .file_name()
            .ok_or(ProbeError::new(Reason::UnknownBacking))?
            .as_bytes();
        if memory_device(name) {
            return Ok(BackingClass::Memory);
        }
        if numbered(name, b"loop") {
            return Ok(BackingClass::Unknown);
        }
        match self.provider.read(&target.join("partition"), budget) {
            Ok(bytes) => {
                if number::<u32>(trim(&bytes))? == 0 {
                    return Ok(BackingClass::Unknown);
                }
                let parent = target
                    .parent()
                    .ok_or(ProbeError::new(Reason::UnknownBacking))?;
                let device = parse_device(&self.provider.read(&parent.join("dev"), budget)?)?;
                return self.device(device, depth + 1, active, budget);
            }
            Err(error) if error.code == Some(libc::ENOENT) => {}
            Err(error) => return Err(error),
        }
        let slaves = self.provider.directory(&target.join("slaves"), budget)?;
        if !slaves.is_empty() {
            if !name.starts_with(b"dm-") && !numbered(name, b"md") {
                return Ok(BackingClass::Unknown);
            }
            let mut classes = Vec::new();
            for slave in slaves {
                let device = parse_device(
                    &self
                        .provider
                        .read(&target.join("slaves").join(slave).join("dev"), budget)?,
                )?;
                classes.push(self.device(device, depth + 1, active, budget)?);
            }
            return Ok(combine_backing(&classes));
        }
        let size = self.provider.read(&target.join("size"), budget)?;
        if number::<u64>(trim(&size))? == 0 {
            return Ok(BackingClass::Unknown);
        }
        // Supported guest-visible disk contracts. Virtual block namespaces
        // without dependencies do not inherit this allowlist.
        if !disk_device(name) || target.starts_with("/sys/devices/virtual") {
            return Ok(BackingClass::Unknown);
        }
        self.leaf_driver(&target, name, budget)
    }

    fn leaf_driver(
        &mut self,
        target: &Path,
        name: &[u8],
        budget: &mut Budget,
    ) -> ProbeResult<BackingClass> {
        let mut supported = false;
        for (depth, ancestor) in target.ancestors().enumerate() {
            if !ancestor.starts_with("/sys/devices") {
                break;
            }
            if depth > BACKING_DEPTH {
                return Err(ProbeError::new(Reason::Limit));
            }
            match self.provider.canonical(&ancestor.join("driver"), budget) {
                Ok(driver) => {
                    if !driver.starts_with("/sys/bus") {
                        return Ok(BackingClass::Unknown);
                    }
                    let driver = driver.file_name().map(OsStr::as_bytes).unwrap_or_default();
                    if matches!(driver, b"scsi_debug" | b"brd" | b"zram") {
                        return Ok(BackingClass::Memory);
                    }
                    supported |= disk_driver(name, driver);
                }
                Err(error) if error.code == Some(libc::ENOENT) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(if supported {
            BackingClass::Disk
        } else {
            BackingClass::Unknown
        })
    }
}

impl Kernel for Linux {
    fn backing_hash(&mut self, budget: &mut Budget) -> ProbeResult<String> {
        budget.tick()?;
        let mut hasher = Sha256::new();
        hasher.update(b"memory-hooks-storage-backing-v1\0");
        for observed in &self.observed {
            budget.tick()?;
            let (kind, path) = match observed {
                Observed::File(path, _) => (b"file".as_slice(), path),
                Observed::Directory(path, _) => (b"directory".as_slice(), path),
                Observed::Canonical(path, _) => (b"canonical".as_slice(), path),
                Observed::Object(path, _) => (b"object".as_slice(), path),
                Observed::Absent(path) => (b"absent".as_slice(), path),
            };
            crate::config::hash_part(&mut hasher, kind);
            crate::config::hash_part(&mut hasher, path.as_os_str().as_bytes());
            match observed {
                Observed::File(_, bytes) => crate::config::hash_part(&mut hasher, bytes),
                Observed::Directory(_, names) => {
                    for name in names {
                        crate::config::hash_part(&mut hasher, name.as_bytes());
                    }
                }
                Observed::Canonical(_, target) => {
                    crate::config::hash_part(&mut hasher, target.as_os_str().as_bytes());
                }
                Observed::Object(_, identity) => {
                    crate::config::hash_part(&mut hasher, &identity.device.to_be_bytes());
                    crate::config::hash_part(&mut hasher, &identity.inode.to_be_bytes());
                    crate::config::hash_part(&mut hasher, &identity.mode.to_be_bytes());
                }
                Observed::Absent(_) => {}
            }
        }
        Ok(format!("{:x}", hasher.finalize()))
    }
    fn correlate(
        &mut self,
        snapshot: &Snapshot,
        resolved: &Resolved<Self::Handle>,
        budget: &mut Budget,
    ) -> ProbeResult<(Mount, FilesystemClass)> {
        let (mount, filesystem) = Linux::correlate(snapshot, resolved, budget)?;
        Ok((mount.clone(), filesystem))
    }
    fn backing(
        &mut self,
        handle: &Self::Handle,
        mount: &Mount,
        filesystem: FilesystemClass,
        budget: &mut Budget,
    ) -> ProbeResult<BackingClass> {
        budget.tick()?;
        match filesystem {
            FilesystemClass::Memory => Ok(BackingClass::Memory),
            FilesystemClass::Ext | FilesystemClass::Xfs => {
                self.device((mount.major, mount.minor), 0, &mut BTreeSet::new(), budget)
            }
            FilesystemClass::Btrfs => {
                let mut classes = Vec::new();
                let devices = self.btrfs_devices(handle, budget)?;
                self.btrfs_sets.push(BtrfsSet {
                    handle: handle.clone(),
                    devices: devices.clone(),
                });
                for device in devices {
                    classes.push(self.device(device, 0, &mut BTreeSet::new(), budget)?);
                }
                Ok(combine_backing(&classes))
            }
            FilesystemClass::Overlay
                if mount
                    .super_options
                    .split(|byte| *byte == b',')
                    .any(|option| option == b"volatile") =>
            {
                Ok(BackingClass::Memory)
            }
            _ => Ok(BackingClass::Unknown),
        }
    }
    fn mount_identity(&mut self, handle: &Self::Handle, budget: &mut Budget) -> ProbeResult<u64> {
        Self::mount_id(handle, budget)
    }
    fn recheck(&mut self, initial: &Snapshot, budget: &mut Budget) -> ProbeResult<()> {
        Linux::recheck(self, initial, budget)
    }
}

impl Provider for Linux {
    type Handle = Arc<OwnedFd>;
    fn root(&mut self, budget: &mut Budget) -> ProbeResult<Self::Handle> {
        budget.tick()?;
        Ok(self.root.clone())
    }
    fn entry(
        &mut self,
        parent: &Self::Handle,
        name: &OsStr,
        budget: &mut Budget,
    ) -> ProbeResult<Entry<Self::Handle>> {
        budget.tick()?;
        let handle = Arc::new(openat(
            &**parent,
            name,
            OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        let identity = object(&handle, budget)?;
        Ok(match identity.mode & libc::S_IFMT {
            libc::S_IFDIR => Entry::Directory(handle, identity),
            libc::S_IFLNK => Entry::Symlink(handle, identity),
            _ => Entry::Other,
        })
    }
    fn link(&mut self, handle: &Self::Handle, budget: &mut Budget) -> ProbeResult<Vec<u8>> {
        budget.tick()?;
        if fstatfs(&**handle)?.f_type == libc::PROC_SUPER_MAGIC {
            return Err(ProbeError::new(Reason::SpecialIndirection));
        }
        let mut bytes = vec![0; crate::limits::PATH_BYTES + 1];
        budget.tick()?;
        let length = readlinkat_raw(&**handle, "", bytes.as_mut_slice())?;
        if length > crate::limits::PATH_BYTES {
            return Err(ProbeError::new(Reason::Limit));
        }
        bytes.truncate(length);
        Ok(bytes)
    }
    fn accessible(&mut self, handle: &Self::Handle, budget: &mut Budget) -> ProbeResult<()> {
        budget.tick()?;
        accessat(&**handle, ".", Access::EXEC_OK, AtFlags::EACCESS)?;
        Ok(())
    }
    fn identity(
        &mut self,
        handle: &Self::Handle,
        budget: &mut Budget,
    ) -> ProbeResult<ObjectIdentity> {
        object(handle, budget)
    }
}

pub(super) fn fdinfo_mount_id(bytes: &[u8]) -> ProbeResult<u64> {
    let mut mount = None;
    for line in bytes.split(|byte| *byte == b'\n') {
        if let Some(value) = line.strip_prefix(b"mnt_id:") {
            if mount.is_some() {
                return Err(ProbeError::new(Reason::MountEvidence));
            }
            mount = Some(number(trim(value))?);
        }
    }
    mount
        .filter(|id| *id > 0)
        .ok_or(ProbeError::new(Reason::MountEvidence))
}

fn trim(bytes: &[u8]) -> &[u8] {
    bytes.trim_ascii()
}
fn number<T: std::str::FromStr>(bytes: &[u8]) -> ProbeResult<T> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(ProbeError::new(Reason::MountEvidence));
    }
    std::str::from_utf8(bytes)
        .map_err(|_| ProbeError::new(Reason::MountEvidence))?
        .parse()
        .map_err(|_| ProbeError::new(Reason::MountEvidence))
}
fn parse_device(bytes: &[u8]) -> ProbeResult<(u32, u32)> {
    let bytes = trim(bytes);
    let colon = bytes
        .iter()
        .position(|byte| *byte == b':')
        .ok_or(ProbeError::new(Reason::MountEvidence))?;
    Ok((number(&bytes[..colon])?, number(&bytes[colon + 1..])?))
}

fn check_fs_credentials(bytes: &[u8]) -> ProbeResult<()> {
    // faccessat's effective-ID check must describe the VFS caller too.
    // Nonstandard fsuid/fsgid execution is conservatively unsupported; never
    // change credentials to manufacture accessibility or disclose their values.
    for label in [b"Uid:".as_slice(), b"Gid:"] {
        let mut rows = bytes
            .split(|byte| *byte == b'\n')
            .filter_map(|line| line.strip_prefix(label));
        let row = rows.next().ok_or(ProbeError::new(Reason::Inaccessible))?;
        if rows.next().is_some() {
            return Err(ProbeError::new(Reason::Inaccessible));
        }
        let ids = row
            .split(u8::is_ascii_whitespace)
            .filter(|part| !part.is_empty())
            .map(number::<u32>)
            .collect::<ProbeResult<Vec<_>>>()?;
        if ids.len() != 4 || ids[1] != ids[3] {
            return Err(ProbeError::new(Reason::Inaccessible));
        }
    }
    Ok(())
}

fn unescape(bytes: &[u8]) -> ProbeResult<PathBuf> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            let escape = bytes
                .get(index..index + 4)
                .ok_or(ProbeError::new(Reason::MountEvidence))?;
            output.push(match escape {
                b"\\040" => b' ',
                b"\\011" => b'\t',
                b"\\012" => b'\n',
                b"\\134" => b'\\',
                _ => return Err(ProbeError::new(Reason::MountEvidence)),
            });
            index += 4;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    if output.first() != Some(&b'/')
        || output.len() > crate::limits::PATH_BYTES
        || output.contains(&0)
    {
        return Err(ProbeError::new(Reason::MountEvidence));
    }
    Ok(PathBuf::from(OsString::from_vec(output)))
}

pub(super) fn mountinfo(bytes: &[u8]) -> ProbeResult<BTreeMap<u64, Mount>> {
    if bytes.is_empty() || bytes.len() > MOUNT_BYTES || bytes.last() != Some(&b'\n') {
        return Err(ProbeError::new(Reason::MountEvidence));
    }
    let mut mounts = BTreeMap::new();
    for (index, line) in bytes[..bytes.len() - 1]
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if index >= MOUNT_ROWS || line.len() > ROW_BYTES {
            return Err(ProbeError::new(Reason::Limit));
        }
        let fields = line.split(|byte| *byte == b' ').collect::<Vec<_>>();
        if fields.len() > 80
            || fields
                .iter()
                .any(|field| field.is_empty() || field.len() > crate::limits::PATH_BYTES)
        {
            return Err(ProbeError::new(Reason::MountEvidence));
        }
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or(ProbeError::new(Reason::MountEvidence))?;
        if separator < 6 || fields.len() != separator + 4 {
            return Err(ProbeError::new(Reason::MountEvidence));
        }
        let (major, minor) = parse_device(fields[2])?;
        let mount = Mount {
            id: number(fields[0])?,
            parent: number(fields[1])?,
            major,
            minor,
            root: unescape(fields[3])?,
            mountpoint: unescape(fields[4])?,
            options: fields[5].to_vec(),
            filesystem: fields[separator + 1].to_vec(),
            super_options: fields[separator + 3].to_vec(),
            row_hash: hash(b"mount", line),
        };
        if mount.id == 0 || mount.parent == mount.id || mounts.insert(mount.id, mount).is_some() {
            return Err(ProbeError::new(Reason::MountEvidence));
        }
    }
    Ok(mounts)
}

fn coherent_filesystem(name: &[u8], magic: rustix::fs::FsWord) -> ProbeResult<FilesystemClass> {
    let expected = match name {
        b"ext2" | b"ext3" | b"ext4" => Some((libc::EXT4_SUPER_MAGIC, FilesystemClass::Ext)),
        b"xfs" => Some((libc::XFS_SUPER_MAGIC, FilesystemClass::Xfs)),
        b"btrfs" => Some((libc::BTRFS_SUPER_MAGIC, FilesystemClass::Btrfs)),
        b"tmpfs" => Some((libc::TMPFS_MAGIC, FilesystemClass::Memory)),
        b"ramfs" => Some((RAMFS_MAGIC, FilesystemClass::Memory)),
        b"overlay" => Some((libc::OVERLAYFS_SUPER_MAGIC, FilesystemClass::Overlay)),
        _ => None,
    };
    if let Some((expected, class)) = expected {
        if magic != expected {
            return Err(ProbeError::new(Reason::MountEvidence));
        }
        return Ok(class);
    }
    if matches!(
        magic,
        libc::TMPFS_MAGIC
            | RAMFS_MAGIC
            | libc::EXT4_SUPER_MAGIC
            | libc::XFS_SUPER_MAGIC
            | libc::BTRFS_SUPER_MAGIC
            | libc::OVERLAYFS_SUPER_MAGIC
    ) {
        return Err(ProbeError::new(Reason::MountEvidence));
    }
    Ok(FilesystemClass::Other)
}

fn numbered(name: &[u8], prefix: &[u8]) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|tail| !tail.is_empty() && tail.iter().all(u8::is_ascii_digit))
}
fn memory_device(name: &[u8]) -> bool {
    numbered(name, b"ram") || numbered(name, b"zram")
}
fn disk_device(name: &[u8]) -> bool {
    [b"sd".as_slice(), b"vd", b"xvd"].iter().any(|prefix| {
        name.strip_prefix(*prefix)
            .is_some_and(|tail| !tail.is_empty() && tail.iter().all(u8::is_ascii_lowercase))
    }) || numbered(name, b"mmcblk")
        || name.strip_prefix(b"nvme").is_some_and(|tail| {
            tail.split(|byte| *byte == b'n').count() == 2
                && tail
                    .split(|byte| *byte == b'n')
                    .all(|part| !part.is_empty() && part.iter().all(u8::is_ascii_digit))
        })
}
fn disk_driver(name: &[u8], driver: &[u8]) -> bool {
    (name.starts_with(b"sd")
        && matches!(
            driver,
            b"ahci"
                | b"ata_piix"
                | b"virtio_scsi"
                | b"mpt3sas"
                | b"megaraid_sas"
                | b"usb-storage"
                | b"uas"
        ))
        || (name.starts_with(b"vd") && driver == b"virtio_blk")
        || (name.starts_with(b"xvd") && driver == b"vbd")
        || (name.starts_with(b"nvme") && driver == b"nvme")
        || (name.starts_with(b"mmcblk") && driver == b"mmcblk")
}
fn combine_backing(classes: &[BackingClass]) -> BackingClass {
    if classes.contains(&BackingClass::Memory) {
        BackingClass::Memory
    } else if !classes.is_empty() && classes.iter().all(|class| *class == BackingClass::Disk) {
        BackingClass::Disk
    } else {
        BackingClass::Unknown
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "private explicit assessment inputs prevent ambient authority"
)]
pub(super) fn assess<P: Kernel>(
    provider: &mut P,
    budget: &mut Budget,
    snapshot: &Snapshot,
    resolved: &Resolved<P::Handle>,
    candidate: &Candidate,
    requirement: &StorageRequirement,
    profile: Option<&str>,
    attestation: Option<&StorageExecution>,
) -> ProbeResult<(Outcome, Reason, Evidence)> {
    let (mount, filesystem) = provider.correlate(snapshot, resolved, budget)?;
    let mut evidence = super::empty_evidence(candidate, None);
    evidence.resolved_hash = Some(hash(b"resolved", resolved.effective.as_os_str().as_bytes()));
    evidence.ancestor_hash = Some(hash(
        b"ancestor",
        &serde_json::to_vec(&resolved.identity).map_err(|_| ProbeError::new(Reason::Limit))?,
    ));
    evidence.mount_hash = Some(mount.row_hash.clone());
    evidence.execution_hash = Some(hash(
        b"execution",
        &serde_json::to_vec(&snapshot.execution).map_err(|_| ProbeError::new(Reason::Limit))?,
    ));
    evidence.missing_suffix = resolved.missing;
    evidence.filesystem = filesystem;
    let (outcome, reason, backing) = if requirement.kind == ConstraintKind::ContainerTarget {
        container(
            provider,
            budget,
            snapshot,
            &mount,
            resolved,
            candidate.role,
            profile,
            attestation,
        )?
    } else {
        let backing = provider.backing(&resolved.handle, &mount, filesystem, budget)?;
        match backing {
            BackingClass::Disk => (Outcome::Pass, Reason::Verified, backing),
            BackingClass::Memory => (Outcome::Deny, Reason::VolatileBacking, backing),
            _ => (Outcome::Indeterminate, Reason::UnknownBacking, backing),
        }
    };
    evidence.backing = backing;
    evidence.backing_hash = Some(provider.backing_hash(budget)?);
    Ok((outcome, reason, evidence))
}

#[expect(
    clippy::too_many_arguments,
    reason = "explicit private protected provenance comparison"
)]
#[expect(
    clippy::too_many_lines,
    reason = "ordered root and exact protected mount validation"
)]
fn container<P: Kernel>(
    provider: &mut P,
    budget: &mut Budget,
    snapshot: &Snapshot,
    mount: &Mount,
    resolved: &Resolved<P::Handle>,
    role: CandidateRole,
    profile: Option<&str>,
    attestation: Option<&StorageExecution>,
) -> ProbeResult<(Outcome, Reason, BackingClass)> {
    let root = Path::new("/tmp/target");
    let requested = normalized(&resolved.requested);
    let location = match role {
        CandidateRole::TargetDirectory => requested == root && resolved.effective == root,
        CandidateRole::BuildOutput => {
            requested.starts_with(root) && resolved.effective.starts_with(root)
        }
        _ => false,
    };
    if !location {
        return Ok((
            Outcome::Deny,
            Reason::ForbiddenTarget,
            BackingClass::Unknown,
        ));
    }
    if profile != Some("woodpecker-container") {
        return Ok((
            Outcome::Indeterminate,
            Reason::MissingAttestation,
            BackingClass::Unknown,
        ));
    }
    if role == CandidateRole::BuildOutput && resolved.effective != root {
        let root_candidate = Candidate::new(0, CandidateRole::TargetDirectory, root.as_os_str())
            .map_err(ProbeError::new)?;
        let root_path = super::path::resolve(provider, budget, Path::new("/"), &root_candidate)?;
        let (root_mount, _) = provider.correlate(snapshot, &root_path, budget)?;
        let root_result = container(
            provider,
            budget,
            snapshot,
            &root_mount,
            &root_path,
            CandidateRole::TargetDirectory,
            profile,
            attestation,
        )?;
        root_path.recheck(provider, budget)?;
        if root_result.0 != Outcome::Pass {
            return Ok(root_result);
        }
    }
    let Some(attestation) = attestation else {
        return Ok((
            Outcome::Indeterminate,
            Reason::MissingAttestation,
            BackingClass::Unknown,
        ));
    };
    let execution = &snapshot.execution;
    if attestation.boot_id != execution.boot
        || attestation.namespace_device != execution.namespace.device
        || attestation.namespace_inode != execution.namespace.inode
        || attestation.root_device != execution.root.device
        || attestation.root_inode != execution.root.inode
    {
        return Ok((
            Outcome::Deny,
            Reason::AttestationMismatch,
            BackingClass::Unknown,
        ));
    }
    let Some(pin) = attestation.mounts.iter().find(|pin| pin.id == mount.id) else {
        return Ok((
            Outcome::Indeterminate,
            Reason::MissingAttestation,
            BackingClass::Unknown,
        ));
    };
    if pin.root != mount.root
        || pin.major != mount.major
        || pin.minor != mount.minor
        || pin.mountpoint != mount.mountpoint
        || pin.filesystem.as_bytes() != mount.filesystem
    {
        return Ok((
            Outcome::Deny,
            Reason::AttestationMismatch,
            BackingClass::Unknown,
        ));
    }
    // Walk the exact mountpoint without following it through a convenient
    // textual prefix. Stacked mounts must still match the descriptor mount ID.
    let candidate = Candidate::new(
        0,
        CandidateRole::SourceWorktree,
        mount.mountpoint.as_os_str(),
    )
    .map_err(ProbeError::new)?;
    let point = super::path::resolve(provider, budget, Path::new("/"), &candidate)?;
    if point.identity.device != pin.mountpoint_device
        || point.identity.inode != pin.mountpoint_inode
        || provider.mount_identity(&point.handle, budget)? != mount.id
    {
        return Ok((
            Outcome::Deny,
            Reason::AttestationMismatch,
            BackingClass::Unknown,
        ));
    }
    point.recheck(provider, budget)?;
    Ok((
        Outcome::Pass,
        Reason::Verified,
        BackingClass::ContainerLocal,
    ))
}

#[cfg(test)]
#[path = "linux_tests.rs"]
mod tests;
