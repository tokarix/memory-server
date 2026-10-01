//! Canonical local repository identity and checked worktree membership.

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use memory_common::policy::ResolutionContext;
use rustix::fs::{Mode, OFlags, fstat, openat};
use rustix::process::{Pid, Signal, kill_process_group};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::config::{
    BindingIdentity, TrustedHooksConfig, hash_part, marker_root, path_bytes, valid_path, within,
};
use crate::error::{Error, Result};
use crate::limits;

/// Local identity keyed by a canonical Git common directory or approved directory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryIdentity {
    /// A Git common directory selected by trusted config.
    Git(PathBuf),
    /// An explicitly approved non-Git directory.
    Directory(PathBuf),
}

/// Complete trusted binding for a real current directory.
#[derive(Clone)]
pub struct ResolvedBinding {
    identity: RepositoryIdentity,
    worktree_root: PathBuf,
    effective_cwd: PathBuf,
    project: String,
    context: ResolutionContext,
    fingerprint: String,
    storage_execution: Option<crate::storage::attestation::StorageExecution>,
}

#[derive(Serialize)]
struct PublicBinding<'a> {
    version: u32,
    identity_kind: &'static str,
    identity_sha256: String,
    worktree_sha256: String,
    cwd_sha256: String,
    guardrails_project: &'a str,
    context: &'a ResolutionContext,
    fingerprint: &'a str,
}

fn digest_path(domain: &[u8], path: &Path) -> String {
    let mut hash = Sha256::new();
    hash.update(b"memory-hooks-public-path-v1\0");
    hash_part(&mut hash, domain);
    hash_part(&mut hash, path_bytes(path));
    format!("{:x}", hash.finalize())
}

impl ResolvedBinding {
    pub(crate) fn storage_execution(
        &self,
    ) -> Option<&crate::storage::attestation::StorageExecution> {
        self.storage_execution.as_ref().filter(|storage| {
            let root = match &self.identity {
                RepositoryIdentity::Git(root) | RepositoryIdentity::Directory(root) => root,
            };
            root == &storage.binding_root && self.project == storage.project
        })
    }
    /// Canonical repository or explicit directory identity.
    #[must_use]
    pub const fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }
    /// Canonical root of the actual checkout, or approved directory scope.
    #[must_use]
    pub fn worktree_root(&self) -> &Path {
        &self.worktree_root
    }
    /// Canonical effective location supplied by the caller.
    #[must_use]
    pub fn effective_cwd(&self) -> &Path {
        &self.effective_cwd
    }
    /// Explicit guardrails project.
    #[must_use]
    pub fn project(&self) -> &str {
        &self.project
    }
    /// Complete trusted context, retaining omitted versus known-empty dimensions.
    #[must_use]
    pub const fn context(&self) -> &ResolutionContext {
        &self.context
    }
    /// Versioned normalized config/binding fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
    /// Versioned nonsecret representation for the administrative CLI.
    ///
    /// # Errors
    /// Returns a safe serialization failure code.
    pub fn public_json(&self) -> Result<String> {
        let (kind, bytes) = match &self.identity {
            RepositoryIdentity::Git(path) => ("git", path_bytes(path)),
            RepositoryIdentity::Directory(path) => ("directory", path_bytes(path)),
        };
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-public-identity-v1\0");
        hash_part(&mut hash, kind.as_bytes());
        hash_part(&mut hash, bytes);
        let value = PublicBinding {
            version: 1,
            identity_kind: kind,
            identity_sha256: format!("{:x}", hash.finalize()),
            worktree_sha256: digest_path(b"worktree", &self.worktree_root),
            cwd_sha256: digest_path(b"cwd", &self.effective_cwd),
            guardrails_project: &self.project,
            context: &self.context,
            fingerprint: &self.fingerprint,
        };
        serde_json::to_string(&value).map_err(|_| Error::IdentityInvalid("JSON"))
    }
}

struct GitFacts {
    top: PathBuf,
    git_dir: PathBuf,
    common: PathBuf,
    worktrees: Vec<u8>,
}

fn read_pipe(mut reader: impl Read, total: &AtomicUsize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        total.fetch_add(count, Ordering::Relaxed);
        if output.len() < limits::PROBE_BYTES + 1 {
            let remaining = limits::PROBE_BYTES + 1 - output.len();
            output.extend_from_slice(&chunk[..count.min(remaining)]);
        }
    }
    Ok(output)
}

fn probe(git: &Path, cwd: &Path, args: &[&str], overall: Instant) -> Result<Vec<u8>> {
    if overall.elapsed() >= limits::RESOLVE_DEADLINE {
        return Err(Error::GitTimeout);
    }
    let mut command = Command::new(git);
    command
        .current_dir(cwd)
        .process_group(0)
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|_| Error::IdentityInvalid("git spawn"))?;
    let group = Pid::from_child(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or(Error::IdentityInvalid("git stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(Error::IdentityInvalid("git stderr"))?;
    let total = Arc::new(AtomicUsize::new(0));
    let deadline = Instant::now()
        + limits::SHORT_DEADLINE.min(limits::RESOLVE_DEADLINE.saturating_sub(overall.elapsed()));
    std::thread::scope(|scope| {
        let out_total = Arc::clone(&total);
        let err_total = Arc::clone(&total);
        let out = scope.spawn(move || read_pipe(stdout, &out_total));
        let err = scope.spawn(move || read_pipe(stderr, &err_total));
        let status = loop {
            if total.load(Ordering::Relaxed) > limits::PROBE_BYTES {
                let _ = kill_process_group(group, Signal::KILL);
                let _ = child.wait();
                break Err(Error::IdentityInvalid("git output limit"));
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = kill_process_group(group, Signal::KILL);
                    let _ = child.wait();
                    break Err(Error::GitTimeout);
                }
                Err(_) => {
                    let _ = kill_process_group(group, Signal::KILL);
                    let _ = child.wait();
                    break Err(Error::IdentityInvalid("git wait"));
                }
            }
        };
        let stdout = out
            .join()
            .map_err(|_| Error::IdentityInvalid("git stdout thread"))?
            .map_err(|_| Error::IdentityInvalid("git stdout read"))?;
        err.join()
            .map_err(|_| Error::IdentityInvalid("git stderr thread"))?
            .map_err(|_| Error::IdentityInvalid("git stderr read"))?;
        if total.load(Ordering::Relaxed) > limits::PROBE_BYTES {
            return Err(Error::IdentityInvalid("git output limit"));
        }
        if !status?.success() {
            return Err(Error::IdentityInvalid("git probe"));
        }
        Ok(stdout)
    })
}

fn line(bytes: &[u8]) -> Result<&[u8]> {
    let content = bytes
        .strip_suffix(b"\n")
        .ok_or(Error::IdentityInvalid("git output"))?;
    if content.is_empty() || content.iter().any(u8::is_ascii_control) {
        return Err(Error::IdentityInvalid("git output"));
    }
    Ok(content)
}

fn path_line(bytes: &[u8]) -> Result<PathBuf> {
    let value = PathBuf::from(std::ffi::OsString::from_vec(line(bytes)?.to_vec()));
    valid_path(&value).map_err(|_| Error::IdentityInvalid("git path"))?;
    fs::canonicalize(value).map_err(|_| Error::IdentityInvalid("git path"))
}

fn read_metadata(path: &Path) -> Result<Vec<u8>> {
    let file = File::from(
        openat(
            rustix::fs::CWD,
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Error::IdentityInvalid("git metadata open"))?,
    );
    let meta = fstat(&file).map_err(|_| Error::IdentityInvalid("git metadata stat"))?;
    let limit = i64::try_from(limits::PATH_BYTES)
        .map_err(|_| Error::IdentityInvalid("git metadata limit"))?;
    if meta.st_mode & libc::S_IFMT != libc::S_IFREG || meta.st_size > limit {
        return Err(Error::IdentityInvalid("git metadata type/size"));
    }
    let mut bytes =
        Vec::with_capacity(limits::PATH_BYTES.min(usize::try_from(meta.st_size).unwrap_or(0)));
    file.take((limits::PATH_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| Error::IdentityInvalid("git metadata read"))?;
    if bytes.len() > limits::PATH_BYTES {
        return Err(Error::IdentityInvalid("git metadata size"));
    }
    Ok(bytes)
}

fn metadata_stamp(path: &Path) -> Result<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| Error::IdentityInvalid("git metadata"))?;
    let mut stamp = Vec::with_capacity(72);
    stamp.extend_from_slice(&metadata.dev().to_be_bytes());
    stamp.extend_from_slice(&metadata.ino().to_be_bytes());
    stamp.extend_from_slice(&metadata.mode().to_be_bytes());
    stamp.extend_from_slice(&metadata.uid().to_be_bytes());
    stamp.extend_from_slice(&metadata.len().to_be_bytes());
    stamp.extend_from_slice(&metadata.mtime().to_be_bytes());
    stamp.extend_from_slice(&metadata.mtime_nsec().to_be_bytes());
    stamp.extend_from_slice(&metadata.ctime().to_be_bytes());
    stamp.extend_from_slice(&metadata.ctime_nsec().to_be_bytes());
    Ok(stamp)
}

fn gitfile_target(marker: &Path) -> Result<PathBuf> {
    let bytes = read_metadata(marker)?;
    let target = bytes
        .strip_prefix(b"gitdir: ")
        .ok_or(Error::IdentityInvalid("gitfile"))?;
    let target = line(target)?;
    let path = PathBuf::from(std::ffi::OsString::from_vec(target.to_vec()));
    let resolved = if path.is_absolute() {
        path
    } else {
        marker
            .parent()
            .ok_or(Error::IdentityInvalid("gitfile parent"))?
            .join(path)
    };
    fs::canonicalize(resolved).map_err(|_| Error::IdentityInvalid("gitfile target"))
}

fn facts(config: &TrustedHooksConfig, cwd: &Path, overall: Instant) -> Result<GitFacts> {
    let top = path_line(&probe(
        &config.git,
        cwd,
        &["rev-parse", "--path-format=absolute", "--show-toplevel"],
        overall,
    )?)?;
    let git_dir = path_line(&probe(
        &config.git,
        cwd,
        &["rev-parse", "--absolute-git-dir"],
        overall,
    )?)?;
    let common = path_line(&probe(
        &config.git,
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        overall,
    )?)?;
    let bare = probe(
        &config.git,
        cwd,
        &["rev-parse", "--is-bare-repository"],
        overall,
    )?;
    if bare != b"false\n" {
        return Err(Error::IdentityInvalid("bare checkout"));
    }
    let worktrees = probe(
        &config.git,
        cwd,
        &["worktree", "list", "--porcelain"],
        overall,
    )?;
    Ok(GitFacts {
        top,
        git_dir,
        common,
        worktrees,
    })
}

fn listed_worktree(output: &[u8], top: &Path) -> Result<bool> {
    let mut found = false;
    for item in output.split(|byte| *byte == b'\n') {
        if let Some(value) = item.strip_prefix(b"worktree ") {
            if value.is_empty() || value.iter().any(u8::is_ascii_control) {
                return Err(Error::IdentityInvalid("worktree list"));
            }
            let path = PathBuf::from(std::ffi::OsString::from_vec(value.to_vec()));
            let canonical =
                fs::canonicalize(path).map_err(|_| Error::IdentityInvalid("worktree list"))?;
            if canonical == top {
                found = true;
            }
        }
    }
    Ok(found)
}

fn validate_membership(facts: &GitFacts, primary: Option<&Path>) -> Result<Vec<Vec<u8>>> {
    let marker = facts.top.join(".git");
    let is_primary = primary == Some(facts.top.as_path());
    let mut snapshot = vec![
        metadata_stamp(&marker)?,
        metadata_stamp(&facts.git_dir)?,
        metadata_stamp(&facts.common)?,
    ];
    if let Some(primary) = primary.filter(|primary| *primary == facts.top) {
        let meta =
            fs::symlink_metadata(&marker).map_err(|_| Error::IdentityInvalid("primary marker"))?;
        if meta.file_type().is_dir() {
            if fs::canonicalize(&marker).map_err(|_| Error::IdentityInvalid("primary marker"))?
                != facts.common
                || facts.git_dir != facts.common
            {
                return Err(Error::IdentityInvalid("primary common directory"));
            }
        } else if meta.file_type().is_file() {
            if gitfile_target(&marker)? != facts.git_dir || facts.git_dir != facts.common {
                return Err(Error::IdentityInvalid("primary gitfile"));
            }
            snapshot.push(read_metadata(&marker)?);
        } else {
            return Err(Error::IdentityInvalid("primary marker type"));
        }
        let _ = primary;
    } else {
        if !facts.git_dir.starts_with(facts.common.join("worktrees"))
            || facts.git_dir.parent() != Some(facts.common.join("worktrees").as_path())
        {
            return Err(Error::IdentityInvalid("worktree registration"));
        }
        if gitfile_target(&marker)? != facts.git_dir {
            return Err(Error::IdentityInvalid("worktree gitfile"));
        }
        let backpointer = facts.git_dir.join("gitdir");
        snapshot.push(metadata_stamp(&backpointer)?);
        let pointer = read_metadata(&backpointer)?;
        let target = PathBuf::from(std::ffi::OsString::from_vec(line(&pointer)?.to_vec()));
        let target = if target.is_absolute() {
            target
        } else {
            facts.git_dir.join(target)
        };
        if target != marker
            || fs::canonicalize(&target)
                .map_err(|_| Error::IdentityInvalid("worktree backpointer"))?
                != marker
        {
            return Err(Error::IdentityInvalid("worktree backpointer"));
        }
        let commondir = facts.git_dir.join("commondir");
        snapshot.push(metadata_stamp(&commondir)?);
        let value = read_metadata(&commondir)?;
        let relative = PathBuf::from(std::ffi::OsString::from_vec(line(&value)?.to_vec()));
        if fs::canonicalize(facts.git_dir.join(relative))
            .map_err(|_| Error::IdentityInvalid("commondir"))?
            != facts.common
        {
            return Err(Error::IdentityInvalid("commondir"));
        }
        snapshot.extend([read_metadata(&marker)?, pointer, value]);
    }
    // Git reports the administration directory, not the checkout, for a
    // primary repository initialized with --separate-git-dir.
    if !is_primary && !listed_worktree(&facts.worktrees, &facts.top)? {
        return Err(Error::IdentityInvalid("unregistered worktree"));
    }
    Ok(snapshot)
}

impl TrustedHooksConfig {
    /// Resolve a real absolute location to exactly one configured identity.
    ///
    /// # Errors
    /// Returns a stable reason when the location is unmapped, unsafe, or changed.
    pub fn resolve(&self, cwd: &Path) -> Result<ResolvedBinding> {
        valid_path(cwd).map_err(|_| Error::IdentityInvalid("cwd"))?;
        let cwd = fs::canonicalize(cwd).map_err(|_| Error::IdentityInvalid("cwd"))?;
        if !cwd.is_dir() {
            return Err(Error::IdentityInvalid("cwd directory"));
        }
        let overall = Instant::now();
        if let Some(nearest) = marker_root(&cwd)? {
            let first = facts(self, &cwd, overall)?;
            if first.top != nearest {
                return Err(Error::IdentityInvalid("nearest repository"));
            }
            if within(&self.config_path, &first.top)
                || within(&self.state_root, &first.top)
                || within(&first.top, &self.state_root)
                || within(&self.helper_path, &first.top)
            {
                return Err(Error::IdentityInvalid("installation inside worktree"));
            }
            let binding = self
                .bindings
                .iter()
                .find(|binding| {
                    matches!(&binding.identity,
                BindingIdentity::Git { common, .. } if *common == first.common)
                })
                .ok_or(Error::IdentityUnmapped)?;
            let BindingIdentity::Git { primary, .. } = &binding.identity else {
                return Err(Error::IdentityInvalid("binding kind"));
            };
            let before = validate_membership(&first, primary.as_deref())?;
            let second = facts(self, &cwd, overall)?;
            let after = validate_membership(&second, primary.as_deref())?;
            if first.top != second.top
                || first.git_dir != second.git_dir
                || first.common != second.common
                || first.worktrees != second.worktrees
                || before != after
            {
                return Err(Error::IdentityChanged);
            }
            return Ok(ResolvedBinding {
                identity: RepositoryIdentity::Git(first.common),
                worktree_root: first.top,
                effective_cwd: cwd,
                project: binding.project.clone(),
                context: binding.context.clone(),
                fingerprint: self.fingerprint.clone(),
                storage_execution: self.storage_execution.clone(),
            });
        }
        let matches: Vec<_> = self
            .bindings
            .iter()
            .filter(|binding| match &binding.identity {
                BindingIdentity::Directory { root, descendants } => {
                    cwd == *root || (*descendants && within(&cwd, root))
                }
                BindingIdentity::Git { .. } => false,
            })
            .collect();
        if matches.len() > 1 {
            return Err(Error::IdentityAmbiguous);
        }
        let binding = matches.first().ok_or(Error::IdentityUnmapped)?;
        let BindingIdentity::Directory { root, .. } = &binding.identity else {
            return Err(Error::IdentityInvalid("binding kind"));
        };
        Ok(ResolvedBinding {
            identity: RepositoryIdentity::Directory(root.clone()),
            worktree_root: root.clone(),
            effective_cwd: cwd,
            project: binding.project.clone(),
            context: binding.context.clone(),
            fingerprint: self.fingerprint.clone(),
            storage_execution: self.storage_execution.clone(),
        })
    }
}
