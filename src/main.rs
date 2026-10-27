#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! The Utility Closet - all-in-one Windows maintenance app (egui version).

use eframe::egui;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const MAX_DRIVES: usize = 6;
const MAX_PRINTERS: usize = 6;

struct DriveRow {
    letter: String,
    path: String,
}

struct PrinterRow {
    path: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Confirm {
    DisplayCache,
    IconCache,
    Reboot,
}

struct ClosetApp {
    drives: Vec<DriveRow>,
    printer_rows: Vec<PrinterRow>,
    printers: Vec<String>,
    selected_printer: Option<usize>,
    forced_shutdown: bool,
    welcome_experience: bool,
    show_all_tray_icons: bool,
    copilot_enabled: bool,
    log: String,
    confirm: Option<Confirm>,
    show_about: bool,
    update_rx: Option<mpsc::Receiver<UpdateMsg>>,
    update_info: Option<ReleaseInfo>,
    update_dismissed: bool,
    update_status: String,
    update_pending: bool,
    update_downloading: bool,
    update_progress: (u64, u64),
}

// ---------------------------------------------------------------------------
// auto update (Tuya-style: download installer + run it, no browser)
// ---------------------------------------------------------------------------

const UPDATE_REPO: &str = "shawnrodis/UtilityCloset";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// A release asset this app installs from.
#[derive(Clone)]
struct ReleaseInfo {
    version: String,    // e.g. "1.0.5" (tag without the 'v')
    asset_name: String, // e.g. "UtilityCloset-Setup-v1.0.5.exe"
    asset_url: String,  // browser_download_url
    asset_size: u64,
}

enum UpdateMsg {
    Available(ReleaseInfo),
    UpToDate,
    CheckFailed,
    DownloadProgress(u64, u64), // (bytes got, total)
    Downloaded(std::path::PathBuf),
    DownloadFailed,
}

fn parse_ver(s: &str) -> (u64, u64, u64) {
    let mut it = s
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0));
    (
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
        it.next().unwrap_or(0),
    )
}

/// The installer asset name for a version — exact match, same strictness
/// as the Tuya app's select_update_asset.
fn expected_asset_name(version: &str) -> String {
    format!("UtilityCloset-Setup-v{version}.exe")
}

fn check_updates(tx: mpsc::Sender<UpdateMsg>) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("UtilityCloset-update-check")
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            send(UpdateMsg::CheckFailed);
            return;
        }
    };
    let v: serde_json::Value = match client
        .get(format!(
            "https://api.github.com/repos/{UPDATE_REPO}/releases/latest"
        ))
        .send()
    {
        Ok(r) if r.status().is_success() => match r.json() {
            Ok(v) => v,
            Err(_) => {
                send(UpdateMsg::CheckFailed);
                return;
            }
        },
        _ => {
            send(UpdateMsg::CheckFailed);
            return;
        }
    };
    let tag = v["tag_name"].as_str().unwrap_or("").to_string();
    if tag.is_empty() {
        send(UpdateMsg::CheckFailed);
        return;
    }
    if parse_ver(&tag) <= parse_ver(APP_VERSION) {
        send(UpdateMsg::UpToDate);
        return;
    }
    // Newer version — find this platform's installer asset by exact name.
    let version = tag.trim_start_matches('v').to_string();
    let want = expected_asset_name(&version);
    let assets = v["assets"].as_array().cloned().unwrap_or_default();
    let asset = assets.iter().find(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .map(|n| n == want)
            .unwrap_or(false)
    });
    match asset {
        Some(a) => {
            let url = a
                .get("browser_download_url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            let size = a.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
            if url.is_empty() {
                send(UpdateMsg::CheckFailed);
            } else {
                send(UpdateMsg::Available(ReleaseInfo {
                    version,
                    asset_name: want,
                    asset_url: url,
                    asset_size: size,
                }));
            }
        }
        None => send(UpdateMsg::CheckFailed),
    }
}

/// Download the installer to %TEMP%, reporting progress. Verifies the
/// file name matches the expected asset before reporting success.
fn download_update(info: ReleaseInfo, tx: mpsc::Sender<UpdateMsg>) {
    let send = |m| {
        let _ = tx.send(m);
    };
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent("UtilityCloset-update-download")
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            send(UpdateMsg::DownloadFailed);
            return;
        }
    };
    let mut resp = match client.get(&info.asset_url).send() {
        Ok(r) if r.status().is_success() => r,
        _ => {
            send(UpdateMsg::DownloadFailed);
            return;
        }
    };
    let total = resp.content_length().unwrap_or(info.asset_size);
    let dest = std::env::temp_dir().join(&info.asset_name);
    let mut file = match std::fs::File::create(&dest) {
        Ok(f) => f,
        Err(_) => {
            send(UpdateMsg::DownloadFailed);
            return;
        }
    };
    let mut got: u64 = 0;
    let mut buf = [0u8; 8192];
    use std::io::Read;
    use std::io::Write;
    loop {
        match resp.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if file.write_all(&buf[..n]).is_err() {
                    send(UpdateMsg::DownloadFailed);
                    return;
                }
                got += n as u64;
                let _ = tx.send(UpdateMsg::DownloadProgress(got, total));
            }
            Err(_) => {
                send(UpdateMsg::DownloadFailed);
                return;
            }
        }
    }
    // Verify the file exists and is non-empty before handing it off.
    match std::fs::metadata(&dest) {
        Ok(m) if m.len() > 0 => send(UpdateMsg::Downloaded(dest)),
        _ => send(UpdateMsg::DownloadFailed),
    }
}

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn config_path() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    p.set_file_name("UtilityCloset.ini");
    p
}

fn is_dark_mode() -> bool {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            let t = String::from_utf8_lossy(&o.stdout);
            t.lines()
                .any(|l| l.contains("AppsUseLightTheme") && l.trim_end().ends_with("0x0"))
        }
        Err(_) => false,
    }
}

/// Forced shutdown: HKCU\Control Panel\Desktop values that decide whether
/// Windows' own Shut down / Restart buttons stop at the "apps are
/// preventing shutdown" warning or end blocking apps themselves.
/// ON:  AutoEndTasks=1, WaitToKillAppTimeout=2000, HungAppTimeout=2000
/// OFF: AutoEndTasks=0, WaitToKillAppTimeout=20000, HungAppTimeout=5000
/// (the Windows defaults). Read on launch so the toggle always shows
/// the real system state, not a saved guess.
const DESKTOP_KEY: &str = r"HKCU\Control Panel\Desktop";

fn is_forced_shutdown() -> bool {
    let out = Command::new("reg")
        .args(["query", DESKTOP_KEY, "/v", "AutoEndTasks"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|l| l.contains("AutoEndTasks") && l.split_whitespace().last() == Some("1")),
        Err(_) => false,
    }
}

fn apply_forced_shutdown(on: bool) -> bool {
    let (auto_end, kill_app, hung) = if on {
        ("1", "2000", "2000")
    } else {
        ("0", "20000", "5000")
    };
    let values = [
        ("AutoEndTasks", auto_end),
        ("WaitToKillAppTimeout", kill_app),
        ("HungAppTimeout", hung),
    ];
    let mut ok = true;
    for (name, value) in values {
        let status = Command::new("reg")
            .args([
                "add", DESKTOP_KEY, "/v", name, "/t", "REG_SZ", "/d", value, "/f",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            ok = false;
        }
    }
    ok
}

/// Windows welcome experience (the post-update "Let's finish setting
/// up your device" / "Get even more out of Windows" screen with OneDrive
/// backup, Microsoft 365, etc. prompts).
/// OFF (suppressed): all three DWORDs below = 0.
/// ON (allowed):    all three DWORDs = 1, so the state is explicit and
/// readable. On launch, ScoobeSystemSettingEnabled is the read switch:
/// 0 = off, 1 or missing = on (the Windows default).
fn is_welcome_experience_on() -> bool {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\UserProfileEngagement",
            "/v",
            "ScoobeSystemSettingEnabled",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            // Bind the lossy text first: the line borrows from it, so
            // it must outlive the find() call (E0716 if inlined).
            let text = String::from_utf8_lossy(&o.stdout);
            let line = text
                .lines()
                .find(|l| l.contains("ScoobeSystemSettingEnabled"));
            match line.and_then(|l| l.split_whitespace().last()) {
                Some("0") | Some("0x0") => false,
                _ => true,
            }
        }
        Err(_) => true,
    }
}

fn apply_welcome_experience(on: bool) -> bool {
    let value = if on { "1" } else { "0" };
    let values = [
        (
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\UserProfileEngagement",
            "ScoobeSystemSettingEnabled",
        ),
        (
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\ContentDeliveryManager",
            "SubscribedContent-310093Enabled",
        ),
        (
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\ContentDeliveryManager",
            "SoftLandingEnabled",
        ),
    ];
    let mut ok = true;
    for (key, name) in values {
        let status = Command::new("reg")
            .args([
                "add", key, "/v", name, "/t", "REG_DWORD", "/d", value, "/f",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            ok = false;
        }
    }
    ok
}

/// Always show all tray icons (notification area): Windows 11 removed
/// the old global checkbox and only offers per-app Taskbar corner
/// overflow toggles, so this is the closest thing to one global switch.
/// Polarity (web guides conflict on this — this is the semantic
/// definition used here):
///   EnableAutoTray = 0 disables "automatically hide inactive tray
///     icons", i.e. icons stay visible; = 1 is the normal hiding.
///   NoAutoTrayNotify = 1 is the "Turn off notification area cleanup"
///     policy value; = 0 is normal.
/// Show-all ON:  EnableAutoTray=0, NoAutoTrayNotify=1.
/// Show-all OFF: EnableAutoTray=1, NoAutoTrayNotify=0.
/// On launch, EnableAutoTray is the read switch: 0 = show-all on,
/// 1 or missing = normal. Needs an Explorer restart to take effect.
fn is_show_all_tray_icons() -> bool {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer",
            "/v",
            "EnableAutoTray",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let line = text.lines().find(|l| l.contains("EnableAutoTray"));
            match line.and_then(|l| l.split_whitespace().last()) {
                Some("0") | Some("0x0") => true,
                _ => false,
            }
        }
        Err(_) => false,
    }
}

fn apply_show_all_tray_icons(on: bool) -> bool {
    let (auto_tray, no_notify) = if on { ("0", "1") } else { ("1", "0") };
    let values = [
        (
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer",
            "EnableAutoTray",
            auto_tray,
        ),
        (
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Policies\Explorer",
            "NoAutoTrayNotify",
            no_notify,
        ),
    ];
    let mut ok = true;
    for (key, name, value) in values {
        let status = Command::new("reg")
            .args([
                "add", key, "/v", name, "/t", "REG_DWORD", "/d", value, "/f",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            ok = false;
        }
    }
    ok
}

/// Microsoft Copilot in Windows: the "Turn off Windows Copilot"
/// policy plus the taskbar Copilot button.
/// OFF (disabled): TurnOffWindowsCopilot=1 (HKLM policy) and
///   ShowCopilotButton=0 (HKCU taskbar button).
/// ON (enabled):   TurnOffWindowsCopilot=0 and ShowCopilotButton=1,
///   so the state is explicit and readable (0 = the policy is not
///   turning Copilot off, i.e. the Windows default behavior).
/// On launch, the policy is the read switch: 1 = off, anything else
/// or missing = on (the Windows default). Needs an Explorer restart
/// for the taskbar button to update.
/// Note: this disables the Windows Copilot experience; it does not
/// uninstall the standalone Copilot app, which stays installed and
/// can still be opened on its own.
fn is_copilot_enabled() -> bool {
    let out = Command::new("reg")
        .args([
            "query",
            r"HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsCopilot",
            "/v",
            "TurnOffWindowsCopilot",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let line = text.lines().find(|l| l.contains("TurnOffWindowsCopilot"));
            match line.and_then(|l| l.split_whitespace().last()) {
                Some("1") | Some("0x1") => false,
                _ => true,
            }
        }
        Err(_) => true,
    }
}

fn apply_copilot_enabled(on: bool) -> bool {
    let (policy, button) = if on { ("0", "1") } else { ("1", "0") };
    let values = [
        (
            r"HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsCopilot",
            "TurnOffWindowsCopilot",
            policy,
        ),
        (
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced",
            "ShowCopilotButton",
            button,
        ),
    ];
    let mut ok = true;
    for (key, name, value) in values {
        let status = Command::new("reg")
            .args([
                "add", key, "/v", name, "/t", "REG_DWORD", "/d", value, "/f",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            ok = false;
        }
    }
    ok
}

impl ClosetApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(if is_dark_mode() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        let mut app = Self {
            drives: Vec::new(),
            printer_rows: vec![PrinterRow { path: String::new() }],
            printers: Vec::new(),
            selected_printer: None,
            forced_shutdown: false,
            welcome_experience: true,
            show_all_tray_icons: false,
            copilot_enabled: true,
            log: String::new(),
            confirm: None,
            show_about: false,
            update_rx: None,
            update_info: None,
            update_dismissed: false,
            update_status: String::new(),
            update_pending: false,
            update_downloading: false,
            update_progress: (0, 0),
        };
        // Check for updates in the background (silent unless one is found).
        let (utx, urx) = mpsc::channel();
        app.update_rx = Some(urx);
        app.update_pending = true;
        thread::spawn(move || check_updates(utx));
        app.load_config();
        // Forced-shutdown toggle reflects the real registry state, not
        // a saved setting (another tool could have changed it).
        app.forced_shutdown = is_forced_shutdown();
        app.welcome_experience = is_welcome_experience_on();
        app.show_all_tray_icons = is_show_all_tray_icons();
        app.copilot_enabled = is_copilot_enabled();
        // Fresh start: one blank X row only. If the user wants more
        // drives, the "Add Drive" button adds them — no prefilled Y row.
        // Saved drives from load_config() above are left untouched.
        if app.drives.is_empty() {
            app.drives.push(DriveRow {
                letter: "X".into(),
                path: String::new(),
            });
        }
        app.log_line("Utility Closet started (running as Administrator).");
        app
    }

    fn log_line(&mut self, msg: &str) {
        self.log.push_str(&format!("[{}] {}\n", timestamp(), msg));
    }

    fn load_config(&mut self) {
        let text = std::fs::read_to_string(config_path()).unwrap_or_default();
        let mut map = std::collections::HashMap::new();
        for line in text.lines() {
            if let Some((k, v)) = line.split_once('=') {
                map.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
        let count: usize = map.get("Count").and_then(|s| s.parse().ok()).unwrap_or(0);
        for i in 0..count.min(MAX_DRIVES) {
            let letter = map
                .get(&format!("Letter{i}"))
                .cloned()
                .unwrap_or_default();
            let path = map
                .get(&format!("Path{i}"))
                .cloned()
                .unwrap_or_default();
            self.drives.push(DriveRow { letter, path });
        }
    }

    fn save_config(&self) {
        let mut s = format!("Count={}\n", self.drives.len());
        for (i, d) in self.drives.iter().enumerate() {
            s.push_str(&format!("Letter{i}={}\n", d.letter.trim()));
            s.push_str(&format!("Path{i}={}\n", d.path.trim()));
        }
        let _ = std::fs::write(config_path(), s);
    }

    /// Run a console tool hidden, log its output lines. True on success.
    fn run_logged(&mut self, program: &str, args: &[&str]) -> bool {
        let out = Command::new(program)
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) => {
                let mut ok = false;
                for chunk in [&o.stdout, &o.stderr] {
                    for line in String::from_utf8_lossy(chunk).lines() {
                        let t = line.trim();
                        if !t.is_empty() {
                            self.log_line(t);
                            if t.contains("successfully") {
                                ok = true;
                            }
                        }
                    }
                }
                ok && o.status.success()
            }
            Err(e) => {
                self.log_line(&format!("Failed to run {program}: {e}"));
                false
            }
        }
    }

    fn normalized_letter(raw: &str) -> Option<String> {
        let l = raw.trim().trim_end_matches(':').to_uppercase();
        if l.len() == 1 && l.chars().next().unwrap().is_ascii_alphabetic() {
            Some(l)
        } else {
            None
        }
    }

    fn map_drives(&mut self) {
        let rows: Vec<(String, String)> = self
            .drives
            .iter()
            .map(|r| (r.letter.clone(), r.path.clone()))
            .collect();
        let mut mapped = 0;
        for (letter_raw, path_raw) in &rows {
            let path = path_raw.trim();
            if path.is_empty() {
                continue;
            }
            match Self::normalized_letter(letter_raw) {
                Some(letter) => {
                    self.log_line(&format!("Mapping {letter}: to {path} ..."));
                    let target = format!("{letter}:");
                    if self.run_logged("net", &["use", &target, path, "/persistent:yes"]) {
                        mapped += 1;
                    }
                }
                None => {
                    self.log_line("Skipped: invalid drive letter.");
                }
            }
        }
        for row in &mut self.drives {
            if let Some(l) = Self::normalized_letter(&row.letter) {
                row.letter = l;
            }
        }
        if mapped > 0 {
            self.save_config();
            self.log_line("Drive list saved for next time.");
        } else {
            self.log_line(r"Nothing mapped. Enter a network path first (e.g. \\SERVER\share).");
        }
    }

    fn disconnect_drives(&mut self) {
        let rows: Vec<String> = self.drives.iter().map(|r| r.letter.clone()).collect();
        for letter_raw in rows {
            if let Some(letter) = Self::normalized_letter(&letter_raw) {
                self.log_line(&format!("Disconnecting {letter}: ..."));
                let target = format!("{letter}:");
                self.run_logged("net", &["use", &target, "/delete", "/y"]);
            }
        }
    }

    fn run_sfc(&mut self) {
        self.log_line("Launching sfc /scannow in a new window...");
        let _ = Command::new("cmd")
            .args(["/c", "start", "cmd", "/k", "sfc", "/scannow"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }

    /// DISM, like SFC, runs in its own window so the user can watch
    /// progress — it is deliberately NOT run hidden via run_logged.
    fn run_dism(&mut self, args: &[&str], label: &str) {
        self.log_line(&format!("Launching DISM {label} in a new window..."));
        let mut cmd_args: Vec<&str> = vec!["/c", "start", "cmd", "/k", "DISM.exe"];
        cmd_args.extend_from_slice(args);
        let _ = Command::new("cmd")
            .args(&cmd_args)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }

    fn clear_display_cache(&mut self) {
        let keys = [
            r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration",
            r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Connectivity",
            r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\ScaleFactors",
        ];
        for k in keys {
            self.log_line(&format!("Deleting {k} ..."));
            self.run_logged("reg", &["delete", k, "/f"]);
        }
        self.log_line("Done. RESTART your PC to finish the display cache reset.");
    }

    fn rebuild_icon_cache(&mut self) {
        self.log_line("Clearing icon cache...");
        let _ = Command::new("ie4uinit.exe")
            .arg("-ClearIconCache")
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        let _ = Command::new("taskkill")
            .args(["/IM", "explorer.exe", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .status();
        std::thread::sleep(std::time::Duration::from_secs(2));
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            let _ = std::fs::remove_file(format!("{local}\\IconCache.db"));
            if let Ok(entries) = std::fs::read_dir(format!("{local}\\Microsoft\\Windows\\Explorer")) {
                for e in entries.flatten() {
                    if e
                        .file_name()
                        .to_string_lossy()
                        .to_lowercase()
                        .starts_with("iconcache")
                    {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
        let _ = Command::new("explorer.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
        self.log_line("Explorer restarted. Icon cache will rebuild.");
    }

    // -----------------------------------------------------------------------
    // Printers (network printer add/remove, below Network Drives)
    // -----------------------------------------------------------------------

    /// Refresh the installed-printer list via PowerShell.
    fn refresh_printers(&mut self) {
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Get-Printer | Select-Object -ExpandProperty Name",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout);
                self.printers = text
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                self.selected_printer = None;
                self.log_line(&format!(
                    "Found {} installed printer(s).",
                    self.printers.len()
                ));
            }
            Err(e) => {
                self.log_line(&format!("Failed to list printers: {e}"));
            }
        }
    }

    /// Add a network printer by UNC path (e.g. \\SERVER\Printer).
    /// Add every printer row with a non-empty path (bulk, like Map Drives).
    fn add_printers(&mut self) {
        let rows: Vec<String> = self
            .printer_rows
            .iter()
            .map(|r| r.path.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        if rows.is_empty() {
            self.log_line("Enter a printer path first (e.g. \\\\SERVER\\Printer).");
            return;
        }
        let mut added = 0;
        for path in &rows {
            self.log_line(&format!("Adding printer {path} ..."));
            let ok = self.run_logged(
                "rundll32",
                &["printui.dll,PrintUIEntry", "/in", "/n", path],
            );
            if ok {
                self.log_line(&format!("Printer {path} added."));
                added += 1;
            } else {
                self.log_line(
                    "Add may have failed \u{2014} check the path and that the print server is reachable.",
                );
            }
        }
        if added > 0 {
            self.refresh_printers();
        }
    }

    /// Remove the selected printer.
    fn remove_printer(&mut self) {
        let Some(idx) = self.selected_printer else {
            self.log_line("Select a printer from the list first.");
            return;
        };
        let Some(name) = self.printers.get(idx).cloned() else {
            return;
        };
        self.log_line(&format!("Removing printer {name} ..."));
        self.run_logged(
            "rundll32",
            &["printui.dll,PrintUIEntry", "/dl", "/n", &name],
        );
        self.refresh_printers();
    }

    /// Set the selected printer as default.
    fn set_default_printer(&mut self) {
        let Some(idx) = self.selected_printer else {
            self.log_line("Select a printer from the list first.");
            return;
        };
        let Some(name) = self.printers.get(idx).cloned() else {
            return;
        };
        self.log_line(&format!("Setting {name} as default printer ..."));
        self.run_logged(
            "rundll32",
            &["printui.dll,PrintUIEntry", "/y", "/n", &name],
        );
    }
}

impl eframe::App for ClosetApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // poll the background update check; keep repainting until it lands
        if self.update_pending {
            ctx.request_repaint();
        }
        let mut got: Option<UpdateMsg> = None;
        if let Some(rx) = &self.update_rx {
            while let Ok(m) = rx.try_recv() {
                got = Some(m);
            }
        }
        match got {
            Some(UpdateMsg::Available(info)) => {
                self.update_info = Some(info);
                self.update_dismissed = false;
                self.update_status.clear();
                self.update_pending = false;
                self.update_downloading = false;
            }
            Some(UpdateMsg::UpToDate) => {
                self.update_info = None;
                self.update_status = format!("You're up to date (v{APP_VERSION}).");
                self.update_pending = false;
            }
            Some(UpdateMsg::CheckFailed) => {
                self.update_pending = false;
                // Only report manual checks; the launch check stays silent.
                if self.update_status == "Checking…" {
                    self.update_status =
                        "Couldn't reach GitHub — check your connection and try again.".into();
                }
            }
            Some(UpdateMsg::DownloadProgress(got_bytes, total)) => {
                self.update_progress = (got_bytes, total);
                ctx.request_repaint();
            }
            Some(UpdateMsg::Downloaded(path)) => {
                self.update_downloading = false;
                self.update_status = format!(
                    "Downloaded {} — launching installer…",
                    path.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()
                );
                // Run the installer, then exit so it can replace us
                // (Tuya-style: the installer handles the upgrade).
                let _ = Command::new(&path)
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
                std::thread::sleep(std::time::Duration::from_millis(500));
                std::process::exit(0);
            }
            Some(UpdateMsg::DownloadFailed) => {
                self.update_downloading = false;
                self.update_status = "Download failed — check your connection and try again.".into();
            }
            None => {}
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Utility Closet");

                ui.horizontal(|ui| {
                    if ui.button("Check for updates").clicked() {
                        let (tx2, rx2) = mpsc::channel();
                        self.update_rx = Some(rx2);
                        self.update_dismissed = false;
                        self.update_info = None;
                        self.update_status = "Checking…".into();
                        self.update_pending = true;
                        thread::spawn(move || check_updates(tx2));
                    }
                    if !self.update_status.is_empty() {
                        ui.label(&self.update_status);
                    }
                });
                ui.add_space(4.0);

                // update banner
                let upd = self.update_info.clone();
                if let Some(info) = upd {
                    if !self.update_dismissed {
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Update available: v{} (you have v{APP_VERSION})",
                                    info.version
                                ))
                                .strong(),
                            );
                            if self.update_downloading {
                                let (got_b, total_b) = self.update_progress;
                                let pct = if total_b > 0 {
                                    got_b as f32 / total_b as f32 * 100.0
                                } else {
                                    0.0
                                };
                                ui.label(format!(
                                    "Downloading… {:.0}% ({:.1} / {:.1} MB)",
                                    pct,
                                    got_b as f32 / 1_048_576.0,
                                    total_b as f32 / 1_048_576.0
                                ));
                            } else if ui.button("Download & Install").clicked() {
                                // Tuya-style: download the installer, run
                                // it, exit so it can replace us.
                                self.update_downloading = true;
                                self.update_progress = (0, info.asset_size);
                                let (tx2, rx2) = mpsc::channel();
                                self.update_rx = Some(rx2);
                                let info2 = info.clone();
                                thread::spawn(move || download_update(info2, tx2));
                            }
                            if ui.button("Dismiss").clicked() {
                                self.update_dismissed = true;
                            }
                        });
                    }
                }

                ui.add_space(4.0);

                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("Network Drives").strong());
                    let mut remove_idx: Option<usize> = None;
                    for (i, row) in self.drives.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut row.letter).desired_width(36.0),
                            );
                            ui.add(
                                egui::TextEdit::singleline(&mut row.path)
                                    .desired_width(360.0)
                                    .hint_text(r"\\SERVER\share"),
                            );
                            if ui.button("Remove").clicked() {
                                remove_idx = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove_idx {
                        self.drives.remove(i);
                    }
                    let (mut do_add, mut do_map, mut do_disco) = (false, false, false);
                    ui.horizontal(|ui| {
                        do_add = ui.button("Add Drive").clicked();
                        do_map = ui.button("Map Drives").clicked();
                        do_disco = ui.button("Disconnect").clicked();
                    });
                    if do_add && self.drives.len() < MAX_DRIVES {
                        self.drives.push(DriveRow {
                            letter: String::new(),
                            path: String::new(),
                        });
                    }
                    if do_map {
                        self.map_drives();
                    }
                    if do_disco {
                        self.disconnect_drives();
                    }
                    ui.label(
                        "Paths start blank — enter yours. Add as many drives as you like; \
                         the list is saved automatically.",
                    );
                });

                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("Printers").strong());
                    // Bulk-add rows, just like Network Drives: queue up
                    // paths, then add them all at once.
                    let mut remove_idx: Option<usize> = None;
                    for (i, row) in self.printer_rows.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut row.path)
                                    .desired_width(360.0)
                                    .hint_text(r"\\SERVER\Printer"),
                            );
                            if ui.button("Remove").clicked() {
                                remove_idx = Some(i);
                            }
                        });
                    }
                    if let Some(i) = remove_idx {
                        self.printer_rows.remove(i);
                    }
                    let (mut do_add_row, mut do_add_all, mut do_refresh) =
                        (false, false, false);
                    ui.horizontal(|ui| {
                        do_add_row = ui.button("Add Printer").clicked();
                        do_add_all = ui.button("Add Printers").clicked();
                        do_refresh = ui.button("Refresh List").clicked();
                    });
                    if do_add_row && self.printer_rows.len() < MAX_PRINTERS {
                        self.printer_rows.push(PrinterRow { path: String::new() });
                    }
                    if do_add_all {
                        self.add_printers();
                    }
                    if do_refresh {
                        self.refresh_printers();
                    }
                    if !self.printers.is_empty() {
                        egui::ScrollArea::vertical().max_height(100.0).show(ui, |ui| {
                            for (i, name) in self.printers.iter().enumerate() {
                                let selected = self.selected_printer == Some(i);
                                if ui.selectable_label(selected, name).clicked() {
                                    self.selected_printer = Some(i);
                                }
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button("Set as Default").clicked() {
                                self.set_default_printer();
                            }
                            if ui.button("Remove Printer").clicked() {
                                self.remove_printer();
                            }
                        });
                    } else {
                        ui.label("No printers listed — click Refresh List to load them.");
                    }
                    ui.label(
                        "Queue up printer paths, then Add Printers adds them all at once. \
                         Select a printer from the list below to set it as default or remove it.",
                    );
                });

                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("System File Checker").strong());
                    ui.horizontal(|ui| {
                        if ui.button("Run SFC Scan").clicked() {
                            self.run_sfc();
                        }
                        ui.label(
                            "Scans and repairs protected Windows system files (sfc /scannow). \
                             Runs in its own window — takes 10-20 minutes.",
                        );
                    });
                });

                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("DISM — Windows Image Repair").strong());
                    ui.horizontal(|ui| {
                        if ui.button("Check Health").clicked() {
                            self.run_dism(
                                &["/Online", "/Cleanup-Image", "/CheckHealth"],
                                "/Online /Cleanup-Image /CheckHealth",
                            );
                        }
                        if ui.button("Scan Health").clicked() {
                            self.run_dism(
                                &["/Online", "/Cleanup-Image", "/ScanHealth"],
                                "/Online /Cleanup-Image /ScanHealth",
                            );
                        }
                        if ui.button("Restore Health").clicked() {
                            self.run_dism(
                                &["/Online", "/Cleanup-Image", "/RestoreHealth"],
                                "/Online /Cleanup-Image /RestoreHealth",
                            );
                        }
                    });
                    ui.label(
                        "Check Health is a quick check for whether the image has \
                         been flagged as corrupted; Scan Health is a deeper scan \
                         for component store damage. Restore Health repairs the \
                         image, downloading replacement files via Windows Update — \
                         it takes a while (10-20+ minutes) and needs an internet \
                         connection for the download part. Each runs in its own \
                         window so you can watch progress.",
                    );
                    ui.label(
                        "This is the same repair as the PowerShell alternative \
                         (Repair-WindowsImage -Online -RestoreHealth) — no \
                         separate PowerShell button is needed.",
                    );
                });

                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("Display Cache").strong());
                    ui.horizontal(|ui| {
                        if ui.button("Clear & Reset Display Cache").clicked() {
                            self.confirm = Some(Confirm::DisplayCache);
                        }
                        ui.label(
                            "Deletes the GPU display configuration registry keys. \
                             You MUST restart your PC afterward.",
                        );
                    });
                });

                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("Icon Cache").strong());
                    ui.horizontal(|ui| {
                        if ui.button("Rebuild Icon Cache").clicked() {
                            self.confirm = Some(Confirm::IconCache);
                        }
                        ui.label(
                            "Deletes the icon cache and restarts Explorer. \
                             Save all open work first.",
                        );
                    });
                });

                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Windows Annoyances").strong(),
                    );
                    // Applies immediately on click — no separate Apply
                    // button. If writing the registry fails, revert the
                    // toggle so it never claims a state it didn't set.
                    if ui
                        .checkbox(
                            &mut self.forced_shutdown,
                            "Force shutdown / restart (no warning)",
                        )
                        .changed()
                    {
                        let on = self.forced_shutdown;
                        if apply_forced_shutdown(on) {
                            if on {
                                self.log_line(
                                    "Forced shutdown ON: Windows will end open apps itself \
                                     after about 2 seconds instead of showing the \
                                     \"apps are preventing shutdown\" warning.",
                                );
                            } else {
                                self.log_line(
                                    "Forced shutdown OFF: Windows will show the \
                                     \"apps are preventing shutdown\" warning again.",
                                );
                            }
                        } else {
                            self.forced_shutdown = !on;
                            self.log_line(
                                "Failed to change the forced-shutdown setting — \
                                 the toggle was left unchanged.",
                            );
                        }
                    }
                    ui.label(
                        "Changes what Windows' own Shut down / Restart buttons do: \
                         with this on, Windows ends blocking apps itself instead of \
                         stopping at the warning. Applies immediately — no reboot \
                         needed — and takes effect the next time Windows is told \
                         to shut down or restart.",
                    );
                    ui.label(
                        "Warning: with this on, unsaved work in open apps can be \
                         lost, because Windows will no longer stop and ask.",
                    );
                    ui.add_space(6.0);
                    // Same immediate-apply, revert-on-failure pattern as
                    // the forced-shutdown toggle above.
                    if ui
                        .checkbox(
                            &mut self.welcome_experience,
                            "Windows welcome experience after updates",
                        )
                        .changed()
                    {
                        let on = self.welcome_experience;
                        if apply_welcome_experience(on) {
                            if on {
                                self.log_line(
                                    "Windows welcome experience ON: the post-update \
                                     setup screen may appear again.",
                                );
                            } else {
                                self.log_line(
                                    "Windows welcome experience OFF: the post-update \
                                     setup screen is suppressed.",
                                );
                            }
                        } else {
                            self.welcome_experience = !on;
                            self.log_line(
                                "Failed to change the welcome-experience setting — \
                                 the toggle was left unchanged.",
                            );
                        }
                    }
                    ui.label(
                        "This is the white post-update setup screen (\"Let's finish \
                         setting up your device\" / \"Get even more out of Windows\") \
                         with OneDrive backup, Microsoft 365, and similar prompts. \
                         Turning it off suppresses that screen.",
                    );
                    ui.label(
                        "Note: Microsoft can reintroduce new prompts in major \
                         updates, so this suppresses the current screen rather than \
                         guaranteeing it never returns in any future form.",
                    );
                    ui.add_space(6.0);
                    // Same immediate-apply, revert-on-failure pattern as
                    // the toggles above, plus the Explorer restart this
                    // change needs (same pattern as the icon-cache tool).
                    if ui
                        .checkbox(
                            &mut self.show_all_tray_icons,
                            "Always show all taskbar tray icons",
                        )
                        .changed()
                    {
                        let on = self.show_all_tray_icons;
                        if apply_show_all_tray_icons(on) {
                            if on {
                                self.log_line(
                                    "Show all tray icons ON: notification-area icons \
                                     will no longer hide in the overflow menu.",
                                );
                            } else {
                                self.log_line(
                                    "Show all tray icons OFF: Windows will hide \
                                     inactive tray icons in the overflow menu again.",
                                );
                            }
                            self.log_line(
                                "Restarting Explorer so the taskbar picks this up \
                                 (the taskbar will briefly disappear and come back).",
                            );
                            let _ = Command::new("taskkill")
                                .args(["/IM", "explorer.exe", "/F"])
                                .creation_flags(CREATE_NO_WINDOW)
                                .status();
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let _ = Command::new("explorer.exe")
                                .creation_flags(CREATE_NO_WINDOW)
                                .spawn();
                        } else {
                            self.show_all_tray_icons = !on;
                            self.log_line(
                                "Failed to change the tray-icons setting — \
                                 the toggle was left unchanged.",
                            );
                        }
                    }
                    ui.label(
                        "Stops Windows hiding system-tray icons in the overflow \
                         menu behind the up-arrow, so every notification-area icon \
                         stays visible on the taskbar. Applying this restarts \
                         Explorer — the taskbar will briefly disappear and come back.",
                    );
                    ui.label(
                        "Caveat: on Windows 11 some apps' per-app overflow settings \
                         can still override this, so it is the closest thing to a \
                         global switch, not a guaranteed override for every app.",
                    );
                    ui.add_space(6.0);
                    // Same immediate-apply, revert-on-failure pattern as
                    // the toggles above, plus the Explorer restart this
                    // change needs (same pattern as the tray-icons toggle).
                    if ui
                        .checkbox(
                            &mut self.copilot_enabled,
                            "Microsoft Copilot in Windows",
                        )
                        .changed()
                    {
                        let on = self.copilot_enabled;
                        if apply_copilot_enabled(on) {
                            if on {
                                self.log_line(
                                    "Microsoft Copilot ON: the Copilot policy is \
                                     cleared and the taskbar Copilot button is shown again.",
                                );
                            } else {
                                self.log_line(
                                    "Microsoft Copilot OFF: the Copilot policy is \
                                     set and the taskbar Copilot button is hidden.",
                                );
                            }
                            self.log_line(
                                "Restarting Explorer so the taskbar picks this up \
                                 (the taskbar will briefly disappear and come back).",
                            );
                            let _ = Command::new("taskkill")
                                .args(["/IM", "explorer.exe", "/F"])
                                .creation_flags(CREATE_NO_WINDOW)
                                .status();
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let _ = Command::new("explorer.exe")
                                .creation_flags(CREATE_NO_WINDOW)
                                .spawn();
                        } else {
                            self.copilot_enabled = !on;
                            self.log_line(
                                "Failed to change the Copilot setting — \
                                 the toggle was left unchanged.",
                            );
                        }
                    }
                    ui.label(
                        "Turns the Windows Copilot experience on or off via the \
                         TurnOffWindowsCopilot policy, and shows or hides the \
                         taskbar Copilot button to match. Applying this restarts \
                         Explorer — the taskbar will briefly disappear and come back.",
                    );
                    ui.label(
                        "Caveat: this does not uninstall the standalone Copilot \
                         app, which stays installed and can still be opened on \
                         its own; and a major Windows update can occasionally \
                         reset policy keys, so the toggle may need a re-click \
                         after one of those.",
                    );
                    ui.add_space(6.0);
                    // Reset to WINDOWS defaults — not the user's preferred
                    // settings. Each toggle follows the same revert-on-failure
                    // pattern as its checkbox; Explorer restarts at most
                    // once, at the end, and only if the tray-icons state
                    // actually changed.
                    if ui.button("Reset to defaults").clicked() {
                        let tray_before = self.show_all_tray_icons;
                        self.log_line(
                            "Resetting Windows Annoyances to Windows defaults \
                             (not your preferred settings): forced shutdown OFF, \
                             welcome experience ON, show all tray icons OFF.",
                        );
                        if apply_forced_shutdown(false) {
                            self.forced_shutdown = false;
                            self.log_line(
                                "Forced shutdown reset to OFF (Windows default).",
                            );
                        } else {
                            self.log_line(
                                "Failed to reset forced shutdown — that toggle \
                                 was left unchanged.",
                            );
                        }
                        if apply_welcome_experience(true) {
                            self.welcome_experience = true;
                            self.log_line(
                                "Welcome experience reset to ON (Windows default: \
                                 the post-update setup screen is allowed again).",
                            );
                        } else {
                            self.log_line(
                                "Failed to reset the welcome experience — that \
                                 toggle was left unchanged.",
                            );
                        }
                        if apply_show_all_tray_icons(false) {
                            self.show_all_tray_icons = false;
                            self.log_line(
                                "Show all tray icons reset to OFF (Windows \
                                 default: inactive icons hide again).",
                            );
                        } else {
                            self.log_line(
                                "Failed to reset the tray-icons setting — that \
                                 toggle was left unchanged.",
                            );
                        }
                        if tray_before != self.show_all_tray_icons {
                            self.log_line(
                                "Restarting Explorer so the taskbar picks up the \
                                 tray-icons reset (the taskbar will briefly \
                                 disappear and come back).",
                            );
                            let _ = Command::new("taskkill")
                                .args(["/IM", "explorer.exe", "/F"])
                                .creation_flags(CREATE_NO_WINDOW)
                                .status();
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            let _ = Command::new("explorer.exe")
                                .creation_flags(CREATE_NO_WINDOW)
                                .spawn();
                        }
                    }
                    ui.label(
                        "\"Defaults\" here means Windows' own defaults, not your \
                         preferred settings: forced shutdown OFF, welcome \
                         experience ON (the post-update setup screen is allowed \
                         again — that is the Windows default, even though you \
                         will usually have it off), and show all tray icons OFF. \
                         If the tray-icons state changed, Explorer restarts once \
                         so the taskbar picks it up.",
                    );
                });

                ui.add_space(6.0);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new("Log").strong());
                    egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.log)
                                .desired_width(f32::INFINITY)
                                .desired_rows(8)
                                .font(egui::TextStyle::Monospace)
                                .interactive(false),
                        );
                    });
                });

                ui.add_space(6.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("About").clicked() {
                        self.show_about = true;
                    }
                });
                ui.add_space(8.0);
            });
        });

        // confirmation modal
        if self.confirm.is_some() {
            let kind = self.confirm.take().unwrap();
            let (title, text) = match kind {
                Confirm::DisplayCache => (
                    "Clear Display Cache",
                    "This deletes the GPU display configuration keys from the registry.\n\n\
                     You must RESTART your PC afterward for it to take effect.\n\n\
                     Continue?",
                ),
                Confirm::IconCache => (
                    "Rebuild Icon Cache",
                    "Explorer will be restarted and the icon cache deleted.\n\n\
                     SAVE ALL OPEN WORK first.\n\n\
                     Continue?",
                ),
                Confirm::Reboot => (
                    "Restart PC",
                    "Icon cache cleared. Restart now to finish rebuilding?",
                ),
            };
            let mut decision: Option<bool> = None;
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(text);
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Yes").clicked() {
                            decision = Some(true);
                        }
                        if ui.button("No").clicked() {
                            decision = Some(false);
                        }
                    });
                });
            match decision {
                Some(true) => match kind {
                    Confirm::DisplayCache => self.clear_display_cache(),
                    Confirm::IconCache => {
                        self.rebuild_icon_cache();
                        self.confirm = Some(Confirm::Reboot);
                    }
                    Confirm::Reboot => {
                        let _ = Command::new("shutdown")
                            .args(["/r", "/f", "/t", "5"])
                            .creation_flags(CREATE_NO_WINDOW)
                            .spawn();
                    }
                },
                Some(false) => {}
                None => self.confirm = Some(kind),
            }
        }

        // about modal
        if self.show_about {
            egui::Window::new("About Utility Closet")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(
                        format!(
                            "Utility Closet v{APP_VERSION}\n\n\
                             All-in-one Windows maintenance:\n\
                             • Network drive mapping\n\
                             • Printer add/remove\n\
                             • System File Checker (sfc /scannow)\n\
                             • DISM Windows image repair (Check / Scan / Restore Health)\n\
                             • Display cache reset\n\
                             • Icon cache rebuild\n\
                             • Forced shutdown toggle\n\
                             • Windows welcome experience toggle\n\
                             • Always show all tray icons toggle\n\
                             • Windows Annoyances Reset to defaults\n\
                             • Microsoft Copilot enable/disable toggle"
                        ),
                    );
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        self.show_about = false;
                    }
                });
        }
    }
}

fn main() {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Utility Closet")
        .with_inner_size([660.0, 800.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Utility Closet",
        options,
        Box::new(|cc| Box::new(ClosetApp::new(cc))),
    );
}
