//! Private, descriptor-relative records for later hook consumers.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::fs::{
    AtFlags, FlockOperation, Mode, OFlags, chmodat, fchmod, fgetxattr, flock, fstat, fstatfs,
    fsync, mkdirat, openat, renameat, unlinkat,
};
use rustix::io::Errno;
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::config::{TrustedHooksConfig, hash_part, path_bytes, valid_path};
use crate::error::{Error, Result, io};
use crate::identity::{RepositoryIdentity, ResolvedBinding};
use crate::limits;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Opaque, domain-separated key for one binding/client/session tuple.
pub struct StateKey {
    name: String,
    scope: String,
    fingerprint: String,
}

impl StateKey {
    /// Hash bounded external identifiers into a safe state key.
    ///
    /// # Errors
    /// Rejects empty or excessive identifiers before hashing.
    pub fn new(binding: &ResolvedBinding, client: &str, session: &str) -> Result<Self> {
        if client.is_empty()
            || client.len() > limits::EXTERNAL_ID_BYTES
            || session.is_empty()
            || session.len() > limits::EXTERNAL_ID_BYTES
        {
            return Err(Error::StateInsecure("external identifier limit"));
        }
        let mut scope = Sha256::new();
        scope.update(b"memory-hooks-state-scope-v1\0");
        match binding.identity() {
            RepositoryIdentity::Git(path) => {
                hash_part(&mut scope, b"git");
                hash_part(&mut scope, path_bytes(path));
            }
            RepositoryIdentity::Directory(path) => {
                hash_part(&mut scope, b"directory");
                hash_part(&mut scope, path_bytes(path));
            }
        }
        hash_part(&mut scope, binding.project().as_bytes());
        hash_part(&mut scope, binding.fingerprint().as_bytes());
        let context =
            serde_json::to_vec(binding.context()).map_err(|_| Error::StateInsecure("context"))?;
        hash_part(&mut scope, &context);
        let scope = format!("{:x}", scope.finalize());
        let mut client_hash = Sha256::new();
        client_hash.update(b"memory-hooks-client-v1\0");
        hash_part(&mut client_hash, client.as_bytes());
        let mut session_hash = Sha256::new();
        session_hash.update(b"memory-hooks-session-v1\0");
        hash_part(&mut session_hash, session.as_bytes());
        let mut key = Sha256::new();
        key.update(b"memory-hooks-key-v1\0");
        hash_part(&mut key, scope.as_bytes());
        hash_part(&mut key, &client_hash.finalize());
        hash_part(&mut key, &session_hash.finalize());
        Ok(Self {
            name: format!("{:x}", key.finalize()),
            scope,
            fingerprint: binding.fingerprint().to_owned(),
        })
    }

    /// Opaque filename-safe hex digest.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.name
    }
}

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u32,
    scope: String,
    payload: T,
}

struct BoundedVec {
    bytes: Vec<u8>,
}

impl Write for BoundedVec {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        if chunk.len() > limits::RECORD_BYTES.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("record size limit"));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(chunk.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Rooted store whose callers can only address validated state keys.
pub struct PrivateStateStore {
    directory: File,
    fingerprint: String,
}

fn check_acl(file: &File) -> Result<()> {
    match fgetxattr(file, "system.posix_acl_access", &mut [] as &mut [u8]) {
        Err(Errno::NODATA) => Ok(()),
        Ok(_) | Err(Errno::RANGE) => Err(Error::StateInsecure("ACL")),
        Err(_) => Err(Error::StateInsecure("ACL verification")),
    }
}

fn check_object(file: &File, expected_type: u32, private: bool, linked: bool) -> Result<()> {
    let stat = fstat(file).map_err(|_| Error::StateInsecure("stat"))?;
    let euid = rustix::process::geteuid().as_raw();
    if stat.st_uid != euid && !(expected_type == libc::S_IFDIR && !private && stat.st_uid == 0) {
        return Err(Error::StateInsecure("owner"));
    }
    if stat.st_mode & libc::S_IFMT != expected_type
        || (private
            && stat.st_mode & 0o7777
                != if expected_type == libc::S_IFDIR {
                    0o700
                } else {
                    0o600
                })
        || (!private && stat.st_mode & 0o022 != 0)
        || (linked && stat.st_nlink != 1)
    {
        return Err(Error::StateInsecure("mode or links"));
    }
    check_acl(file)
}

fn check_local_filesystem(file: &File) -> Result<()> {
    let kind = fstatfs(file)
        .map_err(|_| Error::StateInsecure("filesystem type"))?
        .f_type;
    // Linux local ext4, XFS, Btrfs, and tmpfs. Network/FUSE/overlay semantics
    // are not claimed by this store even when their mode bits look private.
    if matches!(kind, 0xef53 | 0x5846_5342 | 0x9123_683e | 0x0102_1994) {
        Ok(())
    } else {
        Err(Error::StateInsecure("unsupported filesystem"))
    }
}

fn open_root(path: &Path) -> Result<File> {
    valid_path(path).map_err(|_| Error::StateInsecure("state_root path"))?;
    let mut current = File::from(
        openat(
            rustix::fs::CWD,
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Error::StateInsecure("root"))?,
    );
    check_object(&current, libc::S_IFDIR, false, false)?;
    let mut parts = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .peekable();
    while let Some(part) = parts.next() {
        let private = parts.peek().is_none();
        if private {
            check_local_filesystem(&current)?;
        }
        let created = if private {
            match mkdirat(&current, part, Mode::RWXU) {
                Ok(()) => true,
                Err(Errno::EXIST) => false,
                Err(_) => return Err(Error::StateInsecure("create state_root")),
            }
        } else {
            false
        };
        let created_inode = if created {
            // O_PATH opens even if a restrictive umask created mode 000.
            // The procfd path addresses this retained inode, not a mutable
            // directory entry. Older kernels reject fchmodat NOFOLLOW here.
            let held = File::from(
                openat(
                    &current,
                    part,
                    OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| Error::StateInsecure("new state_root handle"))?,
            );
            let stamp = fstat(&held).map_err(|_| Error::StateInsecure("new state_root stat"))?;
            let procfd = format!("/proc/self/fd/{}", held.as_raw_fd());
            chmodat(
                rustix::fs::CWD,
                procfd.as_str(),
                Mode::RWXU,
                AtFlags::empty(),
            )
            .map_err(|error| Error::Io("new state_root mode", Some(error.raw_os_error())))?;
            Some((stamp.st_dev, stamp.st_ino))
        } else {
            None
        };
        let next = File::from(
            openat(
                &current,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Error::StateInsecure("state path component"))?,
        );
        if let Some((device, inode)) = created_inode {
            let stamp = fstat(&next).map_err(|_| Error::StateInsecure("new state_root stat"))?;
            if stamp.st_dev != device || stamp.st_ino != inode {
                return Err(Error::StateInsecure("state_root changed"));
            }
        }
        if created {
            fchmod(&next, Mode::RWXU).map_err(|_| Error::StateInsecure("new state_root mode"))?;
            fsync(&current).map_err(|_| Error::Io("state parent sync", None))?;
        }
        if private {
            check_local_filesystem(&next)?;
        }
        check_object(&next, libc::S_IFDIR, private, false)?;
        current = next;
    }
    Ok(current)
}

fn open_record(dir: &File, name: &str, create: bool) -> Result<Option<File>> {
    let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let opened = if create {
        match openat(
            dir,
            name,
            flags | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => Ok((fd, true)),
            Err(Errno::EXIST) => openat(dir, name, flags, Mode::empty()).map(|fd| (fd, false)),
            Err(error) => Err(error),
        }
    } else {
        openat(dir, name, flags, Mode::empty()).map(|fd| (fd, false))
    };
    match opened {
        Ok((fd, created)) => {
            let file = File::from(fd);
            if created {
                fchmod(&file, Mode::RUSR | Mode::WUSR)
                    .map_err(|_| Error::StateInsecure("new record mode"))?;
            }
            check_object(&file, libc::S_IFREG, true, true)?;
            Ok(Some(file))
        }
        Err(Errno::NOENT) if !create => Ok(None),
        Err(_) => Err(Error::StateInsecure("record open")),
    }
}

fn temporary_name(key: &StateKey) -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos());
    let mut hash = Sha256::new();
    hash.update(b"memory-hooks-temp-v1\0");
    hash_part(&mut hash, key.name.as_bytes());
    hash_part(&mut hash, &u64::from(std::process::id()).to_be_bytes());
    hash_part(&mut hash, &sequence.to_be_bytes());
    hash_part(&mut hash, &time.to_be_bytes());
    format!("temp-{:x}", hash.finalize())
}

impl PrivateStateStore {
    /// Open or create the configured private state root through checked handles.
    ///
    /// # Errors
    /// Rejects insecure paths and unsupported platforms.
    pub fn open(config: &TrustedHooksConfig) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        return Err(Error::UnsupportedPlatform);
        Ok(Self {
            directory: open_root(&config.state_root)?,
            fingerprint: config.fingerprint().to_owned(),
        })
    }

    /// Read and optionally replace a typed record under a stable per-key OS lock.
    /// A `None` replacement retains the previous record unchanged.
    ///
    /// # Errors
    /// Returns corruption, scope, lock, I/O, or durability errors without defaulting records.
    pub fn transact<T, R, F>(&self, key: &StateKey, change: F) -> Result<R>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce(Option<T>) -> Result<(Option<T>, R)>,
    {
        if key.fingerprint != self.fingerprint {
            return Err(Error::StateScopeMismatch);
        }
        let lock_name = format!("lock-{}", key.name);
        let lock = open_record(&self.directory, &lock_name, true)?
            .ok_or(Error::StateInsecure("lock open"))?;
        let deadline = Instant::now() + limits::SHORT_DEADLINE;
        loop {
            match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => break,
                Err(Errno::WOULDBLOCK) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(Errno::WOULDBLOCK) => return Err(Error::StateLockTimeout),
                Err(_) => return Err(Error::StateInsecure("advisory lock")),
            }
        }
        let record_name = format!("record-{}", key.name);
        let existing = match open_record(&self.directory, &record_name, false)? {
            Some(file) => {
                let size = file
                    .metadata()
                    .map_err(|error| io("record metadata", &error))?
                    .len();
                if size > limits::RECORD_BYTES as u64 {
                    return Err(Error::StateCorrupt);
                }
                let capacity = usize::try_from(size).map_err(|_| Error::StateCorrupt)?;
                let mut bytes = Vec::with_capacity(capacity);
                file.take((limits::RECORD_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|error| io("record read", &error))?;
                if bytes.len() > limits::RECORD_BYTES {
                    return Err(Error::StateCorrupt);
                }
                let envelope: Envelope<serde_json::Value> =
                    serde_json::from_slice(&bytes).map_err(|_| Error::StateCorrupt)?;
                if envelope.version != 1 || envelope.scope != key.scope {
                    return Err(Error::StateScopeMismatch);
                }
                Some(serde_json::from_value(envelope.payload).map_err(|_| Error::StateCorrupt)?)
            }
            None => None,
        };
        let (replacement, result) = change(existing)?;
        if let Some(payload) = replacement {
            let mut writer = BoundedVec { bytes: Vec::new() };
            serde_json::to_writer(
                &mut writer,
                &Envelope {
                    version: 1,
                    scope: key.scope.clone(),
                    payload,
                },
            )
            .map_err(|_| Error::StateCorrupt)?;
            let bytes = writer.bytes;
            // Reject an existing insecure destination before replacement.
            let _ = open_record(&self.directory, &record_name, false)?;
            let name = temporary_name(key);
            let mut file = File::from(
                openat(
                    &self.directory,
                    name.as_str(),
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::RUSR | Mode::WUSR,
                )
                .map_err(|_| Error::StateInsecure("temporary create"))?,
            );
            let write_result = (|| {
                fchmod(&file, Mode::RUSR | Mode::WUSR)
                    .map_err(|_| Error::StateInsecure("new temporary mode"))?;
                check_object(&file, libc::S_IFREG, true, true)?;
                file.write_all(&bytes)
                    .map_err(|error| io("temporary write", &error))?;
                file.flush()
                    .map_err(|error| io("temporary flush", &error))?;
                fsync(&file).map_err(|_| Error::Io("temporary sync", None))?;
                renameat(
                    &self.directory,
                    name.as_str(),
                    &self.directory,
                    record_name.as_str(),
                )
                .map_err(|_| Error::Io("record rename", None))?;
                fsync(&self.directory).map_err(|_| Error::DurabilityUncertain)?;
                Ok(())
            })();
            if write_result.is_err() {
                let _ = unlinkat(&self.directory, name.as_str(), AtFlags::empty());
            }
            write_result?;
        }
        Ok(result)
    }
}
