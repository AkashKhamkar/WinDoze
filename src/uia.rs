//! UI Automation (the accessibility API screen readers use), for reading what
//! the user is pointing at on the taskbar or has selected in Alt-Tab.
//! A dozing app can't react to those clicks itself, so we have to work out
//! which app they were meant for.

use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, CUIAutomation8, IUIAutomation, IUIAutomation2, IUIAutomationElement, IUIAutomationTreeWalker,
};
use windows::core::Interface;

pub struct Uia {
    auto: IUIAutomation,
    walker: IUIAutomationTreeWalker,
}

impl Uia {
    /// Must be called on a thread that has initialized COM (the engine thread, MTA).
    pub fn new() -> Option<Uia> {
        unsafe {
            let auto: IUIAutomation = CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER)
                .or_else(|_| CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER))
                .ok()?;
            // Never let a slow/hung provider stall the engine for long.
            if let Ok(auto2) = auto.cast::<IUIAutomation2>() {
                let _ = auto2.SetConnectionTimeout(500);
                let _ = auto2.SetTransactionTimeout(500);
            }
            let walker = auto.ControlViewWalker().ok()?;
            Some(Uia { auto, walker })
        }
    }

    /// Names of the element under `pt` and a few of its parents, e.g.
    /// ["Figma - 1 running window", "Running applications", "Taskbar"].
    pub fn names_at(&self, pt: POINT) -> Vec<String> {
        let el = unsafe { self.auto.ElementFromPoint(pt).ok() };
        self.names_up(el, 3)
    }

    /// Names of the focused element (the highlighted item in Alt-Tab / Task View) and its parent.
    pub fn focused_names(&self) -> Vec<String> {
        let el = unsafe { self.auto.GetFocusedElement().ok() };
        self.names_up(el, 1)
    }

    fn names_up(&self, mut el: Option<IUIAutomationElement>, parents: usize) -> Vec<String> {
        let mut out = Vec::new();
        for _ in 0..=parents {
            let Some(e) = el else { break };
            if let Ok(name) = unsafe { e.CurrentName() } {
                let name = name.to_string();
                if !name.trim().is_empty() {
                    out.push(name);
                }
            }
            el = unsafe { self.walker.GetParentElement(&e).ok() };
        }
        out
    }
}
