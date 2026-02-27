//! UAC elevation via ShellExecuteW with "runas" verb.

/// Re-launch the current executable as Administrator with `--install-wsl` flag.
/// Returns Ok(()) immediately after spawning the elevated process.
pub fn relaunch_as_admin() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;

    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::UI::WindowsAndMessaging::ShellExecuteW;

        let verb: Vec<u16> = OsStr::new("runas\0").encode_wide().collect();
        let file: Vec<u16> = exe.as_os_str().encode_wide().chain(Some(0)).collect();
        let params: Vec<u16> = OsStr::new("--install-wsl\0").encode_wide().collect();

        let result = unsafe {
            ShellExecuteW(
                0isize,
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                1, // SW_SHOWNORMAL
            )
        };

        if result as usize <= 32 {
            return Err(std::io::Error::last_os_error());
        }
    }

    #[cfg(not(windows))]
    {
        let _ = exe;
    }

    Ok(())
}

/// Install WSL2 (must be called with admin privileges).
/// Runs `wsl --install --no-launch` and returns the output.
pub fn install_wsl() -> std::io::Result<std::process::Output> {
    std::process::Command::new("wsl")
        .args(["--install", "--no-launch"])
        .output()
}
