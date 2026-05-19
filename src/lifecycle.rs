//! Start/stop lifecycle for the MITM proxy, reusable from both the CLI
//! (`photobooth-bridge start`) and the GUI.
//!
//! [`start`] performs the privileged setup (resolve targets → bind 443 →
//! install hosts redirect → spawn proxy task) and returns a [`ProxyHandle`].
//! Dropping the handle does NOT stop the proxy — call [`ProxyHandle::shutdown`]
//! to send a shutdown signal, await the proxy task, and scrub the hosts file.
//!
//! The hosts-file scrub is the reason a structured shutdown exists at all: a
//! hard-kill leaves the two `127.0.0.1` lines in `/etc/hosts` (or
//! `…\drivers\etc\hosts` on Windows) pointing at a dead proxy, breaking
//! anything else on the machine that talks to those API hosts until the next
//! `start` (or `uninstall`) re-runs the scrub.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::oneshot;

use crate::ca::CertificateAuthority;
use crate::{platform, proxy, redirect, upstream};

pub const DEFAULT_PORT: u16 = 443;

pub struct ProxyHandle {
	shutdown_tx: oneshot::Sender<()>,
	task: tokio::task::JoinHandle<()>,
	no_redirect: bool,
}

impl ProxyHandle {
	/// Signal the proxy to stop, await its task, and scrub the hosts-file
	/// redirect entries we installed. Consumes the handle.
	pub async fn shutdown(self) -> Result<()> {
		let _ = self.shutdown_tx.send(());
		let _ = self.task.await;
		if !self.no_redirect {
			redirect::scrub().context("scrubbing hosts file on shutdown")?;
		}
		Ok(())
	}
}

/// Resolve target hosts, bind the listener, install the hosts redirect, and
/// spawn the proxy task. Returns once everything is running; the caller drives
/// shutdown via [`ProxyHandle::shutdown`].
pub async fn start(port: u16, no_redirect: bool) -> Result<ProxyHandle> {
	if !no_redirect && port != DEFAULT_PORT {
		anyhow::bail!(
			"the proxy must listen on port {DEFAULT_PORT} (got {port}); hosts-file \
			 redirection cannot remap the port — pass --port {DEFAULT_PORT} or --no-redirect"
		);
	}

	let ca = Arc::new(CertificateAuthority::load_or_create()?);

	let resolved = if no_redirect {
		std::collections::HashMap::new()
	} else {
		// Scrub stale entries from a previous run BEFORE resolving — otherwise
		// getaddrinfo reads our own 127.0.0.1 entry and the resolved IPs that
		// drive the upstream override become loopback. The proxy then loops
		// back to itself for outbound calls and rustls (rightly) rejects its
		// own cert with UnknownIssuer.
		redirect::scrub().context("scrubbing stale hosts entries")?;
		redirect::resolve_targets()
			.await
			.context("resolving target API hostnames")?
	};

	let bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
	// Bind BEFORE touching the hosts file: if 443 is taken, we'd otherwise
	// leave the system hosts file polluted on failure.
	let listener = tokio::net::TcpListener::bind(bind).await.map_err(|err| {
		if err.kind() == std::io::ErrorKind::PermissionDenied && port < 1024 {
			anyhow::anyhow!("binding {bind}: {err}\n\n{}", platform::privileged_port_hint())
		} else {
			anyhow::Error::new(err).context(format!("binding {bind}"))
		}
	})?;
	tracing::info!(addr = %bind, "MITM proxy listening");

	if !no_redirect {
		redirect::install().context("installing hosts-file redirect")?;
	}

	let upstream = Arc::new(upstream::Upstream::new(resolved));
	let (shutdown_tx, shutdown_rx) = oneshot::channel();

	let task = tokio::spawn({
		let ca = Arc::clone(&ca);
		let upstream = Arc::clone(&upstream);
		async move {
			tokio::select! {
				res = proxy::run(ca, upstream, listener) => {
					if let Err(err) = res {
						tracing::error!(?err, "proxy task failed");
					}
				}
				_ = shutdown_rx => {
					tracing::info!("proxy shutdown signal received");
				}
			}
		}
	});

	Ok(ProxyHandle { shutdown_tx, task, no_redirect })
}
