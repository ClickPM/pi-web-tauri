//! One place to say "this child must not flash a console window".
//!
//! Every process this shell spawns is a background helper — the node server,
//! the runtime guard, robocopy, taskkill — and none of them should ever put a
//! console on screen. Without `CREATE_NO_WINDOW` Windows gives a console
//! application its own window, which appears as a black box that flashes for a
//! fraction of a second.
//!
//! This started as the same two lines copy-pasted at each spawn site, and the
//! inevitable happened: the `taskkill` in `Server::stop` was missed, so closing
//! the app flashed a console every time. A trait makes the omission visible —
//! a `Command` that is spawned without `.no_console()` now reads as wrong.

use std::process::Command;

/// `CREATE_NO_WINDOW` — see the Win32 process creation flags.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub trait NoConsole {
    /// Run this child without giving it a console window.
    fn no_console(&mut self) -> &mut Self;
}

impl NoConsole for Command {
    fn no_console(&mut self) -> &mut Self {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            self.creation_flags(CREATE_NO_WINDOW);
        }
        self
    }
}
