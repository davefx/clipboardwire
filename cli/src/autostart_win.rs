// SPDX-License-Identifier: GPL-3.0-or-later

//! Windows auto-start: read and toggle the per-user registry entry that
//! makes clipboardwire launch at login.
//!
//! The key is `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
//! The MSI installer writes the same key via the `AutoStart` WiX
//! feature; this module lets the tray icon keep the key in sync
//! regardless of how the binary was installed.

use anyhow::{Context, Result};
use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP_NAME: &str = "clipboardwire";

/// Returns `true` when the current-user autostart registry value exists.
pub fn is_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_READ) else {
        return false;
    };
    key.get_value::<String, _>(APP_NAME).is_ok()
}

/// Write (or overwrite) the HKCU Run entry so clipboardwire starts at login.
///
/// The value is `"<path-to-current-exe>" connect --tray`, quoted so that
/// paths with spaces (e.g. `C:\Program Files\…`) work correctly.
pub fn enable() -> Result<()> {
    let exe = std::env::current_exe().context("locating current executable")?;
    let value = format!(r#""{}" connect --tray"#, exe.display());

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu
        .create_subkey(RUN_KEY)
        .context("opening HKCU Run registry key")?;
    key.set_value(APP_NAME, &value)
        .context("writing autostart registry entry")
}

/// Delete the HKCU Run entry so clipboardwire no longer starts at login.
/// No-op if the entry does not exist.
pub fn disable() -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let Ok(key) = hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE) else {
        return Ok(()); // key doesn't exist; nothing to delete
    };
    match key.delete_value(APP_NAME) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow::anyhow!(e)).context("deleting autostart registry entry"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: enable → is_enabled == true → disable → is_enabled == false.
    ///
    /// This test writes to the real HKCU registry on the CI runner, but the
    /// key (`HKCU\…\Run\clipboardwire`) is cleaned up in the same test, so
    /// there are no lasting side-effects.
    #[test]
    fn enable_disable_round_trip() {
        // Start from a known clean state.
        disable().expect("initial disable should succeed");
        assert!(!is_enabled(), "should not be enabled before the test");

        enable().expect("enable should succeed");
        assert!(is_enabled(), "should be enabled after enable()");

        disable().expect("disable should succeed");
        assert!(!is_enabled(), "should not be enabled after disable()");
    }
}
