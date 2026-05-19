use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rcgen::{
	BasicConstraints, Certificate, CertificateParams, DistinguishedName, DnType,
	ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha1::{Digest, Sha1};

use crate::paths::{ca_cert_path, ca_key_path, ensure_data_dir};
#[cfg(unix)]
use crate::paths::chown_to_sudo_user;

pub const CA_COMMON_NAME: &str = "Naraka Photo Booth Bridge Root CA";
const CA_ORGANIZATION: &str = "naraka.wiki";

fn ca_params(key_pair: &KeyPair) -> Result<CertificateParams> {
	let mut params = CertificateParams::new(Vec::new())?;
	params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
	// Intentionally NO KeyUsage extension. rcgen 0.13 encodes the BIT STRING
	// non-canonically (`03 03 07 86 00` — trailing zero byte) and Wine's
	// CRYPT_KeyUsageValid only inspects the last byte, so it mis-reads our
	// keyCertSign bit as unset and the chain fails policy SSL with
	// CERT_E_WRONG_USAGE. Wine and real Windows both accept V3 CA certs
	// without a KeyUsage extension at all (see Wine dlls/crypt32/chain.c
	// CRYPT_KeyUsageValid: "MS appears to accept certs that do not contain
	// key usage extensions as CA certs").
	params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
	let mut dn = DistinguishedName::new();
	dn.push(DnType::CommonName, CA_COMMON_NAME);
	dn.push(DnType::OrganizationName, CA_ORGANIZATION);
	params.distinguished_name = dn;
	// Serial pinned to a hash of the public key so the cert's TBS bytes are
	// stable across runs. rcgen still re-signs the cert on every load using
	// ECDSA P-256 with a random per-signature nonce, so the wrapping
	// signature (and therefore the SHA1 thumbprint) varies — the install-ca
	// path leans on the wine.rs orphan scrubber to keep the trust store tidy
	// across re-installs. Functionally fine: SChannel matches trust anchors
	// by (subject, public-key), so any of these self-signed siblings serves
	// as the anchor for our leaves.
	let pubkey_der = key_pair.public_key_der();
	let mut hasher = Sha1::new();
	hasher.update(&pubkey_der);
	let mut serial = hasher.finalize().to_vec();
	serial[0] &= 0x7F; // X.509 serial is a positive INTEGER
	params.serial_number = Some(SerialNumber::from(serial));
	Ok(params)
}

pub struct CertificateAuthority {
	key_pair: KeyPair,
	// Issuer cert is signed once at startup and reused for every leaf — avoids
	// re-doing the CA self-signature on every TLS handshake.
	issuer: Certificate,
	// Per-SNI leaf certs are signed lazily on first connection and reused;
	// keypair generation + signing is the heaviest work in the TLS hot path.
	leaf_cache: Mutex<HashMap<String, Arc<rustls::ServerConfig>>>,
}

impl CertificateAuthority {
	pub fn load_or_create() -> Result<Self> {
		if let Some(existing) = Self::load_existing()? {
			return Ok(existing);
		}
		ensure_data_dir()?;
		let cert_path = ca_cert_path()?;
		let key_path = ca_key_path()?;
		let key_pair = KeyPair::generate()?;
		let issuer = ca_params(&key_pair)?.self_signed(&key_pair)?;
		persist(&cert_path, &issuer.pem(), &key_path, &key_pair.serialize_pem())?;
		tracing::info!("generated new root CA at {}", cert_path.display());
		Ok(Self { key_pair, issuer, leaf_cache: Mutex::new(HashMap::new()) })
	}

	/// Load the CA from disk if both files exist; return `None` otherwise.
	/// Use this from uninstall paths so we don't accidentally generate a new
	/// CA just to read its fingerprint.
	pub fn load_existing() -> Result<Option<Self>> {
		let cert_path = ca_cert_path()?;
		let key_path = ca_key_path()?;
		if !(cert_path.exists() && key_path.exists()) {
			return Ok(None);
		}
		Self::load(&key_path).map(Some)
	}

	fn load(key_path: &Path) -> Result<Self> {
		let key_pem = fs::read_to_string(key_path)
			.with_context(|| format!("reading {}", key_path.display()))?;

		let key_pair = KeyPair::from_pem(&key_pem).context("parsing CA private key")?;
		// rcgen 0.13 dropped from_ca_cert_pem, so we rebuild params from
		// scratch and re-self-sign. ECDSA's random nonce means the in-memory
		// cert's signature (and SHA1 thumbprint) differs from the on-disk PEM
		// — but SChannel/Wine match trust anchors by (subject, public key),
		// not by signature or thumbprint, so any sibling self-signed cert
		// over the same key serves as the anchor for our leaves.
		let issuer = ca_params(&key_pair)?.self_signed(&key_pair)?;

		Ok(Self { key_pair, issuer, leaf_cache: Mutex::new(HashMap::new()) })
	}

	/// DER bytes of the self-signed CA cert. Used by the Wine-prefix trust
	/// install path, which writes the cert directly into the prefix registry.
	pub fn cert_der(&self) -> &[u8] {
		self.issuer.der().as_ref()
	}

	pub fn issue_leaf(&self, host: &str) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
		let mut leaf_params = CertificateParams::new(Vec::new())?;
		leaf_params.is_ca = IsCa::NoCa;
		leaf_params.key_usages =
			vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
		leaf_params.extended_key_usages = vec![
			ExtendedKeyUsagePurpose::ServerAuth,
			ExtendedKeyUsagePurpose::ClientAuth,
		];

		leaf_params.subject_alt_names = vec![if let Ok(ip) = host.parse() {
			SanType::IpAddress(ip)
		} else {
			SanType::DnsName(host.try_into().context("invalid SAN hostname")?)
		}];

		let mut dn = DistinguishedName::new();
		dn.push(DnType::CommonName, host);
		leaf_params.distinguished_name = dn;

		let leaf_key = KeyPair::generate()?;
		let leaf_cert = leaf_params.signed_by(&leaf_key, &self.issuer, &self.key_pair)?;

		let cert_chain = vec![leaf_cert.der().clone(), self.issuer.der().clone()];
		let key_der =
			PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));

		Ok((cert_chain, key_der))
	}

	pub fn server_config_for(&self, host: &str) -> Result<Arc<rustls::ServerConfig>> {
		// Single locked critical section: read-then-insert under one lock so
		// two concurrent first-connections for the same SNI don't both sign.
		let mut cache = self.leaf_cache.lock().unwrap();
		if let Some(cached) = cache.get(host) {
			return Ok(Arc::clone(cached));
		}
		let (chain, key) = self.issue_leaf(host)?;
		let mut config = rustls::ServerConfig::builder()
			.with_no_client_auth()
			.with_single_cert(chain, key)
			.context("building TLS server config")?;
		config.alpn_protocols = vec![b"http/1.1".to_vec()];
		let config = Arc::new(config);
		cache.insert(host.to_string(), Arc::clone(&config));
		Ok(config)
	}
}

fn persist(cert_path: &Path, cert_pem: &str, key_path: &Path, key_pem: &str) -> Result<()> {
	fs::write(cert_path, cert_pem)
		.with_context(|| format!("writing {}", cert_path.display()))?;
	fs::write(key_path, key_pem)
		.with_context(|| format!("writing {}", key_path.display()))?;
	#[cfg(unix)]
	{
		use std::os::unix::fs::PermissionsExt;
		fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))?;
		// Under sudo we'd otherwise leave root-owned CA files, locking the real
		// user out of the mode-600 key and breaking later non-sudo runs.
		chown_to_sudo_user(cert_path);
		chown_to_sudo_user(key_path);
	}
	Ok(())
}
