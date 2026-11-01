//! Just enough ELF to read `DT_NEEDED`: which shared libraries a binary
//! links against. Feeds the soname-rebuild check (a stream bumping
//! `libfoo.so.3` to `.so.4` must rebuild everything that needs `.so.3`).
//! Little-endian ELF32 and ELF64; anything else is reported as having no
//! dependencies rather than failing.

pub const MAGIC: &[u8; 4] = b"\x7fELF";

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}
fn u64le(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap()))
}

fn cstr(b: &[u8], o: usize) -> Option<&str> {
    let s = b.get(o..)?;
    let end = s.iter().position(|&c| c == 0)?;
    std::str::from_utf8(&s[..end]).ok()
}

/// Sonames listed in the dynamic section, in order, or empty when the file
/// is not a dynamically linked little-endian ELF object.
pub fn needed(bytes: &[u8]) -> Vec<String> {
    needed_inner(bytes).unwrap_or_default()
}

fn needed_inner(b: &[u8]) -> Option<Vec<String>> {
    if b.len() < 64 || &b[..4] != MAGIC || b[5] != 1 {
        return None; // not ELF, or big-endian
    }
    let class64 = match b[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    // Section header table.
    let (shoff, shentsize, shnum) = if class64 {
        (u64le(b, 0x28)? as usize, u16le(b, 0x3a)? as usize, u16le(b, 0x3c)? as usize)
    } else {
        (u32le(b, 0x20)? as usize, u16le(b, 0x2e)? as usize, u16le(b, 0x30)? as usize)
    };
    if shentsize == 0 || shnum == 0 {
        return None;
    }
    struct Sh {
        kind: u32,
        off: usize,
        size: usize,
        link: usize,
    }
    let read_sh = |i: usize| -> Option<Sh> {
        let o = shoff + i * shentsize;
        Some(if class64 {
            Sh {
                kind: u32le(b, o + 4)?,
                off: u64le(b, o + 0x18)? as usize,
                size: u64le(b, o + 0x20)? as usize,
                link: u32le(b, o + 0x28)? as usize,
            }
        } else {
            Sh {
                kind: u32le(b, o + 4)?,
                off: u32le(b, o + 0x10)? as usize,
                size: u32le(b, o + 0x14)? as usize,
                link: u32le(b, o + 0x18)? as usize,
            }
        })
    };
    const SHT_DYNAMIC: u32 = 6;
    const DT_NEEDED: u64 = 1;
    let mut out = Vec::new();
    for i in 0..shnum {
        let sh = read_sh(i)?;
        if sh.kind != SHT_DYNAMIC {
            continue;
        }
        let strtab = read_sh(sh.link)?;
        let strs = b.get(strtab.off..strtab.off.checked_add(strtab.size)?)?;
        let entry = if class64 { 16 } else { 8 };
        let dynamic = b.get(sh.off..sh.off.checked_add(sh.size)?)?;
        for e in dynamic.chunks_exact(entry) {
            let (tag, val) = if class64 {
                (u64le(e, 0)?, u64le(e, 8)? as usize)
            } else {
                (u32le(e, 0)? as u64, u32le(e, 4)? as usize)
            };
            if tag == 0 {
                break; // DT_NULL
            }
            if tag == DT_NEEDED
                && let Some(s) = cstr(strs, val)
                && !s.is_empty()
            {
                out.push(s.to_string());
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_elf_has_no_needed() {
        assert!(needed(b"#!/bin/sh\necho hi\n").is_empty());
        assert!(needed(&[0x7f, b'E', b'L', b'F']).is_empty());
    }

    #[test]
    fn this_test_binary_needs_libc() {
        // Rust test binaries on *-linux-gnu link libc dynamically.
        let exe = std::env::current_exe().unwrap();
        let bytes = std::fs::read(exe).unwrap();
        let n = needed(&bytes);
        assert!(n.iter().any(|s| s.starts_with("libc.so")), "{n:?}");
        assert!(n.iter().all(|s| s.contains(".so")), "{n:?}");
    }
}
