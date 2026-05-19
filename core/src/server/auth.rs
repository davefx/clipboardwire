// SPDX-License-Identifier: GPL-3.0-or-later

//! HTTP Basic authentication for the WebSocket upgrade.
//!
//! The check is intentionally pure: it takes the raw `Authorization` header
//! value (or `None`) and the configured credentials, and returns a `bool`.
//! Wiring into axum lives in `ws.rs`.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use subtle::ConstantTimeEq;

/// Validate a `Basic` Authorization header value against configured creds.
///
/// Returns `true` only when:
/// 1. The header starts with `Basic `.
/// 2. The base64 payload decodes to a valid UTF-8 `user:password` pair.
/// 3. Both halves match the configured `user` and `password` in constant time.
///
/// All other inputs return `false`, including a missing header (`None`).
///
/// The username comparison is constant-time-given-equal-length; an attacker
/// who can flood the server with attempts may still learn `len(configured_user)`
/// via timing on the length mismatch path. This is an accepted leak for v0.1
/// (a single hard-coded username is rarely a secret).
pub fn check_basic_auth(header: Option<&str>, user: &str, password: &str) -> bool {
    let Some(header) = header else {
        return false;
    };
    let Some(b64) = header.strip_prefix("Basic ") else {
        return false;
    };
    let Ok(decoded) = STANDARD.decode(b64.trim()) else {
        return false;
    };
    let Ok(s) = std::str::from_utf8(&decoded) else {
        return false;
    };
    let Some((u, p)) = s.split_once(':') else {
        return false;
    };
    let u_ok = u.as_bytes().ct_eq(user.as_bytes());
    let p_ok = p.as_bytes().ct_eq(password.as_bytes());
    bool::from(u_ok & p_ok)
}

/// Build a `Basic <b64>` header value. Used by the client and tests.
pub fn basic_header_value(user: &str, password: &str) -> String {
    let pair = format!("{user}:{password}");
    format!("Basic {}", STANDARD.encode(pair.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_credentials() {
        let h = basic_header_value("alice", "hunter2");
        assert!(check_basic_auth(Some(&h), "alice", "hunter2"));
    }

    #[test]
    fn rejects_wrong_password() {
        let h = basic_header_value("alice", "wrong");
        assert!(!check_basic_auth(Some(&h), "alice", "hunter2"));
    }

    #[test]
    fn rejects_wrong_user() {
        let h = basic_header_value("bob", "hunter2");
        assert!(!check_basic_auth(Some(&h), "alice", "hunter2"));
    }

    #[test]
    fn rejects_missing_header() {
        assert!(!check_basic_auth(None, "alice", "hunter2"));
    }

    #[test]
    fn rejects_non_basic_scheme() {
        assert!(!check_basic_auth(Some("Bearer xyz"), "alice", "hunter2"));
    }

    #[test]
    fn rejects_invalid_base64() {
        assert!(!check_basic_auth(
            Some("Basic !!!not-base64!!!"),
            "alice",
            "hunter2"
        ));
    }

    #[test]
    fn rejects_payload_without_colon() {
        let h = format!("Basic {}", STANDARD.encode(b"alicewithoutcolon"));
        assert!(!check_basic_auth(Some(&h), "alice", "hunter2"));
    }

    #[test]
    fn rejects_non_utf8_payload() {
        let h = format!("Basic {}", STANDARD.encode([0xff, 0xfe, 0xfd]));
        assert!(!check_basic_auth(Some(&h), "alice", "hunter2"));
    }

    #[test]
    fn password_with_colon_is_handled() {
        // RFC 7617: the username is the part before the *first* colon; the
        // password is everything after. So a password may contain colons.
        let h = basic_header_value("alice", "a:b:c");
        assert!(check_basic_auth(Some(&h), "alice", "a:b:c"));
    }
}
