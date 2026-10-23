# File manager sync badges — design (started 2026-10-23, branch `file-badges`, not released)

Goal: a small emblem on files and folders inside a synced OneDrive/SharePoint folder, like Windows:
synced (green check), syncing (blue arrows), not synced / error (red !), paused (grey pause).
Dolphin, Nemo, Caja. GNOME Files can't (emblems were removed in GNOME 40+).

## Where the status comes from
The tray already follows each account's client log (tray.rs LogEvent, parse_progress), so it knows:
- the account phase (core::Phase: Syncing, UpToDate, Paused, Error…)
- files being uploaded/downloaded right now (Uploading new file / modified file / Downloading file)
- files the client refused (Status.blocked, with reasons)

Status of a path = the first that applies:
1. not under any account's sync_dir → "" (no badge)
2. in that account's blocked list (or a folder containing one) → "error"
3. being transferred now (or a folder containing one) → "syncing"
4. account Paused/Off/Stopped → "paused"; Error/SignedOut/NeedsResync/BigDelete → "error"
5. account Syncing → "syncing" only for paths in the transfer set, else "synced"
6. otherwise → "synced"
Skipped by the user's rules (skip_file/skip_dir/.nosync) → "" (cheap check against the same
rules the Options page edits; skipped for now if the check is costly, then shown as "synced").

## How file managers ask
The tray serves a session D-Bus object (zbus, already a dependency):
- name `io.github.sweatshopking.LinuxOnedriveGui.Badges`, path `/io/github/sweatshopking/LinuxOnedriveGui/Badges`
- `GetStatus(as paths) -> as` (same order; values above)
- signal `Changed(as paths)` (empty list = "ask again for everything"), sent when a file starts or
  finishes transferring, the blocked list changes, or an account's phase changes.
No tray running → no badges (plugins treat a D-Bus error as "").

## Plugins
- Nemo / Caja: one Python extension each (nemo-python / caja-python), InfoProvider.update_file_info
  → Gio D-Bus call (cached per folder, refreshed on Changed) → file.add_emblem("emblem-<x>").
  Installed per user: ~/.local/share/nemo-python/extensions, ~/.local/share/caja-python/extensions.
- Dolphin: C++ KOverlayIconPlugin (KF6 KIOWidgets), plugin id in kf6/overlayicon. Needs building
  against the distro's KF6 → system plugin dir → installed with pkexec in the client-install step
  (checkbox, default on), or shipped inside the .deb/.rpm built per distro. Ubuntu 24.04 is still
  KF5/Plasma 5: KF5 build too, or Dolphin badges only on KF6 distros (decide later).
- Emblem icons: standard names where they exist (emblem-default/emblem-synchronizing/
  emblem-important/emblem-pause), so the user's icon theme draws them.

## Status log (update after every step)
- 2026-10-23 19:0x: design written. Next: src/badges.rs (state + D-Bus), hook into tray poll.
- 2026-10-23 19:1x: src/badges.rs (status_of + zbus GetStatus/Changed, 2 unit tests pass) wired into tray.rs (Acc.moving from Progress events, publish_badges after log events + every 3 s poll, badges::serve() at tray start). Next: clippy, Nemo/Caja python extension, Dolphin C++ plugin.
- 2026-10-23 19:3x: plugins/nemo-caja/linux-onedrive-gui-badges.py, plugins/emblems/*.svg (4 emblems, look ok), plugins/dolphin (badges.cpp KOverlayIconPlugin + CMake, NOT built yet: build in archlinux container). TODO: Nemo 'shown' dict unbounded (cap), install step (servicemenu-like user install for nemo/caja + emblems into ~/.local/share/icons/hicolor/scalable/emblems).
- 2026-10-23 19:5x: Dolphin plugin BUILDS (archlinux container, KF6). clippy clean. Local WIP commit on branch file-badges (not pushed). Next: install helper (user-level nemo/caja ext + emblems; Dolphin .so via pkexec), live test with a fake/real tray in a nested session.
- 2026-10-23 20:0x: badges::install_user_files (emblems -> ~/.local/share/icons/hicolor/scalable/emblems, nemo/caja ext if the FM is installed) at tray start; uninstall removes them. 66 tests pass, clippy clean, committed (local). Nemo shown-dict capped at 5000. Next: live test (fake badge D-Bus service + Dolphin with QT_PLUGIN_PATH in nested KWin), Dolphin .so install path (pkexec / packages).
- 2026-10-23 19:0x(local 18:5x): live Dolphin GUI test in private dbus session on real Wayland: Dolphin shows EMPTY folder view even WITHOUT plugin -> test env problem (KIO listing in private session), not plugin. Switched to headless plugin test in archlinux container (plugins/dolphin/test).
- 2026-10-23 (local 19:0x): headless plugin test PASSES (plugins/dolphin/test: QPluginLoader like Dolphin's KFileItemModelRolesUpdater [from memory, confirm in a real Dolphin], fake badge service over dbus-run-session in archlinux container): synced/syncing/error/none all correct. Still to do: real-Dolphin visual check (on josh's desktop when he's back, needs the .so in a QT_PLUGIN_PATH dir + a tray build with badges), Nemo/Caja live test (container with nemo-python), Dolphin .so distribution decision (KF6 only? pkexec install vs packages), README section.
