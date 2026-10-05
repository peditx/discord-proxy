//! Everything that has to touch the machine: the WinINET proxy settings and
//! finding/starting Discord. All of it is Windows-only; other targets get stubs
//! so the crate still type-checks.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
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

    pub fn launch_discord(path: &str, proxy_port: u16, strict: bool) -> Result<(), String> {
        let is_client = Path::new(path)
            .file_name()
            .map(|n| n.eq_ignore_ascii_case("Discord.exe"))
            .unwrap_or(false);

        let mut cmd = Command::new(path);
        if is_client {
            cmd.arg(format!("--proxy-server=http://127.0.0.1:{proxy_port}"));
            cmd.arg("--proxy-bypass-list=<-loopback>");
            if strict {
                // Everything that cannot ride the proxy is dropped instead of leaking.
                cmd.arg("--force-webrtc-ip-handling-policy=disable_non_proxied_udp");
            }
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
    use crate::store::SavedSysProxy;

    pub fn apply_system_proxy(_port: u16) -> Result<SavedSysProxy, String> {
        Err("Windows system proxy is only available on Windows".to_string())
    }

    pub fn restore_system_proxy(_saved: &SavedSysProxy) -> Result<(), String> {
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

    pub fn launch_discord(_path: &str, _proxy_port: u16, _strict: bool) -> Result<(), String> {
        Err("Launching Discord is only supported on Windows".to_string())
    }
}

pub use imp::{
    apply_system_proxy, find_discord, find_updater, launch_discord, proxy_points_at,
    restore_system_proxy, system_proxy_on,
};
