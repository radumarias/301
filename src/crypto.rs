//! WebCrypto helpers: random bytes, PBKDF2 password hashing, RS256 verification.

use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{CryptoKey, SubtleCrypto};

pub const PBKDF2_ITERATIONS: u32 = 10_000;

fn err(e: JsValue) -> worker::Error {
    worker::Error::JsError(format!("{e:?}"))
}

fn subtle() -> worker::Result<SubtleCrypto> {
    let crypto = Reflect::get(&js_sys::global(), &JsValue::from_str("crypto")).map_err(err)?;
    let crypto: web_sys::Crypto = crypto.unchecked_into();
    Ok(crypto.subtle())
}

/// Cryptographically secure random bytes.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).expect("getrandom");
    buf
}

fn obj(pairs: &[(&str, JsValue)]) -> worker::Result<Object> {
    let o = Object::new();
    for (k, v) in pairs {
        Reflect::set(&o, &JsValue::from_str(k), v).map_err(err)?;
    }
    Ok(o)
}

async fn pbkdf2(password: &str, salt: &[u8], iterations: u32) -> worker::Result<Vec<u8>> {
    let s = subtle()?;
    let raw = Uint8Array::from(password.as_bytes());
    let usages = Array::of1(&JsValue::from_str("deriveBits"));
    let key = JsFuture::from(
        s.import_key_with_str("raw", &raw, "PBKDF2", false, &usages)
            .map_err(err)?,
    )
    .await
    .map_err(err)?;
    let key: CryptoKey = key.unchecked_into();
    let algo = obj(&[
        ("name", "PBKDF2".into()),
        ("hash", "SHA-256".into()),
        ("salt", Uint8Array::from(salt).into()),
        ("iterations", iterations.into()),
    ])?;
    let bits = JsFuture::from(s.derive_bits_with_object(&algo, &key, 256).map_err(err)?)
        .await
        .map_err(err)?;
    Ok(Uint8Array::new(&bits).to_vec())
}

/// Hashes a link password as `pbkdf2$<iterations>$<salt b64>$<hash b64>`.
pub async fn hash_password(password: &str) -> worker::Result<String> {
    let salt = random_bytes(16);
    let hash = pbkdf2(password, &salt, PBKDF2_ITERATIONS).await?;
    Ok(format!(
        "pbkdf2${}${}${}",
        PBKDF2_ITERATIONS,
        STANDARD.encode(&salt),
        STANDARD.encode(&hash)
    ))
}

/// Checks a password against a stored hash in constant time.
pub async fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "pbkdf2" {
        return false;
    }
    let (Ok(iter), Ok(salt), Ok(expected)) = (
        parts[1].parse::<u32>(),
        STANDARD.decode(parts[2]),
        STANDARD.decode(parts[3]),
    ) else {
        return false;
    };
    match pbkdf2(password, &salt, iter.min(100_000)).await {
        Ok(actual) => constant_time_eq(&actual, &expected),
        Err(_) => false,
    }
}

/// HMAC-SHA256 of `msg` with `key`.
pub async fn hmac_sha256(key: &[u8], msg: &[u8]) -> worker::Result<Vec<u8>> {
    let s = subtle()?;
    let algo = obj(&[("name", "HMAC".into()), ("hash", "SHA-256".into())])?;
    let usages = Array::of1(&JsValue::from_str("sign"));
    let key = JsFuture::from(
        s.import_key_with_object("raw", &Uint8Array::from(key), &algo, false, &usages)
            .map_err(err)?,
    )
    .await
    .map_err(err)?;
    let key: CryptoKey = key.unchecked_into();
    let sig = JsFuture::from(
        s.sign_with_str_and_u8_array("HMAC", &key, msg)
            .map_err(err)?,
    )
    .await
    .map_err(err)?;
    Ok(Uint8Array::new(&sig).to_vec())
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// Verifies an RS256 signature with a JWK public key.
pub async fn verify_rs256(
    jwk: &serde_json::Value,
    signed: &[u8],
    sig: &[u8],
) -> worker::Result<bool> {
    let s = subtle()?;
    let jwk_js: JsValue = js_sys::JSON::parse(&jwk.to_string()).map_err(err)?;
    let algo = obj(&[
        ("name", "RSASSA-PKCS1-v1_5".into()),
        ("hash", "SHA-256".into()),
    ])?;
    let usages = Array::of1(&JsValue::from_str("verify"));
    let key = JsFuture::from(
        s.import_key_with_object("jwk", jwk_js.unchecked_ref(), &algo, false, &usages)
            .map_err(err)?,
    )
    .await
    .map_err(err)?;
    let key: CryptoKey = key.unchecked_into();
    let ok = JsFuture::from(
        s.verify_with_object_and_u8_array_and_u8_array(&algo, &key, sig, signed)
            .map_err(err)?,
    )
    .await
    .map_err(err)?;
    Ok(ok.as_bool().unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn b64url() {
        assert_eq!(b64url_decode("aGk").unwrap(), b"hi");
        assert_eq!(b64url_decode("aGk=").unwrap(), b"hi");
    }
}
