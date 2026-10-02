//! Bounded manifest, workspace and local dependency selection without Cargo.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use toml::Value;

use super::files::{MAX_DEPTH, ReadProvider, ReadSet};
use super::{Failure, paths};

const MAX_SOURCES: usize = 64;

/// Literal package selection extracted from an already validated invocation.
/// These values describe execution; they never select a policy or binding.
#[derive(Default)]
pub struct Selection {
    /// Explicit manifest, relative to execution cwd, if present.
    pub manifest: Option<PathBuf>,
    /// Select every member (--workspace or --all).
    pub workspace: bool,
    /// Exact package names. Dynamic/package-ID specifications are unsupported.
    pub packages: Vec<String>,
    /// Exact names excluded from an explicit workspace selection.
    pub exclude: Vec<String>,
}

/// Private package metadata used by later output-layout resolution.
#[derive(Clone)]
pub struct Package {
    directory: PathBuf,
    name: String,
    manifest: Value,
}

impl Package {
    /// Absolute source directory, with actual path components preserved.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Private package name; never persist it as diagnostic/audit text.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Bounded private manifest for the subsequent profile/output stage.
    #[must_use]
    pub const fn manifest(&self) -> &Value {
        &self.manifest
    }
}

#[derive(Clone)]
struct Document {
    directory: PathBuf,
    value: Value,
}

fn bounded(value: &Value, depth: usize) -> Result<(), Failure> {
    if depth > MAX_DEPTH {
        return Err(Failure::Limit);
    }
    match value {
        Value::Table(table) => {
            for (key, value) in table {
                paths::validate(Path::new(key))?;
                bounded(value, depth + 1)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                bounded(value, depth + 1)?;
            }
        }
        Value::String(value) if !value.is_empty() => paths::validate(Path::new(value))?,
        _ => {}
    }
    Ok(())
}

impl Document {
    fn read<P: ReadProvider>(
        directory: &Path,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<Option<Self>, Failure> {
        let Some(bytes) = reads.read(&directory.join("Cargo.toml"))? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(bytes).map_err(|_| Failure::Manifest)?;
        let value: Value = toml::from_str(text).map_err(|_| Failure::Manifest)?;
        bounded(&value, 0)?;
        let table = value.as_table().ok_or(Failure::Manifest)?;
        for key in table.keys() {
            if !matches!(
                key.as_str(),
                "package"
                    | "workspace"
                    | "dependencies"
                    | "dev-dependencies"
                    | "build-dependencies"
                    | "target"
                    | "profile"
                    | "lib"
                    | "bin"
                    | "example"
                    | "test"
                    | "bench"
                    | "features"
                    | "lints"
                    | "badges"
            ) {
                // Includes Cargo features, patch/replace and future namespaces.
                return Err(Failure::Manifest);
            }
        }
        for key in ["package", "workspace"] {
            if value.get(key).is_some_and(|value| !value.is_table()) {
                return Err(Failure::Manifest);
            }
        }
        if let Some(package) = value.get("package") {
            for key in package.as_table().ok_or(Failure::Manifest)?.keys() {
                if !matches!(
                    key.as_str(),
                    "name"
                        | "version"
                        | "authors"
                        | "edition"
                        | "rust-version"
                        | "description"
                        | "documentation"
                        | "readme"
                        | "homepage"
                        | "repository"
                        | "license"
                        | "license-file"
                        | "keywords"
                        | "categories"
                        | "workspace"
                        | "build"
                        | "links"
                        | "exclude"
                        | "include"
                        | "publish"
                        | "metadata"
                        | "default-run"
                        | "autobins"
                        | "autoexamples"
                        | "autotests"
                        | "autobenches"
                        | "autolib"
                        | "resolver"
                ) {
                    return Err(Failure::Manifest);
                }
            }
        }
        if let Some(workspace) = value.get("workspace") {
            for key in workspace.as_table().ok_or(Failure::Manifest)?.keys() {
                if !matches!(
                    key.as_str(),
                    "members"
                        | "default-members"
                        | "exclude"
                        | "resolver"
                        | "package"
                        | "dependencies"
                        | "lints"
                        | "metadata"
                ) {
                    return Err(Failure::Manifest);
                }
            }
        }
        if table.get("package").is_none() && table.get("workspace").is_none() {
            return Err(Failure::Manifest);
        }
        Ok(Some(Self {
            directory: directory.to_owned(),
            value,
        }))
    }

    fn package(&self) -> Result<Option<Package>, Failure> {
        let Some(package) = self.value.get("package") else {
            return Ok(None);
        };
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .ok_or(Failure::Manifest)?;
        if !package_name(name) {
            return Err(Failure::Manifest);
        }
        Ok(Some(Package {
            directory: self.directory.clone(),
            name: name.to_owned(),
            manifest: self.value.clone(),
        }))
    }

    fn workspace_path(&self) -> Result<Option<PathBuf>, Failure> {
        self.value
            .get("package")
            .and_then(|value| value.get("workspace"))
            .map(|value| {
                let value = value.as_str().ok_or(Failure::Manifest)?;
                paths::absolute(&self.directory, Path::new(value))
            })
            .transpose()
    }
}

fn package_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn required<P: ReadProvider>(
    directory: &Path,
    reads: &mut ReadSet<'_, P>,
) -> Result<Document, Failure> {
    Document::read(directory, reads)?.ok_or(Failure::Manifest)
}

fn discover<P: ReadProvider>(
    cwd: &Path,
    manifest: Option<&Path>,
    reads: &mut ReadSet<'_, P>,
) -> Result<Document, Failure> {
    if let Some(manifest) = manifest {
        let path = paths::absolute(cwd, manifest)?;
        if path.file_name().is_none_or(|name| name != "Cargo.toml") {
            return Err(Failure::Manifest);
        }
        return required(path.parent().ok_or(Failure::Manifest)?, reads);
    }
    for (depth, directory) in cwd.ancestors().enumerate() {
        if depth > MAX_DEPTH {
            return Err(Failure::Limit);
        }
        if let Some(document) = Document::read(directory, reads)? {
            return Ok(document);
        }
    }
    Err(Failure::Manifest)
}

fn workspace<P: ReadProvider>(
    document: &Document,
    reads: &mut ReadSet<'_, P>,
) -> Result<Document, Failure> {
    if document.value.get("workspace").is_some() {
        if document.workspace_path()?.is_some() {
            return Err(Failure::Manifest);
        }
        return Ok(document.clone());
    }
    if let Some(path) = document.workspace_path()? {
        let root = required(&path, reads)?;
        if root.value.get("workspace").is_none() {
            return Err(Failure::Manifest);
        }
        return Ok(root);
    }
    for (depth, directory) in document.directory.ancestors().skip(1).enumerate() {
        if depth >= MAX_DEPTH {
            return Err(Failure::Limit);
        }
        if let Some(root) = Document::read(directory, reads)?
            && root.value.get("workspace").is_some()
        {
            return Ok(root);
        }
    }
    Ok(document.clone())
}

fn strings(value: Option<&Value>) -> Result<Vec<&str>, Failure> {
    value.map_or_else(
        || Ok(Vec::new()),
        |value| {
            let array = value.as_array().ok_or(Failure::Manifest)?;
            if array.len() > MAX_SOURCES {
                return Err(Failure::Limit);
            }
            array
                .iter()
                .map(|value| value.as_str().ok_or(Failure::Manifest))
                .collect()
        },
    )
}

fn expand<P: ReadProvider>(
    base: &Path,
    pattern: &str,
    reads: &mut ReadSet<'_, P>,
) -> Result<Vec<PathBuf>, Failure> {
    paths::validate(Path::new(pattern))?;
    if pattern.contains(['?', '[', ']', '{', '}', '\\']) {
        return Err(Failure::Manifest);
    }
    if !pattern.contains('*') {
        return Ok(vec![paths::absolute(base, Path::new(pattern))?]);
    }
    // Exactly one whole-component '*' is the supported deterministic glob.
    let parts: Vec<_> = pattern.split('/').collect();
    let index = parts
        .iter()
        .position(|part| *part == "*")
        .ok_or(Failure::Manifest)?;
    if parts.iter().filter(|part| part.contains('*')).count() != 1
        || Path::new(pattern).is_absolute()
        || parts.iter().any(|part| matches!(*part, "" | "." | ".."))
    {
        return Err(Failure::Manifest);
    }
    let prefix = parts[..index]
        .iter()
        .fold(base.to_owned(), |path, part| path.join(part));
    let mut paths = Vec::new();
    for entry in reads.directory(&prefix)? {
        if !entry.is_directory() {
            // Cargo's glob may select this; do not silently discard a symlink.
            return Err(Failure::Manifest);
        }
        let path = parts[index + 1..]
            .iter()
            .fold(prefix.join(entry.name()), |path, part| path.join(part));
        paths::validate(&path)?;
        paths.push(path);
        if paths.len() > MAX_SOURCES {
            return Err(Failure::Limit);
        }
    }
    if paths.is_empty() {
        return Err(Failure::Manifest);
    }
    Ok(paths)
}

fn workspace_paths<P: ReadProvider>(
    root: &Document,
    key: &str,
    reads: &mut ReadSet<'_, P>,
) -> Result<BTreeSet<PathBuf>, Failure> {
    let patterns = strings(root.value.get("workspace").and_then(|value| value.get(key)))?;
    let mut paths = BTreeSet::new();
    for pattern in patterns {
        paths.extend(expand(&root.directory, pattern, reads)?);
        if paths.len() > MAX_SOURCES {
            return Err(Failure::Limit);
        }
    }
    Ok(paths)
}

fn dependency_path(value: &Value, base: &Path) -> Result<Option<PathBuf>, Failure> {
    let table = value.as_table().ok_or(Failure::Manifest)?;
    if table.contains_key("git")
        || table.contains_key("registry")
        || table.contains_key("artifact")
        || table.contains_key("lib")
        || table.contains_key("target")
        || table.contains_key("workspace")
    {
        return Err(Failure::Manifest);
    }
    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "path" | "version" | "package" | "optional" | "features" | "default-features"
        ) {
            return Err(Failure::Manifest);
        }
    }
    table
        .get("path")
        .map(|value| paths::absolute(base, Path::new(value.as_str().ok_or(Failure::Manifest)?)))
        .transpose()
}

fn dependencies(document: &Document, root: &Document) -> Result<Vec<PathBuf>, Failure> {
    let mut paths = Vec::new();
    let mut namespaces = vec![&document.value];
    if let Some(targets) = document.value.get("target") {
        for target in targets.as_table().ok_or(Failure::Manifest)?.values() {
            for key in target.as_table().ok_or(Failure::Manifest)?.keys() {
                if !matches!(
                    key.as_str(),
                    "dependencies" | "dev-dependencies" | "build-dependencies"
                ) {
                    return Err(Failure::Manifest);
                }
            }
            namespaces.push(target);
        }
    }
    for namespace in namespaces {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(values) = namespace.get(key) else {
                continue;
            };
            for (name, value) in values.as_table().ok_or(Failure::Manifest)? {
                let (value, base) = if value.get("workspace").is_some() {
                    let local = value.as_table().ok_or(Failure::Manifest)?;
                    if local.get("workspace").and_then(Value::as_bool) != Some(true)
                        || local.keys().any(|key| {
                            !matches!(
                                key.as_str(),
                                "workspace" | "features" | "optional" | "default-features"
                            )
                        })
                    {
                        return Err(Failure::Manifest);
                    }
                    let inherited = root
                        .value
                        .get("workspace")
                        .and_then(|value| value.get("dependencies"))
                        .and_then(|value| value.get(name))
                        .ok_or(Failure::Manifest)?;
                    (inherited, &root.directory)
                } else {
                    (value, &document.directory)
                };
                // Future/registry/git source locations are unresolved in this
                // local-only stage. Optional and target dependencies are a union.
                paths.push(dependency_path(value, base)?.ok_or(Failure::Manifest)?);
                if paths.len() > MAX_SOURCES {
                    return Err(Failure::Limit);
                }
            }
        }
    }
    Ok(paths)
}

fn members<P: ReadProvider>(
    root: &Document,
    reads: &mut ReadSet<'_, P>,
) -> Result<BTreeMap<PathBuf, Document>, Failure> {
    let excluded = workspace_paths(root, "exclude", reads)?;
    let mut pending = workspace_paths(root, "members", reads)?;
    if root.package()?.is_some() {
        pending.insert(root.directory.clone());
    }
    let mut members = BTreeMap::new();
    for _ in 0..MAX_SOURCES {
        let Some(path) = pending.pop_first() else {
            break;
        };
        if excluded.contains(&path) || members.contains_key(&path) {
            continue;
        }
        let document = required(&path, reads)?;
        if document.package()?.is_none()
            || (path != root.directory && document.value.get("workspace").is_some())
            || document
                .workspace_path()?
                .is_some_and(|workspace| workspace != root.directory)
        {
            return Err(Failure::Manifest);
        }
        for dependency in dependencies(&document, root)? {
            if dependency.starts_with(&root.directory) && !excluded.contains(&dependency) {
                pending.insert(dependency);
            }
        }
        members.insert(path, document);
    }
    if !pending.is_empty() || members.len() > MAX_SOURCES {
        return Err(Failure::Limit);
    }
    if members.is_empty() {
        return Err(Failure::Manifest);
    }
    let mut names = BTreeSet::new();
    for document in members.values() {
        let package = document.package()?.ok_or(Failure::Manifest)?;
        if !names.insert(package.name) {
            return Err(Failure::Manifest);
        }
    }
    Ok(members)
}

#[derive(Default)]
struct Collector {
    directories: BTreeSet<PathBuf>,
    visited: BTreeSet<PathBuf>,
    active: BTreeSet<PathBuf>,
}

impl Collector {
    fn visit<P: ReadProvider>(
        &mut self,
        directory: &Path,
        depth: usize,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<(), Failure> {
        reads.checkpoint()?;
        if depth > MAX_DEPTH {
            return Err(Failure::Limit);
        }
        if self.active.contains(directory) {
            return Err(Failure::Manifest);
        }
        if !self.visited.insert(directory.to_owned()) {
            return Ok(());
        }
        if self.visited.len() > MAX_SOURCES {
            return Err(Failure::Limit);
        }
        self.active.insert(directory.to_owned());
        self.directories.insert(directory.to_owned());
        let document = required(directory, reads)?;
        let root = workspace(&document, reads)?;
        if document.package()?.is_none() {
            return Err(Failure::Manifest);
        }
        self.directories.insert(root.directory.clone());
        for path in dependencies(&document, &root)? {
            self.visit(&path, depth + 1, reads)?;
        }
        if self.directories.len() > MAX_SOURCES {
            return Err(Failure::Limit);
        }
        self.active.remove(directory);
        Ok(())
    }
}

/// Resolved local source coverage. This does not claim complete build outputs.
pub struct Sources {
    root: Document,
    selected: Vec<Package>,
    directories: Vec<PathBuf>,
}

impl Sources {
    /// Resolve selected manifests, workspace members and the union of local
    /// normal/build/dev/target/optional dependency trees without launching Cargo.
    /// `cwd` must already have the same checked binding as the callback.
    ///
    /// # Errors
    /// Unknown selectors, missing manifests, opaque source overrides, remote
    /// dependencies, unsupported globs, cycles and bounded read failures deny.
    pub fn resolve<P: ReadProvider>(
        cwd: &Path,
        selection: &Selection,
        reads: &mut ReadSet<'_, P>,
    ) -> Result<Self, Failure> {
        paths::validate(cwd)?;
        if !cwd.is_absolute() {
            return Err(Failure::Cwd);
        }
        if selection.packages.len() > MAX_SOURCES || selection.exclude.len() > MAX_SOURCES {
            return Err(Failure::Limit);
        }
        if selection
            .packages
            .iter()
            .chain(&selection.exclude)
            .any(|name| !package_name(name))
            || (selection.workspace && !selection.packages.is_empty())
            || (!selection.workspace && !selection.exclude.is_empty())
        {
            return Err(Failure::Manifest);
        }
        let document = discover(cwd, selection.manifest.as_deref(), reads)?;
        let root = workspace(&document, reads)?;
        let members = members(&root, reads)?;
        if !members.contains_key(&document.directory) && document.value.get("package").is_some() {
            return Err(Failure::Manifest);
        }
        let defaults = workspace_paths(&root, "default-members", reads)?;
        if defaults.iter().any(|path| !members.contains_key(path)) {
            return Err(Failure::Manifest);
        }
        let mut selected = Vec::new();
        for member in members.values() {
            let package = member.package()?.ok_or(Failure::Manifest)?;
            let include = if selection.workspace {
                !selection.exclude.contains(&package.name)
            } else if !selection.packages.is_empty() {
                selection.packages.contains(&package.name)
            } else if document.directory == root.directory {
                if !defaults.is_empty() {
                    defaults.contains(&package.directory)
                } else if root.package()?.is_some() {
                    package.directory == root.directory
                } else {
                    true
                }
            } else {
                package.directory == document.directory
            };
            if include {
                selected.push(package);
            }
        }
        for name in selection.packages.iter().chain(&selection.exclude) {
            if !members.values().any(|member| {
                member
                    .package()
                    .ok()
                    .flatten()
                    .is_some_and(|package| &package.name == name)
            }) {
                return Err(Failure::Manifest);
            }
        }
        if selected.is_empty() {
            return Err(Failure::Manifest);
        }
        let mut collector = Collector::default();
        collector.directories.insert(root.directory.clone());
        for package in &selected {
            collector.visit(&package.directory, 0, reads)?;
        }
        Ok(Self {
            root,
            selected,
            directories: collector.directories.into_iter().collect(),
        })
    }

    /// Workspace directory that supplies the default target path and profiles.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.root.directory
    }

    /// Private bounded workspace manifest for output/profile resolution.
    #[must_use]
    pub const fn workspace_manifest(&self) -> &Value {
        &self.root.value
    }

    /// Selected packages in stable directory order.
    #[must_use]
    pub fn selected(&self) -> &[Package] {
        &self.selected
    }

    /// Stable source-directory inventory, including local dependency workspaces.
    #[must_use]
    pub fn directories(&self) -> &[PathBuf] {
        &self.directories
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{Failure, Selection, Sources};
    use crate::rust_build::files::ReadSet;
    use crate::rust_build::files::fixtures::Scripted;

    fn resolve(provider: &Scripted, cwd: &str, selection: &Selection) -> Result<Sources, Failure> {
        let mut reads = ReadSet::new(provider, Instant::now() + Duration::from_secs(5));
        Sources::resolve(Path::new(cwd), selection, &mut reads)
    }

    fn virtual_workspace() -> Scripted {
        let provider = Scripted::default();
        provider.put("/work/Cargo.toml", "[workspace]\nmembers=['crates/*']\ndefault-members=['crates/a']\nexclude=['crates/c']\nresolver='3'\n");
        provider.list("/work/crates", &[("a", true), ("b", true), ("c", true)]);
        for name in ["a", "b", "c"] {
            provider.put(
                &format!("/work/crates/{name}/Cargo.toml"),
                &format!("[package]\nname='{name}'\nversion='0.1.0'\n"),
            );
        }
        provider
    }

    #[test]
    fn virtual_defaults_explicit_packages_workspace_and_excludes_are_distinct() {
        let provider = virtual_workspace();
        let sources = resolve(&provider, "/work", &Selection::default()).unwrap();
        assert_eq!(sources.workspace(), Path::new("/work"));
        assert_eq!(
            sources
                .selected()
                .iter()
                .map(super::Package::name)
                .collect::<Vec<_>>(),
            ["a"]
        );
        let selection = Selection {
            workspace: true,
            exclude: vec!["a".to_owned()],
            ..Selection::default()
        };
        assert_eq!(
            resolve(&provider, "/work", &selection).unwrap().selected()[0].name(),
            "b"
        );
        let selection = Selection {
            packages: vec!["b".to_owned()],
            ..Selection::default()
        };
        assert_eq!(
            resolve(&provider, "/work/crates/a/src", &selection)
                .unwrap()
                .selected()[0]
                .name(),
            "b"
        );
        assert_eq!(
            resolve(&provider, "/work/crates/b/src", &Selection::default())
                .unwrap()
                .selected()[0]
                .name(),
            "b"
        );
        let selection = Selection {
            packages: vec!["c".to_owned()],
            ..Selection::default()
        };
        assert!(matches!(
            resolve(&provider, "/work", &selection),
            Err(Failure::Manifest)
        ));
    }

    #[test]
    fn manifest_selection_does_not_relocate_execution_or_configuration_discovery() {
        let provider = virtual_workspace();
        let selection = Selection {
            manifest: Some(PathBuf::from("../work/crates/b/Cargo.toml")),
            ..Selection::default()
        };
        // The literal parent component is preserved, rather than pretending
        // to know the provider's physical path identity from lexical cleanup.
        provider.put(
            "/other/../work/crates/b/Cargo.toml",
            "[package]\nname='standalone'\nversion='0.1.0'",
        );
        let sources = resolve(&provider, "/other", &selection).unwrap();
        assert_eq!(sources.workspace(), Path::new("/other/../work/crates/b"));
        assert_eq!(
            sources.selected()[0].directory(),
            Path::new("/other/../work/crates/b")
        );
        assert!(
            !provider
                .calls
                .borrow()
                .iter()
                .any(|path| path.ends_with(".cargo/config"))
        );
    }

    #[test]
    fn root_packages_implicit_members_and_local_dependency_sources_are_covered() {
        let provider = Scripted::default();
        provider.put("/work/Cargo.toml", "[workspace]\n[workspace.dependencies]\ninherited={path='shared'}\n[package]\nname='root'\nversion='0.1.0'\n[dependencies]\na={path='a'}\ninherited.workspace=true\n");
        provider.put("/work/a/Cargo.toml", "[package]\nname='a'\nversion='0.1.0'\n[build-dependencies]\nexternal={path='/external'}\n[target.'cfg(unix)'.dev-dependencies]\noptional={path='/optional',optional=true}\n");
        provider.put(
            "/work/shared/Cargo.toml",
            "[package]\nname='shared'\nversion='0.1.0'",
        );
        provider.put(
            "/external/Cargo.toml",
            "[package]\nname='external'\nversion='0.1.0'",
        );
        provider.put(
            "/optional/Cargo.toml",
            "[package]\nname='optional'\nversion='0.1.0'",
        );
        let sources = resolve(&provider, "/work", &Selection::default()).unwrap();
        assert_eq!(sources.selected().len(), 1);
        assert_eq!(sources.selected()[0].name(), "root");
        assert_eq!(
            sources.directories(),
            ["/external", "/optional", "/work", "/work/a", "/work/shared"].map(PathBuf::from)
        );
        let selection = Selection {
            workspace: true,
            ..Selection::default()
        };
        assert_eq!(
            resolve(&provider, "/work", &selection)
                .unwrap()
                .selected()
                .len(),
            3
        );
    }

    #[test]
    fn explicit_external_workspace_and_nested_workspaces_use_their_own_roots() {
        let provider = Scripted::default();
        provider.put(
            "/external/member/Cargo.toml",
            "[package]\nname='member'\nversion='0.1.0'\nworkspace='/work'",
        );
        provider.put(
            "/work/Cargo.toml",
            "[workspace]\nmembers=['/external/member']",
        );
        assert_eq!(
            resolve(&provider, "/external/member", &Selection::default())
                .unwrap()
                .workspace(),
            Path::new("/work")
        );
        provider.put(
            "/work/nested/Cargo.toml",
            "[workspace]\n[package]\nname='nested'\nversion='0.1.0'",
        );
        assert_eq!(
            resolve(&provider, "/work/nested", &Selection::default())
                .unwrap()
                .workspace(),
            Path::new("/work/nested")
        );
        provider.put("/work/Cargo.toml", "[workspace]\nmembers=['nested']");
        assert!(matches!(
            resolve(&provider, "/work", &Selection::default()),
            Err(Failure::Manifest)
        ));
    }

    #[test]
    fn replacements_remote_sources_unavailable_trees_and_opaque_globs_deny() {
        let provider = Scripted::default();
        for manifest in [
            "[package]\nname='root'\n[dependencies]\nremote='1.0'",
            "[package]\nname='root'\n[dependencies]\nremote={git='https://secret.invalid'}",
            "[package]\nname='root'\n[dependencies]\nmissing={path='/future'}",
            "[package]\nname='root'\n[patch.crates-io]\nsecret={path='/override'}",
            "cargo-features=['artifact-dependencies']\n[package]\nname='root'",
            "[workspace]\nmembers=['crates/**']",
            "[workspace]\nmembers=['crates/a?']",
            "[workspace]\nmembers=['crates/*/*']",
        ] {
            provider.put("/work/Cargo.toml", manifest);
            assert!(resolve(&provider, "/work", &Selection::default()).is_err());
        }
        let provider = virtual_workspace();
        provider.list("/work/crates", &[("symlink-sentinel", false)]);
        assert!(matches!(
            resolve(&provider, "/work", &Selection::default()),
            Err(Failure::Manifest)
        ));
        assert!(
            !format!(
                "{:?}",
                resolve(&provider, "/work", &Selection::default()).err()
            )
            .contains("secret")
        );
    }

    #[test]
    fn new_glob_matches_and_missing_manifest_creation_invalidate_the_read_set() {
        let provider = virtual_workspace();
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(5));
        Sources::resolve(
            Path::new("/work/crates/a/src"),
            &Selection::default(),
            &mut reads,
        )
        .unwrap();
        reads.recheck().unwrap();
        provider.list(
            "/work/crates",
            &[("a", true), ("b", true), ("c", true), ("new", true)],
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        provider.list("/work/crates", &[("a", true), ("b", true), ("c", true)]);
        provider.put(
            "/work/crates/a/src/Cargo.toml",
            "[package]\nname='new'\nversion='0.1.0'",
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
    }

    #[test]
    fn duplicate_names_invalid_defaults_selectors_and_cycles_fail_safely() {
        let provider = virtual_workspace();
        provider.put("/work/crates/b/Cargo.toml", "[package]\nname='a'");
        assert!(resolve(&provider, "/work", &Selection::default()).is_err());
        provider.put(
            "/work/crates/b/Cargo.toml",
            "[package]\nname='b'\n[dependencies]\ncycle={path='/work/crates/b'}",
        );
        assert!(resolve(&provider, "/work/crates/b", &Selection::default()).is_err());
        for selection in [
            Selection {
                packages: vec!["a*".to_owned()],
                ..Selection::default()
            },
            Selection {
                workspace: true,
                packages: vec!["a".to_owned()],
                ..Selection::default()
            },
            Selection {
                exclude: vec!["a".to_owned()],
                ..Selection::default()
            },
            Selection {
                manifest: Some(PathBuf::from("Wrong.toml")),
                ..Selection::default()
            },
        ] {
            assert!(resolve(&provider, "/work", &selection).is_err());
        }
    }

    #[test]
    fn recursive_source_and_shared_input_bounds_never_accept_a_partial_inventory() {
        let provider = Scripted::default();
        for index in 0..18 {
            provider.put(
                &format!("/dep{index}/Cargo.toml"),
                &format!(
                    "[package]\nname='dep{index}'\n[dependencies]\nnext={{path='/dep{}'}}\n",
                    index + 1
                ),
            );
        }
        assert!(matches!(
            resolve(&provider, "/dep0", &Selection::default()),
            Err(Failure::Limit)
        ));
        let provider = virtual_workspace();
        let mut reads = ReadSet::new(&provider, Instant::now() + Duration::from_secs(5));
        for index in 0..63 {
            reads.read(Path::new(&format!("/config/{index}"))).unwrap();
        }
        assert!(matches!(
            Sources::resolve(Path::new("/work"), &Selection::default(), &mut reads),
            Err(Failure::Limit)
        ));
    }
}
