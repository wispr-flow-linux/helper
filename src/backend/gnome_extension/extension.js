// Wispr Flow Window Bridge — GNOME Shell extension (GNOME 45+, ESM modules).
//
// Runs inside the privileged gnome-shell process and exports a small D-Bus
// interface giving the focused window's identity plus a focus-change signal.
// This is the "Window Calls" / "Focused Window D-Bus" pattern: the built-in
// org.gnome.Shell.Introspect API is gated behind unsafe_mode on modern GNOME
// (AccessDenied to normal callers), so the Wispr Flow Linux helper bundles and
// enables this extension instead and talks to it over the session bus.
//
// Interface:
//   name      : org.wispr.flow.WindowBridge   (well-known bus name we own)
//   object    : /org/wispr/flow/WindowBridge
//   interface : org.wispr.flow.WindowBridge
//     GetFocusedWindow() -> (s)   JSON {appId,title,pid,wmClass} ("" if none)
//     GetWindowList()    -> (s)   JSON array of the same shape (normal windows)
//     FocusChanged(s)             signal carrying the focused-window JSON

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'org.wispr.flow.WindowBridge';
const OBJECT_PATH = '/org/wispr/flow/WindowBridge';

const IFACE_XML = `
<node>
  <interface name="org.wispr.flow.WindowBridge">
    <method name="GetFocusedWindow">
      <arg type="s" direction="out" name="json"/>
    </method>
    <method name="GetWindowList">
      <arg type="s" direction="out" name="json"/>
    </method>
    <signal name="FocusChanged">
      <arg type="s" name="json"/>
    </signal>
  </interface>
</node>`;

// The focused window, or — when mutter reports none (focus on the shell /
// overview, or a session that hasn't received real input yet) — the
// most-recently-used normal, taskbar-visible window. For a dictation helper the
// MRU window is the right "where would my text go" answer when nothing holds
// keyboard focus; mirrors how the macOS/Windows helpers keep a last-active app.
function focusedOrMru() {
    const win = global.display.get_focus_window();
    if (win)
        return win;
    try {
        const ws = global.workspace_manager.get_active_workspace();
        const list = global.display.get_tab_list(Meta.TabList.NORMAL, ws);
        for (const w of list) {
            if (w && w.window_type === Meta.WindowType.NORMAL && !w.is_skip_taskbar())
                return w;
        }
    } catch (e) {}
    return null;
}

// Build the {appId,title,pid,wmClass} object for a Meta.Window, resolving the
// real .desktop app id via Shell.WindowTracker.
function windowInfo(win) {
    if (!win)
        return {appId: '', title: '', pid: 0, wmClass: ''};

    let appId = '';
    try {
        const tracker = Shell.WindowTracker.get_default();
        const app = tracker.get_window_app(win);
        if (app)
            appId = app.get_id() ?? '';
    } catch (e) {
        // get_window_app can fail transiently while a window is mapping.
    }

    let title = '';
    try {
        title = win.get_title() ?? '';
    } catch (e) {}

    let pid = 0;
    try {
        pid = win.get_pid() ?? 0;
        if (pid < 0)
            pid = 0;
    } catch (e) {}

    let wmClass = '';
    try {
        wmClass = win.get_wm_class() ?? '';
    } catch (e) {}

    return {appId, title, pid, wmClass};
}

// Wispr Flow's own auxiliary windows that are never meant to be taskbar/
// Alt-Tab entries (the dictation status pill, the in-app context menu, ...).
// Electron creates each of these with `skipTaskbar: true`, but that hint has
// no effect under native Wayland: Electron's Ozone/Wayland backend talks raw
// xdg-shell and never creates a GtkWindow, so the gtk_shell1 protocol that
// carries the skip-taskbar request to Mutter is never spoken. Mutter is left
// to assume every xdg_toplevel is an ordinary app window, so the transparent,
// click-through status pill shows up as a second, empty-looking window in the
// taskbar/Overview/Alt-Tab (issue: the pill is "always open" from the user's
// perspective). We run inside gnome-shell's own process, so we can correct
// Mutter's classification directly via Meta.Window, independent of whatever
// protocol the client did or didn't speak. The Hub (titled `Hub` before
// 1.6.1264, `Wispr Flow` since) and `Scratchpad` are the only Wispr Flow
// windows meant to be real, user-facing taskbar entries.
const WISPR_WM_CLASS = 'wispr-flow';
const WISPR_TASKBAR_TITLES = new Set(['Hub', 'Wispr Flow', 'Scratchpad']);

// Mutter refuses focus to a newly mapped window that is mostly (60%+)
// covered by "above" windows and stacks it under the focused one
// (window_would_mostly_be_covered_by_always_above_window in
// src/core/window.c). The status window is mostly transparent canvas around
// the pill (512x614 in 1.6.1209 and 1.6.1264, bottom-centred), so raising it
// at that size takes focus from anything small that maps there, including
// wl-clipboard's surface, which then waits for focus forever and hangs copy
// and paste. Keep the pill above only while its window is no bigger than
// this; linux-status-compact.sh in the app repo makes it 200x110.
const STATUS_ABOVE_MAX_WIDTH = 256;
const STATUS_ABOVE_MAX_HEIGHT = 160;

function isStatusTitle(title) {
    return title === 'Status';
}

export default class WindowBridgeExtension extends Extension {
    enable() {
        // Wispr windows we changed -> their state before we touched them, and
        // every signal we connected on a window, so disable() can put back
        // exactly what Mutter had and leave nothing running behind it.
        this._auxSaved = new Map();
        this._winSignals = new Map();
        this._aboveWatched = new Set();
        this._idleIds = new Set();

        this._dbusImpl = Gio.DBusExportedObject.wrapJSObject(IFACE_XML, this);
        this._dbusImpl.export(Gio.DBus.session, OBJECT_PATH);
        this._ownerId = Gio.bus_own_name_on_connection(
            Gio.DBus.session,
            BUS_NAME,
            Gio.BusNameOwnerFlags.REPLACE,
            null,
            null
        );

        // Track the focused window so we can also fire FocusChanged on title
        // changes of the currently-focused window (mirrors macOS/Windows which
        // surface title updates as focus updates).
        this._focusWindow = null;
        this._titleId = 0;

        this._focusChangedId = global.display.connect(
            'notify::focus-window',
            () => this._onFocusChanged()
        );

        // Seed the title-watch on whatever already has focus.
        this._onFocusChanged();

        // Hide Wispr Flow's non-taskbar windows from the taskbar/Alt-Tab/
        // Overview. `map` (not `window-created`) because wm_class/title are
        // not reliably populated yet at creation time -- they're set by the
        // point the compositor maps the surface.
        this._mapId = global.window_manager.connect('map', (_wm, actor) =>
            this._hideIfAuxiliary(actor.meta_window)
        );
        // Catch anything already mapped before we were enabled (e.g. this
        // extension re-enabling mid-session after a GNOME Shell restart).
        global.get_window_actors().forEach(a =>
            this._hideIfAuxiliary(a.meta_window)
        );
    }

    // Excludes win from the taskbar/Alt-Tab/Overview switcher if it is one of
    // Wispr Flow's auxiliary windows, and corrects its window type so it
    // actually behaves like the overlay it's meant to be. Idempotent and safe
    // to call repeatedly (each of these calls is a no-op on a window already
    // in the target state, and the pre-change state is recorded only once),
    // so the `map` handler, the title watch and the enable()-time sweep can
    // all call it unconditionally.
    _hideIfAuxiliary(win) {
        if (!win)
            return;
        try {
            if (win.get_wm_class() !== WISPR_WM_CLASS)
                return;
            // The title (and possibly other window properties) may not be
            // populated yet at `map` time -- Electron sets the OS-level
            // window title from the loaded page's <title>, which can lag the
            // surface actually mapping. Retry once the title lands instead of
            // silently mis-classifying a not-yet-titled window as one of the
            // real taskbar windows (empty string is never in
            // WISPR_TASKBAR_TITLES, so an untitled window is treated as
            // auxiliary until its title says otherwise, and is restored then).
            if (!this._winSignals.has(win)) {
                this._connectWin(win, 'notify::title', () =>
                    this._hideIfAuxiliary(win)
                );
                this._connectWin(win, 'unmanaged', () =>
                    this._forgetWindow(win)
                );
            }
            if (WISPR_TASKBAR_TITLES.has(win.get_title() ?? '')) {
                this._restoreWindow(win);
                return;
            }
            if (!this._auxSaved.has(win)) {
                this._auxSaved.set(win, {
                    type: win.get_window_type(),
                    above: win.is_above(),
                    sticky: win.is_on_all_workspaces(),
                    skipTaskbar: win.is_skip_taskbar(),
                });
            }
            win.hide_from_window_list();
            // `hide_from_window_list()` only affects Alt-Tab/taskbar-style
            // consumers that check `is_skip_taskbar()`; GNOME Shell's own
            // Overview window grid additionally reads the `skip-taskbar`
            // GObject property directly. Set both explicitly rather than
            // relying on one to imply the other. Separately guarded: some
            // Mutter versions expose this as constructor-only, in which case
            // assignment throws and we fall through to the type/stacking
            // fixes below regardless.
            try {
                win.skip_taskbar = true;
            } catch (e) {
                // Read-only in this Mutter version; hide_from_window_list()
                // above is the fallback lever.
            }
            // Wispr Flow's overlay windows are already asked (from the
            // Electron side) to appear on every workspace; Wayland ignores
            // that request for the same reason it ignores skip-taskbar, so
            // mirror it here too rather than leaving the pill workspace-
            // bound.
            win.stick();
            // Electron's `type: "toolbar"` BrowserWindow option is X11-only
            // and does nothing on Wayland, so Mutter classifies the pill as
            // an ordinary Meta.WindowType.NORMAL toplevel. A NORMAL window's
            // always-on-top state is still subject to Mutter's normal
            // focus/stacking rules, so the "always on top" status pill can
            // end up rendered *behind* whatever window has focus -- from the
            // user's perspective, the push-to-talk indicator never appears.
            // UTILITY windows are stacked/raised independently of focus
            // changes to the main application, matching what this window
            // actually is.
            win.set_type(Meta.WindowType.UTILITY);
            // Only the status pill is kept above other windows. The other
            // auxiliary windows (the work-area-sized context menu overlay)
            // would take focus from every new window under them, and a
            // misclassified main window would sit on top of everything.
            if (isStatusTitle(win.get_title()) &&
                !this._aboveWatched.has(win)) {
                this._aboveWatched.add(win);
                this._connectWin(win, 'size-changed', () =>
                    this._updateStatusAbove(win)
                );
                // Deferred: an above state set while the window is still
                // mapping does not stick.
                const id = GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
                    this._idleIds.delete(id);
                    this._updateStatusAbove(win);
                    return GLib.SOURCE_REMOVE;
                });
                this._idleIds.add(id);
            }
        } catch (e) {
            // Window can vanish mid-call (closed while we're inspecting it);
            // nothing to clean up either way.
        }
    }

    // Keep the status pill above other windows while its window is small
    // enough not to trip Mutter's focus refusal (see STATUS_ABOVE_MAX_WIDTH),
    // and drop it back to the normal layer while it is not. Without the
    // compact-window patch the pill can then sit behind the focused window,
    // which is the lesser problem.
    _updateStatusAbove(win) {
        try {
            const saved = this._auxSaved.get(win);
            // Gone, retitled as a real window, or above on its own already.
            if (!saved || saved.above)
                return;
            const {width, height} = win.get_frame_rect();
            const small = width <= STATUS_ABOVE_MAX_WIDTH &&
                height <= STATUS_ABOVE_MAX_HEIGHT;
            if (small && !win.is_above())
                win.make_above();
            else if (!small && win.is_above())
                win.unmake_above();
        } catch (e) {
            // Window can vanish mid-call; nothing to clean up either way.
        }
    }

    // Undo everything _hideIfAuxiliary changed on win, back to the state
    // recorded before the first change.
    _restoreWindow(win) {
        const saved = this._auxSaved.get(win);
        if (!saved)
            return;
        this._auxSaved.delete(win);
        try {
            if (!saved.above && win.is_above())
                win.unmake_above();
            if (!saved.sticky && win.is_on_all_workspaces())
                win.unstick();
            win.set_type(saved.type);
            if (!saved.skipTaskbar)
                win.show_in_window_list();
        } catch (e) {
            // Window destroyed since we recorded it.
        }
    }

    _connectWin(win, name, fn) {
        let ids = this._winSignals.get(win);
        if (!ids) {
            ids = [];
            this._winSignals.set(win, ids);
        }
        ids.push(win.connect(name, fn));
    }

    _disconnectWin(win) {
        for (const id of this._winSignals.get(win) ?? []) {
            try {
                win.disconnect(id);
            } catch (e) {}
        }
        this._winSignals.delete(win);
    }

    _forgetWindow(win) {
        this._disconnectWin(win);
        this._auxSaved.delete(win);
        this._aboveWatched.delete(win);
    }

    disable() {
        if (this._mapId) {
            global.window_manager.disconnect(this._mapId);
            this._mapId = 0;
        }
        if (this._focusChangedId) {
            global.display.disconnect(this._focusChangedId);
            this._focusChangedId = 0;
        }
        for (const id of this._idleIds ?? [])
            GLib.source_remove(id);
        this._idleIds?.clear();
        // Lower, unstick and re-list every window we changed, so turning the
        // extension off undoes it without restarting Wispr Flow.
        for (const w of [...(this._auxSaved?.keys() ?? [])])
            this._restoreWindow(w);
        for (const w of [...(this._winSignals?.keys() ?? [])])
            this._disconnectWin(w);
        this._aboveWatched?.clear();
        this._disconnectTitle();
        this._focusWindow = null;

        if (this._ownerId) {
            Gio.bus_unown_name(this._ownerId);
            this._ownerId = 0;
        }
        if (this._dbusImpl) {
            this._dbusImpl.unexport();
            this._dbusImpl = null;
        }
    }

    _disconnectTitle() {
        if (this._titleId && this._focusWindow) {
            try {
                this._focusWindow.disconnect(this._titleId);
            } catch (e) {}
        }
        this._titleId = 0;
    }

    _onFocusChanged() {
        const win = global.display.get_focus_window();

        // Re-arm the title watcher on the newly focused window.
        if (win !== this._focusWindow) {
            this._disconnectTitle();
            this._focusWindow = win;
            if (win) {
                this._titleId = win.connect('notify::title', () =>
                    this._emitFocus()
                );
            }
        }

        this._emitFocus();
    }

    _emitFocus() {
        if (!this._dbusImpl)
            return;
        const json = JSON.stringify(windowInfo(focusedOrMru()));
        this._dbusImpl.emit_signal(
            'FocusChanged',
            new GLib.Variant('(s)', [json])
        );
    }

    // D-Bus method: focused window identity as JSON (MRU fallback when none).
    GetFocusedWindow() {
        return JSON.stringify(windowInfo(focusedOrMru()));
    }

    // D-Bus method: all normal, taskbar-visible windows as a JSON array. The
    // skip_taskbar filter drops docks/OSDs and — importantly — the helper's own
    // wl-copy clipboard surfaces (which map as titled "wl-clipboard" windows when
    // the in-process ext-data-control path is unavailable, e.g. on mutter), so
    // they don't pollute GetRunningApps. Matches the KWin script's filter.
    GetWindowList() {
        const out = [];
        for (const actor of global.get_window_actors()) {
            const win = actor.meta_window;
            if (!win)
                continue;
            if (win.window_type !== Meta.WindowType.NORMAL)
                continue;
            if (win.is_skip_taskbar())
                continue;
            const info = windowInfo(win);
            // Drop windows with no real application identity: the Shell assigns a
            // synthetic "window:N" app id and an empty wmClass to surfaces with no
            // .desktop file — e.g. the helper's own wl-copy clipboard surfaces on
            // mutter (which has no ext-data-control). Real apps set a wmClass.
            if (info.wmClass === '' && info.appId.startsWith('window:'))
                continue;
            out.push(info);
        }
        return JSON.stringify(out);
    }
}
