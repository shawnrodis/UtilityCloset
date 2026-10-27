// Rust core bridge (RustDesk-style: Rust logic, Flutter UI).
// Logic lives in rust/src/lib.rs, exposed as a plain C ABI with
// JSON in/out and uc_free_string. Manual C-ABI FFI, NOT
// flutter_rust_bridge codegen.
// Lessons from FileNamerAI, applied here and NOT repeated:
//  - Utf8 is in package:ffi/ffi.dart, unprefixed — NEVER ffi.Utf8.
//  - Isolate.run in current Dart takes only a computation, no
//    message argument — wrap as Isolate.run(() => _fn(args)).
//  - If the DLL is missing, these calls throw — NO Dart fallback,
//    so a missing Rust build can never silently become pure-Dart.
// All PowerShell/winget/registry calls run in isolates, so the UI
// thread is never blocked by a slow system call (Copilot winget
// uninstall/install can take minutes).

import 'dart:convert';
import 'dart:ffi' as ffi;
import 'dart:isolate';

import 'package:ffi/ffi.dart';

// ignore: unused_import — kept for the isolate pattern documentation
// (all calls below use Isolate.run closures).

typedef _JsonNative = ffi.Pointer<Utf8> Function(ffi.Pointer<Utf8>);
typedef _JsonDart = ffi.Pointer<Utf8> Function(ffi.Pointer<Utf8>);
typedef _NoArgNative = ffi.Pointer<Utf8> Function();
typedef _NoArgDart = ffi.Pointer<Utf8> Function();
typedef _FreeNative = ffi.Void Function(ffi.Pointer<Utf8>);
typedef _FreeDart = void Function(ffi.Pointer<Utf8>);

class RustBridge {
  static ffi.DynamicLibrary _open() {
    // Windows-only app: only the Windows DLL name is supported.
    return ffi.DynamicLibrary.open('utility_closet_core.dll');
  }

  static String _readAndFree(
      ffi.DynamicLibrary lib, ffi.Pointer<Utf8> ptr) {
    if (ptr == ffi.nullptr) {
      throw StateError('Rust core returned a null string');
    }
    final result = ptr.toDartString();
    final free =
        lib.lookupFunction<_FreeNative, _FreeDart>('uc_free_string');
    free(ptr);
    return result;
  }

  static Map<String, dynamic> _decode(String s) {
    final v = jsonDecode(s);
    if (v is Map<String, dynamic>) return v;
    return {'value': v};
  }

  static String _call(String name, String inputJson) {
    final lib = _open();
    final fn = lib.lookupFunction<_JsonNative, _JsonDart>(name);
    final ptr = inputJson.toNativeUtf8();
    try {
      return _readAndFree(lib, fn(ptr));
    } finally {
      malloc.free(ptr);
    }
  }

  static String _callNoArg(String name) {
    final lib = _open();
    final fn = lib.lookupFunction<_NoArgNative, _NoArgDart>(name);
    return _readAndFree(lib, fn());
  }

  static Future<Map<String, dynamic>> _iso(
      String name, String inputJson) async {
    final json = await Isolate.run(() => _call(name, inputJson));
    return _decode(json);
  }

  static Future<Map<String, dynamic>> _isoNoArg(String name) async {
    // Every uc_* except uc_version/uc_free_string takes a
    // *const c_char input (ignored for no-arg calls) — calling
    // one with no argument would read garbage, so pass "".
    final json = await Isolate.run(() => _call(name, ''));
    return _decode(json);
  }

  static Future<Map<String, dynamic>> getStates() =>
      _isoNoArg('uc_get_states');

  static Future<List<dynamic>> loadConfig() async {
    final r = await _isoNoArg('uc_load_config');
    if (r['drives'] is List) return r['drives'] as List;
    return [];
  }

  static Future<Map<String, dynamic>> saveConfig(List<dynamic> drives) =>
      _iso('uc_save_config', jsonEncode({'drives': drives}));

  static Future<Map<String, dynamic>> loadExisting(List<dynamic> drives) =>
      _iso('uc_load_existing', jsonEncode({'drives': drives}));

  static Future<Map<String, dynamic>> mapDrives(List<dynamic> drives) =>
      _iso('uc_map_drives', jsonEncode({'drives': drives}));

  static Future<Map<String, dynamic>> disconnectDrives(
          List<dynamic> drives) =>
      _iso('uc_disconnect_drives', jsonEncode({'drives': drives}));

  static Future<Map<String, dynamic>> refreshPrinters() =>
      _isoNoArg('uc_refresh_printers');

  static Future<Map<String, dynamic>> addPrinters(List<String> paths) =>
      _iso('uc_add_printers', jsonEncode({'paths': paths}));

  static Future<Map<String, dynamic>> removePrinter(String name) =>
      _iso('uc_remove_printer', jsonEncode({'name': name}));

  static Future<Map<String, dynamic>> setDefaultPrinter(String name) =>
      _iso('uc_set_default_printer', jsonEncode({'name': name}));

  static Future<Map<String, dynamic>> detectMonitors() =>
      _isoNoArg('uc_detect_monitors');

  static Future<Map<String, dynamic>> exportBackup(
          List<dynamic> drives, List<String> printerRows) =>
      _iso('uc_export_backup',
          jsonEncode({'drives': drives, 'printer_rows': printerRows}));

  static Future<Map<String, dynamic>> importBackup() =>
      _isoNoArg('uc_import_backup');

  static Future<Map<String, dynamic>> applyMonitorLayoutFile() =>
      _isoNoArg('uc_apply_monitor_layout_file');

  static Future<Map<String, dynamic>> deleteBackups() =>
      _isoNoArg('uc_delete_backups');

  static Future<Map<String, dynamic>> runSfc() => _isoNoArg('uc_run_sfc');

  static Future<Map<String, dynamic>> runDism(
          List<String> args, String label) =>
      _iso('uc_run_dism', jsonEncode({'args': args, 'label': label}));

  static Future<Map<String, dynamic>> clearDisplayCache() =>
      _isoNoArg('uc_clear_display_cache');

  static Future<Map<String, dynamic>> rebuildIconCache() =>
      _isoNoArg('uc_rebuild_icon_cache');

  static Future<Map<String, dynamic>> reboot() => _isoNoArg('uc_reboot');

  static Future<Map<String, dynamic>> restartExplorer() =>
      _isoNoArg('uc_restart_explorer');

  static Future<Map<String, dynamic>> applyForcedShutdown(bool on) =>
      _iso('uc_apply_forced_shutdown', jsonEncode({'on': on}));

  static Future<Map<String, dynamic>> applyInstantClose(bool on) =>
      _iso('uc_apply_instant_close_frozen', jsonEncode({'on': on}));

  static Future<Map<String, dynamic>> applyWelcome(bool on) =>
      _iso('uc_apply_welcome_experience', jsonEncode({'on': on}));

  static Future<Map<String, dynamic>> applyTray(bool on) =>
      _iso('uc_apply_show_all_tray_icons', jsonEncode({'on': on}));

  static Future<Map<String, dynamic>> applyCopilot(bool on) =>
      _iso('uc_apply_copilot_enabled', jsonEncode({'on': on}));

  static Future<Map<String, dynamic>> uninstallCopilot() =>
      _isoNoArg('uc_uninstall_copilot_app');

  static Future<Map<String, dynamic>> installCopilot() =>
      _isoNoArg('uc_install_copilot_app');

  static Future<Map<String, dynamic>> unpinCopilot() =>
      _isoNoArg('uc_unpin_copilot_from_taskbar');

  static Future<Map<String, dynamic>> resetDefaults() =>
      _isoNoArg('uc_reset_defaults');

  static Future<Map<String, dynamic>> checkUpdate(String currentVersion) =>
      _iso('uc_check_update', jsonEncode({'current_version': currentVersion}));

  static Future<void> openBackupFolder() async {
    await _isoNoArg('uc_open_backup_folder');
  }

  static Future<Map<String, dynamic>> launchInstaller(String path) async {
    return await _iso('uc_launch_installer', jsonEncode({'path': path}));
  }

  static String version() => _callNoArg('uc_version');
}
