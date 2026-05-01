//! Minimal Unix terminal helpers for interactive PTY mode.

use std::io;
use std::sync::{Mutex, MutexGuard, OnceLock};

static ORIGINAL_TERMIOS: OnceLock<Mutex<Option<libc::termios>>> = OnceLock::new();

fn original_termios() -> &'static Mutex<Option<libc::termios>> {
    ORIGINAL_TERMIOS.get_or_init(|| Mutex::new(None))
}

fn lock_original_termios() -> MutexGuard<'static, Option<libc::termios>> {
    match original_termios().lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            eprintln!("warning: recovered poisoned terminal state lock");
            poisoned.into_inner()
        }
    }
}

/// RAII guard that restores terminal mode when dropped.
pub struct RawModeGuard {
    active: bool,
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            if let Err(err) = disable_raw_mode() {
                eprintln!("warning: failed to restore terminal mode: {err}");
            }
            self.active = false;
        }
    }
}

/// Return the terminal size as `(cols, rows)`.
pub fn size() -> io::Result<(u16, u16)> {
    let mut ws = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, ws.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    let ws = unsafe { ws.assume_init() };
    if ws.ws_col == 0 || ws.ws_row == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "terminal size is zero",
        ));
    }
    Ok((ws.ws_col, ws.ws_row))
}

/// Enable raw mode and return a guard that restores the previous mode on drop.
pub fn raw_mode() -> io::Result<RawModeGuard> {
    enable_raw_mode()?;
    Ok(RawModeGuard { active: true })
}

/// Enable raw mode on stdin, saving the previous terminal settings.
pub fn enable_raw_mode() -> io::Result<()> {
    let mut current = std::mem::MaybeUninit::<libc::termios>::uninit();
    let ret = unsafe { libc::tcgetattr(libc::STDIN_FILENO, current.as_mut_ptr()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    let current = unsafe { current.assume_init() };
    let mut raw = current;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }

    let ret = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    let mut saved = lock_original_termios();
    if saved.is_none() {
        *saved = Some(current);
    }

    Ok(())
}

/// Restore terminal mode saved by [`enable_raw_mode`].
pub fn disable_raw_mode() -> io::Result<()> {
    let mut saved = lock_original_termios();

    let Some(original) = saved.take() else {
        return Ok(());
    };

    let ret = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &original) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}
