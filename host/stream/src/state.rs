//! The stream host's identity and its paired clients, in
//! `$XDG_CONFIG_HOME/conduit/stream/` (default `~/.config/conduit/stream/`):
//!
//!   key.pem    RSA-2048 private key (0600)
//!   cert.pem   self-signed certificate (what Moonlight pins at pairing)
//!   state.json { uniqueid, link_token, clients: [{ name, cert, added }] }
//!
//! Shared by every stream host of this user (one per VM): pairing once works
//! for all of them. The file is re-read on every check, so a pairing done by
//! one instance is seen by the others and `unpair` takes effect at once.

use anyhow::{Context, Result};
use openssl::asn1::Asn1Time;
use openssl::bn::{BigNum, MsbOption};
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::rsa::Rsa;
use openssl::x509::{X509Builder, X509NameBuilder, X509};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Client {
    pub name: String,
    /// PEM.
    pub cert: String,
    #[serde(default)]
    pub added: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Saved {
    uniqueid: String,
    #[serde(default)]
    link_token: String,
    #[serde(default)]
    clients: Vec<Client>,
}

pub struct State {
    dir: PathBuf,
    pub uniqueid: String,
    pub link_token: String,
    pub key: PKey<Private>,
    pub cert: X509,
    pub cert_pem: String,
    lock: Mutex<()>,
}

pub fn default_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into())).join(".config")
        });
    base.join("conduit").join("stream")
}

pub fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    openssl::rand::rand_bytes(&mut b).expect("RNG");
    hex(&b)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

fn write_private(p: &Path, data: &[u8]) -> Result<()> {
    let tmp = p.with_extension("tmp");
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, p)?;
    Ok(())
}

fn make_cert(key: &PKey<Private>) -> Result<X509> {
    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_text("CN", "Conduit Stream")?;
    let name = name.build();
    let mut b = X509Builder::new()?;
    b.set_version(2)?;
    let mut serial = BigNum::new()?;
    serial.rand(127, MsbOption::MAYBE_ZERO, false)?;
    let serial = serial.to_asn1_integer()?;
    b.set_serial_number(&serial)?;
    b.set_subject_name(&name)?;
    b.set_issuer_name(&name)?;
    b.set_pubkey(key)?;
    let (nb, na) = (
        Asn1Time::days_from_now(0)?,
        Asn1Time::days_from_now(20 * 365)?,
    );
    b.set_not_before(&nb)?;
    b.set_not_after(&na)?;
    b.sign(key, MessageDigest::sha256())?;
    Ok(b.build())
}

impl State {
    /// Load, creating key, certificate and ids on first use.
    pub fn open(dir: &Path) -> Result<State> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let kp = dir.join("key.pem");
        let cp = dir.join("cert.pem");
        let (key, cert) = if kp.is_file() && cp.is_file() {
            let key = PKey::private_key_from_pem(&std::fs::read(&kp)?)
                .with_context(|| format!("reading {}", kp.display()))?;
            let cert = X509::from_pem(&std::fs::read(&cp)?)
                .with_context(|| format!("reading {}", cp.display()))?;
            (key, cert)
        } else {
            let key = PKey::from_rsa(Rsa::generate(2048)?)?;
            let cert = make_cert(&key)?;
            write_private(&kp, &key.private_key_to_pem_pkcs8()?)?;
            write_private(&cp, &cert.to_pem()?)?;
            (key, cert)
        };
        let cert_pem = String::from_utf8(cert.to_pem()?)?;
        let st = State {
            dir: dir.to_path_buf(),
            uniqueid: String::new(),
            link_token: String::new(),
            key,
            cert,
            cert_pem,
            lock: Mutex::new(()),
        };
        let mut saved = st.load();
        let mut dirty = false;
        if saved.uniqueid.is_empty() {
            saved.uniqueid = random_hex(8);
            dirty = true;
        }
        if saved.link_token.is_empty() {
            saved.link_token = random_hex(16).to_ascii_lowercase();
            dirty = true;
        }
        if dirty {
            st.save(&saved)?;
        }
        Ok(State {
            uniqueid: saved.uniqueid,
            link_token: saved.link_token,
            ..st
        })
    }

    fn path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn load(&self) -> Saved {
        std::fs::read_to_string(self.path())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, s: &Saved) -> Result<()> {
        write_private(&self.path(), serde_json::to_string_pretty(s)?.as_bytes())
    }

    pub fn clients(&self) -> Vec<Client> {
        self.load().clients
    }

    /// The paired client whose certificate this is.
    pub fn client_for(&self, cert: &X509) -> Option<Client> {
        let der = cert.to_der().ok()?;
        self.clients().into_iter().find(|c| {
            X509::from_pem(c.cert.as_bytes())
                .ok()
                .and_then(|x| x.to_der().ok())
                .is_some_and(|d| d == der)
        })
    }

    pub fn add_client(&self, name: &str, cert_pem: &str) -> Result<()> {
        let _g = self.lock.lock().unwrap();
        let cert = X509::from_pem(cert_pem.as_bytes()).context("client certificate")?;
        let der = cert.to_der()?;
        let mut s = self.load();
        s.clients.retain(|c| {
            X509::from_pem(c.cert.as_bytes())
                .ok()
                .and_then(|x| x.to_der().ok())
                .is_none_or(|d| d != der)
        });
        s.clients.push(Client {
            name: name.to_string(),
            cert: String::from_utf8(cert.to_pem()?)?,
            added: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
        self.save(&s)
    }

    /// Remove clients named `name` (or all with "*"). Returns how many.
    pub fn remove_client(&self, name: &str) -> Result<usize> {
        let _g = self.lock.lock().unwrap();
        let mut s = self.load();
        let before = s.clients.len();
        s.clients.retain(|c| name != "*" && c.name != name);
        let n = before - s.clients.len();
        if n > 0 {
            self.save(&s)?;
        }
        Ok(n)
    }

    /// SHA-256 fingerprint of our certificate (what the conduit link pins).
    pub fn fingerprint(&self) -> String {
        self.cert
            .digest(MessageDigest::sha256())
            .map(|d| hex(&d).to_ascii_lowercase())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_created_once_and_clients_persist() {
        let d = std::env::temp_dir().join(format!("cs-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let a = State::open(&d).unwrap();
        let b = State::open(&d).unwrap();
        assert_eq!(a.uniqueid, b.uniqueid);
        assert_eq!(a.cert_pem, b.cert_pem);
        assert_eq!(a.link_token.len(), 32);
        assert!(a.client_for(&b.cert).is_none());
        a.add_client("phone", &b.cert_pem).unwrap();
        a.add_client("phone again", &b.cert_pem).unwrap();
        assert_eq!(b.clients().len(), 1);
        assert_eq!(b.client_for(&a.cert).unwrap().name, "phone again");
        assert_eq!(a.remove_client("phone again").unwrap(), 1);
        assert!(b.clients().is_empty());
        let mode = std::fs::metadata(d.join("key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(
            unhex(&hex(&[0, 1, 0xab, 0xff])).unwrap(),
            vec![0, 1, 0xab, 0xff]
        );
        assert_eq!(unhex("0a0B").unwrap(), vec![10, 11]);
        assert!(unhex("abc").is_none());
        assert!(unhex("zz").is_none());
    }
}
