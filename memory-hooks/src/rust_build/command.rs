//! A fully consumed literal grammar; this module never starts a process.

use std::path::{Path, PathBuf};

use crate::config::execution::RustExecution;
use crate::execution_input::{CommandInput, ExecutionInput};
use crate::limits;

use super::environment::{Environment, Origin, valid_name};
use super::{Action, BuildAction, Failure};

struct Word {
    value: String,
    assignment: bool,
}
enum Token {
    Word(Word),
    And,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

/// Parsed invocation. All command/environment/path data remains private.
pub struct Invocation {
    action: Action,
    tool: String,
    arguments: Vec<String>,
    cwd: Option<PathBuf>,
    environment: Environment,
    explicit_toolchain: bool,
}

impl Invocation {
    /// Safe action category; build means resolution is still required.
    #[must_use]
    pub const fn action(&self) -> Action {
        self.action
    }
    /// Pinned executable's fixed registry name, with no path.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }
    /// Literal private arguments for the next bounded resolution stage.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        &self.arguments
    }
    /// Literal cwd transition, resolved independently of callback identity.
    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.cwd.as_deref()
    }
    /// Effective environment after all explicit literal overrides.
    #[must_use]
    pub const fn environment(&self) -> &Environment {
        &self.environment
    }
    /// Whether an explicit pinned selector was consumed.
    #[must_use]
    pub const fn explicit_toolchain(&self) -> bool {
        self.explicit_toolchain
    }
}

fn finish(
    tokens: &mut Vec<Token>,
    value: &mut String,
    started: &mut bool,
    assignment: &mut bool,
) -> Result<(), Failure> {
    if !*started {
        return Ok(());
    }
    if value.len() > limits::PATH_BYTES || tokens.len() > limits::EXECUTION_WORDS {
        return Err(Failure::Limit);
    }
    tokens.push(Token::Word(Word {
        value: std::mem::take(value),
        assignment: *assignment,
    }));
    *started = false;
    *assignment = true;
    Ok(())
}

fn escaped(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    value: &mut String,
    quote: Quote,
) -> Result<(), Failure> {
    let character = chars.next().ok_or(Failure::Syntax)?;
    if character == '\n' || character == '\r' {
        return Err(Failure::Syntax);
    }
    if quote == Quote::Double && !matches!(character, '$' | '`' | '"' | '\\') {
        value.push('\\');
    }
    value.push(character);
    Ok(())
}

fn lex(source: &str) -> Result<Vec<Token>, Failure> {
    if source.len() > 64 * 1024 || source.contains('\0') {
        return Err(Failure::Limit);
    }
    let mut tokens = Vec::new();
    let mut value = String::new();
    let mut started = false;
    let mut assignment = true;
    let mut quote = Quote::None;
    let mut chars = source.chars().peekable();
    while let Some(character) = chars.next() {
        match (quote, character) {
            (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::None,
            (Quote::Single, _) => value.push(character),
            (Quote::None, '\'' | '"') => {
                quote = if character == '\'' {
                    Quote::Single
                } else {
                    Quote::Double
                };
                started = true;
                if !value.contains('=') {
                    assignment = false;
                }
            }
            (Quote::None | Quote::Double, '\\') => {
                started = true;
                if !value.contains('=') {
                    assignment = false;
                }
                escaped(&mut chars, &mut value, quote)?;
            }
            (Quote::None, ' ' | '\t') => {
                finish(&mut tokens, &mut value, &mut started, &mut assignment)?;
            }
            (Quote::None, '&') if chars.peek() == Some(&'&') => {
                chars.next();
                finish(&mut tokens, &mut value, &mut started, &mut assignment)?;
                tokens.push(Token::And);
            }
            (Quote::None | Quote::Double, '$' | '`')
            | (
                Quote::None,
                '\n' | '\r' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | ']' | '~'
                | '{' | '}',
            ) => return Err(Failure::Syntax),
            (Quote::None, '#') if !started => return Err(Failure::Syntax),
            (_, character) => {
                started = true;
                value.push(character);
            }
        }
        if value.len() > limits::PATH_BYTES || tokens.len() > limits::EXECUTION_WORDS + 1 {
            return Err(Failure::Limit);
        }
    }
    if quote != Quote::None {
        return Err(Failure::Syntax);
    }
    finish(&mut tokens, &mut value, &mut started, &mut assignment)?;
    Ok(tokens)
}

fn literal_words(input: &ExecutionInput) -> Result<(Vec<Word>, Option<PathBuf>), Failure> {
    let tokens = match input.command() {
        CommandInput::Shell(source) => lex(source)?,
        CommandInput::Argv(words) => words
            .iter()
            .map(|word| {
                Token::Word(Word {
                    value: word.clone(),
                    assignment: false,
                })
            })
            .collect(),
    };
    let mut cwd = input.cwd().map(Path::to_path_buf);
    if tokens
        .iter()
        .filter(|token| matches!(token, Token::Word(_)))
        .count()
        > limits::EXECUTION_WORDS
    {
        return Err(Failure::Limit);
    }
    let mut words = Vec::new();
    let mut transition = false;
    for token in tokens {
        match token {
            Token::Word(word) => words.push(word),
            Token::And if !transition && cwd.is_none() => {
                let directory = match words.as_slice() {
                    [command, directory] if command.value == "cd" => &directory.value,
                    [command, option, directory]
                        if command.value == "cd" && option.value == "--" =>
                    {
                        &directory.value
                    }
                    _ => return Err(Failure::Syntax),
                };
                if directory.is_empty() || directory.starts_with('-') {
                    return Err(Failure::Cwd);
                }
                cwd = Some(PathBuf::from(directory));
                words.clear();
                transition = true;
            }
            Token::And => return Err(Failure::Cwd),
        }
    }
    if words.is_empty() || words.len() > limits::EXECUTION_WORDS {
        return Err(Failure::Limit);
    }
    Ok((words, cwd))
}

fn assignment(word: &str) -> Option<(&str, &str)> {
    word.split_once('=').filter(|(key, _)| valid_name(key))
}

fn env_wrapper(
    words: &[Word],
    position: &mut usize,
    environment: &mut Environment,
) -> Result<(), Failure> {
    *position += 1;
    let mut options = true;
    while let Some(word) = words.get(*position) {
        match word.value.as_str() {
            "--" if options => {
                options = false;
                *position += 1;
            }
            "-i" | "--ignore-environment" if options => {
                environment.clear();
                *position += 1;
            }
            "-u" | "--unset" if options => {
                *position += 1;
                let name = words.get(*position).ok_or(Failure::Syntax)?;
                environment.set(&name.value, None, Origin::Wrapper)?;
                *position += 1;
            }
            word if options && word.starts_with("--unset=") => {
                environment.set(
                    word.strip_prefix("--unset=").ok_or(Failure::Syntax)?,
                    None,
                    Origin::Wrapper,
                )?;
                *position += 1;
            }
            word if word.starts_with('-') && options => return Err(Failure::Unsupported),
            word => {
                if let Some((key, value)) = assignment(word) {
                    environment.set(key, Some(value), Origin::Wrapper)?;
                    *position += 1;
                    options = false;
                } else {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn selected_tool<'a>(
    contract: &'a RustExecution,
    word: &str,
    environment: &Environment,
) -> Result<&'a str, Failure> {
    if !Path::new(word).is_absolute()
        && environment.get("PATH") != contract.environment().get("PATH").map(String::as_str)
    {
        return Err(Failure::Executable);
    }
    contract.tool(word).ok_or(Failure::Executable)
}

fn toolchain(contract: &RustExecution, word: &str) -> Result<(), Failure> {
    if word != contract.toolchain() && word != "1.94.0" {
        return Err(Failure::Toolchain);
    }
    Ok(())
}

/// Parse one fully consumed literal invocation without executing any program.
/// This classifies actions only; it never claims candidate completeness.
///
/// # Errors
/// Unsupported syntax, wrappers, tool selectors and actions fail conservatively.
pub fn parse(input: &ExecutionInput, contract: &RustExecution) -> Result<Invocation, Failure> {
    let (words, cwd) = literal_words(input)?;
    if input
        .shell()
        .is_some_and(|shell| !contract.accepts_shell(shell))
    {
        return Err(Failure::Executable);
    }
    let mut environment = Environment::inherited(contract.environment())?;
    for (key, value) in input.environment() {
        environment.set(key, value.as_deref(), Origin::Client)?;
    }
    let mut position = 0;
    while let Some(word) = words.get(position).filter(|word| word.assignment) {
        if let Some((key, value)) = assignment(&word.value) {
            environment.set(key, Some(value), Origin::Assignment)?;
            position += 1;
        } else {
            break;
        }
    }
    let mut wrappers = 0;
    let mut explicit_toolchain = false;
    let tool = loop {
        let word = words.get(position).ok_or(Failure::Syntax)?;
        let tool = selected_tool(contract, &word.value, &environment)?;
        if !matches!(tool, "env" | "rustup") {
            position += 1;
            break tool;
        }
        wrappers += 1;
        if wrappers > limits::EXECUTION_WRAPPERS {
            return Err(Failure::Limit);
        }
        if tool == "env" {
            env_wrapper(&words, &mut position, &mut environment)?;
        } else {
            if explicit_toolchain
                || words
                    .get(position + 1)
                    .is_none_or(|word| word.value != "run")
            {
                return Err(Failure::Unsupported);
            }
            toolchain(
                contract,
                &words.get(position + 2).ok_or(Failure::Syntax)?.value,
            )?;
            position += 3;
            explicit_toolchain = true;
            let selected = words.get(position).ok_or(Failure::Syntax)?;
            if !matches!(
                selected_tool(contract, &selected.value, &environment)?,
                "cargo" | "rustc" | "rustdoc"
            ) {
                return Err(Failure::Unsupported);
            }
        }
    };
    if tool == "cargo"
        && let Some(selector) = words
            .get(position)
            .and_then(|word| word.value.strip_prefix('+'))
    {
        if explicit_toolchain {
            return Err(Failure::Toolchain);
        }
        toolchain(contract, selector)?;
        position += 1;
        explicit_toolchain = true;
    }
    if environment
        .get("RUSTUP_TOOLCHAIN")
        .is_some_and(|selector| toolchain(contract, selector).is_err())
    {
        return Err(Failure::Toolchain);
    }
    let arguments: Vec<_> = words
        .into_iter()
        .skip(position)
        .map(|word| word.value)
        .collect();
    let action = classify(tool, &arguments)?;
    if let Action::Build(action) = action {
        validate_options(action, &arguments)?;
    }
    Ok(Invocation {
        action,
        tool: tool.to_owned(),
        arguments,
        cwd,
        environment,
        explicit_toolchain,
    })
}

fn cargo_action(word: &str) -> Option<BuildAction> {
    match word {
        "build" | "b" => Some(BuildAction::Build),
        "check" | "c" => Some(BuildAction::Check),
        "test" | "t" => Some(BuildAction::Test),
        "clippy" => Some(BuildAction::Clippy),
        "run" | "r" => Some(BuildAction::Run),
        "bench" => Some(BuildAction::Bench),
        "doc" | "d" => Some(BuildAction::Doc),
        "install" => Some(BuildAction::Install),
        _ => None,
    }
}

fn classify(tool: &str, arguments: &[String]) -> Result<Action, Failure> {
    if arguments.len() == 1
        && matches!(arguments[0].as_str(), "--version" | "-V" | "--help" | "-h")
        && matches!(tool, "cargo" | "rustc" | "rustdoc")
    {
        return Ok(Action::AuditedReadOnly);
    }
    match tool {
        "cargo" => {
            let mut position = 0;
            while let Some(word) = arguments.get(position) {
                if matches!(word.as_str(), "--config" | "--color") {
                    position += 2;
                } else if word.starts_with("--config=")
                    || word.starts_with("--color=")
                    || matches!(
                        word.as_str(),
                        "--offline"
                            | "--frozen"
                            | "--locked"
                            | "-q"
                            | "--quiet"
                            | "-v"
                            | "--verbose"
                    )
                {
                    position += 1;
                } else {
                    break;
                }
                if position > arguments.len() {
                    return Err(Failure::Syntax);
                }
            }
            let action = cargo_action(arguments.get(position).ok_or(Failure::Unsupported)?)
                .ok_or(Failure::Unsupported)?;
            if position == 0
                && arguments.len() == 2
                && matches!(
                    arguments[0].as_str(),
                    "build" | "check" | "test" | "run" | "bench" | "doc" | "install"
                )
                && matches!(arguments[1].as_str(), "--help" | "-h")
            {
                return Ok(Action::AuditedReadOnly);
            }
            Ok(Action::Build(action))
        }
        "rustc" if !arguments.is_empty() => Ok(Action::Build(BuildAction::Rustc)),
        "rustdoc" if !arguments.is_empty() => Ok(Action::Build(BuildAction::Rustdoc)),
        _ => Err(Failure::Unsupported),
    }
}

fn option_value<'a>(
    word: &'a str,
    words: &'a [String],
    position: &mut usize,
) -> Result<(&'a str, &'a str), Failure> {
    if let Some((key, value)) = word.split_once('=') {
        if value.is_empty() {
            return Err(Failure::Syntax);
        }
        Ok((key, value))
    } else {
        *position += 1;
        let value = words.get(*position).ok_or(Failure::Syntax)?;
        if value.is_empty() || value.starts_with('-') {
            return Err(Failure::Syntax);
        }
        Ok((word, value))
    }
}

fn cargo_option(
    action: BuildAction,
    word: &str,
    words: &[String],
    position: &mut usize,
) -> Result<(), Failure> {
    let flag = word.split_once('=').map_or(word, |(key, _)| key);
    let ordinary = matches!(
        flag,
        "--offline"
            | "--frozen"
            | "--locked"
            | "-q"
            | "--quiet"
            | "-v"
            | "--verbose"
            | "--all-features"
            | "--no-default-features"
            | "--keep-going"
            | "--future-incompat-report"
            | "--timings"
    );
    let selection = action != BuildAction::Install
        && matches!(
            flag,
            "--release"
                | "--workspace"
                | "--all"
                | "--lib"
                | "--bins"
                | "--examples"
                | "--tests"
                | "--benches"
                | "--all-targets"
        );
    let specific = match action {
        BuildAction::Test => matches!(flag, "--no-run" | "--no-fail-fast" | "--doc"),
        BuildAction::Bench => flag == "--no-run",
        BuildAction::Doc => matches!(flag, "--no-deps" | "--document-private-items"),
        BuildAction::Install => matches!(flag, "--force" | "--debug" | "--no-track"),
        _ => false,
    };
    if ordinary || selection || specific {
        if word != flag {
            return Err(Failure::Unsupported);
        }
        return Ok(());
    }
    let valued = matches!(
        flag,
        "--config"
            | "--color"
            | "--target-dir"
            | "--target"
            | "--profile"
            | "--features"
            | "-F"
            | "--jobs"
            | "-j"
            | "--message-format"
            | "--bin"
            | "--example"
    ) || (action != BuildAction::Install
        && matches!(
            flag,
            "--manifest-path" | "--package" | "-p" | "--exclude" | "--test" | "--bench"
        ))
        || (action == BuildAction::Install
            && matches!(
                flag,
                "--path"
                    | "--root"
                    | "--git"
                    | "--branch"
                    | "--tag"
                    | "--rev"
                    | "--registry"
                    | "--version"
            ));
    if !valued {
        return Err(Failure::Unsupported);
    }
    let (_, value) = option_value(word, words, position)?;
    if flag == "--color" && !matches!(value, "always" | "auto" | "never") {
        return Err(Failure::Unsupported);
    }
    Ok(())
}

fn compiler_option(
    action: BuildAction,
    word: &str,
    words: &[String],
    position: &mut usize,
) -> Result<(), Failure> {
    let flag = word.split_once('=').map_or(word, |(key, _)| key);
    if matches!(
        word,
        "--test" | "--cap-lints=allow" | "--cap-lints=warn" | "--cap-lints=deny"
    ) && (word != "--test" || action == BuildAction::Rustc)
    {
        return Ok(());
    }
    if action == BuildAction::Rustdoc && word == "--document-private-items" {
        return Ok(());
    }
    if matches!(
        flag,
        "--crate-name"
            | "--crate-type"
            | "--edition"
            | "--cfg"
            | "--cap-lints"
            | "--color"
            | "--error-format"
    ) || (action == BuildAction::Rustc && matches!(flag, "--out-dir" | "--emit"))
        || (action == BuildAction::Rustdoc && matches!(flag, "-o" | "--output"))
    {
        let (_, value) = option_value(word, words, position)?;
        if flag == "--emit"
            && (value.contains('=')
                || value.split(',').any(|kind| {
                    !matches!(
                        kind,
                        "link"
                            | "metadata"
                            | "dep-info"
                            | "asm"
                            | "llvm-ir"
                            | "llvm-bc"
                            | "obj"
                            | "mir"
                    )
                }))
        {
            return Err(Failure::Unsupported);
        }
        return Ok(());
    }
    if matches!(word, "-C" | "-W" | "-A" | "-D" | "-F") {
        let (_, value) = option_value(word, words, position)?;
        if word == "-C"
            && !matches!(
                value.split_once('=').map_or(value, |(key, _)| key),
                "opt-level"
                    | "debuginfo"
                    | "codegen-units"
                    | "debug-assertions"
                    | "overflow-checks"
                    | "panic"
            )
        {
            return Err(Failure::Unsupported);
        }
        return Ok(());
    }
    Err(Failure::Unsupported)
}

/// Validate extra compiler flags without allowing an output destination override.
pub(super) fn validate_extra_flags(action: BuildAction, flags: &[String]) -> Result<(), Failure> {
    if flags.len() > limits::EXECUTION_WORDS {
        return Err(Failure::Limit);
    }
    let mut position = 0;
    while let Some(word) = flags.get(position) {
        let flag = word.split_once('=').map_or(word.as_str(), |(key, _)| key);
        if matches!(flag, "--out-dir" | "--emit" | "--output" | "-o") {
            return Err(Failure::Unsupported);
        }
        compiler_option(action, word, flags, &mut position)?;
        position += 1;
    }
    Ok(())
}

fn validate_options(action: BuildAction, arguments: &[String]) -> Result<(), Failure> {
    let compiler = matches!(action, BuildAction::Rustc | BuildAction::Rustdoc);
    let mut seen_action = compiler;
    let mut sources = 0;
    let mut position = 0;
    while let Some(word) = arguments.get(position) {
        if !seen_action && cargo_action(word) == Some(action) {
            seen_action = true;
        } else if word == "--" {
            if !seen_action
                || !matches!(
                    action,
                    BuildAction::Run | BuildAction::Test | BuildAction::Bench
                )
            {
                return Err(Failure::Unsupported);
            }
            break;
        } else if word.starts_with('-') {
            if compiler {
                compiler_option(action, word, arguments, &mut position)?;
            } else {
                cargo_option(action, word, arguments, &mut position)?;
            }
        } else if (compiler && !word.starts_with('@')) || action == BuildAction::Install {
            sources += 1;
        } else {
            return Err(Failure::Unsupported);
        }
        position += 1;
    }
    if compiler && sources != 1 {
        return Err(Failure::Unsupported);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{Quote, Token, escaped, lex};
    use crate::config::execution::literal_fixture;
    use crate::execution_input::ExecutionInput;
    use crate::limits;
    use crate::rust_build::environment::Origin;
    use crate::rust_build::{Action, BuildAction};

    #[test]
    fn all_named_actions_are_build_capable_and_unknown_options_deny() {
        let (contract, directory) = literal_fixture();
        for (name, action) in [
            ("build", BuildAction::Build),
            ("check", BuildAction::Check),
            ("test", BuildAction::Test),
            ("clippy", BuildAction::Clippy),
            ("run", BuildAction::Run),
            ("bench", BuildAction::Bench),
            ("doc", BuildAction::Doc),
            ("install", BuildAction::Install),
            ("b", BuildAction::Build),
            ("c", BuildAction::Check),
            ("t", BuildAction::Test),
            ("r", BuildAction::Run),
            ("d", BuildAction::Doc),
        ] {
            let input =
                ExecutionInput::from_value(&json!({"command":format!("cargo {name}")})).unwrap();
            assert_eq!(
                super::parse(&input, &contract).unwrap().action(),
                Action::Build(action)
            );
        }
        for source in [
            "cargo test -- --list",
            "cargo test --no-run",
            "cargo bench --no-run",
            "cargo bench -- --list",
            "rustc --test source.rs --out-dir output",
            "rustdoc source.rs -o output",
        ] {
            let input = ExecutionInput::from_value(&json!({"command":source})).unwrap();
            assert!(matches!(
                super::parse(&input, &contract).unwrap().action(),
                Action::Build(_)
            ));
        }
        for source in [
            "cargo --version",
            "rustc --version",
            "rustdoc --version",
            "cargo build --help",
            "cargo install --help",
        ] {
            let input = ExecutionInput::from_value(&json!({"command":source})).unwrap();
            assert_eq!(
                super::parse(&input, &contract).unwrap().action(),
                Action::AuditedReadOnly
            );
        }
        for source in [
            "cargo build --opaque-output secret",
            "cargo metadata",
            "cargo fetch",
            "cargo update",
            "cargo clean",
            "cargo fmt",
            "cargo unknown",
            "cargo rustc -- -o secret",
            "make",
            "just build",
            "ssh remote cargo build",
            "docker run cargo build",
            "sh -c 'cargo build'",
            "rustc @response",
            "rustc source.rs -o file",
            "rustc source.rs --emit=link=file",
            "rustc source.rs -C incremental=opaque",
            "cargo doc --open",
            "cargo build --artifact-dir opaque",
            "cargo b --help",
            "cargo c --help",
            "cargo t --help",
            "cargo r --help",
            "cargo d --help",
            "cargo clippy --help",
        ] {
            let input = ExecutionInput::from_value(&json!({"command":source})).unwrap();
            assert!(super::parse(&input, &contract).is_err(), "{source}");
        }
        for name in ["env", "cargo", "rustc", "rustdoc", "rustup"] {
            assert_eq!(
                std::fs::read(directory.path().join(name)).unwrap(),
                b"NONEXECUTION_SENTINEL"
            );
        }
        assert!(!directory.path().join("marker").exists());
    }

    #[test]
    fn environment_cwd_and_toolchain_forms_have_explicit_precedence() {
        let (contract, directory) = literal_fixture();
        let input = ExecutionInput::from_value(&json!({"command":"TMPDIR=assignment env -u TEMP TMPDIR=wrapper cargo +1.94.0 build", "env":{"TMPDIR":"client"}})).unwrap();
        let parsed = super::parse(&input, &contract).unwrap();
        assert_eq!(parsed.environment().get("TMPDIR"), Some("wrapper"));
        assert_eq!(parsed.environment().origin("TMPDIR"), Some(Origin::Wrapper));
        assert_eq!(parsed.environment().get("TEMP"), None);
        assert!(parsed.explicit_toolchain());
        let source = format!(
            "env -i TMPDIR=literal {}/cargo build",
            directory.path().display()
        );
        let parsed = super::parse(
            &ExecutionInput::from_value(&json!({"command":source})).unwrap(),
            &contract,
        )
        .unwrap();
        assert_eq!(parsed.environment().get("HOME"), None);
        assert_eq!(parsed.environment().get("TMPDIR"), Some("literal"));
        for source in [
            "rustup run 1.94.0 cargo build",
            "rustup run 1.94.0-x86_64-unknown-linux-gnu rustc source.rs --out-dir output",
            "cd -- 'space here' && cargo check",
        ] {
            let parsed = super::parse(
                &ExecutionInput::from_value(&json!({"command":source})).unwrap(),
                &contract,
            )
            .unwrap();
            assert!(matches!(parsed.action(), Action::Build(_)));
        }
        let parsed = super::parse(
            &ExecutionInput::from_value(&json!({"argv":["cargo","build"], "workdir":"nested"}))
                .unwrap(),
            &contract,
        )
        .unwrap();
        assert_eq!(parsed.cwd(), Some(Path::new("nested")));
        for source in [
            "cd && cargo build",
            "cd a && cargo build && true",
            "env TMPDIR=x -u TEMP cargo build",
            "rustup run --install 1.94.0 cargo build",
            "rustup run nightly cargo build",
            "cargo +nightly build",
            "RUSTUP_TOOLCHAIN=nightly cargo build",
            "env -i cargo build",
            "env -S 'cargo build'",
            "env --chdir=a cargo build",
            "env env env env env cargo build",
        ] {
            assert!(
                super::parse(
                    &ExecutionInput::from_value(&json!({"command":source})).unwrap(),
                    &contract
                )
                .is_err(),
                "{source}"
            );
        }
        let input =
            ExecutionInput::from_value(&json!({"command":"cd a && cargo build", "workdir":"a"}))
                .unwrap();
        assert!(super::parse(&input, &contract).is_err());
    }

    fn values(source: &str) -> Vec<String> {
        lex(source)
            .unwrap()
            .into_iter()
            .map(|token| match token {
                Token::Word(word) => word.value,
                Token::And => "&&".to_owned(),
            })
            .collect()
    }

    #[test]
    fn literal_quotes_and_escaping_do_not_expand() {
        assert_eq!(
            values("TMPDIR='space here' cargo build --target-dir \"path here\""),
            [
                "TMPDIR=space here",
                "cargo",
                "build",
                "--target-dir",
                "path here"
            ]
        );
        assert_eq!(
            values("cargo build '$HOME' \"\\$HOME\" abc\\ def '$(touch marker)'"),
            [
                "cargo",
                "build",
                "$HOME",
                "$HOME",
                "abc def",
                "$(touch marker)"
            ]
        );
        assert_eq!(
            values("cd -- 'space here'&&cargo check"),
            ["cd", "--", "space here", "&&", "cargo", "check"]
        );
        assert_eq!(
            values("env TMPDIR=\"a\\qb\" cargo build"),
            ["env", "TMPDIR=a\\qb", "cargo", "build"]
        );
        let mut chars = "x".chars().peekable();
        let mut value = String::new();
        escaped(&mut chars, &mut value, Quote::Double).unwrap();
        assert_eq!(value, "\\x");
    }

    #[test]
    fn scripts_expansions_and_overflow_never_accept_a_prefix() {
        for source in [
            "cargo build; touch marker",
            "cargo build | cat",
            "cargo build > marker",
            "cargo build &",
            "cargo $(touch marker)",
            "cargo \"$HOME\" build",
            "cargo `touch marker`",
            "cargo build *.rs",
            "cargo build <<EOF",
            "cargo build\necho marker",
            "# cargo build",
            "cargo 'unterminated",
            "cargo build\\",
            "cargo build {a,b}",
            "cargo ~/path",
            "cargo build\0",
        ] {
            assert!(lex(source).is_err(), "{source}");
        }
        assert!(lex(&format!("cargo {}", "x".repeat(limits::PATH_BYTES + 1))).is_err());
        assert!(lex(&"x ".repeat(limits::EXECUTION_WORDS + 2)).is_err());
    }
}
