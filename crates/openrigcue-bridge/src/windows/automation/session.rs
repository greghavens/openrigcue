//! A routine-local COM lifetime. Closing stdin drops every retained control.
use super::*;
use openrigcue_model::automation::{ControlAction, SessionRequest};
use std::time::{Duration, Instant};

struct Control {
    source: Source,
    node: Option<IUIAutomationElement>,
}
enum Source {
    Root(String),
    Child { parent: u64, selector: Selector },
    Direct,
}

pub(crate) struct Session {
    automation: IUIAutomation,
    controls: Vec<Control>,
    #[cfg(test)]
    root: Option<IUIAutomationElement>,
    _com: Com,
}
impl Session {
    pub(crate) fn new() -> Result<Self, String> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok().map_err(error)? };
        let com = Com;
        let automation = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).map_err(error)? };
        Ok(Self {
            automation,
            controls: Vec::new(),
            #[cfg(test)]
            root: None,
            _com: com,
        })
    }
    #[cfg(test)]
    pub(super) fn with_root(root: IUIAutomationElement) -> Result<Self, String> {
        let mut session = Self::new()?;
        session.root = Some(root);
        Ok(session)
    }
    fn get(&self, id: u64) -> Result<&Control, String> {
        self.controls.get(id as usize).ok_or_else(|| "Unknown routine control".into())
    }
    fn push(&mut self, control: Control) -> Result<u64, String> {
        if self.controls.len() == 16_384 {
            return Err("Routine control limit exceeded".into());
        }
        let id = self.controls.len() as u64;
        self.controls.push(control);
        Ok(id)
    }
    fn root(&self) -> Result<IUIAutomationElement, String> {
        #[cfg(test)]
        if let Some(root) = &self.root {
            return Ok(root.clone());
        }
        unsafe { self.automation.GetRootElement().map_err(error) }
    }
    fn search_once(
        &self,
        id: u64,
        parent: Option<&IUIAutomationElement>,
    ) -> Result<Option<IUIAutomationElement>, String> {
        match &self.get(id)?.source {
            Source::Root(name) => {
                element_from_root(&self.automation, &self.root()?, &Element { window: name.clone(), path: vec![] })
            }
            Source::Child { selector, .. } => {
                let parent = parent.ok_or("Routine parent missing")?;
                match selector {
                    Selector::Name { depth, .. } | Selector::AutomationId { depth, .. } => {
                        if *depth > 16 {
                            return Err("Automation search is too deep".into());
                        }
                        find(&self.automation, parent, selector, *depth)
                    }
                    Selector::Child { index } => Ok(children(&self.automation, parent)?.get(*index as usize).cloned()),
                }
            }
            Source::Direct => Ok(self.get(id)?.node.clone()),
        }
    }
    fn exists(&mut self, id: u64, timeout: Duration, interval: Duration) -> Result<bool, String> {
        let started = Instant::now();
        if matches!(self.get(id)?.source, Source::Direct) {
            let node = self.get(id)?.node.as_ref().ok_or("Direct routine control missing")?;
            let root = self.root()?;
            unsafe {
                if self.automation.CompareElements(node, &root).map_err(error)?.as_bool() {
                    return Ok(true);
                }
                return Ok(optional_element(raw_walker(&self.automation)?.GetParentElement(node))?.is_some());
            }
        }
        // An explicit existence check searches again: it invalidates the selector cache, even after a prior success.
        self.controls[id as usize].node = None;
        let parent_id = match &self.get(id)?.source {
            Source::Child { parent, .. } => Some(*parent),
            _ => None,
        };
        let parent = match parent_id {
            Some(parent_id) => {
                if self.get(parent_id)?.node.is_none() && !self.exists(parent_id, timeout, interval)? {
                    return Ok(false);
                }
                self.get(parent_id)?.node.clone()
            }
            None => None,
        };
        let node = openrigcue_model::automation::wait_for(
            || self.search_once(id, parent.as_ref()),
            timeout,
            interval,
            || started.elapsed(),
            std::thread::sleep,
        )?;
        let found = node.is_some();
        self.controls[id as usize].node = node;
        Ok(found)
    }
    fn resolve(&mut self, id: u64) -> Result<IUIAutomationElement, String> {
        if self.get(id)?.node.is_none() && !self.exists(id, Duration::from_secs(1), Duration::from_millis(500))? {
            return Err("Automation control not found".into());
        }
        self.get(id)?.node.clone().ok_or_else(|| "Automation control not found".into())
    }
    pub(crate) fn request(&mut self, request: SessionRequest) -> Result<Response, String> {
        let mut response = Response { found: true, ..Default::default() };
        match request {
            SessionRequest::Root { window } => {
                response.controls.push(self.push(Control { source: Source::Root(window), node: None })?)
            }
            SessionRequest::Child { parent, selector } => {
                self.get(parent)?;
                response.controls.push(self.push(Control { source: Source::Child { parent, selector }, node: None })?);
            }
            SessionRequest::Exists { control, timeout_ms, interval_ms } => {
                if timeout_ms > 10_000 || interval_ms == 0 || interval_ms > 10_000 {
                    return Err("Invalid routine search timing".into());
                }
                response.found =
                    self.exists(control, Duration::from_millis(timeout_ms), Duration::from_millis(interval_ms))?;
            }
            SessionRequest::Execute { control, action } => {
                let node = self.resolve(control)?;
                match action {
                    ControlAction::Name => {
                        response.values.push(unsafe { node.CurrentName().map_err(error)?.to_string() })
                    }
                    ControlAction::Children => {
                        for child in children(&self.automation, &node)? {
                            response.controls.push(self.push(Control { source: Source::Direct, node: Some(child) })?);
                        }
                    }
                    ControlAction::Activate => {
                        crate::automation_actions::activate(&mut NativeAction(ActionNode::Uia(&node)))?
                    }
                    ControlAction::Focus => crate::automation_actions::focus(&mut NativeAction(ActionNode::Uia(&node))),
                    ControlAction::Topmost => {
                        crate::automation_actions::topmost(&mut NativeAction(ActionNode::Uia(&node)))?
                    }
                    ControlAction::Click => {
                        crate::automation_actions::click(&mut NativeAction(ActionNode::Uia(&node)))?
                    }
                }
            }
        }
        Ok(response)
    }
}
