//! CA install / uninstall logic, shared between the CLI subcommands
//! (`install-ca`, `uninstall`) and the GUI buttons.
//!
//! Functions here do the work and return structured results; the caller
//! decides how to surface errors (println in the CLI, a label in the GUI).

use anyhow::Result;
#[cfg(unix)]
use anyhow::Context as _;

use crate::ca::CertificateAuthority;
use crate::{paths, redirect};
#[cfg(windows)]
use crate::trust_store;
#[cfg(unix)]
use crate::wine;

/// Default Steam app id for Naraka: Bladepoint. Drives the Wine/Proton
/// integration on Linux; ignored on Windows (the game is native there).
pub const NARAKA_STEAM_APP_ID: &str = "1203220";

/// Outcome of a `step` — the human-readable label is preserved so callers can
/// either print it or display it in a UI without re-deriving the description.
pub struct StepReport {
	pub successes: Vec<String>,
	pub failures: Vec<(String, anyhow::Error)>,
}

impl StepReport {
	pub fn new() -> Self {
		Self { successes: Vec::new(), failures: Vec::new() }
	}

	fn record(&mut self, label: impl Into<String>, result: Result<()>) {
		let label = label.into();
		match result {
			Ok(()) => self.successes.push(label),
			Err(err) => {
				tracing::warn!(?err, "{label} failed");
				self.failures.push((label, err));
			}
		}
	}
}

/// Generate the CA if missing, install it into the system trust store (Windows)
/// and/or the Naraka Proton prefix (Linux). Returns the CA so the caller can
/// reuse it; pass `no_wine = true` on Linux to skip the prefix install.
pub fn install_ca(steam_app_id: &str, no_wine: bool) -> Result<CertificateAuthority> {
	let _ = (steam_app_id, no_wine); // both may be unused on Windows
	let ca = CertificateAuthority::load_or_create()?;
	#[cfg(windows)]
	trust_store::install_ca()?;
	#[cfg(unix)]
	if !no_wine {
		let prefix = wine::locate(steam_app_id)
			.with_context(|| format!("locating Proton prefix for app {steam_app_id}"))?;
		wine::install_ca(&prefix, ca.cert_der()).context("installing CA into Proton prefix")?;
	}
	Ok(ca)
}

/// Reverse `install_ca`: remove from system trust store, clean Proton prefix,
/// scrub hosts file, optionally delete local CA files. Soft-fails per step —
/// the returned [`StepReport`] tells the caller which steps did or didn't run.
pub fn uninstall(steam_app_id: &str, no_wine: bool, keep_files: bool) -> Result<StepReport> {
	let _ = (steam_app_id, no_wine);
	let mut report = StepReport::new();

	#[cfg(windows)]
	report.record("Removed CA from Windows Root store", trust_store::uninstall_ca());

	#[cfg(unix)]
	if !no_wine {
		match CertificateAuthority::load_existing() {
			Ok(Some(ca)) => match wine::locate(steam_app_id) {
				Ok(prefix) => report.record(
					format!("Removed CA from Proton prefix at {}", prefix.pfx.display()),
					wine::uninstall_ca(&prefix, ca.cert_der()),
				),
				Err(err) => {
					tracing::info!(?err, "no Proton prefix to clean up");
				}
			},
			Ok(None) => {
				tracing::info!("no local CA on disk; skipping Wine uninstall");
			}
			Err(err) => {
				tracing::warn!(?err, "failed to load local CA; skipping Wine uninstall");
				report.failures.push(("Read local CA".into(), err));
			}
		}
	}

	report.record("Hosts file scrubbed", redirect::scrub());

	if !keep_files {
		let cert_path = paths::ca_cert_path()?;
		let key_path = paths::ca_key_path()?;
		for path in [&cert_path, &key_path] {
			match std::fs::remove_file(path) {
				Ok(()) => report.successes.push(format!("Deleted {}", path.display())),
				Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
				Err(err) => {
					tracing::warn!(?err, ?path, "failed to delete CA file");
					report.failures.push((
						format!("Delete {}", path.display()),
						anyhow::Error::new(err),
					));
				}
			}
		}
	}

	Ok(report)
}

/// True if both CA files exist on disk. Used by the GUI to decide between
/// "needs install" and "ready to start" on launch.
pub fn ca_files_exist() -> bool {
	let Ok(cert) = paths::ca_cert_path() else { return false };
	let Ok(key) = paths::ca_key_path() else { return false };
	cert.exists() && key.exists()
}
