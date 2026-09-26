//! Administrative inspection CLI for the unwired memory-hooks foundation.

use std::path::Path;

use memory_hooks::TrustedHooksConfig;

fn run() -> memory_hooks::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let command = args
        .next()
        .ok_or(memory_hooks::Error::ConfigInvalid("command"))?;
    if command == "--version" {
        if args.next().is_some() {
            return Err(memory_hooks::Error::ConfigInvalid("arguments"));
        }
        println!(
            "{} {}",
            env!("CARGO_PKG_VERSION"),
            env!("MEMORY_HOOKS_GIT_HASH")
        );
        return Ok(());
    }
    let mut config = None;
    while let Some(option) = args.next() {
        let value = args
            .next()
            .ok_or(memory_hooks::Error::ConfigInvalid("option value"))?;
        if option == "--config" && config.is_none() {
            config = Some(value);
        } else {
            return Err(memory_hooks::Error::ConfigInvalid("option"));
        }
    }
    let config = config.ok_or(memory_hooks::Error::ConfigInvalid("--config"))?;
    let _trusted = TrustedHooksConfig::load(Path::new(&config))?;
    if command == "check-config" {
        println!("{{\"version\":1,\"status\":\"valid\"}}");
    } else {
        return Err(memory_hooks::Error::ConfigInvalid("command"));
    }
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
