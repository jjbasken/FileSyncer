//! The plan review: each listed path side by side, local on the left and
//! remote on the right, with what will happen to it in between. Every path's
//! action can be changed, one at a time or for a selection.

use crate::run_view;
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::eta::is_work;
use filesyncer_core::{
    Choice, ConflictKind, Plan, Session, Side, SkipReason, Stat, approx_duration,
};
use std::cell::RefCell;
use std::rc::Rc;

/// What every cell needs to read the plan, change it, and show the change.
struct Review {
    session: Rc<Session>,
    plan: RefCell<Plan>,
    /// The cells currently bound to a path. Only visible rows are bound, so
    /// this stays small however long the plan is.
    bound: RefCell<Vec<(String, Cell)>>,
    total: adw::ActionRow,
    apply: gtk::Button,
}

#[derive(Clone)]
enum Cell {
    Side(Side, gtk::Label, gtk::Label),
    Action(gtk::MenuButton),
}

impl Cell {
    fn widget(&self) -> gtk::Widget {
        match self {
            Cell::Side(_, name, _) => name.parent().expect("a side cell's labels are packed"),
            Cell::Action(button) => button.clone().upcast(),
        }
    }
}

pub fn show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan) {
    let total = adw::ActionRow::builder().title("Total").build();
    let apply = gtk::Button::builder()
        .label("Apply")
        .css_classes(vec!["suggested-action".to_string()])
        .build();
    let review = Rc::new(Review {
        session: session.clone(),
        plan: RefCell::new(plan),
        bound: RefCell::new(Vec::new()),
        total,
        apply: apply.clone(),
    });
    review.update_total();

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let listed = review.plan.borrow().listed.len();
    if listed == 0 {
        column.append(
            &adw::StatusPage::builder()
                .icon_name("object-select-symbolic")
                .title("Everything is in sync")
                .description(if review.plan.borrow().actions.is_empty() {
                    "No changes are needed."
                } else {
                    "Save sync state to remember this comparison for the next scan."
                })
                .vexpand(true)
                .build(),
        );
    } else {
        let summary = adw::PreferencesGroup::new();
        summary.add(&review.total);
        column.append(&summary);

        let paths: Vec<String> = review.plan.borrow().listed.keys().cloned().collect();
        let model = gtk::StringList::new(&paths.iter().map(String::as_str).collect::<Vec<_>>());
        let selection = gtk::MultiSelection::new(Some(model));
        column.append(&bulk_bar(&review, &selection));
        column.append(&roots(&review.session));
        column.append(&table(&review, &selection));
    }

    if let Some(skipped) = skipped_group(&review.plan.borrow()) {
        column.append(&skipped);
    }

    let cancel = gtk::Button::with_label("Cancel");
    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&cancel);
    buttons.append(&apply);
    column.append(&buttons);

    {
        let aw = aw.clone();
        cancel.connect_clicked(move |_| aw.show_placeholder());
    }
    {
        let aw = aw.clone();
        let review = review.clone();
        apply.connect_clicked(move |_| {
            run_view::show(&aw, session.clone(), review.plan.borrow().clone());
        });
    }

    aw.show_content(&column, "Review plan");
}

/// The three-column comparison: local file, action, remote file.
fn table(review: &Rc<Review>, selection: &gtk::MultiSelection) -> gtk::ScrolledWindow {
    let view = gtk::ColumnView::builder()
        .model(selection)
        .show_column_separators(true)
        .css_classes(vec!["data-table".to_string()])
        .build();

    let local = gtk::ColumnViewColumn::new(Some("Local"), Some(side_factory(review, Side::Local)));
    local.set_expand(true);
    let action = gtk::ColumnViewColumn::new(Some("Action"), Some(action_factory(review)));
    // Fixed, so the side columns do not shift as actions change.
    action.set_fixed_width(150);
    let remote =
        gtk::ColumnViewColumn::new(Some("Remote"), Some(side_factory(review, Side::Remote)));
    remote.set_expand(true);
    view.append_column(&local);
    view.append_column(&action);
    view.append_column(&remote);

    // Never scrolling sideways makes the side columns share the width, and
    // their labels ellipsize to fit it.
    gtk::ScrolledWindow::builder()
        .child(&view)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build()
}

/// The two roots, over the columns that show them.
fn roots(session: &Session) -> gtk::Box {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .homogeneous(true)
        .build();
    let profile = session.profile();
    for path in [&profile.local, &profile.remote] {
        // Ellipsized at the start, keeping the folder names that tell the
        // roots apart.
        let label = gtk::Label::builder()
            .label(path.display().to_string())
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .tooltip_text(path.display().to_string())
            .css_classes(vec!["dim-label".to_string(), "caption".to_string()])
            .build();
        row.append(&label);
    }
    row
}

/// A cell showing one side's file: its path, and its size and date.
fn side_factory(review: &Rc<Review>, side: Side) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .width_chars(12)
            .build();
        let detail = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(vec!["dim-label".to_string(), "caption".to_string()])
            .build();
        let cell = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        cell.append(&name);
        cell.append(&detail);
        list_item(item).set_child(Some(&cell));
    });
    let r = review.clone();
    factory.connect_bind(move |_, item| {
        let cell = list_item(item).child().and_downcast::<gtk::Box>().unwrap();
        let name = cell.first_child().and_downcast::<gtk::Label>().unwrap();
        let detail = cell.last_child().and_downcast::<gtk::Label>().unwrap();
        r.bind(path_of(item), Cell::Side(side, name, detail));
    });
    let r = review.clone();
    factory.connect_unbind(move |_, item| r.unbind(list_item(item).child().as_ref()));
    factory
}

/// A cell holding the button that shows and changes a path's action.
fn action_factory(review: &Rc<Review>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let button = gtk::MenuButton::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(vec!["flat".to_string()])
            .build();
        list_item(item).set_child(Some(&button));
    });
    let r = review.clone();
    factory.connect_bind(move |_, item| {
        let button = list_item(item)
            .child()
            .and_downcast::<gtk::MenuButton>()
            .unwrap();
        r.bind(path_of(item), Cell::Action(button));
    });
    let r = review.clone();
    factory.connect_unbind(move |_, item| r.unbind(list_item(item).child().as_ref()));
    factory
}

/// Buttons that set one action for every selected row it applies to.
fn bulk_bar(review: &Rc<Review>, selection: &gtk::MultiSelection) -> gtk::Box {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    bar.append(
        &gtk::Label::builder()
            .label("For selected rows:")
            .css_classes(vec!["dim-label".to_string()])
            .build(),
    );

    // A row where an action does not apply keeps its own.
    type Pick = fn(&[Choice]) -> Option<Choice>;
    let picks: [(Option<&str>, &str, Pick); 4] = [
        (None, "Copy →", |c| {
            c.iter().copied().find(|&x| x == Choice::Copy(Side::Local))
        }),
        (None, "← Copy", |c| {
            c.iter().copied().find(|&x| x == Choice::Copy(Side::Remote))
        }),
        (Some("user-trash-symbolic"), "Delete", |c| {
            c.iter().copied().find(|x| matches!(x, Choice::Trash(_)))
        }),
        (Some("action-unavailable-symbolic"), "Skip", |_| {
            Some(Choice::Skip)
        }),
    ];
    for (icon, label, pick) in picks {
        let button = gtk::Button::builder()
            .child(&icon_label(icon, label))
            .build();
        let review = review.clone();
        let selection = selection.clone();
        button.connect_clicked(move |_| {
            let selected = selection.selection();
            let paths: Vec<String> = (0..selected.size())
                .filter_map(|i| selection.item(selected.nth(i as u32)))
                .filter_map(|o| o.downcast::<gtk::StringObject>().ok())
                .map(|s| s.string().to_string())
                .collect();
            for path in paths {
                let choice = pick(&review.plan.borrow().choices(&path));
                if let Some(choice) = choice {
                    review.choose(&path, choice);
                }
            }
        });
        bar.append(&button);
    }
    bar
}

impl Review {
    fn bind(self: &Rc<Self>, path: String, cell: Cell) {
        self.render(&path, &cell);
        self.bound.borrow_mut().push((path, cell));
    }

    fn unbind(&self, widget: Option<&gtk::Widget>) {
        if let Some(widget) = widget {
            self.bound
                .borrow_mut()
                .retain(|(_, cell)| &cell.widget() != widget);
        }
    }

    fn choose(self: &Rc<Self>, path: &str, choice: Choice) {
        // The menus only offer open choices, so this cannot fail.
        if self.plan.borrow_mut().set_choice(path, choice).is_err() {
            return;
        }
        let cells: Vec<Cell> = self
            .bound
            .borrow()
            .iter()
            .filter(|(p, _)| p == path)
            .map(|(_, c)| c.clone())
            .collect();
        for cell in cells {
            self.render(path, &cell);
        }
        self.update_total();
    }

    fn render(self: &Rc<Self>, path: &str, cell: &Cell) {
        let plan = self.plan.borrow();
        let Some(listed) = plan.listed.get(path) else {
            return;
        };
        let choice = plan.choice(path);
        match cell {
            Cell::Side(side, name, detail) => {
                match listed.stat(*side) {
                    Some(stat) => {
                        name.set_label(path);
                        name.set_tooltip_text(Some(path));
                        detail.set_label(&describe_stat(stat));
                    }
                    None => {
                        name.set_label("");
                        name.set_tooltip_text(None);
                        detail.set_label("");
                    }
                }
                // The file this run moves to the trash is shown in red.
                if choice == Choice::Trash(*side) {
                    name.add_css_class("error");
                } else {
                    name.remove_css_class("error");
                }
            }
            Cell::Action(button) => {
                let (icon, label) = match (choice, listed.conflict) {
                    (Choice::Skip, Some(_)) => (Some("dialog-warning-symbolic"), "Conflict"),
                    _ => choice_look(choice),
                };
                button.set_child(Some(&icon_label(icon, label)));
                button.set_tooltip_text(listed.conflict.map(describe));
                button.set_popover(Some(&self.menu(path, &plan.choices(path))));
            }
        }
    }

    /// The popover listing a path's open choices.
    fn menu(self: &Rc<Self>, path: &str, choices: &[Choice]) -> gtk::Popover {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let popover = gtk::Popover::builder().child(&list).build();
        for &choice in choices {
            let button = gtk::Button::builder()
                .child(&icon_label(choice_look(choice).0, choice_text(choice)))
                .css_classes(vec!["flat".to_string()])
                .build();
            let review = Rc::downgrade(self);
            let path = path.to_string();
            let popover = popover.downgrade();
            button.connect_clicked(move |_| {
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
                if let Some(review) = review.upgrade() {
                    review.choose(&path, choice);
                }
            });
            list.append(&button);
        }
        popover
    }

    fn update_total(&self) {
        let plan = self.plan.borrow();
        let files = plan.actions.iter().filter(|a| is_work(a)).count();
        let bytes = plan.copy_bytes(Side::Local) + plan.copy_bytes(Side::Remote);
        let mut text = if files == 0 {
            "Nothing to do".to_string()
        } else {
            // An unreadable rate only costs the estimate, not the review.
            let time = match self.session.estimate(&plan).ok().flatten() {
                Some(t) => approx_duration(t),
                None => "time unknown until a sync has measured the speed".to_string(),
            };
            format!("{files} files · {} · {time}", human_bytes(bytes))
        };
        let conflicts = plan.conflicts.len();
        let skipped = plan.listed.len() - files - conflicts;
        if skipped > 0 {
            text.push_str(&format!(" · {skipped} skipped"));
        }
        if conflicts > 0 {
            let noun = if conflicts == 1 {
                "conflict"
            } else {
                "conflicts"
            };
            text.push_str(&format!(" · {conflicts} {noun} left as is"));
        }
        self.total.set_subtitle(&text);
        self.apply.set_label(if plan.is_noop() {
            "Save sync state"
        } else {
            "Apply"
        });
        self.apply.set_sensitive(!plan.actions.is_empty());
    }
}

/// Symlinks and unreadable entries, which are never synced.
fn skipped_group(plan: &Plan) -> Option<adw::PreferencesGroup> {
    if plan.skips.is_empty() {
        return None;
    }
    let expander = adw::ExpanderRow::builder()
        .title("Not synced")
        .subtitle(format!("{} entries", plan.skips.len()))
        .build();
    for skip in &plan.skips {
        let why = match &skip.reason {
            SkipReason::Symlink => "symlink",
            SkipReason::Unreadable(why) => why,
        };
        expander.add_row(
            &adw::ActionRow::builder()
                .title(&skip.rel_path)
                .subtitle(why)
                .build(),
        );
    }
    let group = adw::PreferencesGroup::new();
    group.add(&expander);
    Some(group)
}

fn list_item(item: &gtk::glib::Object) -> &gtk::ListItem {
    item.downcast_ref::<gtk::ListItem>()
        .expect("a column view hands its factories list items")
}

fn path_of(item: &gtk::glib::Object) -> String {
    list_item(item)
        .item()
        .and_downcast::<gtk::StringObject>()
        .expect("the model is a string list")
        .string()
        .to_string()
}

fn icon_label(icon: Option<&str>, label: &str) -> gtk::Box {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    if let Some(icon) = icon {
        content.append(&gtk::Image::from_icon_name(icon));
    }
    content.append(&gtk::Label::new(Some(label)));
    content
}

/// The icon and short label a choice shows in the action column. A copy's
/// arrow is in its text, pointing the way the file goes.
fn choice_look(choice: Choice) -> (Option<&'static str>, &'static str) {
    match choice {
        Choice::Copy(Side::Local) => (None, "Copy →"),
        Choice::Copy(Side::Remote) => (None, "← Copy"),
        Choice::Trash(Side::Local) => (Some("user-trash-symbolic"), "Delete left"),
        Choice::Trash(Side::Remote) => (Some("user-trash-symbolic"), "Delete right"),
        Choice::Skip => (Some("action-unavailable-symbolic"), "Skip"),
    }
}

/// A choice spelled out, for the menu.
fn choice_text(choice: Choice) -> &'static str {
    match choice {
        Choice::Copy(Side::Local) => "Copy local to remote",
        Choice::Copy(Side::Remote) => "Copy remote to local",
        Choice::Trash(Side::Local) => "Move local file to trash",
        Choice::Trash(Side::Remote) => "Move remote file to trash",
        Choice::Skip => "Skip this time",
    }
}

fn describe_stat(stat: &Stat) -> String {
    let date = gtk::glib::DateTime::from_unix_local(stat.mtime)
        .and_then(|d| d.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default();
    format!("{} · {date}", human_bytes(stat.size))
}

fn describe(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothEdited => "Conflict: edited on both sides",
        ConflictKind::BothNew => "Conflict: created on both sides with different contents",
        ConflictKind::EditedLocalDeletedRemote => "Conflict: edited locally, deleted on remote",
        ConflictKind::EditedRemoteDeletedLocal => "Conflict: edited on remote, deleted locally",
        ConflictKind::TypeMismatch => "Conflict: file on one side, directory on the other",
    }
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}
