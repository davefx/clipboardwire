// SPDX-License-Identifier: GPL-3.0-or-later

//! Process-wide X11 CLIPBOARD owner, shared by the file-list adapter
//! ([`super::files_clipboard`]) and the text writer in
//! [`super::clipboard`].
//!
//! Text goes through here instead of arboard on X11 because arboard's
//! X11 backend advertises `text/plain;charset=utf-8` and
//! `text/plain;charset=UTF-8` but only serves `UTF8_STRING`. On
//! GNOME/Wayland those advertised-but-broken targets reach Wayland apps
//! as MIME types, and Firefox picks one and pastes nothing.
//! `x11-clipboard` advertises just `UTF8_STRING`, like `xclip`, and the
//! compositor derives the Wayland text types from it.

use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use x11_clipboard::Clipboard;

/// Long-lived X11 clipboard handle. We *must* keep one `Clipboard` alive
/// for the whole process lifetime: on X11 the selection owner is
/// identified by the underlying X connection, so dropping it releases
/// the selection and the data with it.
pub(crate) fn clipboard() -> Result<&'static Mutex<Clipboard>> {
    static CB: OnceLock<Mutex<Clipboard>> = OnceLock::new();
    if let Some(c) = CB.get() {
        return Ok(c);
    }
    let cb = Clipboard::new().context("opening X11 clipboard")?;
    // race-tolerant set: if another thread won, drop ours.
    let _ = CB.set(Mutex::new(cb));
    Ok(CB.get().expect("OnceLock just set"))
}

/// Take ownership of CLIPBOARD with `text` as `UTF8_STRING`.
pub(crate) fn store_text(text: &str) -> Result<()> {
    let cb = clipboard()?
        .lock()
        .map_err(|_| anyhow::anyhow!("X11 clipboard mutex poisoned"))?;
    cb.store(
        cb.setter.atoms.clipboard,
        cb.setter.atoms.utf8_string,
        text.as_bytes(),
    )
    .context("storing X11 UTF8_STRING selection")
}
