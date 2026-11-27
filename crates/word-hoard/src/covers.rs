//! Cover images for the Library and book pages, loaded without holding up
//! the window.
//!
//! Library covers are often full size (1000×1500 pixels or more), and a tile
//! shows them at 132×198. Each cover is shrunk once to [`THUMB_W`]×[`THUMB_H`]
//! (twice tile size, for HiDPI screens) and saved in the cache folder; tiles
//! load those thumbnails in a background thread and keep recent ones in
//! memory, so scrolling back or redrawing tiles reuses them. The full-size
//! cover is left alone (sending a book to the Kobo embeds it).

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use adw::prelude::*;
use gtk::{gdk, gdk_pixbuf, gio, glib};

const THUMB_W: i32 = 300;
const THUMB_H: i32 = 450;
/// Thumbnails kept in memory (about 0.5 MB each).
const MEMORY_CACHE: usize = 400;

/// A cover file as it is now: a changed cover gets a new thumbnail.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Key {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
}

impl Key {
    fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok().filter(|m| m.is_file())?;
        Some(Self {
            path: path.to_path_buf(),
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }

    fn thumbnail_path(&self) -> PathBuf {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        glib::user_cache_dir()
            .join("word-hoard/covers")
            .join(format!("{:016x}", h.finish()))
    }
}

#[derive(Default)]
struct Cache {
    textures: HashMap<Key, gdk::Texture>,
    order: VecDeque<Key>,
    /// Covers being loaded, and the pictures waiting for each: a tile
    /// rebuilt meanwhile joins the load instead of starting another.
    loading: HashMap<Key, Vec<glib::WeakRef<gtk::Picture>>>,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::default();
}

fn cached(key: &Key) -> Option<gdk::Texture> {
    CACHE.with(|c| c.borrow().textures.get(key).cloned())
}

fn remember(key: Key, texture: gdk::Texture) {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.textures.insert(key.clone(), texture).is_none() {
            c.order.push_back(key);
        }
        while c.order.len() > MEMORY_CACHE {
            if let Some(old) = c.order.pop_front() {
                c.textures.remove(&old);
            }
        }
    });
}

/// The thumbnail for `key`, made (and saved) if there isn't one yet. Runs in
/// a background thread.
fn load(key: &Key) -> Option<gdk::Texture> {
    let thumb = key.thumbnail_path();
    if let Ok(texture) = gdk::Texture::from_filename(&thumb) {
        return Some(texture);
    }
    // Decoding at the smaller size is much faster than decoding in full.
    let pixbuf = gdk_pixbuf::Pixbuf::from_file_at_scale(&key.path, THUMB_W, THUMB_H, true).ok()?;
    if let Some(dir) = thumb.parent() {
        let _ = std::fs::create_dir_all(dir);
        // Written aside and renamed, so a half-written thumbnail is never read.
        let partial = thumb.with_extension(format!("{}.partial", std::process::id()));
        // JPEG is a tenth the size of PNG; covers with transparency stay PNG.
        let saved = if pixbuf.has_alpha() {
            pixbuf.savev(&partial, "png", &[])
        } else {
            pixbuf.savev(&partial, "jpeg", &[("quality", "90")])
        };
        if saved.is_ok() {
            let _ = std::fs::rename(&partial, &thumb);
        } else {
            let _ = std::fs::remove_file(&partial);
        }
    }
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

/// Shows `cover` in `picture`: at once if it's in memory, otherwise once a
/// background thread has loaded it. Returns false when there's no cover file.
pub fn show(picture: &gtk::Picture, cover: Option<&Path>) -> bool {
    let Some(key) = cover.and_then(Key::of) else {
        return false;
    };
    if let Some(texture) = cached(&key) {
        picture.set_paintable(Some(&texture));
        return true;
    }
    let first = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let waiting = c.loading.entry(key.clone()).or_default();
        waiting.push(picture.downgrade());
        waiting.len() == 1
    });
    if !first {
        return true;
    }
    glib::spawn_future_local(async move {
        let job_key = key.clone();
        let texture = gio::spawn_blocking(move || load(&job_key))
            .await
            .ok()
            .flatten();
        let waiting = CACHE
            .with(|c| c.borrow_mut().loading.remove(&key))
            .unwrap_or_default();
        let Some(texture) = texture else { return };
        remember(key, texture.clone());
        // Tiles recycled for other books since are gone or show another cover.
        for picture in waiting.iter().filter_map(|w| w.upgrade()) {
            picture.set_paintable(Some(&texture));
        }
    });
    true
}
