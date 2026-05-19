// Release Windows builds use the `windows` subsystem so double-clicking the
// .exe doesn't briefly flash a console window. `platform::attach_parent_console`
// re-attaches stdout/stderr when invoked from a real shell so the CLI
// subcommands keep working from PowerShell/cmd. Debug builds stay on the
// default console subsystem to make `cargo run` ergonomic.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod ca;
mod gui;
mod install;
mod lifecycle;
mod paths;
mod platform;
mod proxy;
mod redirect;
mod rewrite;
mod trust_store;
mod upstream;
#[cfg(unix)]
mod wine;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "photobooth-bridge", version, about = "Cross-region Naraka Photo Booth bridge (transparent HTTPS MITM)")]
struct Cli {
	#[command(subcommand)]
	command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
	/// Generate the root CA (if missing) and install it where the Naraka client
	/// can see it. On Linux that's the Steam Proton prefix (Naraka runs under
	/// Wine, whose SChannel cert store is per-prefix and ignores
	/// `/etc/ca-certificates`). On Windows that's the machine `Root` store.
	InstallCa {
		/// [Linux only] Steam app id of the game whose Proton prefix should
		/// receive the CA. Defaults to Naraka: Bladepoint (1203220).
		#[arg(long, default_value = install::NARAKA_STEAM_APP_ID)]
		steam_app_id: String,
		/// [Linux only] Skip the Proton prefix install (just generate the local
		/// CA file). Useful for the `curl --resolve` testing flow.
		#[arg(long)]
		no_wine: bool,
	},
	/// Reverse `install-ca`: clean the Proton prefix (Linux) or remove the CA
	/// from the Windows Root store (Windows), scrub any leftover hosts-file
	/// redirect entries, and wipe the local CA files.
	Uninstall {
		/// [Linux only] Steam app id of the game whose Proton prefix should
		/// be cleaned. Defaults to Naraka: Bladepoint (1203220).
		#[arg(long, default_value = install::NARAKA_STEAM_APP_ID)]
		steam_app_id: String,
		/// [Linux only] Skip Proton prefix uninstall.
		#[arg(long)]
		no_wine: bool,
		/// Skip wiping CA files from the local data dir.
		#[arg(long)]
		keep_files: bool,
	},
	/// Show paths and CA status, no network changes.
	Status,
	/// Start the MITM proxy and install hosts-file redirection.
	///
	/// Requires elevated privileges to bind 443 and edit the system hosts
	/// file: run with `sudo` (Linux) or from an elevated shell (Windows).
	/// On Linux you can avoid `sudo` by granting the binary the bind
	/// capability once: `sudo setcap cap_net_bind_service=+ep <binary>`
	/// (re-apply after each rebuild). Hosts-file edits still need root
	/// unless `--no-redirect` is passed.
	Start {
		/// Local port the proxy listens on. Must be 443 unless `--no-redirect`
		/// is also passed (hosts-file redirection can't remap ports).
		#[arg(long, default_value_t = lifecycle::DEFAULT_PORT)]
		port: u16,
		/// Skip hosts-file setup (useful for testing the proxy directly with
		/// `curl --resolve`, or running on a non-443 port).
		#[arg(long)]
		no_redirect: bool,
	},
	/// Explicitly launch the GUI. Same as running with no subcommand.
	Gui,
}

fn init_tracing() {
	let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
	tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
}

fn main() -> Result<()> {
	// Windows release builds run under the `windows` subsystem and need this
	// hook to forward output back to a parent PowerShell/cmd. No-op elsewhere.
	platform::attach_parent_console();

	let cli = Cli::parse();
	let is_gui = matches!(cli.command, None | Some(Cmd::Gui));

	if !is_gui {
		init_tracing();
	}

	rustls::crypto::ring::default_provider().install_default().ok();

	match cli.command {
		None | Some(Cmd::Gui) => gui::run(),
		Some(Cmd::InstallCa { steam_app_id, no_wine }) => cmd_install_ca(&steam_app_id, no_wine),
		Some(Cmd::Uninstall { steam_app_id, no_wine, keep_files }) => {
			cmd_uninstall(&steam_app_id, no_wine, keep_files)
		}
		Some(Cmd::Status) => cmd_status(),
		Some(Cmd::Start { port, no_redirect }) => run_async(cmd_start(port, no_redirect)),
	}
}

/// Build a multi-threaded tokio runtime for the async CLI subcommands.
/// The GUI path manages its own runtime on a background thread.
fn run_async<F: std::future::Future<Output = Result<()>>>(fut: F) -> Result<()> {
	let rt = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.context("building tokio runtime")?;
	rt.block_on(fut)
}

fn cmd_install_ca(steam_app_id: &str, no_wine: bool) -> Result<()> {
	install::install_ca(steam_app_id, no_wine)?;
	#[cfg(windows)]
	println!("Root CA installed into Windows Root store.");
	println!("CA path: {}", paths::ca_cert_path()?.display());
	#[cfg(unix)]
	if !no_wine {
		println!("Root CA installed into Proton prefix.");
	}
	Ok(())
}

fn cmd_uninstall(steam_app_id: &str, no_wine: bool, keep_files: bool) -> Result<()> {
	let report = install::uninstall(steam_app_id, no_wine, keep_files)?;
	for ok in &report.successes {
		println!("{ok}.");
	}
	for (label, err) in &report.failures {
		println!("{label} failed: {err:#}");
	}
	if report.successes.is_empty() && report.failures.is_empty() {
		println!("Nothing to remove.");
	}
	Ok(())
}

fn cmd_status() -> Result<()> {
	let cert = paths::ca_cert_path()?;
	let key = paths::ca_key_path()?;
	println!("data dir : {}", paths::data_dir()?.display());
	println!("ca cert  : {}", cert.display());
	println!("ca key   : {}", key.display());
	println!("ca exists: {}", cert.exists() && key.exists());
	Ok(())
}

#[cfg(windows)]
fn warn_if_ca_not_in_trust_store() {
	if !trust_store::ca_is_installed_in_system_store() {
		tracing::warn!(
			"root CA is NOT installed in the Windows Root store. Run \
			 `photobooth-bridge install-ca` (as Administrator) once, then \
			 restart. Without this step the game's TLS handshake to the \
			 proxy will fail."
		);
	}
}

#[cfg(unix)]
fn warn_if_ca_not_in_trust_store() {
	// No reliable Linux check: the only store that matters is the Proton
	// prefix's per-process registry, and probing it would require running
	// certutil.exe through Proton at startup — too expensive for a hint.
	// `install-ca` is the manual gate.
}

async fn cmd_start(port: u16, no_redirect: bool) -> Result<()> {
	warn_if_ca_not_in_trust_store();

	let handle = lifecycle::start(port, no_redirect).await?;
	platform::wait_for_shutdown().await;
	tracing::info!("shutdown signal received");

	if !no_redirect {
		println!("Removing hosts-file redirect entries...");
	}
	match handle.shutdown().await {
		Ok(()) => {
			if !no_redirect {
				println!("Hosts file cleaned. Safe to exit.");
			}
		}
		Err(err) => {
			tracing::warn!(?err, "shutdown cleanup failed");
			println!("Shutdown cleanup failed: {err:#}");
		}
	}
	Ok(())
}
