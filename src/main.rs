#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! The Utility Closet - all-in-one Windows maintenance app (egui version).

use eframe::egui;
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Arc;

const CREATE_NO_WINDOW: u32 = 0x08000000;
const MAX_DRIVES: usize = 6;

struct DriveRow {
    letter: String,
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
    log: String,
    confirm: Option<Confirm>,
    show_about: bool,
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

impl ClosetApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(if is_dark_mode() {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
        let mut app = Self {
            drives: Vec::new(),
            log: String::new(),
            confirm: None,
            show_about: false,
        };
        app.load_config();
        if app.drives.is_empty() {
            app.drives.push(DriveRow {
                letter: "X".into(),
                path: String::new(),
            });
            app.drives.push(DriveRow {
                letter: "Y".into(),
                path: String::new(),
            });
        }
        app.log_line("The Utility Closet started (running as Administrator).");
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
}

impl eframe::App for ClosetApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("The Utility Closet");
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
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("About").clicked() {
                            self.show_about = true;
                        }
                    });
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
            egui::Window::new("About The Utility Closet")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label(
                        "The Utility Closet v1.0\n\n\
                         All-in-one Windows maintenance:\n\
                         • Network drive mapping\n\
                         • System File Checker (sfc /scannow)\n\
                         • Display cache reset\n\
                         • Icon cache rebuild\n\n\
                         Built for Shawn Rodis.",
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
        .with_title("The Utility Closet")
        .with_inner_size([660.0, 800.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png")) {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    let _ = eframe::run_native(
        "The Utility Closet",
        options,
        Box::new(|cc| Box::new(ClosetApp::new(cc))),
    );
}
