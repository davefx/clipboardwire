// SPDX-License-Identifier: GPL-3.0-or-later

//! clipboardwire core library.
//!
//! Wire protocol types, hub server, and clipboard client supervisor live here.
//! The `clipboardwire` binary in `cli/` is a thin clap wrapper around these.
//!
//! See `PROTOCOL.md` and `ARCHITECTURE.md` in the repository root.

pub mod protocol;
pub mod server;
