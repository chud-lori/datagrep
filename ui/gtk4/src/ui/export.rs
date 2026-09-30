use std::path::PathBuf;

use adw::prelude::*;
use gtk::gio;

use crate::model::ExportFormat;

#[derive(Clone)]
pub struct ExportChoice {
    pub path: PathBuf,
    pub format: ExportFormat,
    pub table: Option<String>,
}

/// Format first, then where: GtkFileDialog has no room for a format picker of its own.
pub fn choose(
    parent: &gtk::Widget,
    formats: Vec<ExportFormat>,
    suggested_name: &str,
    on_chosen: impl Fn(ExportChoice) + 'static,
) {
    let formats = if formats.is_empty() {
        vec![ExportFormat::Csv]
    } else {
        formats
    };
    let dialog = adw::AlertDialog::new(
        Some("Export Result"),
        Some("Runs the statement again and writes every row, not just the loaded ones."),
    );

    let titles: Vec<&str> = formats.iter().map(|f| f.title()).collect();
    let format_row = adw::ComboRow::builder()
        .title("Format")
        .model(&gtk::StringList::new(&titles))
        .build();
    let table_row = adw::EntryRow::builder()
        .title("Table for the INSERT statements")
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&format_row);
    group.add(&table_row);
    dialog.set_extra_child(Some(&group));

    dialog.add_response("cancel", "Cancel");
    dialog.add_response("choose", "Choose File…");
    dialog.set_response_appearance("choose", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("choose"));
    dialog.set_close_response("cancel");

    let selected = {
        let formats = formats.clone();
        move |row: &adw::ComboRow| formats[(row.selected() as usize).min(formats.len() - 1)]
    };
    let refresh = {
        let (dialog, table_row, selected) = (dialog.clone(), table_row.clone(), selected.clone());
        move |row: &adw::ComboRow| {
            let needs_table = selected(row) == ExportFormat::Sql;
            table_row.set_visible(needs_table);
            dialog.set_response_enabled(
                "choose",
                !needs_table || !table_row.text().trim().is_empty(),
            );
        }
    };
    refresh(&format_row);
    format_row.connect_selected_notify(refresh.clone());
    {
        let format_row = format_row.clone();
        table_row.connect_changed(move |_| refresh(&format_row));
    }

    let parent_window = parent.root().and_downcast::<gtk::Window>();
    let suggested_name = suggested_name.to_owned();
    let on_chosen = std::rc::Rc::new(on_chosen);
    dialog.connect_response(None, move |_, response| {
        if response != "choose" {
            return;
        }
        let format = selected(&format_row);
        let table = (format == ExportFormat::Sql).then(|| table_row.text().trim().to_owned());
        let files = gtk::FileDialog::builder()
            .title("Export Result")
            .accept_label("Export")
            .initial_name(format!("{suggested_name}.{}", format.extension()))
            .build();
        let on_chosen = on_chosen.clone();
        files.save(
            parent_window.as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                if let Some(path) = result.ok().and_then(|file| file.path()) {
                    on_chosen(ExportChoice {
                        path,
                        format,
                        table: table.clone(),
                    });
                }
            },
        );
    });
    dialog.present(Some(parent));
}
