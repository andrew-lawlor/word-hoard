//! The Library page: every book as a cover grid or a sortable list, both
//! virtualised (only visible books get widgets), over one model.

use std::cmp::Ordering;

use hoard_core::store::Book;

use super::*;

/// Setting remembering grid or list.
const VIEW_SETTING: &str = "library_view";

pub(super) struct LibraryViews {
    /// The books shown, in the Sort menu's order.
    pub store: gio::ListStore,
    /// "grid" or "list".
    pub stack: gtk::Stack,
    pub list_toggle: gtk::ToggleButton,
    /// Holds both toggles, shown only on the Library.
    pub toggles: gtk::Box,
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

/// Builds both views. `open` is called with a book's ID when it's activated.
pub(super) fn build(lib: &Library, open: impl Fn(i64) + Clone + 'static) -> LibraryViews {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();

    // Grid: tiles built only for visible books.
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        if let Some(tile) = item.item().and_then(|obj| with_book(&obj, card::book_tile)) {
            item.set_child(Some(&tile));
        }
    });
    factory.connect_unbind(|_, item| {
        if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
            item.set_child(None::<&gtk::Widget>);
        }
    });
    let grid = gtk::GridView::builder()
        .model(&gtk::NoSelection::new(Some(store.clone())))
        .factory(&factory)
        .single_click_activate(true)
        .min_columns(2)
        .max_columns(10)
        .css_classes(["library-grid"])
        .build();
    let on_open = open.clone();
    grid.connect_activate(move |view, position| {
        if let Some(id) = view.model().and_then(|m| book_at(&m, position)) {
            on_open(id);
        }
    });

    // List: sortable columns over the same books.
    let sorted = gtk::SortListModel::new(Some(store.clone()), None::<gtk::Sorter>);
    let list = gtk::ColumnView::builder()
        .model(&gtk::SingleSelection::new(Some(sorted.clone())))
        .single_click_activate(true)
        .show_row_separators(true)
        .reorderable(false)
        .css_classes(["data-table", "library-list"])
        .build();
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
    sorted.set_sorter(list.sorter().as_ref());
    list.connect_activate(move |view, position| {
        if let Some(id) = view.model().and_then(|m| book_at(&m, position)) {
            open(id);
        }
    });

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

    LibraryViews {
        store,
        stack,
        list_toggle,
        toggles,
    }
}

impl Window {
    /// Shows the Library as covers or a list, and remembers the choice.
    pub(super) fn connect_library_toggles(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.library.list_toggle.connect_toggled(move |toggle| {
            let Some(this) = weak.upgrade() else { return };
            let view = if toggle.is_active() { "list" } else { "grid" };
            this.library.stack.set_visible_child_name(view);
            if let Err(err) = this.lib.borrow().set_setting(VIEW_SETTING, view) {
                this.error("Couldn’t Save Setting", err);
            }
        });
    }

    /// Replaces the Library's books.
    pub(super) fn show_library_books(&self, books: Vec<Book>) {
        let objects: Vec<glib::BoxedAnyObject> =
            books.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.books_shown.set(objects.len());
        self.library
            .store
            .splice(0, self.library.store.n_items(), &objects);
    }
}
