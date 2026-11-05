//! HTTP Digest authentication (RFC 7616) — slice 7.
//!
//! The engine's other auth is a prebuilt header (Basic from the
//! caller's login/password). Digest cannot be prebuilt: the
//! response hash binds the server's nonce, the method and the
//! request URI. This module parses a `WWW-Authenticate: Digest`
//! challenge and computes the `Authorization` answer for one
//! request. `jar_call` owns the exchange: on a 401 carrying a
//! challenge it stores the challenge in the job's shared
//! DigestState and retries once; later requests to the same
//! host answer preemptively with a fresh nonce count.
//!
//! Supported: MD5 / MD5-sess / SHA-256 / SHA-256-sess, qop=auth
//! and the legacy no-qop form. qop=auth-int is declined (it
//! hashes the request body; engine requests have none worth
//! hashing, and a server offering ONLY auth-int gets a clean
//! auth failure instead of a wrong answer).

use md5::{Digest as _, Md5};
use sha2::Sha256;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone)]
pub struct Challenge {
    pub realm: String,
    pub nonce: String,
    pub qop: Option<String>, // the raw offered list, e.g. "auth,auth-int"
    pub algorithm: String,   // as named by the server, default MD5
    pub opaque: Option<String>,
    pub stale: bool,
}

/// The job-shared digest session: the last challenge seen for
/// one origin plus the nonce count. Lives inside `Auth` behind
/// a mutex; every request clones the challenge and takes the
/// next count.
#[derive(Debug)]
pub struct DigestState {
    pub challenge: Challenge,
    pub host: String,
    pub nc: AtomicU64,
}

impl DigestState {
    pub fn new(challenge: Challenge, host: String) -> Self {
        DigestState {
            challenge,
            host,
            nc: AtomicU64::new(0),
        }
    }
}

fn hash(algorithm: &str, parts: &[&str]) -> Option<String> {
    let data = parts.join(":");
    match algorithm.to_ascii_uppercase().as_str() {
        "MD5" | "MD5-SESS" => {
            let mut h = Md5::new();
            h.update(data.as_bytes());
            Some(format!("{:x}", h.finalize()))
        }
        "SHA-256" | "SHA-256-SESS" => {
            let mut h = Sha256::new();
            h.update(data.as_bytes());
            Some(format!("{:x}", h.finalize()))
        }
        _ => None,
    }
}

fn is_sess(algorithm: &str) -> bool {
    algorithm.to_ascii_uppercase().ends_with("-SESS")
}

/// Parse one `WWW-Authenticate` header value. Returns None when
/// it is not a Digest challenge or lacks the mandatory fields.
pub fn parse_challenge(header: &str) -> Option<Challenge> {
    let rest = header.trim();
    let rest = rest
        .strip_prefix("Digest")
        .or_else(|| rest.strip_prefix("digest"))?;
    let mut realm = None;
    let mut nonce = None;
    let mut qop = None;
    let mut algorithm = "MD5".to_string();
    let mut opaque = None;
    let mut stale = false;
    for pair in split_params(rest) {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = unquote(v.trim());
        match k.as_str() {
            "realm" => realm = Some(v),
            "nonce" => nonce = Some(v),
            "qop" => qop = Some(v),
            "algorithm" => algorithm = v,
            "opaque" => opaque = Some(v),
            "stale" => stale = v.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    Some(Challenge {
        realm: realm?,
        nonce: nonce?,
        qop,
        algorithm,
        opaque,
        stale,
    })
}

/// Split a challenge parameter list on commas that are not
/// inside a quoted string.
fn split_params(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
        } else if c == '\\' && quoted {
            cur.push(c);
            escaped = true;
        } else if c == '"' {
            quoted = !quoted;
            cur.push(c);
        } else if c == ',' && !quoted {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        let inner = &v[1..v.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut escaped = false;
        for c in inner.chars() {
            if escaped {
                out.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else {
                out.push(c);
            }
        }
        out
    } else {
        v.to_string()
    }
}

fn quote(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('"');
    for c in v.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// A deterministic cnonce: derived from the nonce and count so
/// tests can pin the RFC vectors; in production the nonce is
/// the server's random value, so the derivation is unique per
/// session and count.
fn cnonce_for(nonce: &str, nc: u64) -> String {
    let mut h = Md5::new();
    h.update(format!("{nonce}:{nc}:ndm").as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

impl Challenge {
    /// The qop this client will use: "auth" when offered, None
    /// for the legacy form, and Err when only auth-int is on
    /// the table.
    fn chosen_qop(&self) -> Result<Option<&str>, ()> {
        match &self.qop {
            None => Ok(None),
            Some(list) => {
                let offered: Vec<&str> = list.split(',').map(|q| q.trim()).collect();
                if offered.contains(&"auth") {
                    Ok(Some("auth"))
                } else {
                    Err(())
                }
            }
        }
    }

    /// Compute the full `Authorization` header value for one
    /// request. `nc` is this request's nonce count (used only
    /// in the qop form).
    pub fn authorize(
        &self,
        username: &str,
        password: &str,
        method: &str,
        uri: &str,
        nc: u64,
    ) -> Option<String> {
        let qop = self.chosen_qop().ok()?;
        let cnonce = cnonce_for(&self.nonce, nc);
        let mut ha1 = hash(&self.algorithm, &[username, &self.realm, password])?;
        if is_sess(&self.algorithm) {
            ha1 = hash(&self.algorithm, &[&ha1, &self.nonce, &cnonce])?;
        }
        let ha2 = hash(&self.algorithm, &[method, uri])?;
        let nc_str = format!("{nc:08x}");
        let response = match qop {
            Some(q) => hash(
                &self.algorithm,
                &[&ha1, &self.nonce, &nc_str, &cnonce, q, &ha2],
            )?,
            None => hash(&self.algorithm, &[&ha1, &self.nonce, &ha2])?,
        };
        let mut out = format!(
            "Digest username={}, realm={}, nonce={}, uri={}, response={}",
            quote(username),
            quote(&self.realm),
            quote(&self.nonce),
            quote(uri),
            quote(&response),
        );
        if let Some(o) = &self.opaque {
            out += &format!(", opaque={}", quote(o));
        }
        if let Some(q) = qop {
            out += &format!(", qop={q}, nc={nc_str}, cnonce={}", quote(&cnonce));
        }
        out += &format!(", algorithm={}", self.algorithm);
        Some(out)
    }
}

impl DigestState {
    /// The Authorization header for the next request to `uri`,
    /// with the nonce count advanced.
    pub fn header_for(&self, username: &str, password: &str, method: &str, uri: &str) -> Option<String> {
        let nc = self.nc.fetch_add(1, Ordering::Relaxed) + 1;
        self.challenge
            .authorize(username, password, method, uri, nc)
    }
}

/// The origin-form URI Digest signs: path + "?" + query, "/"
/// when the URL has no path.
pub fn uri_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    match after_scheme.find('/') {
        Some(i) => {
            let p = &after_scheme[i..];
            let p = p.split('#').next().unwrap_or(p);
            if p.is_empty() {
                "/".to_string()
            } else {
                p.to_string()
            }
        }
        None => "/".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 7616 §3.9.1 example, MD5.
    #[test]
    fn rfc7616_md5_vector() {
        let c = parse_challenge(
            "Digest realm=\"testrealm@host.com\", qop=\"auth,auth-int\", \
             nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", \
             opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"",
        )
        .unwrap();
        // the RFC computes with cnonce 0a4f113b; our cnonce is
        // derived, so verify the digest MATH directly instead:
        let ha1 = hash("MD5", &["Mufasa", "testrealm@host.com", "Circle Of Life"]).unwrap();
        assert_eq!(ha1, "939e7578ed9e3c518a452acee763bce9");
        let ha2 = hash("MD5", &["GET", "/dir/index.html"]).unwrap();
        assert_eq!(ha2, "39aff3a2bab6126f332b942af96d3366");
        let resp = hash(
            "MD5",
            &[
                &ha1,
                "dcd98b7102dd2f0e8b11d0f600bfb0c093",
                "00000001",
                "0a4f113b",
                "auth",
                &ha2,
            ],
        )
        .unwrap();
        assert_eq!(resp, "6629fae49393a05397450978507c4ef1");
        // and the assembled header carries every field
        let h = c
            .authorize("Mufasa", "Circle Of Life", "GET", "/dir/index.html", 1)
            .unwrap();
        assert!(h.starts_with("Digest username=\"Mufasa\""));
        assert!(h.contains("qop=auth, nc=00000001"));
        assert!(h.contains("opaque=\"5ccc069c403ebaf9f0171e9517f40e41\""));
    }

    #[test]
    fn rfc7616_sha256_math() {
        let ha1 = hash(
            "SHA-256",
            &["Mufasa", "testrealm@host.com", "Circle Of Life"],
        )
        .unwrap();
        let ha2 = hash("SHA-256", &["GET", "/dir/index.html"]).unwrap();
        let resp = hash(
            "SHA-256",
            &[
                &ha1,
                "dcd98b7102dd2f0e8b11d0f600bfb0c093",
                "00000001",
                "0a4f113b",
                "auth",
                &ha2,
            ],
        )
        .unwrap();
        // cross-verified against Python hashlib on the same
        // RFC inputs (ha1 3ba6cd94…, ha2 9a3fdae9…)
        assert_eq!(
            resp,
            "5abdd07184ba512a22c53f41470e5eea7dcaa3a93a59b630c13dfe0a5dc6e38b"
        );
    }

    #[test]
    fn legacy_no_qop_and_auth_int_only() {
        let legacy = parse_challenge("Digest realm=\"r\", nonce=\"n\"").unwrap();
        let h = legacy.authorize("u", "p", "GET", "/f", 1).unwrap();
        assert!(!h.contains("qop="), "legacy form carries no qop: {h}");
        let int_only =
            parse_challenge("Digest realm=\"r\", nonce=\"n\", qop=\"auth-int\"").unwrap();
        assert!(int_only.authorize("u", "p", "GET", "/f", 1).is_none());
    }

    #[test]
    fn parse_and_uri() {
        assert!(parse_challenge("Basic realm=\"x\"").is_none());
        let c = parse_challenge(
            "Digest realm=\"a,b\", nonce=\"xyz\", algorithm=SHA-256, stale=true",
        )
        .unwrap();
        assert_eq!(c.realm, "a,b");
        assert_eq!(c.algorithm, "SHA-256");
        assert!(c.stale);
        assert_eq!(uri_of("http://h:8080/a/b?x=1#frag"), "/a/b?x=1");
        assert_eq!(uri_of("http://h"), "/");
    }
}
