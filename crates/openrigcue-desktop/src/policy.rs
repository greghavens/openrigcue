use openrigcue_cueing::{str2bool, CueError, PyValue};
use openrigcue_model::native_value::{Dict, LegacyValue};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{class_name}: {message}")]
pub struct WindowError {
    pub class_name: &'static str,
    pub message: String,
}
impl From<CueError> for WindowError {
    fn from(error: CueError) -> Self {
        Self { class_name: error.exception().name(), message: error.message().into() }
    }
}
impl From<String> for WindowError {
    fn from(error: String) -> Self {
        let (class, message) = error.split_once(": ").unwrap_or((&error, ""));
        Self {
            class_name: match class {
                "TypeError" => "TypeError",
                "OverflowError" => "OverflowError",
                "ZeroDivisionError" => "ZeroDivisionError",
                "MemoryError" => "MemoryError",
                "KeyError" => "KeyError",
                _ => "ValueError",
            },
            message: message.to_owned(),
        }
    }
}
#[derive(Debug, Clone, Copy, serde::Deserialize, PartialEq, Eq)]
pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}
impl Monitor {
    fn contains_native(
        &self,
        x: &LegacyValue,
        y: &LegacyValue,
        width: &LegacyValue,
        height: &LegacyValue,
    ) -> Result<bool, WindowError> {
        Ok(x.py_gt(&LegacyValue::Int(i64::from(self.x)))?
            && y.py_gt(&LegacyValue::Int(i64::from(self.y)))?
            && LegacyValue::Int(i64::from(self.x) + i64::from(self.width)).py_gt(&x.py_add(width)?)?
            && LegacyValue::Int(i64::from(self.y) + i64::from(self.height)).py_gt(&y.py_add(height)?)?)
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct WindowPlan {
    pub width: i128,
    pub height: i128,
    pub x: i128,
    pub y: i128,
    pub min_width: i32,
    pub min_height: i32,
    pub connect_closed: bool,
    pub monitors: Dict,
}
#[derive(Debug, Clone, PartialEq)]
pub struct NativeWindowPlan {
    pub width: LegacyValue,
    pub height: LegacyValue,
    pub x: LegacyValue,
    pub y: LegacyValue,
    pub min_width: i32,
    pub min_height: i32,
    pub connect_closed: bool,
    pub monitors: Dict,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    Quit,
    Minimize,
}
impl CloseAction {
    pub fn from_config(config: &Dict) -> Result<Self, WindowError> {
        let config = PyValue::from(&LegacyValue::Dict(config.clone()));
        Ok(if str2bool(&config.item_owned("APP")?.item_owned("x_button_exit")?)? { Self::Quit } else { Self::Minimize })
    }
}
impl WindowPlan {
    /// Every invalid/missing start_minimized lookup reads as false.
    pub fn starts_minimized(config: &Dict) -> bool {
        let config = PyValue::from(&LegacyValue::Dict(config.clone()));
        config
            .item_owned("APP")
            .and_then(|app| app.item_owned("start_minimized"))
            .and_then(|value| str2bool(&value))
            .unwrap_or(false)
    }
}

/// The window placement policy, before the OS webview is created. Coordinates/rectangles
/// remain integers and each monitor edge is a strict comparison.
pub fn window_plan(
    config: &Dict,
    monitors: &[Option<Monitor>],
    dpi: f64,
    screen_width: i32,
    screen_height: i32,
) -> Result<WindowPlan, WindowError> {
    let plan = native_window_plan(config, monitors, dpi, screen_width, screen_height)?;
    let bounded = |value: LegacyValue| -> Result<i128, WindowError> {
        i128::try_from(value.py_integer()?)
            .map_err(|_| WindowError { class_name: "OverflowError", message: "integer result beyond 128 bits".into() })
    };
    Ok(WindowPlan {
        width: bounded(plan.width)?,
        height: bounded(plan.height)?,
        x: bounded(plan.x)?,
        y: bounded(plan.y)?,
        min_width: plan.min_width,
        min_height: plan.min_height,
        connect_closed: plan.connect_closed,
        monitors: plan.monitors,
    })
}

/// Geometry remains an unbounded integer through placement and monitor arithmetic;
/// the desktop backend checks its own integer range when creating the window.
pub fn native_window_plan(
    config: &Dict,
    monitors: &[Option<Monitor>],
    dpi: f64,
    screen_width: i32,
    screen_height: i32,
) -> Result<NativeWindowPlan, WindowError> {
    let config = PyValue::from(&LegacyValue::Dict(config.clone()));
    let manual = config
        .item_owned("APP")
        .and_then(|app| app.item_owned("force_manual_quit"))
        .and_then(|value| str2bool(&value))
        .unwrap_or(false);
    let app = config.item_owned("APP")?;
    let integer = |key| -> Result<LegacyValue, CueError> { Ok(app.item_owned(key)?.to_integer_value()?.to_native()) };
    let mut width = integer("window_width")?;
    let mut height = integer("window_height")?;
    use openrigcue_model::python_compare::native_equal;
    if native_equal(&width, &LegacyValue::Int(1280))
        && native_equal(&height, &LegacyValue::Int(800))
        && app.item_owned("pixel_ratio")?.to_float()? == 1.0
    {
        width = LegacyValue::from_integer(LegacyValue::Float(1280.0 * (dpi * 0.98) / 100.0).py_integer()?);
        height = LegacyValue::from_integer(LegacyValue::Float(800.0 * (dpi * 0.8) / 100.0).py_integer()?);
    }
    let (mut x, mut y) = (|| -> Result<_, CueError> { Ok((integer("window_x_pos")?, integer("window_y_pos")?)) })()
        .unwrap_or((LegacyValue::Int(-1), LegacyValue::Int(-1)));
    let fingerprint = monitor_fingerprint(monitors);
    let current = PyValue::from(&LegacyValue::Dict(fingerprint.clone()));
    let mut contained = false;
    if config.item_owned("MONITORS")?.equals(&current) {
        for monitor in monitors.iter().flatten() {
            if monitor.contains_native(&x, &y, &width, &height)? {
                contained = true;
                break;
            }
        }
    }
    if !contained {
        x = LegacyValue::Int(-1);
        y = LegacyValue::Int(-1);
    }
    let centered = |screen: i32, extent: &LegacyValue| -> Result<LegacyValue, WindowError> {
        let half_screen = LegacyValue::Int(i64::from(screen)).py_truediv(&LegacyValue::Int(2))?;
        let half_extent = extent.py_truediv(&LegacyValue::Int(2))?;
        Ok(LegacyValue::from_integer(half_screen.py_sub(&half_extent)?.py_integer()?))
    };
    if native_equal(&x, &LegacyValue::Int(-1)) {
        x = centered(screen_width, &width)?;
    }
    if native_equal(&y, &LegacyValue::Int(-1)) {
        y = centered(screen_height, &height)?;
    }
    Ok(NativeWindowPlan {
        width,
        height,
        x,
        y,
        min_width: 450,
        min_height: 700,
        connect_closed: !manual,
        monitors: fingerprint,
    })
}

pub(crate) fn monitor_fingerprint(monitors: &[Option<Monitor>]) -> Dict {
    let mut fingerprint = Dict::new();
    for (index, monitor) in monitors.iter().flatten().enumerate() {
        let sum = i64::from(monitor.x) + i64::from(monitor.y) + i64::from(monitor.width) + i64::from(monitor.height);
        fingerprint.insert(index.to_string(), LegacyValue::Str(sum.to_string()));
    }
    fingerprint
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_geometry_retains_wide_integer_extent_until_the_desktop_boundary() {
        let mut app = Dict::new();
        let text = format!("1{}", "0".repeat(80));
        let width = LegacyValue::from_integer(LegacyValue::from(text.clone()).py_integer().unwrap());
        app.insert("window_width", width.clone());
        app.insert("window_height", LegacyValue::Int(800));
        app.insert("window_x_pos", LegacyValue::Int(-1));
        app.insert("window_y_pos", LegacyValue::Int(-1));
        let mut config = Dict::new();
        config.insert("APP", LegacyValue::Dict(app));
        config.insert("MONITORS", LegacyValue::Dict(Dict::new()));
        let plan = native_window_plan(&config, &[], 100.0, 1920, 1080).unwrap();
        assert_eq!(plan.width.py_str().unwrap(), text);
        assert!(plan.x.py_lt(&LegacyValue::Int(i64::MIN)).unwrap());
        assert_eq!(plan.y, LegacyValue::Int(140));
        assert!(
            i32::try_from(plan.width.integer().unwrap()).is_err(),
            "only the backend's native range rejects this extent"
        );
        assert_eq!(plan.x, LegacyValue::from_integer(LegacyValue::Float(960.0 - 5e79).py_integer().unwrap()));
    }

    #[test]
    fn native_geometry_float_division_overflow_occurs_during_centering() {
        let mut app = Dict::new();
        app.insert("window_width", LegacyValue::from(format!("1{}", "0".repeat(400))));
        app.insert("window_height", LegacyValue::Int(800));
        let mut config = Dict::new();
        config.insert("APP", LegacyValue::Dict(app));
        config.insert("MONITORS", LegacyValue::Dict(Dict::new()));
        let error = native_window_plan(&config, &[], 100.0, 1920, 1080).unwrap_err();
        assert_eq!(error.class_name, "OverflowError");
        assert_eq!(error.message, "integer division result too large for a float");
    }
}
