use std::fmt;
use std::path::Path;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

/// SHA-256 of a certificate's DER encoding. This is what gets pinned.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(cert_der: &[u8]) -> Self {
        Fingerprint(Sha256::digest(cert_der).into())
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Fingerprint(out))
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", self.to_hex())
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// This install's self-signed certificate and private key.
pub struct Identity {
    pub(crate) cert: CertificateDer<'static>,
    pub(crate) key: PrivateKeyDer<'static>,
    fingerprint: Fingerprint,
}

impl Clone for Identity {
    fn clone(&self) -> Self {
        Identity { cert: self.cert.clone(), key: self.key.clone_key(), fingerprint: self.fingerprint }
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("fingerprint", &self.fingerprint).finish_non_exhaustive()
    }
}

impl Identity {
    /// Fresh in-memory identity; nothing is written to disk.
    pub fn generate() -> Result<Self> {
        let ck = rcgen::generate_simple_self_signed(vec!["telepresence".to_string()])?;
        let cert = ck.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(ck.signing_key.serialize_der().into());
        Ok(Self::from_parts(cert, key))
    }

    /// Loads the PEM file at `path`, or generates an identity and writes it
    /// there. The file holds the certificate followed by the private key, so a
    /// single path is enough for the caller to manage.
    pub fn load_or_generate(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(pem) => Self::from_pem(&pem),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let id = Self::generate()?;
                if let Some(dir) = path.parent()
                    && !dir.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(path, id.to_pem())?;
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn from_pem(pem: &[u8]) -> Result<Self> {
        let cert = CertificateDer::from_pem_slice(pem)
            .map_err(|_| Error::Protocol("identity file has no certificate"))?;
        let key = PrivateKeyDer::from_pem_slice(pem)
            .map_err(|_| Error::Protocol("identity file has no private key"))?;
        Ok(Self::from_parts(cert, key))
    }

    pub fn to_pem(&self) -> String {
        format!(
            "{}{}",
            pem_block("CERTIFICATE", self.cert.as_ref()),
            pem_block("PRIVATE KEY", self.key.secret_der())
        )
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }

    fn from_parts(cert: CertificateDer<'static>, key: PrivateKeyDer<'static>) -> Self {
        let fingerprint = Fingerprint::of(cert.as_ref());
        Identity { cert, key, fingerprint }
    }
}

fn pem_block(label: &str, der: &[u8]) -> String {
    let b64 = base64_encode(der);
    let mut s = format!("-----BEGIN {label}-----\n");
    for line in b64.as_bytes().chunks(64) {
        s.push_str(std::str::from_utf8(line).expect("ascii"));
        s.push('\n');
    }
    s.push_str(&format!("-----END {label}-----\n"));
    s
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("tp-proto-id-{}", std::process::id()));
        let path = dir.join("identity.pem");
        let _ = std::fs::remove_file(&path);
        let a = Identity::load_or_generate(&path).unwrap();
        let b = Identity::load_or_generate(&path).unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_hex_round_trip() {
        let id = Identity::generate().unwrap();
        let fp = id.fingerprint();
        assert_eq!(Fingerprint::from_hex(&fp.to_hex()), Some(fp));
        assert_eq!(Fingerprint::from_hex("zz"), None);
    }
}
