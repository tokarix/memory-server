//! Strict operator-selected configuration and normalized trust fingerprint.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use memory_common::policy::ResolutionContext;
use rustix::fs::{Mode, OFlags, fgetxattr, fstat, openat};
use rustix::io::Errno;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::{Error, Result, io};
use crate::limits;

/// One trusted binding, canonicalized at load time.
#[derive(Clone)]
pub(crate) struct Binding {
    pub(crate) identity: BindingIdentity,
    pub(crate) project: String,
    pub(crate) context: ResolutionContext,
}

/// Configured local identity; directory paths retain OS bytes.
#[derive(Clone)]
pub(crate) enum BindingIdentity {
    Git {
        common: PathBuf,
        primary: Option<PathBuf>,
    },
    Directory {
        root: PathBuf,
        descendants: bool,
    },
}

/// Validated operator configuration; there is no unchecked public constructor.
#[expect(
    dead_code,
    reason = "identity and state consumers arrive in the next commits"
)]
pub struct TrustedHooksConfig {
    pub(crate) git: PathBuf,
    pub(crate) config_path: PathBuf,
    pub(crate) helper_path: PathBuf,
    pub(crate) state_root: PathBuf,
    pub(crate) bindings: Vec<Binding>,
    pub(crate) fingerprint: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    state_root: PathBuf,
    git_executable: PathBuf,
    bindings: Vec<RawBinding>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawBinding {
    Git {
        label: String,
        common_dir: PathBuf,
        primary_worktree: Option<PathBuf>,
        #[serde(default)]
        bare_backed: bool,
        guardrails_project: String,
        context: ResolutionContext,
    },
    Directory {
        label: String,
        root: PathBuf,
        #[serde(default)]
        allow_subdirectories: bool,
        guardrails_project: String,
        context: ResolutionContext,
    },
}

pub(crate) fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

pub(crate) fn valid_path(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path_bytes(path).len() > limits::PATH_BYTES
        || path_bytes(path)
            .split(|byte| *byte == b'/')
            .any(|part| part == b"." || part == b"..")
    {
        return Err(Error::ConfigInvalid("absolute bounded path"));
    }
    Ok(())
}

pub(crate) fn within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

pub(crate) fn marker_root(cwd: &Path) -> Result<Option<PathBuf>> {
    for ancestor in cwd.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(meta) => {
                if !meta.file_type().is_file() && !meta.file_type().is_dir() {
                    return Err(Error::IdentityInvalid(".git marker type"));
                }
                return Ok(Some(ancestor.to_path_buf()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(Error::IdentityInvalid(".git marker access")),
        }
        let head = std::fs::symlink_metadata(ancestor.join("HEAD"));
        let objects = std::fs::symlink_metadata(ancestor.join("objects"));
        let config = std::fs::symlink_metadata(ancestor.join("config"));
        if let (Ok(head), Ok(objects), Ok(config)) = (head, objects, config)
            && head.file_type().is_file()
            && objects.file_type().is_dir()
            && config.file_type().is_file()
        {
            return Ok(Some(ancestor.to_path_buf()));
        }
    }
    Ok(None)
}

pub(crate) fn hash_part(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn check_acl(file: &File, kind: &'static str) -> Result<()> {
    match fgetxattr(file, "system.posix_acl_access", &mut [] as &mut [u8]) {
        Err(Errno::NODATA) => Ok(()),
        Ok(_) | Err(Errno::RANGE) => Err(Error::ConfigUntrusted(kind)),
        Err(_) => Err(Error::ConfigUntrusted("acl verification")),
    }
}

fn check_component(file: &File, final_file: bool, executable: bool) -> Result<()> {
    let stat = fstat(file).map_err(|_| Error::ConfigUntrusted("stat"))?;
    let euid = rustix::process::geteuid().as_raw();
    if stat.st_uid != euid && stat.st_uid != 0 {
        return Err(Error::ConfigUntrusted("owner"));
    }
    if final_file && executable {
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || stat.st_mode & 0o022 != 0
            || stat.st_mode & 0o100 == 0
            || stat.st_mode & 0o6000 != 0
        {
            return Err(Error::ConfigUntrusted("Git executable mode"));
        }
    } else if final_file {
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG
            || !matches!(stat.st_mode & 0o7777, 0o400 | 0o600)
        {
            return Err(Error::ConfigUntrusted("config file mode"));
        }
    } else if stat.st_mode & libc::S_IFMT != libc::S_IFDIR || stat.st_mode & 0o022 != 0 {
        return Err(Error::ConfigUntrusted("config parent mode"));
    }
    check_acl(file, "acl")
}

fn open_trusted_path(path: &Path, executable: bool) -> Result<File> {
    valid_path(path)?;
    let root = File::from(
        openat(
            rustix::fs::CWD,
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| Error::ConfigUntrusted("root"))?,
    );
    check_component(&root, false, executable)?;
    let mut current = root;
    let mut parts = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .peekable();
    while let Some(part) = parts.next() {
        let final_file = parts.peek().is_none();
        let mut flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        if !final_file {
            flags |= OFlags::DIRECTORY;
        }
        let next = File::from(
            openat(&current, part, flags, Mode::empty())
                .map_err(|_| Error::ConfigUntrusted("path component"))?,
        );
        check_component(&next, final_file, executable)?;
        current = next;
    }
    if path.file_name().is_none() {
        return Err(Error::ConfigInvalid("config path"));
    }
    Ok(current)
}

fn open_trusted_config(path: &Path) -> Result<File> {
    open_trusted_path(path, false)
}

fn canonical(path: &Path, field: &'static str) -> Result<PathBuf> {
    valid_path(path)?;
    std::fs::canonicalize(path).map_err(|_| Error::ConfigInvalid(field))
}

fn reject_symlink_components(path: &Path, field: &'static str) -> Result<()> {
    let mut walked = PathBuf::from("/");
    for part in path.components() {
        if let Component::Normal(name) = part {
            walked.push(name);
            match std::fs::symlink_metadata(&walked) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(Error::ConfigUntrusted(field));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && walked == path => {}
                Err(_) => return Err(Error::ConfigUntrusted(field)),
            }
        }
    }
    Ok(())
}

fn fingerprint(
    config_path: &Path,
    state: &Path,
    git: &Path,
    bindings: &[Binding],
) -> Result<String> {
    let mut entries = Vec::with_capacity(bindings.len());
    for binding in bindings {
        let mut hasher = Sha256::new();
        hasher.update(b"memory-hooks-binding-v1\0");
        match &binding.identity {
            BindingIdentity::Git { common, primary } => {
                hash_part(&mut hasher, b"git");
                hash_part(&mut hasher, path_bytes(common));
                hash_part(&mut hasher, primary.as_deref().map_or(&[][..], path_bytes));
            }
            BindingIdentity::Directory { root, descendants } => {
                hash_part(&mut hasher, b"directory");
                hash_part(&mut hasher, path_bytes(root));
                hash_part(&mut hasher, &[u8::from(*descendants)]);
            }
        }
        hash_part(&mut hasher, binding.project.as_bytes());
        let context = serde_json::to_vec(&binding.context)
            .map_err(|_| Error::ConfigInvalid("context serialization"))?;
        hash_part(&mut hasher, &context);
        entries.push(hasher.finalize().to_vec());
    }
    entries.sort();
    let mut hasher = Sha256::new();
    hasher.update(b"memory-hooks-config-v1\0");
    hash_part(&mut hasher, path_bytes(config_path));
    hash_part(&mut hasher, path_bytes(state));
    hash_part(&mut hasher, path_bytes(git));
    for entry in entries {
        hash_part(&mut hasher, &entry);
    }
    Ok(format!("v1:{:x}", hasher.finalize()))
}

impl TrustedHooksConfig {
    /// Load and verify one absolute, symlink-free operator config path.
    ///
    /// # Errors
    /// Returns a redacted reason when trust or schema validation fails.
    #[expect(
        clippy::too_many_lines,
        reason = "one ordered validation of the complete trust boundary"
    )]
    pub fn load(path: &Path) -> Result<Self> {
        #[cfg(not(target_os = "linux"))]
        return Err(Error::UnsupportedPlatform);
        let file = open_trusted_config(path)?;
        let size = file
            .metadata()
            .map_err(|error| io("config metadata", &error))?
            .len();
        if size > limits::CONFIG_BYTES as u64 {
            return Err(Error::ConfigTooLarge);
        }
        let capacity = usize::try_from(size).map_err(|_| Error::ConfigTooLarge)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take((limits::CONFIG_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| io("config read", &error))?;
        if bytes.len() > limits::CONFIG_BYTES {
            return Err(Error::ConfigTooLarge);
        }
        let source = std::str::from_utf8(&bytes).map_err(|_| Error::ConfigInvalid("UTF-8"))?;
        let raw: RawConfig = toml::from_str(source).map_err(|error: toml::de::Error| {
            let offset = error.span().map_or(0, |span| span.start.min(source.len()));
            let prefix = &source[..offset];
            let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
            let column = prefix
                .rsplit_once('\n')
                .map_or(prefix.len(), |(_, tail)| tail.len())
                + 1;
            Error::ConfigSyntax { line, column }
        })?;
        if raw.schema_version != 1
            || raw.bindings.is_empty()
            || raw.bindings.len() > limits::BINDINGS
        {
            return Err(Error::ConfigInvalid("schema_version or bindings"));
        }
        valid_path(&raw.state_root)?;
        valid_path(&raw.git_executable)?;
        reject_symlink_components(&raw.state_root, "state_root symlink")?;
        let state_parent = raw
            .state_root
            .parent()
            .ok_or(Error::ConfigInvalid("state_root"))?;
        let state_leaf = raw
            .state_root
            .file_name()
            .ok_or(Error::ConfigInvalid("state_root"))?;
        let state_root = canonical(state_parent, "state_root parent")?.join(state_leaf);
        let _git_file = open_trusted_path(&raw.git_executable, true)?;
        let git = canonical(&raw.git_executable, "git_executable")?;
        let mut labels = HashSet::new();
        let mut identities = HashSet::new();
        let mut bindings = Vec::with_capacity(raw.bindings.len());
        for item in raw.bindings {
            let (label, identity, project, context) = match item {
                RawBinding::Git {
                    label,
                    common_dir,
                    primary_worktree,
                    bare_backed,
                    guardrails_project,
                    context,
                } => {
                    if bare_backed == primary_worktree.is_some() {
                        return Err(Error::ConfigInvalid("git primary_worktree or bare_backed"));
                    }
                    let common = canonical(&common_dir, "common_dir")?;
                    let primary = primary_worktree
                        .as_deref()
                        .map(|p| canonical(p, "primary_worktree"))
                        .transpose()?;
                    if !common.is_dir() || primary.as_ref().is_some_and(|p| !p.is_dir()) {
                        return Err(Error::ConfigInvalid("git directory"));
                    }
                    (
                        label,
                        BindingIdentity::Git { common, primary },
                        guardrails_project,
                        context,
                    )
                }
                RawBinding::Directory {
                    label,
                    root,
                    allow_subdirectories,
                    guardrails_project,
                    context,
                } => {
                    let root = canonical(&root, "directory root")?;
                    if !root.is_dir() {
                        return Err(Error::ConfigInvalid("directory root"));
                    }
                    if marker_root(&root)
                        .map_err(|_| Error::ConfigInvalid("directory Git boundary"))?
                        .is_some()
                    {
                        return Err(Error::ConfigInvalid("directory inside Git checkout"));
                    }
                    (
                        label,
                        BindingIdentity::Directory {
                            root,
                            descendants: allow_subdirectories,
                        },
                        guardrails_project,
                        context,
                    )
                }
            };
            if label.is_empty() || label.len() > 128 || !labels.insert(label) {
                return Err(Error::ConfigInvalid("binding label"));
            }
            if project.trim().is_empty()
                || project.len() > limits::PROJECT_BYTES
                || project.chars().any(char::is_control)
            {
                return Err(Error::ConfigInvalid("guardrails_project"));
            }
            context
                .validate()
                .map_err(|_| Error::ConfigInvalid("context"))?;
            if context.profile.is_none() {
                return Err(Error::ConfigInvalid("context.profile"));
            }
            let key = match &identity {
                BindingIdentity::Git { common, .. } => (0, common.clone()),
                BindingIdentity::Directory { root, .. } => (1, root.clone()),
            };
            if !identities.insert(key) {
                return Err(Error::ConfigInvalid("duplicate identity"));
            }
            bindings.push(Binding {
                identity,
                project,
                context,
            });
        }
        let helper_path = std::env::current_exe()
            .map_err(|_| Error::ConfigUntrusted("helper path"))?
            .canonicalize()
            .map_err(|_| Error::ConfigUntrusted("helper path"))?;
        let config_path = canonical(path, "config path")?;
        for binding in &bindings {
            let scope = match &binding.identity {
                BindingIdentity::Git { primary, common } => primary.as_ref().unwrap_or(common),
                BindingIdentity::Directory { root, .. } => root,
            };
            if within(&config_path, scope)
                || within(&state_root, scope)
                || within(scope, &state_root)
                || within(&git, scope)
                || within(&helper_path, scope)
            {
                return Err(Error::ConfigUntrusted("installation inside workspace"));
            }
        }
        for (i, left) in bindings.iter().enumerate() {
            if let BindingIdentity::Directory {
                root: a,
                descendants: da,
            } = &left.identity
            {
                for right in bindings.iter().skip(i + 1) {
                    if let BindingIdentity::Directory {
                        root: b,
                        descendants: db,
                    } = &right.identity
                        && ((within(a, b) && *db) || (within(b, a) && *da))
                    {
                        return Err(Error::ConfigInvalid("overlapping directory scopes"));
                    }
                }
            }
        }
        let fingerprint = fingerprint(&config_path, &state_root, &git, &bindings)?;
        Ok(Self {
            git,
            config_path,
            helper_path,
            state_root,
            bindings,
            fingerprint,
        })
    }

    /// Versioned digest of normalized nonsecret trust fields.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}
