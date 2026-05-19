use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

#[derive(Debug)]
pub struct ProtonPrefix {
	/// `$STEAM_COMPAT_CLIENT_INSTALL_PATH` — the Steam install root.
	pub steam_root: PathBuf,
	/// `$STEAM_COMPAT_DATA_PATH` — `steamapps/compatdata/<appid>/`.
	pub compat_data: PathBuf,
	/// The .../pfx/ directory inside compat_data.
	pub pfx: PathBuf,
	/// `proton` script for whichever Proton version we picked.
	pub proton: PathBuf,
}

fn candidate_steam_roots(home: &Path) -> Vec<PathBuf> {
	vec![
		home.join(".steam/steam"),
		home.join(".local/share/Steam"),
		home.join(".steam/debian-installation"),
	]
}

fn find_steam_root(home: &Path) -> Result<PathBuf> {
	for candidate in candidate_steam_roots(home) {
		if candidate.join("steamapps").is_dir() {
			return Ok(candidate);
		}
	}
	bail!("no Steam install found under ~/.steam/steam, ~/.local/share/Steam, or ~/.steam/debian-installation")
}

/// Prefer Experimental > GE-Proton* > the highest-version Proton N.M.
fn rank_proton_dir(name: &str) -> (u8, i64, i64) {
	let lower = name.to_lowercase();
	if lower.contains("experimental") {
		return (3, 0, 0);
	}
	if let Some(rest) = lower.strip_prefix("ge-proton") {
		let (major, minor) = parse_major_minor(rest, '-');
		return (2, major, minor);
	}
	if let Some(rest) = lower.strip_prefix("proton ") {
		// Strip suffixes like " (Beta)" so "Proton 9.0 (Beta)" parses as (9, 0).
		let head_end = rest
			.find(|c: char| !c.is_ascii_digit() && c != '.')
			.unwrap_or(rest.len());
		let (major, minor) = parse_major_minor(&rest[..head_end], '.');
		return (1, major, minor);
	}
	(0, 0, 0)
}

fn parse_major_minor(s: &str, sep: char) -> (i64, i64) {
	let mut parts = s.split(sep);
	let major = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
	let minor = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
	(major, minor)
}

fn find_proton(steam_root: &Path) -> Result<PathBuf> {
	let common = steam_root.join("steamapps/common");
	let mut candidates: Vec<(PathBuf, String)> = Vec::new();
	for entry in std::fs::read_dir(&common)
		.with_context(|| format!("listing {}", common.display()))?
	{
		let entry = entry?;
		let name = entry.file_name().to_string_lossy().to_string();
		let proton_script = entry.path().join("proton");
		let lower = name.to_lowercase();
		if proton_script.is_file() && (lower.starts_with("proton") || lower.starts_with("ge-proton")) {
			candidates.push((proton_script, name));
		}
	}
	candidates
		.into_iter()
		.max_by_key(|c| rank_proton_dir(&c.1))
		.map(|c| c.0)
		.with_context(|| format!("no Proton install found under {}", common.display()))
}

pub fn locate(steam_app_id: &str) -> Result<ProtonPrefix> {
	// When invoked via sudo, $HOME points to /root/. Resolve the real user's
	// home — Steam lives there, not under /root.
	let real_home = crate::paths::real_user_home()?;

	let steam_root = find_steam_root(&real_home)?;
	let compat_data = steam_root.join("steamapps/compatdata").join(steam_app_id);
	if !compat_data.is_dir() {
		bail!(
			"no Proton prefix for app id {steam_app_id} at {} \
			 (game has never been launched, or wrong app id)",
			compat_data.display()
		);
	}
	let pfx = compat_data.join("pfx");
	if !pfx.is_dir() {
		bail!("prefix dir missing at {}", pfx.display());
	}
	let proton = find_proton(&steam_root)?;
	Ok(ProtonPrefix { steam_root, compat_data, pfx, proton })
}

/// Wine prefix registry key under which Windows-style root certs live. The
/// per-cert subkey is the cert's uppercase-hex SHA1 thumbprint.
///
/// We write only into `HKEY_CURRENT_USER` — this is where `certutil -addstore
/// Root` running unprivileged would land on real Windows, and what Wine's
/// `CertOpenSystemStoreW(NULL, "Root")` returns. Games that link their own
/// statically-bundled TLS library (OpenSSL, mbedTLS) ignore this trust source
/// entirely; for those, there's no install-side fix from this tool.
const HKCU_ROOT_CERTS: &str =
	r"HKEY_CURRENT_USER\Software\Microsoft\SystemCertificates\Root\Certificates";

pub fn install_ca(prefix: &ProtonPrefix, cert_der: &[u8]) -> Result<()> {
	// Earlier versions shelled out to `certutil.exe -addstore -f Root` here.
	// On Proton-Experimental that command returns exit 0 but silently fails to
	// persist the cert (`HKCU\...\Root\Certificates` parent key is created,
	// the per-fingerprint child key is not). We bypass certutil entirely:
	// build a .reg file with the cert in Wine's serialized property-blob
	// format and apply it with `regedit.exe /S`.
	let sha = sha1(cert_der);
	let current = hex_upper(&sha);
	// Pre-existing photobooth-bridge entries (e.g. from older builds whose
	// random serial gave each install a fresh fingerprint) are stripped in
	// the same .reg apply, so the prefix only carries the one current cert.
	let stale: Vec<String> = find_our_fingerprints(&prefix.pfx)
		.into_iter()
		.filter(|s| s != &current)
		.collect();
	if !stale.is_empty() {
		tracing::info!(?stale, "purging stale photobooth-bridge cert entries");
	}
	let reg = build_install_reg(&current, &serialize_cert_blob(cert_der, &sha), &stale);
	apply_reg_file(prefix, &reg).context("installing CA via regedit")
}

pub fn uninstall_ca(prefix: &ProtonPrefix, cert_der: &[u8]) -> Result<()> {
	// Delete every photobooth-bridge cert we can find — both the current
	// fingerprint (computed from the on-disk CA) and any stragglers.
	let mut shas = find_our_fingerprints(&prefix.pfx);
	let current = hex_upper(&sha1(cert_der));
	if !shas.contains(&current) {
		shas.push(current);
	}
	let reg = build_uninstall_reg(&shas);
	apply_reg_file(prefix, &reg).context("uninstalling CA via regedit")
}

fn build_install_reg(fingerprint: &str, cert_blob: &[u8], stale: &[String]) -> String {
	let mut out = build_uninstall_reg(stale);
	let blob_hex = format_reg_hex(cert_blob);
	let _ = write!(
		out,
		"[{HKCU_ROOT_CERTS}\\{fingerprint}]\r\n\"Blob\"=hex:{blob_hex}\r\n\r\n",
	);
	out
}

fn build_uninstall_reg(shas: &[String]) -> String {
	let mut out = String::from("REGEDIT4\r\n\r\n");
	for sha in shas {
		let _ = write!(out, "[-{HKCU_ROOT_CERTS}\\{sha}]\r\n\r\n");
	}
	out
}

/// Scan the prefix's `user.reg` for every Root\Certificates entry whose Blob
/// contains our CA's Common Name in cleartext (it appears in the cert's DN
/// bytes inside the serialized blob). Used to scrub leftover registrations
/// from older builds and to find what `uninstall` should clear.
fn find_our_fingerprints(pfx: &std::path::Path) -> Vec<String> {
	// The CN appears in the cert DN as ASCII bytes; the Blob is a hex dump
	// with `,` separators wrapped across `\`-continued lines. Strip everything
	// non-hex from each candidate's Blob and look for the lowercase hex of
	// our CN bytes.
	let needle: String = crate::ca::CA_COMMON_NAME
		.bytes()
		.map(|b| format!("{b:02x}"))
		.collect();
	let mut out = Vec::new();
	if let Ok(text) = fs::read_to_string(pfx.join("user.reg")) {
		scan_hive_for_our_certs(&text, &needle, &mut out);
	}
	out.sort();
	out.dedup();
	out
}

fn scan_hive_for_our_certs(hive_text: &str, needle: &str, out: &mut Vec<String>) {
	let key_prefix = r"[Software\\Microsoft\\SystemCertificates\\Root\\Certificates\\";

	let mut current_sha: Option<String> = None;
	let mut blob_hex = String::new();
	let mut in_blob = false;

	let flush = |sha: &mut Option<String>, blob: &mut String, hits: &mut Vec<String>| {
		if let Some(s) = sha.take()
			&& blob.contains(needle)
		{
			hits.push(s);
		}
		blob.clear();
	};

	for line in hive_text.lines() {
		if line.starts_with('[') {
			flush(&mut current_sha, &mut blob_hex, out);
			in_blob = false;
			if let Some(rest) = line.strip_prefix(key_prefix)
				&& let Some(end) = rest.find(']')
			{
				current_sha = Some(rest[..end].to_string());
			}
		} else if current_sha.is_some() {
			if let Some(rest) = line.strip_prefix("\"Blob\"=hex:") {
				in_blob = true;
				append_hex_bytes(&mut blob_hex, rest);
			} else if in_blob && (line.starts_with(' ') || line.starts_with('\t')) {
				append_hex_bytes(&mut blob_hex, line);
			} else if in_blob {
				in_blob = false;
			}
		}
	}
	flush(&mut current_sha, &mut blob_hex, out);
}

fn append_hex_bytes(out: &mut String, src: &str) {
	for c in src.chars() {
		if c.is_ascii_hexdigit() {
			out.push(c.to_ascii_lowercase());
		}
	}
}

fn sha1(data: &[u8]) -> [u8; 20] {
	let mut hasher = Sha1::new();
	hasher.update(data);
	hasher.finalize().into()
}

fn hex_upper(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// Wine/Windows on-disk cert serialization: a concatenation of
/// `{u32 prop_id, u32 encoding=1, u32 data_len, [data...]}` records. SChannel
/// looks up the cert blob in the registry and parses this byte stream; we
/// only need the two mandatory properties:
///   - 0x03 CERT_SHA1_HASH_PROP_ID (20-byte sha1 of the DER)
///   - 0x20 CERT_CERT_PROP_ID      (the DER itself)
fn serialize_cert_blob(cert_der: &[u8], sha: &[u8; 20]) -> Vec<u8> {
	let mut out = Vec::with_capacity(12 + sha.len() + 12 + cert_der.len());
	push_prop(&mut out, 0x03, sha);
	push_prop(&mut out, 0x20, cert_der);
	out
}

fn push_prop(out: &mut Vec<u8>, prop_id: u32, data: &[u8]) {
	out.extend_from_slice(&prop_id.to_le_bytes());
	out.extend_from_slice(&1u32.to_le_bytes()); // encoding flags, always 1 here
	out.extend_from_slice(&(data.len() as u32).to_le_bytes());
	out.extend_from_slice(data);
}

/// REG file binary value formatting: lowercase hex bytes, comma-separated,
/// chunked PER_LINE bytes per source line. A trailing `,\` continues the
/// value onto the next line; subsequent lines are indented two spaces.
fn format_reg_hex(bytes: &[u8]) -> String {
	const PER_LINE: usize = 25;
	let lines: Vec<String> = bytes
		.chunks(PER_LINE)
		.map(|chunk| chunk.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(","))
		.collect();
	// First line has no leading indent; continuation lines are indented.
	lines.join(",\\\r\n  ")
}

fn apply_reg_file(prefix: &ProtonPrefix, reg_contents: &str) -> Result<()> {
	let tmp_path = std::env::temp_dir()
		.join(format!("photobooth-bridge-{}.reg", std::process::id()));
	fs::write(&tmp_path, reg_contents)
		.with_context(|| format!("writing temp reg file {}", tmp_path.display()))?;
	// World-readable so the user we drop to (under sudo) can read it back.
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		let _ = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o644));
	}

	let wine_path = linux_path_to_wine(&tmp_path);
	tracing::info!(
		proton = %prefix.proton.display(),
		pfx = %prefix.pfx.display(),
		reg = %tmp_path.display(),
		"applying registry import via regedit.exe"
	);

	let mut cmd = build_proton_command(prefix);
	cmd.arg("run").arg("regedit.exe").arg("/S").arg(&wine_path);

	let result = cmd.output();
	let _ = fs::remove_file(&tmp_path);
	let output = result.with_context(|| format!("running {cmd:?}"))?;
	let stdout = String::from_utf8_lossy(&output.stdout);
	let stderr = String::from_utf8_lossy(&output.stderr);

	if !output.status.success() {
		bail!(
			"regedit.exe failed (exit {}). stdout:\n{stdout}\nstderr:\n{stderr}",
			output.status.code().unwrap_or(-1)
		);
	}

	tracing::info!("regedit.exe succeeded");
	if !stdout.trim().is_empty() {
		tracing::debug!("regedit stdout: {stdout}");
	}
	Ok(())
}

/// Map a Linux absolute path to its `Z:\…` Wine drive-letter equivalent.
fn linux_path_to_wine(path: &Path) -> String {
	let s = path.to_string_lossy();
	format!("Z:{s}").replace('/', "\\")
}

fn build_proton_command(prefix: &ProtonPrefix) -> Command {
	let env: [(&str, &Path); 2] = [
		("STEAM_COMPAT_CLIENT_INSTALL_PATH", &prefix.steam_root),
		("STEAM_COMPAT_DATA_PATH", &prefix.compat_data),
	];
	if let Some(sudo_user) = crate::paths::sudo_user_name()
		&& nix::unistd::geteuid().is_root()
	{
		// Drop privileges so Proton writes prefix files owned by the real user.
		// sudo scrubs the env by default, so pass STEAM_COMPAT_* as positional
		// VAR=val args (sudo's documented way of injecting env into the child)
		// rather than via `Command::env`, which would set them on sudo itself
		// and never reach proton.
		let mut c = Command::new("sudo");
		c.arg("-u").arg(&sudo_user);
		for (k, v) in env {
			c.arg(format!("{k}={}", v.display()));
		}
		c.arg("--").arg(&prefix.proton);
		c
	} else {
		let mut c = Command::new(&prefix.proton);
		for (k, v) in env {
			c.env(k, v);
		}
		c
	}
}
