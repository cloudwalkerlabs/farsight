//! Client keys and server pinning (`docs/design.md` §6).
//!
//! **Clients** hold an Ed25519 key and prove it in `Hello` by signing a
//! value exported from the connection's keys (RFC 5705-style keying
//! material), which is unique to the connection. A signature can't be
//! replayed on another connection, and a man in the middle, holding two
//! connections, can't forward one. The server accepts the keys listed in
//! its `authorized_keys`, one per line in OpenSSH's format
//! (`ssh-ed25519 AAAA… comment`), read again on every connection.
//!
//! **Servers** are pinned on first use: `known_hosts` holds one line per
//! server, `name mode fingerprint`, where mode is `tls` (with the
//! certificate's SHA-256) or `plain` (plaintext mode, §1, with `-`). A
//! server known in TLS is never reached in plaintext without removing its
//! line first.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use quinn::Connection;
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

/// The exporter label; the context is empty.
const LABEL: &[u8] = b"EXPORTER-farsight-client-auth";

/// What the client signs: a domain separator, then the exported value.
const DOMAIN: &[u8] = b"farsight client auth v1\0";

const KEY_TYPE: &str = "ssh-ed25519";

/// A client's key pair.
pub struct ClientKey {
    pair: Ed25519KeyPair,
}

impl ClientKey {
    /// Loads `path` (PKCS#8), creating it first if it doesn't exist.
    pub fn load_or_generate(path: &Path) -> anyhow::Result<Self> {
        let der = match std::fs::read(path) {
            Ok(der) => der,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
                    .map_err(|_| anyhow::anyhow!("generating a key"))?;
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                }
                crate::endpoint::write_private(path, doc.as_ref())
                    .with_context(|| format!("writing {}", path.display()))?;
                tracing::info!(path = %path.display(), "new client key");
                doc.as_ref().to_vec()
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der)
            .map_err(|e| anyhow::anyhow!("{}: not an Ed25519 key: {e}", path.display()))?;
        Ok(Self { pair })
    }

    pub fn public(&self) -> [u8; 32] {
        self.pair.public_key().as_ref().try_into().expect("Ed25519 public keys are 32 bytes")
    }

    /// The public key as an `authorized_keys` line.
    pub fn authorized_line(&self, comment: &str) -> String {
        format_line(&self.public(), comment)
    }

    /// Proves this key on `conn`.
    pub fn sign(&self, conn: &Connection) -> anyhow::Result<Vec<u8>> {
        Ok(self.pair.sign(&message(conn)?).as_ref().to_vec())
    }
}

fn message(conn: &Connection) -> anyhow::Result<Vec<u8>> {
    let mut exported = [0; 32];
    conn.export_keying_material(&mut exported, LABEL, b"")
        .map_err(|_| anyhow::anyhow!("the connection can't export keying material"))?;
    Ok([DOMAIN, &exported].concat())
}

/// Whether `signature` proves `key` on `conn`.
pub fn verify(conn: &Connection, key: &[u8; 32], signature: &[u8]) -> bool {
    message(conn).is_ok_and(|m| UnparsedPublicKey::new(&ED25519, key).verify(&m, signature).is_ok())
}

/// `ssh-ed25519 <base64 of the SSH wire blob> comment`.
pub fn format_line(key: &[u8; 32], comment: &str) -> String {
    let mut blob = Vec::with_capacity(51);
    blob.extend_from_slice(&(KEY_TYPE.len() as u32).to_be_bytes());
    blob.extend_from_slice(KEY_TYPE.as_bytes());
    blob.extend_from_slice(&32u32.to_be_bytes());
    blob.extend_from_slice(key);
    format!("{KEY_TYPE} {} {comment}", BASE64.encode(blob)).trim_end().to_string()
}

/// The keys in an `authorized_keys` file. Lines of other key types, and
/// blank and comment lines, are skipped; options are not supported.
pub fn parse_authorized(text: &str) -> Vec<[u8; 32]> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<[u8; 32]> {
    let mut words = line.split_whitespace();
    if words.next()? != KEY_TYPE {
        return None;
    }
    let blob = BASE64.decode(words.next()?).ok()?;
    let (kind, rest) = read_string(&blob)?;
    if kind != KEY_TYPE.as_bytes() {
        return None;
    }
    let (key, rest) = read_string(rest)?;
    if !rest.is_empty() {
        return None;
    }
    key.try_into().ok()
}

fn read_string(b: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as usize;
    let s = b.get(4..4 + len)?;
    Some((s, &b[4 + len..]))
}

/// Whether `key` is in the `authorized_keys` file at `path`. A missing file
/// authorizes no one.
pub fn is_authorized(path: &Path, key: &[u8; 32]) -> anyhow::Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_authorized(&text).contains(key)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// How a server is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pin {
    /// TLS, with this certificate fingerprint.
    Tls(String),
    /// Plaintext mode (§1).
    Plain,
}

/// The client's record of the servers it has seen.
pub struct KnownHosts {
    path: PathBuf,
}

impl KnownHosts {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn lookup(&self, name: &str) -> anyhow::Result<Option<(usize, Pin)>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        for (i, line) in text.lines().enumerate() {
            let w: Vec<&str> = line.split_whitespace().collect();
            match w.as_slice() {
                [n, "tls", fp] if *n == name => return Ok(Some((i + 1, Pin::Tls(fp.to_string())))),
                [n, "plain", ..] if *n == name => return Ok(Some((i + 1, Pin::Plain))),
                _ => {}
            }
        }
        Ok(None)
    }

    /// Checks `seen` against the record for `name`, recording it if there
    /// is none (trust on first use). Fails on a changed certificate, and on
    /// plaintext to a server known in TLS.
    pub fn check(&self, name: &str, seen: &Pin) -> anyhow::Result<()> {
        let known = self.lookup(name)?;
        let file = self.path.display();
        match (known, seen) {
            (Some((_, k)), s) if k == *s => Ok(()),
            (Some((line, Pin::Tls(old))), Pin::Tls(new)) => bail!(
                "{name}'s certificate has changed!\n  known:  {old}\n  now:    {new}\n\
                 Someone may be intercepting the connection. If the server's identity was \
                 replaced on purpose, remove line {line} of {file}."
            ),
            (Some((line, Pin::Tls(_))), Pin::Plain) => bail!(
                "{name} is known to use TLS; refusing plaintext. If it has moved to --no-tls, \
                 remove line {line} of {file}."
            ),
            (Some((line, Pin::Plain)), Pin::Tls(fp)) => {
                // Moving to TLS only adds protection; pin the certificate.
                self.replace(line, name, &Pin::Tls(fp.clone()))
            }
            (None, s) => {
                self.append(name, s)?;
                match s {
                    Pin::Tls(fp) => tracing::warn!(%name, %fp, file = %file, "first connection: pinned the certificate"),
                    Pin::Plain => tracing::warn!(%name, file = %file, "first connection: recorded as plaintext"),
                }
                Ok(())
            }
            (Some(_), _) => unreachable!("every combination is covered above"),
        }
    }

    /// Forgets what was pinned for `name`, so the next connection pins
    /// afresh: for a server whose identity was replaced on purpose.
    pub fn forget(&self, name: &str) -> anyhow::Result<()> {
        let Some((line, _)) = self.lookup(name)? else { return Ok(()) };
        let text = std::fs::read_to_string(&self.path)?;
        let out: String = text.lines().enumerate().filter(|(i, _)| i + 1 != line).map(|(_, l)| format!("{l}\n")).collect();
        std::fs::write(&self.path, out).with_context(|| format!("writing {}", self.path.display()))
    }

    fn line(name: &str, pin: &Pin) -> String {
        match pin {
            Pin::Tls(fp) => format!("{name} tls {fp}"),
            Pin::Plain => format!("{name} plain -"),
        }
    }

    fn append(&self, name: &str, pin: &Pin) -> anyhow::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("writing {}", self.path.display()))?;
        writeln!(f, "{}", Self::line(name, pin))?;
        Ok(())
    }

    fn replace(&self, line: usize, name: &str, pin: &Pin) -> anyhow::Result<()> {
        let text = std::fs::read_to_string(&self.path)?;
        let out: Vec<String> = text
            .lines()
            .enumerate()
            .map(|(i, l)| if i + 1 == line { Self::line(name, pin) } else { l.to_string() })
            .collect();
        std::fs::write(&self.path, out.join("\n") + "\n")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorized_lines_round_trip() {
        let key = [7u8; 32];
        let line = format_line(&key, "me@laptop");
        assert!(line.starts_with("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAI"));
        let file = format!("# comment\n\nssh-rsa AAAAB3Nza bad\n{line}\n");
        assert_eq!(parse_authorized(&file), vec![key]);
        assert!(parse_authorized("ssh-ed25519 !!!").is_empty());
    }

    #[test]
    fn known_hosts_pin_on_first_use() {
        let dir = std::env::temp_dir().join(format!("farsight-kh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let kh = KnownHosts::new(dir.join("known_hosts"));
        let a = Pin::Tls("aa".into());
        kh.check("host:7740", &a).unwrap();
        kh.check("host:7740", &a).unwrap();
        let err = kh.check("host:7740", &Pin::Tls("bb".into())).unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        assert!(kh.check("host:7740", &Pin::Plain).is_err());
        kh.check("other:7740", &Pin::Plain).unwrap();
        kh.check("other:7740", &Pin::Tls("cc".into())).unwrap();
        assert!(kh.check("other:7740", &Pin::Plain).is_err());
        kh.forget("host:7740").unwrap();
        kh.check("host:7740", &Pin::Tls("bb".into())).unwrap();
        kh.check("other:7740", &Pin::Tls("cc".into())).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
