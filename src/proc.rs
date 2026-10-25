//! Console-window policy for every child process.
//!
//! A console child started **without** `CREATE_NO_WINDOW` makes Windows open a
//! console for it — and with Windows Terminal as the default terminal that is a
//! Terminal window that appears and vanishes: the "a command prompt flashes when I
//! open /api/tunnel/status" report. `storage/tunnel` used to shell out to
//! `netstat`/`tasklist` on every status poll, and each call flashed a window.
//!
//! So this module is the ONE place that knows how to spawn a helper quietly, and
//! every `Command` in the binary should come from here. Verified signal: a normal
//! console child produces two visible windows (Terminal + PseudoConsole), a
//! `CREATE_NO_WINDOW` child produces none — see `tools/probe_console_flash.ps1`.

use std::process::Command;

/// `CREATE_NO_WINDOW`: the child gets no console window at all.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `CREATE_NEW_PROCESS_GROUP`: console control events (Ctrl-C in the server's own
/// terminal) cannot reach the child.
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// A `Command` that can never flash a console window. Use this instead of
/// `Command::new` for anything that runs on Windows.
pub fn cmd(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut c = Command::new(program);
    hide(&mut c);
    c
}

/// Apply `CREATE_NO_WINDOW` to a `Command` built some other way (no-op off
/// Windows). Note `creation_flags` REPLACES the flag word, so a caller that also
/// wants `CREATE_NEW_PROCESS_GROUP` must pass both in one call.
pub fn hide(c: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = c;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_match_the_win32_constants() {
        // Pinned so a typo cannot silently re-enable the flashing console.
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
        assert_eq!(CREATE_NEW_PROCESS_GROUP, 0x0000_0200);
    }

    #[test]
    fn cmd_runs_a_program_off_windows() {
        // On every host this builds a Command; the point of the test is that the
        // helper is usable as a drop-in for Command::new.
        let mut c = cmd("echo");
        c.arg("ok");
        #[cfg(unix)]
        {
            let out = c.output().expect("echo must run");
            assert!(out.status.success());
        }
    }
}
