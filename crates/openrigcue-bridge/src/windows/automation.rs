//! Scoped COM calls; native interfaces live for one request or one routine session.
#![allow(unsafe_code)]
#[path = "automation/session.rs"]
pub(crate) mod session;
use openrigcue_model::automation::{Element, Request, Response, Selector};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    mouse_event, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetCursorPos, GetSystemMetrics, GetWindowTextLengthW, GetWindowTextW, IsIconic,
    IsWindowVisible, SetCursorPos, SetForegroundWindow, SetWindowPos, ShowWindow, GA_ROOT, HWND_TOPMOST, SM_CXSCREEN,
    SM_CYSCREEN, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE, SW_SHOW,
};

fn error(value: windows::core::Error) -> String {
    format!("UI Automation: {value}")
}
struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

fn titles() -> Result<Response, String> {
    unsafe extern "system" fn collect(window: HWND, data: LPARAM) -> BOOL {
        if IsWindowVisible(window).as_bool() {
            let length = GetWindowTextLengthW(window).max(0) as usize;
            let mut text = vec![0u16; length + 1];
            let read = GetWindowTextW(window, &mut text).max(0) as usize;
            let values = &mut *(data.0 as *mut Vec<String>);
            values.push(String::from_utf16_lossy(&text[..read]));
        }
        BOOL(1)
    }
    let mut values = Vec::new();
    unsafe { EnumWindows(Some(collect), LPARAM((&mut values as *mut Vec<String>) as isize)).map_err(error)? };
    Ok(Response { values, found: true, ..Default::default() })
}
fn children(automation: &IUIAutomation, node: &IUIAutomationElement) -> Result<Vec<IUIAutomationElement>, String> {
    unsafe {
        let walker = raw_walker(automation)?;
        let mut result = Vec::new();
        let mut child = optional_element(walker.GetFirstChildElement(node))?;
        while let Some(current) = child {
            if result.len() == 4096 {
                return Err("Automation child limit exceeded".into());
            }
            child = optional_element(walker.GetNextSiblingElement(&current))?;
            result.push(current);
        }
        Ok(result)
    }
}

fn raw_walker(automation: &IUIAutomation) -> Result<IUIAutomationTreeWalker, String> {
    unsafe {
        match automation.RawViewWalker() {
            Ok(walker) => Ok(walker),
            // Wine may expose the general tree walker but leave the convenience
            // raw-view property unimplemented. The raw view includes every node.
            Err(value) if value.code() == windows::Win32::Foundation::E_NOTIMPL => {
                let all = automation.CreateTrueCondition().map_err(error)?;
                automation.CreateTreeWalker(&all).map_err(error)
            }
            Err(value) => Err(error(value)),
        }
    }
}

fn optional_element(
    value: windows::core::Result<IUIAutomationElement>,
) -> Result<Option<IUIAutomationElement>, String> {
    match value {
        Ok(value) => Ok(Some(value)),
        // The walker returns S_OK with a null interface at the end of a branch.
        // windows-rs represents that null interface as an error with code zero.
        Err(value) if value.code().is_ok() || value.code() == windows::Win32::Foundation::E_POINTER => Ok(None),
        Err(value) => Err(error(value)),
    }
}
fn find(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    selector: &Selector,
    remaining: u32,
) -> Result<Option<IUIAutomationElement>, String> {
    if remaining == 0 {
        return Ok(None);
    }
    for child in children(automation, root)? {
        let matches = unsafe {
            match selector {
                Selector::Name { value, .. } => {
                    child.CurrentName().map_err(error)?.as_wide().iter().copied().eq(value.encode_utf16())
                }
                Selector::AutomationId { value, .. } => {
                    automation_id(&child)?.as_wide().iter().copied().eq(value.encode_utf16())
                }
                Selector::Child { .. } => false,
            }
        };
        if matches {
            return Ok(Some(child));
        }
        if let Some(found) = find(automation, &child, selector, remaining - 1)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}
fn element(automation: &IUIAutomation, target: &Element) -> Result<Option<IUIAutomationElement>, String> {
    let root = unsafe { automation.GetRootElement().map_err(error)? };
    element_from_root(automation, &root, target)
}

fn wine_desktop_windows(
    root: &IUIAutomationElement,
    walker: &IUIAutomationTreeWalker,
) -> Result<Option<Vec<HWND>>, String> {
    unsafe {
        use windows::core::{s, w};
        use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        let wine = GetModuleHandleW(w!("ntdll.dll"))
            .ok()
            .is_some_and(|module| GetProcAddress(module, s!("wine_get_version")).is_some());
        // Some Wine releases expose a valid desktop node but no children.
        // Acquire actual UIA provider roots by HWND only in that case. Child
        // discovery still uses the provider's raw UIA tree and properties.
        if !wine || optional_element(walker.GetFirstChildElement(root))?.is_some() {
            return Ok(None);
        }
        unsafe extern "system" fn collect(window: HWND, data: LPARAM) -> BOOL {
            let windows = &mut *(data.0 as *mut Vec<HWND>);
            windows.push(window);
            BOOL(1)
        }
        let mut windows = Vec::<HWND>::new();
        EnumWindows(Some(collect), LPARAM((&mut windows as *mut Vec<HWND>) as isize)).map_err(error)?;
        Ok(Some(windows))
    }
}

fn window_from_roots(
    automation: &IUIAutomation,
    windows: Vec<HWND>,
    name: &str,
    walker: &IUIAutomationTreeWalker,
) -> Result<Option<IUIAutomationElement>, String> {
    for window in windows {
        unsafe {
            // Resolve each root only when reached, so an earlier match returns even
            // if a later unrelated window is destroyed or inaccessible.
            let root = automation.ElementFromHandle(window).map_err(error)?;
            if root.CurrentName().map_err(error)?.as_wide().iter().copied().eq(name.encode_utf16())
                && root.CurrentControlType().map_err(error)? == UIA_WindowControlTypeId
            {
                return Ok(Some(root.clone()));
            }
            let mut child = optional_element(walker.GetFirstChildElement(&root))?;
            while let Some(node) = child {
                if node.CurrentName().map_err(error)?.as_wide().iter().copied().eq(name.encode_utf16())
                    && node.CurrentControlType().map_err(error)? == UIA_WindowControlTypeId
                {
                    return Ok(Some(node));
                }
                child = optional_element(walker.GetNextSiblingElement(&node))?;
            }
        }
    }
    Ok(None)
}

fn automation_id(node: &IUIAutomationElement) -> Result<windows::core::BSTR, String> {
    unsafe {
        match node.CurrentAutomationId() {
            Err(value) if value.code() == windows::Win32::Foundation::E_NOTIMPL => {
                // Wine implements property access before some convenience getters.
                // Request the same property, including its documented empty default.
                let value = node.GetCurrentPropertyValueEx(UIA_AutomationIdPropertyId, false).map_err(error)?;
                match value.as_raw().Anonymous.Anonymous.vt {
                    0 => Ok(windows::core::BSTR::new()), // VT_EMPTY: unsupported provider property.
                    8 => windows::core::BSTR::try_from(&value).map_err(error), // VT_BSTR.
                    _ => Err("UI Automation: AutomationId property is not a string".into()),
                }
            }
            result => result.map_err(error),
        }
    }
}

fn native_handle(node: &IUIAutomationElement) -> Result<HWND, String> {
    unsafe {
        match node.CurrentNativeWindowHandle() {
            Err(value) if value.code() == windows::Win32::Foundation::E_NOTIMPL => {
                let value = node.GetCurrentPropertyValueEx(UIA_NativeWindowHandlePropertyId, false).map_err(error)?;
                let raw = value.as_raw().Anonymous.Anonymous;
                // The UIA NativeWindowHandle property is VT_I4, including on Win64.
                // Read its declared value directly: Wine's propsys VariantToInt32 is a stub.
                match raw.vt {
                    0 => Ok(HWND(0)),                           // VT_EMPTY: the documented no-native-window default.
                    3 => Ok(HWND(raw.Anonymous.lVal as isize)), // VT_I4.
                    _ => Err("UI Automation: NativeWindowHandle property is not an integer".into()),
                }
            }
            result => result.map_err(error),
        }
    }
}

fn element_from_root(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
    target: &Element,
) -> Result<Option<IUIAutomationElement>, String> {
    if target.path.len() > 24 {
        return Err("Automation path is too deep".into());
    }
    let walker = raw_walker(automation)?;
    let selected = if let Some(windows) = wine_desktop_windows(root, &walker)? {
        window_from_roots(automation, windows, &target.window, &walker)?
    } else {
        crate::automation_tree::window(
            root,
            &target.window,
            &|node| unsafe { optional_element(walker.GetFirstChildElement(node)) },
            &|node| unsafe { optional_element(walker.GetNextSiblingElement(node)) },
            &|node| unsafe { node.CurrentControlType().map(|kind| kind == UIA_WindowControlTypeId).map_err(error) },
            &|node| unsafe { node.CurrentName().map(|name| name.to_string()).map_err(error) },
        )?
    };
    let mut current = match selected {
        Some(current) => current,
        None => return Ok(None),
    };
    for selector in &target.path {
        current = match selector {
            Selector::Child { index } => match children(automation, &current)?.get(*index as usize).cloned() {
                Some(child) => child,
                None => return Ok(None),
            },
            Selector::Name { depth, .. } | Selector::AutomationId { depth, .. } => {
                if *depth > 16 {
                    return Err("Automation search is too deep".into());
                }
                match find(automation, &current, selector, *depth)? {
                    Some(child) => child,
                    None => return Ok(None),
                }
            }
        };
    }
    Ok(Some(current))
}
pub fn run(request: Request) -> Result<Response, String> {
    // Window titles come from enumerating visible top-level HWNDs, without COM.
    if matches!(request, Request::Titles) {
        return titles();
    }
    if let Request::Launch { command } = request {
        // Expand the Windows environment inside this same prefix, never a shell.
        let input: Vec<u16> = command.encode_utf16().chain(Some(0)).collect();
        let size = unsafe { ExpandEnvironmentStringsW(PCWSTR(input.as_ptr()), None) };
        if size == 0 || size > 32768 {
            return Err("Cannot expand routine program path".into());
        }
        let mut expanded = vec![0_u16; size as usize];
        if unsafe { ExpandEnvironmentStringsW(PCWSTR(input.as_ptr()), Some(&mut expanded)) } != size {
            return Err("Routine environment changed during path expansion".into());
        }
        let path = std::ffi::OsString::from(String::from_utf16_lossy(&expanded[..expanded.len() - 1]));
        std::process::Command::new(path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Start program: {e}"))?;
        return Ok(Response { found: true, ..Default::default() });
    }
    unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().map_err(error)? };
    let _com = Com;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(error)? };
    let target = match &request {
        Request::Exists { element }
        | Request::Name { element }
        | Request::Children { element }
        | Request::Activate { element }
        | Request::Focus { element }
        | Request::Topmost { element }
        | Request::Click { element } => element,
        _ => unreachable!(),
    };
    let started = std::time::Instant::now();
    let selected = if matches!(request, Request::Exists { .. }) {
        // The caller owns the deadline of an explicit existence check.
        element(&automation, target)
    } else {
        // Every routine waits up to 1s for an element to appear.
        openrigcue_model::automation::wait_for(
            || element(&automation, target),
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(500),
            || started.elapsed(),
            std::thread::sleep,
        )
    };
    let node = match selected {
        Ok(Some(node)) => node,
        Ok(None) if matches!(request, Request::Exists { .. }) => return Ok(Response::default()),
        Ok(None) => return Err("Automation control not found".into()),
        Err(error) => return Err(error),
    };
    let values = unsafe {
        match request {
            Request::Exists { .. } => vec![],
            Request::Name { .. } => vec![node.CurrentName().map_err(error)?.to_string()],
            Request::Children { .. } => children(&automation, &node)?
                .iter()
                .map(|child| child.CurrentName().map(|name| name.to_string()).map_err(error))
                .collect::<Result<Vec<_>, _>>()?,
            Request::Focus { .. } => {
                // A failed focus is not an error: the routines ignore its result.
                crate::automation_actions::focus(&mut NativeAction(ActionNode::Uia(&node)));
                vec![]
            }
            Request::Activate { .. } => {
                crate::automation_actions::activate(&mut NativeAction(ActionNode::Uia(&node)))?;
                vec![]
            }
            Request::Topmost { .. } => {
                crate::automation_actions::topmost(&mut NativeAction(ActionNode::Uia(&node)))?;
                vec![]
            }
            Request::Click { .. } => {
                crate::automation_actions::click(&mut NativeAction(ActionNode::Uia(&node)))?;
                vec![]
            }
            _ => unreachable!(),
        }
    };
    Ok(Response { values, found: true, ..Default::default() })
}

enum ActionNode<'a> {
    Uia(&'a IUIAutomationElement),
    #[cfg(test)]
    Window(HWND),
}
struct NativeAction<'a>(ActionNode<'a>);
impl crate::automation_actions::Native for NativeAction<'_> {
    type Handle = HWND;
    fn handle(&mut self) -> Result<HWND, String> {
        match self.0 {
            ActionNode::Uia(node) => native_handle(node),
            #[cfg(test)]
            ActionNode::Window(window) => Ok(window),
        }
    }
    fn top_level(&mut self, h: HWND) -> bool {
        unsafe { h.0 != 0 && GetAncestor(h, GA_ROOT) == h }
    }
    fn iconic(&mut self, h: HWND) -> bool {
        unsafe { IsIconic(h).as_bool() }
    }
    fn visible(&mut self, h: HWND) -> bool {
        unsafe { IsWindowVisible(h).as_bool() }
    }
    fn show(&mut self, h: HWND, restore: bool) {
        unsafe {
            let _ = ShowWindow(h, if restore { SW_RESTORE } else { SW_SHOW });
        }
    }
    fn foreground(&mut self, h: HWND) {
        unsafe {
            let _ = SetForegroundWindow(h);
        }
    }
    fn topmost(&mut self, h: HWND) {
        unsafe {
            let _ = SetWindowPos(h, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
    }
    fn focus(&mut self) {
        match self.0 {
            ActionNode::Uia(node) => unsafe {
                let _ = node.SetFocus();
            },
            #[cfg(test)]
            ActionNode::Window(_) => {}
        }
    }
    fn bounds(&mut self) -> Result<(i32, i32, i32, i32), String> {
        match self.0 {
            ActionNode::Uia(node) => unsafe {
                node.CurrentBoundingRectangle().map(|r| (r.left, r.top, r.right, r.bottom)).map_err(error)
            },
            #[cfg(test)]
            ActionNode::Window(window) => {
                let mut r = windows::Win32::Foundation::RECT::default();
                unsafe {
                    windows::Win32::UI::WindowsAndMessaging::GetWindowRect(window, &mut r).map_err(error)?;
                }
                Ok((r.left, r.top, r.right, r.bottom))
            }
        }
    }
    fn cursor(&mut self) -> (i32, i32) {
        let mut p = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut p);
        }
        (p.x, p.y)
    }
    fn screen(&mut self) -> (i32, i32) {
        unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) }
    }
    fn set_cursor(&mut self, x: i32, y: i32) {
        unsafe {
            let _ = SetCursorPos(x, y);
        }
    }
    fn mouse(&mut self, down: bool, x: u32, y: u32) {
        unsafe {
            mouse_event(
                (if down { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }) | MOUSEEVENTF_ABSOLUTE,
                x as i32,
                y as i32,
                0,
                0,
            );
        }
    }
    fn sleep(&mut self, t: std::time::Duration) {
        std::thread::sleep(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn explicit_provider(test: impl FnOnce(&IUIAutomation, &IUIAutomationElement, HWND)) {
        if std::env::var("OPENRIGCUE_EXPLICIT_UIA_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        unsafe {
            use windows::core::w;
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().unwrap();
            let _com = Com;
            let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).unwrap();
            let window = windows::Win32::UI::WindowsAndMessaging::FindWindowW(
                w!("OpenRigCueExplicitProvider"),
                w!("OpenRigCue explicit provider"),
            );
            assert_ne!(window.0, 0, "private explicit provider missing");
            let root = automation.ElementFromHandle(window).unwrap();
            test(&automation, &root, window);
        }
    }

    #[test]
    fn explicit_provider_traverses_production_selectors_and_order() {
        explicit_provider(|automation, root, _| unsafe {
            let target = Element {
                window: "FanaLab V1.66".into(),
                path: vec![Selector::AutomationId { value: "tuning".into(), depth: 2 }],
            };
            let selected = element_from_root(automation, root, &target).unwrap().unwrap();
            assert_eq!(selected.CurrentName().unwrap().to_string(), "Tuning Menu");
            let names: Vec<_> = children(automation, root)
                .unwrap()
                .iter()
                .map(|node| node.CurrentName().unwrap().to_string())
                .collect();
            assert_eq!(names, ["FanaLab V1.66"]);
            let target = Element { window: "FanaLab V1.66".into(), path: vec![Selector::Child { index: 0 }] };
            assert_eq!(
                element_from_root(automation, root, &target).unwrap().unwrap().CurrentName().unwrap().to_string(),
                "Tuning Menu"
            );
            let target = Element {
                window: "FanaLab V1.66".into(),
                path: vec![Selector::Name { value: "Tuning Menu".into(), depth: 1 }],
            };
            assert!(element_from_root(automation, root, &target).unwrap().is_some());
        });
    }

    #[test]
    fn explicit_provider_preserves_native_handle_property() {
        explicit_provider(|_, root, window| assert_eq!(native_handle(root).unwrap(), window));
    }

    #[test]
    fn explicit_provider_is_found_from_the_real_desktop_without_an_injected_root() {
        explicit_provider(|automation, _, window| unsafe {
            use openrigcue_model::automation::{ControlAction, SessionRequest};
            let target = Element {
                window: "FanaLab V1.66".into(),
                path: vec![Selector::AutomationId { value: "tuning".into(), depth: 2 }],
            };
            let selected = element(automation, &target).unwrap().expect("provider-backed control from desktop");
            assert_eq!(selected.CurrentName().unwrap().to_string(), "Tuning Menu");
            for name in ["fanalab V1.66", "Tuning Menu", "Profile 1"] {
                assert!(element(automation, &Element { window: name.into(), path: vec![] }).unwrap().is_none());
            }
            // The root lookup is not restricted to visible windows.
            let _ = ShowWindow(window, windows::Win32::UI::WindowsAndMessaging::SW_HIDE);
            assert!(element(automation, &target).unwrap().is_some());
            let _ = ShowWindow(window, SW_SHOW);
            let mut session = session::Session::new().unwrap();
            let root = session.request(SessionRequest::Root { window: target.window }).unwrap().controls[0];
            assert!(
                session.request(SessionRequest::Exists { control: root, timeout_ms: 1, interval_ms: 1 }).unwrap().found
            );
            let selected = session
                .request(SessionRequest::Child {
                    parent: root,
                    selector: Selector::AutomationId { value: "tuning".into(), depth: 2 },
                })
                .unwrap()
                .controls[0];
            assert_eq!(
                session
                    .request(SessionRequest::Execute { control: selected, action: ControlAction::Name })
                    .unwrap()
                    .values,
                ["Tuning Menu"]
            );
        });
    }

    #[test]
    fn explicit_provider_session_retains_controls_and_invalidates_only_explicit_searches() {
        explicit_provider(|_, root, window| unsafe {
            use openrigcue_model::automation::{ControlAction, SessionRequest};
            use windows::core::w;
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::UI::WindowsAndMessaging::{RemovePropW, SetPropW};
            let mut session = session::Session::with_root(root.clone()).unwrap();
            let root_id = session.request(SessionRequest::Root { window: "FanaLab V1.66".into() }).unwrap().controls[0];
            assert!(
                session
                    .request(SessionRequest::Exists { control: root_id, timeout_ms: 1, interval_ms: 1 })
                    .unwrap()
                    .found
            );
            let make = |session: &mut session::Session| {
                session
                    .request(SessionRequest::Child {
                        parent: root_id,
                        selector: Selector::Name { value: "Tuning Menu".into(), depth: 1 },
                    })
                    .unwrap()
                    .controls[0]
            };
            let selected = make(&mut session);
            let independent = make(&mut session);
            assert_ne!(selected, independent);
            let name = |session: &mut session::Session, control| {
                session.request(SessionRequest::Execute { control, action: ControlAction::Name })
            };
            assert_eq!(name(&mut session, selected).unwrap().values, ["Tuning Menu"]);
            let children = session
                .request(SessionRequest::Execute { control: root_id, action: ControlAction::Children })
                .unwrap()
                .controls;
            assert_eq!(children.len(), 2);
            SetPropW(window, w!("reverse"), HANDLE(1)).unwrap();
            assert_eq!(name(&mut session, children[0]).unwrap().values, ["Tuning Menu"]);
            SetPropW(window, w!("rename"), HANDLE(1)).unwrap();
            assert_eq!(name(&mut session, selected).unwrap().values, ["Renamed"]);
            assert!(
                name(&mut session, independent).is_err(),
                "independent lazy controls must not share the resolved cache"
            );
            assert!(
                !session
                    .request(SessionRequest::Exists { control: selected, timeout_ms: 1, interval_ms: 1 })
                    .unwrap()
                    .found
            );
            let _ = RemovePropW(window, w!("rename"));
            assert_eq!(name(&mut session, selected).unwrap().values, ["Tuning Menu"]);
            let _ = RemovePropW(window, w!("reverse"));
        });
    }

    #[test]
    fn explicit_provider_actions_use_the_actual_com_properties() {
        explicit_provider(|_, root, window| unsafe {
            use windows::Win32::UI::WindowsAndMessaging::*;
            let mut action = NativeAction(ActionNode::Uia(root));
            let _ = ShowWindow(window, SW_MINIMIZE);
            assert!(IsIconic(window).as_bool());
            crate::automation_actions::activate(&mut action).unwrap();
            assert!(!IsIconic(window).as_bool());
            crate::automation_actions::focus(&mut action);
            crate::automation_actions::topmost(&mut action).unwrap();
            assert_ne!(GetWindowLongW(window, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as i32, 0);
            crate::automation_actions::click(&mut action).unwrap();
            assert_eq!(crate::automation_actions::Native::cursor(&mut action), (200, 115));
        });
    }

    #[test]
    fn isolated_native_actions_restore_move_click_and_topmost() {
        if std::env::var("OPENRIGCUE_ISOLATED_UIA_FIXTURE").as_deref() != Ok("1") {
            return;
        }
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::*;
            let title: Vec<u16> = "OpenRigCue isolated UIA fixture".encode_utf16().chain(Some(0)).collect();
            let window = FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr()));
            assert!(window.0 != 0, "private synthetic window missing");
            let mut action = NativeAction(ActionNode::Window(window));
            let _ = ShowWindow(window, SW_MINIMIZE);
            assert!(IsIconic(window).as_bool());
            crate::automation_actions::activate(&mut action).unwrap();
            assert!(!IsIconic(window).as_bool());
            crate::automation_actions::topmost(&mut action).unwrap();
            assert_ne!(GetWindowLongW(window, GWL_EXSTYLE) & WS_EX_TOPMOST.0 as i32, 0);
            crate::automation_actions::click(&mut action).unwrap();
            let (x, y) = crate::automation_actions::Native::cursor(&mut action);
            let mut rect = windows::Win32::Foundation::RECT::default();
            GetWindowRect(window, &mut rect).unwrap();
            assert_eq!((x, y), (rect.left + (rect.right - rect.left) / 2, rect.top + (rect.bottom - rect.top) / 2));
        }
    }
}
