// SPDX-License-Identifier: GPL-3.0-or-later

//! Wire protocol types. See `PROTOCOL.md` for the full spec.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Value advertised by the server in the `welcome` frame.
pub const PROTOCOL_VERSION: &str = "clipboardwire/0.1.0";

/// Maximum WebSocket frame size accepted by the server (10 MiB).
pub const MAX_FRAME_BYTES: usize = 10 * 1024 * 1024;

/// The only `content_type` accepted in v0.1.
pub const SUPPORTED_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// `clip` frame body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipFrame {
    pub id: Uuid,
    pub ts: i64,
    pub content_type: String,
    pub content: String,
    /// Filled by the server on relay; absent on the client's outbound send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Uuid>,
}

/// `welcome` frame body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WelcomeFrame {
    pub server: String,
    pub client_id: Uuid,
    pub last_clip: Option<ClipFrame>,
}

/// `error` frame body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorFrame {
    pub code: ErrorCode,
    pub message: String,
}

/// Protocol-level error codes — also the `name` column of close codes 4001–4005.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Unauthorized,
    BadFrame,
    FrameTooLarge,
    UnsupportedType,
    DuplicateSession,
}

impl ErrorCode {
    /// WebSocket close code that pairs with this error (per `PROTOCOL.md` §6).
    pub fn close_code(self) -> u16 {
        match self {
            Self::Unauthorized => 4001,
            Self::BadFrame => 4002,
            Self::FrameTooLarge => 4003,
            Self::UnsupportedType => 4004,
            Self::DuplicateSession => 4005,
        }
    }
}

/// Any frame on the wire. `#[serde(tag = "type")]` flattens the variant fields
/// alongside a `"type": "<variant>"` discriminator, matching `PROTOCOL.md` §3.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Frame {
    Clip(ClipFrame),
    Welcome(WelcomeFrame),
    Error(ErrorFrame),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_frame_roundtrip() {
        let id = Uuid::new_v4();
        let from = Uuid::new_v4();
        let f = Frame::Clip(ClipFrame {
            id,
            ts: 1_716_163_200_000,
            content_type: SUPPORTED_CONTENT_TYPE.to_string(),
            content: "hello world".to_string(),
            from: Some(from),
        });
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.contains(r#""type":"clip""#));
        assert!(s.contains(r#""content":"hello world""#));
        let back: Frame = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn welcome_frame_serializes_with_type_tag() {
        let f = Frame::Welcome(WelcomeFrame {
            server: PROTOCOL_VERSION.to_string(),
            client_id: Uuid::nil(),
            last_clip: None,
        });
        let s = serde_json::to_string(&f).unwrap();
        assert!(s.starts_with(r#"{"type":"welcome""#));
        assert!(s.contains(r#""last_clip":null"#));
    }

    #[test]
    fn error_codes_have_documented_close_codes() {
        assert_eq!(ErrorCode::Unauthorized.close_code(), 4001);
        assert_eq!(ErrorCode::BadFrame.close_code(), 4002);
        assert_eq!(ErrorCode::FrameTooLarge.close_code(), 4003);
        assert_eq!(ErrorCode::UnsupportedType.close_code(), 4004);
        assert_eq!(ErrorCode::DuplicateSession.close_code(), 4005);
    }

    #[test]
    fn unknown_type_fails_to_deserialize() {
        // §3: unknown frame types are ignored. We let serde reject them; the
        // ws handler will dispatch on the parse failure (see ws.rs).
        let s = r#"{"type":"future_frame","foo":1}"#;
        let r: Result<Frame, _> = serde_json::from_str(s);
        assert!(r.is_err());
    }

    #[test]
    fn clip_frame_from_field_is_omitted_when_none() {
        let f = ClipFrame {
            id: Uuid::nil(),
            ts: 0,
            content_type: SUPPORTED_CONTENT_TYPE.to_string(),
            content: String::new(),
            from: None,
        };
        let s = serde_json::to_string(&f).unwrap();
        assert!(!s.contains("from"));
    }
}
