//! Just enough flattened-device-tree reading and in-place patching for the
//! CLI runner.
//!
//! The runner needs to tell the guest where it put the initrd, and those
//! addresses are not known until load time — they depend on the kernel's
//! footprint and the DTB's own length. Two ways to do that: rebuild the FDT
//! in Rust with the properties appended, or ship them in `guest.dts` as
//! placeholders and overwrite the cells in place.
//!
//! This is the second. Appending to an FDT means growing the struct block,
//! adding to the string block, and fixing up every offset in the header —
//! a real FDT writer, for two `u64`s. Overwriting a property's value costs
//! a walk and a `copy_from_slice`, changes no length, and leaves the blob
//! structurally identical to the one `dtc` emitted and `tests/dtb.rs`
//! checked. The cost is that `guest.dts` has to carry the placeholders,
//! which is why they are `<0x0 0x0>`: `early_init_dt_check_for_initrd()`
//! reads them, computes `phys_initrd_size = end - start == 0`, and
//! `reserve_initrd_mem()` returns immediately on `!phys_initrd_size`. A run
//! with no `--initrd` therefore leaves an inert pair of properties rather
//! than a dangling pointer.
//!
//! The struct block is a flat token stream (DT spec §5.4) and is trivial to
//! walk. This walker is the one `tests/dtb.rs` uses too — it was written
//! there first, and lives here now so the property lookup the runner
//! depends on is the same code the device-tree test exercises, rather than
//! a hand-copy that can drift from it.

use core::ops::Range;

const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;

/// FDT magic (`0xd00dfeed`, big-endian) at offset 0 — the one structural
/// fact every FDT consumer checks before touching the rest of the blob.
pub const FDT_MAGIC: u32 = 0xd00d_feed;

fn be32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

/// Byte range, within `blob`, of the *value* of property `name` on node
/// `path` (a full path such as `/chosen`). The range is what
/// [`set_u64`] overwrites and what [`prop`] reads.
pub fn prop_range(blob: &[u8], path: &str, name: &str) -> Option<Range<usize>> {
    if be32(blob, 0)? != FDT_MAGIC {
        return None;
    }
    let off_struct = be32(blob, 8)? as usize;
    let off_strings = be32(blob, 12)? as usize;
    let size_struct = be32(blob, 36)? as usize;
    let s = blob.get(off_struct..off_struct + size_struct)?;

    let cstr = |b: &[u8], at: usize| -> Option<usize> {
        Some(b.get(at..)?.iter().position(|&c| c == 0)? + at)
    };

    // The root node's name is the empty string, so a node at depth d has
    // path "/" + the names below the root joined by "/".
    let mut stack: Vec<&str> = Vec::new();
    let mut i = 0usize;
    while i + 4 <= s.len() {
        let token = be32(s, i)?;
        i += 4;
        match token {
            FDT_BEGIN_NODE => {
                let end = cstr(s, i)?;
                stack.push(core::str::from_utf8(&s[i..end]).ok()?);
                i = (end + 1).next_multiple_of(4);
            }
            FDT_END_NODE => {
                stack.pop();
            }
            FDT_PROP => {
                let len = be32(s, i)? as usize;
                let nameoff = off_strings + be32(s, i + 4)? as usize;
                i += 8;
                // `len` comes from the blob, so a malformed tree can claim
                // a property longer than the file. The struct-block slice
                // `s` is bounds-checked, but the range returned here indexes
                // `blob` directly — leaving it unchecked turns a bad `--dtb`
                // into a panic instead of the `None` every other malformed
                // input in this module yields.
                let value = off_struct + i..off_struct.checked_add(i + len)?;
                if value.end > blob.len() {
                    return None;
                }
                i = (i + len).next_multiple_of(4);

                let pname = core::str::from_utf8(&blob[nameoff..cstr(blob, nameoff)?]).ok()?;
                if pname == name && stack.len() > 1 && format!("/{}", stack[1..].join("/")) == path
                {
                    return Some(value);
                }
            }
            FDT_NOP => {}
            // FDT_END, or anything unrecognised: the walk is over.
            _ => break,
        }
    }
    None
}

/// The value of property `name` on node `path`, if present.
pub fn prop<'a>(blob: &'a [u8], path: &str, name: &str) -> Option<&'a [u8]> {
    prop_range(blob, path, name).map(|r| &blob[r])
}

/// Overwrites an existing 8-byte property with `value`, big-endian.
///
/// Deliberately refuses to resize: a property whose length is not exactly 8
/// is not the two-cell `u64` this was written for, and silently writing 8
/// bytes over (say) a 4-byte cell would corrupt whatever property follows
/// it in the struct block.
pub fn set_u64(blob: &mut [u8], path: &str, name: &str, value: u64) -> Result<(), String> {
    let r = prop_range(blob, path, name)
        .ok_or_else(|| format!("the device tree has no `{name}` property on `{path}`"))?;
    if r.len() != 8 {
        return Err(format!(
            "`{name}` on `{path}` is {} bytes; expected an 8-byte (two-cell) value",
            r.len()
        ));
    }
    blob[r].copy_from_slice(&value.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built FDT with one `/chosen` node carrying an 8-byte and a
    /// 4-byte property, so the walker and the width check can both be
    /// exercised without shelling out to `dtc`. (`tests/dtb.rs` covers the
    /// real compiled tree.)
    fn fixture() -> Vec<u8> {
        let mut strings = Vec::new();
        let off = |s: &mut Vec<u8>, name: &str| -> u32 {
            let at = s.len() as u32;
            s.extend_from_slice(name.as_bytes());
            s.push(0);
            at
        };
        let n_start = off(&mut strings, "linux,initrd-start");
        let n_narrow = off(&mut strings, "narrow");

        let mut st = Vec::new();
        let tok = |st: &mut Vec<u8>, t: u32| st.extend_from_slice(&t.to_be_bytes());
        // root node, empty name
        tok(&mut st, FDT_BEGIN_NODE);
        st.extend_from_slice(&[0, 0, 0, 0]);
        // /chosen
        tok(&mut st, FDT_BEGIN_NODE);
        st.extend_from_slice(b"chosen\0\0");
        tok(&mut st, FDT_PROP);
        st.extend_from_slice(&8u32.to_be_bytes());
        st.extend_from_slice(&n_start.to_be_bytes());
        st.extend_from_slice(&0u64.to_be_bytes());
        tok(&mut st, FDT_PROP);
        st.extend_from_slice(&4u32.to_be_bytes());
        st.extend_from_slice(&n_narrow.to_be_bytes());
        st.extend_from_slice(&0u32.to_be_bytes());
        tok(&mut st, FDT_END_NODE);
        tok(&mut st, FDT_END_NODE);
        tok(&mut st, 9); // FDT_END

        let off_struct = 64usize;
        let off_strings = off_struct + st.len();
        let total = off_strings + strings.len();
        let mut blob = vec![0u8; total];
        blob[0..4].copy_from_slice(&FDT_MAGIC.to_be_bytes());
        blob[4..8].copy_from_slice(&(total as u32).to_be_bytes());
        blob[8..12].copy_from_slice(&(off_struct as u32).to_be_bytes());
        blob[12..16].copy_from_slice(&(off_strings as u32).to_be_bytes());
        blob[36..40].copy_from_slice(&(st.len() as u32).to_be_bytes());
        blob[off_struct..off_strings].copy_from_slice(&st);
        blob[off_strings..].copy_from_slice(&strings);
        blob
    }

    #[test]
    fn set_u64_overwrites_the_value_in_place_without_changing_the_length() {
        let mut blob = fixture();
        let before = blob.len();
        set_u64(&mut blob, "/chosen", "linux,initrd-start", 0x8050_0000).unwrap();
        assert_eq!(blob.len(), before, "patching must not resize the blob");
        assert_eq!(
            prop(&blob, "/chosen", "linux,initrd-start").unwrap(),
            &0x8050_0000u64.to_be_bytes()
        );
    }

    /// A missing property must be a diagnosable error, not a silent no-op:
    /// silently skipping it would boot a kernel that looks for an initrd
    /// at address zero.
    #[test]
    fn set_u64_reports_a_missing_property() {
        let mut blob = fixture();
        let err = set_u64(&mut blob, "/chosen", "linux,initrd-end", 1).unwrap_err();
        assert!(err.contains("linux,initrd-end"), "unhelpful message: {err}");
    }

    /// Writing eight bytes over a four-byte property would silently corrupt
    /// whichever property follows it in the struct block.
    #[test]
    fn set_u64_refuses_a_property_that_is_not_two_cells_wide() {
        let mut blob = fixture();
        let err = set_u64(&mut blob, "/chosen", "narrow", 1).unwrap_err();
        assert!(err.contains("4 bytes"), "unhelpful message: {err}");
    }

    #[test]
    fn a_blob_without_the_fdt_magic_yields_nothing_rather_than_panicking() {
        assert!(prop(b"not an fdt at all, really", "/chosen", "bootargs").is_none());
    }

    /// `--dtb` is a user-supplied file, so a property whose declared length
    /// runs off the end of the blob is reachable input, not a theoretical
    /// one. It must read as "no such property", the same as every other
    /// malformed shape here — not as an out-of-bounds index.
    #[test]
    fn a_property_longer_than_the_blob_yields_nothing_rather_than_panicking() {
        let mut blob = fixture();
        // Find the 8-byte property's length field: FDT_PROP is followed by
        // len, then nameoff. Overstate the length by a wide margin.
        let r = prop_range(&blob, "/chosen", "linux,initrd-start").unwrap();
        blob[r.start - 8..r.start - 4].copy_from_slice(&0xFFFF_u32.to_be_bytes());

        assert!(prop(&blob, "/chosen", "linux,initrd-start").is_none());
        assert!(set_u64(&mut blob, "/chosen", "linux,initrd-start", 1).is_err());
    }

    /// The same, but with a length large enough that `off_struct + i + len`
    /// would wrap rather than merely exceed the blob.
    #[test]
    fn a_property_length_that_overflows_yields_nothing_rather_than_panicking() {
        let mut blob = fixture();
        let r = prop_range(&blob, "/chosen", "linux,initrd-start").unwrap();
        blob[r.start - 8..r.start - 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(prop(&blob, "/chosen", "linux,initrd-start").is_none());
    }
}
