//! The Utility Closet — Rust core (RustDesk-style: Rust logic, Flutter UI).
//!
//! Extracted from the egui app (~/workspace/closet-rs/src/main.rs, v1.0.23,
//! repo github.com/shawnrodis/UtilityCloset, branch master), with ALL egui
//! UI code stripped. The Flutter UI calls this library through a plain C ABI
//! (see the `uc_*` exports at the bottom) via Dart's ffi package, exactly
//! like FileNamerAI's `fna_*` pattern. No flutter_rust_bridge codegen.
//!
//! Behaviour is the egui logic, unchanged wherever possible: registry
//! toggles, net-use drive mapping, Network Shortcuts locations, printer
//! add/remove, SFC/DISM, display/icon cache tools, backup export/import
//! (live Get-SmbMapping / Get-Printer / monitor snapshot), monitor layout
//! apply via ChangeDisplaySettingsEx, Copilot install/uninstall, and the
//! GitHub update check.
//!
//! Differences from the egui app, all deliberate and all in the UI layer's
//! old job, not in the logic:
//! - Log lines are returned WITHOUT timestamps; the Flutter UI adds
//!   [HH:MM:SS] and caps the log at ~32 KB newest, matching egui's
//!   `log_line`.
//! - `uc_check_update` implements the check only. Downloading the
//!   installer with progress lives in Dart, like the FileNamerAI split
//!   (egui's `download_update` is NOT ported here).
//! - Explorer restarts that egui performed inline inside the tray-icons
//!   toggle, the Copilot toggle, and Reset are NOT done inside the core
//!   `uc_apply_*` / `uc_reset_defaults` calls: those return state (and
//!   `explorer_restart_needed` for reset) and the Flutter UI calls
//!   `uc_restart_explorer()` once, so the taskbar never restarts twice.
//!   The one exception is `uc_rebuild_icon_cache`, which restarts
//!   Explorer itself exactly as egui did, because the restart is part
//!   of that tool, not a side effect of a toggle.
//! - `uc_apply_copilot_enabled` writes the registry policy/button values
//!   ONLY. In egui the checkbox handler then uninstalled (OFF) or
//!   reinstalled (ON) the standalone Copilot app and unpinned it; the
//!   Flutter UI must call `uc_uninstall_copilot_app` /
//!   `uc_install_copilot_app` / `uc_unpin_copilot_from_taskbar` after a
//!   successful apply, and `uc_restart_explorer()` after that.
//! - On non-Windows the crate still compiles: `CREATE_NO_WINDOW` is
//!   cfg-gated, and registry/PowerShell calls fail gracefully (the
//!   `reg`/`powershell`/`net` binaries are simply absent), returning the
//!   same safe defaults egui used on command error. Windows behaviour
//!   itself was NOT executed on this Linux VM — see the delivery report.

use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

#[cfg_attr(not(windows), allow(dead_code))]
const CREATE_NO_WINDOW: u32 = 0x08000000;
const MAX_DRIVES: usize = 12;
const MAX_PRINTERS: usize = 6;
const APP_VERSION: &str = "1.1.3";
const UPDATE_REPO: &str = "shawnrodis/UtilityCloset";
const DESKTOP_KEY: &str = r"HKCU\Control Panel\Desktop";

#[cfg(windows)]
fn no_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
}
#[cfg(not(windows))]
fn no_window(_cmd: &mut Command) {}

fn new_cmd(program: &str) -> Command {
    let mut c = Command::new(program);
    no_window(&mut c);
    c
}

// ---------------------------------------------------------------------------
// JSON helpers (verbatim semantics from egui)
// ---------------------------------------------------------------------------

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

fn timestamp() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn computer_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".into())
}

/// Run a PowerShell snippet hidden and return its stdout (trimmed).
/// Used for live system snapshots (drives, printers, monitors).
fn run_ps(script: &str) -> Result<String, String> {
    let out = new_cmd("powershell")
        .args(["-NoProfile", "-Command", script])
        .output()
        .map_err(|e| format!("Failed to run PowerShell: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run a console tool hidden, collecting its output lines.
/// True only when some line contains "successfully" AND the process
/// succeeded — exactly egui's `run_logged` (which is why a silent
/// tool such as rundll32 printui reports not-ok; see uc_add_printers).
fn run_logged(program: &str, args: &[&str]) -> (bool, Vec<String>) {
    match new_cmd(program).args(args).output() {
        Ok(o) => {
            let mut ok = false;
            let mut log = Vec::new();
            for chunk in [&o.stdout, &o.stderr] {
                for line in String::from_utf8_lossy(chunk).lines() {
                    let t = line.trim();
                    if !t.is_empty() {
                        if t.contains("successfully") {
                            ok = true;
                        }
                        log.push(t.to_string());
                    }
                }
            }
            (ok && o.status.success(), log)
        }
        Err(e) => (false, vec![format!("Failed to run {program}: {e}")]),
    }
}

fn reg_add(key: &str, name: &str, typ: &str, value: &str) -> bool {
    matches!(
        new_cmd("reg")
            .args(["add", key, "/v", name, "/t", typ, "/d", value, "/f"])
            .status(),
        Ok(s) if s.success()
    )
}

fn reg_query(key: &str, name: &str) -> Result<String, String> {
    let out = new_cmd("reg")
        .args(["query", key, "/v", name])
        .output()
        .map_err(|e| format!("Failed to run reg: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Last whitespace token of the `reg query` line mentioning `name`.
fn reg_value_last(key: &str, name: &str) -> Option<String> {
    let text = reg_query(key, name).ok()?;
    text.lines()
        .find(|l| l.contains(name))
        .and_then(|l| l.split_whitespace().last().map(|s| s.to_string()))
}

// ---------------------------------------------------------------------------
// Drives
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriveRow {
    pub kind: String, // "drive" | "location"
    pub label: String,
    pub path: String,
}

impl DriveRow {
    fn drive(label: String, path: String) -> Self {
        Self { kind: "drive".into(), label, path }
    }
    fn location(label: String, path: String) -> Self {
        Self { kind: "location".into(), label, path }
    }
    fn is_location(&self) -> bool {
        self.kind == "location"
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "kind": self.kind, "label": self.label, "path": self.path })
    }
}

fn drives_to_json(drives: &[DriveRow]) -> serde_json::Value {
    serde_json::Value::Array(drives.iter().map(|d| d.to_json()).collect())
}

/// Parse a drives list from FFI input: a bare array of
/// {kind,label,path} objects, or an object with a "drives" array.
/// `label` falls back to `letter` (backup-file shape), `path` falls
/// back to `remote_path`.
fn parse_drives(input: &str) -> Vec<DriveRow> {
    let v: serde_json::Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let items = match &v {
        serde_json::Value::Array(_) => json_array(v),
        serde_json::Value::Object(_) => {
            json_array(v.get("drives").cloned().unwrap_or(serde_json::Value::Null))
        }
        _ => Vec::new(),
    };
    items
        .iter()
        .map(|item| {
            let kind = match json_str(item, "kind").as_str() {
                "location" => "location".to_string(),
                _ => "drive".to_string(),
            };
            let label = {
                let l = json_str(item, "label");
                if l.is_empty() {
                    json_str(item, "letter")
                } else {
                    l
                }
            };
            let path = {
                let p = json_str(item, "path");
                if p.is_empty() {
                    json_str(item, "remote_path")
                } else {
                    p
                }
            };
            DriveRow { kind, label, path }
        })
        .collect()
}

/// Parse printer-row inputs: an array of strings, an array of
/// {path} objects, or an object with "paths" / "printer_rows".
fn parse_printer_paths(input: &str) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let items = match &v {
        serde_json::Value::Array(_) => json_array(v),
        serde_json::Value::Object(_) => {
            let inner = v
                .get("paths")
                .or_else(|| v.get("printer_rows"))
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            json_array(inner)
        }
        _ => Vec::new(),
    };
    items
        .iter()
        .map(|item| match item {
            serde_json::Value::String(s) => s.clone(),
            _ => {
                let p = json_str(item, "path");
                if p.is_empty() {
                    json_str(item, "port")
                } else {
                    p
                }
            }
        })
        .collect()
}

fn normalized_letter(raw: &str) -> Option<String> {
    let l = raw.trim().trim_end_matches(':').to_uppercase();
    if l.len() == 1 && l.chars().next().unwrap().is_ascii_alphabetic() {
        Some(l)
    } else {
        None
    }
}

/// Sanitize a network-location shortcut name for use as a file name.
pub fn sanitize_location_name(raw: &str) -> String {
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
fn network_shortcuts_dir() -> Option<PathBuf> {
    std::env::var("APPDATA")
        .ok()
        .map(|a| PathBuf::from(a).join(r"Microsoft\Windows\Network Shortcuts"))
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

// ---------------------------------------------------------------------------
// Config file (UtilityCloset.ini next to current_exe)
// ---------------------------------------------------------------------------

fn config_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    p.set_file_name("UtilityCloset.ini");
    p
}

/// Pure parse of ini text (also used by tests, so the file itself is
/// never touched by unit tests).
pub fn parse_config_text(text: &str) -> Vec<DriveRow> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let count: usize = map.get("Count").and_then(|s| s.parse().ok()).unwrap_or(0);
    let mut drives = Vec::new();
    for i in 0..count.min(MAX_DRIVES) {
        // Label{i} is the current key; Letter{i} is the pre-1.0.12
        // key, kept so old configs still load (as Drives).
        let label = map
            .get(&format!("Label{i}"))
            .or_else(|| map.get(&format!("Letter{i}")))
            .cloned()
            .unwrap_or_default();
        let kind = match map.get(&format!("Kind{i}")).map(|s| s.as_str()) {
            Some("location") => "location".to_string(),
            _ => "drive".to_string(),
        };
        let path = map.get(&format!("Path{i}")).cloned().unwrap_or_default();
        drives.push(DriveRow { kind, label, path });
    }
    drives
}

pub fn serialize_config(drives: &[DriveRow]) -> String {
    let mut s = format!("Count={}\n", drives.len());
    for (i, d) in drives.iter().enumerate() {
        let kind = if d.is_location() { "location" } else { "drive" };
        s.push_str(&format!("Kind{i}={kind}\n"));
        s.push_str(&format!("Label{i}={}\n", d.label.trim()));
        s.push_str(&format!("Path{i}={}\n", d.path.trim()));
    }
    s
}

fn load_config_vec() -> Vec<DriveRow> {
    let text = std::fs::read_to_string(config_path()).unwrap_or_default();
    parse_config_text(&text)
}

fn save_config_vec(drives: &[DriveRow]) -> Result<(), String> {
    std::fs::write(config_path(), serialize_config(drives)).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Backup folder (cached with OnceLock — see egui comment in code)
// ---------------------------------------------------------------------------

/// The app's backup folder: Documents\Utility Closet. Documents is
/// read from the Shell Folders registry value (which follows OneDrive
/// redirection) with a USERPROFILE\Documents fallback.
pub fn backup_dir() -> PathBuf {
    // Cached after the first call (OnceLock): the old version ran
    // `reg query` on EVERY call, and the Overview and Backup pages
    // call backup_file_path() on EVERY FRAME to show "Backups live
    // in…". That was a reg.exe spawn + wait per frame on the UI
    // thread — 60 processes a second — which is why dragging the
    // window stuttered on an otherwise fast PC (window moves only
    // redraw when the app frames, so a blocked UI thread makes
    // the window trail the cursor). The Documents path does not
    // change during a run (a OneDrive redirection change needs a
    // restart anyway), so caching it is safe.
    static CACHED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    CACHED
        .get_or_init(|| {
            let mut docs: Option<PathBuf> = None;
            if let Ok(out) = new_cmd("reg")
                .args([
                    "query",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Shell Folders",
                    "/v",
                    "Personal",
                ])
                .output()
            {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    if line.contains("Personal") {
                        if let Some(v) = line.split("REG_SZ").nth(1) {
                            let v = v.trim();
                            if !v.is_empty() {
                                docs = Some(PathBuf::from(v));
                            }
                        }
                    }
                }
            }
            let base = docs.unwrap_or_else(|| {
                let home = std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join("Documents")
            });
            base.join("Utility Closet")
        })
        .clone()
}

fn backup_file_path() -> PathBuf {
    backup_dir().join("UtilityCloset-Backup.json")
}

// ---------------------------------------------------------------------------
// Registry state reads (egui's is_* functions, verbatim)
// ---------------------------------------------------------------------------

fn is_dark_mode() -> bool {
    match new_cmd("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize",
            "/v",
            "AppsUseLightTheme",
        ])
        .output()
    {
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
/// (the Windows defaults). Read switch is AutoEndTasks=1.
fn is_forced_shutdown() -> bool {
    match reg_value_last(DESKTOP_KEY, "AutoEndTasks") {
        Some(v) => v == "1",
        None => false,
    }
}

/// Instantly close frozen apps. Read switch is HungAppTimeout <= 1000.
/// Shares keys with forced shutdown (which uses 2000), so callers
/// re-read the other toggle after either one changes.
fn is_instant_close_frozen() -> bool {
    let out = new_cmd("reg")
        .args(["query", DESKTOP_KEY, "/v", "HungAppTimeout"])
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

/// Windows welcome experience. Read switch is ScoobeSystemSettingEnabled:
/// 0 = off, 1 or missing = on (the Windows default).
fn is_welcome_experience_on() -> bool {
    match reg_query(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\UserProfileEngagement",
        "ScoobeSystemSettingEnabled",
    ) {
        Ok(text) => {
            let line = text.lines().find(|l| l.contains("ScoobeSystemSettingEnabled"));
            match line.and_then(|l| l.split_whitespace().last()) {
                Some("0") | Some("0x0") => false,
                _ => true,
            }
        }
        Err(_) => true,
    }
}

/// Always show all tray icons. Read switch is EnableAutoTray:
/// 0 = show-all on, 1 or missing = normal.
fn is_show_all_tray_icons() -> bool {
    match reg_query(
        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer",
        "EnableAutoTray",
    ) {
        Ok(text) => {
            let line = text.lines().find(|l| l.contains("EnableAutoTray"));
            match line.and_then(|l| l.split_whitespace().last()) {
                Some("0") | Some("0x0") => true,
                _ => false,
            }
        }
        Err(_) => false,
    }
}

/// Microsoft Copilot in Windows. Read switch is the TurnOffWindowsCopilot
/// policy: 1 = off, anything else or missing = on (the Windows default).
/// Note: this is the Windows Copilot experience, not the standalone app.
fn is_copilot_enabled() -> bool {
    match reg_query(
        r"HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsCopilot",
        "TurnOffWindowsCopilot",
    ) {
        Ok(text) => {
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
    match new_cmd("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-AppxPackage -Name 'Microsoft.Copilot*' | Select-Object -ExpandProperty Name",
        ])
        .output()
    {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
        Err(_) => false,
    }
}

fn get_states_value() -> serde_json::Value {
    serde_json::json!({
        "forced_shutdown": is_forced_shutdown(),
        "instant_close_frozen": is_instant_close_frozen(),
        "welcome_experience_on": is_welcome_experience_on(),
        "show_all_tray_icons": is_show_all_tray_icons(),
        "copilot_enabled": is_copilot_enabled(),
        "copilot_app_installed": is_copilot_app_installed(),
        "dark_mode": is_dark_mode(),
        "backup_dir": backup_dir().to_string_lossy(),
        "backup_file_path": backup_file_path().to_string_lossy(),
        "computer": computer_name(),
    })
}

// ---------------------------------------------------------------------------
// Drives: map / disconnect / load-existing
// ---------------------------------------------------------------------------

/// Map every row with a non-empty path. Drive rows use `net use`,
/// Location rows create a Network Shortcuts .lnk. Returns
/// (normalized drives, log, mapped count). Normalization of Drive
/// labels and the config save happen exactly as in egui.
fn map_drives_vec(drives_in: &[DriveRow]) -> (Vec<DriveRow>, Vec<String>, usize) {
    let mut log: Vec<String> = Vec::new();
    let mut mapped = 0usize;
    for row in drives_in {
        let path = row.path.trim();
        if path.is_empty() {
            continue;
        }
        if row.is_location() {
            let name = sanitize_location_name(&row.label);
            log.push(format!("Adding network location '{name}' -> {path} ..."));
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
                    log.push(format!("Network location '{name}' added (This PC > Network locations)."));
                }
                Err(e) => log.push(format!("Failed to add network location '{name}': {e}")),
            }
        } else {
            match normalized_letter(&row.label) {
                Some(letter) => {
                    log.push(format!("Mapping {letter}: to {path} ..."));
                    let target = format!("{letter}:");
                    let (ok, mut lines) = run_logged("net", &["use", &target, path, "/persistent:yes"]);
                    log.append(&mut lines);
                    if ok {
                        mapped += 1;
                    }
                }
                None => {
                    log.push("Skipped drive row: invalid drive letter.".to_string());
                }
            }
        }
    }
    let mut drives: Vec<DriveRow> = drives_in.to_vec();
    for row in &mut drives {
        if !row.is_location() {
            if let Some(l) = normalized_letter(&row.label) {
                row.label = l;
            }
        }
    }
    if mapped > 0 {
        let _ = save_config_vec(&drives);
        log.push("Network list saved for next time.".to_string());
    } else {
        log.push(r"Nothing mapped. Enter a network path first (e.g. \\SERVER\share).".to_string());
    }
    (drives, log, mapped)
}

fn disconnect_drives_vec(drives_in: &[DriveRow]) -> Vec<String> {
    let mut log: Vec<String> = Vec::new();
    for row in drives_in {
        if row.is_location() {
            let name = sanitize_location_name(&row.label);
            if let Some(dir) = network_shortcuts_dir() {
                let link = dir.join(format!("{name}.lnk"));
                match std::fs::remove_file(&link) {
                    Ok(_) => log.push(format!("Removed network location '{name}'.")),
                    Err(_) => log.push(format!(
                        "Network location '{name}' shortcut not found at {}.",
                        link.display()
                    )),
                }
            }
        } else if let Some(letter) = normalized_letter(&row.label) {
            log.push(format!("Disconnecting {letter}: ..."));
            let target = format!("{letter}:");
            let (_, mut lines) = run_logged("net", &["use", &target, "/delete", "/y"]);
            log.append(&mut lines);
        }
    }
    log
}

/// Fill the drive rows from the PC's currently mapped drives
/// (Get-SmbMapping), the HKCU\Network fallback (the elevated-session
/// fix), and existing Network Shortcuts locations. Merges by letter /
/// path, saves the config when anything was loaded, and returns
/// (drives, log). Load Existing remains a manual re-scan; removing a
/// live row only hides it until the next launch — Disconnect removes it.
fn load_existing_vec(drives_in: &[DriveRow]) -> (Vec<DriveRow>, Vec<String>) {
    let mut log: Vec<String> = Vec::new();
    let mut drives: Vec<DriveRow> = drives_in.to_vec();
    let mut loaded_drives = 0usize;
    let mut confirmed: std::collections::HashSet<String> = std::collections::HashSet::new();
    match run_ps("Get-SmbMapping | Select-Object LocalPath,RemotePath | ConvertTo-Json -Compress") {
        Ok(text) => {
            if text.trim().is_empty() {
                log.push("No live session mappings found (an elevated session sees only its own mappings).".to_string());
            } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                for item in json_array(v) {
                    let letter = json_str(&item, "LocalPath").trim_end_matches(':').to_string();
                    let path = json_str(&item, "RemotePath");
                    if letter.is_empty() || path.is_empty() {
                        continue;
                    }
                    confirmed.insert(normalized_letter(&letter).unwrap_or_else(|| letter.clone()));
                    if let Some(row) = drives.iter_mut().find(|r| {
                        !r.is_location()
                            && normalized_letter(&r.label).as_deref() == Some(letter.as_str())
                    }) {
                        row.path = path;
                    } else if drives.len() < MAX_DRIVES {
                        drives.push(DriveRow::drive(letter, path));
                    }
                    loaded_drives += 1;
                }
            } else {
                log.push("Could not read existing mapped drives.".to_string());
            }
        }
        Err(e) => log.push(format!("Failed to list existing mapped drives: {e}")),
    }
    // Registry fallback — the actual fix for "0 drives" on a PC that
    // clearly has mapped drives. This app runs as Administrator, and
    // a mapped drive belongs to the user session that created it:
    // Get-SmbMapping in an elevated session sees only that session's
    // mappings, which is usually none, even though Explorer (standard
    // session) shows X: and Y: plainly. The mapping definitions
    // themselves live in HKCU\Network\<letter> with a RemotePath
    // value, and the same user's hive is readable from the elevated
    // process — so read those. Only letters SmbMapping didn't already
    // cover are added (live session wins if the two ever disagree).
    if let Ok(text) = run_ps(
        "Get-ChildItem 'HKCU:\\Network' -ErrorAction SilentlyContinue | Get-ItemProperty -ErrorAction SilentlyContinue | Select-Object @{N='Letter';E={$_.PSChildName}}, RemotePath | ConvertTo-Json -Compress",
    ) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            for item in json_array(v) {
                let Some(letter) = normalized_letter(&json_str(&item, "Letter")) else {
                    continue;
                };
                let path = json_str(&item, "RemotePath");
                if path.is_empty() {
                    continue;
                }
                if confirmed.contains(&letter) {
                    continue;
                }
                confirmed.insert(letter.clone());
                if let Some(row) = drives.iter_mut().find(|r| {
                    !r.is_location()
                        && normalized_letter(&r.label).as_deref() == Some(letter.as_str())
                }) {
                    row.path = path;
                    loaded_drives += 1;
                } else if drives.len() < MAX_DRIVES {
                    drives.push(DriveRow::drive(letter, path));
                    loaded_drives += 1;
                }
            }
        }
    }
    let mut loaded_locations = 0usize;
    for (name, target) in collect_network_locations() {
        if drives.iter().any(|r| r.is_location() && r.path.eq_ignore_ascii_case(&target)) {
            continue;
        }
        if drives.len() < MAX_DRIVES {
            drives.push(DriveRow::location(name, target));
            loaded_locations += 1;
        }
    }
    if loaded_drives > 0 || loaded_locations > 0 {
        let has_real = drives.iter().any(|x| !x.path.trim().is_empty());
        if has_real {
            drives.retain(|r| !r.path.trim().is_empty());
        }
        let _ = save_config_vec(&drives);
    }
    log.push(format!(
        "Loaded {loaded_drives} existing mapped drive(s) and {loaded_locations} network location(s) into the list."
    ));
    (drives, log)
}

// ---------------------------------------------------------------------------
// Printers
// ---------------------------------------------------------------------------

fn refresh_printers_vec() -> (Vec<String>, Vec<String>) {
    let out = new_cmd("powershell")
        .args(["-NoProfile", "-Command", "Get-Printer | Select-Object -ExpandProperty Name"])
        .output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            let printers: Vec<String> = text
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let log = vec![format!("Found {} installed printer(s).", printers.len())];
            (printers, log)
        }
        Err(e) => (Vec::new(), vec![format!("Failed to list printers: {e}")]),
    }
}

/// Add every non-empty printer path (bulk, like Map Drives), capped at
/// MAX_PRINTERS rows. Returns (log, added, refreshed printers).
/// NOTE (egui quirk, preserved): `run_logged` only counts a run as ok
/// when its output contains "successfully", and rundll32 printui is
/// silent on success — so a successful add logs "Add may have
/// failed …" and does NOT trigger the refresh, exactly as egui did.
/// Callers that need a fresh list should call uc_refresh_printers()
/// after adding.
fn add_printers_vec(paths_in: &[String]) -> (Vec<String>, usize, Vec<String>) {
    let mut log: Vec<String> = Vec::new();
    let rows: Vec<String> = paths_in
        .iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    if rows.is_empty() {
        log.push("Enter a printer path first (e.g. \\\\SERVER\\Printer).".to_string());
        return (log, 0, Vec::new());
    }
    let rows: Vec<String> = rows.into_iter().take(MAX_PRINTERS).collect();
    if paths_in.iter().filter(|p| !p.trim().is_empty()).count() > MAX_PRINTERS {
        log.push(format!("Only the first {MAX_PRINTERS} printer paths are added."));
    }
    let mut added = 0usize;
    for path in &rows {
        log.push(format!("Adding printer {path} ..."));
        let (ok, mut lines) = run_logged("rundll32", &["printui.dll,PrintUIEntry", "/in", "/n", path]);
        log.append(&mut lines);
        if ok {
            log.push(format!("Printer {path} added."));
            added += 1;
        } else {
            log.push("Add may have failed — check the path and that the print server is reachable.".to_string());
        }
    }
    let mut printers = Vec::new();
    if added > 0 {
        let (p, mut plog) = refresh_printers_vec();
        printers = p;
        log.append(&mut plog);
    }
    (log, added, printers)
}

// ---------------------------------------------------------------------------
// Monitors: snapshot / detect / apply
// ---------------------------------------------------------------------------

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

fn monitor_summary(monitors: &[serde_json::Value]) -> String {
    let mut summary = String::new();
    for m in monitors {
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
    summary
}

fn monitor_layout_doc(monitors: &[serde_json::Value]) -> serde_json::Value {
    serde_json::json!({
        "format": "UtilityClosetMonitorLayout",
        "version": 1,
        "computer": computer_name(),
        "exported": timestamp(),
        "monitors": monitors,
    })
}

/// Detect the current monitors and write
/// Documents\Utility Closet\Monitor-Layout.json so the layout can be
/// applied on its own, without a full backup. Returns
/// (monitors, summary, log, layout_path).
fn detect_monitors_vec() -> (Vec<serde_json::Value>, String, Vec<String>, String) {
    let mut log: Vec<String> = Vec::new();
    match snapshot_monitors() {
        Ok(monitors) => {
            let summary = monitor_summary(&monitors);
            let dir = backup_dir();
            let mut layout_path = String::new();
            if std::fs::create_dir_all(&dir).is_ok() {
                let doc = monitor_layout_doc(&monitors);
                let path = dir.join("Monitor-Layout.json");
                if std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap_or_default()).is_ok() {
                    layout_path = path.to_string_lossy().to_string();
                    log.push(format!(
                        "Detected {} monitor(s); layout saved to {}.",
                        monitors.len(),
                        path.display()
                    ));
                }
            }
            (monitors, summary, log, layout_path)
        }
        Err(e) => (Vec::new(), String::new(), vec![format!("Monitor detection failed: {e}")], String::new()),
    }
}

/// Build the PowerShell monitor-restore script for a monitor list
/// (payload JSON is single-quote escaped, as in egui).
fn build_apply_monitor_script(monitors: &[serde_json::Value]) -> String {
    let payload = serde_json::to_string(monitors)
        .unwrap_or_default()
        .replace('\'', "''");
    let mut script = String::from(
        r#"
$ErrorActionPreference = 'Stop'
$code = @'
using System;
using System.Runtime.InteropServices;
public static class Disp {
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
    [DllImport("user32.dll", CharSet = CharSet.Ansi)] public static extern int ChangeDisplaySettingsEx(string lpszDeviceName, ref DEVMODE lpDevMode, IntPtr hwnd, uint dwflags, IntPtr lParam);
    [DllImport("user32.dll")] public static extern int ChangeDisplaySettings(IntPtr devMode, uint flags);
}
'@
Add-Type -TypeDefinition $code
$monitors = '__PAYLOAD__' | ConvertFrom-Json
if ($monitors -isnot [array]) { $monitors = @($monitors) }
$results = @()
for ($i = 0; $true; $i++) {
    $dd = New-Object Disp+DISPLAY_DEVICE; $dd.cb = [System.Runtime.InteropServices.Marshal]::SizeOf($dd)
    if (-not [Disp]::EnumDisplayDevices($null, $i, [ref]$dd, 0)) { break }
    $want = $monitors | Where-Object { $_.device -eq $dd.DeviceName } | Select-Object -First 1
    if (-not $want) { $results += "$($dd.DeviceName): not in backup, skipped"; continue }
    $dm = New-Object Disp+DEVMODE; $dm.dmSize = [System.Runtime.InteropServices.Marshal]::SizeOf($dm)
    if (-not [Disp]::EnumDisplaySettings($dd.DeviceName, -1, [ref]$dm)) { $results += "$($dd.DeviceName): could not read current mode"; continue }
    $dm.dmFields = 0x20 -bor 0x80000 -bor 0x100000
    $dm.dmPelsWidth = [int]$want.width; $dm.dmPelsHeight = [int]$want.height
    $dm.dmPosition = New-Object Disp+POINTL; $dm.dmPosition.x = [int]$want.x; $dm.dmPosition.y = [int]$want.y
    if ([int]$want.bits -gt 0) { $dm.dmFields = $dm.dmFields -bor 0x40000; $dm.dmBitsPerPel = [int]$want.bits }
    if ($null -ne $want.orientation) { $dm.dmFields = $dm.dmFields -bor 0x80; $dm.dmDisplayOrientation = [short]$want.orientation }
    if ([int]$want.refresh -gt 0) { $dm.dmFields = $dm.dmFields -bor 0x400000; $dm.dmDisplayFrequency = [int]$want.refresh }
    $flags = 1 -bor 0x10000000
    if ($want.primary) { $flags = $flags -bor 0x10 }
    $rc = [Disp]::ChangeDisplaySettingsEx($dd.DeviceName, [ref]$dm, [IntPtr]::Zero, $flags, [IntPtr]::Zero)
    $results += "$($dd.DeviceName): ChangeDisplaySettingsEx = $rc (0 = success)"
}
[void][Disp]::ChangeDisplaySettings([IntPtr]::Zero, 0)
$results -join "`n"
"#,
    );
    script = script.replace("__PAYLOAD__", &payload);
    script
}

/// Apply a backed-up monitor layout live, per display, via
/// ChangeDisplaySettingsEx (position, size, primary, orientation,
/// refresh rate). Displays in the backup that are not currently
/// connected are skipped, never forced. The primary display gets
/// CDS_SET_PRIMARY (0x10). A total failure leaves the layout
/// unchanged (the .reg files in the backup folder are the fallback).
fn apply_monitor_layout_vec(monitors: &[serde_json::Value]) -> Vec<String> {
    let mut log: Vec<String> = Vec::new();
    if monitors.is_empty() {
        log.push("Backup has no monitors — monitor layout skipped.".to_string());
        return log;
    }
    let script = build_apply_monitor_script(monitors);
    let tmp = std::env::temp_dir().join("UtilityCloset-MonitorRestore.ps1");
    match std::fs::write(&tmp, &script) {
        Ok(_) => match new_cmd("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", &tmp.to_string_lossy()])
            .output()
        {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    if !line.trim().is_empty() {
                        log.push(format!("Monitor: {line}"));
                    }
                }
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    for line in err.lines() {
                        if !line.trim().is_empty() {
                            log.push(format!("Monitor restore error: {line}"));
                        }
                    }
                    log.push(
                        "Monitor layout may not have applied — the Display-*.reg files in the backup folder are the fallback (import + restart).".to_string(),
                    );
                }
            }
            Err(e) => log.push(format!("Failed to run monitor restore: {e}")),
        },
        Err(e) => log.push(format!("Failed to write monitor restore script: {e}")),
    }
    log
}

fn parse_monitors(input: &str) -> Vec<serde_json::Value> {
    let v: serde_json::Value = match serde_json::from_str(input) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    match &v {
        serde_json::Value::Array(_) => json_array(v),
        serde_json::Value::Object(_) => {
            json_array(v.get("monitors").cloned().unwrap_or(serde_json::Value::Null))
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Backup & Restore — Documents\Utility Closet\UtilityCloset-Backup.json
//
// Export snapshots the LIVE system, not just what is typed into
// the app: currently mapped network drives (Get-SmbMapping),
// currently installed printers (Get-Printer), and the full display
// topology (monitor snapshot + video controllers). App rows
// that are not currently live are added too, so nothing typed
// into the app is lost. Import restores drives and printers
// immediately; monitors are applied live per display (see
// apply_monitor_layout_vec), with missing displays skipped.
// ---------------------------------------------------------------------------

fn backup_doc(
    drives: Vec<serde_json::Value>,
    printers: Vec<serde_json::Value>,
    monitors: Vec<serde_json::Value>,
    controllers: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "format": "UtilityClosetBackup",
        "version": 1,
        "app_version": APP_VERSION,
        "computer": computer_name(),
        "exported": timestamp(),
        "drives": drives,
        "printers": printers,
        "monitors": monitors,
        "video_controllers": controllers,
    })
}

fn export_backup_vec(app_drives: &[DriveRow], app_printer_paths: &[String]) -> (Vec<String>, String, usize, usize, usize) {
    let mut log: Vec<String> = Vec::new();
    let dir = backup_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log.push(format!("Could not create backup folder {}: {e}", dir.display()));
        return (log, String::new(), 0, 0, 0);
    }
    // Live mapped drives.
    let mut drives: Vec<serde_json::Value> = Vec::new();
    match run_ps("Get-SmbMapping | Select-Object LocalPath,RemotePath,Status | ConvertTo-Json -Compress") {
        Ok(text) => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                for item in json_array(v) {
                    let letter = json_str(&item, "LocalPath").trim_end_matches(':').to_string();
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
        Err(e) => log.push(format!("Live drive snapshot failed ({e}) — app rows only.")),
    }
    // App network rows not already live.
    for row in app_drives {
        let path = row.path.trim();
        if path.is_empty() {
            continue;
        }
        if row.is_location() {
            if drives.iter().any(|d| json_str(d, "path").eq_ignore_ascii_case(path)) {
                continue;
            }
            drives.push(serde_json::json!({
                "kind": "location", "letter": "", "label": row.label, "path": path, "source": "app", "status": "",
            }));
        } else {
            let Some(letter) = normalized_letter(&row.label) else {
                continue;
            };
            if drives.iter().any(|d| json_str(d, "letter") == letter) {
                continue;
            }
            drives.push(serde_json::json!({
                "kind": "drive", "letter": letter, "label": letter, "path": path, "source": "app", "status": "",
            }));
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
    match run_ps("Get-Printer | Select-Object Name,PortName,DriverName,Shared | ConvertTo-Json -Compress") {
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
        Err(e) => log.push(format!("Live printer snapshot failed ({e}) — app rows only.")),
    }
    // Queued printer rows not already live (by port).
    for raw in app_printer_paths {
        let path = raw.trim();
        if path.is_empty() || printers.iter().any(|p| json_str(p, "port") == path) {
            continue;
        }
        printers.push(serde_json::json!({
            "name": path, "port": path, "driver": "", "shared": false, "source": "app",
        }));
    }
    // Full display topology: monitors + video controllers.
    let mut monitors: Vec<serde_json::Value> = Vec::new();
    match snapshot_monitors() {
        Ok(m) => monitors = m,
        Err(e) => log.push(format!("Monitor snapshot failed: {e}")),
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
    // old way (import + restart). Best-effort, failures only logged
    // (in egui: not logged at all — the status is ignored).
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
        let _ = new_cmd("reg")
            .args(["export", key, &target.to_string_lossy(), "/y"])
            .status();
    }
    let doc = backup_doc(drives, printers, monitors, controllers);
    let path = backup_file_path();
    let (dc, pc, mc) = (
        doc["drives"].as_array().map(|a| a.len()).unwrap_or(0),
        doc["printers"].as_array().map(|a| a.len()).unwrap_or(0),
        doc["monitors"].as_array().map(|a| a.len()).unwrap_or(0),
    );
    match std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap_or_default()) {
        Ok(_) => {
            log.push(format!(
                "Backup exported to {}: {dc} drive(s), {pc} printer(s), {mc} monitor(s).",
                path.display()
            ));
        }
        Err(e) => log.push(format!("Failed to write backup {}: {e}", path.display())),
    }
    (log, path.to_string_lossy().to_string(), dc, pc, mc)
}

fn import_backup_vec() -> (Vec<DriveRow>, Vec<String>, Vec<String>) {
    let mut log: Vec<String> = Vec::new();
    let path = backup_file_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            log.push(format!("No backup found at {} — export one first.", path.display()));
            return (Vec::new(), Vec::new(), log);
        }
    };
    let doc: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            log.push(format!("Backup file is not readable JSON: {e}"));
            return (Vec::new(), Vec::new(), log);
        }
    };
    if json_str(&doc, "format") != "UtilityClosetBackup" {
        log.push("This file is not a Utility Closet backup — import cancelled.".to_string());
        return (Vec::new(), Vec::new(), log);
    }
    let backup_pc = json_str(&doc, "computer");
    let this_pc = computer_name();
    if !backup_pc.is_empty() && backup_pc != "unknown" && backup_pc != this_pc {
        log.push(format!(
            "Warning: this backup is from PC '{backup_pc}', but this PC is '{this_pc}' — drives, printers, and monitors may differ. Restoring anyway."
        ));
    }
    // Drives: replace app rows, save, then map (live restore).
    let mut drives: Vec<DriveRow> = Vec::new();
    let backup_drives = json_array(doc.get("drives").cloned().unwrap_or(serde_json::Value::Null));
    if !backup_drives.is_empty() {
        drives = backup_drives
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
                            Some(DriveRow::location(label, p))
                        }
                    }
                    _ => {
                        let letter = json_str(d, "letter");
                        if letter.is_empty() {
                            None
                        } else {
                            Some(DriveRow::drive(letter, p))
                        }
                    }
                }
            })
            .take(MAX_DRIVES)
            .collect();
        let _ = save_config_vec(&drives);
        log.push(format!("Restoring {} drive(s) from backup ...", drives.len()));
        let (mapped_drives, mut map_log, _) = map_drives_vec(&drives);
        drives = mapped_drives;
        log.append(&mut map_log);
    } else {
        log.push("Backup has no drives — drive restore skipped.".to_string());
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
    let mut printer_rows: Vec<String> = Vec::new();
    if !to_add.is_empty() {
        printer_rows = to_add.iter().take(MAX_PRINTERS).cloned().collect();
        log.push(format!("Restoring {} network printer(s) from backup ...", to_add.len()));
        let (mut add_log, _, _) = add_printers_vec(&printer_rows);
        log.append(&mut add_log);
    } else {
        log.push("Backup has no network printers to add — printer restore skipped (local printers are not added by UNC path).".to_string());
    }
    // Monitors: live apply, missing displays skipped.
    let monitors = json_array(doc.get("monitors").cloned().unwrap_or(serde_json::Value::Null));
    let mut mlog = apply_monitor_layout_vec(&monitors);
    log.append(&mut mlog);
    log.push(format!("Import complete from {}.", path.display()));
    (drives, printer_rows, log)
}

/// Delete the app's backup files (main backup, standalone
/// monitor layout, and Display-*.reg fallbacks). Only those
/// known files are removed — the backup folder itself and
/// anything else in it are left alone. This does not change
/// any live drive, printer, or monitor setting.
fn delete_backups_vec() -> (Vec<String>, usize) {
    let dir = backup_dir();
    let mut deleted = 0usize;
    for name in [
        "UtilityCloset-Backup.json",
        "Monitor-Layout.json",
        "Display-Configuration.reg",
        "Display-Connectivity.reg",
        "Display-ScaleFactors.reg",
    ] {
        if std::fs::remove_file(dir.join(name)).is_ok() {
            deleted += 1;
        }
    }
    let log = if deleted > 0 {
        vec![format!("Deleted {deleted} backup file(s) from {}.", dir.display())]
    } else {
        vec!["No backup files found to delete.".to_string()]
    };
    (log, deleted)
}

// ---------------------------------------------------------------------------
// Repairs
// ---------------------------------------------------------------------------

fn restart_explorer_vec() {
    let _ = new_cmd("taskkill").args(["/IM", "explorer.exe", "/F"]).status();
    std::thread::sleep(Duration::from_secs(2));
    let _ = new_cmd("explorer.exe").spawn();
}

// ---------------------------------------------------------------------------
// Annoyance toggles: exact registry values from egui
// ---------------------------------------------------------------------------

fn apply_forced_shutdown(on: bool) -> bool {
    let (auto_end, kill_app, hung) = if on { ("1", "2000", "2000") } else { ("0", "20000", "5000") };
    let mut ok = true;
    for (name, value) in [
        ("AutoEndTasks", auto_end),
        ("WaitToKillAppTimeout", kill_app),
        ("HungAppTimeout", hung),
    ] {
        if !reg_add(DESKTOP_KEY, name, "REG_SZ", value) {
            ok = false;
        }
    }
    ok
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
        if !reg_add(DESKTOP_KEY, name, "REG_SZ", value) {
            ok = false;
        }
    }
    if !reg_add(
        r"HKLM\SYSTEM\CurrentControlSet\Control",
        "WaitToKillServiceTimeout",
        "REG_SZ",
        kill_svc,
    ) {
        ok = false;
    }
    ok
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
        if !reg_add(key, name, "REG_DWORD", value) {
            ok = false;
        }
    }
    ok
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
        if !reg_add(key, name, "REG_DWORD", value) {
            ok = false;
        }
    }
    ok
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
        if !reg_add(key, name, "REG_DWORD", value) {
            ok = false;
        }
    }
    ok
}

// ---------------------------------------------------------------------------
// Copilot app install / uninstall (exact egui sequence)
// ---------------------------------------------------------------------------

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
    let _ = new_cmd("taskkill").args(["/IM", "Copilot.exe", "/F"]).status();
    // No early "not installed" exit: v1.0.12 returned that when
    // its presence check missed the install form, skipping every
    // attempt. Try everything, then check presence at the end.
    for id in ["Microsoft.Copilot_8wekyb3d8bbwe", "Microsoft.Copilot"] {
        if let Ok(s) = new_cmd("winget")
            .args(["uninstall", "--id", id, "--silent", "--accept-source-agreements"])
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
        if let Ok(s) = new_cmd("winget").args(&args).status() {
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
    if new_cmd("powershell")
        .args(["-NoProfile", "-Command", ps])
        .status()
        .is_ok()
        && !copilot_still_present()
    {
        return "Copilot app uninstalled via its Programs and Features uninstaller.".into();
    }
    match new_cmd("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-AppxPackage -Name 'Microsoft.Copilot*' | Remove-AppxPackage -ErrorAction Continue",
        ])
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
    for id in ["Microsoft.Copilot_8wekyb3d8bbwe", "Microsoft.Copilot"] {
        if let Ok(s) = new_cmd("winget")
            .args([
                "install",
                "--id",
                id,
                "--silent",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ])
            .status()
        {
            if s.success() {
                return format!("Copilot app reinstalled via winget ({id}).");
            }
        }
    }
    if let Ok(s) = new_cmd("winget")
        .args([
            "install",
            "--name",
            "Copilot",
            "--silent",
            "--accept-source-agreements",
            "--accept-package-agreements",
        ])
        .status()
    {
        if s.success() {
            return "Copilot app reinstalled via winget (by name).".into();
        }
    }
    "Copilot app reinstall via winget did not confirm — install it from the Microsoft Store (search 'Microsoft Copilot') if it is still missing.".into()
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

// ---------------------------------------------------------------------------
// Reset to Windows defaults (egui's Reset button, minus the inline
// Explorer restart — that is reported via explorer_restart_needed
// and left to the Flutter UI, which restarts at most once).
// ---------------------------------------------------------------------------

fn reset_defaults_vec() -> (Vec<String>, serde_json::Value, bool) {
    let mut log: Vec<String> = Vec::new();
    let before = get_states_value();
    let tray_before = before["show_all_tray_icons"].as_bool().unwrap_or(false);
    let copilot_before = before["copilot_enabled"].as_bool().unwrap_or(true);
    log.push(
        "Resetting Windows Annoyances to Windows defaults (not your preferred settings): forced shutdown OFF, instant close OFF, welcome experience ON, show all tray icons OFF, Microsoft Copilot ON.".to_string(),
    );
    if apply_forced_shutdown(false) {
        // Forced OFF resets HungAppTimeout to 5000, so instant close
        // is off after that (re-read, as the toggle handlers do).
        log.push("Forced shutdown reset to OFF (Windows default).".to_string());
    } else {
        log.push("Failed to reset forced shutdown — that toggle was left unchanged.".to_string());
    }
    if apply_welcome_experience(true) {
        log.push("Welcome experience reset to ON (Windows default: the post-update setup screen is allowed again).".to_string());
    } else {
        log.push("Failed to reset the welcome experience — that toggle was left unchanged.".to_string());
    }
    if apply_show_all_tray_icons(false) {
        log.push("Show all tray icons reset to OFF (Windows default: inactive icons hide again).".to_string());
    } else {
        log.push("Failed to reset the tray-icons setting — that toggle was left unchanged.".to_string());
    }
    // Copilot was the one toggle Reset skipped in older versions:
    // Windows' default is Copilot ON, so Reset now clears the
    // policy, shows the taskbar button again, and reinstalls the
    // app (winget, can take a few minutes) — same as flipping
    // the toggle on.
    if apply_copilot_enabled(true) {
        log.push("Microsoft Copilot reset to ON (Windows default: policy cleared, taskbar button back).".to_string());
        log.push(install_copilot_app());
    } else {
        log.push("Failed to reset Microsoft Copilot — that toggle was left unchanged.".to_string());
    }
    let states = get_states_value();
    let explorer_restart_needed = tray_before != states["show_all_tray_icons"].as_bool().unwrap_or(false)
        || copilot_before != states["copilot_enabled"].as_bool().unwrap_or(true);
    (log, states, explorer_restart_needed)
}

// ---------------------------------------------------------------------------
// Update check (check only — no download; download lives in Dart)
// ---------------------------------------------------------------------------

pub fn parse_ver(s: &str) -> (u64, u64, u64) {
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
/// as the egui app's select_update_asset.
fn expected_asset_name(version: &str) -> String {
    format!("UtilityCloset-Setup-v{version}.exe")
}

fn check_update_value(current_version: &str) -> serde_json::Value {
    let failed = || serde_json::json!({ "available": false, "status": "check_failed" });
    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("UtilityCloset-update-check")
        .build()
    {
        Ok(c) => c,
        Err(_) => return failed(),
    };
    let v: serde_json::Value = match client
        .get(format!("https://api.github.com/repos/{UPDATE_REPO}/releases/latest"))
        .send()
    {
        Ok(r) if r.status().is_success() => match r.json() {
            Ok(v) => v,
            Err(_) => return failed(),
        },
        _ => return failed(),
    };
    let tag = v["tag_name"].as_str().unwrap_or("").to_string();
    if tag.is_empty() {
        return failed();
    }
    if parse_ver(&tag) <= parse_ver(current_version) {
        return serde_json::json!({
            "available": false,
            "status": "up_to_date",
            "latest_version": tag.trim_start_matches('v'),
        });
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
                failed()
            } else {
                serde_json::json!({
                    "available": true,
                    "status": "available",
                    "version": version,
                    "asset_name": want,
                    "asset_url": url,
                    "asset_size": size,
                })
            }
        }
        None => failed(),
    }
}

// ---------------------------------------------------------------------------
// FFI input parsing helpers
// ---------------------------------------------------------------------------

fn cstr_to_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
}

fn string_to_ptr(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => CString::new("{\"error\":\"invalid output\"}")
            .unwrap()
            .into_raw(),
    }
}

fn json_ptr(v: serde_json::Value) -> *mut c_char {
    string_to_ptr(serde_json::to_string(&v).unwrap_or_else(|_| "{\"error\":\"encode\"}".into()))
}

/// Toggle input: JSON true/false, {"on": bool} / {"enabled": bool},
/// or a plain "true"/"false"/"1"/"0"/"on"/"off" string.
fn parse_bool_input(input: &str) -> bool {
    let t = input.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        match v {
            serde_json::Value::Bool(b) => return b,
            serde_json::Value::Object(_) => {
                for k in ["on", "enabled", "value"] {
                    if let Some(b) = v.get(k).and_then(|x| x.as_bool()) {
                        return b;
                    }
                }
                return false;
            }
            serde_json::Value::String(s) => return matches!(s.to_lowercase().as_str(), "true" | "1" | "on" | "yes"),
            _ => return false,
        }
    }
    matches!(t.to_lowercase().as_str(), "true" | "1" | "on" | "yes")
}

/// Single-name input: a JSON string, {"name": ...} / {"path": ...},
/// or a plain string.
fn parse_name_input(input: &str) -> String {
    let t = input.trim();
    if t.is_empty() {
        return String::new();
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
        match v {
            serde_json::Value::String(s) => return s,
            serde_json::Value::Object(_) => {
                for k in ["name", "path", "value"] {
                    if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
                        return s.to_string();
                    }
                }
                return String::new();
            }
            _ => return String::new(),
        }
    }
    t.to_string()
}

fn log_json(log: Vec<String>) -> serde_json::Value {
    serde_json::json!({ "log": log })
}

// ---------------------------------------------------------------------------
// C ABI exports
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn uc_free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}

#[no_mangle]
pub extern "C" fn uc_version() -> *mut c_char {
    string_to_ptr("1.1.3-rust".to_string())
}

#[no_mangle]
pub extern "C" fn uc_get_states(_input: *const c_char) -> *mut c_char {
    json_ptr(get_states_value())
}

#[no_mangle]
pub extern "C" fn uc_load_config(_input: *const c_char) -> *mut c_char {
    json_ptr(serde_json::json!({ "drives": drives_to_json(&load_config_vec()) }))
}

#[no_mangle]
pub extern "C" fn uc_save_config(input: *const c_char) -> *mut c_char {
    let drives = parse_drives(&cstr_to_string(input));
    let path = config_path().to_string_lossy().to_string();
    match save_config_vec(&drives) {
        Ok(()) => json_ptr(serde_json::json!({ "ok": true, "path": path, "count": drives.len() })),
        Err(e) => json_ptr(serde_json::json!({ "ok": false, "path": path, "count": drives.len(), "error": e })),
    }
}

#[no_mangle]
pub extern "C" fn uc_load_existing(input: *const c_char) -> *mut c_char {
    let (drives, log) = load_existing_vec(&parse_drives(&cstr_to_string(input)));
    json_ptr(serde_json::json!({ "drives": drives_to_json(&drives), "log": log }))
}

#[no_mangle]
pub extern "C" fn uc_map_drives(input: *const c_char) -> *mut c_char {
    let (drives, log, mapped) = map_drives_vec(&parse_drives(&cstr_to_string(input)));
    json_ptr(serde_json::json!({ "drives": drives_to_json(&drives), "log": log, "mapped": mapped }))
}

#[no_mangle]
pub extern "C" fn uc_disconnect_drives(input: *const c_char) -> *mut c_char {
    let log = disconnect_drives_vec(&parse_drives(&cstr_to_string(input)));
    json_ptr(log_json(log))
}

#[no_mangle]
pub extern "C" fn uc_refresh_printers(_input: *const c_char) -> *mut c_char {
    let (printers, log) = refresh_printers_vec();
    json_ptr(serde_json::json!({ "printers": printers, "log": log }))
}

#[no_mangle]
pub extern "C" fn uc_add_printers(input: *const c_char) -> *mut c_char {
    let (log, added, printers) = add_printers_vec(&parse_printer_paths(&cstr_to_string(input)));
    json_ptr(serde_json::json!({ "log": log, "added": added, "printers": printers }))
}

#[no_mangle]
pub extern "C" fn uc_remove_printer(input: *const c_char) -> *mut c_char {
    let name = parse_name_input(&cstr_to_string(input));
    let mut log = vec![format!("Removing printer {name} ...")];
    let (ok, mut lines) = run_logged("rundll32", &["printui.dll,PrintUIEntry", "/dl", "/n", &name]);
    let _ = ok;
    log.append(&mut lines);
    let (printers, mut plog) = refresh_printers_vec();
    log.append(&mut plog);
    json_ptr(serde_json::json!({ "log": log, "printers": printers }))
}

#[no_mangle]
pub extern "C" fn uc_set_default_printer(input: *const c_char) -> *mut c_char {
    let name = parse_name_input(&cstr_to_string(input));
    let mut log = vec![format!("Setting {name} as default printer ...")];
    let (_, mut lines) = run_logged("rundll32", &["printui.dll,PrintUIEntry", "/y", "/n", &name]);
    log.append(&mut lines);
    json_ptr(log_json(log))
}

#[no_mangle]
pub extern "C" fn uc_detect_monitors(_input: *const c_char) -> *mut c_char {
    let (monitors, summary, log, layout_path) = detect_monitors_vec();
    json_ptr(serde_json::json!({
        "monitors": monitors, "summary": summary, "log": log, "layout_path": layout_path
    }))
}

#[no_mangle]
pub extern "C" fn uc_export_backup(input: *const c_char) -> *mut c_char {
    let raw = cstr_to_string(input);
    let drives = parse_drives(&raw);
    // Printer rows live beside the drives in the same JSON object
    // ({"drives": [...], "printer_rows": [...]}) when present.
    let printer_paths = if raw.trim_start().starts_with('{') {
        parse_printer_paths(&raw)
    } else {
        Vec::new()
    };
    let (log, path, dc, pc, mc) = export_backup_vec(&drives, &printer_paths);
    json_ptr(serde_json::json!({
        "log": log, "path": path,
        "drive_count": dc, "printer_count": pc, "monitor_count": mc
    }))
}

#[no_mangle]
pub extern "C" fn uc_import_backup(_input: *const c_char) -> *mut c_char {
    let (drives, printer_rows, log) = import_backup_vec();
    json_ptr(serde_json::json!({
        "drives": drives_to_json(&drives), "printer_rows": printer_rows, "log": log
    }))
}

#[no_mangle]
pub extern "C" fn uc_apply_monitor_layout(input: *const c_char) -> *mut c_char {
    let log = apply_monitor_layout_vec(&parse_monitors(&cstr_to_string(input)));
    json_ptr(log_json(log))
}

#[no_mangle]
pub extern "C" fn uc_apply_monitor_layout_file(_input: *const c_char) -> *mut c_char {
    let dir = backup_dir();
    let standalone = dir.join("Monitor-Layout.json");
    let text = std::fs::read_to_string(&standalone)
        .or_else(|_| std::fs::read_to_string(backup_file_path()));
    match text {
        Ok(t) => match serde_json::from_str::<serde_json::Value>(&t) {
            Ok(doc) => {
                let monitors = json_array(doc.get("monitors").cloned().unwrap_or(serde_json::Value::Null));
                let log = apply_monitor_layout_vec(&monitors);
                json_ptr(serde_json::json!({ "log": log, "monitors": monitors }))
            }
            Err(e) => json_ptr(serde_json::json!({
                "log": [format!("Monitor layout file is not readable JSON: {e}")],
                "monitors": []
            })),
        },
        Err(_) => json_ptr(serde_json::json!({
            "log": ["No monitor layout saved yet — click Detect Monitors first."],
            "monitors": []
        })),
    }
}

#[no_mangle]
pub extern "C" fn uc_delete_backups(_input: *const c_char) -> *mut c_char {
    let (log, deleted) = delete_backups_vec();
    json_ptr(serde_json::json!({ "log": log, "deleted": deleted }))
}

#[no_mangle]
pub extern "C" fn uc_run_sfc(_input: *const c_char) -> *mut c_char {
    // Egui logged the launch line unconditionally, even though the
    // spawn result is ignored.
    let _ = new_cmd("cmd")
        .args(["/c", "start", "cmd", "/k", "sfc", "/scannow"])
        .spawn();
    json_ptr(serde_json::json!({ "log": ["Launching sfc /scannow in a new window..."] }))
}

#[no_mangle]
pub extern "C" fn uc_run_dism(input: *const c_char) -> *mut c_char {
    let raw = cstr_to_string(input);
    let (args, label) = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => {
            let args: Vec<String> = v
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default();
            let label = v
                .get("label")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| args.join(" "));
            (args, label)
        }
        Err(_) => (Vec::new(), raw.clone()),
    };
    let mut cmd_args: Vec<String> = vec!["/c".into(), "start".into(), "cmd".into(), "/k".into(), "DISM.exe".into()];
    cmd_args.extend(args);
    let refs: Vec<&str> = cmd_args.iter().map(|s| s.as_str()).collect();
    let _ = new_cmd("cmd").args(&refs).spawn();
    json_ptr(serde_json::json!({ "log": [format!("Launching DISM {label} in a new window...")] }))
}

#[no_mangle]
pub extern "C" fn uc_clear_display_cache(_input: *const c_char) -> *mut c_char {
    let mut log: Vec<String> = Vec::new();
    for k in [
        r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Configuration",
        r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\Connectivity",
        r"HKLM\SYSTEM\CurrentControlSet\Control\GraphicsDrivers\ScaleFactors",
    ] {
        log.push(format!("Deleting {k} ..."));
        let (_, mut lines) = run_logged("reg", &["delete", k, "/f"]);
        log.append(&mut lines);
    }
    log.push("Done. RESTART your PC to finish the display cache reset.".to_string());
    json_ptr(log_json(log))
}

#[no_mangle]
pub extern "C" fn uc_rebuild_icon_cache(_input: *const c_char) -> *mut c_char {
    let mut log = vec!["Clearing icon cache...".to_string()];
    let _ = new_cmd("ie4uinit.exe").arg("-ClearIconCache").status();
    let _ = new_cmd("taskkill").args(["/IM", "explorer.exe", "/F"]).status();
    std::thread::sleep(Duration::from_secs(2));
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let _ = std::fs::remove_file(format!("{local}\\IconCache.db"));
        if let Ok(entries) = std::fs::read_dir(format!("{local}\\Microsoft\\Windows\\Explorer")) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().to_lowercase().starts_with("iconcache") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    let _ = new_cmd("explorer.exe").spawn();
    log.push("Explorer restarted. Icon cache will rebuild.".to_string());
    json_ptr(log_json(log))
}

#[no_mangle]
pub extern "C" fn uc_reboot(_input: *const c_char) -> *mut c_char {
    let ok = new_cmd("shutdown")
        .args(["/r", "/f", "/t", "5"])
        .spawn()
        .is_ok();
    json_ptr(serde_json::json!({ "ok": ok, "log": [] }))
}

#[no_mangle]
pub extern "C" fn uc_restart_explorer(_input: *const c_char) -> *mut c_char {
    restart_explorer_vec();
    json_ptr(serde_json::json!({ "ok": true, "log": [] }))
}

fn apply_toggle_json(
    on: bool,
    apply: fn(bool) -> bool,
    on_msg: &str,
    off_msg: &str,
    fail_msg: &str,
) -> serde_json::Value {
    let ok = apply(on);
    let mut log = Vec::new();
    if ok {
        log.push(if on { on_msg } else { off_msg }.to_string());
    } else {
        log.push(fail_msg.to_string());
    }
    serde_json::json!({ "ok": ok, "log": log, "states": get_states_value() })
}

#[no_mangle]
pub extern "C" fn uc_apply_forced_shutdown(input: *const c_char) -> *mut c_char {
    let on = parse_bool_input(&cstr_to_string(input));
    json_ptr(apply_toggle_json(
        on,
        apply_forced_shutdown,
        "Forced shutdown ON: Windows will end open apps itself after about 2 seconds instead of showing the \"apps are preventing shutdown\" warning.",
        "Forced shutdown OFF: Windows will show the \"apps are preventing shutdown\" warning again.",
        "Failed to change the forced-shutdown setting — the toggle was left unchanged.",
    ))
}

#[no_mangle]
pub extern "C" fn uc_apply_instant_close_frozen(input: *const c_char) -> *mut c_char {
    let on = parse_bool_input(&cstr_to_string(input));
    json_ptr(apply_toggle_json(
        on,
        apply_instant_close_frozen,
        "Instant close ON: a not-responding app is ended after about 1 second (HungAppTimeout, WaitToKillAppTimeout and service timeout all 1000 ms) instead of Windows waiting ~5–20 seconds.",
        "Instant close OFF: frozen-app and shutdown timeouts restored to Windows defaults.",
        "Failed to change the instant-close setting — the toggle was left unchanged.",
    ))
}

#[no_mangle]
pub extern "C" fn uc_apply_welcome_experience(input: *const c_char) -> *mut c_char {
    let on = parse_bool_input(&cstr_to_string(input));
    json_ptr(apply_toggle_json(
        on,
        apply_welcome_experience,
        "Windows welcome experience ON: the post-update setup screen may appear again.",
        "Windows welcome experience OFF: the post-update setup screen is suppressed.",
        "Failed to change the welcome-experience setting — the toggle was left unchanged.",
    ))
}

#[no_mangle]
pub extern "C" fn uc_apply_show_all_tray_icons(input: *const c_char) -> *mut c_char {
    let on = parse_bool_input(&cstr_to_string(input));
    json_ptr(apply_toggle_json(
        on,
        apply_show_all_tray_icons,
        "Show all tray icons ON: notification-area icons will no longer hide in the overflow menu.",
        "Show all tray icons OFF: Windows will hide inactive tray icons in the overflow menu again.",
        "Failed to change the tray-icons setting — the toggle was left unchanged.",
    ))
}

#[no_mangle]
pub extern "C" fn uc_apply_copilot_enabled(input: *const c_char) -> *mut c_char {
    let on = parse_bool_input(&cstr_to_string(input));
    json_ptr(apply_toggle_json(
        on,
        apply_copilot_enabled,
        "Microsoft Copilot ON: policy cleared.",
        "Microsoft Copilot OFF: policy set.",
        "Failed to change the Copilot setting — the toggle was left unchanged.",
    ))
}

#[no_mangle]
pub extern "C" fn uc_uninstall_copilot_app(_input: *const c_char) -> *mut c_char {
    let message = uninstall_copilot_app();
    json_ptr(serde_json::json!({ "message": message, "still_present": copilot_still_present() }))
}

#[no_mangle]
pub extern "C" fn uc_install_copilot_app(_input: *const c_char) -> *mut c_char {
    let message = install_copilot_app();
    json_ptr(serde_json::json!({ "message": message }))
}

#[no_mangle]
pub extern "C" fn uc_unpin_copilot_from_taskbar(_input: *const c_char) -> *mut c_char {
    json_ptr(serde_json::json!({ "removed": unpin_copilot_from_taskbar() }))
}

#[no_mangle]
pub extern "C" fn uc_reset_defaults(_input: *const c_char) -> *mut c_char {
    let (log, states, explorer_restart_needed) = reset_defaults_vec();
    json_ptr(serde_json::json!({
        "log": log, "states": states, "explorer_restart_needed": explorer_restart_needed
    }))
}

#[no_mangle]
pub extern "C" fn uc_check_update(input: *const c_char) -> *mut c_char {
    // Single source of truth is the UI's displayed version, passed in
    // by Dart. Fall back to the Rust const only if Dart sends nothing,
    // so a missed Rust bump can never again report a same-version
    // update (v1.1.1 bug: Rust was still 1.1.0 while Dart was 1.1.1).
    let text = cstr_to_string(input);
    let current = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("current_version").and_then(|x| x.as_str()).map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| APP_VERSION.to_string());
    json_ptr(check_update_value(&current))
}

#[no_mangle]
pub extern "C" fn uc_open_backup_folder(_input: *const c_char) -> *mut c_char {
    let dir = backup_dir();
    let created = std::fs::create_dir_all(&dir).is_ok();
    let ok = created
        && new_cmd("explorer.exe")
            .arg(dir.clone())
            .spawn()
            .is_ok();
    json_ptr(serde_json::json!({ "ok": ok, "path": dir.to_string_lossy() }))
}

#[no_mangle]
pub extern "C" fn uc_launch_installer(input: *const c_char) -> *mut c_char {
    let path = parse_name_input(&cstr_to_string(input));
    // Do NOT run the installer directly: it would try to replace the
    // running app exe while it is still locked, and the app exits
    // immediately after this call. Instead write a tiny .bat that waits
    // 2s (app fully closed by then), runs the installer, then deletes
    // itself. The .bat survives the app exit because cmd owns it.
    let bat = std::env::temp_dir().join("UtilityCloset-update.bat");
    let clean_path = path.replace('"', "");
    let mut bat_text = String::from("@echo off\r\n");
    bat_text.push_str("timeout /t 2 /nobreak >nul\r\n");
    bat_text.push_str("start \"\" \"");
    bat_text.push_str(&clean_path);
    bat_text.push_str("\"\r\n");
    bat_text.push_str("del \"%~f0\"\r\n");
    let ok = std::fs::write(&bat, bat_text).is_ok()
        && new_cmd("cmd")
            .arg("/C")
            .arg(bat.to_string_lossy().as_ref())
            .spawn()
            .is_ok();
    json_ptr(serde_json::json!({ "ok": ok, "path": path }))
}

// ---------------------------------------------------------------------------
// Tests — pure functions only; no Windows binaries are invoked except
// get_states(), which degrades to safe defaults on Linux.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_location_name_replaces_forbidden_chars() {
        assert_eq!(sanitize_location_name(r"\\SRV\share"), "__SRV_share");
        assert_eq!(sanitize_location_name("a/b:c*d?e\"f<g>h|i"), "a_b_c_d_e_f_g_h_i");
        assert_eq!(sanitize_location_name("  My Share  "), "My Share");
        assert_eq!(sanitize_location_name("///"), "___"); // non-empty after replace, so no fallback
        assert_eq!(sanitize_location_name("   "), "Network Location"); // empty after trim
        assert_eq!(sanitize_location_name(""), "Network Location");
    }

    #[test]
    fn parse_ver_handles_tags_and_junk() {
        assert_eq!(parse_ver("v1.0.23"), (1, 0, 23));
        assert_eq!(parse_ver("1.1.0"), (1, 1, 0));
        assert_eq!(parse_ver("v2"), (2, 0, 0));
        assert_eq!(parse_ver(""), (0, 0, 0));
        assert_eq!(parse_ver("v1.x.3"), (1, 0, 3));
        assert!(parse_ver("1.1.0") > parse_ver("1.0.23"));
    }

    #[test]
    fn config_round_trip_and_legacy_letter_fallback() {
        let drives = vec![
            DriveRow::drive("X".into(), r"\\SERVER\share".into()),
            DriveRow::location("Media".into(), r"\\SERVER\media".into()),
        ];
        let text = serialize_config(&drives);
        assert!(text.starts_with("Count=2\n"));
        assert!(text.contains("Kind1=location\n"));
        assert_eq!(parse_config_text(&text), drives);
        // Pre-1.0.12 config: Letter{i}, no Kind, no Label.
        let legacy = "Count=1\nLetter0=Y\nPath0=\\\\OLD\\data\n";
        let parsed = parse_config_text(legacy);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].kind, "drive");
        assert_eq!(parsed[0].label, "Y");
        assert_eq!(parsed[0].path, r"\\OLD\data");
        // Count is capped at MAX_DRIVES.
        let mut big = String::from("Count=20\n");
        for i in 0..20 {
            big.push_str(&format!("Label{i}=Z\nPath{i}=\\\\S\\{i}\n"));
        }
        assert_eq!(parse_config_text(&big).len(), MAX_DRIVES);
    }

    #[test]
    fn normalized_letter_rules() {
        assert_eq!(normalized_letter("x:"), Some("X".to_string()));
        assert_eq!(normalized_letter(" X "), Some("X".to_string()));
        assert_eq!(normalized_letter("XY"), None);
        assert_eq!(normalized_letter(""), None);
        assert_eq!(normalized_letter("1"), None);
    }

    #[test]
    fn backup_and_monitor_layout_json_shapes() {
        let monitors = vec![serde_json::json!({
            "device": r"\\.\DISPLAY1", "primary": true, "x": 0, "y": 0,
            "width": 2560, "height": 1440, "bits": 32, "orientation": 0, "refresh": 165
        })];
        let layout = monitor_layout_doc(&monitors);
        assert_eq!(layout["format"], "UtilityClosetMonitorLayout");
        assert_eq!(layout["version"], 1);
        assert_eq!(layout["monitors"][0]["refresh"], 165);
        let doc = backup_doc(vec![], vec![], monitors.clone(), vec![]);
        assert_eq!(doc["format"], "UtilityClosetBackup");
        assert_eq!(doc["version"], 1);
        assert_eq!(doc["app_version"], "1.1.3");
        assert_eq!(doc["monitors"][0]["device"], r"\\.\DISPLAY1");
        // Summary lines match the egui format.
        let summary = monitor_summary(&monitors);
        assert!(summary.contains(r"\\.\DISPLAY1 — 2560x1440 at (0, 0) (primary), 165 Hz"));
    }

    #[test]
    fn apply_monitor_script_carries_cds_set_primary_and_fields() {
        let monitors = vec![serde_json::json!({
            "device": r"\\.\DISPLAY1", "primary": true, "x": 0, "y": 0,
            "width": 1920, "height": 1080, "bits": 32, "orientation": 1, "refresh": 60
        })];
        let script = build_apply_monitor_script(&monitors);
        assert!(script.contains("ChangeDisplaySettingsEx"));
        assert!(script.contains("0x10")); // CDS_SET_PRIMARY
        assert!(script.contains("0x40000")); // bits
        assert!(script.contains("0x80")); // orientation
        assert!(script.contains("0x400000")); // refresh
        assert!(script.contains("ChangeDisplaySettings([IntPtr]::Zero, 0)"));
    }

    #[test]
    fn parse_drives_and_bool_inputs_are_flexible() {
        let drives = parse_drives(r#"{"drives":[{"kind":"location","label":"Media","path":"\\\\S\\m"}]}"#);
        assert_eq!(drives.len(), 1);
        assert!(drives[0].is_location());
        assert!(parse_bool_input("true"));
        assert!(parse_bool_input(r#"{"on":false}"#) == false);
        assert!(!parse_bool_input(""));
        assert_eq!(parse_name_input(r#""Foo Printer""#), "Foo Printer");
        assert_eq!(parse_name_input(r"\\SERVER\Printer"), r"\\SERVER\Printer"); // plain passthrough
        assert_eq!(parse_name_input(r#"{"name":"Foo"}"#), "Foo");
    }

    #[test]
    fn get_states_returns_safe_defaults_on_linux() {
        // No reg/powershell on Linux: every read must degrade to the
        // egui command-error defaults, never panic.
        let s = get_states_value();
        assert_eq!(s["forced_shutdown"], false);
        assert_eq!(s["instant_close_frozen"], false);
        assert_eq!(s["welcome_experience_on"], true);
        assert_eq!(s["show_all_tray_icons"], false);
        assert_eq!(s["copilot_enabled"], true);
        assert_eq!(s["copilot_app_installed"], false);
        assert!(s["backup_dir"].as_str().unwrap().ends_with("Utility Closet"));
        assert!(s["backup_file_path"]
            .as_str()
            .unwrap()
            .ends_with("UtilityCloset-Backup.json"));
    }
}
