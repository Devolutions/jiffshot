//! Linux template actions backed by the shared document editor.

use jiffshot_doc::combine::{self, Options};
use jiffshot_doc::editor::Editor;
use jiffshot_doc::object::Kind;
use jiffshot_doc::{Document, Pixmap};

pub(super) fn create_template(
    layout: i32,
    theme: i32,
    captions: bool,
    numbers: bool,
    footer: &str,
) -> Result<Document, String> {
    let name = usize::try_from(layout)
        .ok()
        .and_then(|index| combine::TEMPLATES.get(index))
        .ok_or_else(|| "select a template layout".to_owned())?;
    let theme = usize::try_from(theme)
        .ok()
        .and_then(|index| combine::THEMES.get(index))
        .ok_or_else(|| "select a template theme".to_owned())?;
    let options = Options {
        theme,
        captions,
        numbers,
        footer: (!footer.trim().is_empty()).then(|| footer.to_owned()),
    };
    combine::template_with(name, &[], &options)
        .ok_or_else(|| format!("could not create template: {name}"))
}

pub(super) fn selected_zone(editor: &Editor) -> Result<u64, String> {
    let object = editor
        .selected()
        .filter(|object| object.zone.is_some() && matches!(object.kind, Kind::Image { .. }))
        .ok_or_else(|| "select a template image zone first".to_owned())?;
    Ok(object.id)
}

pub(super) fn fill_selected_zone(editor: &mut Editor, image: Pixmap) -> Result<(), String> {
    let id = selected_zone(editor)?;
    editor.fill_zone(id, image);
    Ok(())
}

pub(super) fn set_selected_zone_fit(editor: &mut Editor, fit: bool) -> Result<(), String> {
    selected_zone(editor)?;
    editor.set_zone_fit(fit);
    Ok(())
}

pub(super) fn clear_selected_zone(editor: &mut Editor) -> Result<(), String> {
    selected_zone(editor)?;
    editor.empty_zone();
    Ok(())
}

pub(super) fn swap_selected_zones(editor: &mut Editor) -> Result<(), String> {
    let zones: Vec<_> = editor
        .selected_ids()
        .into_iter()
        .filter_map(|id| editor.document().get(id))
        .filter(|object| object.zone.is_some())
        .collect();
    if zones.len() == 2
        && zones
            .iter()
            .all(|object| matches!(object.kind, Kind::Image { .. }))
        && editor.swap_zones()
    {
        Ok(())
    } else {
        Err("select exactly two template image zones to swap".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiffshot_doc::object::{Kind, ShapeKind, Style, Zone};
    use jiffshot_doc::{Box2, Rgba};

    fn zone_ids(editor: &Editor) -> Vec<u64> {
        editor
            .document()
            .objects
            .iter()
            .filter(|object| object.zone.is_some())
            .map(|object| object.id)
            .collect()
    }

    #[test]
    fn each_layout_has_editable_zones_and_each_theme_is_applied() {
        for (index, expected) in [2, 3, 4, 1, 4, 3].into_iter().enumerate() {
            let document = create_template(index as i32, 0, false, false, "")
                .expect("listed template should be created");
            let zones: Vec<_> = document
                .objects
                .iter()
                .filter(|object| object.zone.is_some())
                .collect();
            assert_eq!(
                zones.len(),
                expected,
                "layout {}",
                combine::TEMPLATES[index]
            );
            assert!(zones
                .iter()
                .all(|zone| matches!(zone.kind, Kind::Image { .. })));
        }
        for (index, theme) in combine::THEMES.iter().enumerate() {
            let document = create_template(3, index as i32, false, false, "")
                .expect("listed theme should be accepted");
            let pixel = document
                .background()
                .pixel(0, document.height() - 1)
                .expect("template background should contain the sample")
                .demultiply();
            assert_eq!(
                [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()],
                [
                    theme.background.r,
                    theme.background.g,
                    theme.background.b,
                    theme.background.a
                ],
                "theme {}",
                theme.name
            );
        }
        assert!(create_template(-1, 0, false, false, "").is_err());
        assert!(create_template(99, 0, false, false, "").is_err());
        assert!(create_template(0, -1, false, false, "").is_err());
        assert!(create_template(0, 99, false, false, "").is_err());
    }

    #[test]
    fn captions_numbers_and_footer_are_editable_template_objects() {
        let plain =
            create_template(0, 0, false, false, "").expect("default template should be created");
        let document = create_template(0, 2, true, true, "Reviewed by team")
            .expect("configured template should be created");
        assert!(document.height() > plain.height());
        let text: Vec<_> = document
            .objects
            .iter()
            .filter_map(|object| object.text())
            .collect();
        assert!(text.contains(&"Caption 1"));
        assert!(text.contains(&"Caption 2"));
        assert!(text.contains(&"Reviewed by team"));
        assert_eq!(
            document
                .objects
                .iter()
                .filter(|object| matches!(object.kind, Kind::Step { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn zones_can_be_filled_fitted_cleared_swapped_and_undone() {
        let document = create_template(0, 0, false, false, "").expect("template should be created");
        let mut editor = Editor::new(document);
        let zones = zone_ids(&editor);
        assert!(fill_selected_zone(&mut editor, Pixmap::new(1, 1).unwrap()).is_err());
        assert!(set_selected_zone_fit(&mut editor, true).is_err());
        assert!(clear_selected_zone(&mut editor).is_err());
        assert!(swap_selected_zones(&mut editor).is_err());

        editor.select(Some(zones[0]));
        let image = jiffshot_doc::raster::solid(4, 3, Rgba::RED).expect("image should be valid");
        fill_selected_zone(&mut editor, image).expect("selected zone should accept an image");
        assert!(matches!(
            &editor.selected().expect("zone remains selected").kind,
            Kind::Image { image, .. } if image.pixmap.as_ref().is_some_and(|p| p.width() == 4)
        ));
        set_selected_zone_fit(&mut editor, true).expect("fit should apply");
        assert!(editor.selected().unwrap().zone.unwrap().fit);
        assert!(editor.undo(), "fit should be undoable");
        assert!(!editor.selected().unwrap().zone.unwrap().fit);

        editor.toggle_selected(zones[1]);
        swap_selected_zones(&mut editor).expect("two selected zones should swap");
        let second = editor.document().get(zones[1]).expect("second zone exists");
        assert!(matches!(
            &second.kind,
            Kind::Image { image, .. } if image.pixmap.as_ref().is_some_and(|p| p.width() == 4)
        ));
        assert!(editor.undo(), "swap should be undoable");
        editor.select(Some(zones[0]));
        clear_selected_zone(&mut editor).expect("selected zone should clear");
        assert!(matches!(
            &editor.selected().unwrap().kind,
            Kind::Image { image, .. } if image.pixmap.is_none()
        ));
        assert!(editor.undo(), "clearing a zone should be undoable");
        assert!(matches!(
            &editor.selected().unwrap().kind,
            Kind::Image { image, .. } if image.pixmap.is_some()
        ));
        let bytes = jiffshot_doc::io::document_bytes(editor.document())
            .expect("template should save as an editable document");
        let restored =
            jiffshot_doc::io::document_from_bytes(bytes).expect("saved template should reopen");
        assert_eq!(restored.objects, editor.document().objects);
        let Kind::Image { image, .. } = &restored
            .get(zones[0])
            .expect("filled zone should remain")
            .kind
        else {
            panic!("saved zone must still be an image");
        };
        let pixmap = image
            .pixmap
            .as_ref()
            .expect("saved zone should retain its image pixels");
        assert_eq!((pixmap.width(), pixmap.height()), (4, 3));
        assert_eq!(pixmap.pixel(0, 0), Some(Rgba::RED.premultiplied()));
    }

    #[test]
    fn malformed_zone_is_rejected_without_panicking_or_mutating_the_document() {
        let document = create_template(0, 0, false, false, "").expect("template should be created");
        let mut editor = Editor::new(document);
        let first = zone_ids(&editor)[0];
        let malformed = editor.add_object(
            "Malformed zone",
            Kind::Shape {
                rect: Box2::from_xywh(0.0, 0.0, 20.0, 20.0),
                shape: ShapeKind::Rectangle,
            },
            Style::default(),
        );
        editor.edit(malformed, "Attach invalid zone", None, |object| {
            object.zone = Some(Zone::default());
        });
        editor.set_selection(vec![malformed, first]);
        let before = editor.document().objects.clone();
        assert!(selected_zone(&editor).is_err());
        assert!(swap_selected_zones(&mut editor).is_err());
        assert_eq!(editor.document().objects, before);
    }
}
