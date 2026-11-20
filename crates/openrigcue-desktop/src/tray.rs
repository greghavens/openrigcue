//! The launcher's Open + Quit tray, as a StatusNotifierItem with a DBusMenu.
//! The session-bus worker only sends commands; it never touches GTK objects.

use std::collections::HashMap;
use std::sync::Arc;
use zbus::zvariant::{ObjectPath, OwnedValue, Str, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    Open,
    Quit,
    Menu { x: i32, y: i32 },
}
pub type Action = Arc<dyn Fn(TrayAction) + Send + Sync>;
type Properties = HashMap<String, OwnedValue>;
type Layout = (i32, Properties, Vec<OwnedValue>);
type Pixmaps = Vec<(i32, i32, Vec<u8>)>;

pub struct Tray {
    _connection: zbus::blocking::Connection,
}
impl Tray {
    pub fn start(action: Action) -> zbus::Result<Self> {
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let connection = zbus::blocking::connection::Builder::session()?
            .name(name.as_str())?
            .serve_at("/StatusNotifierItem", Item { action: action.clone() })?
            .serve_at("/Menu", Menu { action })?
            .build()?;
        let watcher = zbus::blocking::Proxy::new(
            &connection,
            "org.kde.StatusNotifierWatcher",
            "/StatusNotifierWatcher",
            "org.kde.StatusNotifierWatcher",
        )?;
        watcher.call::<_, _, ()>("RegisterStatusNotifierItem", &(name.as_str(),))?;
        Ok(Self { _connection: connection })
    }
}

struct Item {
    action: Action,
}
#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "openrigcue"
    }
    #[zbus(property)]
    fn title(&self) -> &str {
        "OpenRigCue"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }
    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        "openrigcue"
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        icon()
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }
    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }
    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        ("openrigcue".into(), icon(), "OpenRigCue".into(), String::new())
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked("/Menu")
    }
    fn activate(&self, _x: i32, _y: i32) {
        (self.action)(TrayAction::Open);
    }
    fn secondary_activate(&self, _x: i32, _y: i32) {
        (self.action)(TrayAction::Open);
    }
    // DBusMenu hosts display /Menu themselves. Other hosts request our GTK menu.
    fn context_menu(&self, x: i32, y: i32) {
        (self.action)(TrayAction::Menu { x, y });
    }
    fn scroll(&self, _delta: i32, _orientation: &str) {}
}

struct Menu {
    action: Action,
}
fn properties(id: i32, requested: &[String]) -> Option<Properties> {
    let mut props = match id {
        0 => Properties::from([("children-display".into(), OwnedValue::from(Str::from("submenu")))]),
        1 | 2 => Properties::from([
            ("label".into(), OwnedValue::from(Str::from(if id == 1 { "Open" } else { "Quit" }))),
            ("enabled".into(), OwnedValue::from(true)),
            ("visible".into(), OwnedValue::from(true)),
        ]),
        _ => return None,
    };
    if !requested.is_empty() {
        props.retain(|key, _| requested.contains(key));
    }
    Some(props)
}
fn layout(id: i32, depth: i32, names: &[String]) -> zbus::fdo::Result<Layout> {
    let props = properties(id, names).ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown menu item".into()))?;
    let children = if id == 0 && depth != 0 {
        [1, 2]
            .into_iter()
            .map(|id| {
                let child = layout(id, if depth < 0 { depth } else { depth - 1 }, names)?;
                OwnedValue::try_from(Value::from(child)).map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
            })
            .collect::<zbus::fdo::Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok((id, props, children))
}
impl Menu {
    fn dispatch(&self, id: i32, event: &str) -> bool {
        if !(0..=2).contains(&id) {
            return false;
        }
        if event == "clicked" {
            match id {
                1 => (self.action)(TrayAction::Open),
                2 => (self.action)(TrayAction::Quit),
                _ => {}
            }
        }
        true
    }
}
#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(out_args("revision", "layout"))]
    fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        Ok((1, layout(parent_id, recursion_depth, &property_names)?))
    }
    fn get_group_properties(&self, ids: Vec<i32>, property_names: Vec<String>) -> Vec<(i32, Properties)> {
        let ids = if ids.is_empty() { vec![0, 1, 2] } else { ids };
        ids.into_iter().filter_map(|id| properties(id, &property_names).map(|props| (id, props))).collect()
    }
    fn get_property(&self, id: i32, name: &str) -> zbus::fdo::Result<OwnedValue> {
        properties(id, &[])
            .and_then(|mut props| props.remove(name))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("unknown menu property".into()))
    }
    fn event(&self, id: i32, event_id: &str, _data: OwnedValue, _timestamp: u32) -> zbus::fdo::Result<()> {
        if self.dispatch(id, event_id) {
            Ok(())
        } else {
            Err(zbus::fdo::Error::InvalidArgs("unknown menu item".into()))
        }
    }
    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        events.into_iter().filter_map(|(id, event, _, _)| (!self.dispatch(id, &event)).then_some(id)).collect()
    }
    fn about_to_show(&self, id: i32) -> zbus::fdo::Result<bool> {
        if (0..=2).contains(&id) {
            Ok(false)
        } else {
            Err(zbus::fdo::Error::InvalidArgs("unknown menu item".into()))
        }
    }
    #[zbus(out_args("updates_needed", "id_errors"))]
    fn about_to_show_group(&self, ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), ids.into_iter().filter(|id| !(0..=2).contains(id)).collect())
    }
}

/// The canonical app mark, rendered at 32px and stored in StatusNotifierItem's
/// network-order ARGB format so the tray needs no host icon theme or decoder.
fn icon() -> Pixmaps {
    let bytes = include_bytes!("../../../packaging/appimage/openrigcue-tray.argb");
    vec![(32, 32, bytes.to_vec())]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_has_open_and_quit_with_the_dbusmenu_wire_shape() {
        use zbus::zvariant::Type;
        assert_eq!(Layout::SIGNATURE.to_string(), "(ia{sv}av)");
        let (_, root) = Menu { action: Arc::new(|_| {}) }.get_layout(0, -1, Vec::new()).unwrap();
        assert_eq!(root.2.len(), 2);
        let data =
            zbus::zvariant::to_bytes(zbus::zvariant::serialized::Context::new_dbus(zbus::zvariant::LE, 0), &root)
                .unwrap();
        let decoded: Layout = data.deserialize().unwrap().0;
        assert_eq!(decoded.0, 0);
        assert_eq!(layout(0, 0, &[]).unwrap().2.len(), 0);
        assert!(layout(9, -1, &[]).is_err());
        assert_eq!(properties(1, &["label".into()]).unwrap().len(), 1);
    }
    #[test]
    fn only_clicked_actions_reach_the_launcher() {
        let (tx, rx) = std::sync::mpsc::channel();
        let menu = Menu {
            action: Arc::new(move |action| {
                tx.send(action).unwrap();
            }),
        };
        assert!(menu.dispatch(1, "opened"));
        assert!(!menu.dispatch(3, "clicked"));
        assert!(menu.dispatch(1, "clicked"));
        assert!(menu.dispatch(2, "clicked"));
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![TrayAction::Open, TrayAction::Quit]);
        let (_, _, bytes) = icon().pop().unwrap();
        assert_eq!(bytes.len(), 32 * 32 * 4);
        assert_eq!(&bytes[0..4], &[0, 0, 0, 0]);
    }
}
