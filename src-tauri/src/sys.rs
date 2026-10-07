//! Everything that has to touch the machine: the WinINET proxy settings and
//! finding/starting Discord. All of it is Windows-only; other targets get stubs
//! so the crate still type-checks.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use winreg::enums::HKEY_CURRENT_USER;
    use winreg::RegKey;

    use crate::store::SavedSysProxy;

    const INTERNET_SETTINGS: &str =
        r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    #[link(name = "wininet")]
    extern "system" {
        fn InternetSetOptionW(
            handle: isize,
            option: u32,
            buffer: *mut c_void,
            length: u32,
        ) -> i32;
    }

    #[link(name = "advapi32")]
    extern "system" {
        // RtlGenRandom: the only easy cryptographically-strong RNG reachable
        // from a stable ABI without pulling in a crate.
        fn SystemFunction036(buffer: *mut c_void, length: u32) -> u8;
    }

    #[link(name = "shell32")]
    extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> i32;
    }

    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        f_mask: u32,
        hwnd: isize,
        lp_verb: *const u16,
        lp_file: *const u16,
        lp_parameters: *const u16,
        lp_directory: *const u16,
        n_show: i32,
        h_inst_app: isize,
        lp_id_list: *mut c_void,
        lp_class: *const u16,
        hkey_class: isize,
        dw_hot_key: u32,
        h_icon_or_monitor: isize,
        h_process: isize,
    }

    #[link(name = "user32")]
    extern "system" {
        fn SendMessageTimeoutW(
            window: isize,
            message: u32,
            wparam: usize,
            lparam: *const u16,
            flags: u32,
            timeout: u32,
            result: *mut usize,
        ) -> isize;
    }

    /// Poke WinINET and broadcast WM_SETTINGCHANGE so Chromium re-reads the settings.
    fn notify_change() {
        unsafe {
            // INTERNET_OPTION_SETTINGS_CHANGED / INTERNET_OPTION_REFRESH
            InternetSetOptionW(0, 39, std::ptr::null_mut(), 0);
            InternetSetOptionW(0, 51, std::ptr::null_mut(), 0);
            let text: Vec<u16> = "Internet Settings\0".encode_utf16().collect();
            let mut delivered = 0usize;
            SendMessageTimeoutW(
                0xFFFF, // HWND_BROADCAST
                0x001A, // WM_SETTINGCHANGE
                0,
                text.as_ptr(),
                0x0002, // SMTO_ABORTIFHUNG
                3000,
                &mut delivered,
            );
        }
    }

    fn settings_key() -> Result<RegKey, String> {
        RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(INTERNET_SETTINGS)
            .map(|(key, _)| key)
            .map_err(|e| format!("cannot open Internet Settings: {e}"))
    }

    pub fn apply_system_proxy(port: u16) -> Result<SavedSysProxy, String> {
        let key = settings_key()?;
        let pac: String = key.get_value("AutoConfigURL").unwrap_or_default();
        if !pac.trim().is_empty() {
            return Err(format!(
                "a PAC script is configured (AutoConfigURL = {pac}), so Windows ignores the \
                 plain proxy settings. Remove the PAC script first."
            ));
        }

        let saved = SavedSysProxy {
            enable: key.get_value("ProxyEnable").ok(),
            server: key.get_value("ProxyServer").ok(),
            bypass: key.get_value("ProxyOverride").ok(),
        };

        let value = format!("http=127.0.0.1:{port};https=127.0.0.1:{port}");
        key.set_value("ProxyEnable", &1u32)
            .map_err(|e| format!("cannot write ProxyEnable: {e}"))?;
        key.set_value("ProxyServer", &value)
            .map_err(|e| format!("cannot write ProxyServer: {e}"))?;
        notify_change();
        Ok(saved)
    }

    pub fn restore_system_proxy(saved: &SavedSysProxy) -> Result<(), String> {
        let key = settings_key()?;
        let put = |name: &str, value: Option<&str>| -> Result<(), String> {
            match value {
                Some(v) => key
                    .set_value(name, &v.to_string())
                    .map_err(|e| format!("cannot write {name}: {e}")),
                None => {
                    let _ = key.delete_value(name);
                    Ok(())
                }
            }
        };
        match saved.enable {
            Some(v) => key
                .set_value("ProxyEnable", &v)
                .map_err(|e| format!("cannot write ProxyEnable: {e}"))?,
            None => {
                let _ = key.delete_value("ProxyEnable");
            }
        }
        put("ProxyServer", saved.server.as_deref())?;
        put("ProxyOverride", saved.bypass.as_deref())?;
        notify_change();
        Ok(())
    }

    /// Point WinINET at the relay's current port without touching the saved
    /// snapshot - used when the port moves while the proxy is already on.
    pub fn set_proxy_server(port: u16) -> Result<(), String> {
        let key = settings_key()?;
        let value = format!("http=127.0.0.1:{port};https=127.0.0.1:{port}");
        key.set_value("ProxyServer", &value)
            .map_err(|e| format!("cannot write ProxyServer: {e}"))?;
        notify_change();
        Ok(())
    }

    pub fn system_proxy_on() -> bool {
        settings_key()
            .ok()
            .and_then(|k| k.get_value::<u32, _>("ProxyEnable").ok())
            == Some(1)
    }

    /// True while WinINET still carries a proxy on our relay's port. A stale
    /// one left behind by a crash looks exactly like ours; the user's own local
    /// proxy (127.0.0.1:7890 and friends) does not, and must not be touched.
    pub fn proxy_points_at(port: u16) -> bool {
        settings_key()
            .ok()
            .and_then(|k| k.get_value::<String, _>("ProxyServer").ok())
            .map(|v| v.contains(&format!("127.0.0.1:{port}")))
            .unwrap_or(false)
    }

    /// `n` random bytes as lowercase hex. Names the voice control pipe, so a
    /// squatter cannot guess it - and the elevated engine's argv is unreadable
    /// from a normal process, which is what makes the nonce worth anything.
    pub fn random_hex(n: usize) -> Result<String, String> {
        let mut buf = vec![0u8; n];
        let ok = unsafe { SystemFunction036(buf.as_mut_ptr().cast(), buf.len() as u32) };
        if ok == 0 {
            return Err("cannot read random bytes".to_string());
        }
        Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
    }

    /// Start this same exe again, elevated - the one UAC prompt the voice fix
    /// costs. `params` is the raw command line; every argument in it must be
    /// quoted if it can contain spaces (ours never do).
    pub fn run_elevated(params: &str) -> Result<(), String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot locate this exe: {e}"))?;
        let file: Vec<u16> = exe
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let args: Vec<u16> = params.encode_utf16().chain(std::iter::once(0)).collect();
        const RUNAS: [u16; 6] = [
            b'r' as u16,
            b'u' as u16,
            b'n' as u16,
            b'a' as u16,
            b's' as u16,
            0,
        ];
        unsafe {
            let mut info: ShellExecuteInfoW = std::mem::zeroed();
            info.cb_size = std::mem::size_of::<ShellExecuteInfoW>() as u32;
            // NOASYNC: wait for the launch to actually be accepted, FLAG_NO_UI:
            // never show ShellExecute's own error box - we report it instead.
            info.f_mask = 0x0000_0100 | 0x0000_0400;
            info.lp_verb = RUNAS.as_ptr();
            info.lp_file = file.as_ptr();
            info.lp_parameters = args.as_ptr();
            info.n_show = 0; // SW_HIDE: the engine is a background process
            if ShellExecuteExW(&mut info) == 0 {
                let err = std::io::Error::last_os_error();
                // ERROR_CANCELLED - the user said no to the UAC dialog.
                if err.raw_os_error() == Some(1223) {
                    return Err("the admin prompt was declined".to_string());
                }
                return Err(format!("cannot start the elevated helper: {err}"));
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------ Discord

    fn version_key(name: &str) -> Vec<u64> {
        name.trim_start_matches("app-")
            .split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse().ok())
            .collect()
    }

    fn discord_exes(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(root) {
            let mut apps: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_dir()
                        && p.file_name()
                            .map(|n| n.to_string_lossy().starts_with("app-"))
                            .unwrap_or(false)
                })
                .collect();
            apps.sort_by_key(|p| {
                p.file_name()
                    .map(|n| version_key(&n.to_string_lossy()))
                    .unwrap_or_default()
            });
            for dir in apps.into_iter().rev() {
                let exe = dir.join("Discord.exe");
                if exe.is_file() {
                    found.push(exe);
                }
            }
        }
        let exe = root.join("Discord.exe");
        if exe.is_file() {
            found.push(exe);
        }
        found
    }

    pub fn find_discord() -> Option<String> {
        let local = std::env::var("LOCALAPPDATA").ok()?;
        let mut candidates = Vec::new();

        for product in ["Discord", "DiscordPTB", "DiscordCanary"] {
            candidates.extend(discord_exes(&Path::new(&local).join(product)));
        }
        for base in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(dir) = std::env::var(base) {
                let exe = Path::new(&dir).join("Discord").join("Discord.exe");
                if exe.is_file() {
                    candidates.push(exe);
                }
            }
        }

        candidates
            .into_iter()
            .next()
            .map(|p| p.to_string_lossy().into_owned())
            // No client exe (mid-update, or an install that only unpacked the
            // updater so far): the updater is enough, launch_discord knows how
            // to drive it with --processStart.
            .or_else(find_updater)
    }

    /// Squirrel puts Update.exe at the root of each install, beside app-*.
    pub fn find_updater() -> Option<String> {
        let local = std::env::var("LOCALAPPDATA").ok()?;
        for product in ["Discord", "DiscordPTB", "DiscordCanary"] {
            for name in ["Update.exe", "updater.exe"] {
                let exe = Path::new(&local).join(product).join(name);
                if exe.is_file() {
                    return Some(exe.to_string_lossy().into_owned());
                }
            }
        }
        None
    }

    /// Cheap "is this image running" check: tasklist needs no extra crate, and
    /// only ASCII image names come back, so the console code page is irrelevant.
    /// The exit code is useless here (no match still exits 0) - read the list.
    fn process_running(image: &str) -> bool {
        Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {image}"), "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(image))
            .unwrap_or(false)
    }

    /// True while the Discord client is up - the Launch button turns into Kill.
    pub fn discord_running() -> bool {
        process_running("Discord.exe")
    }

    /// Squirrel's updater while it is actually running. The dialog's session
    /// clock keys off this instead of a fixed deadline, so an update in
    /// progress is never cut off halfway.
    pub fn updater_running() -> bool {
        process_running("Update.exe")
    }

    /// Discord and Squirrel's updater, trees included (`/T` also takes the
    /// renderers hanging off Discord.exe). Whether it was running at all is
    /// decided beforehand with tasklist, because taskkill's "not found" message
    /// comes translated from Windows.
    pub fn kill_discord() -> Result<String, String> {
        let mut stopped = Vec::new();
        let mut failed = Vec::new();
        for image in ["Discord.exe", "Update.exe"] {
            if !process_running(image) {
                continue;
            }
            match Command::new("taskkill")
                .args(["/F", "/T", "/IM", image])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
            {
                Ok(o) if o.status.success() => stopped.push(image),
                Ok(o) => failed.push(format!(
                    "{image}: {}",
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    )
                    .trim()
                )),
                Err(e) => failed.push(format!("{image}: {e}")),
            }
        }
        if stopped.is_empty() {
            if failed.is_empty() {
                return Ok("Discord was not running".to_string());
            }
            return Err(failed.join("; "));
        }
        let mut note = format!("stopped {}", stopped.join(" + "));
        if !failed.is_empty() {
            note.push_str(&format!(" ({})", failed.join("; ")));
        }
        Ok(note)
    }

    /// Hand a downloaded installer to NSIS. The delay comes first because NSIS
    /// refuses to overwrite an exe that is still running; `/S` installs without
    /// a window and `/R` starts the new version once that is done.
    pub fn run_installer(path: &Path) -> Result<(), String> {
        let line = format!(
            "ping -n 4 127.0.0.1 >nul & start \"\" \"{}\" /S /R",
            path.display()
        );
        Command::new("cmd")
            .raw_arg("/C")
            .raw_arg(&line)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("cannot start the installer: {e}"))
    }

    pub fn launch_discord(path: &str, proxy_port: u16) -> Result<(), String> {
        let is_client = Path::new(path)
            .file_name()
            .map(|n| n.eq_ignore_ascii_case("Discord.exe"))
            .unwrap_or(false);

        let mut cmd = Command::new(path);
        if is_client {
            cmd.arg(format!("--proxy-server=http://127.0.0.1:{proxy_port}"));
            cmd.arg("--proxy-bypass-list=<-loopback>");
        } else {
            // Squirrel's Update.exe swallows command-line switches; the system
            // proxy setting is what carries the traffic in this case.
            cmd.arg("--processStart").arg("Discord.exe");
        }
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.spawn()
            .map(|_| ())
            .map_err(|e| format!("cannot start Discord ({path}): {e}"))
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    use crate::store::SavedSysProxy;

    pub fn apply_system_proxy(_port: u16) -> Result<SavedSysProxy, String> {
        Err("Windows system proxy is only available on Windows".to_string())
    }

    pub fn restore_system_proxy(_saved: &SavedSysProxy) -> Result<(), String> {
        Err("Windows system proxy is only available on Windows".to_string())
    }

    pub fn set_proxy_server(_port: u16) -> Result<(), String> {
        Err("Windows system proxy is only available on Windows".to_string())
    }

    pub fn system_proxy_on() -> bool {
        false
    }

    pub fn proxy_points_at(_port: u16) -> bool {
        false
    }

    pub fn find_discord() -> Option<String> {
        None
    }

    pub fn find_updater() -> Option<String> {
        None
    }

    pub fn discord_running() -> bool {
        false
    }

    pub fn updater_running() -> bool {
        false
    }

    pub fn kill_discord() -> Result<String, String> {
        Err("Killing Discord is only supported on Windows".to_string())
    }

    pub fn launch_discord(_path: &str, _proxy_port: u16) -> Result<(), String> {
        Err("Launching Discord is only supported on Windows".to_string())
    }

    pub fn run_installer(_path: &Path) -> Result<(), String> {
        Err("Installing updates is only supported on Windows".to_string())
    }

    pub fn random_hex(_n: usize) -> Result<String, String> {
        Err("Windows only".to_string())
    }

    pub fn run_elevated(_params: &str) -> Result<(), String> {
        Err("Windows only".to_string())
    }
}

pub use imp::{
    apply_system_proxy, discord_running, find_discord, find_updater, kill_discord, launch_discord,
    proxy_points_at, random_hex, restore_system_proxy, run_elevated, run_installer,
    set_proxy_server, system_proxy_on, updater_running,
};
