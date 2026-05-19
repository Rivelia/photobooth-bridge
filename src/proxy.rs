use std::convert::Infallible;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rustls::server::Acceptor;
use tokio::net::TcpListener;
use tokio_rustls::LazyConfigAcceptor;

use crate::ca::CertificateAuthority;
use crate::rewrite;
use crate::upstream::Upstream;

pub async fn run(ca: Arc<CertificateAuthority>, upstream: Arc<Upstream>, listener: TcpListener) -> Result<()> {
	loop {
		let (stream, peer) = match listener.accept().await {
			Ok(pair) => pair,
			Err(err) => {
				tracing::warn!(?err, "accept failed");
				continue;
			}
		};
		stream.set_nodelay(true).ok();

		let ca = Arc::clone(&ca);
		let upstream = Arc::clone(&upstream);

		tracing::info!(%peer, "accepted connection");
		tokio::spawn(async move {
			if let Err(err) = handle_connection(stream, ca, upstream).await {
				tracing::warn!(%peer, ?err, "connection ended with error");
			}
		});
	}
}

async fn handle_connection(
	stream: tokio::net::TcpStream,
	ca: Arc<CertificateAuthority>,
	upstream: Arc<Upstream>,
) -> Result<()> {
	let acceptor = LazyConfigAcceptor::new(Acceptor::default(), stream);
	let handshake = acceptor.await.context("TLS ClientHello")?;
	let hello = handshake.client_hello();
	let sni = hello
		.server_name()
		.ok_or_else(|| anyhow!("client did not send SNI"))?
		.to_string();

	// JA3-ish fingerprint of the ClientHello: the cipher-suite list, signature
	// schemes, and ALPN values together identify the TLS library (OpenSSL
	// 1.1.0g, mbedTLS, SChannel, etc.) the caller is using. Helpful when the
	// game uses a statically-linked TLS library and we need to know which one.
	// Skip the allocations entirely when info-level isn't enabled.
	if tracing::enabled!(tracing::Level::INFO) {
		let ciphers: Vec<u16> = hello.cipher_suites().iter().map(|cs| u16::from(*cs)).collect();
		let sig_schemes: Vec<u16> =
			hello.signature_schemes().iter().map(|s| u16::from(*s)).collect();
		let alpn: Vec<String> = hello
			.alpn()
			.map(|iter| iter.map(|p| String::from_utf8_lossy(p).into_owned()).collect())
			.unwrap_or_default();
		tracing::info!(
			%sni,
			cipher_count = ciphers.len(),
			ciphers = ?ciphers,
			sig_schemes = ?sig_schemes,
			alpn = ?alpn,
			"client hello"
		);
	}

	let server_config = ca.server_config_for(&sni).context("issuing leaf cert")?;
	let tls_stream = handshake
		.into_stream(server_config)
		.await
		.context("TLS handshake")?;

	let sni = Arc::new(sni);

	let service = service_fn(move |req: Request<Incoming>| {
		let upstream = Arc::clone(&upstream);
		let sni = Arc::clone(&sni);
		async move { Ok::<_, Infallible>(serve_request(sni.as_str(), upstream, req).await) }
	});

	http1::Builder::new()
		.keep_alive(true)
		.serve_connection(TokioIo::new(tls_stream), service)
		.await
		.context("HTTP/1.1 serve")?;

	Ok(())
}

async fn serve_request(
	sni: &str,
	upstream: Arc<Upstream>,
	req: Request<Incoming>,
) -> Response<Full<Bytes>> {
	let path = req
		.uri()
		.path_and_query()
		.map_or_else(|| "/".into(), |p| p.as_str().to_string());
	tracing::debug!(%sni, method = %req.method(), %path, "request");

	let upstream_resp = match upstream.forward(sni, req).await {
		Ok(resp) => resp,
		Err(err) => {
			tracing::warn!(?err, %sni, %path, "upstream failed");
			return error_response(StatusCode::BAD_GATEWAY, format!("upstream failed: {err}"));
		}
	};

	let rewritten = match rewrite::maybe_rewrite_photo_booth(sni, &path, &upstream, upstream_resp).await {
		Ok(resp) => resp,
		Err(err) => {
			tracing::warn!(?err, "rewrite failed");
			return error_response(StatusCode::BAD_GATEWAY, format!("rewrite failed: {err}"));
		}
	};

	let (parts, body) = rewritten.into_parts();
	Response::from_parts(parts, Full::new(body))
}

fn error_response(status: StatusCode, message: String) -> Response<Full<Bytes>> {
	Response::builder()
		.status(status)
		.header("content-type", "text/plain; charset=utf-8")
		.body(Full::new(Bytes::from(message)))
		.expect("static response")
}
