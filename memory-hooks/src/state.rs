//! Private, descriptor-relative records for later hook consumers.

#[cfg(test)]
use std::cell::RefCell;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
#[cfg(test)]
use std::path::PathBuf;
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

#[cfg(test)]
thread_local! {
    static REPLACE_FAULT: RefCell<Option<ReplaceFault>> = const { RefCell::new(None) };
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplaceStage {
    Create,
    Write,
    Flush,
    FileSync,
    Rename,
    ParentSync,
    AfterParentSync,
}

#[cfg(test)]
struct ReplaceFault {
    prefix: String,
    stage: ReplaceStage,
    occurrence: usize,
    seen: usize,
    barrier: Option<PathBuf>,
}

#[cfg(test)]
fn fail_at(name: &str, stage: ReplaceStage) -> bool {
    REPLACE_FAULT.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(fault) = slot.as_mut() else {
            return false;
        };
        if !name.starts_with(&fault.prefix) || fault.stage != stage {
            return false;
        }
        fault.seen += 1;
        if fault.seen != fault.occurrence {
            return false;
        }
        if let Some(barrier) = &fault.barrier {
            std::fs::write(barrier, b"reached").expect("replacement crash barrier");
            loop {
                std::thread::park();
            }
        }
        true
    })
}

#[cfg(test)]
fn with_replace_fault<T>(
    prefix: &str,
    stage: ReplaceStage,
    occurrence: usize,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REPLACE_FAULT.with(|slot| *slot.borrow_mut() = None);
        }
    }
    REPLACE_FAULT.with(|slot| {
        *slot.borrow_mut() = Some(ReplaceFault {
            prefix: prefix.to_owned(),
            stage,
            occurrence,
            seen: 0,
            barrier: None,
        });
    });
    let _reset = Reset;
    run()
}

#[cfg(test)]
fn with_replace_pause<T>(
    prefix: &str,
    stage: ReplaceStage,
    occurrence: usize,
    barrier: PathBuf,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            REPLACE_FAULT.with(|slot| *slot.borrow_mut() = None);
        }
    }
    REPLACE_FAULT.with(|slot| {
        *slot.borrow_mut() = Some(ReplaceFault {
            prefix: prefix.to_owned(),
            stage,
            occurrence,
            seen: 0,
            barrier: Some(barrier),
        });
    });
    let _reset = Reset;
    run()
}

/// Whether a private root may be created while walking its trusted parent.
#[derive(Clone, Copy)]
pub(crate) enum RootMode {
    CreateOrOpen,
    CreateNew,
    Existing,
}

/// Descriptor-relative private directory used by the snapshot and control stores.
pub(crate) struct PrivateDirectory {
    directory: File,
}

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
    directory: PrivateDirectory,
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

fn open_root(path: &Path, mode: RootMode) -> Result<File> {
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
        let created = if private && !matches!(mode, RootMode::Existing) {
            match mkdirat(&current, part, Mode::RWXU) {
                Ok(()) => true,
                Err(Errno::EXIST) if matches!(mode, RootMode::CreateOrOpen) => false,
                Err(Errno::EXIST) => return Err(Error::StateInsecure("private root exists")),
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

fn temporary_name(name: &str) -> String {
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |value| value.as_nanos());
    let mut hash = Sha256::new();
    hash.update(b"memory-hooks-temp-v1\0");
    hash_part(&mut hash, name.as_bytes());
    hash_part(&mut hash, &u64::from(std::process::id()).to_be_bytes());
    hash_part(&mut hash, &sequence.to_be_bytes());
    hash_part(&mut hash, &time.to_be_bytes());
    format!("temp-{:x}", hash.finalize())
}

fn valid_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(Error::StateInsecure("record name"));
    }
    Ok(())
}

impl PrivateDirectory {
    /// Open a checked private root with an explicit creation policy.
    pub(crate) fn open(path: &Path, mode: RootMode) -> Result<Self> {
        Ok(Self {
            directory: open_root(path, mode)?,
        })
    }

    /// Device and inode of the held private root descriptor.
    pub(crate) fn identity(&self) -> Result<(u64, u64)> {
        let stat = fstat(&self.directory).map_err(|_| Error::StateInsecure("root stat"))?;
        Ok((stat.st_dev, stat.st_ino))
    }

    /// Hold a stable lock file until the returned descriptor is dropped.
    pub(crate) fn lock(&self, name: &str) -> Result<File> {
        valid_name(name)?;
        let lock =
            open_record(&self.directory, name, true)?.ok_or(Error::StateInsecure("lock open"))?;
        let deadline = Instant::now() + limits::SHORT_DEADLINE;
        loop {
            match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(lock),
                Err(Errno::WOULDBLOCK) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(Errno::WOULDBLOCK) => return Err(Error::StateLockTimeout),
                Err(_) => return Err(Error::StateInsecure("advisory lock")),
            }
        }
    }

    /// Read a bounded private record without treating malformed data as absent.
    pub(crate) fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        valid_name(name)?;
        let Some(file) = open_record(&self.directory, name, false)? else {
            return Ok(None);
        };
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
        Ok(Some(bytes))
    }

    /// Atomically replace one bounded record and sync its parent directory.
    pub(crate) fn replace(&self, name: &str, bytes: &[u8]) -> Result<()> {
        valid_name(name)?;
        if bytes.len() > limits::RECORD_BYTES {
            return Err(Error::StateCorrupt);
        }
        let _ = open_record(&self.directory, name, false)?;
        let temporary = temporary_name(name);
        #[cfg(test)]
        if fail_at(name, ReplaceStage::Create) {
            return Err(Error::StateInsecure("temporary create"));
        }
        let mut file = File::from(
            openat(
                &self.directory,
                temporary.as_str(),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(|_| Error::StateInsecure("temporary create"))?,
        );
        let write_result = (|| {
            fchmod(&file, Mode::RUSR | Mode::WUSR)
                .map_err(|_| Error::StateInsecure("new temporary mode"))?;
            check_object(&file, libc::S_IFREG, true, true)?;
            #[cfg(test)]
            if fail_at(name, ReplaceStage::Write) {
                return Err(Error::Io("temporary write", None));
            }
            file.write_all(bytes)
                .map_err(|error| io("temporary write", &error))?;
            #[cfg(test)]
            if fail_at(name, ReplaceStage::Flush) {
                return Err(Error::Io("temporary flush", None));
            }
            file.flush()
                .map_err(|error| io("temporary flush", &error))?;
            #[cfg(test)]
            if fail_at(name, ReplaceStage::FileSync) {
                return Err(Error::Io("temporary sync", None));
            }
            fsync(&file).map_err(|_| Error::Io("temporary sync", None))?;
            #[cfg(test)]
            if fail_at(name, ReplaceStage::Rename) {
                return Err(Error::Io("record rename", None));
            }
            renameat(&self.directory, temporary.as_str(), &self.directory, name)
                .map_err(|_| Error::Io("record rename", None))?;
            #[cfg(test)]
            if fail_at(name, ReplaceStage::ParentSync) {
                return Err(Error::DurabilityUncertain);
            }
            fsync(&self.directory).map_err(|_| Error::DurabilityUncertain)?;
            #[cfg(test)]
            if fail_at(name, ReplaceStage::AfterParentSync) {
                return Err(Error::DurabilityUncertain);
            }
            Ok(())
        })();
        if write_result.is_err() {
            let _ = unlinkat(&self.directory, temporary.as_str(), AtFlags::empty());
        }
        write_result
    }

    /// Resolve a journal only after its replacement has been synced. If the
    /// final directory sync is uncertain, restore an unresolved journal while
    /// the caller still holds its control locks. Persistent storage failure
    /// can also prevent that recovery, which needs managed repair.
    pub(crate) fn resolve_journal(
        &self,
        name: &str,
        resolved: &[u8],
        unresolved: &[u8],
    ) -> Result<()> {
        match self.replace(name, resolved) {
            Ok(()) => Ok(()),
            Err(error) => {
                if matches!(error, Error::DurabilityUncertain) {
                    let _ = self.replace(name, unresolved);
                }
                Err(error)
            }
        }
    }
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
            directory: PrivateDirectory::open(&config.state_root, RootMode::CreateOrOpen)?,
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
        let _lock = self.directory.lock(&format!("lock-{}", key.name))?;
        let record_name = format!("record-{}", key.name);
        let existing = match self.directory.read(&record_name)? {
            Some(bytes) => {
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
            self.directory.replace(&record_name, &writer.bytes)?;
        }
        Ok(result)
    }
}

#[cfg(test)]
mod journal_fault_tests {
    use std::collections::BTreeMap;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use memory_common::guardrails::GuardrailPack;
    use memory_common::policy::{CanonicalRule, DeliveryClass, PolicySelectors};
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::{ReplaceStage, with_replace_fault, with_replace_pause};
    use crate::config::ClientAdapter;
    use crate::error::Error;
    use crate::installation::Installation;

    const STAGES: [ReplaceStage; 6] = [
        ReplaceStage::Create,
        ReplaceStage::Write,
        ReplaceStage::Flush,
        ReplaceStage::FileSync,
        ReplaceStage::Rename,
        ReplaceStage::ParentSync,
    ];

    struct Rig {
        root: TempDir,
        cwd: PathBuf,
        config: PathBuf,
        id: Uuid,
        installation: Installation,
    }

    impl Rig {
        fn new() -> Self {
            let base = std::env::var_os("MEMORY_HOOKS_TEST_ROOT")
                .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
                .map_or_else(std::env::temp_dir, PathBuf::from);
            let root = tempfile::tempdir_in(base).expect("private fixture");
            let cwd = root.path().join("workspace");
            fs::create_dir(&cwd).expect("workspace");
            let config = root.path().join("config.toml");
            let anchor = root.path().join("control");
            let id = Installation::initialize(&anchor, ClientAdapter::CodexV1).expect("initialize");
            let installation =
                Installation::open(&anchor, id, ClientAdapter::CodexV1).expect("open");
            let rig = Self {
                root,
                cwd,
                config,
                id,
                installation,
            };
            rig.write_config();
            rig.installation.activate(&rig.config).expect("activate");
            rig
        }

        fn write_config(&self) {
            let source = format!(
                "schema_version = 2\nstate_root = {:?}\ngit_executable = \"/usr/bin/git\"\n[[bindings]]\nkind = \"directory\"\nlabel = \"plain\"\nroot = {:?}\nguardrails_project = \"general\"\n[bindings.context]\nprofile = \"workstation\"\n[transport]\nmemoryd_url = \"http://127.0.0.1:55486/\"\nunauthenticated = true\ncredential_revision = \"initial\"\n[client]\nadapter = \"codex-v1\"\ncontract = \"managed-session-start-v1\"\nasync_hook = false\ntimeout_seconds = 90\nadditional_context_limit = 0\n",
                self.root.path().join("snapshots").display().to_string(),
                self.cwd.display().to_string()
            );
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .mode(0o600)
                .open(&self.config)
                .expect("config");
            file.write_all(source.as_bytes()).expect("config bytes");
        }

        fn reopen(&self) -> Installation {
            Installation::open(
                &self.root.path().join("control"),
                self.id,
                ClientAdapter::CodexV1,
            )
            .expect("reopen")
        }

        fn pack(&self) -> GuardrailPack {
            pack_for(&self.installation, &self.cwd)
        }

        fn emit(&self, session: &str) {
            let pending = self.installation.begin_session(session).expect("pending");
            self.installation
                .attach_binding(&pending, &self.cwd)
                .expect("binding");
            self.installation
                .complete_session(&pending, &self.cwd, self.pack(), &mut Vec::new())
                .expect("emitted");
        }
    }

    fn pack_for(installation: &Installation, cwd: &Path) -> GuardrailPack {
        let binding = installation
            .active()
            .expect("active")
            .config
            .resolve(cwd)
            .expect("binding");
        GuardrailPack::new(
            binding.project().to_owned(),
            binding.context().clone(),
            1,
            vec![CanonicalRule {
                project: "general".to_owned(),
                id: Uuid::from_u128(1),
                policy_key: Some("exact".to_owned()),
                revision: Some(1),
                delivery_class: Some(DeliveryClass::Mandatory),
                selectors: PolicySelectors::default(),
                values: BTreeMap::new(),
                content: "fault test marker".to_owned(),
                overrides: None,
            }],
        )
        .expect("pack")
    }

    #[test]
    fn replacement_crash_worker() {
        let Ok(case) = std::env::var("MEMORY_HOOKS_REPLACE_CRASH_CASE") else {
            return;
        };
        let control =
            PathBuf::from(std::env::var_os("MEMORY_HOOKS_REPLACE_CRASH_CONTROL").expect("control"));
        let cwd = PathBuf::from(std::env::var_os("MEMORY_HOOKS_REPLACE_CRASH_CWD").expect("cwd"));
        let barrier =
            PathBuf::from(std::env::var_os("MEMORY_HOOKS_REPLACE_CRASH_BARRIER").expect("barrier"));
        let id = std::env::var("MEMORY_HOOKS_REPLACE_CRASH_ID")
            .expect("installation id")
            .parse()
            .expect("UUID");
        let installation =
            Installation::open(&control, id, ClientAdapter::CodexV1).expect("open installation");
        let pending = installation.begin_session("crash").expect("claim");
        installation
            .attach_binding(&pending, &cwd)
            .expect("binding");
        let pack = pack_for(&installation, &cwd);
        let (prefix, stage, occurrence) = match case.as_str() {
            "payload_before_rename" => ("payload-", ReplaceStage::Rename, 1),
            "payload_after_sync" => ("payload-", ReplaceStage::AfterParentSync, 1),
            "emitted_head_after_sync" => ("head-", ReplaceStage::AfterParentSync, 2),
            "resolution_before_rename" => ("journal-", ReplaceStage::Rename, 4),
            "resolution_after_sync" => ("journal-", ReplaceStage::AfterParentSync, 4),
            _ => panic!("unknown crash case"),
        };
        with_replace_pause(prefix, stage, occurrence, barrier, || {
            installation.complete_session(&pending, &cwd, pack, &mut Vec::new())
        })
        .expect("worker must pause at replacement stage");
        panic!("worker completed without reaching crash barrier");
    }

    #[test]
    fn killed_replacement_workers_reopen_at_the_last_durable_commit_boundary() {
        for case in [
            "payload_before_rename",
            "payload_after_sync",
            "emitted_head_after_sync",
            "resolution_before_rename",
            "resolution_after_sync",
        ] {
            let rig = Rig::new();
            let barrier = rig.root.path().join("replacement-crash-barrier");
            let mut child = Command::new(std::env::current_exe().expect("test executable"))
                .args([
                    "--exact",
                    "state::journal_fault_tests::replacement_crash_worker",
                ])
                .env("MEMORY_HOOKS_REPLACE_CRASH_CASE", case)
                .env(
                    "MEMORY_HOOKS_REPLACE_CRASH_CONTROL",
                    rig.root.path().join("control"),
                )
                .env("MEMORY_HOOKS_REPLACE_CRASH_CWD", &rig.cwd)
                .env("MEMORY_HOOKS_REPLACE_CRASH_BARRIER", &barrier)
                .env("MEMORY_HOOKS_REPLACE_CRASH_ID", rig.id.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("crash worker");
            let deadline = Instant::now() + Duration::from_secs(8);
            while !barrier.exists() && Instant::now() < deadline {
                assert!(child.try_wait().expect("worker status").is_none(), "{case}");
                thread::sleep(Duration::from_millis(5));
            }
            if !barrier.exists() {
                child.kill().expect("terminate missing barrier worker");
                child.wait().expect("reap missing barrier worker");
                panic!("worker did not reach {case}");
            }
            child.kill().expect("terminate worker");
            child.wait().expect("reap worker");
            let reopened = rig.reopen();
            if case == "resolution_after_sync" {
                assert_eq!(
                    reopened
                        .read_snapshot("crash", &rig.cwd)
                        .expect("fully committed reader")
                        .pack,
                    rig.pack(),
                    "{case}"
                );
            } else {
                assert!(reopened.read_snapshot("crash", &rig.cwd).is_err(), "{case}");
            }
            if reopened.begin_session("crash").is_err() {
                reopened
                    .repair_session("crash")
                    .expect("repair unresolved head");
            }
            rig.emit("crash");
            assert_eq!(
                rig.reopen()
                    .read_snapshot("crash", &rig.cwd)
                    .expect("fresh reader")
                    .pack,
                rig.pack(),
                "{case}"
            );
        }
    }

    #[test]
    fn final_emitted_journal_sync_error_remains_unusable_after_reopen() {
        let rig = Rig::new();
        let pending = rig.installation.begin_session("session").expect("pending");
        rig.installation
            .attach_binding(&pending, &rig.cwd)
            .expect("binding");
        let journal = fs::read_dir(rig.root.path().join("control"))
            .expect("control entries")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .find(|name| name.starts_with("journal-"))
            .expect("session journal");
        let mut output = Vec::new();
        let result = with_replace_fault(&journal, ReplaceStage::ParentSync, 4, || {
            rig.installation
                .complete_session(&pending, &rig.cwd, rig.pack(), &mut output)
        });
        assert_eq!(result, Err(Error::DurabilityUncertain));
        assert_eq!(output.last(), Some(&b'\n'));
        let reopened = rig.reopen();
        assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
        assert!(reopened.begin_session("session").is_err());
        reopened.repair_session("session").expect("repair");
        assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
        rig.emit("session");
        assert!(rig.reopen().read_snapshot("session", &rig.cwd).is_ok());
    }

    #[test]
    fn final_active_journal_sync_error_retires_old_epoch_after_reopen() {
        let rig = Rig::new();
        rig.emit("session");
        let result = with_replace_fault("activation-journal", ReplaceStage::ParentSync, 4, || {
            rig.installation.activate(&rig.config)
        });
        assert_eq!(result, Err(Error::DurabilityUncertain));
        let reopened = rig.reopen();
        assert!(reopened.active().is_err());
        assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
        reopened.repair_incomplete().expect("repair");
        assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
        reopened.activate(&rig.config).expect("reactivate");
        assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
        rig.emit("session");
        assert!(rig.reopen().read_snapshot("session", &rig.cwd).is_ok());
    }

    #[test]
    fn private_replace_does_not_accept_oversized_record() {
        let rig = Rig::new();
        let private = super::PrivateDirectory::open(
            &rig.root.path().join("snapshots"),
            super::RootMode::Existing,
        )
        .expect("private root");
        assert_eq!(
            private.replace("limit", &vec![0; crate::limits::RECORD_BYTES + 1]),
            Err(Error::StateCorrupt)
        );
        assert!(private.read("limit").expect("read").is_none());
        private
            .replace("limit", &vec![b'x'; crate::limits::RECORD_BYTES])
            .expect("exact record limit");
        assert_eq!(
            private
                .read("limit")
                .expect("bounded read")
                .expect("record")
                .len(),
            crate::limits::RECORD_BYTES
        );
        let mut record = OpenOptions::new()
            .append(true)
            .open(rig.root.path().join("snapshots/limit"))
            .expect("record append");
        record.write_all(b"x").expect("one byte over");
        assert_eq!(private.read("limit"), Err(Error::StateCorrupt));
    }

    #[test]
    fn replacement_stage_faults_preserve_the_old_record_until_rename() {
        for stage in STAGES {
            let rig = Rig::new();
            let directory = super::PrivateDirectory::open(
                &rig.root.path().join("snapshots"),
                super::RootMode::Existing,
            )
            .expect("payload root");
            directory.replace("record", b"old").expect("old record");
            let result = with_replace_fault("record", stage, 1, || {
                directory.replace("record", b"successor")
            });
            assert!(result.is_err(), "{stage:?}");
            assert_eq!(
                directory.read("record").expect("record read"),
                Some(if stage == ReplaceStage::ParentSync {
                    b"successor".to_vec()
                } else {
                    b"old".to_vec()
                }),
                "{stage:?}"
            );
            directory
                .replace("record", b"fresh")
                .expect("recovery write");
            assert_eq!(
                directory.read("record").expect("read"),
                Some(b"fresh".to_vec())
            );
        }
    }

    #[test]
    fn payload_replacement_faults_never_emit_or_authorize_orphan_data() {
        for stage in STAGES {
            let rig = Rig::new();
            let pending = rig.installation.begin_session("session").expect("pending");
            rig.installation
                .attach_binding(&pending, &rig.cwd)
                .expect("binding");
            let mut output = Vec::new();
            let result = with_replace_fault("payload-", stage, 1, || {
                rig.installation
                    .complete_session(&pending, &rig.cwd, rig.pack(), &mut output)
            });
            assert!(result.is_err(), "{stage:?}");
            assert!(output.is_empty(), "{stage:?}");
            let reopened = rig.reopen();
            assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
            rig.emit("session");
            assert!(rig.reopen().read_snapshot("session", &rig.cwd).is_ok());
        }
    }

    #[test]
    fn prepared_head_and_journal_faults_require_a_fresh_generation() {
        for prefix in ["journal-", "head-"] {
            for stage in STAGES {
                let rig = Rig::new();
                let pending = rig.installation.begin_session("session").expect("pending");
                rig.installation
                    .attach_binding(&pending, &rig.cwd)
                    .expect("binding");
                let mut output = Vec::new();
                let result = with_replace_fault(prefix, stage, 1, || {
                    rig.installation
                        .complete_session(&pending, &rig.cwd, rig.pack(), &mut output)
                });
                assert!(result.is_err(), "{prefix} {stage:?}");
                assert!(output.is_empty(), "{prefix} {stage:?}");
                let reopened = rig.reopen();
                assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
                if reopened.begin_session("session").is_err() {
                    reopened.repair_session("session").expect("repair");
                }
                rig.emit("session");
                assert!(rig.reopen().read_snapshot("session", &rig.cwd).is_ok());
            }
        }
    }

    #[test]
    fn final_emitted_head_and_journal_faults_reject_complete_output() {
        for (prefix, occurrence) in [("journal-", 4), ("head-", 2)] {
            for stage in STAGES {
                let rig = Rig::new();
                let pending = rig.installation.begin_session("session").expect("pending");
                rig.installation
                    .attach_binding(&pending, &rig.cwd)
                    .expect("binding");
                let mut output = Vec::new();
                let result = with_replace_fault(prefix, stage, occurrence, || {
                    rig.installation
                        .complete_session(&pending, &rig.cwd, rig.pack(), &mut output)
                });
                assert!(result.is_err(), "{prefix} {stage:?}");
                assert_eq!(output.last(), Some(&b'\n'), "{prefix} {stage:?}");
                let reopened = rig.reopen();
                assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
                reopened.repair_session("session").expect("repair");
                rig.emit("session");
                assert!(rig.reopen().read_snapshot("session", &rig.cwd).is_ok());
            }
        }
    }

    #[test]
    fn installation_journal_and_manifest_faults_retire_only_after_a_durable_barrier() {
        for prefix in ["activation-journal", "installation-manifest"] {
            for stage in STAGES {
                let rig = Rig::new();
                rig.emit("session");
                let result =
                    with_replace_fault(prefix, stage, 1, || rig.installation.activate(&rig.config));
                assert!(result.is_err(), "{prefix} {stage:?}");
                let reopened = rig.reopen();
                let prior_survives =
                    prefix == "activation-journal" && stage != ReplaceStage::ParentSync;
                assert_eq!(
                    reopened.read_snapshot("session", &rig.cwd).is_ok(),
                    prior_survives,
                    "{prefix} {stage:?}"
                );
                if !prior_survives {
                    reopened.repair_incomplete().expect("repair");
                }
                reopened.activate(&rig.config).expect("fresh activation");
                assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
                rig.emit("session");
                assert!(rig.reopen().read_snapshot("session", &rig.cwd).is_ok());
            }
        }
    }

    #[test]
    fn later_activation_resolution_faults_never_promote_an_active_manifest() {
        for (prefix, occurrence) in [
            ("activation-journal", 2),
            ("activation-journal", 4),
            ("installation-manifest", 2),
        ] {
            for stage in STAGES {
                let rig = Rig::new();
                rig.emit("session");
                let result = with_replace_fault(prefix, stage, occurrence, || {
                    rig.installation.activate(&rig.config)
                });
                assert!(result.is_err(), "{prefix} {occurrence} {stage:?}");
                let reopened = rig.reopen();
                assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
                reopened.repair_incomplete().expect("repair");
                assert!(reopened.read_snapshot("session", &rig.cwd).is_err());
                reopened.activate(&rig.config).expect("fresh activation");
                rig.emit("session");
                assert!(rig.reopen().read_snapshot("session", &rig.cwd).is_ok());
            }
        }
    }
}
