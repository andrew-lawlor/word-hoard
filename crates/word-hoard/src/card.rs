//! The widget showing one annotation.

use adw::prelude::*;
use chrono::Local;
use gtk::{gio, glib};
use hoard_core::store::{Annotation, Book, BookDetails, BookFile, Status};

pub fn format_date(date: chrono::DateTime<chrono::Utc>) -> String {
    date.with_timezone(&Local).format("%-d %b %Y").to_string()
}

fn pill(text: &str, warning: bool) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("pill");
    if warning {
        label.add_css_class("warning");
    }
    label
}

fn wrapped_label(text: &str, css: &[&str]) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(css.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .build();
    label.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    label
}

/// The card's popover menu; items are actions in the row's `card` group.
fn menu(a: &Annotation) -> gio::Menu {
    let menu = gio::Menu::new();
    let main = gio::Menu::new();
    main.append(Some("_Edit…"), Some("card.edit"));
    main.append(Some("_Copy Text"), Some("card.copy"));
    main.append(Some("Copy as _Markdown"), Some("card.copy-markdown"));
    if a.text().is_some() || a.note().is_some() {
        main.append(Some("_Share as Image…"), Some("card.share"));
    }
    if a.markup_svg.is_some() || a.markup_jpg.is_some() {
        main.append(Some("_Open Page Image"), Some("card.open-image"));
    }
    menu.append_section(None, &main);

    let status = gio::Menu::new();
    match a.status {
        Status::Inbox => {
            status.append(Some("_Keep"), Some("card.keep"));
            status.append(Some("_Archive"), Some("card.archive"));
        }
        Status::Kept => {
            status.append(Some("Move to _Inbox"), Some("card.inbox"));
            status.append(Some("_Archive"), Some("card.archive"));
        }
        Status::Archived | Status::Trashed => {
            status.append(Some("_Restore"), Some("card.keep"));
        }
    }
    if a.status != Status::Trashed {
        status.append(Some("Move to _Trash"), Some("card.trash"));
    }
    menu.append_section(None, &status);

    if a.device_changed_at.is_some() {
        let device = gio::Menu::new();
        device.append(Some("Use _Kobo’s Version"), Some("card.accept-device"));
        menu.append_section(None, &device);
    }
    menu
}

/// Accessible name for an icon-only control: the tooltip minus its shortcut hint.
fn set_accessible_label(widget: &impl IsA<gtk::Accessible>, tooltip: &str) {
    let label = tooltip.split(" (").next().unwrap_or(tooltip);
    widget.update_property(&[gtk::accessible::Property::Label(label)]);
}

fn icon_button(icon: &str, tooltip: &str, action: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .action_name(action)
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    set_accessible_label(&button, tooltip);
    button
}

/// Builds the card. In a book view the list is grouped by chapter, so the
/// chapter is left out of the details line; elsewhere it's grouped by book.
/// `name_reader` adds which reader the annotation was made in, for a
/// library with annotations from more than one.
pub fn build(a: &Annotation, in_book_view: bool, name_reader: bool) -> gtk::Widget {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .margin_top(14)
        .margin_bottom(8)
        .margin_start(14)
        .margin_end(10)
        .build();

    let bar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    bar.add_css_class("color-bar");
    bar.add_css_class(&match hoard_core::color::color_index(&a.color) {
        Some(_) => format!("hl-{}", a.color),
        None => "hl-unknown".to_owned(),
    });
    bar.set_margin_bottom(6);
    bar.set_tooltip_text(Some(&a.color));
    // Notebook pages have no highlight colour.
    if a.kind != "page" {
        root.append(&bar);
    }

    let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    body.set_hexpand(true);
    let page = a.kind == "page";
    if a.kind == "markup" || page {
        // The ink first; what it marks and says (as read) below it. For a
        // notebook page, what it says.
        match a.markup_view() {
            Some(path) => {
                let picture = gtk::Picture::builder()
                    .file(&gtk::gio::File::for_path(&path))
                    .content_fit(gtk::ContentFit::Contain)
                    .can_shrink(true)
                    .halign(gtk::Align::Start)
                    .css_classes(["markup-image"])
                    .build();
                // Sized for a typical card width (~680px) from the crop's
                // shape, so handwriting stays readable; a whole page is capped
                // (higher for a notebook page, which is all handwriting).
                let cap = if page { 560 } else { 320 };
                let height = a.markup_crop.map_or(cap, |c| {
                    (680 * (c.bottom - c.top + 1) / (c.right - c.left + 1)).clamp(160, cap)
                });
                picture.set_height_request(height);
                let (tooltip, label) = if page {
                    (
                        "Notebook page (redrawn from your Kobo’s ink)",
                        "Notebook page image",
                    )
                } else {
                    (
                        "Handwritten markup (page image from your Kobo)",
                        "Handwritten markup page image",
                    )
                };
                picture.set_tooltip_text(Some(tooltip));
                picture.update_property(&[gtk::accessible::Property::Label(label)]);
                body.append(&picture);
            }
            None => {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                row.append(&gtk::Image::from_icon_name(if page {
                    "accessories-text-editor-symbolic"
                } else {
                    "input-tablet-symbolic"
                }));
                row.append(&wrapped_label(
                    if page {
                        "Notebook page. Its ink is copied when the Kobo is connected."
                    } else {
                        "Handwritten markup. The page image is copied when the Kobo is connected."
                    },
                    &["dim-label"],
                ));
                body.append(&row);
            }
        }
        if let Some(text) = a.text() {
            body.append(&wrapped_label(text, &["quote"]));
        }
    } else {
        match a.text() {
            Some(text) => body.append(&wrapped_label(text, &["quote"])),
            None => body.append(&wrapped_label("(no text)", &["dim-label"])),
        }
    }

    if let Some(note) = a.note() {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let icon = gtk::Image::from_icon_name("document-edit-symbolic");
        icon.set_valign(gtk::Align::Start);
        icon.add_css_class("dim-label");
        row.append(&icon);
        row.append(&wrapped_label(note, &["note"]));
        body.append(&row);
    }

    // Text read from handwriting is a guess until the user corrects it.
    let machine_read = (a.user_text.is_none() && a.device_text.is_none() && a.ink_text.is_some())
        || (a.user_note.is_none() && a.device_note.is_none() && a.ink_note.is_some());
    if machine_read && let Some(source) = &a.ink_source {
        let name = hoard_transcribe::catalog()
            .iter()
            .find(|m| m.id == source)
            .map_or(source.as_str(), |m| m.name);
        body.append(&wrapped_label(
            &format!("Read from handwriting by {name}. Edit to correct it."),
            &["caption", "dim-label"],
        ));
    }

    // Details line: chapter · date, pills, then actions.
    let details = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let mut meta = Vec::new();
    if !in_book_view && let Some(chapter) = &a.chapter_title {
        meta.push(chapter.clone());
    }
    if let Some(date) = a.created_at {
        meta.push(format_date(date));
    }
    if name_reader {
        meta.push(if a.from_koreader { "KOReader" } else { "Kobo" }.to_owned());
    }
    let meta_label = gtk::Label::builder()
        .label(meta.join(" · "))
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["caption", "dim-label"])
        .build();
    meta_label.set_tooltip_text(Some(&meta.join("\n")));
    details.append(&meta_label);
    for tag in &a.tags {
        details.append(&pill(tag, false));
    }
    if a.user_text.is_some() || a.user_note.is_some() {
        details.append(&pill("edited", false));
    }
    if a.device_changed_at.is_some() {
        let p = pill("changed on Kobo", true);
        p.set_tooltip_text(Some(
            "Edited on the Kobo after you edited it here. Your version is shown.",
        ));
        details.append(&p);
    }
    if a.removed_on_device_at.is_some() {
        let p = pill("deleted on Kobo", true);
        p.set_tooltip_text(Some("No longer on the Kobo. Word Hoard keeps it."));
        details.append(&p);
    }

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    actions.set_margin_start(6);
    let (star_icon, star_tip) = if a.starred {
        ("starred-symbolic", "Unstar (S)")
    } else {
        ("non-starred-symbolic", "Star (S)")
    };
    actions.append(&icon_button(star_icon, star_tip, "card.star"));
    if a.status == Status::Inbox {
        actions.append(&icon_button("checkmark-symbolic", "Keep (K)", "card.keep"));
        actions.append(&icon_button(
            "folder-symbolic",
            "Archive (A)",
            "card.archive",
        ));
    }
    let more = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More")
        .menu_model(&menu(a))
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    set_accessible_label(&more, "More");
    actions.append(&more);
    details.append(&actions);
    body.append(&details);
    root.append(&body);
    root.upcast()
}

/// The header row at the top of a book's page: cover, title and progress.
/// The top of a book's page: cover, details, description and files.
pub fn book_header(book: &Book, details: &BookDetails) -> gtk::ListBoxRow {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(12)
        .build();
    let hbox = gtk::Box::builder().spacing(18).build();
    const COVER_W: i32 = 140;
    let cover = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .width_request(COVER_W)
        .height_request(COVER_W * 3 / 2)
        .valign(gtk::Align::Start)
        .css_classes(["book-cover"])
        .build();
    match book.cover.as_ref().filter(|p| p.is_file()) {
        Some(path) => cover.set_filename(Some(path)),
        None => cover.add_css_class("no-cover"),
    }
    cover.update_property(&[gtk::accessible::Property::Label("Cover")]);
    // A picture asks for its image's full size; clamp it to the cover width.
    hbox.append(
        &adw::Clamp::builder()
            .maximum_size(COVER_W)
            .tightening_threshold(COVER_W)
            .valign(gtk::Align::Start)
            .child(&cover)
            .build(),
    );

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    text.append(&wrapped_label(&book.title, &["title-2"]));
    if let Some(author) = &book.author {
        text.append(&wrapped_label(author, &["dim-label"]));
    }
    if let Some(series) = &book.series {
        let number = book
            .series_number
            .as_deref()
            .map(|n| n.strip_suffix(".0").unwrap_or(n))
            .filter(|n| !n.is_empty() && *n != "0");
        let line = match number {
            Some(n) => format!("{series} #{n}"),
            None => series.clone(),
        };
        text.append(&wrapped_label(&line, &[]));
    }
    let mut facts: Vec<String> = Vec::new();
    facts.extend(details.year().map(str::to_owned));
    facts.extend(book.publisher.clone());
    facts.extend(details.language.clone());
    facts.extend(book.isbn.as_ref().map(|i| format!("ISBN {i}")));
    if !facts.is_empty() {
        let label = wrapped_label(&facts.join(" · "), &["caption", "dim-label"]);
        label.set_margin_top(6);
        text.append(&label);
    }
    let mut reading = Vec::new();
    if let Some(p) = book.percent_read.filter(|p| *p > 0) {
        reading.push(format!("{p}% read"));
    }
    if let Some(date) = book.last_read_at {
        reading.push(format!("last read {}", format_date(date)));
    }
    if !reading.is_empty() {
        text.append(&wrapped_label(
            &reading.join(" · "),
            &["caption", "dim-label"],
        ));
    }
    hbox.append(&text);
    outer.append(&hbox);

    if let Some(description) = &details.description {
        outer.append(&description_label(description));
    }
    if !details.files.is_empty() {
        let files = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for file in &details.files {
            files.append(&file_row(file));
        }
        outer.append(&files);
    }

    gtk::ListBoxRow::builder()
        .child(&outer)
        .selectable(false)
        .activatable(false)
        .focusable(false)
        .css_classes(["book-header"])
        .build()
}

/// A description shown in its first lines, with More to read all of it.
fn description_label(description: &str) -> gtk::Widget {
    const SHORT_LINES: i32 = 5;
    let label = wrapped_label(description, &["body"]);
    label.set_selectable(true);
    let long =
        description.chars().count() > 360 || description.lines().count() > SHORT_LINES as usize;
    if !long {
        return label.upcast();
    }
    label.set_lines(SHORT_LINES);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let more = gtk::Button::builder()
        .label("More")
        .halign(gtk::Align::Start)
        .css_classes(["flat", "caption"])
        .build();
    let shown = label.clone();
    more.connect_clicked(move |button| {
        let expanded = shown.lines() == -1;
        shown.set_lines(if expanded { SHORT_LINES } else { -1 });
        shown.set_ellipsize(if expanded {
            gtk::pango::EllipsizeMode::End
        } else {
            gtk::pango::EllipsizeMode::None
        });
        button.set_label(if expanded { "More" } else { "Less" });
    });
    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .build();
    vbox.append(&label);
    vbox.append(&more);
    vbox.upcast()
}

/// One of a book's files: its format and size, Open and Show in Files.
fn file_row(file: &BookFile) -> adw::ActionRow {
    let format = match file.format.as_str() {
        "kepub" => "KEPUB",
        "epub" => "EPUB",
        other => other,
    };
    let row = adw::ActionRow::builder()
        .title(format!("{format} · {}", glib::format_size(file.size)))
        .subtitle(file.path.file_name().unwrap_or_default().to_string_lossy())
        .subtitle_lines(1)
        .build();
    let missing = !file.path.is_file();
    if missing {
        row.set_subtitle("Missing from the books folder");
    }
    let open = gtk::Button::builder()
        .label("Open")
        .valign(gtk::Align::Center)
        .sensitive(!missing)
        .tooltip_text("Open in your e-book reader")
        .build();
    let show = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .sensitive(!missing)
        .tooltip_text("Show in Files")
        .build();
    show.update_property(&[gtk::accessible::Property::Label("Show in Files")]);
    let path = file.path.clone();
    open.connect_clicked(move |button| {
        let window = button.root().and_downcast::<gtk::Window>();
        gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
            window.as_ref(),
            None::<&gio::Cancellable>,
            |_| {},
        );
    });
    let path = file.path.clone();
    show.connect_clicked(move |button| {
        let window = button.root().and_downcast::<gtk::Window>();
        gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).open_containing_folder(
            window.as_ref(),
            None::<&gio::Cancellable>,
            |_| {},
        );
    });
    row.add_suffix(&show);
    row.add_suffix(&open);
    row
}

/// Shown under a book's details when it has no highlights.
pub fn no_highlights_row() -> gtk::ListBoxRow {
    let label = gtk::Label::builder()
        .label("No highlights in this book yet. Highlights you make on your Kobo appear here after an import.")
        .wrap(true)
        .xalign(0.0)
        .margin_top(6)
        .css_classes(["dim-label"])
        .build();
    gtk::ListBoxRow::builder()
        .child(&label)
        .selectable(false)
        .activatable(false)
        .focusable(false)
        .css_classes(["book-header"])
        .build()
}

/// A cover tile in the Library grid: cover, title, author and counts. The
/// grid view makes it clickable and focusable.
pub fn book_tile(book: &Book) -> gtk::Widget {
    const W: i32 = 132;
    let tile = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .width_request(W)
        .halign(gtk::Align::Center)
        .build();
    match book.cover.as_ref().filter(|p| p.is_file()) {
        Some(path) => {
            let cover = gtk::Picture::builder()
                .content_fit(gtk::ContentFit::Cover)
                .can_shrink(true)
                .height_request(W * 3 / 2)
                .css_classes(["book-cover"])
                .build();
            cover.set_filename(Some(path));
            // A picture asks for its image's full size; clamp it to the tile.
            tile.append(
                &adw::Clamp::builder()
                    .maximum_size(W)
                    .tightening_threshold(W)
                    .child(&cover)
                    .build(),
            );
        }
        None => {
            let placeholder = gtk::Box::builder()
                .width_request(W)
                .height_request(W * 3 / 2)
                .css_classes(["book-cover", "no-cover"])
                .build();
            let icon = gtk::Image::builder()
                .icon_name("x-office-document-symbolic")
                .pixel_size(40)
                .hexpand(true)
                .vexpand(true)
                .css_classes(["dim-label"])
                .build();
            placeholder.append(&icon);
            tile.append(&placeholder);
        }
    }
    let label = |text: &str, css: &[&str], lines: i32| {
        gtk::Label::builder()
            .label(text)
            .xalign(0.0)
            // Only multi-line labels wrap: a one-line label that both wraps
            // and ellipsizes measures inconsistently (GTK warns about it).
            .wrap(lines > 1)
            .lines(lines)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(1)
            .css_classes(css.iter().map(|c| c.to_string()).collect::<Vec<_>>())
            .build()
    };
    let title = label(&book.title, &["heading"], 2);
    title.set_margin_top(6);
    tile.append(&title);
    if let Some(author) = &book.author {
        tile.append(&label(author, &["caption", "dim-label"], 1));
    }
    let mut counts = Vec::new();
    if book.annotation_count > 0 {
        counts.push(format!(
            "{} {}",
            book.annotation_count,
            if book.annotation_count == 1 {
                "highlight"
            } else {
                "highlights"
            }
        ));
    }
    if book.vocab_count > 0 {
        counts.push(format!(
            "{} {}",
            book.vocab_count,
            if book.vocab_count == 1 {
                "word"
            } else {
                "words"
            }
        ));
    }
    if !counts.is_empty() {
        tile.append(&label(&counts.join(" · "), &["caption", "dim-label"], 1));
    }

    let name = match &book.author {
        Some(author) => format!("{} — {author}", book.title),
        None => book.title.clone(),
    };
    tile.set_tooltip_text(Some(&name));
    tile.update_property(&[gtk::accessible::Property::Label(&name)]);
    tile.add_css_class("book-tile");
    // Every tile is the cover's width; long titles wrap rather than widen it.
    adw::Clamp::builder()
        .maximum_size(W)
        .tightening_threshold(W)
        .child(&tile)
        .build()
        .upcast()
}

/// Markdown blockquote with the note and source, for "Copy as Markdown".
pub fn to_markdown(a: &Annotation) -> String {
    let mut out = String::new();
    if let Some(text) = a.text() {
        for line in text.lines() {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
    let source = match (&a.book_author, &a.chapter_title) {
        (Some(author), Some(ch)) => format!("{author}, *{}*, {ch}", a.book_title),
        (Some(author), None) => format!("{author}, *{}*", a.book_title),
        (None, Some(ch)) => format!("*{}*, {ch}", a.book_title),
        (None, None) => format!("*{}*", a.book_title),
    };
    out.push_str(&format!("> — {source}\n"));
    if let Some(note) = a.note() {
        out.push('\n');
        out.push_str(note);
        out.push('\n');
    }
    out
}
