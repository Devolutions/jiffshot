//! Retains capture geometry across a multi-frame operation.

use jiffshot_capture::screen::{self, Grabber, Monitor};
use jiffshot_capture::{CaptureMapping, Frame, Rect};
use std::io;

#[cfg(target_os = "macos")]
use crate::settings::SavedAreaConfirmation;

#[cfg(target_os = "macos")]
#[derive(Clone, Copy)]
enum OutputMapping {
    Reference(CaptureMapping),
    Logical { reference: CaptureMapping, exact: screen::LogicalCaptureMapping },
}

#[cfg(target_os = "macos")]
impl OutputMapping {
    fn reference(self) -> CaptureMapping {
        match self {
            Self::Reference(mapping) | Self::Logical { reference: mapping, .. } => mapping,
        }
    }
}

#[derive(Clone)]
pub(crate) struct CaptureLayout {
    #[cfg(target_os = "macos")]
    display: screen::DisplayLayout,
    #[cfg(target_os = "macos")]
    output: Option<OutputMapping>,
    #[cfg(test)]
    invalid: bool,
}

impl CaptureLayout {
    pub fn current() -> io::Result<Self> {
        Ok(Self {
            #[cfg(target_os = "macos")]
            display: screen::DisplayLayout::current()?,
            #[cfg(target_os = "macos")]
            output: None,
            #[cfg(test)]
            invalid: false,
        })
    }

    #[cfg(target_os = "macos")]
    pub fn from_display(display: screen::DisplayLayout) -> Self {
        Self {
            display,
            output: None,
            #[cfg(test)]
            invalid: false,
        }
    }

    pub fn validate(&self) -> io::Result<()> {
        #[cfg(test)]
        if self.invalid {
            return Err(io::Error::other("the display configuration changed during capture; retry the capture"));
        }
        #[cfg(target_os = "macos")]
        self.display.validate()?;
        Ok(())
    }

    pub fn grab(&self, rect: Rect) -> io::Result<Frame> {
        self.validate()?;
        #[cfg(target_os = "macos")]
        {
            if let Some(OutputMapping::Logical { exact, .. }) = self.output_for(rect) {
                return self.display.grab_logical(exact);
            }
            return self.display.grab_mapped(self.mapping(rect)?);
        }
        #[cfg(windows)]
        screen::grab(rect)
    }

    #[cfg(any(windows, test))]
    pub fn grabber(&self, rect: Rect) -> io::Result<Grabber> {
        self.validate()?;
        #[cfg(target_os = "macos")]
        return self.grabber_for_mapping(self.mapping(rect)?);
        #[cfg(windows)]
        Grabber::new(rect)
    }

    pub fn monitors(&self) -> Vec<Monitor> {
        #[cfg(target_os = "macos")]
        return self.display.monitors().to_vec();
        #[cfg(windows)]
        screen::monitors()
    }

    pub fn virtual_screen(&self) -> Rect {
        #[cfg(target_os = "macos")]
        return self.display.virtual_screen();
        #[cfg(windows)]
        screen::virtual_screen()
    }

    #[cfg(windows)]
    pub fn monitor_at(&self, x: i32, y: i32) -> Option<Monitor> {
        screen::monitor_at(x, y)
    }

    pub fn monitor_under_cursor(&self) -> io::Result<Option<Monitor>> {
        self.validate()?;
        #[cfg(target_os = "macos")]
        return self.display.monitor_under_cursor().map(Some);
        #[cfg(windows)]
        {
            let (x, y) = crate::sys::cursor_pos();
            Ok(self.monitor_at(x, y))
        }
    }

    /// Retains the exact display mapping for a full-monitor operation.
    pub fn with_monitor(self, monitor: &Monitor) -> io::Result<Self> {
        self.validate()?;
        #[cfg(target_os = "macos")]
        {
            let mut selected = self;
            selected.output = Some(OutputMapping::Reference(selected.monitor_mapping(monitor)?));
            Ok(selected)
        }
        #[cfg(windows)]
        {
            let _ = monitor;
            Ok(self)
        }
    }

    pub fn mapping(&self, rect: Rect) -> io::Result<CaptureMapping> {
        self.validate()?;
        #[cfg(target_os = "macos")]
        {
            self.preview_mapping(rect)
        }
        #[cfg(windows)]
        CaptureMapping::identity(rect).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty capture rectangle"))
    }

    #[cfg(target_os = "macos")]
    pub fn preview_mapping(&self, rect: Rect) -> io::Result<CaptureMapping> {
        self.validate()?;
        let mapping = match self.output_for(rect) {
            Some(mapping) => mapping.reference(),
            None => self.display.mapping(rect)?,
        };
        self.validate()?;
        Ok(mapping)
    }

    #[cfg(target_os = "macos")]
    fn output_for(&self, rect: Rect) -> Option<OutputMapping> {
        self.output.filter(|mapping| mapping.reference().rect() == rect)
    }

    #[cfg(target_os = "macos")]
    fn logical_output(&self, rect: Rect, bounds: screen::LogicalBounds) -> io::Result<OutputMapping> {
        self.validate()?;
        let exact = self.display.logical_mapping(bounds)?;
        let (width, height) = exact.size();
        let edges = bounds.edges().map(|edge| edge * self.desktop_scale());
        let reference = CaptureMapping::with_desktop_bounds(rect, width, height, edges)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "logical bounds do not match the retained target rectangle"))?;
        self.validate()?;
        Ok(OutputMapping::Logical { reference, exact })
    }

    #[cfg(target_os = "macos")]
    pub fn with_logical_bounds(mut self, rect: Rect, bounds: screen::LogicalBounds) -> io::Result<Self> {
        self.validate()?;
        self.output = Some(self.logical_output(rect, bounds)?);
        Ok(self)
    }

    #[cfg(target_os = "macos")]
    pub fn with_desktop(self) -> io::Result<Self> {
        let rect = self.virtual_screen();
        let bounds = self.display.logical_virtual_bounds()?;
        self.with_logical_bounds(rect, bounds)
    }

    #[cfg(target_os = "macos")]
    pub fn preview_target_mapping(&self, target: &jiffshot_capture::targets::Target) -> io::Result<CaptureMapping> {
        match target.logical_bounds {
            Some(bounds) => Ok(self.logical_output(target.rect, bounds)?.reference()),
            None => self.preview_mapping(target.rect),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn render_frozen(&self, frozen: &screen::FrozenDesktop, rect: Rect) -> io::Result<Frame> {
        self.validate()?;
        match self.output_for(rect) {
            Some(OutputMapping::Logical { exact, .. }) => frozen.render_logical(exact),
            _ => frozen.render(self.mapping(rect)?),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn monitor_mapping(&self, monitor: &Monitor) -> io::Result<CaptureMapping> {
        self.validate()?;
        let mapping = self.display.monitor_mapping(monitor)?;
        self.validate()?;
        Ok(mapping)
    }

    #[cfg(target_os = "macos")]
    pub fn monitor_mapping_for_rect(&self, rect: Rect) -> io::Result<CaptureMapping> {
        self.validate()?;
        let mapping = self.display.monitor_mapping_for_rect(rect)?;
        self.validate()?;
        Ok(mapping)
    }

    #[cfg(target_os = "macos")]
    pub fn for_selection(&self, selection: &crate::overlay::Selection, targets: &jiffshot_capture::targets::Targets) -> io::Result<Self> {
        let rect = selection.rect();
        match selection {
            crate::overlay::Selection::Fixed { size: (width, height), .. } => self.clone().with_output_size(rect, *width, *height),
            crate::overlay::Selection::Monitor(_) => {
                let mut selected = self.clone();
                selected.output = Some(OutputMapping::Reference(self.monitor_mapping_for_rect(rect)?));
                Ok(selected)
            }
            crate::overlay::Selection::Desktop(_) => {
                if rect != self.virtual_screen() {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "the selected desktop geometry changed"));
                }
                self.clone().with_desktop()
            }
            crate::overlay::Selection::Window { target, .. } => {
                let target = targets.list.get(*target)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the selected capture target is unavailable"))?;
                if target.rect != rect {
                    return Err(io::Error::new(io::ErrorKind::InvalidInput, "the selected capture target geometry changed"));
                }
                match target.logical_bounds {
                    Some(bounds) => self.clone().with_logical_bounds(rect, bounds),
                    None => Ok(self.clone()),
                }
            }
            _ => Ok(self.clone()),
        }
    }

    #[cfg(target_os = "macos")]
    pub fn with_output_size(mut self, rect: Rect, width: u32, height: u32) -> io::Result<Self> {
        self.output = Some(OutputMapping::Reference(
            CaptureMapping::new(rect, width, height).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid capture dimensions"))?
        ));
        Ok(self)
    }

    #[cfg(target_os = "macos")]
    pub fn fixed_at(&self, x: i32, y: i32, width: u32, height: u32) -> io::Result<(Rect, Self)> {
        self.validate()?;
        if width == 0 || height == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid fixed capture dimensions"));
        }
        let (desktop_width, desktop_height) = self.desktop_size_for_pixels(x, y, width, height)?;
        let rect = Rect::new(x, y, desktop_width, desktop_height);
        Ok((rect, self.clone().with_output_size(rect, width, height)?))
    }

    #[cfg(target_os = "macos")]
    pub fn desktop_size_for_pixels(&self, x: i32, y: i32, width: u32, height: u32) -> io::Result<(i32, i32)> {
        self.validate()?;
        let size = self.display.desktop_size_for_pixels(x, y, width, height)?;
        self.validate()?;
        Ok(size)
    }

    #[cfg(target_os = "macos")]
    pub fn desktop_size_for_pixels_precise(&self, x: f64, y: f64, width: u32, height: u32) -> io::Result<(i32, i32)> {
        self.validate()?;
        let size = self.display.desktop_size_for_pixels_precise(x, y, width, height)?;
        self.validate()?;
        Ok(size)
    }

    #[cfg(target_os = "macos")]
    pub fn desktop_pixel_step_precise(&self, x: f64, y: f64) -> io::Result<(f64, f64)> {
        self.validate()?;
        let step = self.display.desktop_pixel_step_precise(x, y)?;
        self.validate()?;
        Ok(step)
    }

    #[cfg(target_os = "macos")]
    pub fn monitor_at_precise(&self, x: f64, y: f64) -> io::Result<Monitor> {
        self.validate()?;
        let monitor = self.display.monitor_at_precise(x, y)?;
        self.validate()?;
        Ok(monitor)
    }

    #[cfg(target_os = "macos")]
    pub fn grabber_for_mapping(&self, mapping: CaptureMapping) -> io::Result<Grabber> {
        self.validate()?;
        if let Some(OutputMapping::Logical { reference, exact }) = self.output_for(mapping.rect()) {
            if reference.desktop_bounds() == mapping.desktop_bounds() {
                let resized = screen::LogicalCaptureMapping::new(exact.bounds(), mapping.size())?;
                return Grabber::for_logical(resized, &self.display);
            }
        }
        Grabber::for_mapping(mapping, &self.display)
    }

    #[cfg(target_os = "macos")]
    pub fn desktop_scale(&self) -> f64 {
        self.display.desktop_scale()
    }

    #[cfg(target_os = "macos")]
    pub fn display(&self) -> &screen::DisplayLayout {
        &self.display
    }

    #[cfg(test)]
    pub fn invalidated_for_test(mut self) -> Self {
        self.invalid = true;
        self
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SavedAreaKind {
    Fixed,
    Scheduled,
}

#[cfg(target_os = "macos")]
impl SavedAreaKind {
    fn index(self) -> usize {
        match self {
            Self::Fixed => 0,
            Self::Scheduled => 1,
        }
    }

    fn instruction(self) -> &'static str {
        match self {
            Self::Fixed => "Review the fixed position in Capture > Region options and confirm it for these displays.",
            Self::Scheduled => "Review the saved area in Capture > Time delay and confirm it for these displays.",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Scheduled => "scheduled",
        }
    }

    fn unconfirmed(self) -> io::Error {
        io::Error::other(format!("Saved coordinates need confirmation for the current display configuration. {}", self.instruction()))
    }
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
pub(crate) struct SavedAreaTicket {
    kind: SavedAreaKind,
    rect: Rect,
    layout: CaptureLayout,
    revision: u64,
}

#[cfg(target_os = "macos")]
impl SavedAreaTicket {
    fn validate_layout(&self) -> io::Result<()> {
        self.layout.validate().map_err(|error| io::Error::other(format!("{} ({error})", self.kind.unconfirmed())))
    }

    /// A delayed capture must still have its original confirmation and layout.
    pub fn validate(&self) -> io::Result<()> {
        let current = {
            let saved = SAVED_AREAS.lock().map_err(|_| io::Error::other("could not check saved capture coordinates"))?;
            saved.is_current(self)
        };
        if !current {
            return Err(self.kind.unconfirmed());
        }
        self.validate_layout()
    }

    pub fn layout(&self) -> &CaptureLayout {
        &self.layout
    }

    pub fn rect(&self) -> Rect {
        self.rect
    }

    #[cfg(test)]
    pub fn invalidated_for_test(mut self) -> Self {
        self.layout = self.layout.invalidated_for_test();
        self
    }
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct SavedAreas {
    entries: [Option<SavedAreaTicket>; 2],
    next_revision: u64,
}

#[cfg(target_os = "macos")]
impl SavedAreas {
    fn install(&mut self, kind: SavedAreaKind, rect: Rect, layout: CaptureLayout) -> io::Result<()> {
        self.next_revision = self.next_revision.checked_add(1)
            .ok_or_else(|| io::Error::other("saved capture confirmation revisions were exhausted"))?;
        self.entries[kind.index()] = Some(SavedAreaTicket { kind, rect, layout, revision: self.next_revision });
        Ok(())
    }

    fn ticket(&self, kind: SavedAreaKind, rect: Rect) -> io::Result<SavedAreaTicket> {
        self.entries[kind.index()].as_ref().filter(|entry| entry.rect == rect).cloned().ok_or_else(|| kind.unconfirmed())
    }

    fn is_current(&self, ticket: &SavedAreaTicket) -> bool {
        self.entries[ticket.kind.index()].as_ref().is_some_and(|entry| entry.revision == ticket.revision && entry.rect == ticket.rect)
    }
}

#[cfg(target_os = "macos")]
static SAVED_AREAS: std::sync::Mutex<SavedAreas> = std::sync::Mutex::new(SavedAreas { entries: [None, None], next_revision: 0 });

#[cfg(target_os = "macos")]
enum SavedAreaChange {
    Clear,
    Confirm { rect: Rect, layout: CaptureLayout },
}

/// Confirmations prepared by a settings edit but not yet made visible to captures.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub(crate) struct SavedAreaChanges {
    entries: [Option<SavedAreaChange>; 2],
}

#[cfg(target_os = "macos")]
impl SavedAreaChanges {
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    pub fn clear(&mut self, kind: SavedAreaKind) {
        self.entries[kind.index()] = Some(SavedAreaChange::Clear);
    }

    pub fn confirm(&mut self, kind: SavedAreaKind, rect: Rect) -> io::Result<SavedAreaConfirmation> {
        let (layout, confirmation) = prepare_saved_area(kind, rect, CaptureLayout::current()?)?;
        self.entries[kind.index()] = Some(SavedAreaChange::Confirm { rect, layout });
        Ok(confirmation)
    }

    /// Publishes every prepared confirmation only after the associated settings are saved.
    pub fn commit_after(self, save: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
        if self.is_empty() {
            return save();
        }
        let mut saved = SAVED_AREAS.lock().map_err(|_| io::Error::other("could not update saved capture coordinates"))?;
        let confirmations = self.entries.iter().filter(|entry| matches!(entry, Some(SavedAreaChange::Confirm { .. }))).count() as u64;
        saved.next_revision.checked_add(confirmations)
            .ok_or_else(|| io::Error::other("saved capture confirmation revisions were exhausted"))?;
        for change in self.entries.iter().flatten() {
            if let SavedAreaChange::Confirm { layout, .. } = change {
                layout.validate()?;
            }
        }
        save()?;
        for (index, change) in self.entries.into_iter().enumerate() {
            match change {
                Some(SavedAreaChange::Clear) => saved.entries[index] = None,
                Some(SavedAreaChange::Confirm { rect, layout }) => {
                    saved.next_revision += 1;
                    let kind = if index == SavedAreaKind::Fixed.index() { SavedAreaKind::Fixed } else { SavedAreaKind::Scheduled };
                    saved.entries[index] = Some(SavedAreaTicket { kind, rect, layout, revision: saved.next_revision });
                }
                None => {}
            }
        }
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) static SAVED_AREAS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(target_os = "macos")]
const COORDINATE_SPACE_VERSION: u32 = 2;

#[cfg(target_os = "macos")]
pub(crate) const SESSION_ONLY_AREA_WARNING: &str =
    "macOS could not identify a display. This area works until Jiffshot quits; review it again after restarting.";

#[cfg(all(test, target_os = "macos"))]
thread_local! {
    static MISSING_UUID_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn with_missing_uuid_for_test<R>(run: impl FnOnce() -> R) -> R {
    MISSING_UUID_FOR_TEST.with(|value| {
        struct Restore<'a>(&'a std::cell::Cell<bool>, bool);
        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }
        let restore = Restore(value, value.replace(true));
        let result = run();
        drop(restore);
        result
    })
}

#[cfg(target_os = "macos")]
fn prepare_saved_area(kind: SavedAreaKind, rect: Rect, layout: CaptureLayout) -> io::Result<(CaptureLayout, SavedAreaConfirmation)> {
    let dimensions = match kind {
        SavedAreaKind::Fixed if (16..=16384).contains(&rect.width) && (16..=16384).contains(&rect.height) => None,
        SavedAreaKind::Fixed => Some("Fixed width and height must be between 16 and 16384 pixels."),
        SavedAreaKind::Scheduled if (1..=32767).contains(&rect.width) && (1..=32767).contains(&rect.height) => None,
        SavedAreaKind::Scheduled => Some("Scheduled width and height must be between 1 and 32767 grid pixels."),
    };
    if let Some(message) = dimensions {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
    }
    let area = match kind {
        SavedAreaKind::Fixed => layout.fixed_at(rect.x, rect.y, rect.width as u32, rect.height as u32)?.0,
        SavedAreaKind::Scheduled => rect,
    };
    layout.mapping(area)?;
    let coordinate_space = layout.display.coordinate_space_key()?;
    #[cfg(test)]
    let coordinate_space = if MISSING_UUID_FOR_TEST.with(|value| value.get()) {
        let mut key = coordinate_space;
        key.components[3..5].fill(0);
        key.restorable = false;
        key
    } else {
        coordinate_space
    };
    let confirmation = SavedAreaConfirmation {
        version: COORDINATE_SPACE_VERSION,
        kind: kind.key().into(),
        rect,
        coordinate_space: coordinate_space.components,
        session_only: !coordinate_space.restorable,
    };
    Ok((layout, confirmation))
}

/// Confirms a saved rectangle for the current display layout.
#[cfg(all(test, target_os = "macos"))]
pub(crate) fn confirm_saved_area(kind: SavedAreaKind, rect: Rect) -> io::Result<SavedAreaConfirmation> {
    confirm_saved_area_in_layout(kind, rect, CaptureLayout::current()?)
}

/// Confirms a selection against the same display layout on which it was made.
#[cfg(target_os = "macos")]
pub(crate) fn confirm_saved_area_in_layout(kind: SavedAreaKind, rect: Rect, layout: CaptureLayout) -> io::Result<SavedAreaConfirmation> {
    confirm_saved_area_in_layout_after(kind, rect, layout, |_| Ok(()))
}

/// Installs a confirmation only after its associated settings have been saved.
#[cfg(target_os = "macos")]
pub(crate) fn confirm_saved_area_in_layout_after(
    kind: SavedAreaKind,
    rect: Rect,
    layout: CaptureLayout,
    save: impl FnOnce(&SavedAreaConfirmation) -> io::Result<()>,
) -> io::Result<SavedAreaConfirmation> {
    let (layout, confirmation) = prepare_saved_area(kind, rect, layout)?;
    let mut saved = SAVED_AREAS.lock().map_err(|_| io::Error::other("could not update saved capture coordinates"))?;
    saved.next_revision.checked_add(1).ok_or_else(|| io::Error::other("saved capture confirmation revisions were exhausted"))?;
    save(&confirmation)?;
    saved.install(kind, rect, layout)?;
    Ok(confirmation)
}

/// Restores a prior confirmation only for the exact saved rectangle and grid.
#[cfg(target_os = "macos")]
pub(crate) fn restore_saved_area(kind: SavedAreaKind, rect: Rect, confirmation: &SavedAreaConfirmation) -> io::Result<()> {
    clear_saved_area(kind)?;
    if confirmation.session_only || confirmation.version != COORDINATE_SPACE_VERSION || confirmation.kind != kind.key() || confirmation.rect != rect {
        return Err(kind.unconfirmed());
    }
    let (layout, current) = prepare_saved_area(kind, rect, CaptureLayout::current()?)?;
    if current != *confirmation {
        return Err(kind.unconfirmed());
    }
    let mut saved = SAVED_AREAS.lock().map_err(|_| io::Error::other("could not restore saved capture coordinates"))?;
    saved.install(kind, rect, layout)
}

#[cfg(target_os = "macos")]
pub(crate) fn saved_area_ticket(kind: SavedAreaKind, rect: Rect) -> io::Result<SavedAreaTicket> {
    let ticket = {
        let saved = SAVED_AREAS.lock().map_err(|_| io::Error::other("could not check saved capture coordinates"))?;
        saved.ticket(kind, rect)?
    };
    ticket.validate()?;
    Ok(ticket)
}

/// Returns the retained layout only when the exact rectangle is still confirmed.
#[cfg(target_os = "macos")]
pub(crate) fn saved_area_layout(kind: SavedAreaKind, rect: Rect) -> io::Result<CaptureLayout> {
    Ok(saved_area_ticket(kind, rect)?.layout)
}

#[cfg(target_os = "macos")]
pub(crate) fn clear_saved_area(kind: SavedAreaKind) -> io::Result<()> {
    let mut saved = SAVED_AREAS.lock().map_err(|_| io::Error::other("could not clear saved capture coordinates"))?;
    saved.entries[kind.index()] = None;
    Ok(())
}

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn invalidate_saved_area_for_test(kind: SavedAreaKind) -> io::Result<()> {
    let mut saved = SAVED_AREAS.lock().map_err(|_| io::Error::other("could not invalidate saved capture coordinates"))?;
    let entry = saved.entries[kind.index()].as_mut().ok_or_else(|| kind.unconfirmed())?;
    entry.layout = entry.layout.clone().invalidated_for_test();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_geometry_stops_capture_workflows_before_they_act() {
        let layout = CaptureLayout::current().unwrap().invalidated_for_test();
        let rect = Rect::new(0, 0, 100, 100);
        let errors = [
            layout.validate().err(),
            layout.grab(rect).err(),
            layout.grabber(rect).err(),
            layout.monitor_under_cursor().err(),
            crate::workflow::immediate_in_layout(jiffshot_capture::library::CaptureKind::Region, rect, false, &layout).err(),
            crate::modes::scrolling_in_layout(rect, &layout).err(),
            crate::modes::scrolling_horizontal_in_layout(rect, &layout).err(),
            crate::modes::scrolling_both_in_layout(rect, &layout).err(),
            crate::modes::panoramic_in_layout(rect, &layout).err(),
        ];
        for error in errors {
            assert!(error.expect("stale geometry must fail").to_string().contains("display configuration changed"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn stale_preview_geometry_rejects_cached_and_monitor_mappings() {
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let cached = layout.with_monitor(&monitor).unwrap().invalidated_for_test();
        for error in [
            cached.preview_mapping(monitor.rect).err(),
            cached.monitor_mapping(&monitor).err(),
            cached.monitor_mapping_for_rect(monitor.rect).err(),
            cached.clone().with_monitor(&monitor).err(),
        ] {
            assert!(error.expect("stale preview geometry must fail").to_string().contains("display configuration changed"));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn positioned_fixed_capture_retains_requested_output_size() {
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let (x, y) = (monitor.rect.x, monitor.rect.y);
        let (rect, fixed) = layout.fixed_at(x, y, 101, 75).unwrap();
        assert_eq!((rect.width, rect.height), layout.desktop_size_for_pixels(x, y, 101, 75).unwrap());
        assert_eq!(fixed.mapping(rect).unwrap().size(), (101, 75));
        assert!(layout.fixed_at(x, y, 0, 75).is_err());
        assert!(layout.invalidated_for_test().fixed_at(x, y, 101, 75).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn saved_areas_require_the_exact_rectangle_and_an_unchanged_layout() {
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 100);
        let mut saved = SavedAreas::default();
        assert!(saved.ticket(SavedAreaKind::Fixed, rect).err().unwrap().to_string().contains("Region options"));
        saved.install(SavedAreaKind::Fixed, rect, layout.clone()).unwrap();
        let first = saved.ticket(SavedAreaKind::Fixed, rect).unwrap();
        assert!(first.validate_layout().is_ok());
        assert!(saved.is_current(&first));
        assert!(saved.ticket(SavedAreaKind::Fixed, Rect { width: 101, ..rect }).is_err());
        assert!(saved.ticket(SavedAreaKind::Scheduled, rect).err().unwrap().to_string().contains("Time delay"));
        saved.install(SavedAreaKind::Fixed, rect, layout.invalidated_for_test()).unwrap();
        assert!(!saved.is_current(&first), "a newer confirmation cannot replace a delayed capture's ticket");
        assert!(saved.ticket(SavedAreaKind::Fixed, rect).unwrap().validate_layout().err().unwrap().to_string().contains("display configuration changed"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn saved_confirmation_restores_only_the_same_kind_rectangle_and_coordinate_space() {
        let _guard = SAVED_AREAS_TEST_LOCK.lock().unwrap();
        clear_saved_area(SavedAreaKind::Fixed).unwrap();
        clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);

        let confirmation = confirm_saved_area(SavedAreaKind::Fixed, rect).unwrap();
        let encoded = serde_json::to_string(&confirmation).unwrap();
        let confirmation: SavedAreaConfirmation = serde_json::from_str(&encoded).unwrap();
        clear_saved_area(SavedAreaKind::Fixed).unwrap();
        assert!(saved_area_layout(SavedAreaKind::Fixed, rect).is_err());
        restore_saved_area(SavedAreaKind::Fixed, rect, &confirmation).unwrap();
        let delayed = saved_area_ticket(SavedAreaKind::Fixed, rect).unwrap();
        assert!(delayed.validate().is_ok());

        clear_saved_area(SavedAreaKind::Fixed).unwrap();
        assert!(delayed.validate().is_err());
        assert!(restore_saved_area(SavedAreaKind::Scheduled, rect, &confirmation).is_err());
        assert!(restore_saved_area(SavedAreaKind::Fixed, Rect { width: 101, ..rect }, &confirmation).is_err());
        let mut wrong_version = confirmation.clone();
        wrong_version.version += 1;
        assert!(restore_saved_area(SavedAreaKind::Fixed, rect, &wrong_version).is_err());
        wrong_version.version = 1;
        assert!(restore_saved_area(SavedAreaKind::Fixed, rect, &wrong_version).is_err(), "pre-UUID confirmations require review");
        for index in [0, 3] {
            let mut wrong_space = confirmation.clone();
            wrong_space.coordinate_space[index] ^= 1;
            assert!(restore_saved_area(SavedAreaKind::Fixed, rect, &wrong_space).is_err());
            assert!(saved_area_layout(SavedAreaKind::Fixed, rect).is_err());
        }

        restore_saved_area(SavedAreaKind::Fixed, rect, &confirmation).unwrap();
        let old = saved_area_ticket(SavedAreaKind::Fixed, rect).unwrap();
        confirm_saved_area(SavedAreaKind::Fixed, rect).unwrap();
        assert!(old.validate().is_err(), "reconfirmation must not revive an old delayed capture");
        clear_saved_area(SavedAreaKind::Fixed).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn missing_display_uuid_confirms_only_the_current_session() {
        let _guard = SAVED_AREAS_TEST_LOCK.lock().unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let rect = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);

        for kind in [SavedAreaKind::Fixed, SavedAreaKind::Scheduled] {
            clear_saved_area(kind).unwrap();
            let confirmation = with_missing_uuid_for_test(|| confirm_saved_area(kind, rect)).unwrap();
            assert!(confirmation.session_only);
            assert_eq!(&confirmation.coordinate_space[3..5], &[0, 0]);
            let ticket = saved_area_ticket(kind, rect).unwrap();
            assert!(ticket.validate().is_ok());
            assert!(ticket.clone().invalidated_for_test().validate().is_err());

            let error = with_missing_uuid_for_test(|| {
                confirm_saved_area_in_layout_after(kind, rect, layout.clone(), |_| {
                    Err(io::Error::new(io::ErrorKind::PermissionDenied, "settings are read-only"))
                })
            }).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            assert!(ticket.validate().is_ok(), "a failed save must retain the previous ticket");

            clear_saved_area(kind).unwrap();
            assert!(ticket.validate().is_err());
            assert!(restore_saved_area(kind, rect, &confirmation).is_err());
            let mut tampered = confirmation.clone();
            tampered.session_only = false;
            assert!(restore_saved_area(kind, rect, &tampered).is_err(), "a later UUID must not match the missing identity");
            assert!(with_missing_uuid_for_test(|| restore_saved_area(kind, rect, &tampered)).is_err(),
                "removing the flag must not restore a still-unidentifiable display");
            assert!(saved_area_ticket(kind, rect).is_err());

            let replacement = with_missing_uuid_for_test(|| confirm_saved_area(kind, rect)).unwrap();
            assert!(replacement.session_only);
            let ticket = saved_area_ticket(kind, rect).unwrap();
            invalidate_saved_area_for_test(kind).unwrap();
            assert!(saved_area_ticket(kind, rect).is_err());
            assert!(ticket.invalidated_for_test().validate().is_err());
            clear_saved_area(kind).unwrap();
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn prepared_area_changes_commit_only_after_a_successful_save() {
        let _guard = SAVED_AREAS_TEST_LOCK.lock().unwrap();
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let old = Rect::new(monitor.rect.x, monitor.rect.y, 100, 80);
        let next = Rect::new(monitor.rect.x + 1, monitor.rect.y + 1, 101, 81);
        for kind in [SavedAreaKind::Fixed, SavedAreaKind::Scheduled] {
            clear_saved_area(kind).unwrap();
            confirm_saved_area(kind, old).unwrap();
        }
        let fixed = saved_area_ticket(SavedAreaKind::Fixed, old).unwrap();
        let scheduled = saved_area_ticket(SavedAreaKind::Scheduled, old).unwrap();
        let mut changes = SavedAreaChanges::default();
        let proof = changes.confirm(SavedAreaKind::Fixed, next).unwrap();
        changes.clear(SavedAreaKind::Scheduled);
        assert_eq!(proof.rect, next);
        assert!(fixed.validate().is_ok() && scheduled.validate().is_ok());
        assert!(saved_area_ticket(SavedAreaKind::Fixed, next).is_err());
        let error = changes.commit_after(|| Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only settings"))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(fixed.validate().is_ok() && scheduled.validate().is_ok());
        assert!(saved_area_ticket(SavedAreaKind::Fixed, next).is_err());

        let mut stale = SavedAreaChanges::default();
        stale.confirm(SavedAreaKind::Fixed, next).unwrap();
        if let Some(SavedAreaChange::Confirm { layout, .. }) = &mut stale.entries[SavedAreaKind::Fixed.index()] {
            *layout = layout.clone().invalidated_for_test();
        } else {
            panic!("the prepared confirmation is missing");
        }
        let saved = std::cell::Cell::new(false);
        assert!(stale.commit_after(|| {
            saved.set(true);
            Ok(())
        }).is_err());
        assert!(!saved.get(), "a stale layout must be rejected before writing settings");
        assert!(fixed.validate().is_ok());

        let mut changes = SavedAreaChanges::default();
        changes.confirm(SavedAreaKind::Fixed, next).unwrap();
        changes.clear(SavedAreaKind::Scheduled);
        changes.commit_after(|| Ok(())).unwrap();
        assert!(fixed.validate().is_err() && scheduled.validate().is_err());
        assert!(saved_area_ticket(SavedAreaKind::Fixed, next).is_ok());
        assert!(saved_area_ticket(SavedAreaKind::Scheduled, old).is_err());
        clear_saved_area(SavedAreaKind::Fixed).unwrap();
        clear_saved_area(SavedAreaKind::Scheduled).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn saved_confirmation_rejects_empty_oversized_and_offscreen_areas() {
        let _guard = SAVED_AREAS_TEST_LOCK.lock().unwrap();
        clear_saved_area(SavedAreaKind::Fixed).unwrap();
        clear_saved_area(SavedAreaKind::Scheduled).unwrap();
        for (kind, rect) in [
            (SavedAreaKind::Fixed, Rect::new(0, 0, 0, 80)),
            (SavedAreaKind::Fixed, Rect::new(0, 0, 16385, 80)),
            (SavedAreaKind::Scheduled, Rect::new(0, 0, 100, -1)),
            (SavedAreaKind::Scheduled, Rect::new(0, 0, 32768, 80)),
            (SavedAreaKind::Scheduled, Rect::new(i32::MAX, 0, 100, 80)),
        ] {
            assert!(confirm_saved_area(kind, rect).is_err(), "{kind:?}: {rect:?}");
            assert!(saved_area_layout(kind, rect).is_err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn full_monitor_selection_and_recording_retain_precise_edges() {
        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let expected = layout.monitor_mapping(&monitor).unwrap();
        let by_monitor = layout.clone().with_monitor(&monitor).unwrap();
        let targets = jiffshot_capture::targets::Targets::default();
        let by_selection = layout.for_selection(&crate::overlay::Selection::Monitor(monitor.rect), &targets).unwrap();
        assert_eq!(by_monitor.mapping(monitor.rect).unwrap(), expected);
        assert_eq!(by_selection.mapping(monitor.rect).unwrap(), expected);
        assert!(layout.for_selection(&crate::overlay::Selection::Monitor(Rect::new(i32::MIN, 0, 1, 1)), &targets).is_err());

        let rect = Rect::new(monitor.rect.x + 10, monitor.rect.y + 10, 202, 150);
        let bounds = [
            rect.x as f64 + 0.25,
            rect.y as f64 + 0.25,
            rect.right() as f64 + 0.25,
            rect.bottom() as f64 + 0.25,
        ];
        let initial = CaptureMapping::with_desktop_bounds(rect, 101, 75, bounds).unwrap();
        let mut precise = layout;
        precise.output = Some(OutputMapping::Reference(initial));
        let (recording, spec) = crate::recorder::recording_geometry(rect, 30, &precise).unwrap();
        assert_eq!(recording.desktop_bounds(), bounds);
        assert_eq!(recording.size(), (100, 74));
        assert_eq!((spec.width, spec.height), (100, 74));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn desktop_selection_and_recording_retain_logical_outer_edges() {
        use crate::overlay::Selection;

        let layout = CaptureLayout::current().unwrap();
        let rect = layout.virtual_screen();
        let bounds = layout.display.logical_virtual_bounds().unwrap();
        let expected = layout.display.logical_mapping(bounds).unwrap();
        let targets = jiffshot_capture::targets::Targets::default();
        for selected in [
            layout.clone().with_desktop().unwrap(),
            layout.for_selection(&Selection::Desktop(rect), &targets).unwrap(),
        ] {
            let exact = match selected.output_for(rect).unwrap() {
                OutputMapping::Logical { exact, .. } => exact,
                OutputMapping::Reference(_) => panic!("a desktop selection must retain its logical bounds"),
            };
            assert_eq!(exact, expected);
            assert_eq!(selected.mapping(rect).unwrap().desktop_bounds(), bounds.edges().map(|edge| edge * layout.desktop_scale()));
            let (recording, spec) = crate::recorder::recording_geometry(rect, 30, &selected).unwrap();
            assert_eq!(recording.desktop_bounds(), selected.mapping(rect).unwrap().desktop_bounds());
            assert_eq!(recording.size(), (spec.width, spec.height));
            assert_eq!((spec.width % 2, spec.height % 2), (0, 0));
        }
        assert!(layout.for_selection(&Selection::Desktop(Rect::new(rect.x + 1, rect.y, rect.width, rect.height)), &targets).is_err());
        assert!(layout.invalidated_for_test().with_desktop().is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn window_selection_preserves_logical_bounds_for_capture_and_recording() {
        use crate::overlay::Selection;
        use jiffshot_capture::targets::{Target, Targets};

        let layout = CaptureLayout::current().unwrap();
        let monitor = layout.monitors().into_iter().find(|monitor| monitor.primary).unwrap();
        let scale = layout.desktop_scale();
        let [left, top, _, _] = layout.monitor_mapping(&monitor).unwrap().desktop_bounds().map(|edge| edge / scale);
        let bounds = screen::LogicalBounds::new([left + 10.125, top + 10.125, left + 60.375, top + 41.875]).unwrap();
        let edges = bounds.edges().map(|edge| (edge * scale).round() as i32);
        let rect = Rect::from_ltrb(edges[0], edges[1], edges[2], edges[3]);
        let targets = Targets { list: vec![Target {
            hwnd: 1, rect, title: "Fixture".into(), class: "Fixture".into(), process_id: 1, depth: 0, parent: None,
            logical_bounds: Some(bounds),
        }] };
        let selection = Selection::Window { rect, target: 0 };
        let selected = layout.for_selection(&selection, &targets).unwrap();
        let exact = match selected.output_for(rect).unwrap() {
            OutputMapping::Logical { exact, .. } => exact,
            OutputMapping::Reference(_) => panic!("a window selection must retain its logical bounds"),
        };
        assert_eq!(exact.bounds(), bounds);
        assert_eq!(selected.mapping(rect).unwrap().desktop_bounds(), bounds.edges().map(|edge| edge * scale));
        assert_eq!(selected.preview_target_mapping(&targets.list[0]).unwrap().size(), exact.size());

        let (recording, spec) = crate::recorder::recording_geometry(rect, 30, &selected).unwrap();
        assert_eq!(recording.desktop_bounds(), selected.mapping(rect).unwrap().desktop_bounds());
        assert_eq!(recording.size(), (spec.width, spec.height));
        assert_eq!((spec.width % 2, spec.height % 2), (0, 0));
        assert!(layout.for_selection(&Selection::Window { rect, target: 1 }, &targets).is_err());
        assert!(layout.for_selection(&Selection::Window { rect: Rect::new(rect.x + 1, rect.y, rect.width, rect.height), target: 0 }, &targets).is_err());
        assert!(layout.clone().with_logical_bounds(Rect::new(rect.x + 1, rect.y, rect.width, rect.height), bounds).is_err());
        assert!(matches!(selected.clone().with_output_size(rect, 40, 30).unwrap().output_for(rect), Some(OutputMapping::Reference(_))));
        assert!(matches!(selected.with_monitor(&monitor).unwrap().output_for(monitor.rect), Some(OutputMapping::Reference(_))));
    }
}
