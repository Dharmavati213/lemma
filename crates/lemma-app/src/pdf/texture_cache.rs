//! The PDF view's page textures: a byte-bounded LRU ([`ByteLru`]) plus an index of the cached
//! keys of each page, so drawing can fall back to any texture of a page (another scale while
//! zooming, an older generation while a rebuilt PDF loads).

use gtk::gdk;
use lemma_pdf::{ByteLru, PageKey};
use std::collections::HashMap;

/// Textures by [`PageKey`], at most `cap` bytes of bitmap data in total, except while the
/// textures the view must keep need more (see [`TextureCache::insert_keeping`]).
pub struct TextureCache {
    lru: ByteLru<PageKey, gdk::Texture>,
    /// The configured cap; the LRU's own cap is raised above it only for kept textures.
    cap: usize,
    /// The keys in `lru`, per page.
    by_page: HashMap<u32, Vec<PageKey>>,
    /// The bytes of each key in `lru`.
    sizes: HashMap<PageKey, usize>,
}

impl TextureCache {
    /// An empty cache holding at most `cap_bytes` bytes.
    pub fn new(cap_bytes: usize) -> Self {
        Self {
            lru: ByteLru::new(cap_bytes),
            cap: cap_bytes,
            by_page: HashMap::new(),
            sizes: HashMap::new(),
        }
    }

    /// Adds `texture` (whose bitmap takes `bytes`) as the most recently used entry, evicting the
    /// least recently used ones beyond the cap.
    #[cfg(test)]
    pub fn insert(&mut self, key: PageKey, texture: gdk::Texture, bytes: usize) {
        self.insert_keeping(key, texture, bytes, |_| false);
    }

    /// Adds `texture` (whose bitmap takes `bytes`) as the most recently used entry, unless `keep`
    /// does not hold for `key` and it does not fit beside the entries for which `keep` holds; then
    /// the texture is dropped and false returned.
    ///
    /// Kept entries (the textures on screen and the ones about to be) are evicted only after
    /// every other one, and never to make room for another kept entry: while the kept entries
    /// need more than the cap, the cache grows to hold them. Other entries are evicted from the
    /// least recently used on, as usual.
    pub fn insert_keeping(
        &mut self,
        key: PageKey,
        texture: gdk::Texture,
        bytes: usize,
        keep: impl Fn(&PageKey) -> bool,
    ) -> bool {
        let kept: Vec<PageKey> = self
            .sizes
            .keys()
            .filter(|k| **k != key && keep(k))
            .copied()
            .collect();
        let needed = kept.iter().map(|k| self.sizes[k]).sum::<usize>() + bytes;
        if needed > self.cap && !keep(&key) {
            return false;
        }
        // The kept entries become the most recently used, so the others are evicted first.
        for k in &kept {
            self.lru.get(k);
        }
        let mut evicted = self.lru.set_cap(self.cap.max(needed));
        evicted.extend(self.lru.insert(key, texture, bytes));
        let keys = self.by_page.entry(key.page).or_default();
        if !keys.contains(&key) {
            keys.push(key);
        }
        self.sizes.insert(key, bytes);
        for gone in evicted {
            self.forget(&gone);
        }
        debug_assert!(
            self.lru.used() <= self.lru.cap(),
            "the texture cache exceeds its cap"
        );
        debug_assert!(
            kept.iter().all(|k| self.sizes.contains_key(k)),
            "a kept texture was evicted"
        );
        true
    }

    /// True when `key` is cached (does not count as a use).
    pub fn contains(&self, key: &PageKey) -> bool {
        self.by_page
            .get(&key.page)
            .is_some_and(|keys| keys.contains(key))
    }

    /// The texture for `key`, marked as most recently used.
    pub fn get(&mut self, key: &PageKey) -> Option<gdk::Texture> {
        self.lru.get(key).cloned()
    }

    /// Every cached key of `page`, in no particular order.
    pub fn keys_of_page(&self, page: u32) -> &[PageKey] {
        self.by_page.get(&page).map_or(&[], Vec::as_slice)
    }

    /// Keeps only the entries whose key satisfies `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(&PageKey) -> bool) {
        self.lru.retain(&mut keep);
        self.by_page.retain(|_, keys| {
            keys.retain(&mut keep);
            !keys.is_empty()
        });
        self.sizes.retain(|k, _| keep(k));
    }

    /// Bytes in use (more than the cap only while kept textures need it).
    pub fn used(&self) -> usize {
        self.lru.used()
    }

    /// The configured cap in bytes.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Number of textures.
    pub fn len(&self) -> usize {
        self.lru.len()
    }

    /// Sets the configured cap, evicting the least recently used textures beyond it.
    pub fn set_cap(&mut self, cap_bytes: usize) {
        self.cap = cap_bytes;
        let evicted = self.lru.set_cap(cap_bytes);
        for gone in evicted {
            self.forget(&gone);
        }
    }

    fn forget(&mut self, key: &PageKey) {
        self.sizes.remove(key);
        if let Some(keys) = self.by_page.get_mut(&key.page) {
            keys.retain(|k| k != key);
            if keys.is_empty() {
                self.by_page.remove(&key.page);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(page: u32, scale_q: u16) -> PageKey {
        PageKey {
            generation: 1,
            page,
            scale_q,
            tile: None,
        }
    }

    fn texture() -> gdk::Texture {
        let bytes = glib::Bytes::from_owned(vec![255u8; 4]);
        gdk::MemoryTexture::new(1, 1, gdk::MemoryFormat::B8g8r8a8Premultiplied, &bytes, 4).into()
    }

    #[test]
    fn the_page_index_follows_evictions() {
        let mut cache = TextureCache::new(100);
        cache.insert(key(0, 8), texture(), 40);
        cache.insert(key(0, 16), texture(), 40);
        cache.insert(key(1, 8), texture(), 40);
        assert!(!cache.contains(&key(0, 8)), "the oldest entry was evicted");
        assert_eq!(cache.keys_of_page(0), &[key(0, 16)]);
        assert_eq!(cache.keys_of_page(1), &[key(1, 8)]);
        assert_eq!(cache.used(), 80);

        // A use protects an entry from the next eviction.
        assert!(cache.get(&key(0, 16)).is_some());
        cache.insert(key(2, 8), texture(), 40);
        assert!(cache.contains(&key(0, 16)) && !cache.contains(&key(1, 8)));

        cache.retain(|k| k.page != 0);
        assert!(cache.keys_of_page(0).is_empty());
        assert_eq!(cache.len(), 1);
        assert!(cache.used() <= cache.cap());

        // Replacing a key keeps one index entry.
        cache.insert(key(2, 8), texture(), 30);
        assert_eq!(cache.keys_of_page(2), &[key(2, 8)]);
        assert_eq!(cache.used(), 30);
    }

    #[test]
    fn kept_textures_are_never_evicted_for_others() {
        let mut cache = TextureCache::new(100);
        let on_screen = |k: &PageKey| k.page <= 1 && k.scale_q == 16;
        cache.insert(key(5, 8), texture(), 40);
        cache.insert_keeping(key(0, 16), texture(), 40, on_screen);
        // Page 5 was used last, but it is not kept, so it makes room for the second kept page.
        assert!(cache.get(&key(5, 8)).is_some());
        assert!(cache.insert_keeping(key(1, 16), texture(), 40, on_screen));
        assert!(cache.contains(&key(0, 16)) && cache.contains(&key(1, 16)));
        assert!(!cache.contains(&key(5, 8)));
        // A texture that is not kept and does not fit beside the kept ones is dropped.
        assert!(!cache.insert_keeping(key(2, 16), texture(), 30, on_screen));
        assert!(!cache.contains(&key(2, 16)) && cache.used() == 80);
        // One that fits evicts only textures that are not kept.
        assert!(cache.insert_keeping(key(2, 16), texture(), 20, on_screen));
        assert_eq!(cache.used(), 100);

        // Kept textures that need more than the cap all stay; the next insert gives the room
        // back once they are no longer kept.
        let three = |k: &PageKey| k.page <= 2 && k.scale_q == 16;
        assert!(cache.insert_keeping(key(2, 16), texture(), 40, three));
        assert_eq!((cache.used(), cache.cap()), (120, 100));
        assert!((0..3).all(|p| cache.contains(&key(p, 16))));
        cache.insert_keeping(key(3, 16), texture(), 10, |k| k.page == 3);
        assert!(cache.used() <= 100 && cache.contains(&key(3, 16)));
    }

    #[test]
    fn a_lower_cap_evicts_the_oldest() {
        let mut cache = TextureCache::new(100);
        for page in 0..3 {
            cache.insert(key(page, 8), texture(), 30);
        }
        cache.get(&key(0, 8));
        cache.set_cap(60);
        assert_eq!((cache.cap(), cache.used()), (60, 60));
        assert!(cache.contains(&key(0, 8)) && cache.contains(&key(2, 8)));
        assert!(!cache.contains(&key(1, 8)) && cache.keys_of_page(1).is_empty());
        cache.set_cap(100);
        cache.insert(key(1, 8), texture(), 30);
        assert_eq!(cache.used(), 90);
    }
}
