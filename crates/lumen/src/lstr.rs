//! The engine string: a thin refcounted UTF-8 buffer with spare capacity.
//!
//! `Value::Str` used to hold `Rc<str>` — immutable and exactly sized, which makes an append loop
//! (`s += x`, astring's `this.output += e`) inherently O(n²): every step materializes a fresh
//! allocation of the whole accumulated string. `LStr` is the classic engine fix (QuickJS's
//! JSString): `{strong, len, cap}` header + bytes, one thin pointer. When a string is *uniquely
//! referenced* an append writes in place (amortized by capacity doubling); shared strings copy
//! first, exactly like `Rc::make_mut`.
//!
//! The payload is a single 8-byte pointer with the strong count at offset 0 — the same shape the
//! JIT's inline templates already assume for refcounted payloads (`Rc`'s RcBox), so the machine
//! code that bumps/decrements tag-6 values is unchanged. Logical content is always `len` bytes of
//! valid UTF-8 (lone surrogates smuggled, as before — see [`crate::jstr`]); capacity beyond `len`
//! is invisible to every reader because `Deref` slices to `len`.
//!
//! Like `Rc`, `LStr` is neither `Send` nor `Sync` (non-atomic count; the engine is one thread
//! per realm).

use std::alloc::{alloc, dealloc, Layout};
use std::cell::Cell;
use std::ptr::NonNull;

#[repr(C)]
struct Header {
    /// Strong count — MUST stay the first field (the JIT bumps it at payload offset 0).
    strong: Cell<usize>,
    len: Cell<u32>,
    cap: Cell<u32>,
    // `cap` bytes of UTF-8 follow.
}

/// See the module docs. `repr(transparent)`-thin: one pointer.
pub struct LStr {
    p: NonNull<Header>,
}

const HDR: usize = std::mem::size_of::<Header>();

/// Byte offset of the length within the header — the JIT's inline equality/truthiness templates
/// read `len` from machine code through the stored pointer (the strong count stays at offset 0).
pub(crate) const LEN_OFF: usize = std::mem::offset_of!(Header, len);
/// Byte offset of `cap` (which carries [`ASCII_HINT`] in its top bit) — the JIT's charCodeAt
/// intrinsic tests the hint from machine code.
pub(crate) const CAP_OFF: usize = std::mem::offset_of!(Header, cap);
/// Byte offset of the first content byte.
pub(crate) const DATA_OFF: usize = HDR;
/// Top bit of `cap`: the content is KNOWN all-ASCII (byte index == UTF-16 unit index, and every
/// byte IS its unit). Purely a hint — never set for non-ASCII content, may be clear for ASCII
/// content. Maintained by every constructor/mutator; capacity readers mask it off.
pub(crate) const ASCII_HINT: u32 = 1 << 31;

fn layout(cap: u32) -> Layout {
    Layout::from_size_align(HDR + cap as usize, std::mem::align_of::<Header>())
        .expect("string too large")
}

impl LStr {
    /// Allocate with `cap` bytes of capacity, seeding `content` (must fit).
    fn alloc(content: &str, cap: u32) -> LStr {
        Self::alloc_with_hint(content, cap, content.is_ascii())
    }

    /// Reuse representation metadata when copying an engine string. A conservative false
    /// hint is valid; never rescan a growing prefix just to rediscover that it is ASCII.
    fn alloc_with_hint(content: &str, cap: u32, ascii: bool) -> LStr {
        debug_assert!(content.len() <= cap as usize);
        assert!(cap & ASCII_HINT == 0, "capacity claims the hint bit");
        debug_assert!(!ascii || content.is_ascii());
        unsafe {
            let p = alloc(layout(cap)) as *mut Header;
            let p = NonNull::new(p).expect("allocation failed");
            // The hint holds for `content`; constructors that append more bytes afterwards
            // re-AND it with the extra bytes' ASCII-ness (see concat2/concat_grown).
            let hint = if ascii { ASCII_HINT } else { 0 };
            p.as_ptr().write(Header {
                strong: Cell::new(1),
                len: Cell::new(content.len() as u32),
                cap: Cell::new(cap | hint),
            });
            let data = (p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(content.as_ptr(), data, content.len());
            LStr { p }
        }
    }

    /// Whether the content is KNOWN all-ASCII (see [`ASCII_HINT`]).
    #[inline]
    pub(crate) fn ascii_hint(&self) -> bool {
        self.hdr().cap.get() & ASCII_HINT != 0
    }

    #[inline]
    fn and_ascii(&self, extra_is_ascii: bool) {
        if !extra_is_ascii {
            let h = self.hdr();
            h.cap.set(h.cap.get() & !ASCII_HINT);
        }
    }

    /// The two halves concatenated (a single copy of each into the result).
    pub fn concat2(a: &str, b: &str) -> LStr {
        let total = a.len() + b.len();
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(a.as_ptr(), data, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), data.add(a.len()), b.len());
            s.hdr().len.set(total as u32);
        }
        s.and_ascii(a.is_ascii() && b.is_ascii());
        s
    }

    /// The three pieces concatenated into one engine allocation.
    #[inline]
    pub(crate) fn concat3(a: &str, b: &str, c: &str) -> LStr {
        let total = a
            .len()
            .checked_add(b.len())
            .and_then(|total| total.checked_add(c.len()))
            .expect("string too large");
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(a.as_ptr(), data, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), data.add(a.len()), b.len());
            std::ptr::copy_nonoverlapping(c.as_ptr(), data.add(a.len() + b.len()), c.len());
            s.hdr().len.set(total as u32);
        }
        s.and_ascii(a.is_ascii() && b.is_ascii() && c.is_ascii());
        s
    }

    /// The four pieces concatenated into one engine allocation.
    #[inline]
    pub(crate) fn concat4(a: &str, b: &str, c: &str, d: &str) -> LStr {
        let total = a
            .len()
            .checked_add(b.len())
            .and_then(|total| total.checked_add(c.len()))
            .and_then(|total| total.checked_add(d.len()))
            .expect("string too large");
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(a.as_ptr(), data, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), data.add(a.len()), b.len());
            std::ptr::copy_nonoverlapping(c.as_ptr(), data.add(a.len() + b.len()), c.len());
            std::ptr::copy_nonoverlapping(
                d.as_ptr(),
                data.add(a.len() + b.len() + c.len()),
                d.len(),
            );
            s.hdr().len.set(total as u32);
        }
        s.and_ascii(a.is_ascii() && b.is_ascii() && c.is_ascii() && d.is_ascii());
        s
    }

    /// Concatenate already-coerced strings into one engine allocation.
    pub(crate) fn concat_many(parts: &[LStr], total: usize) -> LStr {
        let s = LStr::alloc("", u32::try_from(total).expect("string too large"));
        let mut offset = 0;
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            for part in parts {
                let bytes = part.as_str().as_bytes();
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.add(offset), bytes.len());
                offset += bytes.len();
            }
        }
        debug_assert_eq!(offset, total);
        s.hdr().len.set(total as u32);
        s.and_ascii(parts.iter().all(LStr::ascii_hint));
        s
    }

    #[inline]
    fn hdr(&self) -> &Header {
        unsafe { self.p.as_ref() }
    }

    #[inline]
    fn data(&self) -> *const u8 {
        unsafe { (self.p.as_ptr() as *const u8).add(HDR) }
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        unsafe {
            let bytes = std::slice::from_raw_parts(self.data(), self.hdr().len.get() as usize);
            std::str::from_utf8_unchecked(bytes)
        }
    }

    /// Pointer identity (cache keys — same contract as `Rc::as_ptr`).
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.p.as_ptr() as *const u8
    }

    #[inline]
    pub fn ptr_eq(a: &LStr, b: &LStr) -> bool {
        a.p == b.p
    }

    #[inline]
    pub fn strong_count(&self) -> usize {
        self.hdr().strong.get()
    }

    /// Requested bytes retained by this allocation, excluding allocator rounding.
    ///
    /// This includes Lumen's stable string header because [`LStr`] allocates that header and its
    /// byte capacity as one explicit allocation. It intentionally does not include allocator
    /// metadata outside that request.
    pub(crate) fn retained_requested_bytes(&self) -> usize {
        HDR.saturating_add((self.hdr().cap.get() & !ASCII_HINT) as usize)
    }

    /// Append in place when this is the ONLY reference and capacity suffices. Returns false
    /// (without modifying anything) otherwise — the caller copies. The unique-owner requirement
    /// is what makes the mutation invisible: no other handle can observe the content, and the
    /// caller must not hold a `&str` borrow of `self` across the call (enforced by `&mut self`).
    pub fn append_in_place(&mut self, x: &str) -> bool {
        self.append_with_hint(x, x.is_ascii())
    }

    fn append_with_hint(&mut self, x: &str, ascii: bool) -> bool {
        let h = self.hdr();
        if h.strong.get() != 1 {
            return false;
        }
        let len = h.len.get() as usize;
        if len + x.len() > (h.cap.get() & !ASCII_HINT) as usize {
            return false;
        }
        unsafe {
            let data = (self.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(x.as_ptr(), data.add(len), x.len());
        }
        h.len.set((len + x.len()) as u32);
        self.and_ascii(ascii);
        true
    }

    /// `self + x` with growth capacity: used by the fused append ops when in-place didn't apply.
    /// Doubles (at least) so a rebuilt accumulator amortizes the next appends.
    pub fn concat_grown(&self, x: &str) -> LStr {
        self.grow_with_hint(x, x.is_ascii())
    }

    fn grow_with_hint(&self, x: &str, ascii: bool) -> LStr {
        let need = self.len().checked_add(x.len()).expect("string too large");
        assert!(need < ASCII_HINT as usize, "string too large");
        let cap = need
            .saturating_mul(2)
            .max(32)
            .min((ASCII_HINT - 1) as usize) as u32;
        let s = LStr::alloc_with_hint(self.as_str(), cap, self.ascii_hint());
        unsafe {
            let data = (s.p.as_ptr() as *mut u8).add(HDR);
            std::ptr::copy_nonoverlapping(x.as_ptr(), data.add(self.as_str().len()), x.len());
        }
        s.hdr().len.set(need as u32);
        s.and_ascii(ascii);
        s
    }

    /// ECMAScript code-unit concatenation after the caller's coercions and length check.
    /// Shared by all execution tiers and String#concat. Cached ASCII hints eliminate prefix
    /// rescans; moving the left handle also permits amortized appends to unique temporaries.
    /// Aliases (including `s + s`) keep the strong count above one and force a separate buffer.
    pub(crate) fn concat_owned(mut self, right: &LStr) -> LStr {
        if right.is_empty() {
            return self;
        }
        if self.is_empty() {
            return right.clone();
        }
        if !self.ascii_hint() && !right.ascii_hint() && crate::jstr::needs_join_fixup(&self, right)
        {
            return crate::jstr::concat(&self, right).into();
        }
        if self.append_with_hint(right.as_str(), right.ascii_hint()) {
            self
        } else {
            self.grow_with_hint(right.as_str(), right.ascii_hint())
        }
    }

    /// Repeat the byte representation directly into one engine allocation.
    ///
    /// The caller has already performed the observable `String.prototype.repeat` coercions and
    /// length check (ECMA-262 §22.1.3.18).  Keeping this representation-local avoids first
    /// building a Rust `String` and then copying it into an `LStr`.  The doubling copy uses
    /// `ptr::copy`, which is explicitly overlap-safe as each completed prefix becomes the source
    /// for the next block.
    pub(crate) fn repeat_direct(&self, count: usize) -> LStr {
        let source_len = self.hdr().len.get() as usize;
        let total = source_len
            .checked_mul(count)
            .expect("repeat length checked by caller");
        let repeated = LStr::alloc("", u32::try_from(total).expect("string too large"));
        if source_len == 0 || count == 0 {
            return repeated;
        }
        unsafe {
            let destination = repeated.p.as_ptr().cast::<u8>().add(HDR);
            std::ptr::copy_nonoverlapping(self.data(), destination, source_len);
            let mut filled = source_len;
            while filled < total {
                let copy_len = filled.min(total - filled);
                std::ptr::copy(destination, destination.add(filled), copy_len);
                filled += copy_len;
            }
            repeated.hdr().len.set(total as u32);
        }
        repeated.and_ascii(self.ascii_hint());
        repeated
    }

    /// Repeat an ASCII string directly into one engine allocation.
    #[inline]
    pub(crate) fn repeat_ascii(&self, count: usize) -> LStr {
        debug_assert!(self.ascii_hint());
        self.repeat_direct(count)
    }

    /// Build an ASCII-padded string directly in its final engine allocation. The caller has
    /// already applied StringPad's UTF-16 length and coercion rules, and both operands are known
    /// ASCII, so repeating/truncating the fill cannot require surrogate fixup.
    pub(crate) fn pad_ascii(source: &str, pad: &str, need: usize, at_start: bool) -> LStr {
        debug_assert!(source.is_ascii() && pad.is_ascii() && !pad.is_empty());
        let total = source.len() + need;
        let padded = LStr::alloc("", u32::try_from(total).expect("string too large"));
        unsafe {
            let destination = padded.p.as_ptr().cast::<u8>().add(HDR);
            let (fill_start, source_start) = if at_start {
                (0, need)
            } else {
                (source.len(), 0)
            };
            let mut offset = fill_start;
            while offset < fill_start + need {
                let copy_len = (fill_start + need - offset).min(pad.len());
                std::ptr::copy_nonoverlapping(pad.as_ptr(), destination.add(offset), copy_len);
                offset += copy_len;
            }
            std::ptr::copy_nonoverlapping(
                source.as_ptr(),
                destination.add(source_start),
                source.len(),
            );
        }
        padded.hdr().len.set(total as u32);
        padded.and_ascii(true);
        padded
    }

    /// Map ASCII letters directly into one engine allocation. The caller must have already
    /// selected the ASCII representation; non-ASCII case mappings (including one-to-many and
    /// context-sensitive mappings) remain on the Unicode implementation path.
    pub(crate) fn map_ascii_case(&self, upper: bool) -> LStr {
        debug_assert!(self.ascii_hint());
        let source = self.as_str().as_bytes();
        let mapped = LStr::alloc("", u32::try_from(source.len()).expect("string too large"));
        unsafe {
            let destination = mapped.p.as_ptr().cast::<u8>().add(HDR);
            for (index, &byte) in source.iter().enumerate() {
                let byte = if upper {
                    byte.to_ascii_uppercase()
                } else {
                    byte.to_ascii_lowercase()
                };
                destination.add(index).write(byte);
            }
        }
        mapped.hdr().len.set(source.len() as u32);
        mapped
    }
}

impl Clone for LStr {
    #[inline]
    fn clone(&self) -> LStr {
        let h = self.hdr();
        h.strong.set(h.strong.get() + 1);
        LStr { p: self.p }
    }
}

impl Drop for LStr {
    #[inline]
    fn drop(&mut self) {
        let h = self.hdr();
        let s = h.strong.get();
        if s == 1 {
            let cap = h.cap.get() & !ASCII_HINT;
            unsafe { dealloc(self.p.as_ptr() as *mut u8, layout(cap)) };
        } else {
            h.strong.set(s - 1);
        }
    }
}

impl std::ops::Deref for LStr {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::borrow::Borrow<str> for LStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for LStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for LStr {
    fn from(s: &str) -> LStr {
        LStr::alloc(s, u32::try_from(s.len()).expect("string too large"))
    }
}

impl From<String> for LStr {
    fn from(s: String) -> LStr {
        LStr::from(s.as_str())
    }
}

impl From<std::rc::Rc<str>> for LStr {
    fn from(s: std::rc::Rc<str>) -> LStr {
        LStr::from(&*s)
    }
}

impl From<&String> for LStr {
    fn from(s: &String) -> LStr {
        LStr::from(s.as_str())
    }
}

impl From<char> for LStr {
    fn from(c: char) -> LStr {
        LStr::from(c.encode_utf8(&mut [0u8; 4]) as &str)
    }
}

impl PartialEq for LStr {
    fn eq(&self, other: &LStr) -> bool {
        LStr::ptr_eq(self, other) || self.as_str() == other.as_str()
    }
}
impl Eq for LStr {}

impl PartialEq<str> for LStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl std::hash::Hash for LStr {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl std::fmt::Display for LStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for LStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl From<&LStr> for std::rc::Rc<str> {
    fn from(s: &LStr) -> std::rc::Rc<str> {
        std::rc::Rc::from(s.as_str())
    }
}

impl From<LStr> for std::rc::Rc<str> {
    fn from(s: LStr) -> std::rc::Rc<str> {
        std::rc::Rc::from(s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_concat_reuses_unique_capacity_and_preserves_aliases() {
        let mut s = LStr::from("a").concat_owned(&LStr::from("b"));
        let p = s.as_ptr();
        s = s.concat_owned(&LStr::from("c"));
        assert_eq!(s.as_ptr(), p);
        assert_eq!(s.as_str(), "abc");
        assert!(s.ascii_hint());
        let alias = s.clone();
        s = s.concat_owned(&LStr::from("d"));
        assert!(!LStr::ptr_eq(&s, &alias));
        assert_eq!(alias.as_str(), "abc");
        assert_eq!(s.as_str(), "abcd");
        let right = s.clone();
        s = s.concat_owned(&right);
        assert_eq!(right.as_str(), "abcd");
        assert_eq!(s.as_str(), "abcdabcd");
    }

    #[test]
    fn owned_concat_retains_conservative_hints_and_clears_for_unicode() {
        let s = LStr::alloc_with_hint("abc", 3, false);
        let s = s.concat_owned(&LStr::from("d"));
        assert!(!s.ascii_hint(), "growth must not rescan the prefix");
        assert_eq!(s.as_str(), "abcd");
        let s = LStr::from("abc").concat_owned(&LStr::alloc_with_hint("d", 1, false));
        assert!(!s.ascii_hint(), "right hint must also be preserved");
        let s = LStr::from("a").concat_owned(&LStr::from("b"));
        let p = s.as_ptr();
        let s = s.concat_owned(&LStr::from("é"));
        assert_eq!(s.as_ptr(), p);
        assert!(!s.ascii_hint());
        assert_eq!(s.as_str(), "abé");
    }

    #[test]
    fn owned_concat_empty_and_surrogate_boundaries() {
        let s = LStr::from("abc");
        let p = s.as_ptr();
        let s = s.concat_owned(&LStr::from(""));
        assert_eq!(s.as_ptr(), p);
        assert!(LStr::ptr_eq(&LStr::from("").concat_owned(&s), &s));
        let high = LStr::from(crate::jstr::from_units(&[0xD834]));
        let low = LStr::from(crate::jstr::from_units(&[0xDF06]));
        assert_eq!(high.concat_owned(&low).as_str(), "𝌆");
        assert_eq!(std::mem::size_of::<LStr>(), std::mem::size_of::<usize>());
        assert_eq!(LEN_OFF, std::mem::size_of::<usize>());
        assert_eq!(CAP_OFF, LEN_OFF + 4);
    }
}
