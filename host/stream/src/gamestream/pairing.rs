//! GameStream PIN pairing, server side. Four HTTP round trips after the
//! client shows the PIN (`/pair?phrase=getservercert`, then `clientchallenge`,
//! `serverchallengeresp`, `clientpairingsecret`):
//!
//!   key = SHA-256(salt ‖ PIN)[..16]                      (AES-128-ECB, no padding)
//!   1. client: salt, its cert          → server: our cert (after the PIN is entered here)
//!   2. client: E(challenge)            → server: E(SHA-256(challenge ‖ sig(our cert) ‖ S) ‖ serverchallenge)
//!   3. client: E(clienthash)           → server: S ‖ RSA-SHA256(S) with our key
//!   4. client: C ‖ RSA-SHA256(C)       → server checks the signature and that
//!      SHA-256(serverchallenge ‖ sig(client cert) ‖ C) == clienthash; then the client is paired
//!
//! "sig(cert)" is the certificate's own signature bytes. Both sides prove they
//! know the PIN and hold their certificate's key; the client cert is then pinned.

use anyhow::{anyhow, bail, Result};
use openssl::hash::{hash, MessageDigest};
use openssl::pkey::{PKey, Private};
use openssl::sign::{Signer, Verifier};
use openssl::symm::{Cipher, Crypter, Mode};
use openssl::x509::X509;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    WaitPin,
    GotCert,
    ClientChallenge,
    ServerChallengeResp,
}

pub struct Pairing {
    pub client_cert_pem: String,
    pub device: String,
    pub salt: [u8; 16],
    pub phase: Phase,
    key: [u8; 16],
    server_secret: [u8; 16],
    server_challenge: [u8; 16],
    client_hash: Vec<u8>,
}

pub fn aes_key(salt: &[u8; 16], pin: &str) -> [u8; 16] {
    let mut d = salt.to_vec();
    d.extend_from_slice(pin.as_bytes());
    let h = hash(MessageDigest::sha256(), &d).expect("sha256");
    h[..16].try_into().unwrap()
}

pub fn ecb(key: &[u8; 16], data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        bail!("pairing data is not whole AES blocks");
    }
    let mut c = Crypter::new(
        Cipher::aes_128_ecb(),
        if encrypt {
            Mode::Encrypt
        } else {
            Mode::Decrypt
        },
        key,
        None,
    )?;
    c.pad(false);
    let mut out = vec![0u8; data.len() + 16];
    let mut n = c.update(data, &mut out)?;
    n += c.finalize(&mut out[n..])?;
    out.truncate(n);
    Ok(out)
}

pub fn cert_signature(c: &X509) -> Vec<u8> {
    c.signature().as_slice().to_vec()
}

fn rand16() -> [u8; 16] {
    let mut b = [0u8; 16];
    openssl::rand::rand_bytes(&mut b).expect("RNG");
    b
}

pub fn sha256(parts: &[&[u8]]) -> Vec<u8> {
    let mut d = Vec::new();
    for p in parts {
        d.extend_from_slice(p);
    }
    hash(MessageDigest::sha256(), &d).expect("sha256").to_vec()
}

impl Pairing {
    pub fn new(salt: [u8; 16], client_cert_pem: String, device: String) -> Pairing {
        Pairing {
            client_cert_pem,
            device,
            salt,
            phase: Phase::WaitPin,
            key: [0; 16],
            server_secret: [0; 16],
            server_challenge: [0; 16],
            client_hash: Vec::new(),
        }
    }

    /// The PIN was entered on the host.
    pub fn set_pin(&mut self, pin: &str) {
        self.key = aes_key(&self.salt, pin);
        self.phase = Phase::GotCert;
    }

    /// Phase 2: returns `challengeresponse`.
    pub fn client_challenge(&mut self, enc: &[u8], our_cert: &X509) -> Result<Vec<u8>> {
        if self.phase != Phase::GotCert {
            bail!("out of order: clientchallenge");
        }
        let challenge = ecb(&self.key, enc, false)?;
        self.server_secret = rand16();
        self.server_challenge = rand16();
        let h = sha256(&[&challenge, &cert_signature(our_cert), &self.server_secret]);
        let mut plain = h;
        plain.extend_from_slice(&self.server_challenge);
        self.phase = Phase::ClientChallenge;
        ecb(&self.key, &plain, true)
    }

    /// Phase 3: returns `pairingsecret`.
    pub fn server_challenge_resp(
        &mut self,
        enc: &[u8],
        our_key: &PKey<Private>,
    ) -> Result<Vec<u8>> {
        if self.phase != Phase::ClientChallenge {
            bail!("out of order: serverchallengeresp");
        }
        self.client_hash = ecb(&self.key, enc, false)?;
        let mut s = Signer::new(MessageDigest::sha256(), our_key)?;
        s.update(&self.server_secret)?;
        let sig = s.sign_to_vec()?;
        let mut out = self.server_secret.to_vec();
        out.extend_from_slice(&sig);
        self.phase = Phase::ServerChallengeResp;
        Ok(out)
    }

    /// Phase 4: true if the client proved the PIN and its key.
    pub fn client_pairing_secret(&mut self, data: &[u8]) -> Result<bool> {
        if self.phase != Phase::ServerChallengeResp {
            bail!("out of order: clientpairingsecret");
        }
        if data.len() <= 16 {
            bail!("client pairing secret too short");
        }
        let (secret, sig) = data.split_at(16);
        let cert = X509::from_pem(self.client_cert_pem.as_bytes())
            .map_err(|e| anyhow!("client certificate: {e}"))?;
        let expect = sha256(&[&self.server_challenge, &cert_signature(&cert), secret]);
        if expect != self.client_hash {
            return Ok(false); // wrong PIN (or someone in the middle)
        }
        let pk = cert.public_key()?;
        let mut v = Verifier::new(MessageDigest::sha256(), &pk)?;
        v.update(secret)?;
        Ok(v.verify(sig).unwrap_or(false))
    }
}

/// The client half, for tests (what Moonlight does).
#[cfg(test)]
pub mod client {
    use super::*;

    pub struct Client {
        pub key: PKey<Private>,
        pub cert: X509,
        pub salt: [u8; 16],
        aes: [u8; 16],
        challenge: [u8; 16],
        secret: [u8; 16],
        server_cert: Option<X509>,
        server_secret: Vec<u8>,
    }

    impl Client {
        pub fn new(pin: &str) -> Client {
            let key = PKey::from_rsa(openssl::rsa::Rsa::generate(2048).unwrap()).unwrap();
            let mut b = openssl::x509::X509Builder::new().unwrap();
            let mut n = openssl::x509::X509NameBuilder::new().unwrap();
            n.append_entry_by_text("CN", "NVIDIA GameStream Client")
                .unwrap();
            let n = n.build();
            b.set_subject_name(&n).unwrap();
            b.set_issuer_name(&n).unwrap();
            b.set_pubkey(&key).unwrap();
            b.set_not_before(&openssl::asn1::Asn1Time::days_from_now(0).unwrap())
                .unwrap();
            b.set_not_after(&openssl::asn1::Asn1Time::days_from_now(10).unwrap())
                .unwrap();
            b.sign(&key, MessageDigest::sha256()).unwrap();
            let salt = rand16();
            Client {
                aes: aes_key(&salt, pin),
                key,
                cert: b.build(),
                salt,
                challenge: rand16(),
                secret: rand16(),
                server_cert: None,
                server_secret: vec![],
            }
        }
        pub fn got_server_cert(&mut self, c: X509) {
            self.server_cert = Some(c);
        }
        pub fn challenge(&self) -> Vec<u8> {
            ecb(&self.aes, &self.challenge, true).unwrap()
        }
        /// Returns E(clienthash).
        pub fn on_challenge_response(&mut self, enc: &[u8]) -> Vec<u8> {
            let p = ecb(&self.aes, enc, false).unwrap();
            let (_hash, server_challenge) = p.split_at(32);
            let h = sha256(&[server_challenge, &cert_signature(&self.cert), &self.secret]);
            ecb(&self.aes, &h, true).unwrap()
        }
        pub fn on_pairing_secret(&mut self, data: &[u8]) -> bool {
            let (secret, sig) = data.split_at(16);
            self.server_secret = secret.to_vec();
            let pk = self.server_cert.as_ref().unwrap().public_key().unwrap();
            let mut v = Verifier::new(MessageDigest::sha256(), &pk).unwrap();
            v.update(secret).unwrap();
            v.verify(sig).unwrap()
        }
        pub fn pairing_secret(&self) -> Vec<u8> {
            let mut s = Signer::new(MessageDigest::sha256(), &self.key).unwrap();
            s.update(&self.secret).unwrap();
            let mut out = self.secret.to_vec();
            out.extend_from_slice(&s.sign_to_vec().unwrap());
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::client::Client;
    use super::*;

    fn server_identity() -> (PKey<Private>, X509) {
        let c = Client::new("0000");
        (c.key, c.cert)
    }

    fn run(client_pin: &str, host_pin: &str) -> bool {
        let (skey, scert) = server_identity();
        let mut c = Client::new(client_pin);
        let mut p = Pairing::new(
            c.salt,
            String::from_utf8(c.cert.to_pem().unwrap()).unwrap(),
            "test".into(),
        );
        p.set_pin(host_pin);
        c.got_server_cert(scert.clone());
        let resp = p.client_challenge(&c.challenge(), &scert).unwrap();
        let ch = c.on_challenge_response(&resp);
        let ps = p.server_challenge_resp(&ch, &skey).unwrap();
        assert!(c.on_pairing_secret(&ps), "server signature must verify");
        p.client_pairing_secret(&c.pairing_secret()).unwrap()
    }

    #[test]
    fn right_pin_pairs() {
        assert!(run("1234", "1234"));
    }

    #[test]
    fn wrong_pin_does_not() {
        assert!(!run("1234", "4321"));
    }

    #[test]
    fn phases_must_come_in_order() {
        let c = Client::new("1111");
        let (skey, scert) = server_identity();
        let mut p = Pairing::new(c.salt, String::new(), String::new());
        assert!(p.client_challenge(&[0u8; 16], &scert).is_err());
        p.set_pin("1111");
        assert!(p.server_challenge_resp(&[0u8; 16], &skey).is_err());
        assert!(p.client_pairing_secret(&[0u8; 300]).is_err());
    }

    #[test]
    fn key_derivation_is_sha256_of_salt_and_pin() {
        let salt = [7u8; 16];
        let k = aes_key(&salt, "5678");
        let mut d = salt.to_vec();
        d.extend_from_slice(b"5678");
        assert_eq!(&k[..], &hash(MessageDigest::sha256(), &d).unwrap()[..16]);
    }
}
