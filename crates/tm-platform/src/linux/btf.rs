//! Minimal pure-Rust BTF parser for `/sys/kernel/btf/vmlinux`.
//!
//! Resolves kernel function BTF type IDs (`vfs_read`, `vfs_write`, `vfs_readv`,
//! `vfs_writev`, `filp_close`) and struct member byte offsets at runtime so the
//! eBPF file-I/O tracer works across kernel versions without build-time C headers
//! or CO-RE relocations.

use std::collections::HashMap;
use std::path::Path;

const BTF_MAGIC: u16 = 0xeb9f;
const BTF_VERSION: u8 = 1;
const VMLINUX_BTF_PATH: &str = "/sys/kernel/btf/vmlinux";

const BTF_KIND_INT: u32 = 1;
const BTF_KIND_PTR: u32 = 2;
const BTF_KIND_ARRAY: u32 = 3;
const BTF_KIND_STRUCT: u32 = 4;
const BTF_KIND_UNION: u32 = 5;
const BTF_KIND_ENUM: u32 = 6;
const BTF_KIND_FWD: u32 = 7;
const BTF_KIND_TYPEDEF: u32 = 8;
const BTF_KIND_VOLATILE: u32 = 9;
const BTF_KIND_CONST: u32 = 10;
const BTF_KIND_RESTRICT: u32 = 11;
const BTF_KIND_FUNC: u32 = 12;
const BTF_KIND_FUNC_PROTO: u32 = 13;
const BTF_KIND_VAR: u32 = 14;
const BTF_KIND_DATASEC: u32 = 15;
const BTF_KIND_FLOAT: u32 = 16;
const BTF_KIND_DECL_TAG: u32 = 17;
const BTF_KIND_TYPE_TAG: u32 = 18;
const BTF_KIND_ENUM64: u32 = 19;

/// Kernel BTF IDs and struct field byte offsets needed by the VFS eBPF tracer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct KernelLayout {
    pub vfs_read_id: u32,
    pub vfs_write_id: u32,
    pub vfs_readv_id: u32,
    pub vfs_writev_id: u32,
    pub filp_close_id: Option<u32>,
    pub off_file_f_inode: u16,
    pub off_file_f_path: u16,
    pub off_inode_i_mode: u16,
    pub off_inode_i_ino: u16,
    pub off_inode_i_sb: u16,
    pub off_sb_s_dev: u16,
    pub off_task_group_leader: u16,
    pub off_task_start_boottime: u16,
}

#[derive(Clone, Copy)]
struct MemberRef {
    name_off: u32,
    type_id: u32,
    bit_offset: u32,
}

#[derive(Clone)]
enum TypeEntry {
    Struct {
        members_start: usize,
        members_len: usize,
    },
    Other,
}

/// Read and parse `/sys/kernel/btf/vmlinux` into [`KernelLayout`].
pub(super) fn load_vmlinux_layout() -> Result<KernelLayout, String> {
    load_layout_from_path(Path::new(VMLINUX_BTF_PATH))
}

pub(super) fn load_layout_from_path(path: &Path) -> Result<KernelLayout, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_btf_layout(&bytes)
}

pub(super) fn parse_btf_layout(bytes: &[u8]) -> Result<KernelLayout, String> {
    if bytes.len() < 24 {
        return Err("BTF header truncated".into());
    }
    let magic = read_u16(bytes, 0)?;
    if magic != BTF_MAGIC {
        return Err(format!("invalid BTF magic {magic:#x}"));
    }
    let version = bytes[2];
    if version != BTF_VERSION {
        return Err(format!("unsupported BTF version {version}"));
    }
    let hdr_len = usize::try_from(read_u32(bytes, 4)?).map_err(|_| "BTF hdr_len overflow")?;
    let type_off = usize::try_from(read_u32(bytes, 8)?).map_err(|_| "BTF type_off overflow")?;
    let type_len = usize::try_from(read_u32(bytes, 12)?).map_err(|_| "BTF type_len overflow")?;
    let str_off = usize::try_from(read_u32(bytes, 16)?).map_err(|_| "BTF str_off overflow")?;
    let str_len = usize::try_from(read_u32(bytes, 20)?).map_err(|_| "BTF str_len overflow")?;

    if hdr_len < 24 || hdr_len > bytes.len() {
        return Err("invalid BTF header length".into());
    }
    let type_start = hdr_len
        .checked_add(type_off)
        .ok_or("BTF type_start overflow")?;
    let type_end = type_start
        .checked_add(type_len)
        .filter(|end| *end <= bytes.len())
        .ok_or("BTF type section out of bounds")?;
    let str_start = hdr_len
        .checked_add(str_off)
        .ok_or("BTF str_start overflow")?;
    let str_end = str_start
        .checked_add(str_len)
        .filter(|end| *end <= bytes.len())
        .ok_or("BTF string section out of bounds")?;

    let strings = &bytes[str_start..str_end];
    let mut entries: Vec<TypeEntry> = Vec::with_capacity(4096);
    // Type IDs are 1-based (`0` is `void`).
    entries.push(TypeEntry::Other);
    let mut members: Vec<MemberRef> = Vec::with_capacity(16384);

    let mut wanted_funcs: HashMap<&'static str, u32> = HashMap::new();
    let mut wanted_structs: HashMap<&'static str, u32> = HashMap::new();

    let mut pos = type_start;
    let mut tid = 1u32;
    while pos < type_end {
        if pos + 12 > type_end {
            return Err("truncated BTF type record".into());
        }
        let name_off = read_u32(bytes, pos)?;
        let info = read_u32(bytes, pos + 4)?;
        let _size_or_type = read_u32(bytes, pos + 8)?;
        pos += 12;

        let kind = (info >> 24) & 0x1f;
        let vlen = usize::try_from(info & 0xffff).unwrap_or(0);
        let kflag = (info >> 31) != 0;

        let entry = match kind {
            BTF_KIND_INT | BTF_KIND_VAR | BTF_KIND_DECL_TAG => {
                pos = advance(pos, 4, type_end)?;
                TypeEntry::Other
            }
            BTF_KIND_PTR | BTF_KIND_FWD | BTF_KIND_TYPEDEF | BTF_KIND_VOLATILE | BTF_KIND_CONST
            | BTF_KIND_RESTRICT | BTF_KIND_FLOAT | BTF_KIND_TYPE_TAG => TypeEntry::Other,
            BTF_KIND_ARRAY => {
                pos = advance(pos, 12, type_end)?;
                TypeEntry::Other
            }
            BTF_KIND_ENUM | BTF_KIND_FUNC_PROTO => {
                pos = advance(pos, vlen * 8, type_end)?;
                TypeEntry::Other
            }
            BTF_KIND_DATASEC | BTF_KIND_ENUM64 => {
                pos = advance(pos, vlen * 12, type_end)?;
                TypeEntry::Other
            }
            BTF_KIND_FUNC => {
                if let Some(name) = btf_name(strings, name_off)
                    && matches!(
                        name,
                        "vfs_read" | "vfs_write" | "vfs_readv" | "vfs_writev" | "filp_close"
                    )
                {
                    wanted_funcs.entry(static_func_key(name)).or_insert(tid);
                }
                TypeEntry::Other
            }
            BTF_KIND_STRUCT | BTF_KIND_UNION => {
                let rec_bytes = vlen * 12;
                let next_pos = advance(pos, rec_bytes, type_end)?;
                let members_start = members.len();
                for idx in 0..vlen {
                    let base = pos + idx * 12;
                    let m_name_off = read_u32(bytes, base)?;
                    let m_type = read_u32(bytes, base + 4)?;
                    let m_off = read_u32(bytes, base + 8)?;
                    let bit_offset = if kflag { m_off & 0x00ff_ffff } else { m_off };
                    members.push(MemberRef {
                        name_off: m_name_off,
                        type_id: m_type,
                        bit_offset,
                    });
                }
                pos = next_pos;
                if kind == BTF_KIND_STRUCT
                    && let Some(name) = btf_name(strings, name_off)
                    && matches!(name, "file" | "inode" | "super_block" | "task_struct")
                {
                    wanted_structs.entry(static_struct_key(name)).or_insert(tid);
                }
                TypeEntry::Struct {
                    members_start,
                    members_len: vlen,
                }
            }
            other => return Err(format!("unknown BTF kind {other} at type {tid}")),
        };
        entries.push(entry);
        tid = tid.checked_add(1).ok_or("BTF type count overflow")?;
    }

    let req_func = |name: &str| -> Result<u32, String> {
        wanted_funcs
            .get(name)
            .copied()
            .ok_or_else(|| format!("BTF function '{name}' not found"))
    };
    let req_field = |struct_name: &str, field_name: &str| -> Result<u16, String> {
        let stid = wanted_structs
            .get(struct_name)
            .copied()
            .ok_or_else(|| format!("BTF struct '{struct_name}' not found"))?;
        find_member_byte_offset(&entries, &members, strings, stid, field_name, 0, 0)
            .ok_or_else(|| format!("BTF field '{struct_name}.{field_name}' not found"))
    };

    Ok(KernelLayout {
        vfs_read_id: req_func("vfs_read")?,
        vfs_write_id: req_func("vfs_write")?,
        vfs_readv_id: req_func("vfs_readv")?,
        vfs_writev_id: req_func("vfs_writev")?,
        filp_close_id: wanted_funcs.get("filp_close").copied(),
        off_file_f_inode: req_field("file", "f_inode")?,
        off_file_f_path: req_field("file", "f_path")?,
        off_inode_i_mode: req_field("inode", "i_mode")?,
        off_inode_i_ino: req_field("inode", "i_ino")?,
        off_inode_i_sb: req_field("inode", "i_sb")?,
        off_sb_s_dev: req_field("super_block", "s_dev")?,
        off_task_group_leader: req_field("task_struct", "group_leader")?,
        off_task_start_boottime: req_field("task_struct", "start_boottime")?,
    })
}

fn find_member_byte_offset(
    entries: &[TypeEntry],
    members: &[MemberRef],
    strings: &[u8],
    type_id: u32,
    target: &str,
    base_bits: u32,
    depth: u32,
) -> Option<u16> {
    if depth > 8 {
        return None;
    }
    let entry = entries.get(usize::try_from(type_id).ok()?)?;
    let TypeEntry::Struct {
        members_start,
        members_len,
    } = entry
    else {
        return None;
    };
    let slice = members.get(*members_start..members_start + members_len)?;
    for member in slice {
        let total_bits = base_bits.checked_add(member.bit_offset)?;
        match btf_name(strings, member.name_off) {
            Some(name) if name == target => {
                if total_bits % 8 != 0 {
                    return None;
                }
                return u16::try_from(total_bits / 8).ok();
            }
            None | Some("") => {
                // Anonymous struct/union (e.g. `struct file::f_path`).
                if let Some(found) = find_member_byte_offset(
                    entries,
                    members,
                    strings,
                    member.type_id,
                    target,
                    total_bits,
                    depth + 1,
                ) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

fn static_func_key(name: &str) -> &'static str {
    match name {
        "vfs_read" => "vfs_read",
        "vfs_write" => "vfs_write",
        "vfs_readv" => "vfs_readv",
        "vfs_writev" => "vfs_writev",
        "filp_close" => "filp_close",
        _ => unreachable!(),
    }
}

fn static_struct_key(name: &str) -> &'static str {
    match name {
        "file" => "file",
        "inode" => "inode",
        "super_block" => "super_block",
        "task_struct" => "task_struct",
        _ => unreachable!(),
    }
}

fn btf_name(strings: &[u8], off: u32) -> Option<&str> {
    let off = usize::try_from(off).ok()?;
    let tail = strings.get(off..)?;
    let nul = tail.iter().position(|b| *b == 0)?;
    std::str::from_utf8(&tail[..nul]).ok()
}

fn advance(pos: usize, delta: usize, limit: usize) -> Result<usize, String> {
    pos.checked_add(delta)
        .filter(|next| *next <= limit)
        .ok_or_else(|| "BTF section truncated".to_string())
}

fn read_u16(bytes: &[u8], at: usize) -> Result<u16, String> {
    let slice = bytes
        .get(at..at + 2)
        .ok_or_else(|| "BTF read_u16 out of bounds".to_string())?;
    Ok(u16::from_ne_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], at: usize) -> Result<u32, String> {
    let slice = bytes
        .get(at..at + 4)
        .ok_or_else(|| "BTF read_u32 out of bounds".to_string())?;
    Ok(u32::from_ne_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SyntheticBtf {
        types: Vec<u8>,
        strings: Vec<u8>,
    }

    impl SyntheticBtf {
        fn new() -> Self {
            Self {
                types: Vec::new(),
                strings: vec![0],
            }
        }

        fn add_str(&mut self, s: &str) -> u32 {
            let off = self.strings.len() as u32;
            self.strings.extend_from_slice(s.as_bytes());
            self.strings.push(0);
            off
        }

        fn push_type(
            &mut self,
            name_off: u32,
            kind: u32,
            vlen: u32,
            kflag: bool,
            size_or_type: u32,
        ) {
            let info = (u32::from(kflag) << 31) | ((kind & 0x1f) << 24) | (vlen & 0xffff);
            self.types.extend_from_slice(&name_off.to_ne_bytes());
            self.types.extend_from_slice(&info.to_ne_bytes());
            self.types.extend_from_slice(&size_or_type.to_ne_bytes());
        }

        fn push_member(&mut self, name: &str, type_id: u32, bit_offset: u32) {
            let no = if name.is_empty() {
                0
            } else {
                self.add_str(name)
            };
            self.types.extend_from_slice(&no.to_ne_bytes());
            self.types.extend_from_slice(&type_id.to_ne_bytes());
            self.types.extend_from_slice(&bit_offset.to_ne_bytes());
        }

        fn build(&self) -> Vec<u8> {
            let hdr_len = 24u32;
            let type_off = 0u32;
            let type_len = self.types.len() as u32;
            let str_off = type_len;
            let str_len = self.strings.len() as u32;
            let mut out = Vec::new();
            out.extend_from_slice(&BTF_MAGIC.to_ne_bytes());
            out.push(BTF_VERSION);
            out.push(0);
            out.extend_from_slice(&hdr_len.to_ne_bytes());
            out.extend_from_slice(&type_off.to_ne_bytes());
            out.extend_from_slice(&type_len.to_ne_bytes());
            out.extend_from_slice(&str_off.to_ne_bytes());
            out.extend_from_slice(&str_len.to_ne_bytes());
            out.extend_from_slice(&self.types);
            out.extend_from_slice(&self.strings);
            out
        }
    }

    #[test]
    fn parses_synthetic_btf_with_anonymous_nested_structs_and_bitfield_kflag() {
        let mut btf = SyntheticBtf::new();
        // tid 1: INT u64
        let n_u64 = btf.add_str("u64");
        btf.push_type(n_u64, BTF_KIND_INT, 0, false, 8);
        btf.types.extend_from_slice(&0u32.to_ne_bytes());

        // tid 2: anonymous union holding `f_path` at +64 bytes (512 bits)
        btf.push_type(0, BTF_KIND_UNION, 1, false, 16);
        btf.push_member("f_path", 1, 0);

        // tid 3: struct file (f_inode @ 32, anon union @ 64)
        let n_file = btf.add_str("file");
        btf.push_type(n_file, BTF_KIND_STRUCT, 2, false, 128);
        btf.push_member("f_inode", 1, 32 * 8);
        btf.push_member("", 2, 64 * 8);

        // tid 4: struct inode (i_mode @ 0, i_sb @ 40, i_ino @ 64)
        let n_inode = btf.add_str("inode");
        btf.push_type(n_inode, BTF_KIND_STRUCT, 3, false, 96);
        btf.push_member("i_mode", 1, 0);
        btf.push_member("i_sb", 1, 40 * 8);
        btf.push_member("i_ino", 1, 64 * 8);

        // tid 5: struct super_block (s_dev @ 16)
        let n_sb = btf.add_str("super_block");
        btf.push_type(n_sb, BTF_KIND_STRUCT, 1, false, 32);
        btf.push_member("s_dev", 1, 16 * 8);

        // tid 6: struct task_struct with kflag=1 bitfield encoding
        let n_task = btf.add_str("task_struct");
        btf.push_type(n_task, BTF_KIND_STRUCT, 2, true, 4000);
        btf.push_member("group_leader", 1, (1u32 << 24) | (3000 * 8));
        btf.push_member("start_boottime", 1, (64u32 << 24) | (3288 * 8));

        // tid 7..11: VFS functions
        for fname in [
            "vfs_read",
            "vfs_write",
            "vfs_readv",
            "vfs_writev",
            "filp_close",
        ] {
            let no = btf.add_str(fname);
            btf.push_type(no, BTF_KIND_FUNC, 0, false, 1);
        }

        let layout = parse_btf_layout(&btf.build()).expect("valid synthetic BTF");
        assert_eq!(layout.vfs_read_id, 7);
        assert_eq!(layout.vfs_write_id, 8);
        assert_eq!(layout.vfs_readv_id, 9);
        assert_eq!(layout.vfs_writev_id, 10);
        assert_eq!(layout.filp_close_id, Some(11));
        assert_eq!(layout.off_file_f_inode, 32);
        assert_eq!(layout.off_file_f_path, 64);
        assert_eq!(layout.off_inode_i_mode, 0);
        assert_eq!(layout.off_inode_i_sb, 40);
        assert_eq!(layout.off_inode_i_ino, 64);
        assert_eq!(layout.off_sb_s_dev, 16);
        assert_eq!(layout.off_task_group_leader, 3000);
        assert_eq!(layout.off_task_start_boottime, 3288);
    }

    #[test]
    fn rejects_corrupt_or_truncated_btf() {
        assert!(parse_btf_layout(&[]).is_err());
        assert!(parse_btf_layout(&[0; 24]).is_err());
    }

    #[test]
    fn parses_live_vmlinux_btf_when_present() {
        let path = Path::new(VMLINUX_BTF_PATH);
        if !path.exists() {
            return;
        }
        let layout = load_layout_from_path(path).expect("parse host /sys/kernel/btf/vmlinux");
        assert!(layout.vfs_read_id > 0);
        assert!(layout.vfs_write_id > 0);
        assert!(layout.vfs_readv_id > 0);
        assert!(layout.vfs_writev_id > 0);
        assert!(layout.filp_close_id.is_some());
        assert!(layout.off_file_f_inode > 0);
        assert!(layout.off_file_f_path > 0);
        assert!(layout.off_inode_i_ino > 0);
        assert!(layout.off_inode_i_sb > 0);
        assert!(layout.off_sb_s_dev > 0);
        assert!(layout.off_task_group_leader > 0);
        assert!(layout.off_task_start_boottime > 0);
    }
}
