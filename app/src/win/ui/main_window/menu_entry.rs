//! One menu tree rendered as Avalonia controls or as the macOS menu bar.

use super::main_common::reactive_items;
use rustolonia::{AnyEl, Command, El, MenuItem, MenuItemToggleType, Separator, Signal};
#[cfg(target_os = "macos")]
use std::cell::RefCell;

pub(super) enum MenuEntry {
    Action {
        text: String,
        command: Command,
        gesture: Option<String>,
        enabled: Option<Signal<bool>>,
        checked: Option<Signal<bool>>,
    },
    Disabled(String),
    Separator,
    Submenu(String, Vec<MenuEntry>),
    Dynamic {
        text: String,
        data: Box<dyn Fn() -> Vec<String>>,
        build: Box<dyn Fn(&Vec<String>) -> Vec<MenuEntry>>,
        #[cfg(target_os = "macos")]
        cached: RefCell<Option<DynamicItems>>,
    },
}

impl MenuEntry {
    pub fn action(text: &str, command: Command, gesture: Option<&str>) -> Self {
        Self::Action { text: text.to_owned(), command, gesture: gesture.map(crate::ui::shortcut), enabled: None, checked: None }
    }

    pub fn enabled(mut self, value: Signal<bool>) -> Self {
        if let Self::Action { enabled, .. } = &mut self {
            *enabled = Some(value);
        }
        self
    }

    pub fn checked(mut self, value: Signal<bool>) -> Self {
        if let Self::Action { checked, .. } = &mut self {
            *checked = Some(value);
        }
        self
    }

    pub fn dynamic(text: &str, data: impl Fn() -> Vec<String> + 'static, build: impl Fn(&Vec<String>) -> Vec<MenuEntry> + 'static) -> Self {
        Self::Dynamic {
            text: text.replace('_', ""),
            data: Box::new(data),
            build: Box::new(build),
            #[cfg(target_os = "macos")]
            cached: RefCell::new(None),
        }
    }

    pub fn control(self) -> El<MenuItem> {
        match self {
            Self::Action { text, command, gesture, enabled, checked } => {
                let mut item = MenuItem::el().header(text).command(command);
                if let Some(enabled) = enabled {
                    item = item.enabled(enabled);
                }
                if let Some(gesture) = gesture {
                    item = item.input_gesture(gesture.clone()).hot_key(gesture);
                }
                if let Some(checked) = checked {
                    item = item.toggle_type(MenuItemToggleType::CheckBox).checked(checked);
                }
                item
            }
            Self::Disabled(text) => MenuItem::el().header(text).enabled(false),
            Self::Submenu(text, items) => MenuItem::el().header(text).items_iter(items.into_iter().map(AnyEl::from)),
            Self::Dynamic { text, data, build, .. } => {
                reactive_items(MenuItem::el().header(text), MenuItem::items, data, move |values| build(values).into_iter().map(AnyEl::from).collect())
            }
            Self::Separator => unreachable!("separators are not menu items"),
        }
    }
}

#[cfg(target_os = "macos")]
pub(super) struct DynamicItems {
    data: Vec<String>,
    entries: Vec<MenuEntry>,
    scope: rustolonia::ReactiveScope,
}

impl From<MenuEntry> for AnyEl {
    fn from(entry: MenuEntry) -> Self {
        match entry {
            MenuEntry::Separator => Separator::el().into(),
            entry => entry.control().into(),
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use rustolonia::{AppScope, Effect, NativeMenu, NativeMenuItem, ReactiveScope, Result, Variant};
    use std::sync::{Arc, Mutex};

    type Actions = Arc<Mutex<Vec<Command>>>;

    pub(in super::super) fn build(scope: &AppScope, entries: Vec<MenuEntry>) -> Result<NativeMenu> {
        let actions: Actions = Arc::default();
        let dispatch = actions.clone();
        let menu = scope.create_native_menu(move |id| {
            let command = id.checked_sub(1).and_then(|index| dispatch.lock().expect("native menu actions poisoned").get(index as usize).cloned());
            let command = command.ok_or_else(|| rustolonia::Error::Load(format!("unknown native menu command: {id}")))?;
            if command.is_enabled() {
                command.execute(Variant::None)?;
            }
            Ok(())
        })?;
        let update = menu.clone();
        let mut previous = Vec::<NativeMenuItem>::new();
        Effect::new(move || {
            let mut next_actions = Vec::new();
            let next = entries.iter().map(|entry| native_item(entry, &mut next_actions)).collect::<Vec<_>>();
            if previous != next {
                if !update_states(&update, &previous, &next)? {
                    update.set_items(&next)?;
                }
                previous = next;
            }
            *actions.lock().expect("native menu actions poisoned") = next_actions;
            Ok(())
        })?;
        Ok(menu)
    }

    fn native_item(entry: &MenuEntry, actions: &mut Vec<Command>) -> NativeMenuItem {
        match entry {
            MenuEntry::Action { text, command, gesture, enabled, checked } => {
                actions.push(command.clone());
                let id = actions.len() as u32;
                // NativeMenu interprets access-key markers; our labels are literal.
                let text = text.replace('_', "__");
                let mut item = match checked {
                    Some(value) => NativeMenuItem::check(id, &text, value.get()),
                    None => NativeMenuItem::command(id, &text),
                }
                .enabled(enabled.map(|value| value.get()).unwrap_or(true));
                if let Some(gesture) = gesture {
                    item = item.gesture(gesture);
                }
                item
            }
            MenuEntry::Disabled(text) => {
                actions.push(Command::new(|_| Ok(())));
                NativeMenuItem::command(actions.len() as u32, text.replace('_', "__")).enabled(false)
            }
            MenuEntry::Separator => NativeMenuItem::separator(),
            MenuEntry::Submenu(text, children) => NativeMenuItem::submenu(text.replace('_', "__"), children.iter().map(|child| native_item(child, actions)).collect()),
            MenuEntry::Dynamic { text, data, build, cached } => {
                let values = data();
                let mut cached = cached.borrow_mut();
                if cached.as_ref().is_none_or(|previous| previous.data != values) {
                    if let Some(previous) = cached.take() {
                        previous.scope.dispose();
                    }
                    let scope = ReactiveScope::new();
                    let entries = scope.run(|| build(&values));
                    *cached = Some(DynamicItems { data: values, entries, scope });
                }
                NativeMenuItem::submenu(text.replace('_', "__"), cached.as_ref().expect("dynamic menu was built").entries.iter().map(|child| native_item(child, actions)).collect())
            }
        }
    }

    // State changes must not rebuild an open AppKit menu.
    pub(in crate::ui) fn update_states(menu: &NativeMenu, old: &[NativeMenuItem], new: &[NativeMenuItem]) -> Result<bool> {
        use rustolonia::NativeMenuItemKind;
        if old.len() != new.len() {
            return Ok(false);
        }
        for (old, new) in old.iter().zip(new) {
            if old.header != new.header || old.gesture != new.gesture {
                return Ok(false);
            }
            match (&old.kind, &new.kind) {
                (NativeMenuItemKind::Submenu(old), NativeMenuItemKind::Submenu(new)) => {
                    if !update_states(menu, old, new)? {
                        return Ok(false);
                    }
                }
                (old_kind, new_kind) if old_kind == new_kind => {
                    if old.state != new.state {
                        let id = match new_kind {
                            NativeMenuItemKind::Command(id) | NativeMenuItemKind::Check(id) | NativeMenuItemKind::Radio(id) => *id,
                            _ => continue,
                        };
                        menu.update_item(id, None, new.state)?;
                    }
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}

#[cfg(target_os = "macos")]
pub(super) use native::build as native_menu;
#[cfg(target_os = "macos")]
pub(in crate::ui) use native::update_states as update_native_states;
