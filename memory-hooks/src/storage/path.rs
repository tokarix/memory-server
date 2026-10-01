//! Descriptor component walking with normal symlink/parent semantics.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use super::{
    Budget, Candidate, CandidateRole, Entry, ObjectIdentity, ProbeError, ProbeResult, Provider,
    Reason,
};

pub(super) const MAX_STEPS: usize = 256;
pub(super) const MAX_LINKS: usize = 40;

enum Step {
    Component(OsString),
    EndLink,
}

struct Edge<H> {
    parent: H,
    name: OsString,
    handle: H,
    identity: ObjectIdentity,
    link: Option<Vec<u8>>,
}

pub(super) struct Resolved<H> {
    pub(super) handle: H,
    pub(super) identity: ObjectIdentity,
    pub(super) requested: PathBuf,
    pub(super) effective: PathBuf,
    pub(super) missing: bool,
    edges: Vec<Edge<H>>,
    absent: Option<(H, OsString)>,
}

impl<H: Clone> Resolved<H> {
    pub(super) fn recheck<P: Provider<Handle = H>>(
        &self,
        provider: &mut P,
        budget: &mut Budget,
    ) -> ProbeResult<()> {
        for edge in &self.edges {
            if provider.identity(&edge.handle, budget)? != edge.identity {
                return Err(ProbeError::new(Reason::Changed));
            }
            let observed = provider.entry(&edge.parent, &edge.name, budget)?;
            let identity = match observed {
                Entry::Directory(_, identity) | Entry::Symlink(_, identity) => identity,
                Entry::Other => return Err(ProbeError::new(Reason::Changed)),
            };
            if identity != edge.identity {
                return Err(ProbeError::new(Reason::Changed));
            }
            if let Some(link) = &edge.link
                && provider.link(&edge.handle, budget)? != *link
            {
                return Err(ProbeError::new(Reason::Changed));
            }
        }
        if let Some((parent, name)) = &self.absent {
            match provider.entry(parent, name, budget) {
                Err(error) if error.code == Some(libc::ENOENT) => {}
                _ => return Err(ProbeError::new(Reason::Changed)),
            }
        }
        provider.accessible(&self.handle, budget)?;
        Ok(())
    }
}

fn components(bytes: &[u8]) -> VecDeque<Step> {
    bytes
        .split(|byte| *byte == b'/')
        .filter(|part| !part.is_empty())
        .map(|part| Step::Component(OsString::from_vec(part.to_vec())))
        .collect()
}

/// Lexical normalization is only used for requested-location containment;
/// filesystem resolution below never cancels `link/..` lexically.
pub(super) fn normalized(path: &Path) -> PathBuf {
    let mut result = PathBuf::from("/");
    for part in path.as_os_str().as_bytes().split(|byte| *byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                result.pop();
            }
            value => result.push(OsString::from_vec(value.to_vec())),
        }
    }
    result
}

#[expect(
    clippy::too_many_lines,
    reason = "single bounded ordered filesystem walk"
)]
pub(super) fn resolve<P: Provider>(
    provider: &mut P,
    budget: &mut Budget,
    cwd: &Path,
    candidate: &Candidate,
) -> ProbeResult<Resolved<P::Handle>> {
    let requested = if candidate.path.is_absolute() {
        candidate.path.clone()
    } else {
        cwd.join(&candidate.path)
    };
    if requested.as_os_str().as_bytes().len() > crate::limits::PATH_BYTES {
        return Err(ProbeError::new(Reason::Limit));
    }
    let root = provider.root(budget)?;
    provider.accessible(&root, budget)?;
    let mut stack = vec![root];
    let mut resolved = PathBuf::from("/");
    let mut pending = components(requested.as_os_str().as_bytes());
    if pending.len() > MAX_STEPS {
        return Err(ProbeError::new(Reason::Limit));
    }
    let mut edges = Vec::new();
    let mut steps = 0;
    let mut links = 0;
    let mut active_links = 0;
    let mut absent = None;
    while let Some(step) = pending.pop_front() {
        let Step::Component(name) = step else {
            active_links -= 1;
            continue;
        };
        steps += 1;
        if steps > MAX_STEPS {
            return Err(ProbeError::new(Reason::Limit));
        }
        match name.as_bytes() {
            b"." => continue,
            b".." => {
                if absent.is_some() {
                    return Err(ProbeError::new(Reason::MissingParent));
                }
                if stack.len() > 1 {
                    stack.pop();
                    resolved.pop();
                }
                continue;
            }
            _ => {}
        }
        if absent.is_some() {
            resolved.push(name);
            continue;
        }
        let parent = stack
            .last()
            .ok_or(ProbeError::new(Reason::Changed))?
            .clone();
        provider.accessible(&parent, budget)?;
        match provider.entry(&parent, &name, budget) {
            Ok(Entry::Directory(handle, identity)) => {
                provider.accessible(&handle, budget)?;
                edges.push(Edge {
                    parent,
                    name: name.clone(),
                    handle: handle.clone(),
                    identity,
                    link: None,
                });
                stack.push(handle);
                resolved.push(name);
            }
            Ok(Entry::Symlink(handle, identity)) => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(ProbeError::new(Reason::Limit));
                }
                let target = provider.link(&handle, budget)?;
                if target.is_empty()
                    || target.len() > crate::limits::PATH_BYTES
                    || target.contains(&0)
                {
                    return Err(ProbeError::new(Reason::Limit));
                }
                let mut next = components(&target);
                if next.len() + pending.len() > MAX_STEPS {
                    return Err(ProbeError::new(Reason::Limit));
                }
                next.push_back(Step::EndLink);
                next.append(&mut pending);
                pending = next;
                active_links += 1;
                if target.first() == Some(&b'/') {
                    stack.truncate(1);
                    resolved = PathBuf::from("/");
                }
                edges.push(Edge {
                    parent,
                    name,
                    handle,
                    identity,
                    link: Some(target),
                });
            }
            Ok(Entry::Other) => return Err(ProbeError::new(Reason::NotDirectory)),
            Err(error) if error.code == Some(libc::ENOENT) => {
                if active_links > 0 {
                    return Err(ProbeError::new(Reason::DanglingSymlink));
                }
                if candidate.role == CandidateRole::SourceWorktree {
                    return Err(ProbeError::new(Reason::MissingSource));
                }
                absent = Some((parent, name.clone()));
                resolved.push(name);
            }
            Err(error) => return Err(error),
        }
        if resolved.as_os_str().as_bytes().len() > crate::limits::PATH_BYTES {
            return Err(ProbeError::new(Reason::Limit));
        }
    }
    let handle = stack
        .last()
        .ok_or(ProbeError::new(Reason::Changed))?
        .clone();
    let identity = provider.identity(&handle, budget)?;
    Ok(Resolved {
        handle,
        identity,
        requested,
        effective: resolved,
        missing: absent.is_some(),
        edges,
        absent,
    })
}
