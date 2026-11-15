//! Shared Devolutions DarkBlue / Light surface tokens, primary/secondary
//! button styles, and dialog chrome helpers used across Preferences, editor
//! dialogs, and notifications.

use super::capture_view::parts;
use super::dark_blue;
use rustolonia::{
    AppScope, Brush, Color, Effect, El, FontWeight, Result, Signal, Style, StyleSheet,
    StyledElement, ThemeVariant, Upcast, Window,
};
use std::cell::Cell;
use std::time::Duration;

/// Semantic colors for a themed Jiffshot window or dialog.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub window: u32,
    pub panel: u32,
    pub card: u32,
    pub hover: u32,
    pub pressed: u32,
    pub border: u32,
    pub divider: u32,
    pub input_border: u32,
    pub strong_border: u32,
    pub text: u32,
    pub secondary_text: u32,
    pub muted_text: u32,
    pub accent: u32,
    pub accent_hover: u32,
    pub accent_pressed: u32,
    pub accent_text: u32,
}

impl Surface {
    pub const fn for_theme(dark: bool) -> Self {
        if dark {
            Self {
                window: dark_blue::windows_or(dark_blue::WINDOW, 0xFF1B1C1F),
                panel: dark_blue::windows_or(dark_blue::PANEL, 0xFF232428),
                card: dark_blue::windows_or(dark_blue::CARD, 0xFF26282C),
                hover: dark_blue::windows_or(dark_blue::HOVER, 0xFF2C2E33),
                pressed: dark_blue::windows_or(0xFF19202B, 0xFF202226),
                border: dark_blue::windows_or(dark_blue::BORDER, 0xFF2E3034),
                divider: dark_blue::windows_or(0xFF262B36, 0xFF2C2E33),
                input_border: dark_blue::windows_or(dark_blue::INPUT_BORDER, 0xFF3A3C40),
                strong_border: dark_blue::windows_or(dark_blue::STRONG_BORDER, 0xFF484C52),
                text: dark_blue::windows_or(dark_blue::TEXT, 0xFFF2F3F5),
                secondary_text: dark_blue::windows_or(dark_blue::SECONDARY_TEXT, 0xFFC8CACE),
                muted_text: dark_blue::windows_or(dark_blue::MUTED_TEXT, 0xFF8A8F98),
                accent: dark_blue::windows_or(dark_blue::ACCENT, 0xFF2F5DA8),
                accent_hover: dark_blue::windows_or(0xFF1E8CF0, 0xFF3A6EC0),
                accent_pressed: dark_blue::windows_or(0xFF086AC2, 0xFF264D8E),
                accent_text: dark_blue::windows_or(dark_blue::ACCENT_TEXT, 0xFF3D8BFF),
            }
        } else {
            Self {
                window: 0xFFF6F7F9,
                panel: 0xFFF0F2F5,
                card: 0xFFFFFFFF,
                hover: 0xFFF2F4F7,
                pressed: 0xFFE4E7EC,
                border: 0xFFD9DCE3,
                divider: 0xFFEAECF0,
                input_border: 0xFFC6CBD4,
                strong_border: 0xFF98A2B3,
                text: 0xFF101828,
                secondary_text: 0xFF344054,
                muted_text: 0xFF667085,
                accent: 0xFF0068C3,
                accent_hover: 0xFF0058A6,
                accent_pressed: 0xFF004887,
                accent_text: 0xFF0068C3,
            }
        }
    }
}

pub fn argb_color(argb: u32) -> Color {
    Color::from(argb)
}

pub fn solid(argb: u32) -> Brush {
    Brush::solid(Color::from(argb))
}

/// Visual button roles following RDM and UniGetUI's primary/secondary hierarchy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonRole {
    Primary,
    Secondary,
    Compact,
    Link,
}

impl ButtonRole {
    pub const fn class_pair(self) -> (&'static str, &'static str) {
        match self {
            ButtonRole::Primary => ("jiffshot-btn-primary-dark", "jiffshot-btn-primary-light"),
            ButtonRole::Secondary => (
                "jiffshot-btn-secondary-dark",
                "jiffshot-btn-secondary-light",
            ),
            ButtonRole::Compact => ("jiffshot-btn-compact-dark", "jiffshot-btn-compact-light"),
            ButtonRole::Link => ("jiffshot-btn-link-dark", "jiffshot-btn-link-light"),
        }
    }
}

thread_local! {
    static STYLES_INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Builds the application-wide stylesheet for primary, secondary, compact, and
/// link buttons, plus compact Preferences tabs.
pub fn build_stylesheet() -> StyleSheet {
    let mut rules = Vec::new();
    let fade = Duration::from_millis(90);

    for (dark, suffix) in [(true, "dark"), (false, "light")] {
        let s = Surface::for_theme(dark);
        let white = argb_color(0xFFFFFFFF);

        let primary = format!("jiffshot-btn-primary-{suffix}");
        rules.push(
            Style::control("Button")
                .class(&primary)
                .min_width(96.0)
                .height(32.0)
                .padding((16.0, 5.0))
                .corner_radius(4.0)
                .border_thickness(1.0)
                .font_size(13.0)
                .font_weight(FontWeight::DemiBold)
                .background(argb_color(s.accent))
                .border_brush(argb_color(s.accent_hover))
                .foreground(white)
                .transition_background(fade)
                .transition_border_brush(fade),
        );
        rules.push(
            Style::control("Button")
                .class(&primary)
                .hover()
                .background(argb_color(s.accent_hover))
                .border_brush(argb_color(s.accent_hover))
                .foreground(white),
        );
        rules.push(
            Style::control("Button")
                .class(&primary)
                .pressed()
                .background(argb_color(s.accent_pressed))
                .border_brush(argb_color(s.accent_pressed))
                .foreground(white),
        );
        rules.push(
            Style::control("Button")
                .class(&primary)
                .disabled()
                .opacity(0.45),
        );

        let secondary = format!("jiffshot-btn-secondary-{suffix}");
        rules.push(
            Style::control("Button")
                .class(&secondary)
                .min_width(96.0)
                .height(32.0)
                .padding((16.0, 5.0))
                .corner_radius(4.0)
                .border_thickness(1.0)
                .font_size(13.0)
                .background(argb_color(s.card))
                .border_brush(argb_color(s.input_border))
                .foreground(argb_color(s.text))
                .transition_background(fade)
                .transition_border_brush(fade),
        );
        rules.push(
            Style::control("Button")
                .class(&secondary)
                .hover()
                .background(argb_color(s.hover))
                .border_brush(argb_color(s.strong_border))
                .foreground(argb_color(s.text)),
        );
        rules.push(
            Style::control("Button")
                .class(&secondary)
                .pressed()
                .background(argb_color(s.pressed))
                .border_brush(argb_color(s.strong_border))
                .foreground(argb_color(s.text)),
        );
        rules.push(
            Style::control("Button")
                .class(&secondary)
                .disabled()
                .opacity(0.45),
        );

        let compact = format!("jiffshot-btn-compact-{suffix}");
        rules.push(
            Style::control("Button")
                .class(&compact)
                .min_height(28.0)
                .height(28.0)
                .padding((12.0, 4.0))
                .corner_radius(4.0)
                .border_thickness(1.0)
                .font_size(12.0)
                .background(argb_color(s.card))
                .border_brush(argb_color(s.input_border))
                .foreground(argb_color(s.text))
                .transition_background(fade)
                .transition_border_brush(fade),
        );
        rules.push(
            Style::control("Button")
                .class(&compact)
                .hover()
                .background(argb_color(s.hover))
                .border_brush(argb_color(s.strong_border))
                .foreground(argb_color(s.text)),
        );
        rules.push(
            Style::control("Button")
                .class(&compact)
                .pressed()
                .background(argb_color(s.pressed))
                .border_brush(argb_color(s.strong_border))
                .foreground(argb_color(s.text)),
        );
        rules.push(
            Style::control("Button")
                .class(&compact)
                .disabled()
                .opacity(0.45),
        );

        let link = format!("jiffshot-btn-link-{suffix}");
        rules.push(
            Style::control("Button")
                .class(&link)
                .height(30.0)
                .padding((12.0, 4.0))
                .corner_radius(4.0)
                .border_thickness(1.0)
                .font_size(12.5)
                .background(argb_color(s.card))
                .border_brush(argb_color(s.border))
                .foreground(argb_color(s.accent_text))
                .transition_background(fade)
                .transition_border_brush(fade),
        );
        rules.push(
            Style::control("Button")
                .class(&link)
                .hover()
                .background(argb_color(s.hover))
                .border_brush(argb_color(s.accent_text))
                .foreground(argb_color(s.accent_text)),
        );
        rules.push(
            Style::control("Button")
                .class(&link)
                .pressed()
                .background(argb_color(s.pressed))
                .border_brush(argb_color(s.accent_text))
                .foreground(argb_color(s.accent_text)),
        );
    }

    rules.push(
        Style::control("TabItem")
            .class("jiffshot-prefs-tab")
            .font_size(13.5)
            .font_weight(FontWeight::Normal)
            .min_height(36.0)
            .padding((14.0, 6.0)),
    );
    rules.push(
        Style::control("TabItem")
            .class("jiffshot-prefs-tab")
            .selected()
            .font_weight(FontWeight::DemiBold),
    );

    StyleSheet::from(rules)
}

/// Ensures the shared button/tab stylesheet is installed once on `scope`.
pub fn ensure_styles(scope: &AppScope) -> Result<()> {
    let already = STYLES_INSTALLED.with(|installed| installed.replace(true));
    if !already {
        if let Err(error) = scope.apply_styles(build_stylesheet()) {
            STYLES_INSTALLED.with(|installed| installed.set(false));
            return Err(error);
        }
    }
    Ok(())
}

/// Applies a theme-following button role class to an imperative control.
pub fn apply_role_to_control(
    control: &impl Upcast<StyledElement>,
    dark: bool,
    role: ButtonRole,
) -> Result<()> {
    let (dark_cls, light_cls) = role.class_pair();
    let (want, drop) = if dark {
        (dark_cls, light_cls)
    } else {
        (light_cls, dark_cls)
    };
    let classes = control.upcast_ref().classes()?;
    let _ = classes.remove_value(drop);
    if !classes.contains(want)? {
        classes.add(want)?;
    }
    Ok(())
}

/// Attaches a reactive theme-aware [`ButtonRole`] to a declarative element.
pub fn with_role<T>(el: El<T>, dark: Signal<bool>, role: ButtonRole) -> El<T>
where
    T: Upcast<StyledElement> + Clone + 'static,
{
    el.with_op(move |scope, control| {
        ensure_styles(scope)?;
        let target = control.clone();
        Effect::new(move || apply_role_to_control(&target, dark.get(), role))?;
        Ok(())
    })
}

/// Styles `window` with Jiffshot's Devolutions surface background, shared
/// button styles, and Windows DWM caption colors, updating `dark` on live
/// theme switches.
pub fn bind_window_theme(scope: &AppScope, window: &Window, dark: Signal<bool>) -> Result<()> {
    ensure_styles(scope)?;
    let initial = parts::dark_theme(scope);
    dark.set_if_changed(initial);
    window.set_background(solid(Surface::for_theme(initial).window))?;

    #[cfg(windows)]
    {
        let app = scope.clone();
        let target = super::UiOnly::new(window.clone());
        window.clone().on_loaded(scope, move |_| {
            super::native_theme::apply(&app, target.get());
        })?;
    }

    let app = scope.clone();
    let target = super::UiOnly::new(window.clone());
    window
        .clone()
        .on_actual_theme_variant_changed(scope, move |_| {
            let is_dark = !matches!(app.actual_theme_variant(), Ok(ThemeVariant::Light));
            dark.set_if_changed(is_dark);
            let _ = target
                .get()
                .set_background(solid(Surface::for_theme(is_dark).window));
            #[cfg(windows)]
            super::native_theme::apply(&app, target.get());
        })?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stylesheet_includes_primary_secondary_and_tab_rules() {
        let json = build_stylesheet().to_json();
        assert!(json.contains("jiffshot-btn-primary-dark"));
        assert!(json.contains("jiffshot-btn-primary-light"));
        assert!(json.contains("jiffshot-btn-secondary-dark"));
        assert!(json.contains("jiffshot-btn-compact-dark"));
        assert!(json.contains("jiffshot-btn-link-dark"));
        assert!(json.contains("jiffshot-prefs-tab"));
    }

    #[test]
    fn dark_and_light_surfaces_maintain_contrast_hierarchy() {
        let dark = Surface::for_theme(true);
        let light = Surface::for_theme(false);
        assert_ne!(dark.window, dark.card);
        assert_ne!(light.window, light.card);
        assert_ne!(dark.accent, dark.card);
        assert_ne!(light.accent, light.card);
    }
}
