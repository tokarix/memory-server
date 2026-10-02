//! Bounded directory coverage for the static output contract.
//!
//! Every existing output descendant has a separate candidate. Ordinary future
//! directories inherit an assessed existing prefix at the check instant; later
//! creation/replacement/mount races remain outside this point-in-time proof.
//! Individual output-file overrides, symlinks and file mounts are unsupported.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::storage::{Candidate, CandidateRole, MAX_CANDIDATES};

use super::cargo::Layout;
use super::command::Invocation;
use super::environment::Environment;
use super::files::{MAX_DEPTH, ReadProvider, ReadSet};
use super::{Action, BuildAction, Failure, paths};

struct Entry {
    role: CandidateRole,
    path: PathBuf,
}

/// Private stable directory set, with existing descendant coverage and no raw
/// Debug/Serialize. It still needs execution/read-set/locality/pack validation
/// and authoritative storage evaluation; it never authorizes execution.
pub struct Inventory {
    entries: Vec<Entry>,
}

impl Inventory {
    /// Expand Cargo's required roots through all existing output directories.
    /// Regular files remain on their parent mount; unsupported file kinds deny.
    ///
    /// # Errors
    /// Unknown directory evidence, aliasing, overflow or deadline expiry deny.
    pub fn cargo<P: ReadProvider>(
        layout: &Layout,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<Self, Failure> {
        let mut inventory = Self {
            entries: Vec::new(),
        };
        for directory in layout.directories() {
            inventory.push(directory.role(), directory.path().to_owned())?;
        }
        inventory.expand(reads)?;
        Ok(inventory)
    }

    /// Resolve a direct rustc --out-dir or rustdoc -o/--output directory form.
    /// The compiler environment must come from the protected execution model.
    /// Rust source contents/identity and output absences enter the same read set.
    ///
    /// # Errors
    /// File outputs, response files, unsupported names/options and missing inputs deny.
    pub fn direct<P: ReadProvider>(
        invocation: &Invocation,
        cwd: &Path,
        compiler: &Environment,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<Self, Failure> {
        let Action::Build(action @ (BuildAction::Rustc | BuildAction::Rustdoc)) =
            invocation.action()
        else {
            return Err(Failure::Unsupported);
        };
        let (source, output) = direct_paths(action, invocation.arguments(), cwd)?;
        reads.read(&source)?.ok_or(Failure::Read)?;
        let mut inventory = Self {
            entries: Vec::new(),
        };
        inventory.push(
            CandidateRole::SourceWorktree,
            source.parent().ok_or(Failure::Cwd)?.to_owned(),
        )?;
        inventory.push(CandidateRole::TargetDirectory, output.clone())?;
        inventory.push(CandidateRole::BuildOutput, output)?;
        inventory.push(
            CandidateRole::CompilerTemporary,
            paths::absolute(cwd, Path::new(compiler.get("TMPDIR").unwrap_or("/tmp")))?,
        )?;
        inventory.expand(reads)?;
        Ok(inventory)
    }

    fn push(&mut self, role: CandidateRole, path: PathBuf) -> Result<(), Failure> {
        paths::validate(&path)?;
        if !path.is_absolute() {
            return Err(Failure::Cwd);
        }
        if self
            .entries
            .iter()
            .any(|item| item.role == role && item.path == path)
        {
            return Ok(());
        }
        if self.entries.len() >= MAX_CANDIDATES {
            return Err(Failure::Limit);
        }
        self.entries.push(Entry { role, path });
        Ok(())
    }

    fn expand<P: ReadProvider>(&mut self, reads: &mut ReadSet<'_, P>) -> Result<(), Failure> {
        let roots: BTreeSet<_> = self
            .entries
            .iter()
            .filter(|item| {
                matches!(
                    item.role,
                    CandidateRole::TargetDirectory | CandidateRole::BuildOutput
                )
            })
            .map(|item| item.path.clone())
            .collect();
        let mut visited = BTreeSet::new();
        for root in roots {
            self.visit(&root, 0, false, &mut visited, reads)?;
        }
        for role in [
            CandidateRole::TargetDirectory,
            CandidateRole::BuildOutput,
            CandidateRole::CompilerTemporary,
            CandidateRole::SourceWorktree,
        ] {
            if !self.entries.iter().any(|item| item.role == role) {
                return Err(Failure::Unsupported);
            }
        }
        reads.checkpoint()
    }

    fn visit<P: ReadProvider>(
        &mut self,
        path: &Path,
        depth: usize,
        required: bool,
        visited: &mut BTreeSet<PathBuf>,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<(), Failure> {
        reads.checkpoint()?;
        if depth > MAX_DEPTH {
            return Err(Failure::Limit);
        }
        if !visited.insert(path.to_owned()) {
            return Ok(());
        }
        if let Some(entries) = reads.output_directory(path)? {
            for entry in entries {
                if entry.is_directory() {
                    let child = path.join(entry.name());
                    self.push(CandidateRole::BuildOutput, child.clone())?;
                    self.visit(&child, depth + 1, true, visited, reads)?;
                }
            }
        } else if required {
            return Err(Failure::Changed);
        }
        Ok(())
    }

    /// Stable indices retain coincident-path role coverage.
    ///
    /// # Errors
    /// Storage v1's bounded directory/index contract remains authoritative.
    pub fn candidates(&self) -> Result<Vec<Candidate>, Failure> {
        self.entries
            .iter()
            .enumerate()
            .map(|(index, item)| {
                Candidate::new(
                    u8::try_from(index).map_err(|_| Failure::Limit)?,
                    item.role,
                    &item.path,
                )
                .map_err(|_| Failure::Limit)
            })
            .collect()
    }

    /// Private coverage digest. Hashes do not encrypt low-entropy path data.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"memory-hooks-rust-directory-inventory-v1\0");
        for item in &self.entries {
            hash.update([match item.role {
                CandidateRole::TargetDirectory => 0,
                CandidateRole::BuildOutput => 1,
                CandidateRole::CompilerTemporary => 2,
                CandidateRole::SourceWorktree => 3,
            }]);
            let bytes = item.path.as_os_str().as_encoded_bytes();
            hash.update(bytes.len().to_be_bytes());
            hash.update(bytes);
        }
        hash.finalize().into()
    }
}

fn set_once(destination: &mut Option<PathBuf>, value: &str, cwd: &Path) -> Result<(), Failure> {
    if destination.is_some() {
        return Err(Failure::Unsupported);
    }
    *destination = Some(paths::absolute(cwd, Path::new(value))?);
    Ok(())
}

fn direct_paths(
    action: BuildAction,
    words: &[String],
    cwd: &Path,
) -> Result<(PathBuf, PathBuf), Failure> {
    let mut source = None;
    let mut output = None;
    let mut position = 0;
    while let Some(word) = words.get(position) {
        if word.starts_with('-') {
            let (flag, inline) = word
                .split_once('=')
                .map_or((word.as_str(), None), |(flag, value)| (flag, Some(value)));
            // getopts short spelling is not the long-option key=value grammar.
            if inline.is_some() && !word.starts_with("--") {
                return Err(Failure::Unsupported);
            }
            if !matches!(flag, "--test" | "--document-private-items") {
                let value = if let Some(value) = inline {
                    value
                } else {
                    position += 1;
                    words.get(position).ok_or(Failure::Syntax)?
                };
                match flag {
                    "--out-dir" if action == BuildAction::Rustc => {
                        set_once(&mut output, value, cwd)?;
                    }
                    "-o" | "--output" if action == BuildAction::Rustdoc => {
                        set_once(&mut output, value, cwd)?;
                    }
                    "--edition" if !matches!(value, "2015" | "2018" | "2021" | "2024") => {
                        return Err(Failure::Unsupported);
                    }
                    "--crate-name"
                        if value.is_empty()
                            || !value.bytes().enumerate().all(|(index, byte)| {
                                byte == b'_'
                                    || byte.is_ascii_alphabetic()
                                    || (index != 0 && byte.is_ascii_digit())
                            }) =>
                    {
                        return Err(Failure::Unsupported);
                    }
                    "--crate-type"
                        if value.split(',').any(|value| {
                            !matches!(
                                value,
                                "lib"
                                    | "rlib"
                                    | "dylib"
                                    | "cdylib"
                                    | "staticlib"
                                    | "bin"
                                    | "proc-macro"
                            )
                        }) =>
                    {
                        return Err(Failure::Unsupported);
                    }
                    _ => {}
                }
            }
        } else {
            set_once(&mut source, word, cwd)?;
        }
        position += 1;
    }
    Ok((
        source.ok_or(Failure::Unsupported)?,
        output.ok_or(Failure::Unsupported)?,
    ))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{Failure, Inventory};
    use crate::config::execution::literal_fixture;
    use crate::execution_input::ExecutionInput;
    use crate::rust_build::cargo::{Layout, Options};
    use crate::rust_build::command::parse;
    use crate::rust_build::config::Configuration;
    use crate::rust_build::files::{ReadSet, fixtures::Scripted};
    use crate::rust_build::manifest::Sources;
    use crate::storage::CandidateRole;

    fn direct(source: &str, provider: &Scripted) -> Result<Inventory, Failure> {
        let (contract, _files) = literal_fixture();
        let input =
            ExecutionInput::from_value(&json!({"command":source})).map_err(|_| Failure::Syntax)?;
        let invocation = parse(&input, &contract)?;
        let mut reads = ReadSet::new(provider, Instant::now() + Duration::from_secs(2));
        let inventory = Inventory::direct(
            &invocation,
            Path::new("/work"),
            invocation.environment(),
            &mut reads,
        )?;
        reads.recheck()?;
        Ok(inventory)
    }

    fn fixture() -> Scripted {
        let provider = Scripted::default();
        provider.put(
            "/work/source.rs",
            "// SECRET_SENTINEL: never launched\nfn main() {}\n",
        );
        provider
    }

    #[test]
    fn direct_compilers_test_emit_and_space_paths_have_all_required_roles() {
        let provider = fixture();
        provider.put("/work/space source.rs", "fn main() {}\n");
        for source in [
            "rustc source.rs --out-dir '/output dir'",
            "rustc --test source.rs --out-dir '/output dir'",
            "rustc 'space source.rs' --out-dir '/output dir' --emit=metadata,dep-info",
            "rustdoc source.rs -o '/output dir'",
            "rustdoc source.rs --output='/output dir' --crate-name literal --document-private-items",
        ] {
            let inventory = direct(source, &provider).unwrap();
            let candidates = inventory.candidates().unwrap();
            assert_eq!(candidates.len(), 4);
            for role in [
                CandidateRole::TargetDirectory,
                CandidateRole::BuildOutput,
                CandidateRole::SourceWorktree,
                CandidateRole::CompilerTemporary,
            ] {
                assert!(candidates.iter().any(|candidate| candidate.role() == role));
            }
            assert!(
                inventory
                    .entries
                    .iter()
                    .any(|entry| entry.path == Path::new("/output dir"))
            );
            assert!(
                candidates
                    .iter()
                    .enumerate()
                    .all(|(index, candidate)| index == usize::from(candidate.index()))
            );
        }
    }

    #[test]
    fn existing_unknown_hash_and_documentation_descendants_have_own_candidates() {
        let provider = fixture();
        provider.list(
            "/output",
            &[
                ("crate", true),
                ("static.files", true),
                ("index.html", false),
            ],
        );
        provider.list("/output/crate", &[("nested", true)]);
        provider.list("/output/crate/nested", &[("trait.literal.html", false)]);
        provider.list("/output/static.files", &[]);
        let inventory = direct("rustdoc source.rs -o /output", &provider).unwrap();
        for path in [
            "/output/crate",
            "/output/crate/nested",
            "/output/static.files",
        ] {
            assert!(
                inventory
                    .entries
                    .iter()
                    .any(|entry| entry.path == Path::new(path)
                        && entry.role == CandidateRole::BuildOutput)
            );
        }
        assert_eq!(inventory.candidates().unwrap().len(), 7);
        assert_eq!(
            inventory.digest(),
            direct("rustdoc source.rs -o /output", &provider)
                .unwrap()
                .digest()
        );
        assert!(
            provider
                .calls
                .borrow()
                .iter()
                .all(|path| path != Path::new("/output/index.html"))
        );
    }

    #[test]
    fn cargo_dynamic_build_incremental_doc_cache_and_install_roots_expand() {
        let provider = fixture();
        provider.put(
            "/work/Cargo.toml",
            "[package]\nname='literal'\nversion='1.0.0'",
        );
        provider.list("/work/target", &[("release", true)]);
        provider.list(
            "/work/target/release",
            &[("build", true), ("incremental", true)],
        );
        provider.list(
            "/work/target/release/build",
            &[("literal-unknown-hash", true)],
        );
        provider.list(
            "/work/target/release/build/literal-unknown-hash",
            &[("out", true)],
        );
        provider.list("/work/target/release/build/literal-unknown-hash/out", &[]);
        provider.list(
            "/work/target/release/incremental",
            &[("literal-session", true)],
        );
        provider.list("/work/target/release/incremental/literal-session", &[]);
        provider.list("/sealed/cargo-home/bin", &[("literal", false)]);
        let (contract, _files) = literal_fixture();
        let invocation = parse(
            &ExecutionInput::from_value(&json!({"command":"cargo install --path ."})).unwrap(),
            &contract,
        )
        .unwrap();
        let cwd = Path::new("/work");
        let options = Options::extract(&invocation, cwd).unwrap();
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(2));
        let configuration = Configuration::load(
            cwd,
            Some(options.discovery_root(cwd)),
            invocation.environment(),
            options.configuration(),
            &mut reads,
        )
        .unwrap();
        let sources = Sources::resolve(cwd, options.selection(), &mut reads).unwrap();
        let layout = Layout::resolve(
            &options,
            &configuration,
            &sources,
            &contract,
            (invocation.environment(), invocation.environment()),
            cwd,
        )
        .unwrap();
        let inventory = Inventory::cargo(&layout, &mut reads).unwrap();
        assert!(
            inventory.entries.iter().any(|entry| entry.path
                == Path::new("/work/target/release/build/literal-unknown-hash/out"))
        );
        assert!(inventory.entries.iter().any(
            |entry| entry.path == Path::new("/work/target/release/incremental/literal-session")
        ));
        reads.recheck().unwrap();
    }

    #[test]
    fn new_outputs_and_source_replacements_invalidate() {
        let provider = fixture();
        let (contract, _files) = literal_fixture();
        let invocation = parse(
            &ExecutionInput::from_value(&json!({"command":"rustc source.rs --out-dir /output"}))
                .unwrap(),
            &contract,
        )
        .unwrap();
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(2));
        Inventory::direct(
            &invocation,
            Path::new("/work"),
            invocation.environment(),
            &mut reads,
        )
        .unwrap();
        provider.list("/output", &[("new-mounted-subtree", true)]);
        provider.list("/output/new-mounted-subtree", &[]);
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(2));
        Inventory::direct(
            &invocation,
            Path::new("/work"),
            invocation.environment(),
            &mut reads,
        )
        .unwrap();
        provider.put(
            "/work/source.rs",
            "// changed secret sentinel\nfn main() {}\n",
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
    }

    #[test]
    fn candidate_depth_read_and_deadline_limits_cannot_be_truncated_to_success() {
        let provider = fixture();
        let entries: Vec<_> = (0..64)
            .map(|index| (format!("child{index}"), true))
            .collect();
        provider.list(
            "/output",
            &entries
                .iter()
                .map(|(name, directory)| (name.as_str(), *directory))
                .collect::<Vec<_>>(),
        );
        for (name, _) in &entries {
            provider.list(&format!("/output/{name}"), &[]);
        }
        assert_eq!(
            direct("rustc source.rs --out-dir /output", &provider).err(),
            Some(Failure::Limit)
        );
        let provider = fixture();
        let mut path = "/output".to_owned();
        for _ in 0..18 {
            provider.list(&path, &[("nested", true)]);
            path.push_str("/nested");
        }
        assert_eq!(
            direct("rustc source.rs --out-dir /output", &provider).err(),
            Some(Failure::Limit)
        );
        let (contract, _files) = literal_fixture();
        let invocation = parse(
            &ExecutionInput::from_value(&json!({"command":"rustc source.rs --out-dir /output"}))
                .unwrap(),
            &contract,
        )
        .unwrap();
        let mut reads = ReadSet::new(&provider, Instant::now());
        assert_eq!(
            Inventory::direct(
                &invocation,
                Path::new("/work"),
                invocation.environment(),
                &mut reads
            )
            .err(),
            Some(Failure::Deadline)
        );
    }

    #[test]
    fn unmodeled_output_spellings_versions_types_and_missing_sources_deny() {
        for source in [
            "rustc source.rs",
            "rustc source.rs -o file",
            "rustc source.rs --out-dir /output --out-dir /else",
            "rustc @secret-response --out-dir /output",
            "rustc source.rs --out-dir /output --emit=link=secret-file",
            "rustc source.rs --out-dir /output --edition=2027",
            "rustc source.rs --out-dir /output --crate-type=secret-type",
            "rustdoc source.rs -o=/output",
            "rustdoc source.rs -o /output --crate-name=../secret",
            "rustdoc missing.rs -o /output",
            "rustdoc source.rs",
            "rustc --version",
        ] {
            assert!(direct(source, &fixture()).is_err(), "{source}");
        }
    }
}
