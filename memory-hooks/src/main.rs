//! Administrative inspection and installation control CLI for managed hooks.

use std::ffi::OsStr;
use std::path::Path;

use memory_hooks::TrustedHooksConfig;
#[cfg(target_os = "linux")]
use memory_hooks::{Installation, config::ClientAdapter};
#[cfg(target_os = "linux")]
use uuid::Uuid;

#[cfg(target_os = "linux")]
fn admin(
    command: &OsStr,
    config: Option<&OsStr>,
    installation: Option<&OsStr>,
    installation_id: Option<&OsStr>,
    client: Option<&OsStr>,
) -> memory_hooks::Result<()> {
    let path = Path::new(installation.ok_or(memory_hooks::Error::ConfigInvalid("--installation"))?);
    let adapter = client
        .and_then(OsStr::to_str)
        .and_then(ClientAdapter::from_name)
        .ok_or(memory_hooks::Error::ConfigInvalid("--client"))?;
    if command == "init-installation" {
        if config.is_some() || installation_id.is_some() {
            return Err(memory_hooks::Error::ConfigInvalid("arguments"));
        }
        let id = Installation::initialize(path, adapter)?;
        println!(
            "{}",
            serde_json::json!({"version": 1, "installation_id": id})
        );
        return Ok(());
    }
    let id = installation_id
        .and_then(OsStr::to_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or(memory_hooks::Error::ConfigInvalid("--installation-id"))?;
    let anchor = Installation::open(path, id, adapter)?;
    let epoch = if command == "activate-installation" {
        anchor.activate(Path::new(
            config.ok_or(memory_hooks::Error::ConfigInvalid("--config"))?,
        ))?
    } else if command == "retire-installation" && config.is_none() {
        anchor.retire()?
    } else if command == "repair-installation" && config.is_none() {
        anchor.repair_incomplete()?
    } else if command == "installation-status" && config.is_none() {
        let (lifecycle, epoch) = anchor.status()?;
        println!(
            "{}",
            serde_json::json!({"version": 1, "lifecycle": lifecycle, "epoch": epoch})
        );
        return Ok(());
    } else {
        return Err(memory_hooks::Error::ConfigInvalid("command or arguments"));
    };
    println!("{}", serde_json::json!({"version": 1, "epoch": epoch}));
    Ok(())
}

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
    let mut cwd = None;
    let mut installation = None;
    let mut installation_id = None;
    let mut client = None;
    while let Some(option) = args.next() {
        let value = args
            .next()
            .ok_or(memory_hooks::Error::ConfigInvalid("option value"))?;
        if option == "--config" && config.is_none() {
            config = Some(value);
        } else if option == "--cwd" && cwd.is_none() {
            cwd = Some(value);
        } else if option == "--installation" && installation.is_none() {
            installation = Some(value);
        } else if option == "--installation-id" && installation_id.is_none() {
            installation_id = Some(value);
        } else if option == "--client" && client.is_none() {
            client = Some(value);
        } else {
            return Err(memory_hooks::Error::ConfigInvalid("option"));
        }
    }
    if command != "check-config" && command != "resolve" {
        if cwd.is_some() {
            return Err(memory_hooks::Error::ConfigInvalid("--cwd"));
        }
        #[cfg(target_os = "linux")]
        return admin(
            &command,
            config.as_deref(),
            installation.as_deref(),
            installation_id.as_deref(),
            client.as_deref(),
        );
        #[cfg(not(target_os = "linux"))]
        return Err(memory_hooks::Error::UnsupportedPlatform);
    }
    if installation.is_some() || installation_id.is_some() || client.is_some() {
        return Err(memory_hooks::Error::ConfigInvalid("arguments"));
    }
    let config = config.ok_or(memory_hooks::Error::ConfigInvalid("--config"))?;
    let trusted = TrustedHooksConfig::load(Path::new(&config))?;
    if command == "check-config" && cwd.is_none() {
        println!("{{\"version\":1,\"status\":\"valid\"}}");
    } else if command == "resolve" {
        let cwd = cwd.ok_or(memory_hooks::Error::ConfigInvalid("--cwd"))?;
        let binding = trusted.resolve(Path::new(&cwd))?;
        println!("{}", binding.public_json()?);
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
