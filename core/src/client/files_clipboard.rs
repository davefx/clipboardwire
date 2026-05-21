// SPDX-License-Identifier: GPL-3.0-or-later

//! OS file-clipboard adapter: read/write the platform's "files
//! clipboard" target so a Ctrl+C in the file manager triggers a
//! clipboardwire send, and a Ctrl+V on the receiving side pastes
//! the files we just saved to disk.
//!
//! `arboard` (used elsewhere in [`crate::client::clipboard`] for
//! text + image) does not expose this target type. We provide a
//! per-OS implementation here:
//!
//! - **Linux X11:** [`x11-clipboard`] for the `text/uri-list`
//!   selection target. Stores/loads `file://…` URIs.
//! - **Windows:** [`clipboard-win`] for `CF_HDROP`.
//! - **macOS:** TODO (NSPasteboardTypeFileURL — covered by tests
//!   in CI's macos-latest job; the runtime impl is the next step).
//! - **Linux Wayland:** TODO (Xwayland covers a lot of cases via
//!   the X11 path; native `wl_data_device` is a follow-up).
//!
//! For platforms not yet supported, the read returns `None` and
//! the write is a no-op — the wire-level file transfer still
//! works via the explicit `clipboardwire send <FILE>` subcommand;
//! only the auto-pickup-from-clipboard UX is missing.

use std::path::PathBuf;

use anyhow::Result;

/// Read the OS clipboard's file list, if it currently holds files.
/// Returns `None` if the clipboard is empty, is holding a different
/// data type (text, image, …), or this platform doesn't have a
/// file-clipboard adapter yet.
pub fn read_files() -> Result<Option<Vec<PathBuf>>> {
    backend::read_files()
}

/// Set the OS clipboard to a list of file paths. After this call,
/// pasting in a file manager will paste these files (the OS knows
/// how to copy/move them on paste).
pub fn write_files(paths: &[PathBuf]) -> Result<()> {
    backend::write_files(paths)
}

#[cfg(target_os = "linux")]
mod backend {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    use std::time::Duration;

    use anyhow::{Context, Result};
    use x11_clipboard::Clipboard;

    /// One-shot wait for `load`. Polling cadence on the supervisor
    /// is already 300 ms by default; 200 ms here is a safe ceiling
    /// for "is the clipboard holding files."
    const LOAD_TIMEOUT: Duration = Duration::from_millis(200);

    /// Long-lived X11 clipboard handle. We *must* keep one
    /// `Clipboard` alive for the whole process lifetime — on X11
    /// the selection owner is identified by the underlying X
    /// connection, so dropping it releases the selection. Without
    /// this OnceLock, every `write_files` set the selection and
    /// then immediately gave it up, which made our paste support
    /// look broken even on a perfectly functional X session.
    fn clipboard() -> Result<&'static Mutex<Clipboard>> {
        static CB: OnceLock<Mutex<Clipboard>> = OnceLock::new();
        if let Some(c) = CB.get() {
            return Ok(c);
        }
        let cb = Clipboard::new().context("opening X11 clipboard")?;
        // race-tolerant set: if another thread won, drop ours.
        let _ = CB.set(Mutex::new(cb));
        Ok(CB.get().expect("OnceLock just set"))
    }

    pub fn read_files() -> Result<Option<Vec<PathBuf>>> {
        let cb = clipboard()?
            .lock()
            .map_err(|_| anyhow::anyhow!("X11 clipboard mutex poisoned"))?;
        let uri_list = cb
            .getter
            .get_atom("text/uri-list")
            .context("intern text/uri-list atom")?;

        match cb.load(
            cb.getter.atoms.clipboard,
            uri_list,
            cb.getter.atoms.property,
            LOAD_TIMEOUT,
        ) {
            Ok(bytes) => Ok(Some(parse_uri_list(&bytes))),
            // The clipboard owner refused / timed out / doesn't have
            // text/uri-list. That's the common case (clipboard holds
            // text or nothing) — translate to None, not an error.
            Err(_) => Ok(None),
        }
    }

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let cb = clipboard()?
            .lock()
            .map_err(|_| anyhow::anyhow!("X11 clipboard mutex poisoned"))?;
        let uri_list_atom = cb
            .setter
            .get_atom("text/uri-list")
            .context("intern text/uri-list atom for write")?;
        let body = render_uri_list(paths);
        cb.store(cb.setter.atoms.clipboard, uri_list_atom, body)
            .context("storing X11 text/uri-list selection")?;
        Ok(())
    }

    fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
        // RFC 2483: lines separated by CRLF; lines starting with '#'
        // are comments. In practice X11 file managers use \n too, so
        // we accept both line terminators.
        let text = String::from_utf8_lossy(bytes);
        text.split(['\n', '\r'])
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .filter_map(file_url_to_path)
            .collect()
    }

    fn file_url_to_path(uri: &str) -> Option<PathBuf> {
        let path = uri.strip_prefix("file://")?;
        // RFC 8089: a file URI is "file://" + host + path. The host
        // is usually empty or "localhost". We strip a leading host
        // segment if present.
        let path = if let Some(rest) = path.strip_prefix("localhost") {
            rest
        } else {
            // If the next segment isn't "/", treat as host:
            // file://host/foo — split on the first '/'.
            match path.find('/') {
                Some(0) => path,
                Some(idx) => &path[idx..],
                None => path,
            }
        };
        Some(PathBuf::from(percent_decode(path)))
    }

    /// Minimal `%XX` decoder — enough for the path segment of a
    /// `file://` URI, which is the only place we encounter it.
    fn percent_decode(s: &str) -> String {
        let mut out = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn from_hex(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    fn render_uri_list(paths: &[PathBuf]) -> Vec<u8> {
        let mut out = String::new();
        for p in paths {
            out.push_str("file://");
            out.push_str(&percent_encode_path(&p.to_string_lossy()));
            out.push_str("\r\n");
        }
        out.into_bytes()
    }

    fn percent_encode_path(s: &str) -> String {
        // Encode anything that's not unreserved + path separators.
        // file:// URIs allow `/` unescaped; we escape everything
        // else that isn't [A-Za-z0-9._~-] or a forward slash.
        let mut out = String::with_capacity(s.len());
        for &b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                    out.push(b as char)
                }
                _ => {
                    out.push('%');
                    out.push_str(&format!("{b:02X}"));
                }
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_a_two_line_uri_list() {
            let body = b"file:///home/alice/a.txt\r\nfile:///tmp/b%20c.pdf\r\n";
            let paths = parse_uri_list(body);
            assert_eq!(paths.len(), 2);
            assert_eq!(paths[0], PathBuf::from("/home/alice/a.txt"));
            assert_eq!(paths[1], PathBuf::from("/tmp/b c.pdf"));
        }

        #[test]
        fn ignores_comments_and_blank_lines() {
            let body = b"# RFC 2483 comment\nfile:///x\n\n";
            assert_eq!(parse_uri_list(body), vec![PathBuf::from("/x")]);
        }

        #[test]
        fn skips_non_file_schemes() {
            let body = b"https://example.com/\nfile:///wanted\n";
            assert_eq!(parse_uri_list(body), vec![PathBuf::from("/wanted")]);
        }

        #[test]
        fn round_trips_simple_paths() {
            let paths = vec![
                PathBuf::from("/etc/hosts"),
                PathBuf::from("/tmp/with space.txt"),
            ];
            let rendered = render_uri_list(&paths);
            let parsed = parse_uri_list(&rendered);
            assert_eq!(parsed, paths);
        }

        #[test]
        fn percent_encodes_spaces() {
            assert!(percent_encode_path("/a b").contains("%20"));
        }
    }
}

#[cfg(windows)]
mod backend {
    use std::path::PathBuf;

    use anyhow::{anyhow, Context, Result};
    use clipboard_win::formats::{FileList, CF_HDROP};
    use clipboard_win::raw::is_format_avail;
    use clipboard_win::{get_clipboard, Clipboard, Setter};

    pub fn read_files() -> Result<Option<Vec<PathBuf>>> {
        // Open + immediately scope the clipboard so the RAII handle
        // closes the Win32 clipboard before we return.
        let avail = {
            let _cb = Clipboard::new_attempts(10)
                .map_err(|e| anyhow!("opening Windows clipboard: code={e}"))?;
            is_format_avail(CF_HDROP)
        };
        if !avail {
            return Ok(None);
        }
        let paths: Vec<PathBuf> = get_clipboard(FileList)
            .map_err(|e| anyhow!("reading CF_HDROP: code={e}"))
            .context("reading CF_HDROP from clipboard")?;
        Ok(Some(paths))
    }

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        // FileList's Setter is impl'd for `[T: AsRef<str>]` (unsized),
        // so we can't go through clipboard_win::set_clipboard (which
        // expects a sized R). Open the clipboard explicitly and call
        // the Setter trait method directly.
        let as_strings: Vec<String> = paths
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let _cb = Clipboard::new_attempts(10)
            .map_err(|e| anyhow!("opening Windows clipboard for write: code={e}"))?;
        FileList
            .write_clipboard(&as_strings[..])
            .map_err(|e| anyhow!("writing CF_HDROP: code={e}"))
            .context("setting CF_HDROP on clipboard")?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use std::path::PathBuf;

    use anyhow::{anyhow, Context, Result};
    use objc2::rc::Retained;
    use objc2::AnyThread;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypeFileURL};
    use objc2_foundation::{NSArray, NSString, NSURL};

    pub fn read_files() -> Result<Option<Vec<PathBuf>>> {
        // SAFETY: NSPasteboard's `generalPasteboard` is thread-safe per
        // AppKit's documentation. We only borrow Retained handles
        // briefly to read primitive data; no UI work happens here.
        unsafe {
            let pasteboard = NSPasteboard::generalPasteboard();

            // Fast path: check whether any of the available types
            // is the file-URL UTI before iterating items.
            let Some(types) = pasteboard.types() else {
                return Ok(None);
            };
            // `types` is NSArray<NSPasteboardType> (= NSString).
            // NSPasteboardTypeFileURL is `&'static NSPasteboardType`,
            // so we deref once to reach the NSString and compare via
            // Rust string contents (NSString implements Display via
            // a single deref, but two derefs walks past NSString to
            // NSObject which doesn't implement Display).
            let target = NSPasteboardTypeFileURL.to_string();
            let mut has_file_url = false;
            for t in types.iter() {
                if t.to_string() == target {
                    has_file_url = true;
                    break;
                }
            }
            if !has_file_url {
                return Ok(None);
            }

            let Some(items) = pasteboard.pasteboardItems() else {
                return Ok(None);
            };
            let mut paths = Vec::new();
            for item in items.iter() {
                let Some(url_str) = item.stringForType(NSPasteboardTypeFileURL) else {
                    continue;
                };
                // url_str is e.g. "file:///Users/alice/foo.pdf"
                let Some(url) = NSURL::URLWithString(&url_str) else {
                    continue;
                };
                let Some(path) = url.path() else { continue };
                paths.push(PathBuf::from(path.to_string()));
            }
            if paths.is_empty() {
                Ok(None)
            } else {
                Ok(Some(paths))
            }
        }
    }

    pub fn write_files(paths: &[PathBuf]) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        unsafe {
            let pasteboard = NSPasteboard::generalPasteboard();
            // Clear before writing so we own the pasteboard contents
            // entirely (NSPasteboard mixes additive vs replace
            // semantics; we want replace).
            pasteboard.clearContents();

            // Build [NSURL] and writeObjects.
            let urls: Vec<Retained<NSURL>> = paths
                .iter()
                .map(|p| {
                    let s = NSString::from_str(&p.to_string_lossy());
                    NSURL::fileURLWithPath(&s)
                })
                .collect();
            // NSArray<NSURL> from Vec<Retained<NSURL>>.
            let url_refs: Vec<&NSURL> = urls.iter().map(|u| &**u).collect();
            let array: Retained<NSArray<NSURL>> = NSArray::from_slice(&url_refs);

            // `writeObjects` takes ProtocolObject<dyn NSPasteboardWriting>.
            // NSURL conforms; we cast via the typed-array wrapper.
            // SAFETY: NSURL implements NSPasteboardWriting since 10.6.
            let writable: &NSArray<
                objc2::runtime::ProtocolObject<dyn objc2_app_kit::NSPasteboardWriting>,
            > = std::mem::transmute(&*array);
            let ok = pasteboard.writeObjects(writable);
            if !ok {
                return Err(anyhow!("NSPasteboard.writeObjects returned NO"))
                    .context("setting file URLs on NSPasteboard");
            }
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod backend {
    use anyhow::Result;
    use std::path::PathBuf;
    pub fn read_files() -> Result<Option<Vec<PathBuf>>> {
        Ok(None)
    }
    pub fn write_files(_paths: &[PathBuf]) -> Result<()> {
        Ok(())
    }
}
