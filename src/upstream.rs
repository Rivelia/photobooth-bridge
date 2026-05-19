use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::vec::IntoIter as VecIntoIter;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::header::{
	CONNECTION, HOST, HeaderName, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use hyper::http::uri::{Authority, PathAndQuery, Scheme};
use hyper::{Method, Request, Response, Uri};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::{GaiResolver, Name};
use hyper_util::rt::TokioExecutor;
use rustls::ClientConfig;
use tower_service::Service;

type HttpsConnector = hyper_rustls::HttpsConnector<HttpConnector<OverrideResolver>>;
type UpstreamClient = Client<HttpsConnector, Full<Bytes>>;

// Hop-by-hop headers (RFC 7230 §6.1) plus "host" (hyper derives that from the
// URI). The names not in hyper's typed-constant set (proxy-connection,
// keep-alive — not IANA-registered) are matched by lowercase string.
const SKIPPED_REQUEST_HEADERS: &[HeaderName] = &[
	HOST,
	CONNECTION,
	PROXY_AUTHORIZATION,
	TE,
	TRAILER,
	TRANSFER_ENCODING,
	UPGRADE,
];
const SKIPPED_REQUEST_HEADER_STRINGS: &[&str] = &["proxy-connection", "keep-alive"];

fn strip_hop_by_hop_headers(headers: &mut hyper::HeaderMap) {
	for h in SKIPPED_REQUEST_HEADERS {
		headers.remove(h);
	}
	for h in SKIPPED_REQUEST_HEADER_STRINGS {
		headers.remove(*h);
	}
}

/// Returns pre-baked IPs for the target API hostnames so the proxy's own
/// upstream calls bypass the `hosts`-file redirect and don't loop.
#[derive(Clone)]
pub struct OverrideResolver {
	overrides: Arc<HashMap<String, Vec<IpAddr>>>,
	fallback: GaiResolver,
}

impl OverrideResolver {
	fn new(overrides: HashMap<String, Vec<IpAddr>>) -> Self {
		Self { overrides: Arc::new(overrides), fallback: GaiResolver::new() }
	}
}

impl Service<Name> for OverrideResolver {
	type Response = VecIntoIter<SocketAddr>;
	type Error = io::Error;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut TaskContext<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, name: Name) -> Self::Future {
		if let Some(ips) = self.overrides.get(name.as_str()) {
			let addrs: Vec<SocketAddr> =
				ips.iter().map(|ip| SocketAddr::new(*ip, 0)).collect();
			return Box::pin(async move { Ok(addrs.into_iter()) });
		}
		let mut fallback = self.fallback.clone();
		Box::pin(async move {
			let iter = fallback.call(name).await.map_err(io::Error::other)?;
			Ok(iter.collect::<Vec<_>>().into_iter())
		})
	}
}

pub struct Upstream {
	client: UpstreamClient,
}

impl Upstream {
	pub fn new(dns_overrides: HashMap<String, Vec<IpAddr>>) -> Self {
		let mut root_store = rustls::RootCertStore::empty();
		root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

		let tls_config = ClientConfig::builder()
			.with_root_certificates(root_store)
			.with_no_client_auth();

		let resolver = OverrideResolver::new(dns_overrides);
		let mut http = HttpConnector::new_with_resolver(resolver);
		http.enforce_http(false);

		let https = hyper_rustls::HttpsConnectorBuilder::new()
			.with_tls_config(tls_config)
			.https_or_http()
			.enable_http1()
			.wrap_connector(http);

		let client: UpstreamClient =
			Client::builder(TokioExecutor::new()).build::<_, Full<Bytes>>(https);

		Self { client }
	}

	pub async fn forward(
		&self,
		host: &str,
		req: Request<Incoming>,
	) -> Result<Response<Bytes>> {
		let (mut parts, body) = req.into_parts();
		let body_bytes = body.collect().await.context("reading client body")?.to_bytes();

		let path_and_query = parts
			.uri
			.path_and_query()
			.cloned()
			.unwrap_or_else(|| PathAndQuery::from_static("/"));

		let upstream_uri = Uri::builder()
			.scheme(Scheme::HTTPS)
			.authority(Authority::try_from(host).context("invalid host")?)
			.path_and_query(path_and_query)
			.build()?;

		parts.uri = upstream_uri;
		strip_hop_by_hop_headers(&mut parts.headers);
		let upstream_req = Request::from_parts(parts, Full::new(body_bytes));

		let upstream_resp = self.client.request(upstream_req).await
			.with_context(|| format!("upstream request to https://{host}"))?;

		let (parts, body) = upstream_resp.into_parts();
		let body_bytes = body.collect().await.context("reading upstream body")?.to_bytes();

		Ok(Response::from_parts(parts, body_bytes))
	}

	pub async fn get_json(&self, url: &str) -> Result<serde_json::Value> {
		let uri: Uri = url.parse().context("parsing url")?;
		let req = Request::builder()
			.method(Method::GET)
			.uri(uri)
			.header("user-agent", "photobooth-bridge")
			.body(Full::new(Bytes::new()))?;

		let resp = self.client.request(req).await
			.with_context(|| format!("GET {url}"))?;

		if !resp.status().is_success() {
			anyhow::bail!("upstream {url} returned {}", resp.status());
		}

		let body = resp.into_body().collect().await?.to_bytes();
		Ok(serde_json::from_slice(&body)?)
	}
}
