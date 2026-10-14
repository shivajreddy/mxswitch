//! mxswitch – move a Logitech Easy-Switch mouse to the other PC with a hotkey.
//!
//! With no arguments it lives in the tray and listens for the hotkey (F21 by default).
//! With arguments it acts as a command-line tool (`mxswitch --help`).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("mxswitch only runs on Windows: it talks to the mouse through the Windows HID API.");

#[cfg(windows)]
mod cli;
#[cfg(windows)]
mod hidpp;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    cli::run();
}

#[cfg(not(windows))]
fn main() {}
