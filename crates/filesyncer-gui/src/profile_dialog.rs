use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Profile, ProfileSet, create_profile_roots, profile};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub fn present(aw: &Rc<AppWindow>) {
    let name_row = adw::EntryRow::builder().title("Name").build();
    let id_row = adw::EntryRow::builder()
        .title("Id (used for the state database and root markers)")
        .build();

    let local = Rc::new(RefCell::new(None::<PathBuf>));
    let remote = Rc::new(RefCell::new(None::<PathBuf>));

    let local_row = adw::ActionRow::builder()
        .title("Local folder")
        .subtitle("Not chosen")
        .build();
    let remote_row = adw::ActionRow::builder()
        .title("Remote folder")
        .subtitle("Not chosen")
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&name_row);
    group.add(&id_row);
    group.add(&local_row);
    group.add(&remote_row);

    let dialog = adw::AlertDialog::new(Some("Add profile"), None);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));

    attach_chooser(&local_row, &local, "Choose local folder", aw);
    attach_chooser(&remote_row, &remote, "Choose remote folder", aw);

    {
        let aw = aw.clone();
        let local = local.clone();
        let remote = remote.clone();
        dialog.connect_response(None, move |dialog, response| {
            if response != "add" {
                return;
            }
            match build_and_save(&id_row.text(), &name_row.text(), &local, &remote) {
                Ok(id) => {
                    aw.reload_profiles();
                    let ok = adw::AlertDialog::new(
                        Some("Profile added"),
                        Some(&format!("Markers written to both roots for `{id}`.")),
                    );
                    ok.add_response("ok", "OK");
                    ok.present(Some(&aw.window));
                }
                Err(message) => {
                    // Keep the dialog's data by reporting the failure separately.
                    let err = adw::AlertDialog::new(Some("Could not add profile"), Some(&message));
                    err.add_response("ok", "OK");
                    err.present(Some(&aw.window));
                }
            }
            let _ = dialog;
        });
    }

    dialog.present(Some(&aw.window));
}

fn attach_chooser(
    row: &adw::ActionRow,
    slot: &Rc<RefCell<Option<PathBuf>>>,
    title: &str,
    aw: &Rc<AppWindow>,
) {
    let button = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&button);

    let slot = slot.clone();
    let row = row.clone();
    let window = aw.window.clone();
    let title = title.to_string();
    button.connect_clicked(move |_| {
        let chooser = gtk::FileDialog::builder().title(&title).build();
        let slot = slot.clone();
        let row = row.clone();
        chooser.select_folder(
            Some(&window),
            None::<&gtk::gio::Cancellable>,
            move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        row.set_subtitle(&path.display().to_string());
                        *slot.borrow_mut() = Some(path);
                    }
                }
            },
        );
    });
}

fn build_and_save(
    id: &str,
    name: &str,
    local: &Rc<RefCell<Option<PathBuf>>>,
    remote: &Rc<RefCell<Option<PathBuf>>>,
) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("An id is required.".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Ids may contain only letters, digits, hyphens, and underscores.".into());
    }
    let local = local.borrow().clone().ok_or("Choose a local folder.")?;
    let remote = remote.borrow().clone().ok_or("Choose a remote folder.")?;

    let config = profile::config_path();
    let mut set = ProfileSet::load(&config).map_err(|e| e.to_string())?;
    if set.get(id).is_some() {
        return Err(format!("A profile with id `{id}` already exists."));
    }

    let new_profile = Profile {
        id: id.to_string(),
        name: if name.trim().is_empty() {
            id.to_string()
        } else {
            name.trim().to_string()
        },
        local,
        remote,
    };

    // Validates the roots (existence, nesting) and stamps the markers.
    // Nothing is written to the config file unless this succeeds.
    create_profile_roots(&new_profile).map_err(|e| e.to_string())?;

    set.profiles.push(new_profile);
    set.save(&config).map_err(|e| e.to_string())?;
    Ok(id.to_string())
}
