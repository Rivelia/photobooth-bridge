use std::path::PathBuf;

use anyhow::{Context, Result};

const APPLICATION: &str = "photobooth-bridge";

/// `$SUDO_USER` if invoked via sudo by a non-root user. Returns None when
/// unset, empty, or "root" — the cases where chowning/locating-home would
/// either fail or be a no-op.
#[cfg(unix)]
pub fn sudo_user_name() -> Option<String> {
	let name = std::env::var("SUDO_USER").ok()?;
	(!name.is_empty() && name != "root").then_some(name)
}

/// `$SUDO_USER`'s home dir, if invoked via sudo by a non-root user.
#[cfg(unix)]
fn sudo_user_home() -> Option<PathBuf> {
	let sudo_user = sudo_user_name()?;
	nix::unistd::User::from_name(&sudo_user).ok().flatten().map(|u| u.dir)
}

/// Real-user home, even when invoked via sudo. Under sudo `$HOME` becomes
/// `/root/`, so we prefer `$SUDO_USER`'s passwd entry so the CA lands where
/// the regular user can reach it.
#[cfg(unix)]
pub fn real_user_home() -> Result<PathBuf> {
	if let Some(home) = sudo_user_home() {
		return Ok(home);
	}
	std::env::var_os("HOME")
		.map(PathBuf::from)
		.context("could not determine home directory ($HOME unset and no SUDO_USER)")
}

#[cfg(windows)]
pub fn real_user_home() -> Result<PathBuf> {
	std::env::var_os("USERPROFILE")
		.map(PathBuf::from)
		.context("USERPROFILE unset")
}

#[cfg(unix)]
pub fn data_dir() -> Result<PathBuf> {
	Ok(real_user_home()?.join(".local/share").join(APPLICATION))
}

#[cfg(windows)]
pub fn data_dir() -> Result<PathBuf> {
	// %LOCALAPPDATA% is the conventional per-machine, per-user state dir on
	// Windows. Fall back to %APPDATA% (Roaming) if Local isn't set.
	let base = std::env::var_os("LOCALAPPDATA")
		.or_else(|| std::env::var_os("APPDATA"))
		.map(PathBuf::from)
		.context("LOCALAPPDATA/APPDATA unset")?;
	Ok(base.join(APPLICATION))
}

/// `data_dir()` with `mkdir -p` and (on Linux under sudo) ownership fixup.
/// Call before writing; pure path lookups (status, uninstall) should stick
/// to [`data_dir`] so they don't materialize an empty dir as a side effect.
pub fn ensure_data_dir() -> Result<PathBuf> {
	let dir = data_dir()?;
	std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
	#[cfg(unix)]
	chown_to_sudo_user(&dir);
	Ok(dir)
}

/// If running as root via `sudo`, chown `path` to `$SUDO_USER`. No-op otherwise.
/// Logs a warning (doesn't propagate) on failure — chown is a best-effort UX
/// fixup, not a correctness invariant.
#[cfg(unix)]
pub fn chown_to_sudo_user(path: &std::path::Path) {
	if !nix::unistd::geteuid().is_root() {
		return;
	}
	let Some(sudo_user) = sudo_user_name() else { return };
	if let Err(err) = chown_to_user(path, &sudo_user) {
		tracing::warn!(?err, ?path, "failed to chown to SUDO_USER");
	}
}

#[cfg(unix)]
fn chown_to_user(path: &std::path::Path, user: &str) -> Result<()> {
	let user = nix::unistd::User::from_name(user)
		.context("getpwnam")?
		.with_context(|| format!("no passwd entry for {user}"))?;
	std::os::unix::fs::chown(path, Some(user.uid.as_raw()), Some(user.gid.as_raw()))
		.context("chown")
}

pub fn ca_cert_path() -> Result<PathBuf> {
	Ok(data_dir()?.join("ca.pem"))
}

pub fn ca_key_path() -> Result<PathBuf> {
	Ok(data_dir()?.join("ca-key.pem"))
}
