//! Checked, allocation-free access to the generated little-endian u32 index.
//! Strings and PNGs stay in immutable byte blobs; no per-entry relocations.
use super::Entry;
use std::sync::OnceLock;

const DATA: &[u8] = include_bytes!("../../../assets/emoji/emoji-index.bin");
pub(super) const PNGS: &[u8] = include_bytes!("../../../assets/emoji/twemoji-17.0.3.bin");
const HEADER: usize = 32;
const ENTRY: usize = 44;
const LOOKUP: usize = 12;

#[derive(Clone, Copy)]
struct Index<'a> {
    records: &'a [u8],
    lookup: &'a [u8],
    strings: &'a [u8],
    png_len: usize,
}

fn word(bytes: &[u8], at: usize) -> Option<usize> {
    let raw: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
    usize::try_from(u32::from_le_bytes(raw)).ok()
}

impl<'a> Index<'a> {
    fn parse(bytes: &'a [u8], png_len: usize) -> Option<Self> {
        if bytes.get(..8)? != b"FCEMIDX1" {
            return None;
        }
        let entries = word(bytes, 8)?;
        let lookups = word(bytes, 12)?;
        let record_at = word(bytes, 16)?;
        let lookup_at = word(bytes, 20)?;
        let strings_at = word(bytes, 24)?;
        let strings_len = word(bytes, 28)?;
        if entries > usize::from(u16::MAX) + 1
            || record_at != HEADER
            || lookup_at != record_at.checked_add(entries.checked_mul(ENTRY)?)?
            || strings_at != lookup_at.checked_add(lookups.checked_mul(LOOKUP)?)?
            || bytes.len() != strings_at.checked_add(strings_len)?
        {
            return None;
        }
        Some(Self {
            records: bytes.get(record_at..lookup_at)?,
            lookup: bytes.get(lookup_at..strings_at)?,
            strings: bytes.get(strings_at..)?,
            png_len,
        })
    }

    fn len(self) -> usize {
        self.records.len() / ENTRY
    }

    fn string(self, record: &[u8], at: usize) -> Option<&'a str> {
        let start = word(record, at)?;
        let len = word(record, at.checked_add(4)?)?;
        std::str::from_utf8(self.strings.get(start..start.checked_add(len)?)?).ok()
    }

    fn entry(self, id: usize) -> Option<Entry<'a>> {
        let start = id.checked_mul(ENTRY)?;
        let record = self.records.get(start..start.checked_add(ENTRY)?)?;
        let base = word(record, 32)?;
        let offset = word(record, 36)?;
        let len = word(record, 40)?;
        if base > id || offset.checked_add(len)? > self.png_len {
            return None;
        }
        Some(Entry {
            text: self.string(record, 0)?,
            name: self.string(record, 8)?,
            aliases: self.string(record, 16)?,
            category: self.string(record, 24)?,
            base: u16::try_from(base).ok()?,
            offset,
            len,
        })
    }

    fn lookup(self, at: usize) -> Option<(&'a str, u16)> {
        let start = at.checked_mul(LOOKUP)?;
        let record = self.lookup.get(start..start.checked_add(LOOKUP)?)?;
        let id = word(record, 8)?;
        if id >= self.len() {
            return None;
        }
        Some((self.string(record, 0)?, u16::try_from(id).ok()?))
    }

    fn unicode(self, sequence: &str) -> Option<u16> {
        let (mut low, mut high) = (0, self.lookup.len() / LOOKUP);
        while low < high {
            let middle = low + (high - low) / 2;
            let (text, id) = self.lookup(middle)?;
            match text.cmp(sequence) {
                std::cmp::Ordering::Less => low = middle + 1,
                std::cmp::Ordering::Greater => high = middle,
                std::cmp::Ordering::Equal => return self.entry(usize::from(id)).map(|_| id),
            }
        }
        None
    }
}

fn index() -> Option<Index<'static>> {
    static INDEX: OnceLock<Option<Index<'static>>> = OnceLock::new();
    *INDEX.get_or_init(|| {
        let parsed = Index::parse(DATA, PNGS.len())?;
        if !(0..parsed.len()).all(|id| parsed.entry(id).is_some()) {
            return None;
        }
        let mut previous = None;
        for at in 0..parsed.lookup.len() / LOOKUP {
            let (text, _) = parsed.lookup(at)?;
            if previous.is_some_and(|previous| previous >= text) {
                return None;
            }
            previous = Some(text);
        }
        Some(parsed)
    })
}

#[derive(Clone, Copy)]
pub(crate) struct Entries;
impl Entries {
    #[cfg(test)]
    pub(crate) fn len(self) -> usize {
        index().map_or(0, Index::len)
    }
    pub(crate) fn get(self, id: usize) -> Option<Entry<'static>> {
        index()?.entry(id)
    }
    pub(crate) fn iter(self) -> impl Iterator<Item = Entry<'static>> {
        // Header/records/order are validated once without building a heap table.
        // Invalid assets yield no entries, so enumeration never renumbers IDs.
        let index = index();
        (0..index.map_or(0, Index::len)).filter_map(move |id| index?.entry(id))
    }
}

pub(super) fn unicode(sequence: &str) -> Option<u16> {
    index()?.unicode(sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_lookup_is_sorted_valid_and_searchable() {
        let index = Index::parse(DATA, PNGS.len()).unwrap();
        assert_eq!(index.len(), 3785);
        assert_eq!(index.lookup.len() / LOOKUP, 4737);
        let mut previous = None;
        for at in 0..index.lookup.len() / LOOKUP {
            let (text, id) = index.lookup(at).unwrap();
            assert!(previous.is_none_or(|previous| previous < text));
            assert!(index.entry(usize::from(id)).is_some());
            assert_eq!(index.unicode(text), Some(id));
            previous = Some(text);
        }
        assert_eq!(index.unicode(""), None);
        assert_eq!(index.unicode("not an emoji"), None);
        assert!(index.entry(index.len()).is_none());
        assert!(index.entry(usize::MAX).is_none());
        assert!(index.lookup(usize::MAX).is_none());
    }

    #[test]
    fn corrupt_headers_ranges_utf8_and_png_offsets_fail_closed() {
        for length in 0..HEADER {
            assert!(Index::parse(&DATA[..length], PNGS.len()).is_none());
        }
        for field in [8, 12, 16, 20, 24, 28] {
            let mut bad = DATA.to_vec();
            bad[field..field + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(Index::parse(&bad, PNGS.len()).is_none());
        }
        for field in [0, 4, 32, 36, 40] {
            let mut bad = DATA.to_vec();
            bad[HEADER + field..HEADER + field + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            assert!(Index::parse(&bad, PNGS.len()).unwrap().entry(0).is_none());
        }
        let mut bad = DATA.to_vec();
        let strings = word(DATA, 24).unwrap();
        bad[strings] = 0xff;
        assert!(Index::parse(&bad, PNGS.len()).unwrap().entry(0).is_none());
        let mut bad = DATA.to_vec();
        let lookup = word(DATA, 20).unwrap();
        bad[lookup + 8..lookup + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(Index::parse(&bad, PNGS.len()).unwrap().lookup(0).is_none());
        assert!(Index::parse(DATA, 0).unwrap().entry(0).is_none());
    }
}
