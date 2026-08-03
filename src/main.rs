#[cfg(not(any(target_os = "linux", target_os = "windows")))]
compile_error!("ifuse-rs only supports Linux and Windows");

use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::{ArgAction, Parser};
use ifuse_rs::{DeviceTarget, IfuseBuilder, MountSource, list_apps};

#[derive(Debug, Parser)]
#[command(
    name = "ifuse-rs",
    version,
    about = "Mount directories of an iOS device locally",
    long_about = None
)]
struct Cli {
    /// Mountpoint. A legacy ignored device argument may precede it.
    #[arg(value_name = "MOUNTPOINT", num_args = 0..=2)]
    positional: Vec<PathBuf>,

    /// Mount options, as a comma-separated list (Linux only).
    #[arg(short = 'o', value_name = "OPT[,OPT...]", action = ArgAction::Append)]
    mount_options: Vec<String>,

    /// Mount a specific device by UDID.
    #[arg(short = 'u', long, value_name = "UDID")]
    udid: Option<String>,

    /// Connect only to a network device exposed by usbmuxd.
    #[arg(short = 'n', long)]
    network: bool,

    /// Mount the AFC2 root filesystem (requires a jailbroken device).
    #[arg(long, conflicts_with_all = ["documents", "container"])]
    root: bool,

    /// Mount the Documents directory of an application.
    #[arg(long, value_name = "APPID", conflicts_with = "container")]
    documents: Option<String>,

    /// Mount the complete sandbox container of an application.
    #[arg(long, value_name = "APPID")]
    container: Option<String>,

    /// List installed applications that have file sharing enabled.
    #[arg(long)]
    list_apps: bool,

    /// Enable verbose communication logging.
    #[arg(short = 'd', long)]
    debug: bool,
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ERROR: failed to create runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(Cli::parse())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ERROR: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    init_logging(cli.debug);
    let target = if cli.network {
        DeviceTarget::UsbmuxdNetwork { udid: cli.udid }
    } else {
        DeviceTarget::Usb { udid: cli.udid }
    };

    if cli.list_apps {
        if !cli.positional.is_empty() {
            return Err("--list-apps does not accept a mountpoint".into());
        }
        println!("\"CFBundleIdentifier\",\"CFBundleVersion\",\"CFBundleDisplayName\"");
        for app in list_apps(target).await? {
            println!(
                "\"{}\",\"{}\",\"{}\"",
                csv(&app.bundle_id),
                csv(&app.version),
                csv(&app.display_name)
            );
        }
        return Ok(());
    }

    let mountpoint = cli
        .positional
        .last()
        .cloned()
        .ok_or("no mountpoint specified")?;
    let source = if cli.root {
        MountSource::Root
    } else if let Some(bundle_id) = cli.documents {
        MountSource::Documents { bundle_id }
    } else if let Some(bundle_id) = cli.container {
        MountSource::Container { bundle_id }
    } else {
        MountSource::Media
    };

    let handle = IfuseBuilder::with_target(target)
        .mount_point(mountpoint)
        .source(source)
        .mount_options(cli.mount_options)
        .mount()
        .await?;

    let signal = shutdown_signal();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            result = &mut signal => {
                result?;
                handle.unmount().await?;
                break;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                if !handle.is_mounted() {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = interrupt.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(target_os = "windows")]
async fn shutdown_signal() -> std::io::Result<()> {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_logoff, ctrl_shutdown};
    let mut ctrl_c = ctrl_c()?;
    let mut ctrl_break = ctrl_break()?;
    let mut ctrl_close = ctrl_close()?;
    let mut ctrl_logoff = ctrl_logoff()?;
    let mut ctrl_shutdown = ctrl_shutdown()?;
    tokio::select! {
        _ = ctrl_c.recv() => Ok(()),
        _ = ctrl_break.recv() => Ok(()),
        _ = ctrl_close.recv() => Ok(()),
        _ = ctrl_logoff.recv() => Ok(()),
        _ = ctrl_shutdown.recv() => Ok(()),
    }
}

fn init_logging(debug: bool) {
    let default = if debug { "debug" } else { "warn" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

fn csv(value: &str) -> String {
    value.replace('"', "\"\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_options_after_mountpoint() {
        let cli = Cli::try_parse_from(["ifuse-rs", "/mnt/phone", "-u", "abc"]).unwrap();
        assert_eq!(cli.positional, vec![PathBuf::from("/mnt/phone")]);
        assert_eq!(cli.udid.as_deref(), Some("abc"));
    }

    #[test]
    fn accepts_legacy_two_positionals() {
        let cli = Cli::try_parse_from(["ifuse-rs", "ignored", "/mnt/phone"]).unwrap();
        assert_eq!(cli.positional.last(), Some(&PathBuf::from("/mnt/phone")));
    }

    #[test]
    fn rejects_conflicting_app_modes() {
        assert!(
            Cli::try_parse_from([
                "ifuse-rs",
                "--documents",
                "app.one",
                "--container",
                "app.two",
                "/mnt"
            ])
            .is_err()
        );
    }

    #[test]
    fn escapes_csv_quotes() {
        assert_eq!(csv("a\"b"), "a\"\"b");
    }
}
