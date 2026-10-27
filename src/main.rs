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
const MAX_DRIVES: usize = 12;
const MAX_PRINTERS: usize = 6;

/// A network row is either a letter-mapped drive (net use) or a
/// network location: a shortcut in Network Shortcuts that shows up
/// in This PC under "Network locations" with no drive letter.
/// `label` is the drive letter for a Drive, the shortcut name for
/// a Location.
#[derive(Clone, Copy, PartialEq)]
enum DriveKind {
    Drive,
    Location,
}

struct DriveRow {
    kind: DriveKind,
    label: String,
    path: String,
}

/// Sanitize a network-location shortcut name for use as a file name.
fn sanitize_location_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() {
        "Network Location".into()
    } else {
        cleaned
    }
}

/// Folder where Windows keeps network-location shortcuts.
fn network_shortcuts_dir() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|a| {
            std::path::PathBuf::from(a)
                .join(r"Microsoft\Windows\Network Shortcuts")
        })
}

/// Live network locations on this PC: (shortcut name, UNC target),
/// resolved from the .lnk files in Network Shortcuts.
fn collect_network_locations() -> Vec<(String, String)> {
    match run_ps(
        "$dir = Join-Path $env:APPDATA 'Microsoft\\Windows\\Network Shortcuts'; \
         Get-ChildItem $dir -Filter *.lnk -ErrorAction SilentlyContinue | ForEach-Object { \
         $s = (New-Object -ComObject WScript.Shell).CreateShortcut($_.FullName); \
         [PSCustomObject]@{Name=$_.BaseName; Target=$s.TargetPath} } | ConvertTo-Json -Compress",
    ) {
        Ok(text) => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                json_array(v)
                    .iter()
                    .filter_map(|item| {
                        let name = json_str(item, "Name");
                        let target = json_str(item, "Target");
                        if name.is_empty() || !target.starts_with(r"\\") {
                            None
                        } else {
                            Some((name, target))
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        Err(_) => Vec::new(),
    }
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

#[derive(Clone, Copy, PartialEq)]
enum Nav {
    Overview,
    Drives,
    Printers,
    Backup,
    Repairs,
    Annoyances,
    Log,
}

struct ClosetApp {
    nav: Nav,
    monitors_text: String,
    drives: Vec<DriveRow>,
    printer_rows: Vec<PrinterRow>,
    printers: Vec<String>,
    selected_printer: Option<usize>,
    forced_shutdown: bool,
    instant_close_frozen: bool,
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

/// Instantly close frozen apps: the same hung-app settings as
/// forced shutdown, but at the fastest practical values, so a
/// not-responding app is ended after about 1 second instead of
/// Windows waiting ~5 seconds (apps) / ~20 seconds (shutdown
/// kill) first. ON: AutoEndTasks=1, WaitToKillAppTimeout=1000,
/// HungAppTimeout=1000, WaitToKillServiceTimeout=1000.
/// OFF: the Windows defaults (0, 20000, 5000, 5000).
/// Read switch is HungAppTimeout <= 1000. Note the overlap with
/// forced shutdown (which uses 2000): the two toggles share
/// these keys, so the app re-reads the other toggle after
/// either one changes, and they can never claim different
/// states for the same registry values.
fn is_instant_close_frozen() -> bool {
    let out = Command::new("reg")
        .args(["query", DESKTOP_KEY, "/v", "HungAppTimeout"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .find(|l| l.contains("HungAppTimeout"))
            .and_then(|l| l.split_whitespace().last())
            .and_then(|v| v.trim_start_matches("0x").parse::<u32>().ok().or_else(|| v.parse::<u32>().ok()))
            .map(|ms| ms > 0 && ms <= 1000)
            .unwrap_or(false),
        Err(_) => false,
    }
}

fn apply_instant_close_frozen(on: bool) -> bool {
    let (auto_end, kill_app, hung, kill_svc) = if on {
        ("1", "1000", "1000", "1000")
    } else {
        ("0", "20000", "5000", "5000")
    };
    let mut ok = true;
    for (name, value) in [
        ("AutoEndTasks", auto_end),
        ("WaitToKillAppTimeout", kill_app),
        ("HungAppTimeout", hung),
    ] {
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
    let status = Command::new("reg")
        .args([
            "add",
            r"HKLM\SYSTEM\CurrentControlSet\Control",
            "/v",
            "WaitToKillServiceTimeout",
            "/t",
            "REG_SZ",
            "/d",
            kill_svc,
            "/f",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    if !matches!(status, Ok(s) if s.success()) {
        ok = false;
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

/// Is the standalone Microsoft Copilot app installed for this user?
/// (Package family Microsoft.Copilot_8wekyb3d8bbwe; Microsoft 365
/// Copilot is a different app and is deliberately left alone.)
fn is_copilot_app_installed() -> bool {
    match Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-AppxPackage -Name 'Microsoft.Copilot*' | Select-Object -ExpandProperty Name",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

/// Is any Copilot install still present — Store Appx package OR
/// a Programs and Features entry with DisplayName exactly
/// "Copilot"? (Microsoft 365 Copilot is a different entry and is
/// deliberately not counted here.)
fn copilot_still_present() -> bool {
    // All three Programs and Features locations, not just the
    // 64-bit HKLM one: v1.0.12 only checked HKLM main, so a
    // per-user (HKCU) or 32-bit (WOW6432Node) install made the
    // check say "gone" while the entry was still sitting there.
    match run_ps(
        "$keys = @('HKLM:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall', 'HKLM:\\SOFTWARE\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall', 'HKCU:\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall'); \
         $reg = $false; foreach ($k in $keys) { if (Get-ChildItem $k -ErrorAction SilentlyContinue | Get-ItemProperty -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -eq 'Copilot' }) { $reg = $true } }; \
         $appx = Get-AppxPackage -Name 'Microsoft.Copilot*' -ErrorAction SilentlyContinue; \
         if ($appx -or $reg) { 'yes' } else { 'no' }",
    ) {
        Ok(t) => t.trim() == "yes",
        Err(_) => false,
    }
}

/// Uninstall the Copilot app, trying every install form in turn:
/// Store winget IDs, winget by name (catches Win32 installs),
/// the Programs and Features UninstallString for the exact
/// "Copilot" entry (MSI runs quiet; other uninstallers run as-is
/// so the user can confirm), then AppxPackage. After each attempt
/// it checks whether Copilot is actually gone before claiming
/// success — v1.0.11 claimed success from winget's exit code
/// while a desktop install (e.g. v156.x, 2 GB, in Programs and
/// Features) was still sitting there.
fn uninstall_copilot_app() -> String {
    // Close a running Copilot first: an uninstaller can fail
    // (or refuse) while its app is open. Best-effort, ignored.
    let _ = Command::new("taskkill")
        .args(["/IM", "Copilot.exe", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .status();
    // No early "not installed" exit: v1.0.12 returned that when
    // its presence check missed the install form, skipping every
    // attempt. Try everything, then check presence at the end.
    for id in [
        "Microsoft.Copilot_8wekyb3d8bbwe",
        "Microsoft.Copilot",
    ] {
        if let Ok(s) = Command::new("winget")
            .args([
                "uninstall",
                "--id",
                id,
                "--silent",
                "--accept-source-agreements",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
        {
            if s.success() && !copilot_still_present() {
                return format!("Copilot app uninstalled via winget ({id}).");
            }
        }
    }
    for exact in [false, true] {
        let mut args: Vec<&str> = vec!["uninstall", "--name", "Copilot", "--silent", "--accept-source-agreements"];
        if exact {
            args.push("--exact");
        }
        if let Ok(s) = Command::new("winget")
            .args(&args)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
        {
            if s.success() && !copilot_still_present() {
                return "Copilot app uninstalled via winget (by name).".into();
            }
        }
    }
    // Programs and Features UninstallString, exact DisplayName
    // only. This is NOT silent: Copilot's own uninstaller shows
    // an "Are you sure you want to uninstall Copilot?" dialog,
    // exactly as Programs and Features does — the user clicks
    // Yes, and -Wait holds until that dialog finishes. The old
    // code wrapped the whole string in cmd /c quotes, which
    // broke paths-with-arguments; now the executable and its
    // arguments are split properly (quoted path first, then
    // the rest as arguments).
    let ps = r#"
$keys = @('HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall', 'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall', 'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall')
foreach ($k in $keys) {
    $e = Get-ChildItem $k -ErrorAction SilentlyContinue | Get-ItemProperty -ErrorAction SilentlyContinue | Where-Object { $_.DisplayName -eq 'Copilot' } | Select-Object -First 1
    if ($e -and $e.UninstallString) {
        $u = $e.UninstallString.Trim()
        if ($u -match 'msiexec') {
            $guid = [regex]::Match($u, '\{[0-9A-Fa-f\-]+\}').Value
            if ($guid) { Start-Process 'msiexec.exe' -ArgumentList "/X$guid /quiet /norestart" -Wait }
        } elseif ($u -match '^\s*"([^"]+)"\s*(.*)$') {
            Start-Process $Matches[1] -ArgumentList $Matches[2] -Wait -ErrorAction Continue
        } else {
            $parts = $u -split '\s+', 2
            if ($parts.Count -gt 1) { Start-Process $parts[0] -ArgumentList $parts[1] -Wait -ErrorAction Continue } else { Start-Process $parts[0] -Wait -ErrorAction Continue }
        }
        break
    }
}
"#;
    if Command::new("powershell")
        .args(["-NoProfile", "-Command", ps])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok()
        && !copilot_still_present()
    {
        return "Copilot app uninstalled via its Programs and Features uninstaller.".into();
    }
    match Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-AppxPackage -Name 'Microsoft.Copilot*' | Remove-AppxPackage -ErrorAction Continue",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
    {
        Ok(_) if !copilot_still_present() => "Copilot app uninstalled (AppxPackage).".into(),
        _ => "Copilot app uninstall did not confirm — it is still present. If its uninstall dialog appeared, answer Yes; otherwise uninstall it in Programs and Features (select Copilot > Uninstall), which shows the same dialog.".into(),
    }
}

/// Reinstall the Copilot app via winget. Needs internet and can
/// take a few minutes; the caller warns about that in the UI.
/// Returns a human-readable outcome for the log.
fn install_copilot_app() -> String {
    for id in [
        "Microsoft.Copilot_8wekyb3d8bbwe",
        "Microsoft.Copilot",
    ] {
        if let Ok(s) = Command::new("winget")
            .args([
                "install",
                "--id",
                id,
                "--silent",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
        {
            if s.success() {
                return format!("Copilot app reinstalled via winget ({id}).");
            }
        }
    }
    if let Ok(s) = Command::new("winget")
        .args([
            "install",
            "--name",
            "Copilot",
            "--silent",
            "--accept-source-agreements",
            "--accept-package-agreements",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
    {
        if s.success() {
            return "Copilot app reinstalled via winget (by name).".into();
        }
    }
    "Copilot app reinstall via winget did not confirm — install it from the Microsoft Store (search 'Microsoft Copilot') if it is still missing.".into()
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

/// Remove the pinned Copilot app icon from the taskbar, best-effort.
/// The policy + button values above do not touch the pinned app
/// icon — that pin lives as a shortcut in the per-user pinned-taskbar
/// folder, and Explorer rebuilds the taskbar pins from that folder
/// on restart. Deleting the Copilot link (matched by name, so no
/// other pin is touched) removes the pin. Returns true if a link
/// was removed. This deliberately does NOT touch the Taskband
/// registry blob: that binary format is undocumented, and a bad edit
/// there can wipe every pinned icon at once. If no link is found
/// (some builds keep pins only in Taskband), the caller tells the
/// user to unpin manually with right-click -> Unpin from taskbar.
fn unpin_copilot_from_taskbar() -> bool {
    let appdata = match std::env::var("APPDATA") {
        Ok(v) => v,
        Err(_) => return false,
    };
    let dir = std::path::Path::new(&appdata)
        .join(r"Microsoft\Internet Explorer\Quick Launch\User Pinned\TaskBar");
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let mut removed = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.contains("copilot") && name.ends_with(".lnk") {
            if std::fs::remove_file(entry.path()).is_ok() {
                removed = true;
            }
        }
    }
    removed
}

/// The app's backup folder: Documents\Utility Closet. Documents is
/// read from the Shell Folders registry value (which follows OneDrive
/// redirection) with a USERPROFILE\Documents fallback.
fn backup_dir() -> std::path::PathBuf {
    let mut docs: Option<std::path::PathBuf> = None;
    if let Ok(out) = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders",
            "/v",
            "Personal",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.contains("Personal") {
                if let Some(v) = line.split("REG_SZ").nth(1) {
                    let v = v.trim();
                    if !v.is_empty() {
                        docs = Some(std::path::PathBuf::from(v));
                    }
                }
            }
        }
    }
    let base = docs.unwrap_or_else(|| {
        let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join("Documents")
    });
    base.join("Utility Closet")
}

fn backup_file_path() -> std::path::PathBuf {
    backup_dir().join("UtilityCloset-Backup.json")
}

/// Run a PowerShell snippet hidden and return its stdout (trimmed).
/// Used for live system snapshots (drives, printers, monitors).
fn run_ps(script: &str) -> Result<String, String> {
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("Failed to run PowerShell: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A serde_json value that may be a single object or an array of
/// objects (ConvertTo-Json emits an object, not an array, for one
/// item) — always returned as an array.
fn json_array(v: serde_json::Value) -> Vec<serde_json::Value> {
    match v {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Null => Vec::new(),
        other => vec![other],
    }
}

fn json_str(v: &serde_json::Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn json_bool(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn json_i64(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

/// Live monitor snapshot, one query for both Detect and Export.
/// Uses EnumDisplayDevices + EnumDisplaySettings (current mode),
/// NOT Screen::AllScreens — that only gives Bounds, which hides
/// rotation: a portrait monitor's Bounds are already swapped, so
/// a restore that only sets width/height comes back landscape
/// (or fails) because dmDisplayOrientation is never set. This
/// query also gives refresh rate, which AllScreens never had.
/// Fields per display: device, primary, x, y, width, height,
/// bits, orientation (0=0°, 1=90°, 2=180°, 3=270°), refresh (Hz).
fn snapshot_monitors() -> Result<Vec<serde_json::Value>, String> {
    let script = r#"
$code = @'
using System;
using System.Runtime.InteropServices;
public static class DispSnap {
    [StructLayout(LayoutKind.Sequential)]
    public struct POINTL { public int x; public int y; }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DEVMODE {
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public short dmSpecVersion; public short dmDriverVersion; public short dmSize; public short dmDriverExtra; public int dmFields;
        public short dmOrientation; public short dmPaperSize; public short dmPaperLength; public short dmPaperWidth; public short dmScale; public short dmCopies; public short dmDefaultSource; public short dmPrintQuality;
        public short dmColor; public short dmDuplex; public short dmYResolution; public short dmTTOption; public short dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public short dmLogPixels; public int dmBitsPerPel; public int dmPelsWidth; public int dmPelsHeight;
        public int dmDisplayFlags; public int dmDisplayFrequency; public int dmICMMethod; public int dmICMIntent; public int dmMediaType; public int dmDitherType; public int dmReserved1; public int dmReserved2; public int dmPanningWidth; public int dmPanningHeight;
        public POINTL dmPosition;
    }
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DISPLAY_DEVICE {
        public int cb;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string DeviceName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceString;
        public int StateFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceID;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceKey;
    }
    [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern bool EnumDisplayDevices(string lpDevice, uint iDevNum, ref DISPLAY_DEVICE lpDisplayDevice, uint dwFlags);
    [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern bool EnumDisplaySettings(string lpszDeviceName, int iModeNum, ref DEVMODE lpDevMode);
}
'@
Add-Type -TypeDefinition $code
$out = @()
for ($i = 0; $true; $i++) {
    $dd = New-Object DispSnap+DISPLAY_DEVICE; $dd.cb = [System.Runtime.InteropServices.Marshal]::SizeOf($dd)
    if (-not [DispSnap]::EnumDisplayDevices($null, $i, [ref]$dd, 0)) { break }
    if (($dd.StateFlags -band 1) -eq 0) { continue }
    $dm = New-Object DispSnap+DEVMODE; $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
    if (-not [DispSnap]::EnumDisplaySettings($dd.DeviceName, -1, [ref]$dm)) { continue }
    $out += [PSCustomObject]@{
        DeviceName = $dd.DeviceName; Primary = (($dd.StateFlags -band 4) -ne 0)
        X = $dm.dmPosition.x; Y = $dm.dmPosition.y
        Width = $dm.dmPelsWidth; Height = $dm.dmPelsHeight
        BitsPerPixel = $dm.dmBitsPerPel; Orientation = $dm.dmDisplayOrientation; Refresh = $dm.dmDisplayFrequency
    }
}
$out | ConvertTo-Json -Compress
"#;
    let text = run_ps(script)?;
    let v = serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|_| "Could not read monitor layout.".to_string())?;
    Ok(json_array(v)
        .iter()
        .map(|item| {
            serde_json::json!({
                "device": json_str(item, "DeviceName"),
                "primary": json_bool(item, "Primary"),
                "x": json_i64(item, "X"),
                "y": json_i64(item, "Y"),
                "width": json_i64(item, "Width"),
                "height": json_i64(item, "Height"),
                "bits": json_i64(item, "BitsPerPixel"),
                "orientation": json_i64(item, "Orientation"),
                "refresh": json_i64(item, "Refresh"),
            })
        })
        .collect())
}

impl ClosetApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(if is_dark_mode() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        {
            let mut style = (*cc.egui_ctx.style()).clone();
            style.text_styles.insert(
                egui::TextStyle::Heading,
                egui::FontId::new(24.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Body,
                egui::FontId::new(15.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Button,
                egui::FontId::new(15.0, egui::FontFamily::Proportional),
            );
            style.text_styles.insert(
                egui::TextStyle::Monospace,
                egui::FontId::new(13.0, egui::FontFamily::Monospace),
            );
            style.visuals.selection.bg_fill = egui::Color32::from_rgb(0, 128, 140);
            style.visuals.widgets.noninteractive.rounding = egui::Rounding::same(8.0);
            cc.egui_ctx.set_style(style);
        }
        let mut app = Self {
            nav: Nav::Overview,
            monitors_text: String::new(),
            drives: Vec::new(),
            printer_rows: vec![PrinterRow { path: String::new() }],
            printers: Vec::new(),
            selected_printer: None,
            forced_shutdown: false,
            instant_close_frozen: false,
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
        // Auto-detect on launch: the list must show drives and
        // network locations this PC already has, whether or not
        // the app configured them — not just what was typed in
        // or pulled in with Load Existing. Load Existing remains
        // as a manual re-scan. Consequence, by design: removing
        // a live drive/location row only hides it until the next
        // launch; use Disconnect to actually remove it.
        app.load_existing_drives();
        // Forced-shutdown toggle reflects the real registry state, not
        // a saved setting (another tool could have changed it).
        app.forced_shutdown = is_forced_shutdown();
        app.instant_close_frozen = is_instant_close_frozen();
        app.welcome_experience = is_welcome_experience_on();
        app.show_all_tray_icons = is_show_all_tray_icons();
        app.copilot_enabled = is_copilot_enabled();
        // Fresh start: one blank X row only. If the user wants more
        // drives, the "Add Drive" button adds them — no prefilled Y row.
        // Saved drives from load_config() above are left untouched.
        if app.drives.is_empty() {
            app.drives.push(DriveRow {
                kind: DriveKind::Drive,
                label: "X".into(),
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
            // Label{i} is the current key; Letter{i} is the pre-1.0.12
            // key, kept so old configs still load (as Drives).
            let label = map
                .get(&format!("Label{i}"))
                .or_else(|| map.get(&format!("Letter{i}")))
                .cloned()
                .unwrap_or_default();
            let kind = match map.get(&format!("Kind{i}")).map(|s| s.as_str()) {
                Some("location") => DriveKind::Location,
                _ => DriveKind::Drive,
            };
            let path = map
                .get(&format!("Path{i}"))
                .cloned()
                .unwrap_or_default();
            self.drives.push(DriveRow { kind, label, path });
        }
    }

    fn save_config(&self) {
        let mut s = format!("Count={}\n", self.drives.len());
        for (i, d) in self.drives.iter().enumerate() {
            let kind = if d.kind == DriveKind::Location {
                "location"
            } else {
                "drive"
            };
            s.push_str(&format!("Kind{i}={kind}\n"));
            s.push_str(&format!("Label{i}={}\n", d.label.trim()));
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
        let rows: Vec<(DriveKind, String, String)> = self
            .drives
            .iter()
            .map(|r| (r.kind, r.label.clone(), r.path.clone()))
            .collect();
        let mut mapped = 0;
        for (kind, label_raw, path_raw) in &rows {
            let path = path_raw.trim();
            if path.is_empty() {
                continue;
            }
            match kind {
                DriveKind::Drive => match Self::normalized_letter(label_raw) {
                    Some(letter) => {
                        self.log_line(&format!("Mapping {letter}: to {path} ..."));
                        let target = format!("{letter}:");
                        if self.run_logged("net", &["use", &target, path, "/persistent:yes"]) {
                            mapped += 1;
                        }
                    }
                    None => {
                        self.log_line("Skipped drive row: invalid drive letter.");
                    }
                },
                DriveKind::Location => {
                    let name = sanitize_location_name(label_raw);
                    self.log_line(&format!("Adding network location '{name}' -> {path} ..."));
                    let script = format!(
                        "$dir = Join-Path $env:APPDATA 'Microsoft\\Windows\\Network Shortcuts'; \
                         New-Item -ItemType Directory -Force $dir | Out-Null; \
                         $s = (New-Object -ComObject WScript.Shell).CreateShortcut((Join-Path $dir '{name}.lnk')); \
                         $s.TargetPath = '{path_esc}'; $s.Save()",
                        name = name.replace('\'', ""),
                        path_esc = path.replace('\'', ""),
                    );
                    match run_ps(&script) {
                        Ok(_) => {
                            mapped += 1;
                            self.log_line(&format!("Network location '{name}' added (This PC > Network locations)."));
                        }
                        Err(e) => self.log_line(&format!("Failed to add network location '{name}': {e}")),
                    }
                }
            }
        }
        for row in &mut self.drives {
            if row.kind == DriveKind::Drive {
                if let Some(l) = Self::normalized_letter(&row.label) {
                    row.label = l;
                }
            }
        }
        if mapped > 0 {
            self.save_config();
            self.log_line("Network list saved for next time.");
        } else {
            self.log_line(r"Nothing mapped. Enter a network path first (e.g. \\SERVER\share).");
        }
    }

    fn disconnect_drives(&mut self) {
        let rows: Vec<(DriveKind, String)> = self
            .drives
            .iter()
            .map(|r| (r.kind, r.label.clone()))
            .collect();
        for (kind, label_raw) in rows {
            match kind {
                DriveKind::Drive => {
                    if let Some(letter) = Self::normalized_letter(&label_raw) {
                        self.log_line(&format!("Disconnecting {letter}: ..."));
                        let target = format!("{letter}:");
                        self.run_logged("net", &["use", &target, "/delete", "/y"]);
                    }
                }
                DriveKind::Location => {
                    let name = sanitize_location_name(&label_raw);
                    if let Some(dir) = network_shortcuts_dir() {
                        let link = dir.join(format!("{name}.lnk"));
                        match std::fs::remove_file(&link) {
                            Ok(_) => self.log_line(&format!("Removed network location '{name}'.")),
                            Err(_) => self.log_line(&format!(
                                "Network location '{name}' shortcut not found at {}.",
                                link.display()
                            )),
                        }
                    }
                }
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

    // -----------------------------------------------------------------------
    // Backup & Restore — Documents\Utility Closet\UtilityCloset-Backup.json
    //
    // Export snapshots the LIVE system, not just what is typed into
    // the app: currently mapped network drives (Get-SmbMapping),
    // currently installed printers (Get-Printer), and the full display
    // topology (Screen::AllScreens + video controllers). App rows
    // that are not currently live are added too, so nothing typed
    // into the app is lost. Import restores drives and printers
    // immediately; monitors are applied live per display (see
    // apply_monitor_layout), with missing displays skipped.
    // -----------------------------------------------------------------------

    /// Fill the drive rows from the PC's currently mapped drives
    /// (Get-SmbMapping), so the app stops looking blank on a PC
    /// that already has drives mapped. Merges by letter, saves.
    fn load_existing_drives(&mut self) {
        let mut loaded_drives = 0;
        match run_ps(
            "Get-SmbMapping | Select-Object LocalPath,RemotePath | ConvertTo-Json -Compress",
        ) {
            Ok(text) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    for item in json_array(v) {
                        let letter = json_str(&item, "LocalPath")
                            .trim_end_matches(':')
                            .to_string();
                        let path = json_str(&item, "RemotePath");
                        if letter.is_empty() || path.is_empty() {
                            continue;
                        }
                        if let Some(row) = self.drives.iter_mut().find(|r| {
                            r.kind == DriveKind::Drive
                                && Self::normalized_letter(&r.label).as_deref() == Some(letter.as_str())
                        }) {
                            row.path = path;
                        } else if self.drives.len() < MAX_DRIVES {
                            self.drives.push(DriveRow {
                                kind: DriveKind::Drive,
                                label: letter,
                                path,
                            });
                        }
                        loaded_drives += 1;
                    }
                } else {
                    self.log_line("Could not read existing mapped drives.");
                }
            }
            Err(e) => self.log_line(&format!("Failed to list existing mapped drives: {e}")),
        }
        // Existing network locations (Network Shortcuts shortcuts).
        let mut loaded_locations = 0;
        for (name, target) in collect_network_locations() {
            if self.drives.iter().any(|r| {
                r.kind == DriveKind::Location && r.path.eq_ignore_ascii_case(&target)
            }) {
                continue;
            }
            if self.drives.len() < MAX_DRIVES {
                self.drives.push(DriveRow {
                    kind: DriveKind::Location,
                    label: name,
                    path: target,
                });
                loaded_locations += 1;
            }
        }
        if loaded_drives > 0 || loaded_locations > 0 {
            let has_real = self.drives.iter().any(|x| !x.path.trim().is_empty());
            if has_real {
                self.drives.retain(|r| !r.path.trim().is_empty());
            }
            self.save_config();
        }
        self.log_line(&format!(
            "Loaded {loaded_drives} existing mapped drive(s) and {loaded_locations} network location(s) into the list."
        ));
    }

    /// Detect the current monitors (full topology, same query as
    /// Export uses) and show them in the Backup section. Also
    /// writes Documents\Utility Closet\Monitor-Layout.json so the
    /// layout can be applied on its own, without a full backup.
    fn detect_monitors(&mut self) {
        match snapshot_monitors() {
            Ok(monitors) => {
                    let mut summary = String::new();
                    for m in &monitors {
                        let orient = match json_i64(m, "orientation") {
                            1 => ", 90°",
                            2 => ", 180°",
                            3 => ", 270°",
                            _ => "",
                        };
                        let refresh = json_i64(m, "refresh");
                        summary.push_str(&format!(
                            "{} — {}x{} at ({}, {}){}{}{}\n",
                            json_str(m, "device"),
                            json_i64(m, "width"),
                            json_i64(m, "height"),
                            json_i64(m, "x"),
                            json_i64(m, "y"),
                            if json_bool(m, "primary") { " (primary)" } else { "" },
                            orient,
                            if refresh > 0 { format!(", {refresh} Hz") } else { String::new() },
                        ));
                    }
                    self.monitors_text = summary;
                    let dir = backup_dir();
                    if std::fs::create_dir_all(&dir).is_ok() {
                        let doc = serde_json::json!({
                            "format": "UtilityClosetMonitorLayout",
                            "version": 1,
                            "computer": std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
                            "exported": timestamp(),
                            "monitors": monitors,
                        });
                        let path = dir.join("Monitor-Layout.json");
                        if std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap_or_default()).is_ok() {
                            self.log_line(&format!(
                                "Detected {} monitor(s); layout saved to {}.",
                                doc["monitors"].as_array().map(|a| a.len()).unwrap_or(0),
                                path.display()
                            ));
                        }
                    }
            }
            Err(e) => self.log_line(&format!("Monitor detection failed: {e}")),
        }
    }

    fn apply_monitor_layout_file(&mut self) {
        let dir = backup_dir();
        let standalone = dir.join("Monitor-Layout.json");
        let text = std::fs::read_to_string(&standalone)
            .or_else(|_| {
                std::fs::read_to_string(backup_file_path())
            });
        match text {
            Ok(t) => match serde_json::from_str::<serde_json::Value>(&t) {
                Ok(doc) => {
                    let monitors = json_array(doc.get("monitors").cloned().unwrap_or(serde_json::Value::Null));
                    self.apply_monitor_layout(&monitors);
                }
                Err(e) => self.log_line(&format!("Monitor layout file is not readable JSON: {e}")),
            },
            Err(_) => self.log_line("No monitor layout saved yet — click Detect Monitors first."),
        }
    }

    fn export_backup(&mut self) {
        let dir = backup_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.log_line(&format!("Could not create backup folder {}: {e}", dir.display()));
            return;
        }
        // Live mapped drives.
        let mut drives: Vec<serde_json::Value> = Vec::new();
        match run_ps(
            "Get-SmbMapping | Select-Object LocalPath,RemotePath,Status | ConvertTo-Json -Compress",
        ) {
            Ok(text) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    for item in json_array(v) {
                        let letter = json_str(&item, "LocalPath")
                            .trim_end_matches(':')
                            .to_string();
                        let path = json_str(&item, "RemotePath");
                        if !letter.is_empty() && !path.is_empty() {
                            drives.push(serde_json::json!({
                                "letter": letter,
                                "path": path,
                                "source": "live",
                                "status": json_str(&item, "Status"),
                            }));
                        }
                    }
                }
            }
            Err(e) => self.log_line(&format!("Live drive snapshot failed ({e}) — app rows only.")),
        }
        // App network rows not already live.
        for row in &self.drives {
            let path = row.path.trim();
            if path.is_empty() {
                continue;
            }
            match row.kind {
                DriveKind::Drive => {
                    let Some(letter) = Self::normalized_letter(&row.label) else {
                        continue;
                    };
                    if drives.iter().any(|d| json_str(d, "letter") == letter) {
                        continue;
                    }
                    drives.push(serde_json::json!({
                        "kind": "drive", "letter": letter, "label": letter, "path": path, "source": "app", "status": "",
                    }));
                }
                DriveKind::Location => {
                    if drives.iter().any(|d| json_str(d, "path").eq_ignore_ascii_case(path)) {
                        continue;
                    }
                    drives.push(serde_json::json!({
                        "kind": "location", "letter": "", "label": row.label, "path": path, "source": "app", "status": "",
                    }));
                }
            }
        }
        // Live network locations (Network Shortcuts) not already listed.
        for (name, target) in collect_network_locations() {
            if drives.iter().any(|d| json_str(d, "path").eq_ignore_ascii_case(&target)) {
                continue;
            }
            drives.push(serde_json::json!({
                "kind": "location", "letter": "", "label": name, "path": target, "source": "live", "status": "",
            }));
        }
        // Live installed printers.
        let mut printers: Vec<serde_json::Value> = Vec::new();
        match run_ps(
            "Get-Printer | Select-Object Name,PortName,DriverName,Shared | ConvertTo-Json -Compress",
        ) {
            Ok(text) => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    for item in json_array(v) {
                        let name = json_str(&item, "Name");
                        if name.is_empty() {
                            continue;
                        }
                        printers.push(serde_json::json!({
                            "name": name,
                            "port": json_str(&item, "PortName"),
                            "driver": json_str(&item, "DriverName"),
                            "shared": json_bool(&item, "Shared"),
                            "source": "live",
                        }));
                    }
                }
            }
            Err(e) => self.log_line(&format!("Live printer snapshot failed ({e}) — app rows only.")),
        }
        // Queued printer rows not already live (by port).
        for row in &self.printer_rows {
            let path = row.path.trim();
            if path.is_empty() || printers.iter().any(|p| json_str(p, "port") == path) {
                continue;
            }
            printers.push(serde_json::json!({
                "name": path, "port": path, "driver": "", "shared": false, "source": "app",
            }));
        }
        // Full display topology: monitors (position, size, primary,
        // orientation, refresh) + video controllers.
        let mut monitors: Vec<serde_json::Value> = Vec::new();
        match snapshot_monitors() {
            Ok(m) => monitors = m,
            Err(e) => self.log_line(&format!("Monitor snapshot failed: {e}")),
        }
        let mut controllers: Vec<serde_json::Value> = Vec::new();
        if let Ok(text) = run_ps(
            "Get-CimInstance Win32_VideoController | Select-Object Name,CurrentHorizontalResolution,CurrentVerticalResolution,CurrentRefreshRate,CurrentBitsPerPixel | ConvertTo-Json -Compress",
        ) {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                for item in json_array(v) {
                    controllers.push(serde_json::json!({
                        "name": json_str(&item, "Name"),
                        "width": json_i64(&item, "CurrentHorizontalResolution"),
                        "height": json_i64(&item, "CurrentVerticalResolution"),
                        "refresh": json_i64(&item, "CurrentRefreshRate"),
                        "bits": json_i64(&item, "CurrentBitsPerPixel"),
                    }));
                }
            }
        }
        // Display registry keys as a fallback: if the live monitor
        // apply ever fails, these .reg files restore the layout the
        // old way (import + restart). Best-effort, failures only logged.
        for (key, file) in [
            (
                r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration",
                "Display-Configuration.reg",
            ),
            (
                r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Connectivity",
                "Display-Connectivity.reg",
            ),
            (
                r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\ScaleFactors",
                "Display-ScaleFactors.reg",
            ),
        ] {
            let target = dir.join(file);
            let _ = Command::new("reg")
                .args(["export", key, &target.to_string_lossy(), "/y"])
                .creation_flags(CREATE_NO_WINDOW)
                .status();
        }
        let doc = serde_json::json!({
            "format": "UtilityClosetBackup",
            "version": 1,
            "app_version": APP_VERSION,
            "computer": std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into()),
            "exported": timestamp(),
            "drives": drives,
            "printers": printers,
            "monitors": monitors,
            "video_controllers": controllers,
        });
        let path = backup_file_path();
        match std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
            Ok(_) => {
                self.log_line(&format!(
                    "Backup exported to {}: {} drive(s), {} printer(s), {} monitor(s).",
                    path.display(),
                    doc["drives"].as_array().map(|a| a.len()).unwrap_or(0),
                    doc["printers"].as_array().map(|a| a.len()).unwrap_or(0),
                    doc["monitors"].as_array().map(|a| a.len()).unwrap_or(0),
                ));
            }
            Err(e) => self.log_line(&format!("Failed to write backup {}: {e}", path.display())),
        }
    }

    /// Apply a backed-up monitor layout live, per display, via
    /// ChangeDisplaySettingsEx (position, size, primary, orientation,
    /// refresh rate). Displays in the backup that are not
    /// currently connected are skipped, never forced. The primary
    /// display gets CDS_SET_PRIMARY. Returns nothing; every outcome
    /// is logged, and a total failure leaves the layout unchanged
    /// (the .reg files in the backup folder are the fallback).
    fn apply_monitor_layout(&mut self, monitors: &[serde_json::Value]) {
        if monitors.is_empty() {
            self.log_line("Backup has no monitors — monitor layout skipped.");
            return;
        }
        let payload = serde_json::to_string(monitors).unwrap_or_default();
        // PowerShell + user32 P/Invoke. DEVMODE layout matches the
        // Win32 struct through dmDisplayFrequency + dmPosition;
        // dmSize is set from the marshalled size at runtime.
        let script = format!(
            r#"
$ErrorActionPreference = 'Stop'
$code = @'
using System;
using System.Runtime.InteropServices;
public static class Disp {{
    [StructLayout(LayoutKind.Sequential)]
    public struct POINTL {{ public int x; public int y; }}
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DEVMODE {{
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmDeviceName;
        public short dmSpecVersion; public short dmDriverVersion; public short dmSize; public short dmDriverExtra; public int dmFields;
        public short dmOrientation; public short dmPaperSize; public short dmPaperLength; public short dmPaperWidth; public short dmScale; public short dmCopies; public short dmDefaultSource; public short dmPrintQuality;
        public short dmColor; public short dmDuplex; public short dmYResolution; public short dmTTOption; public short dmCollate;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string dmFormName;
        public short dmLogPixels; public int dmBitsPerPel; public int dmPelsWidth; public int dmPelsHeight;
        public int dmDisplayFlags; public int dmDisplayFrequency; public int dmICMMethod; public int dmICMIntent; public int dmMediaType; public int dmDitherType; public int dmReserved1; public int dmReserved2; public int dmPanningWidth; public int dmPanningHeight;
        public POINTL dmPosition;
    }}
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Ansi)]
    public struct DISPLAY_DEVICE {{
        public int cb;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 32)] public string DeviceName;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceString;
        public int StateFlags;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceID;
        [MarshalAs(UnmanagedType.ByValTStr, SizeConst = 128)] public string DeviceKey;
    }}
    [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern bool EnumDisplayDevices(string lpDevice, uint iDevNum, ref DISPLAY_DEVICE lpDisplayDevice, uint dwFlags);
    [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern bool EnumDisplaySettings(string lpszDeviceName, int iModeNum, ref DEVMODE lpDevMode);
    [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern int ChangeDisplaySettingsEx(string lpszDeviceName, ref DEVMODE lpDevMode, IntPtr hwnd, uint dwflags, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int ChangeDisplaySettings(IntPtr devMode, uint flags);
}}
'@
Add-Type -TypeDefinition $code
$monitors = '{payload}' | ConvertFrom-Json
if ($monitors -isnot [array]) {{ $monitors = @($monitors) }}
$results = @()
for ($i = 0; $true; $i++) {{
    $dd = New-Object Disp+DISPLAY_DEVICE; $dd.cb = [System.Runtime.InteropServices.Marshal]::SizeOf($dd)
    if (-not [Disp]::EnumDisplayDevices($null, $i, [ref]$dd, 0)) {{ break }}
    $want = $monitors | Where-Object {{ $_.device -eq $dd.DeviceName }} | Select-Object -First 1
    if (-not $want) {{ $results += "$($dd.DeviceName): not in backup, skipped"; continue }}
    $dm = New-Object Disp+DEVMODE; $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
    if (-not [Disp]::EnumDisplaySettings($dd.DeviceName, -1, [ref]$dm)) {{ $results += "$($dd.DeviceName): could not read current mode"; continue }}
    $dm.dmFields = 0x20 -bor 0x80000 -bor 0x100000
    $dm.dmPelsWidth = [int]$want.width; $dm.dmPelsHeight = [int]$want.height
    $dm.dmPosition = New-Object Disp+POINTL; $dm.dmPosition.x = [int]$want.x; $dm.dmPosition.y = [int]$want.y
    if ([int]$want.bits -gt 0) {{ $dm.dmFields = $dm.dmFields -bor 0x40000; $dm.dmBitsPerPel = [int]$want.bits }}
    if ($null -ne $want.orientation) {{ $dm.dmFields = $dm.dmFields -bor 0x80; $dm.dmDisplayOrientation = [short]$want.orientation }}
    if ([int]$want.refresh -gt 0) {{ $dm.dmFields = $dm.dmFields -bor 0x400000; $dm.dmDisplayFrequency = [int]$want.refresh }}
    $flags = 1 -bor 0x10000000
    if ($want.primary) {{ $flags = $flags -bor 0x10 }}
    $rc = [Disp]::ChangeDisplaySettingsEx($dd.DeviceName, [ref]$dm, [IntPtr]::Zero, $flags, [IntPtr]::Zero)
    $results += "$($dd.DeviceName): ChangeDisplaySettingsEx = $rc (0 = success)"
}}
[void][Disp]::ChangeDisplaySettings([IntPtr]::Zero, 0)
$results -join "`n"
"#,
            payload = payload.replace('\'', "''")
        );
        let tmp = std::env::temp_dir().join("UtilityCloset-MonitorRestore.ps1");
        match std::fs::write(&tmp, &script) {
            Ok(_) => match Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    &tmp.to_string_lossy(),
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output()
            {
                Ok(out) => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    for line in text.lines() {
                        if !line.trim().is_empty() {
                            self.log_line(&format!("Monitor: {line}"));
                        }
                    }
                    if !out.status.success() {
                        let err = String::from_utf8_lossy(&out.stderr);
                        for line in err.lines() {
                            if !line.trim().is_empty() {
                                self.log_line(&format!("Monitor restore error: {line}"));
                            }
                        }
                        self.log_line(
                            "Monitor layout may not have applied — the Display-*.reg files in the backup folder are the fallback (import + restart).",
                        );
                    }
                }
                Err(e) => self.log_line(&format!("Failed to run monitor restore: {e}")),
            },
            Err(e) => self.log_line(&format!("Failed to write monitor restore script: {e}")),
        }
    }

    fn import_backup(&mut self) {
        let path = backup_file_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => {
                self.log_line(&format!(
                    "No backup found at {} — export one first.",
                    path.display()
                ));
                return;
            }
        };
        let doc: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                self.log_line(&format!("Backup file is not readable JSON: {e}"));
                return;
            }
        };
        if json_str(&doc, "format") != "UtilityClosetBackup" {
            self.log_line("This file is not a Utility Closet backup — import cancelled.");
            return;
        }
        let backup_pc = json_str(&doc, "computer");
        let this_pc = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into());
        if !backup_pc.is_empty() && backup_pc != "unknown" && backup_pc != this_pc {
            self.log_line(&format!(
                "Warning: this backup is from PC '{backup_pc}', but this PC is '{this_pc}' — drives, printers, and monitors may differ. Restoring anyway."
            ));
        }
        // Drives: replace app rows, save, then map (live restore).
        let drives = json_array(doc.get("drives").cloned().unwrap_or(serde_json::Value::Null));
        if !drives.is_empty() {
            self.drives = drives
                .iter()
                .filter_map(|d| {
                    let p = json_str(d, "path");
                    if p.is_empty() {
                        return None;
                    }
                    // kind defaults to drive for pre-1.0.12 backups.
                    match json_str(d, "kind").as_str() {
                        "location" => {
                            let label = json_str(d, "label");
                            if label.is_empty() {
                                None
                            } else {
                                Some(DriveRow {
                                    kind: DriveKind::Location,
                                    label,
                                    path: p,
                                })
                            }
                        }
                        _ => {
                            let letter = json_str(d, "letter");
                            if letter.is_empty() {
                                None
                            } else {
                                Some(DriveRow {
                                    kind: DriveKind::Drive,
                                    label: letter,
                                    path: p,
                                })
                            }
                        }
                    }
                })
                .take(MAX_DRIVES)
                .collect();
            self.save_config();
            self.log_line(&format!("Restoring {} drive(s) from backup ...", self.drives.len()));
            self.map_drives();
        } else {
            self.log_line("Backup has no drives — drive restore skipped.");
        }
        // Printers: only network (UNC) ports can be added by this
        // app's printer tool; local/IP printers are logged, not forced.
        let printers = json_array(doc.get("printers").cloned().unwrap_or(serde_json::Value::Null));
        let mut to_add: Vec<String> = Vec::new();
        for p in &printers {
            let port = json_str(p, "port");
            let target = if port.starts_with(r"\\") {
                port
            } else {
                let name = json_str(p, "name");
                if name.starts_with(r"\\") {
                    name
                } else {
                    String::new()
                }
            };
            if !target.is_empty() && !to_add.contains(&target) {
                to_add.push(target);
            }
        }
        if !to_add.is_empty() {
            self.printer_rows = to_add
                .iter()
                .take(MAX_PRINTERS)
                .map(|p| PrinterRow { path: p.clone() })
                .collect();
            self.log_line(&format!("Restoring {} network printer(s) from backup ...", to_add.len()));
            self.add_printers();
        } else {
            self.log_line("Backup has no network printers to add — printer restore skipped (local printers are not added by UNC path).");
        }
        // Monitors: live apply, missing displays skipped.
        let monitors = json_array(doc.get("monitors").cloned().unwrap_or(serde_json::Value::Null));
        self.apply_monitor_layout(&monitors);
        self.log_line(&format!("Import complete from {}.", path.display()));
    }
    fn section_overview(&mut self, ui: &mut egui::Ui) {
        ui.heading("Overview");
        ui.label("Everything at a glance — pick a section on the left for the full controls.");
        ui.add_space(8.0);
        let drives_configured = self.drives.iter().filter(|r| !r.path.trim().is_empty()).count();
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Network Drives & Locations").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            ui.label(format!("{drives_configured} drive(s)/location(s) in the app list, including ones this PC already had."));
            if ui.button("Open Network Drives & Locations").clicked() {
                self.nav = Nav::Drives;
            }
        });
        ui.add_space(12.0);
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Printers").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            ui.label(format!("{} printer(s) listed (Refresh List updates this).", self.printers.len()));
            ui.horizontal(|ui| {
                if ui.button("Refresh List").clicked() {
                    self.refresh_printers();
                }
                if ui.button("Open Printers").clicked() {
                    self.nav = Nav::Printers;
                }
            });
        });
        ui.add_space(12.0);
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Backup & Restore").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            ui.label(format!("Backups live in {}", backup_file_path().display()));
            ui.horizontal(|ui| {
                if ui.button("Export Backup").clicked() {
                    self.export_backup();
                }
                if ui.button("Open Backup").clicked() {
                    self.nav = Nav::Backup;
                }
            });
        });
        ui.add_space(12.0);
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Windows Annoyances").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            ui.label(format!(
                "Forced shutdown {} \u{00b7} Instant close {} \u{00b7} Welcome experience {} \u{00b7} Tray icons {} \u{00b7} Copilot {}",
                if self.forced_shutdown { "on" } else { "off" },
                if self.instant_close_frozen { "on" } else { "off" },
                if self.welcome_experience { "on" } else { "off" },
                if self.show_all_tray_icons { "all shown" } else { "normal" },
                if self.copilot_enabled { "on" } else { "off" },
            ));
            if ui.button("Open Windows Annoyances").clicked() {
                self.nav = Nav::Annoyances;
            }
        });
    }

    fn section_drives(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Network Drives & Locations").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            let mut remove_idx: Option<usize> = None;
            for (i, row) in self.drives.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_source(format!("drive_kind{i}"))
                        .selected_text(if row.kind == DriveKind::Drive {
                            "Drive"
                        } else {
                            "Location"
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut row.kind, DriveKind::Drive, "Drive (letter)");
                            ui.selectable_value(
                                &mut row.kind,
                                DriveKind::Location,
                                "Location (This PC)",
                            );
                        });
                    let (label_width, label_hint) = if row.kind == DriveKind::Drive {
                        (36.0, "X")
                    } else {
                        (140.0, "Name")
                    };
                    ui.add(
                        egui::TextEdit::singleline(&mut row.label)
                            .desired_width(label_width)
                            .hint_text(label_hint),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut row.path)
                            .desired_width(320.0)
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
            let (mut do_add, mut do_map, mut do_disco, mut do_load) = (false, false, false, false);
            ui.horizontal(|ui| {
                do_add = ui.button("Add Drive").clicked();
                do_map = ui.button("Map Drives").clicked();
                do_disco = ui.button("Disconnect").clicked();
                do_load = ui.button("Load Existing").clicked();
            });
            if do_load {
                self.load_existing_drives();
            }
            if do_add && self.drives.len() < MAX_DRIVES {
                self.drives.push(DriveRow {
                    kind: DriveKind::Drive,
                    label: String::new(),
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
                "Drive = a letter-mapped drive (net use, e.g. X:). Location = a \
                 network location shortcut in This PC > Network locations, with \
                 no drive letter — just a name and a UNC path. This list always \
                 includes drives and locations this PC already has, whether or \
                 not the app configured them — it is scanned on launch, and \
                 Load Existing re-scans it. Removing a row only hides it until \
                 the next launch; Disconnect actually removes it. Map Drives \
                 adds both kinds; Disconnect removes both kinds.",
            );
        });

    }

    fn section_printers(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Printers").strong().color(egui::Color32::from_rgb(0, 128, 140)));
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

    }

    fn section_backup(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Backup & Restore").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            ui.horizontal(|ui| {
                if ui.button("Export Backup").clicked() {
                    self.export_backup();
                }
                if ui.button("Import Backup").clicked() {
                    self.import_backup();
                }
                if ui.button("Open Backup Folder").clicked() {
                    let dir = backup_dir();
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = Command::new("explorer.exe")
                        .arg(dir)
                        .creation_flags(CREATE_NO_WINDOW)
                        .spawn();
                }
            });
            ui.label(format!(
                "Saves to {}",
                backup_file_path().display()
            ));
            ui.label(
                "Export snapshots what is actually on this PC right now: \
                 mapped network drives, installed printers, and the full \
                 monitor layout (position, size, primary display, orientation, refresh rate) — plus \
                 any drive/printer rows typed into the app that are not \
                 currently live. Import maps those drives back, re-adds \
                 the network printers, and applies the monitor layout to \
                 displays that are currently connected (missing displays \
                 are skipped, never forced). Display registry keys are \
                 also saved as Display-*.reg files in the same folder as \
                 a fallback.",
            );
        });
        ui.add_space(12.0);
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Monitor Layout").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            ui.horizontal(|ui| {
                if ui.button("Detect Monitors").clicked() {
                    self.detect_monitors();
                }
                if ui.button("Apply Monitor Layout").clicked() {
                    self.apply_monitor_layout_file();
                }
            });
            if self.monitors_text.is_empty() {
                ui.label("Click Detect Monitors to list this PC's displays (device, resolution, position, primary, orientation, refresh rate) — that list is what gets backed up and restored.");
            } else {
                ui.label("Detected monitors:");
                ui.add(
                    egui::TextEdit::multiline(&mut self.monitors_text)
                        .desired_width(f32::INFINITY)
                        .font(egui::TextStyle::Monospace)
                        .interactive(false),
                );
            }
            ui.label(
                "Detect also saves Monitor-Layout.json in the backup folder, so \
                 the layout can be applied on its own without a full backup. \
                 Apply only affects displays that are currently connected; \
                 missing displays are skipped, never forced.",
            );
        });

    }

    fn section_repairs(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("System File Checker").strong().color(egui::Color32::from_rgb(0, 128, 140)));
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

        ui.add_space(12.0);
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("DISM — Windows Image Repair").strong().color(egui::Color32::from_rgb(0, 128, 140)));
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

        ui.add_space(12.0);
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Display Cache").strong().color(egui::Color32::from_rgb(0, 128, 140)));
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

        ui.add_space(12.0);
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Icon Cache").strong().color(egui::Color32::from_rgb(0, 128, 140)));
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

    }

    fn section_annoyances(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(
                egui::RichText::new("Windows Annoyances").strong().color(egui::Color32::from_rgb(0, 128, 140)),
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
                    // Shared hung-app keys: re-read the instant-close
                    // toggle so it never claims a state the registry
                    // no longer has (forced OFF resets HungAppTimeout
                    // to 5000, so instant close is off after that).
                    self.instant_close_frozen = is_instant_close_frozen();
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
            // Instant close for frozen apps, same pattern as above.
            if ui
                .checkbox(
                    &mut self.instant_close_frozen,
                    "Instantly close frozen apps (no waiting)",
                )
                .changed()
            {
                let on = self.instant_close_frozen;
                if apply_instant_close_frozen(on) {
                    // Shared keys: turning this on also sets
                    // AutoEndTasks=1, so Force shutdown reads on
                    // too; turning it off restores the defaults,
                    // so Force shutdown reads off too.
                    self.forced_shutdown = is_forced_shutdown();
                    if on {
                        self.log_line(
                            "Instant close ON: a not-responding app is ended \
                             after about 1 second (HungAppTimeout, \
                             WaitToKillAppTimeout and service timeout all \
                             1000 ms) instead of Windows waiting ~5–20 seconds.",
                        );
                    } else {
                        self.log_line(
                            "Instant close OFF: frozen-app and shutdown \
                             timeouts restored to Windows defaults.",
                        );
                    }
                } else {
                    self.instant_close_frozen = !on;
                    self.log_line(
                        "Failed to change the instant-close setting — \
                         the toggle was left unchanged.",
                    );
                }
            }
            ui.label(
                "With this on, Windows treats an app as frozen and ends \
                 it after about 1 second, instead of waiting several \
                 seconds (and, at shutdown, up to 20 seconds) before \
                 acting. Applies immediately — no reboot needed.",
            );
            ui.label(
                "Warning: with this on, an app that is merely slow (not \
                 truly frozen) can be ended sooner, and unsaved work \
                 in it can be lost. This shares Windows' hung-app \
                 settings with Force shutdown above, so turning it on \
                 also turns Force shutdown on, and turning Force \
                 shutdown off turns this off.",
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
                            "Microsoft Copilot ON: policy cleared — reinstalling \
                             the Copilot app (winget, needs internet; this can \
                             take a few minutes and the app may pause).",
                        );
                        let msg = install_copilot_app();
                        self.log_line(&msg);
                    } else {
                        self.log_line(
                            "Microsoft Copilot OFF: policy set — uninstalling \
                             the Copilot app (the old policy/button alone does \
                             not remove the standalone app's taskbar icon).",
                        );
                        let msg = uninstall_copilot_app();
                        self.log_line(&msg);
                        if unpin_copilot_from_taskbar() {
                            self.log_line(
                                "Copilot taskbar pin removed — the pinned \
                                 Copilot app icon will be gone once Explorer restarts.",
                            );
                        } else {
                            self.log_line(
                                "No Copilot taskbar pin link found — if the \
                                 Copilot icon is still on the taskbar, right-click \
                                 it and choose Unpin from taskbar.",
                            );
                        }
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
                "Turning it off uninstalls the Microsoft Copilot app in \
                 whatever form it is installed — Store app, winget by name, \
                 or its Programs and Features uninstaller — and sets the \
                 TurnOffWindowsCopilot policy; turning it back on clears \
                 the policy and reinstalls the app via winget (needs \
                 internet, can take a few minutes). Applying this restarts \
                 Explorer — the taskbar will briefly disappear and come back.",
            );
            ui.label(
                "Caveat: this covers the Microsoft Copilot app only, not \
                 Microsoft 365 Copilot (a different app, left alone). A \
                 major Windows update can occasionally reset policy keys \
                 or re-provision the app, so the toggle may need a re-click \
                 after one of those. Turning it off also removes the \
                 pinned Copilot icon from the taskbar (best-effort, via \
                 the pinned-shortcut folder); turning it back on does \
                 not re-pin it — re-pin from Start if you want it back.",
            );
            ui.add_space(6.0);
            // Standalone uninstall, independent of the checkbox:
            // if the checkbox is already OFF, there is no OFF
            // transition left to trigger an uninstall, so a
            // still-installed Copilot could never be retried.
            // This button runs the same uninstall on demand.
            if ui.button("Uninstall Copilot app now").clicked() {
                self.log_line("Uninstalling the Copilot app on demand (all install forms; its own uninstall dialog may appear — answer Yes).");
                let msg = uninstall_copilot_app();
                self.log_line(&msg);
                if unpin_copilot_from_taskbar() {
                    self.log_line("Copilot taskbar pin removed.");
                }
            }
            ui.add_space(6.0);
            // Reset to WINDOWS defaults — not the user's preferred
            // settings. Each toggle follows the same revert-on-failure
            // pattern as its checkbox; Explorer restarts at most
            // once, at the end, and only if the tray-icons state
            // actually changed.
            if ui.button("Reset to defaults").clicked() {
                let tray_before = self.show_all_tray_icons;
                let copilot_before = self.copilot_enabled;
                self.log_line(
                    "Resetting Windows Annoyances to Windows defaults \
                     (not your preferred settings): forced shutdown OFF, \
                     instant close OFF, welcome experience ON, show all \
                     tray icons OFF, Microsoft Copilot ON.",
                );
                if apply_forced_shutdown(false) {
                    self.forced_shutdown = false;
                    self.instant_close_frozen = false;
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
                // Copilot was the one toggle Reset skipped: Windows'
                // default is Copilot ON, so Reset now clears the
                // policy, shows the taskbar button again, and
                // reinstalls the app (winget, can take a few
                // minutes) — same as flipping the toggle on.
                if apply_copilot_enabled(true) {
                    self.copilot_enabled = true;
                    self.log_line(
                        "Microsoft Copilot reset to ON (Windows default: \
                         policy cleared, taskbar button back).",
                    );
                    let msg = install_copilot_app();
                    self.log_line(&msg);
                } else {
                    self.log_line(
                        "Failed to reset Microsoft Copilot — that toggle \
                         was left unchanged.",
                    );
                }
                if tray_before != self.show_all_tray_icons
                    || copilot_before != self.copilot_enabled
                {
                    self.log_line(
                        "Restarting Explorer so the taskbar picks up the \
                         reset (the taskbar will briefly disappear and \
                         come back).",
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
                 preferred settings: forced shutdown OFF, instant close \
                 OFF, welcome experience ON (the post-update setup screen \
                 is allowed again — that is the Windows default, even \
                 though you will usually have it off), show all tray \
                 icons OFF, and Microsoft Copilot ON (reinstalled if it \
                 was removed). This covers every toggle on this page. \
                 If the tray-icons or Copilot state changed, Explorer \
                 restarts once so the taskbar picks it up.",
            );
        });

    }

    fn section_log(&mut self, ui: &mut egui::Ui) {
        egui::Frame::group(ui.style()).rounding(egui::Rounding::same(12.0)).inner_margin(egui::Margin::same(12.0)).show(ui, |ui| {
            ui.label(egui::RichText::new("Log").strong().color(egui::Color32::from_rgb(0, 128, 140)));
            egui::ScrollArea::vertical().max_height(600.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.log)
                        .desired_width(f32::INFINITY)
                        .desired_rows(24)
                        .font(egui::TextStyle::Monospace)
                        .interactive(false),
                );
            });
        });

    }

    fn start_update_check(&mut self) {
        let (tx2, rx2) = mpsc::channel();
        self.update_rx = Some(rx2);
        self.update_dismissed = false;
        self.update_info = None;
        self.update_status = "Checking\u{2026}".into();
        self.update_pending = true;
        thread::spawn(move || check_updates(tx2));
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

        egui::SidePanel::left("nav_panel")
            .exact_width(210.0)
            .show(ctx, |ui| {
                ui.heading("Utility Closet");
                ui.label(format!("v{APP_VERSION}"));
                ui.separator();
                for (label, nav) in [
                    ("Overview", Nav::Overview),
                    ("Network Drives & Locations", Nav::Drives),
                    ("Printers", Nav::Printers),
                    ("Backup & Restore", Nav::Backup),
                    ("Repairs", Nav::Repairs),
                    ("Windows Annoyances", Nav::Annoyances),
                    ("Log", Nav::Log),
                ] {
                    if ui.selectable_label(self.nav == nav, label).clicked() {
                        self.nav = nav;
                    }
                }
                ui.separator();
                if ui.button("Check for updates").clicked() {
                    self.start_update_check();
                }
                if !self.update_status.is_empty() {
                    ui.label(&self.update_status);
                }
                ui.separator();
                if ui.button("About").clicked() {
                    self.show_about = true;
                }
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
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

            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                match self.nav {
                    Nav::Overview => self.section_overview(ui),
                    Nav::Drives => self.section_drives(ui),
                    Nav::Printers => self.section_printers(ui),
                    Nav::Backup => self.section_backup(ui),
                    Nav::Repairs => self.section_repairs(ui),
                    Nav::Annoyances => self.section_annoyances(ui),
                    Nav::Log => self.section_log(ui),
                }
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
                             • Network drive and network location mapping\n\
                             • Printer add/remove\n\
                             • System File Checker (sfc /scannow)\n\
                             • DISM Windows image repair (Check / Scan / Restore Health)\n\
                             • Display cache reset\n\
                             • Icon cache rebuild\n\
                             • Forced shutdown toggle\n\
                             • Windows welcome experience toggle\n\
                             • Always show all tray icons toggle\n\
                             • Windows Annoyances Reset to defaults\n\
                             • Microsoft Copilot enable/disable toggle (off uninstalls the app, on reinstalls it)\n\
                             • Backup & restore (drives, printers, monitor layout) to Documents\\Utility Closet\n\
                             • Sidebar navigation + Overview dashboard"
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
        .with_inner_size([1040.0, 780.0]);
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
