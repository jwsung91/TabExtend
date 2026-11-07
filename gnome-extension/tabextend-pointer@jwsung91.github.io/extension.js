// Tells TabExtend where the pointer is.
//
// GNOME sends a virtual monitor's pointer to screen casts only with frames
// that repaint something, not when the pointer just moves (seen on GNOME 50),
// so the tablet cannot follow the pointer from the stream alone. This
// extension reports the pointer's position in the desktop's logical
// coordinates over the session bus; TabExtend maps it onto its own monitor.
//
// Any program on the session bus can read the pointer position through this,
// as X11 programs always could.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.github.jwsung91.TabExtendPointer';
const OBJECT_PATH = '/io/github/jwsung91/TabExtendPointer';
const IFACE = `
<node>
  <interface name="io.github.jwsung91.TabExtendPointer">
    <method name="GetPointer">
      <arg type="d" direction="out" name="x"/>
      <arg type="d" direction="out" name="y"/>
      <arg type="b" direction="out" name="visible"/>
    </method>
    <signal name="PointerChanged">
      <arg type="d" name="x"/>
      <arg type="d" name="y"/>
      <arg type="b" name="visible"/>
    </signal>
  </interface>
</node>`;

// At most this often; a pointer move can be reported a thousand times a second.
const MIN_INTERVAL_MS = 8;

export default class TabExtendPointer extends Extension {
    enable() {
        this._tracker = global.backend.get_cursor_tracker();
        this._lastSentUs = 0;
        this._timeoutId = 0;

        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._dbus.export(Gio.DBus.session, OBJECT_PATH);
        this._nameId = Gio.bus_own_name_on_connection(
            Gio.DBus.session, BUS_NAME, Gio.BusNameOwnerFlags.NONE, null, null);

        this._signals = [
            this._tracker.connect('position-invalidated', () => this._queue()),
            this._tracker.connect('visibility-changed', () => this._queue()),
        ];
    }

    disable() {
        for (const id of this._signals ?? [])
            this._tracker.disconnect(id);
        this._signals = null;
        if (this._timeoutId) {
            GLib.source_remove(this._timeoutId);
            this._timeoutId = 0;
        }
        if (this._nameId) {
            Gio.bus_unown_name(this._nameId);
            this._nameId = 0;
        }
        this._dbus?.unexport();
        this._dbus = null;
        this._tracker = null;
    }

    GetPointer() {
        return this._state();
    }

    _state() {
        const [x, y] = global.get_pointer();
        return [x, y, this._tracker.get_pointer_visible()];
    }

    _queue() {
        if (this._timeoutId)
            return;
        const waitMs = MIN_INTERVAL_MS - (GLib.get_monotonic_time() - this._lastSentUs) / 1000;
        if (waitMs <= 0) {
            this._send();
            return;
        }
        this._timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, Math.ceil(waitMs), () => {
            this._timeoutId = 0;
            this._send();
            return GLib.SOURCE_REMOVE;
        });
    }

    _send() {
        this._lastSentUs = GLib.get_monotonic_time();
        this._dbus?.emit_signal('PointerChanged', new GLib.Variant('(ddb)', this._state()));
    }
}
