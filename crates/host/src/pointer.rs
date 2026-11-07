//! The pointer from the TabExtend GNOME Shell extension, mapped onto this
//! tablet's virtual monitor.
//!
//! GNOME sends a virtual monitor's pointer to screen casts only with frames
//! that repaint something, not when the pointer just moves (seen on GNOME 50),
//! so the stream's own pointer metadata cannot be followed. The extension in
//! `gnome-extension/` reports the pointer in the desktop's logical
//! coordinates; Mutter's DisplayConfig says where this monitor is. Without
//! the extension, the stream's metadata is used as before.

use std::collections::{HashMap, HashSet};

use futures_util::StreamExt;
use tokio::sync::watch;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, Proxy};

use crate::pipeline::{CursorPosition, cursor_on_stream};

const EXTENSION_BUS: &str = "io.github.jwsung91.TabExtendPointer";
const EXTENSION_PATH: &str = "/io/github/jwsung91/TabExtendPointer";
const EXTENSION_INTERFACE: &str = "io.github.jwsung91.TabExtendPointer";
const DISPLAY_CONFIG_BUS: &str = "org.gnome.Mutter.DisplayConfig";
const DISPLAY_CONFIG_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

type MonitorSpec = (String, String, String, String);
type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Monitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
type CurrentState = (
    u32,
    Vec<Monitor>,
    Vec<LogicalMonitor>,
    HashMap<String, OwnedValue>,
);

/// A monitor's place on the desktop, in logical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Where a pointer at desktop position (`x`, `y`) lands on a stream showing
/// `monitor`, or None if it is hidden or on another monitor.
pub fn pointer_on_stream(
    (x, y, visible): (f64, f64, bool),
    monitor: Rect,
    stream_size: (u32, u32),
) -> CursorPosition {
    if !visible {
        return None;
    }
    let (dx, dy) = (x - monitor.x, y - monitor.y);
    if dx < 0.0 || dy < 0.0 {
        return None;
    }
    cursor_on_stream(
        (dx.floor() as i32, dy.floor() as i32),
        (monitor.width.round() as u32, monitor.height.round() as u32),
        stream_size,
    )
}

/// Each connector's logical rectangle, from DisplayConfig's current state.
fn logical_rects(state: &CurrentState) -> HashMap<String, Rect> {
    let (_, monitors, logical_monitors, properties) = state;
    // 1: logical layout (sizes divided by the scale), 2: physical.
    let physical = properties
        .get("layout-mode")
        .and_then(|v| u32::try_from(v).ok())
        == Some(2);
    let current_mode = |connector: &str| {
        monitors
            .iter()
            .find(|((c, ..), ..)| c == connector)
            .and_then(|(_, modes, _)| {
                modes.iter().find(|(.., props)| {
                    props
                        .get("is-current")
                        .and_then(|v| bool::try_from(v).ok())
                        .unwrap_or(false)
                })
            })
            .map(|(_, w, h, ..)| (f64::from(*w), f64::from(*h)))
    };
    let mut rects = HashMap::new();
    for (x, y, scale, transform, _, specs, _) in logical_monitors {
        let Some((connector, ..)) = specs.first() else {
            continue;
        };
        let Some((mut w, mut h)) = current_mode(connector) else {
            continue;
        };
        // Odd transforms rotate by 90 or 270 degrees.
        if transform % 2 == 1 {
            std::mem::swap(&mut w, &mut h);
        }
        if !physical && *scale > 0.0 {
            w = (w / scale).round();
            h = (h / scale).round();
        }
        for (connector, ..) in specs {
            rects.insert(
                connector.clone(),
                Rect {
                    x: f64::from(*x),
                    y: f64::from(*y),
                    width: w,
                    height: h,
                },
            );
        }
    }
    rects
}

async fn display_config(connection: &Connection) -> zbus::Result<Proxy<'static>> {
    Proxy::new(
        connection,
        DISPLAY_CONFIG_BUS,
        DISPLAY_CONFIG_PATH,
        DISPLAY_CONFIG_BUS,
    )
    .await
}

async fn current_rects(proxy: &Proxy<'_>) -> zbus::Result<HashMap<String, Rect>> {
    let state: CurrentState = proxy.call("GetCurrentState", &()).await?;
    Ok(logical_rects(&state))
}

/// The connectors on the desktop now. Taken before the virtual monitor is
/// added, to tell it apart from the others afterwards.
pub async fn connectors() -> HashSet<String> {
    let result = async {
        let connection = Connection::session().await?;
        let proxy = display_config(&connection).await?;
        current_rects(&proxy).await
    }
    .await;
    result.map(|r| r.into_keys().collect()).unwrap_or_default()
}

/// Follows the pointer while the virtual monitor exists.
pub struct PointerWatch {
    positions: watch::Receiver<CursorPosition>,
    task: tokio::task::JoinHandle<()>,
}

impl PointerWatch {
    /// Start following the pointer on the monitor added since `before` was
    /// taken (GNOME may add it later, once the stream flows). None if the
    /// extension is not running; the reason goes to stderr.
    pub async fn start(before: HashSet<String>, stream_size: (u32, u32)) -> Option<Self> {
        match Self::try_start(before, stream_size).await {
            Ok(watch) => Some(watch),
            Err(reason) => {
                eprintln!("note: pointer from the stream only ({reason})");
                None
            }
        }
    }

    async fn try_start(before: HashSet<String>, stream_size: (u32, u32)) -> Result<Self, String> {
        let connection = Connection::session().await.map_err(|e| e.to_string())?;
        let dbus = zbus::fdo::DBusProxy::new(&connection)
            .await
            .map_err(|e| e.to_string())?;
        let running = dbus
            .name_has_owner(EXTENSION_BUS.try_into().map_err(|e| format!("{e}"))?)
            .await
            .map_err(|e| e.to_string())?;
        if !running {
            return Err("the TabExtend pointer extension is not running".into());
        }
        let config = display_config(&connection)
            .await
            .map_err(|e| e.to_string())?;
        let extension = Proxy::new(
            &connection,
            EXTENSION_BUS,
            EXTENSION_PATH,
            EXTENSION_INTERFACE,
        )
        .await
        .map_err(|e| e.to_string())?;
        let mut pointer_moves = extension
            .receive_signal("PointerChanged")
            .await
            .map_err(|e| e.to_string())?;
        let mut monitor_changes = config
            .receive_signal("MonitorsChanged")
            .await
            .map_err(|e| e.to_string())?;
        // GNOME adds the virtual monitor once the stream starts flowing,
        // after this returns: look for it now and on every monitor change.
        let mut connector = None;
        let mut monitor = None;
        find_monitor(&config, &before, &mut connector, &mut monitor).await;
        let mut pointer: (f64, f64, bool) = extension
            .call("GetPointer", &())
            .await
            .map_err(|e| e.to_string())?;

        let position = move |pointer, monitor: Option<Rect>| {
            monitor.and_then(|m| pointer_on_stream(pointer, m, stream_size))
        };
        let (tx, positions) = watch::channel(position(pointer, monitor));
        let task = tokio::spawn(async move {
            // Kept alive for the signal streams.
            let _connection = connection;
            loop {
                tokio::select! {
                    Some(message) = pointer_moves.next() => {
                        match message.body().deserialize::<(f64, f64, bool)>() {
                            Ok(p) => pointer = p,
                            Err(_) => continue,
                        }
                    }
                    Some(_) = monitor_changes.next() => {
                        // It appeared, or the user moved it in Settings.
                        find_monitor(&config, &before, &mut connector, &mut monitor).await;
                    }
                    else => break,
                }
                tx.send_if_modified(|current| {
                    let next = position(pointer, monitor);
                    let changed = *current != next;
                    *current = next;
                    changed
                });
            }
        });
        Ok(Self { positions, task })
    }

    pub fn positions(&self) -> watch::Receiver<CursorPosition> {
        self.positions.clone()
    }
}

impl Drop for PointerWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Find this tablet's monitor: the one connector that appeared since
/// `before`, remembered once found. Updates its rectangle, or clears it if
/// the monitor is gone.
async fn find_monitor(
    config: &Proxy<'_>,
    before: &HashSet<String>,
    connector: &mut Option<String>,
    monitor: &mut Option<Rect>,
) {
    let Ok(rects) = current_rects(config).await else {
        return;
    };
    if connector.is_none() {
        let new: Vec<&String> = rects.keys().filter(|c| !before.contains(*c)).collect();
        match new.as_slice() {
            [one] => *connector = Some((*one).clone()),
            [] => {}
            _ => eprintln!("note: several monitors appeared at once; pointer not shown"),
        }
    }
    *monitor = connector.as_ref().and_then(|c| rects.get(c).copied());
}

#[cfg(test)]
mod tests {
    use super::{Rect, pointer_on_stream};

    const TABLET: Rect = Rect {
        x: 1728.0,
        y: 0.0,
        width: 1920.0,
        height: 1200.0,
    };

    #[test]
    fn on_the_monitor_is_a_fraction_of_the_stream() {
        assert_eq!(
            pointer_on_stream((1728.0 + 960.0, 300.0, true), TABLET, (1920, 1200)),
            Some((0.5, 0.25))
        );
    }

    #[test]
    fn elsewhere_or_hidden_is_none() {
        assert_eq!(
            pointer_on_stream((100.0, 100.0, true), TABLET, (1920, 1200)),
            None
        );
        assert_eq!(
            pointer_on_stream((1728.0 + 1920.0, 10.0, true), TABLET, (1920, 1200)),
            None
        );
        assert_eq!(
            pointer_on_stream((2000.0, 100.0, false), TABLET, (1920, 1200)),
            None
        );
    }
}
