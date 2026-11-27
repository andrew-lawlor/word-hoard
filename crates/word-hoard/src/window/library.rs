//! The Library page: every book as a cover grid or a sortable list, both
//! virtualised (only visible books get widgets), over one model.
//!
//! Select mode (the Select button) turns clicks into choosing books: an
//! action bar counts them and offers Send to Kobo. The selection is kept here
//! as a set of book IDs, shared by both views, rather than in GTK's selection
//! models: a view that opens items on a single click selects on hover.

use std::cmp::Ordering;
use std::collections::HashSet;

use hoard_core::store::Book;

use super::*;

/// Setting remembering grid or list.
const VIEW_SETTING: &str = "library_view";

pub(super) struct LibraryViews {
    /// The books shown, in the Sort menu's order.
    pub store: gio::ListStore,
    /// The same books as the list shows them (sorted by its columns); the
    /// grid shows this order too, so both views agree.
    sorted: gtk::SortListModel,
    /// "grid" or "list".
    pub stack: gtk::Stack,
    pub list_toggle: gtk::ToggleButton,
    /// Holds both toggles, shown only on the Library.
    pub toggles: gtk::Box,
    grid: gtk::GridView,
    list: gtk::ColumnView,
    check_column: gtk::ColumnViewColumn,
    /// Select mode on or off.
    pub select_toggle: gtk::ToggleButton,
    /// IDs of the chosen books.
    selected: Rc<RefCell<HashSet<i64>>>,
    selecting: Rc<Cell<bool>>,
    /// IDs of books on the connected Kobo (empty when none is connected).
    on_device: Rc<RefCell<HashSet<i64>>>,
    device_column: gtk::ColumnViewColumn,
    pub bar: gtk::ActionBar,
    bar_label: gtk::Label,
    pub send: gtk::Button,
    select_all: gtk::Button,
    done: gtk::Button,
}

fn book_at(model: &impl IsA<gio::ListModel>, position: u32) -> Option<i64> {
    let obj = model
        .item(position)
        .and_downcast::<glib::BoxedAnyObject>()?;
    let id = obj.borrow::<Book>().id;
    Some(id)
}

fn with_book<R>(obj: &glib::Object, f: impl FnOnce(&Book) -> R) -> Option<R> {
    let obj = obj.downcast_ref::<glib::BoxedAnyObject>()?;
    Some(f(&obj.borrow::<Book>()))
}

/// A list column showing `text` for each book, sorted by `cmp`.
fn column(
    title: &str,
    expand: bool,
    numeric: bool,
    text: fn(&Book) -> String,
    cmp: fn(&Book, &Book) -> Ordering,
) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::builder()
            .xalign(if numeric { 1.0 } else { 0.0 })
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        if numeric {
            label.add_css_class("numeric");
        }
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(label), Some(obj)) = (item.child().and_downcast::<gtk::Label>(), item.item())
        else {
            return;
        };
        if let Some(t) = with_book(&obj, text) {
            label.set_label(&t);
            label.set_tooltip_text(Some(t.as_str()).filter(|t| !t.is_empty()));
        }
    });
    let sorter = gtk::CustomSorter::new(move |a, b| {
        with_book(a, |a| with_book(b, |b| cmp(a, b)))
            .flatten()
            .unwrap_or(Ordering::Equal)
            .into()
    });
    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(&sorter)
        .expand(expand)
        .resizable(true)
        .build()
}

fn by_text(a: &Option<String>, b: &Option<String>) -> Ordering {
    // Books without the field go last.
    match (a, b) {
        (Some(a), Some(b)) => a.to_lowercase().cmp(&b.to_lowercase()),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn series_number(b: &Book) -> f64 {
    b.series_number
        .as_deref()
        .and_then(|n| n.parse().ok())
        .unwrap_or(f64::MAX)
}

/// A grid tile: a device badge when the book is on the connected Kobo, and
/// a tick and an outline when it's chosen in select mode.
fn tile(book: &Book, chosen: bool, on_device: bool) -> gtk::Widget {
    let tile = card::book_tile(book);
    if !chosen && !on_device {
        return tile;
    }
    let overlay = gtk::Overlay::builder().child(&tile).build();
    if on_device {
        let badge = gtk::Box::builder()
            .spacing(4)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_start(12)
            .tooltip_text("On your connected Kobo")
            .css_classes(["device-badge"])
            .build();
        badge.append(&gtk::Image::from_icon_name("media-removable-symbolic"));
        badge.append(
            &gtk::Label::builder()
                .label("On Kobo")
                .css_classes(["caption-heading"])
                .build(),
        );
        overlay.add_overlay(&badge);
    }
    if !chosen {
        return overlay.upcast();
    }
    let tick = gtk::Image::builder()
        .icon_name("object-select-symbolic")
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(6)
        .margin_end(6)
        .css_classes(["selection-tick"])
        .build();
    overlay.add_overlay(&tick);
    overlay.add_css_class("selected-tile");
    overlay.update_property(&[gtk::accessible::Property::Description("Selected")]);
    overlay.upcast()
}

/// Builds both views and the select-mode controls.
pub(super) fn build(lib: &Library) -> LibraryViews {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let sorted = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
    let selected: Rc<RefCell<HashSet<i64>>> = Rc::default();
    let selecting = Rc::new(Cell::new(false));
    let on_device: Rc<RefCell<HashSet<i64>>> = Rc::default();

    // Grid: tiles built only for visible books.
    let factory = gtk::SignalListItemFactory::new();
    {
        let (selected, selecting, on_device) =
            (selected.clone(), selecting.clone(), on_device.clone());
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let widget = item.item().and_then(|obj| {
                with_book(&obj, |b| {
                    tile(
                        b,
                        selecting.get() && selected.borrow().contains(&b.id),
                        on_device.borrow().contains(&b.id),
                    )
                })
            });
            if let Some(widget) = widget {
                item.set_child(Some(&widget));
            }
        });
    }
    factory.connect_unbind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(None::<&gtk::Widget>);
        }
    });
    let grid = gtk::GridView::builder()
        .model(&gtk::NoSelection::new(Some(sorted.clone())))
        .factory(&factory)
        .single_click_activate(true)
        .min_columns(2)
        .max_columns(10)
        .css_classes(["library-grid"])
        .build();

    // List: sortable columns over the same books, and a tick column in select mode.
    let list = gtk::ColumnView::builder()
        .model(&gtk::NoSelection::new(Some(sorted.clone())))
        .single_click_activate(true)
        .show_row_separators(true)
        .reorderable(false)
        .css_classes(["data-table", "library-list"])
        .build();
    let check_factory = gtk::SignalListItemFactory::new();
    check_factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            // Clicks go to the row (which toggles the book), not the box.
            item.set_child(Some(&gtk::CheckButton::builder().can_target(false).build()));
        }
    });
    {
        let selected = selected.clone();
        check_factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let (Some(check), Some(obj)) =
                (item.child().and_downcast::<gtk::CheckButton>(), item.item())
            else {
                return;
            };
            check.set_active(with_book(&obj, |b| selected.borrow().contains(&b.id)) == Some(true));
        });
    }
    let check_column = gtk::ColumnViewColumn::builder()
        .factory(&check_factory)
        .visible(false)
        .build();
    list.append_column(&check_column);
    list.append_column(&column(
        "Title",
        true,
        false,
        |b| b.title.clone(),
        |a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()),
    ));
    list.append_column(&column(
        "Author",
        true,
        false,
        |b| b.author.clone().unwrap_or_default(),
        |a, b| by_text(&a.author, &b.author),
    ));
    list.append_column(&column(
        "Series",
        true,
        false,
        |b| match (&b.series, &b.series_number) {
            (Some(s), Some(n)) => format!("{s} #{}", n.strip_suffix(".0").unwrap_or(n)),
            (Some(s), None) => s.clone(),
            _ => String::new(),
        },
        |a, b| by_text(&a.series, &b.series).then(series_number(a).total_cmp(&series_number(b))),
    ));
    list.append_column(&column(
        "Highlights",
        false,
        true,
        |b| {
            if b.annotation_count > 0 {
                b.annotation_count.to_string()
            } else {
                String::new()
            }
        },
        |a, b| b.annotation_count.cmp(&a.annotation_count),
    ));
    list.append_column(&column(
        "Read",
        false,
        true,
        |b| {
            b.percent_read
                .filter(|p| *p > 0)
                .map(|p| format!("{p}%"))
                .unwrap_or_default()
        },
        |a, b| {
            b.percent_read
                .unwrap_or(0)
                .cmp(&a.percent_read.unwrap_or(0))
        },
    ));
    // On the connected Kobo (the column shows only while one is connected).
    let device_factory = gtk::SignalListItemFactory::new();
    device_factory.connect_setup(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(Some(&gtk::Image::from_icon_name(
                "media-removable-symbolic",
            )));
        }
    });
    {
        let on_device = on_device.clone();
        device_factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let (Some(icon), Some(obj)) = (item.child().and_downcast::<gtk::Image>(), item.item())
            else {
                return;
            };
            let on = with_book(&obj, |b| on_device.borrow().contains(&b.id)) == Some(true);
            icon.set_opacity(if on { 1.0 } else { 0.0 });
            icon.set_tooltip_text(on.then_some("On your Kobo"));
        });
    }
    let device_column = gtk::ColumnViewColumn::builder()
        .title("On Kobo")
        .factory(&device_factory)
        .visible(false)
        .build();
    list.append_column(&device_column);
    sorted.set_sorter(list.sorter().as_ref());

    let scrolled = |child: &gtk::Widget| {
        gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(child)
            .build()
    };
    let stack = gtk::Stack::new();
    // A grid view scrolls itself; ClampScrollable caps its width without
    // getting between it and the scrolled window (a plain Clamp does, and
    // only the first row gets drawn).
    stack.add_named(
        &scrolled(
            &adw::ClampScrollable::builder()
                .maximum_size(1100)
                .child(&grid)
                .build()
                .upcast(),
        ),
        Some("grid"),
    );
    stack.add_named(&scrolled(&list.clone().upcast()), Some("list"));

    let grid_toggle = gtk::ToggleButton::builder()
        .icon_name("view-grid-symbolic")
        .tooltip_text("Covers")
        .build();
    grid_toggle.update_property(&[gtk::accessible::Property::Label("Show covers")]);
    let list_toggle = gtk::ToggleButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("List")
        .group(&grid_toggle)
        .build();
    list_toggle.update_property(&[gtk::accessible::Property::Label("Show as list")]);
    let toggles = gtk::Box::builder()
        .css_classes(["linked"])
        .visible(false)
        .build();
    toggles.append(&grid_toggle);
    toggles.append(&list_toggle);

    let saved_list = lib.setting(VIEW_SETTING).ok().flatten().as_deref() == Some("list");
    list_toggle.set_active(saved_list);
    grid_toggle.set_active(!saved_list);
    stack.set_visible_child_name(if saved_list { "list" } else { "grid" });

    let select_toggle = gtk::ToggleButton::builder()
        .icon_name("selection-mode-symbolic")
        .tooltip_text("Select Books")
        .visible(false)
        .build();
    select_toggle.update_property(&[gtk::accessible::Property::Label("Select books")]);

    let bar_label = gtk::Label::new(Some("No books selected"));
    let send = gtk::Button::builder()
        .child(
            &adw::ButtonContent::builder()
                .icon_name("document-send-symbolic")
                .label("Send to Kobo")
                .build(),
        )
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let select_all = gtk::Button::builder()
        .label("Select All")
        .tooltip_text("Select every book listed (Ctrl+A)")
        .build();
    let done = gtk::Button::with_label("Done");
    let bar = gtk::ActionBar::builder().revealed(false).build();
    bar.pack_start(&bar_label);
    bar.pack_end(&send);
    bar.pack_end(&done);
    bar.pack_end(&select_all);

    LibraryViews {
        store,
        sorted,
        stack,
        list_toggle,
        toggles,
        grid,
        list,
        check_column,
        select_toggle,
        selected,
        selecting,
        on_device,
        device_column,
        bar,
        bar_label,
        send,
        select_all,
        done,
    }
}

impl LibraryViews {
    /// Redraws the books whose state changed (all of them when `ids` is
    /// `None`). Each is replaced with a fresh copy: GTK keeps the widget of
    /// an item replaced with itself, so it wouldn't be redrawn.
    fn refresh(&self, ids: Option<&HashSet<i64>>) {
        let n = self.store.n_items();
        let fresh = |i: u32| {
            self.store
                .item(i)
                .and_downcast::<glib::BoxedAnyObject>()
                .map(|obj| glib::BoxedAnyObject::new(obj.borrow::<Book>().clone()))
        };
        match ids {
            None => {
                let objects: Vec<glib::BoxedAnyObject> = (0..n).filter_map(fresh).collect();
                self.store.splice(0, n, &objects);
            }
            Some(ids) => {
                for i in 0..n {
                    let hit = book_at(&self.store, i).is_some_and(|id| ids.contains(&id));
                    if let (true, Some(obj)) = (hit, fresh(i).filter(|_| hit)) {
                        self.store.splice(i, 1, &[obj]);
                    }
                }
            }
        }
    }

    /// The chosen books, in the order shown.
    pub fn selected_ids(&self) -> Vec<i64> {
        let selected = self.selected.borrow();
        (0..self.sorted.n_items())
            .filter_map(|i| book_at(&self.sorted, i))
            .filter(|id| selected.contains(id))
            .collect()
    }
}

impl Window {
    /// Marks the books on the connected Kobo (none when it's disconnected).
    /// Called when a Kobo connects or disconnects, and after sending or
    /// importing.
    pub(super) fn refresh_on_device(&self) {
        let root = self.kobo.borrow().as_ref().map(|k| k.root.clone());
        let on = match &root {
            Some(root) => self.lib.borrow().books_on_device(root).unwrap_or_default(),
            None => HashSet::new(),
        };
        self.library.device_column.set_visible(root.is_some());
        if *self.library.on_device.borrow() != on {
            *self.library.on_device.borrow_mut() = on;
            self.library.refresh(None);
        }
    }

    /// Selects every book listed (turning select mode on).
    pub(super) fn select_all_books(&self) {
        if !self.library.selecting.get() {
            self.set_selecting(true);
        }
        let all: HashSet<i64> = (0..self.library.store.n_items())
            .filter_map(|i| book_at(&self.library.store, i))
            .collect();
        *self.library.selected.borrow_mut() = all;
        self.library.refresh(None);
        self.update_send_action();
    }

    /// Grid/list toggle, opening and choosing books, and the select-mode bar.
    pub(super) fn connect_library(self: &Rc<Self>) {
        // Ctrl+A selects every book listed; Escape leaves select mode.
        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, state| {
            let Some(this) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if state.contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::Key::a | gdk::Key::A)
            {
                this.select_all_books();
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::Escape && this.library.selecting.get() {
                this.set_selecting(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.library.stack.add_controller(keys);

        let weak = Rc::downgrade(self);
        self.library.list_toggle.connect_toggled(move |toggle| {
            let Some(this) = weak.upgrade() else { return };
            let view = if toggle.is_active() { "list" } else { "grid" };
            this.library.stack.set_visible_child_name(view);
            if let Err(err) = this.lib.borrow().set_setting(VIEW_SETTING, view) {
                this.error("Couldn’t Save Setting", err);
            }
        });

        let weak = Rc::downgrade(self);
        self.library.grid.connect_activate(move |view, position| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(id) = view.model().and_then(|m| book_at(&m, position)) {
                this.book_clicked(id);
            }
        });
        let weak = Rc::downgrade(self);
        self.library.list.connect_activate(move |view, position| {
            let Some(this) = weak.upgrade() else { return };
            if let Some(id) = view.model().and_then(|m| book_at(&m, position)) {
                this.book_clicked(id);
            }
        });

        let weak = Rc::downgrade(self);
        self.library.select_toggle.connect_toggled(move |toggle| {
            if let Some(this) = weak.upgrade() {
                this.set_selecting(toggle.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        self.library.done.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.library.select_toggle.set_active(false);
            }
        });
        let weak = Rc::downgrade(self);
        self.library.select_all.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.select_all_books();
            }
        });
        let weak = Rc::downgrade(self);
        self.library.send.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                let ids = this.library.selected_ids();
                this.send_books(ids);
            }
        });
    }

    /// A click on a book: in select mode it's chosen or unchosen; otherwise
    /// its page opens.
    fn book_clicked(self: &Rc<Self>, id: i64) {
        if !self.library.selecting.get() {
            self.navigate(Nav::Annotations(View::Book(id)), true);
            return;
        }
        {
            let mut selected = self.library.selected.borrow_mut();
            if !selected.remove(&id) {
                selected.insert(id);
            }
        }
        self.library.refresh(Some(&HashSet::from([id])));
        self.update_send_action();
    }

    /// Turns select mode on or off (off clears the choice).
    pub(super) fn set_selecting(&self, on: bool) {
        self.library.selecting.set(on);
        if !on {
            self.library.selected.borrow_mut().clear();
        }
        self.library.check_column.set_visible(on);
        self.library.bar.set_revealed(on);
        if self.library.select_toggle.is_active() != on {
            self.library.select_toggle.set_active(on);
        }
        self.library.refresh(None);
        self.update_send_action();
    }

    /// Updates the select-mode bar: the count, and Send to Kobo (only with
    /// books chosen and a Kobo connected).
    pub(super) fn update_send_action(&self) {
        let n = self.library.selected.borrow().len();
        self.library.bar_label.set_label(&match n {
            0 => "No books selected".to_owned(),
            n => format!("{} selected", plural(n, "book", "books")),
        });
        let connected = self.kobo.borrow().is_some();
        self.library.send.set_sensitive(n > 0 && connected);
        self.library.send.set_tooltip_text(Some(&if connected {
            format!("Send to Your {}", self.kobo_name())
        } else {
            "Connect your Kobo to send books to it".to_owned()
        }));
    }

    /// Replaces the Library's books. Chosen books that are no longer listed
    /// (a search narrowed the list) stay chosen.
    pub(super) fn show_library_books(&self, books: Vec<Book>) {
        let objects: Vec<glib::BoxedAnyObject> =
            books.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.books_shown.set(objects.len());
        self.library
            .store
            .splice(0, self.library.store.n_items(), &objects);
    }
}
