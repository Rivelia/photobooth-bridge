//! Windows-only system trust-store integration via `certutil.exe` against the
//! machine `Root` store.
//!
//! No Linux equivalent: Naraka is a Windows game and runs under Proton/Wine on
//! Linux, whose SChannel-backed cert store is in the prefix's registry and
//! ignores `/etc/ca-certificates`. The Proton prefix install lives in
//! [`crate::wine`].

#[cfg(windows)]
pub use windows::*;

#[cfg(windows)]
mod windows {
	use std::process::Command;

	use anyhow::{Context, Result, bail};

	use crate::ca::CA_COMMON_NAME;
	use crate::paths;
	use crate::platform;

	pub fn ca_is_installed_in_system_store() -> bool {
		Command::new("certutil")
			.args(["-store", "Root", CA_COMMON_NAME])
			.output()
			.map(|o| o.status.success())
			.unwrap_or(false)
	}

	pub fn install_ca() -> Result<()> {
		platform::require_admin()?;
		let cert_path = paths::ca_cert_path()?;
		let output = Command::new("certutil")
			.args(["-addstore", "-f", "Root"])
			.arg(&cert_path)
			.output()
			.context("invoking certutil -addstore")?;
		if !output.status.success() {
			let stderr = String::from_utf8_lossy(&output.stderr);
			let stdout = String::from_utf8_lossy(&output.stdout);
			bail!(
				"certutil -addstore -f Root {} failed: {}{}",
				cert_path.display(),
				stdout.trim(),
				stderr.trim()
			);
		}
		tracing::info!("installed root CA into Windows Root store");
		Ok(())
	}

	pub fn uninstall_ca() -> Result<()> {
		platform::require_admin()?;
		let output = Command::new("certutil")
			.args(["-delstore", "Root", CA_COMMON_NAME])
			.output()
			.context("invoking certutil -delstore")?;
		if !output.status.success() {
			let stderr = String::from_utf8_lossy(&output.stderr);
			let stdout = String::from_utf8_lossy(&output.stdout);
			let combined = format!("{stdout}{stderr}");
			// Treat "not found" as success — uninstall is idempotent.
			let looks_like_missing =
				combined.contains("CRYPT_E_NOT_FOUND") || combined.contains("0x80092004");
			if !looks_like_missing {
				bail!(
					"certutil -delstore Root \"{CA_COMMON_NAME}\" failed: {}",
					combined.trim()
				);
			}
		}
		tracing::info!("removed root CA from Windows Root store");
		Ok(())
	}
}
