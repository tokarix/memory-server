//! Cargo 1.94 configuration sources, per-key precedence and private path bases.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use toml::Value;

use crate::config::execution::RustExecution;
use crate::limits;

use super::command::validate_extra_flags;
use super::environment::{Environment, Origin as EnvironmentOrigin};
use super::files::{MAX_DEPTH, ReadProvider, ReadSet};
use super::{BuildAction, Failure, paths};

/// Safe configuration provenance. Paths and values stay in private structures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Origin {
    /// Protected or literally overridden process environment.
    Environment,
    /// Cargo home configuration at the lowest discovery priority.
    CargoHome,
    /// Configuration discovered from the execution/discovery directory upwards.
    Ancestor,
    /// An explicit --config file, retaining the file's two-level path base.
    CommandFile,
    /// An explicit --config key=value, relative to the process cwd.
    CommandValue,
    /// A dedicated command option, which wins over configuration.
    DedicatedOption,
    /// Pinned tool default, which is used only after all applicable sources.
    Default,
}

/// Resolved private path and safe provenance; not a candidate completeness claim.
pub struct PathValue {
    path: PathBuf,
    origin: Origin,
}

impl PathValue {
    /// Absolute literal path, retaining `..` for actual filesystem resolution.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Safe origin category with no raw configuration data.
    #[must_use]
    pub const fn origin(&self) -> Origin {
        self.origin
    }
}

#[derive(Clone)]
struct Node {
    value: Kind,
    base: PathBuf,
    origin: Origin,
}

#[derive(Clone)]
enum Kind {
    Table(BTreeMap<String, Node>),
    Array(Vec<Node>),
    Scalar(Value),
}

impl Node {
    fn literal_value(&self) -> Value {
        match &self.value {
            Kind::Table(table) => Value::Table(
                table
                    .iter()
                    .map(|(key, node)| (key.clone(), node.literal_value()))
                    .collect(),
            ),
            Kind::Array(array) => Value::Array(array.iter().map(Self::literal_value).collect()),
            Kind::Scalar(value) => value.clone(),
        }
    }
    fn from_value(
        value: Value,
        base: &Path,
        origin: Origin,
        depth: usize,
    ) -> Result<Self, Failure> {
        if depth > MAX_DEPTH {
            return Err(Failure::Limit);
        }
        let value = match value {
            Value::Table(table) => Kind::Table(
                table
                    .into_iter()
                    .map(|(key, value)| {
                        Ok((key, Self::from_value(value, base, origin, depth + 1)?))
                    })
                    .collect::<Result<_, Failure>>()?,
            ),
            Value::Array(array) => Kind::Array(
                array
                    .into_iter()
                    .map(|value| Self::from_value(value, base, origin, depth + 1))
                    .collect::<Result<_, _>>()?,
            ),
            Value::String(ref value)
                if value.len() > limits::PATH_BYTES || value.contains('\0') =>
            {
                return Err(Failure::Limit);
            }
            value => Kind::Scalar(value),
        };
        Ok(Self {
            value,
            base: base.to_owned(),
            origin,
        })
    }

    fn table(&self) -> Result<&BTreeMap<String, Self>, Failure> {
        match &self.value {
            Kind::Table(table) => Ok(table),
            _ => Err(Failure::Configuration),
        }
    }

    fn string(&self) -> Result<&str, Failure> {
        match &self.value {
            Kind::Scalar(Value::String(value)) => Ok(value),
            _ => Err(Failure::Configuration),
        }
    }

    fn boolean(&self) -> Result<bool, Failure> {
        match self.value {
            Kind::Scalar(Value::Boolean(value)) => Ok(value),
            _ => Err(Failure::Configuration),
        }
    }

    fn path(&self) -> Result<PathValue, Failure> {
        Ok(PathValue {
            path: paths::absolute(&self.base, Path::new(self.string()?))?,
            origin: self.origin,
        })
    }

    fn merge(&mut self, higher: Self) -> Result<(), Failure> {
        let Self {
            value,
            base,
            origin,
        } = higher;
        match (&mut self.value, value) {
            (Kind::Table(lower), Kind::Table(higher)) => {
                for (key, value) in higher {
                    if let Some(lower) = lower.get_mut(&key) {
                        lower.merge(value)?;
                    } else {
                        lower.insert(key, value);
                    }
                }
            }
            (Kind::Array(lower), Kind::Array(mut higher)) => {
                lower.append(&mut higher);
                self.base = base;
                self.origin = origin;
            }
            (Kind::Scalar(lower), Kind::Scalar(higher))
                if std::mem::discriminant(lower) == std::mem::discriminant(&higher) =>
            {
                *lower = higher;
                self.base = base;
                self.origin = origin;
            }
            _ => return Err(Failure::Configuration),
        }
        Ok(())
    }
}

/// Merged supported Cargo configuration, separate from its process environment.
/// No method claims complete manifests, toolchain selection or build candidates.
pub struct Configuration {
    root: Node,
    cwd: PathBuf,
    cargo_home: PathBuf,
}

fn simple_absolute(path: &Path) -> Result<(), Failure> {
    paths::validate(path)?;
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(Failure::Cwd);
    }
    Ok(())
}

fn parse(bytes: &[u8], base: &Path, origin: Origin) -> Result<Node, Failure> {
    let text = std::str::from_utf8(bytes).map_err(|_| Failure::Configuration)?;
    let value: Value = toml::from_str(text).map_err(|_| Failure::Configuration)?;
    let root = Node::from_value(value, base, origin, 0)?;
    classify(&root, false)?;
    Ok(root)
}

fn selected_file<P: ReadProvider>(
    directory: &Path,
    origin: Origin,
    reads: &mut ReadSet<'_, P>,
) -> Result<Option<Node>, Failure> {
    let extensionless = reads.read(&directory.join("config"))?.map(<[u8]>::to_vec);
    let extended = reads
        .read(&directory.join("config.toml"))?
        .map(<[u8]>::to_vec);
    let base = directory.parent().ok_or(Failure::Configuration)?;
    extensionless
        .or(extended)
        .map(|bytes| parse(&bytes, base, origin))
        .transpose()
}

impl Configuration {
    /// Load home, ordered ancestors and explicit --config sources without Cargo.
    /// `cwd` is the proven physical execution cwd. Normal builds discover there;
    /// local install discovers at its proven --path directory instead. A remote
    /// install has no local discovery root, but still needs source completeness.
    /// The caller must supply only --config values extracted from validated argv.
    ///
    /// # Errors
    /// Bounds, failed reads, unsupported namespaces, merging and templates deny.
    pub fn load<P: ReadProvider>(
        cwd: &Path,
        discovery_root: Option<&Path>,
        environment: &Environment,
        overrides: &[String],
        reads: &mut ReadSet<'_, P>,
    ) -> Result<Self, Failure> {
        simple_absolute(cwd)?;
        if let Some(discovery) = discovery_root {
            simple_absolute(discovery)?;
        }
        if overrides.len() > limits::EXECUTION_WORDS {
            return Err(Failure::Limit);
        }
        if overrides.iter().map(String::len).sum::<usize>() > 64 * 1024 {
            return Err(Failure::Limit);
        }
        let cargo_home = if let Some(value) = environment.get("CARGO_HOME") {
            paths::absolute(cwd, Path::new(value))?
        } else {
            let home = environment.get("HOME").ok_or(Failure::Environment)?;
            paths::absolute(cwd, Path::new(home))?.join(".cargo")
        };
        // Without symlink/parent normalization, alias deduplication cannot be proven.
        simple_absolute(&cargo_home)?;
        let mut directories: Vec<_> = discovery_root
            .into_iter()
            .flat_map(Path::ancestors)
            .map(|path| path.join(".cargo"))
            .collect();
        if !directories.contains(&cargo_home) {
            directories.push(cargo_home.clone());
        }
        directories.reverse();
        let mut root = Node::from_value(Value::Table(toml::Table::new()), cwd, Origin::Default, 0)?;
        for directory in directories {
            let origin = if directory == cargo_home {
                Origin::CargoHome
            } else {
                Origin::Ancestor
            };
            if let Some(value) = selected_file(&directory, origin, reads)? {
                root.merge(value)?;
            }
        }
        for value in overrides {
            reads.checkpoint()?;
            if value.len() > limits::PATH_BYTES || value.contains('\0') {
                return Err(Failure::Limit);
            }
            let path = paths::absolute(cwd, Path::new(value))?;
            // Cargo checks existing file spellings before parsing inline TOML,
            // including names that contain '='. Absence is part of the read set.
            let contents = reads.read(&path)?;
            let value = if let Some(bytes) = contents {
                let base = path
                    .parent()
                    .and_then(Path::parent)
                    .ok_or(Failure::Configuration)?;
                parse(bytes, base, Origin::CommandFile)?
            } else if value.contains('=') {
                // Exactly one literal TOML assignment; tables/scripts are not inline overrides.
                if value.contains('\n') || value.contains('\r') {
                    return Err(Failure::Configuration);
                }
                parse(value.as_bytes(), cwd, Origin::CommandValue)?
            } else {
                return Err(Failure::Read);
            };
            root.merge(value)?;
        }
        // Validate merged types again: combining partial [env] tables can change semantics.
        classify(&root, true)?;
        reads.checkpoint()?;
        Ok(Self {
            root,
            cwd: cwd.to_owned(),
            cargo_home,
        })
    }

    fn node(&self, key: &[&str]) -> Result<Option<&Node>, Failure> {
        let mut node = &self.root;
        for key in key {
            let Some(next) = node.table()?.get(*key) else {
                return Ok(None);
            };
            node = next;
        }
        Ok(Some(node))
    }

    fn configured_path(
        &self,
        key: &[&str],
        environment: &Environment,
        variable: &str,
    ) -> Result<Option<PathValue>, Failure> {
        let node = self.node(key)?;
        if node
            .is_some_and(|node| matches!(node.origin, Origin::CommandFile | Origin::CommandValue))
        {
            return node.map(Node::path).transpose();
        }
        if let Some(value) = environment.get(variable) {
            return self.literal_path(value, Origin::Environment).map(Some);
        }
        node.map(Node::path).transpose()
    }

    fn literal_path(&self, value: &str, origin: Origin) -> Result<PathValue, Failure> {
        Ok(PathValue {
            path: paths::absolute(&self.cwd, Path::new(value))?,
            origin,
        })
    }

    /// Cargo's effective cache home, before Cargo child-process [env].
    #[must_use]
    pub fn cargo_home(&self) -> &Path {
        &self.cargo_home
    }

    /// Literal target selectors with CLI-config/environment/discovery precedence.
    /// Local install ignores build.target, matching the pinned command contract.
    /// This only selects layout names; custom JSON targets remain unsupported.
    ///
    /// # Errors
    /// Ambiguous array/environment merging or invalid types deny.
    pub fn targets(
        &self,
        install: bool,
        options: &[String],
        environment: &Environment,
    ) -> Result<Vec<String>, Failure> {
        if !options.is_empty() {
            return Ok(options.to_vec());
        }
        if install {
            return Ok(Vec::new());
        }
        let node = self.node(&["build", "target"])?;
        if environment.get("CARGO_BUILD_TARGET").is_some()
            && node.is_some_and(|node| matches!(node.value, Kind::Array(_)))
        {
            return Err(Failure::Configuration);
        }
        if !node
            .is_some_and(|node| matches!(node.origin, Origin::CommandFile | Origin::CommandValue))
            && let Some(value) = environment.get("CARGO_BUILD_TARGET")
        {
            return Ok(vec![value.to_owned()]);
        }
        node.map(|node| match &node.value {
            Kind::Array(array) => array
                .iter()
                .map(|node| node.string().map(str::to_owned))
                .collect(),
            _ => node.string().map(|value| vec![value.to_owned()]),
        })
        .transpose()
        .map(Option::unwrap_or_default)
    }

    /// Bounded private profile fields after configuration merging. Repository
    /// profile data is execution input, never protected storage policy authority.
    ///
    /// # Errors
    /// Invalid profile namespaces deny before the layout stage.
    pub fn profiles(&self) -> Result<Option<Value>, Failure> {
        self.node(&["profile"])
            .map(|value| value.map(Node::literal_value))
    }

    /// Target precedence: flag, `CARGO_TARGET_DIR`, CLI config, configuration env,
    /// discovered configuration, then the selected workspace's target directory.
    ///
    /// # Errors
    /// Invalid or empty paths and unmodeled types fail conservatively.
    pub fn target_directory(
        &self,
        workspace: &Path,
        option: Option<&str>,
        environment: &Environment,
    ) -> Result<PathValue, Failure> {
        if let Some(value) = option {
            return self.literal_path(value, Origin::DedicatedOption);
        }
        if let Some(value) = environment.get("CARGO_TARGET_DIR") {
            return self.literal_path(value, Origin::Environment);
        }
        if let Some(value) = self.configured_path(
            &["build", "target-dir"],
            environment,
            "CARGO_BUILD_TARGET_DIR",
        )? {
            return Ok(value);
        }
        Ok(PathValue {
            path: paths::absolute(workspace, Path::new("target"))?,
            origin: Origin::Default,
        })
    }

    /// Intermediate build directory, defaulting to the effective target.
    ///
    /// # Errors
    /// Templates and invalid/opaque path semantics fail conservatively.
    pub fn build_directory(
        &self,
        target: &Path,
        environment: &Environment,
    ) -> Result<PathValue, Failure> {
        let value = self.configured_path(
            &["build", "build-dir"],
            environment,
            "CARGO_BUILD_BUILD_DIR",
        )?;
        if let Some(value) = value {
            if value
                .path
                .as_os_str()
                .as_encoded_bytes()
                .iter()
                .any(|byte| matches!(byte, b'{' | b'}'))
            {
                return Err(Failure::Configuration);
            }
            return Ok(value);
        }
        paths::validate(target)?;
        if !target.is_absolute() {
            return Err(Failure::Configuration);
        }
        Ok(PathValue {
            path: target.to_owned(),
            origin: Origin::Default,
        })
    }

    /// Install precedence: `--root`, `CARGO_INSTALL_ROOT`, `install.root`, `CARGO_HOME`.
    ///
    /// # Errors
    /// Empty, invalid and unmodeled paths fail conservatively.
    pub fn install_root(
        &self,
        option: Option<&str>,
        environment: &Environment,
    ) -> Result<PathValue, Failure> {
        if let Some(value) = option {
            return self.literal_path(value, Origin::DedicatedOption);
        }
        if let Some(value) = environment.get("CARGO_INSTALL_ROOT") {
            return self.literal_path(value, Origin::Environment);
        }
        if let Some(node) = self.node(&["install", "root"])? {
            return node.path();
        }
        Ok(PathValue {
            path: self.cargo_home.clone(),
            origin: Origin::Default,
        })
    }

    /// Apply Cargo's [env] to child processes only, with force/relative semantics.
    /// This does not feed [env] back into Cargo's own target/cache/config inputs.
    ///
    /// # Errors
    /// Unknown variables, forbidden rustup/Cargo home selectors or invalid types deny.
    pub fn child_environment(&self, inherited: &Environment) -> Result<Environment, Failure> {
        let mut child = inherited.clone();
        if let Some(env) = self.node(&["env"])? {
            for (key, node) in env.table()? {
                let (value, force, relative) = env_entry(node)?;
                if force || inherited.get(key).is_none() {
                    let value = if relative {
                        paths::absolute(&value.base, Path::new(value.string()?))?
                            .to_str()
                            .ok_or(Failure::Configuration)?
                            .to_owned()
                    } else {
                        value.string()?.to_owned()
                    };
                    child.set(key, Some(&value), EnvironmentOrigin::CargoConfig)?;
                }
            }
        }
        Ok(child)
    }

    /// Validate effective compiler identities, wrapper exclusions and flag sources.
    /// The caller still must validate the installed toolchain and locality.
    ///
    /// # Errors
    /// Compiler replacements, wrappers and output-changing or opaque flags deny.
    pub fn validate_execution(
        &self,
        environment: &Environment,
        contract: &RustExecution,
    ) -> Result<(), Failure> {
        let child = self.child_environment(environment)?;
        if child.get("PATH") != contract.environment().get("PATH").map(String::as_str) {
            // Cargo and its extensions launch secondary programs by name.
            return Err(Failure::Executable);
        }
        for (key, tool) in [("RUSTC", "rustc"), ("RUSTDOC", "rustdoc")] {
            if let Some(value) = environment.get(key) {
                if contract.tool(value) != Some(tool) {
                    return Err(Failure::Executable);
                }
            } else if let Some(node) = self.node(&["build", tool])? {
                let value = node.string()?;
                let path = if value.contains('/') {
                    node.path()?.path
                } else {
                    PathBuf::from(value)
                };
                if contract.tool(path.to_str().ok_or(Failure::Executable)?) != Some(tool) {
                    return Err(Failure::Executable);
                }
            }
            if child.get(key) != environment.get(key) {
                return Err(Failure::Executable);
            }
        }
        for (variable, config) in [
            ("RUSTC_WRAPPER", "rustc-wrapper"),
            ("RUSTC_WORKSPACE_WRAPPER", "rustc-workspace-wrapper"),
        ] {
            if child.get(variable).is_some_and(|value| !value.is_empty())
                || self
                    .node(&["build", config])?
                    .is_some_and(|node| node.string() != Ok(""))
            {
                return Err(Failure::Executable);
            }
        }
        for (action, encoded, variable, config, config_env) in [
            (
                BuildAction::Rustc,
                "CARGO_ENCODED_RUSTFLAGS",
                "RUSTFLAGS",
                "rustflags",
                "CARGO_BUILD_RUSTFLAGS",
            ),
            (
                BuildAction::Rustdoc,
                "CARGO_ENCODED_RUSTDOCFLAGS",
                "RUSTDOCFLAGS",
                "rustdocflags",
                "CARGO_BUILD_RUSTDOCFLAGS",
            ),
        ] {
            let flags = if let Some(value) = environment.get(encoded) {
                if value.is_empty() {
                    Vec::new()
                } else {
                    value.split('\u{1f}').map(str::to_owned).collect()
                }
            } else if let Some(value) = environment.get(variable) {
                value.split_whitespace().map(str::to_owned).collect()
            } else {
                let node = self.node(&["build", config])?;
                if environment.get(config_env).is_some() && node.is_some() {
                    // Array/environment merging is a separate Cargo mechanism.
                    return Err(Failure::Configuration);
                }
                if let Some(value) = environment.get(config_env).filter(|_| {
                    !node.is_some_and(|node| {
                        matches!(node.origin, Origin::CommandFile | Origin::CommandValue)
                    })
                }) {
                    value.split_whitespace().map(str::to_owned).collect()
                } else {
                    node.map(flags_value).transpose()?.unwrap_or_default()
                }
            };
            validate_extra_flags(action, &flags)?;
            for key in [encoded, variable, config_env] {
                if child.get(key) != environment.get(key) {
                    return Err(Failure::Environment);
                }
            }
        }
        Ok(())
    }
}

fn flags_value(node: &Node) -> Result<Vec<String>, Failure> {
    match &node.value {
        Kind::Array(array) => array
            .iter()
            .map(|node| node.string().map(str::to_owned))
            .collect(),
        _ => Ok(node
            .string()?
            .split_whitespace()
            .map(str::to_owned)
            .collect()),
    }
}

fn env_entry(node: &Node) -> Result<(&Node, bool, bool), Failure> {
    if matches!(node.value, Kind::Scalar(Value::String(_))) {
        return Ok((node, false, false));
    }
    let table = node.table()?;
    if table
        .keys()
        .any(|key| !matches!(key.as_str(), "value" | "force" | "relative"))
    {
        return Err(Failure::Configuration);
    }
    let value = table.get("value").ok_or(Failure::Configuration)?;
    value.string()?;
    let force = table
        .get("force")
        .map(Node::boolean)
        .transpose()?
        .unwrap_or(false);
    let relative = table
        .get("relative")
        .map(Node::boolean)
        .transpose()?
        .unwrap_or(false);
    Ok((value, force, relative))
}

fn classify(root: &Node, complete: bool) -> Result<(), Failure> {
    for (key, node) in root.table()? {
        match key.as_str() {
            "profile" => super::cargo::validate_profiles(&node.literal_value())?,
            "build" => classify_build(node)?,
            "install" => {
                for (key, node) in node.table()? {
                    if key != "root" {
                        return Err(Failure::Configuration);
                    }
                    node.string()?;
                }
            }
            "env" => classify_env(node, complete)?,
            "net" => {
                for (key, node) in node.table()? {
                    match key.as_str() {
                        "offline" => {
                            node.boolean()?;
                        }
                        "retry" if matches!(node.value, Kind::Scalar(Value::Integer(_))) => {}
                        "git-fetch-with-cli" if !node.boolean()? => {}
                        _ => return Err(Failure::Configuration),
                    }
                }
            }
            // These namespaces do not select an executable or build destination
            // for the supported commands (doc --open and custom aliases deny).
            "alias" => {
                if node
                    .table()?
                    .keys()
                    .any(|key| matches!(key.as_str(), "b" | "c" | "t" | "r" | "d" | "clippy"))
                {
                    return Err(Failure::Configuration);
                }
            }
            "term" => {
                let allowed = [
                    "quiet",
                    "verbose",
                    "color",
                    "hyperlinks",
                    "unicode",
                    "progress",
                ];
                if node
                    .table()?
                    .keys()
                    .any(|key| !allowed.contains(&key.as_str()))
                {
                    return Err(Failure::Configuration);
                }
                if let Some(progress) = node.table()?.get("progress")
                    && progress
                        .table()?
                        .keys()
                        .any(|key| !matches!(key.as_str(), "when" | "width" | "term-integration"))
                {
                    return Err(Failure::Configuration);
                }
            }
            "future-incompat-report" | "cache" | "cargo-new" => {
                let allowed = match key.as_str() {
                    "future-incompat-report" => "frequency",
                    "cache" => "auto-clean-frequency",
                    _ => "vcs",
                };
                if node.table()?.keys().any(|key| key != allowed) {
                    return Err(Failure::Configuration);
                }
            }
            "doc" => {
                if node.table()?.keys().any(|key| key != "browser") {
                    return Err(Failure::Configuration);
                }
            }
            // Source replacement, credentials, target cfg/linkers/runners,
            // includes and unknown relevant keys are opaque.
            _ => return Err(Failure::Configuration),
        }
    }
    Ok(())
}

fn classify_build(build: &Node) -> Result<(), Failure> {
    for (key, node) in build.table()? {
        match key.as_str() {
            "target-dir"
            | "build-dir"
            | "rustc"
            | "rustdoc"
            | "rustc-wrapper"
            | "rustc-workspace-wrapper"
            | "dep-info-basedir" => {
                node.string()?;
            }
            "target" => match &node.value {
                Kind::Array(array) => {
                    for node in array {
                        node.string()?;
                    }
                }
                _ => {
                    node.string()?;
                }
            },
            "rustflags" => {
                validate_extra_flags(BuildAction::Rustc, &flags_value(node)?)?;
            }
            "rustdocflags" => {
                validate_extra_flags(BuildAction::Rustdoc, &flags_value(node)?)?;
            }
            "incremental" => {
                node.boolean()?;
            }
            "jobs" if matches!(node.value, Kind::Scalar(Value::Integer(_))) => {}
            _ => return Err(Failure::Configuration),
        }
    }
    Ok(())
}

fn classify_env(env: &Node, complete: bool) -> Result<(), Failure> {
    for (key, node) in env.table()? {
        if !crate::config::execution::supported_environment_key(key)
            || matches!(
                key.as_str(),
                "CARGO_HOME" | "RUSTUP_HOME" | "RUSTUP_TOOLCHAIN"
            )
        {
            return Err(Failure::Environment);
        }
        if complete || matches!(node.value, Kind::Scalar(_)) {
            env_entry(node)?;
        } else {
            for (key, value) in node.table()? {
                match key.as_str() {
                    "value" => {
                        value.string()?;
                    }
                    "force" | "relative" => {
                        value.boolean()?;
                    }
                    _ => return Err(Failure::Configuration),
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::time::{Duration, Instant};

    use crate::config::execution::literal_fixture;

    use super::{Configuration, Failure, Origin};
    use crate::rust_build::environment::{Environment, Origin as EnvironmentOrigin};
    use crate::rust_build::files::{ReadSet, fixtures::Scripted};

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }

    fn environment() -> Environment {
        Environment::inherited(&BTreeMap::from([
            ("HOME".to_owned(), "/home/operator".to_owned()),
            ("CARGO_HOME".to_owned(), "/home/operator/.cargo".to_owned()),
            ("TMPDIR".to_owned(), "/compiler-temp".to_owned()),
        ]))
        .unwrap()
    }

    fn load<'a>(
        provider: &'a Scripted,
        env: &Environment,
        overrides: &[String],
    ) -> (Configuration, ReadSet<'a, Scripted>) {
        let mut reads = ReadSet::new(provider, deadline());
        let config = Configuration::load(
            Path::new("/work/nested"),
            Some(Path::new("/work/nested")),
            env,
            overrides,
            &mut reads,
        )
        .unwrap();
        (config, reads)
    }

    #[test]
    fn discovery_order_extension_preference_and_absence_are_explicit() {
        let provider = Scripted::default();
        provider.put(
            "/home/operator/.cargo/config.toml",
            "[build]\ntarget-dir='home-output'",
        );
        provider.put("/.cargo/config.toml", "[build]\ntarget-dir='root-output'");
        provider.put(
            "/work/.cargo/config.toml",
            "[build]\ntarget-dir='work-output'",
        );
        // Cargo ignores the contents of the extended file when both exist.
        provider.put(
            "/work/nested/.cargo/config.toml",
            "malformed secret-sentinel",
        );
        provider.put(
            "/work/nested/.cargo/config",
            "[build]\ntarget-dir='nested-output'",
        );
        let env = environment();
        let (config, reads) = load(&provider, &env, &[]);
        let target = config
            .target_directory(Path::new("/workspace"), None, &env)
            .unwrap();
        assert_eq!(target.path(), Path::new("/work/nested/nested-output"));
        assert_eq!(target.origin(), Origin::Ancestor);
        for path in [
            "/home/operator/.cargo/config",
            "/home/operator/.cargo/config.toml",
            "/.cargo/config",
            "/.cargo/config.toml",
            "/work/.cargo/config",
            "/work/.cargo/config.toml",
            "/work/nested/.cargo/config",
            "/work/nested/.cargo/config.toml",
        ] {
            assert!(
                provider
                    .calls
                    .borrow()
                    .contains(&Path::new(path).to_owned())
            );
        }
        reads.recheck().unwrap();
        provider.put(
            "/work/.cargo/config",
            "[build]\ntarget-dir='new-higher-precedence'",
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
    }

    #[test]
    fn existing_override_file_wins_over_equals_spelling_and_creation_invalidates() {
        let provider = Scripted::default();
        let env = environment();
        let spelling = "build.target-dir='inline'".to_owned();
        let (config, reads) = load(&provider, &env, std::slice::from_ref(&spelling));
        assert_eq!(
            config
                .target_directory(Path::new("/workspace"), None, &env)
                .unwrap()
                .path(),
            Path::new("/work/nested/inline")
        );
        provider.put(
            "/work/nested/build.target-dir='inline'",
            "[build]\ntarget-dir='file-wins'",
        );
        assert_eq!(reads.recheck(), Err(Failure::Changed));
        let (config, _) = load(&provider, &env, &[spelling]);
        let target = config
            .target_directory(Path::new("/workspace"), None, &env)
            .unwrap();
        assert_eq!(target.path(), Path::new("/work/file-wins"));
        assert_eq!(target.origin(), Origin::CommandFile);
    }

    #[test]
    fn output_keys_keep_their_distinct_precedence_and_bases() {
        let provider = Scripted::default();
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[build]\ntarget-dir='discovered'\nbuild-dir='intermediate'\n[install]\nroot='install'",
        );
        provider.put("/overrides/sub/config.toml", "[build]\ntarget-dir='file-output'\nbuild-dir='../file-intermediate'\n[install]\nroot='file-install'");
        let mut env = environment();
        let workspace = Path::new("/selected/workspace");
        let (config, _) = load(&provider, &env, &[]);
        assert_eq!(
            config
                .target_directory(workspace, None, &env)
                .unwrap()
                .path(),
            Path::new("/work/nested/discovered")
        );
        env.set(
            "CARGO_BUILD_TARGET_DIR",
            Some("environment"),
            EnvironmentOrigin::Assignment,
        )
        .unwrap();
        assert_eq!(
            config
                .target_directory(workspace, None, &env)
                .unwrap()
                .path(),
            Path::new("/work/nested/environment")
        );
        let (config, _) = load(
            &provider,
            &env,
            &[
                "/overrides/sub/config.toml".to_owned(),
                "build.target-dir='inline output'".to_owned(),
            ],
        );
        assert_eq!(
            config
                .target_directory(workspace, None, &env)
                .unwrap()
                .path(),
            Path::new("/work/nested/inline output")
        );
        let (config, _) = load(
            &provider,
            &env,
            &[
                "build.target-dir='inline'".to_owned(),
                "/overrides/sub/config.toml".to_owned(),
            ],
        );
        let value = config.target_directory(workspace, None, &env).unwrap();
        assert_eq!(value.path(), Path::new("/overrides/file-output"));
        assert_eq!(value.origin(), Origin::CommandFile);
        env.set(
            "CARGO_TARGET_DIR",
            Some("special"),
            EnvironmentOrigin::Wrapper,
        )
        .unwrap();
        assert_eq!(
            config
                .target_directory(workspace, None, &env)
                .unwrap()
                .path(),
            Path::new("/work/nested/special")
        );
        let value = config
            .target_directory(workspace, Some("dedicated"), &env)
            .unwrap();
        assert_eq!(value.path(), Path::new("/work/nested/dedicated"));
        assert_eq!(value.origin(), Origin::DedicatedOption);
        env.set(
            "CARGO_BUILD_BUILD_DIR",
            Some("env-intermediate"),
            EnvironmentOrigin::Assignment,
        )
        .unwrap();
        assert_eq!(
            config.build_directory(value.path(), &env).unwrap().path(),
            Path::new("/overrides/../file-intermediate")
        );
    }

    #[test]
    fn install_root_keeps_its_command_specific_precedence() {
        let provider = Scripted::default();
        provider.put(
            "/overrides/sub/config.toml",
            "[install]\nroot='file-install'",
        );
        let mut env = environment();
        let (config, _) = load(&provider, &env, &["/overrides/sub/config.toml".to_owned()]);
        assert_eq!(
            config.install_root(None, &env).unwrap().path(),
            Path::new("/overrides/file-install")
        );
        env.set(
            "CARGO_INSTALL_ROOT",
            Some("env-install"),
            EnvironmentOrigin::Assignment,
        )
        .unwrap();
        assert_eq!(
            config.install_root(None, &env).unwrap().path(),
            Path::new("/work/nested/env-install")
        );
        assert_eq!(
            config
                .install_root(Some("flag-install"), &env)
                .unwrap()
                .path(),
            Path::new("/work/nested/flag-install")
        );
    }

    #[test]
    fn home_fallback_defaults_and_install_discovery_differ_from_manifest_selection() {
        let provider = Scripted::default();
        let mut env = environment();
        env.set("CARGO_HOME", None, EnvironmentOrigin::Wrapper)
            .unwrap();
        let (config, _) = load(&provider, &env, &[]);
        assert_eq!(config.cargo_home(), Path::new("/home/operator/.cargo"));
        let target = config
            .target_directory(Path::new("/selected/workspace"), None, &env)
            .unwrap();
        assert_eq!(target.path(), Path::new("/selected/workspace/target"));
        assert_eq!(
            config.build_directory(target.path(), &env).unwrap().path(),
            target.path()
        );
        assert_eq!(
            config.install_root(None, &env).unwrap().path(),
            config.cargo_home()
        );
        provider.put(
            "/selected/workspace/.cargo/config.toml",
            "[build]\ntarget-dir='selected-output'",
        );
        // A normal --manifest-path does not relocate discovery from /work/nested.
        let (config, _) = load(&provider, &env, &[]);
        assert_eq!(
            config
                .target_directory(Path::new("/selected/workspace"), None, &env)
                .unwrap()
                .path(),
            target.path()
        );
        let mut reads = ReadSet::new(&provider, deadline());
        let install = Configuration::load(
            Path::new("/work/nested"),
            Some(Path::new("/selected/workspace")),
            &env,
            &[],
            &mut reads,
        )
        .unwrap();
        assert_eq!(
            install
                .target_directory(Path::new("/selected/workspace"), None, &env)
                .unwrap()
                .path(),
            Path::new("/selected/workspace/selected-output")
        );
        env.clear();
        let mut reads = ReadSet::new(&provider, deadline());
        assert!(matches!(
            Configuration::load(
                Path::new("/work"),
                Some(Path::new("/work")),
                &env,
                &[],
                &mut reads
            ),
            Err(Failure::Environment)
        ));
    }

    #[test]
    fn child_env_force_relative_and_table_merge_do_not_reconfigure_cargo() {
        let provider = Scripted::default();
        provider.put(
            "/work/.cargo/config.toml",
            "[env.TMPDIR]\nvalue='compiler tmp'\nrelative=true",
        );
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[env.TMPDIR]\nforce=true\n[env]\nCARGO_TARGET_DIR={value='child-target',force=true}",
        );
        let env = environment();
        let (config, _) = load(&provider, &env, &[]);
        let child = config.child_environment(&env).unwrap();
        assert_eq!(child.get("TMPDIR"), Some("/work/compiler tmp"));
        assert_eq!(child.origin("TMPDIR"), Some(EnvironmentOrigin::CargoConfig));
        assert_eq!(child.get("CARGO_TARGET_DIR"), Some("child-target"));
        assert_eq!(
            config
                .target_directory(Path::new("/workspace"), None, &env)
                .unwrap()
                .path(),
            Path::new("/workspace/target")
        );
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[env.TMPDIR]\nforce=false",
        );
        let (config, _) = load(&provider, &env, &[]);
        assert_eq!(
            config.child_environment(&env).unwrap().get("TMPDIR"),
            Some("/compiler-temp")
        );
        let mut env = env;
        env.set("TMPDIR", None, EnvironmentOrigin::Wrapper).unwrap();
        assert_eq!(
            config.child_environment(&env).unwrap().get("TMPDIR"),
            Some("/work/compiler tmp")
        );
    }

    #[test]
    fn inline_env_table_is_one_assignment_with_cwd_relative_provenance() {
        let provider = Scripted::default();
        let env = environment();
        let (config, reads) = load(
            &provider,
            &env,
            &["env.TMPDIR={value='inline tmp',force=true,relative=true}".to_owned()],
        );
        assert_eq!(
            config.child_environment(&env).unwrap().get("TMPDIR"),
            Some("/work/nested/inline tmp")
        );
        reads.recheck().unwrap();
    }

    #[test]
    fn array_merging_and_effective_compilers_never_launch_an_executable() {
        let provider = Scripted::default();
        let (contract, fixture) = literal_fixture();
        let env = Environment::inherited(contract.environment()).unwrap();
        provider.put(
            "/work/.cargo/config.toml",
            "[build]\nrustflags=['-C','opt-level=1']",
        );
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[build]\nrustflags=['-C','debuginfo=1']\nrustc='rustc'\nrustdoc='rustdoc'",
        );
        let (config, _) = load(&provider, &env, &[]);
        config.validate_execution(&env, &contract).unwrap();
        assert_eq!(
            super::flags_value(config.node(&["build", "rustflags"]).unwrap().unwrap()).unwrap(),
            ["-C", "opt-level=1", "-C", "debuginfo=1"]
        );
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[build]\nrustc='/secret-sentinel/compiler'",
        );
        let (config, _) = load(&provider, &env, &[]);
        assert_eq!(
            config.validate_execution(&env, &contract),
            Err(Failure::Executable)
        );
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[build]\nrustc-wrapper='/secret-sentinel/wrapper'",
        );
        let (config, _) = load(&provider, &env, &[]);
        assert_eq!(
            config.validate_execution(&env, &contract),
            Err(Failure::Executable)
        );
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[env]\nPATH={value='/secret-sentinel',force=true}",
        );
        let (config, _) = load(&provider, &env, &[]);
        assert_eq!(
            config.validate_execution(&env, &contract),
            Err(Failure::Executable)
        );
        for key in [
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "RUSTDOCFLAGS",
            "CARGO_ENCODED_RUSTDOCFLAGS",
        ] {
            let mut changed = env.clone();
            changed
                .set(
                    key,
                    Some("--out-dir=/secret-sentinel/output"),
                    EnvironmentOrigin::Assignment,
                )
                .unwrap();
            assert!(config.validate_execution(&changed, &contract).is_err());
        }
        for tool in ["cargo", "rustc", "rustdoc"] {
            assert_eq!(
                std::fs::read(fixture.path().join(tool)).unwrap(),
                b"NONEXECUTION_SENTINEL"
            );
        }
        assert!(!fixture.path().join("marker").exists());
    }

    #[test]
    fn unsupported_namespaces_invalid_types_templates_and_overflow_are_safe() {
        for text in [
            "[alias]\nb='!touch secret-sentinel'",
            "[alias]\nclippy='!touch secret-sentinel'",
            "include=['other.toml']",
            "paths=['/external']",
            "[target.x86_64-unknown-linux-gnu]\nrunner='secret-sentinel'",
            "[build]\nnew-output='secret-sentinel'",
            "[source.vendored]\ndirectory='secret-sentinel'",
            "[profile.dev]\nrustflags=['-o','secret-sentinel']",
            "[env]\nCARGO_HOME='secret-sentinel'",
            "[env]\nBASH_ENV='secret-sentinel'",
            "[env]\nTMPDIR={value='secret-sentinel',force='true'}",
            "[build]\nrustflags=['-C','incremental=secret-sentinel']",
            "[net]\ngit-fetch-with-cli=true",
            "[unknown]\noutput='secret-sentinel'",
            "[build]\ntarget-dir=123",
            "[env]\nTMPDIR={force=true}",
            "[build]\nrustflags=['@secret-sentinel']",
        ] {
            let provider = Scripted::default();
            provider.put("/work/nested/.cargo/config.toml", text);
            let mut reads = ReadSet::new(&provider, deadline());
            let result = Configuration::load(
                Path::new("/work/nested"),
                Some(Path::new("/work/nested")),
                &environment(),
                &[],
                &mut reads,
            );
            assert!(result.is_err(), "{text}");
            assert!(!format!("{:?}", result.err()).contains("secret-sentinel"));
        }
        let provider = Scripted::default();
        provider.put(
            "/work/nested/.cargo/config.toml",
            "[build]\nbuild-dir='{workspace-root}/build'",
        );
        let (config, _) = load(&provider, &environment(), &[]);
        assert!(matches!(
            config.build_directory(Path::new("/target"), &environment()),
            Err(Failure::Configuration)
        ));
        let mut reads = ReadSet::new(&provider, deadline());
        assert!(
            Configuration::load(
                Path::new("/work"),
                Some(Path::new("/work")),
                &environment(),
                &["build.target-dir='one'\ninstall.root='two'".to_owned()],
                &mut reads
            )
            .is_err()
        );
        let text = format!(
            "[{}]\nvalue='x'",
            std::iter::repeat_n("nested", 18)
                .collect::<Vec<_>>()
                .join(".")
        );
        assert!(matches!(
            super::parse(text.as_bytes(), Path::new("/work"), Origin::Ancestor),
            Err(Failure::Limit)
        ));
        let mut reads = ReadSet::new(&provider, deadline());
        let too_deep = format!(
            "/{}",
            std::iter::repeat_n("sub", 34).collect::<Vec<_>>().join("/")
        );
        assert!(matches!(
            Configuration::load(
                Path::new(&too_deep),
                Some(Path::new(&too_deep)),
                &environment(),
                &[],
                &mut reads
            ),
            Err(Failure::Limit)
        ));
    }
}
