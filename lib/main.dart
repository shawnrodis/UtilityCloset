// The Utility Closet — Rust core + Flutter UI, RustDesk-style.
// 1:1 with the egui version as technically possible, 100% same
// functionality: same sidebar sections, labels, buttons, dialogs,
// and behaviours. The logic lives in rust/src/lib.rs and is called
// through lib/rust_bridge.dart (manual C-ABI FFI, NOT
// flutter_rust_bridge codegen). If utility_closet_core.dll is
// missing, RustBridge throws — there is NO Dart fallback, so a
// missing Rust build can never silently become a pure-Dart app.
//
// Sections (egui order): Overview, Network Drives & Locations,
// Printers, Backup & Restore, Repairs, Windows Annoyances, Log.
// Sidebar (210px in egui): heading "Utility Closet", version,
// nav list, "Check for updates" + status, "About".
// Window: egui was 1040x780, min 800x500 — Flutter's window size
// is runner-controlled (see workflow notes); the layout below is
// built for that size, scrollable, sidebar fixed at 210px.
// Teal accent: rgb(0,128,140) = 0xFF00808C, card radius 12.
// Theme: follows Windows light/dark (ThemeMode.system).
// Windows-only: no Mac port, by Shawn's standing decision.

import 'dart:async';
import 'dart:convert';
import 'dart:io';

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:http/http.dart' as http;

import 'rust_bridge.dart';

const String appVersion = '1.1.3';
const Color teal = Color(0xFF00808C);

void main() {
  runApp(const UtilityClosetApp());
}

class UtilityClosetApp extends StatelessWidget {
  const UtilityClosetApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Utility Closet',
      theme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
          seedColor: teal,
          brightness: Brightness.light,
        ),
        useMaterial3: true,
      ),
      darkTheme: ThemeData(
        colorScheme: ColorScheme.fromSeed(
          seedColor: teal,
          brightness: Brightness.dark,
        ),
        useMaterial3: true,
      ),
      themeMode: ThemeMode.system,
      home: const ClosetHome(),
    );
  }
}

class DriveRowData {
  String kind; // 'drive' or 'location'
  String label;
  String path;
  // Controllers persist across rebuilds (creating a controller in
  // build() would reset the cursor on every log/state update).
  late final TextEditingController labelCtrl;
  late final TextEditingController pathCtrl;
  DriveRowData({required this.kind, required this.label, required this.path}) {
    labelCtrl = TextEditingController(text: label);
    pathCtrl = TextEditingController(text: path);
  }
  Map<String, dynamic> toJson() => {'kind': kind, 'label': label, 'path': path};
  static DriveRowData fromJson(Map<String, dynamic> j) => DriveRowData(
        kind: (j['kind'] ?? 'drive').toString(),
        label: (j['label'] ?? j['letter'] ?? '').toString(),
        path: (j['path'] ?? '').toString(),
      );
}

enum Nav { overview, drives, printers, backup, repairs, annoyances, log }

const navLabels = [
  'Overview',
  'Network Drives & Locations',
  'Printers',
  'Backup & Restore',
  'Repairs',
  'Windows Annoyances',
  'Log',
];

class ClosetHome extends StatefulWidget {
  const ClosetHome({super.key});
  @override
  State<ClosetHome> createState() => _ClosetHomeState();
}

class _ClosetHomeState extends State<ClosetHome> {
  Nav _nav = Nav.overview;
  List<DriveRowData> _drives = [];
  List<String> _printerRows = [''];
  int _printerVersion = 0;
  List<String> _printers = [];
  int? _selectedPrinter;
  String _monitorsText = '';
  bool _forcedShutdown = false;
  bool _instantClose = false;
  bool _welcome = true;
  bool _trayAll = false;
  bool _copilot = true;
  String _backupPath = '';
  String _log = '';
  String _updateStatus = '';
  Map<String, dynamic>? _updateInfo;
  bool _updateDismissed = false;
  bool _updateDownloading = false;
  double _updateProgress = 0;
  bool _busy = false;
  bool _started = false;

  @override
  void initState() {
    super.initState();
    _startup();
  }

  String _ts() {
    final n = DateTime.now();
    String p(int v) => v.toString().padLeft(2, '0');
    return '${p(n.hour)}:${p(n.minute)}:${p(n.second)}';
  }

  void _logLine(String msg) {
    setState(() {
      _log += '[${_ts()}] $msg\n';
      if (_log.length > 49152) {
        final cut = _log.length - 32768;
        final pos = _log.indexOf('\n', cut);
        if (pos >= 0) _log = _log.substring(pos + 1);
      }
    });
  }

  void _logLines(dynamic logs) {
    if (logs is List) {
      for (final l in logs) {
        final s = l.toString();
        if (s.isNotEmpty) _logLine(s);
      }
    }
  }

  Future<void> _startup() async {
    try {
      final states = await RustBridge.getStates();
      final drivesRaw = await RustBridge.loadConfig();
      final drives = drivesRaw
          .map((e) => DriveRowData.fromJson(e as Map<String, dynamic>))
          .toList();
      setState(() {
        _forcedShutdown = states['forced_shutdown'] == true;
        _instantClose = states['instant_close_frozen'] == true;
        _welcome = states['welcome_experience_on'] != false;
        _trayAll = states['show_all_tray_icons'] == true;
        _copilot = states['copilot_enabled'] != false;
        _backupPath = (states['backup_file_path'] ?? '').toString();
        _drives = drives;
      });
      // Auto-detect on launch, like egui: drives/locations this PC
      // already has, whether or not the app configured them.
      final existing = await RustBridge.loadExisting(_drives);
      _logLines(existing['log']);
      if (existing['drives'] is List) {
        setState(() {
          _drives = (existing['drives'] as List)
              .map((e) => DriveRowData.fromJson(e as Map<String, dynamic>))
              .toList();
        });
      }
      final pr = await RustBridge.refreshPrinters();
      _logLines(pr['log']);
      if (pr['printers'] is List) {
        setState(() {
          _printers =
              (pr['printers'] as List).map((e) => e.toString()).toList();
          _selectedPrinter = null;
        });
      }
      if (_drives.isEmpty) {
        setState(() {
          _drives = [DriveRowData(kind: 'drive', label: 'X', path: '')];
        });
      }
      _logLine('Utility Closet started (running as Administrator).');
      _checkUpdates(silent: true);
    } catch (e) {
      _logLine('Startup failed: $e');
    }
    setState(() => _started = true);
  }

  Future<void> _checkUpdates({bool silent = false}) async {
    if (!silent) setState(() => _updateStatus = 'Checking…');
    try {
      final r = await RustBridge.checkUpdate(appVersion);
      if (r['available'] == true) {
        setState(() {
          _updateInfo = r;
          _updateDismissed = false;
          _updateStatus = '';
        });
      } else if (r['status'] == 'up_to_date') {
        setState(() {
          _updateInfo = null;
          _updateStatus = "You're up to date (v$appVersion).";
        });
      } else {
        if (!silent) {
          setState(() =>
              _updateStatus = "Couldn't reach GitHub — check your connection and try again.");
        }
      }
    } catch (_) {
      if (!silent) {
        setState(() =>
            _updateStatus = "Couldn't reach GitHub — check your connection and try again.");
      }
    }
  }

  Future<void> _downloadAndInstallUpdate() async {
    final info = _updateInfo;
    if (info == null) return;
    final url = (info['asset_url'] ?? '').toString();
    final name = (info['asset_name'] ?? '').toString();
    if (url.isEmpty || name.isEmpty) return;
    setState(() {
      _updateDownloading = true;
      _updateProgress = 0;
    });
    try {
      final req = await http.Client().send(http.Request('GET', Uri.parse(url)));
      if (req.statusCode != 200) throw StateError('HTTP ${req.statusCode}');
      final total = req.contentLength ?? (info['asset_size'] as int? ?? 0);
      final chunks = <int>[];
      var got = 0;
      await for (final chunk in req.stream) {
        chunks.addAll(chunk);
        got += chunk.length;
        if (total > 0) setState(() => _updateProgress = got / total);
      }
      // Verify before launching: a redirect/error page or truncated file
      // must never be run as an installer, and must never exit the app.
      if (chunks.isEmpty ||
          chunks.length < 2 ||
          chunks[0] != 0x4D ||
          chunks[1] != 0x5A ||
          (total > 0 && got != total)) {
        throw StateError('Download verification failed');
      }
      final dest = File('${Directory.systemTemp.path}\\$name');
      await dest.writeAsBytes(chunks);
      setState(() => _updateStatus = 'Downloaded $name — launching installer…');
      final launched = await RustBridge.launchInstaller(dest.path);
      if (launched['ok'] != true) throw StateError('Installer launch failed');
      // The launcher waits 2s before running the installer, so the app
      // is fully closed before the installer tries to replace it.
      exit(0);
    } catch (_) {
      setState(() {
        _updateDownloading = false;
        _updateStatus = 'Download failed — check your connection and try again.';
      });
    }
  }

  Future<void> _runBusy(Future<void> Function() fn) async {
    if (_busy) return;
    setState(() => _busy = true);
    try {
      await fn();
    } catch (e) {
      _logLine('Failed: $e');
    } finally {
      if (mounted) setState(() => _busy = false);
    }
  }

  // --- Drives ---
  Future<void> _mapDrives() => _runBusy(() async {
        final r = await RustBridge.mapDrives(_drives);
        _logLines(r['log']);
        if (r['drives'] is List) {
          setState(() {
            _drives = (r['drives'] as List)
                .map((e) => DriveRowData.fromJson(e as Map<String, dynamic>))
                .toList();
          });
        }
        await RustBridge.saveConfig(_drives);
        if ((r['mapped'] ?? 0) is int && (r['mapped'] as int) > 0) {
          _logLine('Network list saved for next time.');
        }
      });

  Future<void> _disconnect() => _runBusy(() async {
        final r = await RustBridge.disconnectDrives(_drives);
        _logLines(r['log']);
      });

  Future<void> _loadExisting() => _runBusy(() async {
        final r = await RustBridge.loadExisting(_drives);
        _logLines(r['log']);
        if (r['drives'] is List) {
          setState(() {
            _drives = (r['drives'] as List)
                .map((e) => DriveRowData.fromJson(e as Map<String, dynamic>))
                .toList();
          });
        }
      });

  // --- Printers ---
  Future<void> _refreshPrinters() => _runBusy(() async {
        final r = await RustBridge.refreshPrinters();
        _logLines(r['log']);
        if (r['printers'] is List) {
          setState(() {
            _printers =
                (r['printers'] as List).map((e) => e.toString()).toList();
            _selectedPrinter = null;
          });
        }
      });

  Future<void> _addPrinters() => _runBusy(() async {
        final paths =
            _printerRows.map((p) => p.trim()).where((p) => p.isNotEmpty).toList();
        if (paths.isEmpty) {
          _logLine('Enter a printer path first (e.g. \\\\SERVER\\Printer).');
          return;
        }
        final r = await RustBridge.addPrinters(paths);
        _logLines(r['log']);
        await _refreshPrinters();
      });

  Future<void> _removePrinter() => _runBusy(() async {
        final idx = _selectedPrinter;
        if (idx == null || idx >= _printers.length) {
          _logLine('Select a printer from the list first.');
          return;
        }
        final r = await RustBridge.removePrinter(_printers[idx]);
        _logLines(r['log']);
        await _refreshPrinters();
      });

  Future<void> _setDefaultPrinter() => _runBusy(() async {
        final idx = _selectedPrinter;
        if (idx == null || idx >= _printers.length) {
          _logLine('Select a printer from the list first.');
          return;
        }
        final r = await RustBridge.setDefaultPrinter(_printers[idx]);
        _logLines(r['log']);
      });

  // --- Backup ---
  Future<void> _exportBackup() => _runBusy(() async {
        final r = await RustBridge.exportBackup(_drives, _printerRows);
        _logLines(r['log']);
      });

  Future<void> _importBackup() => _runBusy(() async {
        final r = await RustBridge.importBackup();
        _logLines(r['log']);
        if (r['drives'] is List) {
          setState(() {
            _drives = (r['drives'] as List)
                .map((e) => DriveRowData.fromJson(e as Map<String, dynamic>))
                .toList();
          });
        }
        if (r['printer_rows'] is List) {
          setState(() {
            _printerRows = (r['printer_rows'] as List)
                .map((e) => e.toString())
                .toList();
            _printerVersion++;
          });
        }
      });

  Future<void> _detectMonitors() => _runBusy(() async {
        final r = await RustBridge.detectMonitors();
        _logLines(r['log']);
        final summary = (r['summary'] ?? r['text'] ?? '').toString();
        if (summary.isNotEmpty) {
          setState(() => _monitorsText = summary);
        }
      });

  Future<void> _applyMonitorLayout() => _runBusy(() async {
        final r = await RustBridge.applyMonitorLayoutFile();
        _logLines(r['log']);
      });

  Future<void> _deleteBackups() => _runBusy(() async {
        final r = await RustBridge.deleteBackups();
        _logLines(r['log']);
      });

  // --- Repairs ---
  Future<void> _runSfc() => _runBusy(() async {
        final r = await RustBridge.runSfc();
        _logLines(r['log']);
      });

  Future<void> _runDism(List<String> args, String label) => _runBusy(() async {
        final r = await RustBridge.runDism(args, label);
        _logLines(r['log']);
      });

  Future<void> _clearDisplayCache() => _runBusy(() async {
        final r = await RustBridge.clearDisplayCache();
        _logLines(r['log']);
      });

  Future<void> _rebuildIconCache() => _runBusy(() async {
        final r = await RustBridge.rebuildIconCache();
        _logLines(r['log']);
        // egui then offered a Reboot confirm dialog.
        if (!mounted) return;
        final yes = await _confirmDialog(
          'Restart PC',
          'Icon cache cleared. Restart now to finish rebuilding?',
        );
        if (yes == true) await RustBridge.reboot();
      });

  Future<bool?> _confirmDialog(String title, String text) {
    return showDialog<bool>(
      context: context,
      builder: (ctx) => AlertDialog(
        title: Text(title),
        content: Text(text),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx, false),
              child: const Text('No')),
          TextButton(
              onPressed: () => Navigator.pop(ctx, true),
              child: const Text('Yes')),
        ],
      ),
    );
  }

  // --- Annoyances ---
  Future<void> _toggleForced(bool on) => _runBusy(() async {
        final r = await RustBridge.applyForcedShutdown(on);
        _logLines(r['log']);
        if (r['ok'] == true) {
          final st = r['states'] as Map<String, dynamic>?;
          setState(() {
            _forcedShutdown = on;
            _instantClose = st?['instant_close_frozen'] == true;
          });
        }
      });

  Future<void> _toggleInstant(bool on) => _runBusy(() async {
        final r = await RustBridge.applyInstantClose(on);
        _logLines(r['log']);
        if (r['ok'] == true) {
          final st = r['states'] as Map<String, dynamic>?;
          setState(() {
            _instantClose = on;
            _forcedShutdown = st?['forced_shutdown'] == true;
          });
        }
      });

  Future<void> _toggleWelcome(bool on) => _runBusy(() async {
        final r = await RustBridge.applyWelcome(on);
        _logLines(r['log']);
        if (r['ok'] == true) {
          setState(() => _welcome = on);
        }
      });

  Future<void> _toggleTray(bool on) => _runBusy(() async {
        final r = await RustBridge.applyTray(on);
        _logLines(r['log']);
        if (r['ok'] == true) {
          setState(() => _trayAll = on);
          _logLine('Restarting Explorer so the taskbar picks this up (the taskbar will briefly disappear and come back).');
          final e = await RustBridge.restartExplorer();
          _logLines(e['log']);
        } else {
          _logLine('Failed to change the tray-icons setting — the toggle was left unchanged.');
        }
      });

  Future<void> _toggleCopilot(bool on) => _runBusy(() async {
        final r = await RustBridge.applyCopilot(on);
        // Rust log here is only "policy cleared/set" — the egui UI
        // logged the longer reinstall/uninstall lines below instead.
        if (r['ok'] != true) _logLines(r['log']);
        if (r['ok'] == true) {
          setState(() => _copilot = on);
          if (on) {
            _logLine('Microsoft Copilot ON: policy cleared — reinstalling the Copilot app (winget, needs internet; this can take a few minutes and the app may pause).');
            final m = await RustBridge.installCopilot();
            _logLines(m['log']);
            if ((m['message'] ?? '').toString().isNotEmpty) {
              _logLine((m['message'] ?? '').toString());
            }
          } else {
            _logLine("Microsoft Copilot OFF: policy set — uninstalling the Copilot app (the old policy/button alone does not remove the standalone app's taskbar icon).");
            final m = await RustBridge.uninstallCopilot();
            _logLines(m['log']);
            if ((m['message'] ?? '').toString().isNotEmpty) {
              _logLine((m['message'] ?? '').toString());
            }
            final u = await RustBridge.unpinCopilot();
            if (u['removed'] == true) {
              _logLine('Copilot taskbar pin removed — the pinned Copilot app icon will be gone once Explorer restarts.');
            } else {
              _logLine('No Copilot taskbar pin link found — if the Copilot icon is still on the taskbar, right-click it and choose Unpin from taskbar.');
            }
          }
          _logLine('Restarting Explorer so the taskbar picks this up (the taskbar will briefly disappear and come back).');
          final e = await RustBridge.restartExplorer();
          _logLines(e['log']);
        } else {
          _logLine('Failed to change the Copilot setting — the toggle was left unchanged.');
        }
      });

  Future<void> _resetDefaults() => _runBusy(() async {
        final beforeTray = _trayAll;
        final beforeCopilot = _copilot;
        _logLine("Resetting Windows Annoyances to Windows defaults (not your preferred settings): forced shutdown OFF, instant close OFF, welcome experience ON, show all tray icons OFF, Microsoft Copilot ON.");
        final r = await RustBridge.resetDefaults();
        _logLines(r['log']);
        final s = await RustBridge.getStates();
        setState(() {
          _forcedShutdown = s['forced_shutdown'] == true;
          _instantClose = s['instant_close_frozen'] == true;
          _welcome = s['welcome_experience_on'] != false;
          _trayAll = s['show_all_tray_icons'] == true;
          _copilot = s['copilot_enabled'] != false;
        });
        if (beforeTray != _trayAll || beforeCopilot != _copilot) {
          _logLine('Restarting Explorer so the taskbar picks up the reset (the taskbar will briefly disappear and come back).');
          final e = await RustBridge.restartExplorer();
          _logLines(e['log']);
        }
      });

  void _showAbout() {
    showDialog(
      context: context,
      builder: (ctx) => AlertDialog(
        title: const Text('About Utility Closet'),
        content: const SingleChildScrollView(
          child: Text(
            'Utility Closet v$appVersion\n\n'
            'All-in-one Windows maintenance:\n'
            '• Network drive and network location mapping\n'
            '• Printer add/remove\n'
            '• System File Checker (sfc /scannow)\n'
            '• DISM Windows image repair (Check / Scan / Restore Health)\n'
            '• Display cache reset\n'
            '• Icon cache rebuild\n'
            '• Forced shutdown toggle\n'
            '• Windows welcome experience toggle\n'
            '• Always show all tray icons toggle\n'
            '• Windows Annoyances Reset to defaults\n'
            '• Microsoft Copilot enable/disable toggle (off uninstalls the app, on reinstalls it)\n'
            '• Backup & restore (drives, printers, monitor layout) to Documents\\Utility Closet\n'
            '• Sidebar navigation + Overview dashboard',
          ),
        ),
        actions: [
          TextButton(
              onPressed: () => Navigator.pop(ctx), child: const Text('Close')),
        ],
      ),
    );
  }

  Widget _card(String title, List<Widget> children) {
    return Card(
      shape: RoundedRectangleBorder(borderRadius: BorderRadius.circular(12)),
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            Text(title,
                style: const TextStyle(
                    fontWeight: FontWeight.bold, color: teal)),
            const SizedBox(height: 8),
            ...children,
          ],
        ),
      ),
    );
  }

  Widget _pageOverview() {
    final configured =
        _drives.where((r) => r.path.trim().isNotEmpty).length;
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        const Text('Overview',
            style: TextStyle(fontSize: 24, fontWeight: FontWeight.bold)),
        const Text(
            'Everything at a glance — pick a section on the left for the full controls.'),
        const SizedBox(height: 8),
        _card('Network Drives & Locations', [
          Text('$configured drive(s)/location(s) in the app list, including ones this PC already had.'),
          ElevatedButton(
              onPressed: () => setState(() => _nav = Nav.drives),
              child: const Text('Open Network Drives & Locations')),
        ]),
        const SizedBox(height: 12),
        _card('Printers', [
          Text('${_printers.length} printer(s) installed (loaded on launch; Refresh List re-scans).'),
          Row(children: [
            ElevatedButton(
                onPressed: _refreshPrinters, child: const Text('Refresh List')),
            const SizedBox(width: 8),
            ElevatedButton(
                onPressed: () => setState(() => _nav = Nav.printers),
                child: const Text('Open Printers')),
          ]),
        ]),
        const SizedBox(height: 12),
        _card('Backup & Restore', [
          Text('Backups live in $_backupPath'),
          Row(children: [
            ElevatedButton(
                onPressed: _exportBackup, child: const Text('Export Backup')),
            const SizedBox(width: 8),
            ElevatedButton(
                onPressed: () => setState(() => _nav = Nav.backup),
                child: const Text('Open Backup')),
          ]),
        ]),
        const SizedBox(height: 12),
        _card('Windows Annoyances', [
          Text(
              'Forced shutdown ${_forcedShutdown ? 'on' : 'off'} · Instant close ${_instantClose ? 'on' : 'off'} · Welcome experience ${_welcome ? 'on' : 'off'} · Tray icons ${_trayAll ? 'all shown' : 'normal'} · Copilot ${_copilot ? 'on' : 'off'}'),
          ElevatedButton(
              onPressed: () => setState(() => _nav = Nav.annoyances),
              child: const Text('Open Windows Annoyances')),
        ]),
      ],
    );
  }

  Widget _pageDrives() {
    return _card('Network Drives & Locations', [
      ..._drives.asMap().entries.map((e) {
        final i = e.key;
        final row = e.value;
        return Padding(
          padding: const EdgeInsets.only(bottom: 6),
          child: Row(
            children: [
              DropdownButton<String>(
                value: row.kind,
                items: const [
                  DropdownMenuItem(value: 'drive', child: Text('Drive')),
                  DropdownMenuItem(value: 'location', child: Text('Location')),
                ],
                onChanged: (v) => setState(() => row.kind = v ?? 'drive'),
              ),
              const SizedBox(width: 6),
              SizedBox(
                width: row.kind == 'drive' ? 60 : 140,
                child: TextField(
                  controller: row.labelCtrl,
                  decoration: InputDecoration(
                      hintText: row.kind == 'drive' ? 'X' : 'Name',
                      border: const OutlineInputBorder()),
                  onChanged: (v) => row.label = v,
                ),
              ),
              const SizedBox(width: 6),
              Expanded(
                child: TextField(
                  controller: row.pathCtrl,
                  decoration: const InputDecoration(
                      hintText: r'\\SERVER\share',
                      border: OutlineInputBorder()),
                  onChanged: (v) => row.path = v,
                ),
              ),
              const SizedBox(width: 6),
              ElevatedButton(
                  onPressed: () => setState(() => _drives.removeAt(i)),
                  child: const Text('Remove')),
            ],
          ),
        );
      }),
      Row(children: [
        ElevatedButton(
            onPressed: () {
              if (_drives.length < 12) {
                setState(() => _drives.add(
                    DriveRowData(kind: 'drive', label: '', path: '')));
              }
            },
            child: const Text('Add Drive')),
        const SizedBox(width: 8),
        ElevatedButton(onPressed: _mapDrives, child: const Text('Map Drives')),
        const SizedBox(width: 8),
        ElevatedButton(onPressed: _disconnect, child: const Text('Disconnect')),
        const SizedBox(width: 8),
        ElevatedButton(
            onPressed: _loadExisting, child: const Text('Load Existing')),
      ]),
      const SizedBox(height: 8),
      const Text(
          'Drive = a letter-mapped drive (net use, e.g. X:). Location = a network location shortcut in This PC > Network locations, with no drive letter — just a name and a UNC path. This list always includes drives and locations this PC already has, whether or not the app configured them — it is scanned on launch, and Load Existing re-scans it. Removing a row only hides it until the next launch; Disconnect actually removes it. Map Drives adds both kinds; Disconnect removes both kinds.'),
    ]);
  }

  Widget _pagePrinters() {
    return _card('Printers', [
      ..._printerRows.asMap().entries.map((e) {
        final i = e.key;
        return Padding(
          padding: const EdgeInsets.only(bottom: 6),
          child: Row(children: [
            Expanded(
              child: TextFormField(
                key: ValueKey('printer-$i-$_printerVersion'),
                initialValue: _printerRows[i],
                decoration: const InputDecoration(
                    hintText: r'\\SERVER\Printer',
                    border: OutlineInputBorder()),
                onChanged: (v) => _printerRows[i] = v,
              ),
            ),
            const SizedBox(width: 6),
            ElevatedButton(
                onPressed: () => setState(() => _printerRows.removeAt(i)),
                child: const Text('Remove')),
          ]),
        );
      }),
      Row(children: [
        ElevatedButton(
            onPressed: () {
              if (_printerRows.length < 6) {
                setState(() => _printerRows.add(''));
              }
            },
            child: const Text('Add Printer')),
        const SizedBox(width: 8),
        ElevatedButton(
            onPressed: _addPrinters, child: const Text('Add Printers')),
        const SizedBox(width: 8),
        ElevatedButton(
            onPressed: _refreshPrinters, child: const Text('Refresh List')),
      ]),
      const SizedBox(height: 8),
      if (_printers.isNotEmpty) ...[
        SizedBox(
          height: 120,
          child: ListView.builder(
            itemCount: _printers.length,
            itemBuilder: (ctx, i) => ListTile(
              title: Text(_printers[i]),
              selected: _selectedPrinter == i,
              onTap: () => setState(() => _selectedPrinter = i),
            ),
          ),
        ),
        Row(children: [
          ElevatedButton(
              onPressed: _setDefaultPrinter,
              child: const Text('Set as Default')),
          const SizedBox(width: 8),
          ElevatedButton(
              onPressed: _removePrinter, child: const Text('Remove Printer')),
        ]),
      ] else
        const Text(
            'No printers installed on this PC (list loaded on launch; Refresh List re-scans).'),
      const SizedBox(height: 8),
      const Text(
          'Queue up printer paths, then Add Printers adds them all at once. Select a printer from the list below to set it as default or remove it.'),
    ]);
  }

  Widget _pageBackup() {
    return Column(children: [
      _card('Backup & Restore', [
        Row(children: [
          ElevatedButton(
              onPressed: _exportBackup, child: const Text('Export Backup')),
          const SizedBox(width: 8),
          ElevatedButton(
              onPressed: _importBackup, child: const Text('Import Backup')),
          const SizedBox(width: 8),
          ElevatedButton(
              onPressed: () async {
                await RustBridge.openBackupFolder();
              },
              child: const Text('Open Backup Folder')),
          const SizedBox(width: 8),
          ElevatedButton(
              onPressed: () async {
                final yes = await _confirmDialog(
                  'Delete Backups',
                  'This deletes the saved backups in Documents\\Utility Closet:\nUtilityCloset-Backup.json, Monitor-Layout.json, and the Display-*.reg files.\n\nIt does NOT change any live drive, printer, or monitor setting — it only removes the saved files, so Import will have nothing to restore until you Export again.\n\nContinue?',
                );
                if (yes == true) await _deleteBackups();
              },
              child: const Text('Delete Backups')),
        ]),
        const SizedBox(height: 8),
        Text('Saves to $_backupPath'),
        const Text(
            'Export snapshots what is actually on this PC right now: mapped network drives, installed printers, and the full monitor layout (position, size, primary display, orientation, refresh rate) — plus any drive/printer rows typed into the app that are not currently live. Import maps those drives back, re-adds the network printers, and applies the monitor layout to displays that are currently connected (missing displays are skipped, never forced). Display registry keys are also saved as Display-*.reg files in the same folder as a fallback.'),
      ]),
      const SizedBox(height: 12),
      _card('Monitor Layout', [
        Row(children: [
          ElevatedButton(
              onPressed: _detectMonitors,
              child: const Text('Detect Monitors')),
          const SizedBox(width: 8),
          ElevatedButton(
              onPressed: _applyMonitorLayout,
              child: const Text('Apply Monitor Layout')),
        ]),
        const SizedBox(height: 8),
        if (_monitorsText.isEmpty)
          const Text(
              'Click Detect Monitors to list this PC\'s displays (device, resolution, position, primary, orientation, refresh rate) — that list is what gets backed up and restored.')
        else ...[
          const Text('Detected monitors:'),
          SelectableText(_monitorsText,
              style: const TextStyle(fontFamily: 'monospace')),
        ],
        const Text(
            'Detect also saves Monitor-Layout.json in the backup folder, so the layout can be applied on its own without a full backup. Apply only affects displays that are currently connected; missing displays are skipped, never forced.'),
      ]),
    ]);
  }

  Widget _pageRepairs() {
    return Column(children: [
      _card('System File Checker', [
        ElevatedButton(onPressed: _runSfc, child: const Text('Run SFC Scan')),
        const Text(
            'Scans and repairs protected Windows system files (sfc /scannow). Runs in its own window — takes 10-20 minutes.'),
      ]),
      const SizedBox(height: 12),
      _card('DISM — Windows Image Repair', [
        Row(children: [
          ElevatedButton(
              onPressed: () => _runDism(
                  ['/Online', '/Cleanup-Image', '/CheckHealth'],
                  '/Online /Cleanup-Image /CheckHealth'),
              child: const Text('Check Health')),
          const SizedBox(width: 8),
          ElevatedButton(
              onPressed: () => _runDism(
                  ['/Online', '/Cleanup-Image', '/ScanHealth'],
                  '/Online /Cleanup-Image /ScanHealth'),
              child: const Text('Scan Health')),
          const SizedBox(width: 8),
          ElevatedButton(
              onPressed: () => _runDism(
                  ['/Online', '/Cleanup-Image', '/RestoreHealth'],
                  '/Online /Cleanup-Image /RestoreHealth'),
              child: const Text('Restore Health')),
        ]),
        const Text(
            'Check Health is a quick check for whether the image has been flagged as corrupted; Scan Health is a deeper scan for component store damage. Restore Health repairs the image, downloading replacement files via Windows Update — it takes a while (10-20+ minutes) and needs an internet connection for the download part. Each runs in its own window so you can watch progress.'),
        const Text(
            'This is the same repair as the PowerShell alternative (Repair-WindowsImage -Online -RestoreHealth) — no separate PowerShell button is needed.'),
      ]),
      const SizedBox(height: 12),
      _card('Display Cache', [
        ElevatedButton(
            onPressed: () async {
              final yes = await _confirmDialog(
                'Clear Display Cache',
                'This deletes the GPU display configuration keys from the registry.\n\nYou must RESTART your PC afterward for it to take effect.\n\nContinue?',
              );
              if (yes == true) await _clearDisplayCache();
            },
            child: const Text('Clear & Reset Display Cache')),
        const Text(
            'Deletes the GPU display configuration registry keys. You MUST restart your PC afterward.'),
      ]),
      const SizedBox(height: 12),
      _card('Icon Cache', [
        ElevatedButton(
            onPressed: () async {
              final yes = await _confirmDialog(
                'Rebuild Icon Cache',
                'Explorer will be restarted and the icon cache deleted.\n\nSAVE ALL OPEN WORK first.\n\nContinue?',
              );
              if (yes == true) await _rebuildIconCache();
            },
            child: const Text('Rebuild Icon Cache')),
        const Text(
            'Deletes the icon cache and restarts Explorer. Save all open work first.'),
      ]),
    ]);
  }

  Widget _toggleRow(String label, bool value, String desc,
      String? warning, Future<void> Function(bool) onChanged) {
    return Column(
      crossAxisAlignment: CrossAxisAlignment.start,
      children: [
        CheckboxListTile(
          title: Text(label),
          value: value,
          onChanged: (v) {
            if (v != null) onChanged(v);
          },
          controlAffinity: ListTileControlAffinity.leading,
          contentPadding: EdgeInsets.zero,
        ),
        Text(desc),
        if (warning != null) Text(warning),
        const SizedBox(height: 6),
      ],
    );
  }

  Widget _pageAnnoyances() {
    return _card('Windows Annoyances', [
      _toggleRow(
        'Force shutdown / restart (no warning)',
        _forcedShutdown,
        "Changes what Windows' own Shut down / Restart buttons do: with this on, Windows ends blocking apps itself instead of stopping at the warning. Applies immediately — no reboot needed — and takes effect the next time Windows is told to shut down or restart.",
        'Warning: with this on, unsaved work in open apps can be lost, because Windows will no longer stop and ask.',
        _toggleForced,
      ),
      _toggleRow(
        'Instantly close frozen apps (no waiting)',
        _instantClose,
        'With this on, Windows treats an app as frozen and ends it after about 1 second, instead of waiting several seconds (and, at shutdown, up to 20 seconds) before acting. Applies immediately — no reboot needed.',
        "Warning: with this on, an app that is merely slow (not truly frozen) can be ended sooner, and unsaved work in it can be lost. This shares Windows' hung-app settings with Force shutdown above, so turning it on also turns Force shutdown on, and turning Force shutdown off turns this off.",
        _toggleInstant,
      ),
      _toggleRow(
        'Windows welcome experience after updates',
        _welcome,
        'This is the white post-update setup screen ("Let\'s finish setting up your device" / "Get even more out of Windows") with OneDrive backup, Microsoft 365, and similar prompts. Turning it off suppresses that screen.',
        'Note: Microsoft can reintroduce new prompts in major updates, so this suppresses the current screen rather than guaranteeing it never returns in any future form.',
        _toggleWelcome,
      ),
      _toggleRow(
        'Always show all taskbar tray icons',
        _trayAll,
        'Stops Windows hiding system-tray icons in the overflow menu behind the up-arrow, so every notification-area icon stays visible on the taskbar. Applying this restarts Explorer — the taskbar will briefly disappear and come back.',
        'Caveat: on Windows 11 some apps\' per-app overflow settings can still override this, so it is the closest thing to a global switch, not a guaranteed override for every app.',
        _toggleTray,
      ),
      _toggleRow(
        'Microsoft Copilot in Windows',
        _copilot,
        'Turning it off uninstalls the Microsoft Copilot app in whatever form it is installed — Store app, winget by name, or its Programs and Features uninstaller — and sets the TurnOffWindowsCopilot policy; turning it back on clears the policy and reinstalls the app via winget (needs internet, can take a few minutes). Applying this restarts Explorer — the taskbar will briefly disappear and come back.',
        'Caveat: this covers the Microsoft Copilot app only, not Microsoft 365 Copilot (a different app, left alone). A major Windows update can occasionally reset policy keys or re-provision the app, so the toggle may need a re-click after one of those. Turning it off also removes the pinned Copilot icon from the taskbar (best-effort, via the pinned-shortcut folder); turning it back on does not re-pin it — re-pin from Start if you want it back.',
        _toggleCopilot,
      ),
      ElevatedButton(
          onPressed: _resetDefaults, child: const Text('Reset to defaults')),
      const Text(
          '"Defaults" here means Windows\' own defaults, not your preferred settings: forced shutdown OFF, instant close OFF, welcome experience ON (the post-update setup screen is allowed again — that is the Windows default, even though you will usually have it off), show all tray icons OFF, and Microsoft Copilot ON (reinstalled if it was removed). This covers every toggle on this page. If the tray-icons or Copilot state changed, Explorer restarts once so the taskbar picks it up.'),
    ]);
  }

  Widget _pageLog() {
    return _card('Log', [
      ElevatedButton(
          onPressed: () async {
            await Clipboard.setData(ClipboardData(text: _log));
            _logLine('Log copied to clipboard.');
          },
          child: const Text('Copy Log')),
      const SizedBox(height: 8),
      SizedBox(
        height: 600,
        child: SingleChildScrollView(
          child: SelectableText(_log,
              style: const TextStyle(fontFamily: 'monospace')),
        ),
      ),
    ]);
  }

  Widget _body() {
    switch (_nav) {
      case Nav.overview:
        return _pageOverview();
      case Nav.drives:
        return _pageDrives();
      case Nav.printers:
        return _pagePrinters();
      case Nav.backup:
        return _pageBackup();
      case Nav.repairs:
        return _pageRepairs();
      case Nav.annoyances:
        return _pageAnnoyances();
      case Nav.log:
        return _pageLog();
    }
  }

  @override
  Widget build(BuildContext context) {
    final navIndex = Nav.values.indexOf(_nav);
    return Scaffold(
      body: Row(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          SizedBox(
            width: 210,
            child: SingleChildScrollView(
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  const Padding(
                    padding: EdgeInsets.all(12),
                    child: Column(
                      crossAxisAlignment: CrossAxisAlignment.start,
                      children: [
                        Text('Utility Closet',
                            style: TextStyle(
                                fontSize: 24, fontWeight: FontWeight.bold)),
                        Text('v$appVersion'),
                      ],
                    ),
                  ),
                  const Divider(),
                  ...List.generate(navLabels.length, (i) {
                    return ListTile(
                      title: Text(navLabels[i]),
                      selected: i == navIndex,
                      selectedTileColor: teal.withOpacity(0.15),
                      onTap: () =>
                          setState(() => _nav = Nav.values[i]),
                    );
                  }),
                  const Divider(),
                  Padding(
                    padding: const EdgeInsets.symmetric(horizontal: 12),
                    child: ElevatedButton(
                        onPressed: () => _checkUpdates(silent: false),
                        child: const Text('Check for updates')),
                  ),
                  if (_updateStatus.isNotEmpty)
                    Padding(
                      padding: const EdgeInsets.all(12),
                      child: Text(_updateStatus),
                    ),
                  const Divider(),
                  Padding(
                    padding: const EdgeInsets.symmetric(horizontal: 12),
                    child: ElevatedButton(
                        onPressed: _showAbout, child: const Text('About')),
                  ),
                  const SizedBox(height: 12),
                ],
              ),
            ),
          ),
          const VerticalDivider(width: 1),
          Expanded(
            child: SingleChildScrollView(
              padding: const EdgeInsets.all(12),
              child: Column(
                crossAxisAlignment: CrossAxisAlignment.start,
                children: [
                  if (_updateInfo != null && !_updateDismissed)
                    Card(
                      child: Padding(
                        padding: const EdgeInsets.all(12),
                        child: Row(children: [
                          Expanded(
                            child: Text(
                                'Update available: v${_updateInfo!['version']} (you have v$appVersion)',
                                style: const TextStyle(
                                    fontWeight: FontWeight.bold)),
                          ),
                          if (_updateDownloading)
                            Text(
                                'Downloading… ${(_updateProgress * 100).toStringAsFixed(0)}%')
                          else
                            ElevatedButton(
                                onPressed: _downloadAndInstallUpdate,
                                child: const Text('Download & Install')),
                          const SizedBox(width: 8),
                          TextButton(
                              onPressed: () => setState(
                                  () => _updateDismissed = true),
                              child: const Text('Dismiss')),
                        ]),
                      ),
                    ),
                  const SizedBox(height: 4),
                  if (_busy) const LinearProgressIndicator(),
                  _body(),
                  if (!_started)
                    const Padding(
                      padding: EdgeInsets.all(12),
                      child: Text('Loading…'),
                    ),
                ],
              ),
            ),
          ),
        ],
      ),
    );
  }
}
