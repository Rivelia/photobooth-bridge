//! Redirect game traffic to the local MITM proxy by rewriting the system
//! `hosts` file so the two API hostnames resolve to 127.0.0.1.
//!
//! Used on both Linux and Windows. No kernel modules or netfilter
//! capabilities (iptables) involved — the cost is locking the proxy port to
//! 443. The proxy's own outbound HTTPS calls to those same hostnames must
//! NOT consult the hosts file (or they'd loop); that bypass is handled by
//! [`crate::upstream::OverrideResolver`], fed the pre-resolved real IPs from
//! [`resolve_targets`].
//!
//! Entries we add are tagged with the [`MARKER`] comment so we can:
//! 1. strip stale ones on every `start` (crash recovery), and
//! 2. scrub them in `uninstall` regardless of whether the proxy is running.

use std::collections::HashMap;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use tokio::net::lookup_host;

use crate::platform;
use crate::rewrite::{CN_API, GLOBAL_API};

pub const TARGET_HOSTS: &[&str] = &[GLOBAL_API, CN_API];

/// Tag for our hosts-file lines, so `scrub` can find them after a crash.
const MARKER: &str = "# photobooth-bridge";

/// Resolve target API hosts via the real system resolver. Returned map drives
/// both the hosts-file rewrite and the proxy's own DNS overrides so its
/// upstream calls bypass the hosts file.
pub async fn resolve_targets() -> Result<HashMap<String, Vec<IpAddr>>> {
	// Fan out the lookups — getaddrinfo round-trips can each take 10s+ on a
	// slow resolver, no reason to serialize them.
	let handles: Vec<_> = TARGET_HOSTS
		.iter()
		.map(|host| tokio::spawn(resolve_one(host)))
		.collect();
	let mut out: HashMap<String, Vec<IpAddr>> = HashMap::new();
	for handle in handles {
		let (host, ips) = handle.await.context("resolver task panicked")??;
		out.insert(host, ips);
	}
	Ok(out)
}

async fn resolve_one(host: &'static str) -> Result<(String, Vec<IpAddr>)> {
	let resolved = lookup_host((host, 443))
		.await
		.with_context(|| format!("resolving {host}"))?;
	let mut ips: Vec<IpAddr> = Vec::new();
	for addr in resolved {
		let ip = addr.ip();
		if !ips.contains(&ip) {
			tracing::info!(host, %ip, "target resolved");
			ips.push(ip);
		}
	}
	if ips.is_empty() {
		bail!("no IPs resolved for {host}");
	}
	Ok((host.to_string(), ips))
}

#[cfg(unix)]
fn hosts_path() -> PathBuf {
	PathBuf::from("/etc/hosts")
}

#[cfg(windows)]
fn hosts_path() -> PathBuf {
	let root = std::env::var_os("SystemRoot")
		.unwrap_or_else(|| std::ffi::OsString::from("C:\\Windows"));
	PathBuf::from(root).join("System32\\drivers\\etc\\hosts")
}

#[cfg(unix)]
const LINE_SEP: &str = "\n";
#[cfg(windows)]
const LINE_SEP: &str = "\r\n";

fn read_hosts() -> Result<String> {
	let path = hosts_path();
	fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))
}

fn write_hosts(contents: &str) -> Result<()> {
	let path = hosts_path();
	fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))
}

// Trailing separator on non-empty output so subsequent appends start on a fresh line.
fn strip_our_lines(contents: &str) -> String {
	let kept: Vec<&str> = contents
		.lines()
		.filter(|line| !line.contains(MARKER))
		.collect();
	let mut out = kept.join(LINE_SEP);
	if !out.is_empty() {
		out.push_str(LINE_SEP);
	}
	out
}

/// Caller is responsible for ensuring the proxy is bound to port 443 — hosts
/// entries only forge the destination IP, not the port.
pub fn install() -> Result<()> {
	platform::require_admin()?;
	let original = read_hosts()?;
	// Strip any leftover lines from a previous crashed run before appending.
	let mut next = strip_our_lines(&original);
	for host in TARGET_HOSTS {
		next.push_str(&format!("127.0.0.1\t{host}  {MARKER}{LINE_SEP}"));
		tracing::info!(host, "added hosts file entry");
	}
	write_hosts(&next)?;
	Ok(())
}

/// Strip every line we tagged from the hosts file. Safe to call when nothing
/// is installed (becomes a read-only no-op).
pub fn scrub() -> Result<()> {
	let original = match read_hosts() {
		Ok(s) => s,
		Err(err) => {
			tracing::warn!(?err, "could not read hosts file during cleanup");
			return Ok(());
		}
	};
	if !original.contains(MARKER) {
		return Ok(());
	}
	platform::require_admin()?;
	let cleaned = strip_our_lines(&original);
	write_hosts(&cleaned)?;
	Ok(())
}
