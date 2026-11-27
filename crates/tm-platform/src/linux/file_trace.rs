//! Root-side eBPF per-file read/write I/O tracer for Linux.
//!
//! Procfs (`/proc/<pid>/fd`, `/proc/<pid>/fdinfo`, `/proc/<pid>/io`) exposes
//! open file paths and process-wide totals, but has no per-file transfer
//! counters. When `taskman-service` starts with `CAP_BPF` + `CAP_PERFMON` and
//! `/sys/kernel/btf/vmlinux` is readable, this module attaches BTF `fexit`
//! trampolines to `vfs_read`, `vfs_write`, `vfs_readv`, and `vfs_writev` (plus a
//! best-effort `fentry` probe on `filp_close` using `bpf_d_path` to retain
//! paths of short-lived files closed between polls).
//!
//! Every entry in the kernel hash map is keyed by
//! `(start_boottime_ns, tgid, s_dev, i_ino)` so a recycled PID can never
//! inherit another process's counters, and only regular files (`S_ISREG`) are
//! tracked.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::btf::{self, KernelLayout};
use super::process_io::{self, HandleCoverage, OpenHandleRow, ProcessIoCounters};
use crate::actions::{
    FileIoAccounting, FileIoAccountingState, ProcessFileIo, ProcessFileIoSnapshot,
};
use tm_core::error::Result;

const BPF_MAP_CREATE: libc::c_long = 0;
const BPF_MAP_LOOKUP_ELEM: libc::c_long = 1;
const BPF_MAP_DELETE_ELEM: libc::c_long = 3;
const BPF_MAP_GET_NEXT_KEY: libc::c_long = 4;
const BPF_PROG_LOAD: libc::c_long = 5;
const BPF_RAW_TRACEPOINT_OPEN: libc::c_long = 17;
const BPF_LINK_CREATE: libc::c_long = 28;

const BPF_MAP_TYPE_HASH: u32 = 1;
const BPF_MAP_TYPE_ARRAY: u32 = 2;
const BPF_MAP_TYPE_RINGBUF: u32 = 27;

const BPF_PROG_TYPE_TRACING: u32 = 26;
const BPF_TRACE_FENTRY: u32 = 24;
const BPF_TRACE_FEXIT: u32 = 25;

const BPF_NOEXIST: u64 = 1;
const BPF_PSEUDO_MAP_FD: u8 = 1;

const BPF_RINGBUF_BUSY_BIT: u32 = 1 << 31;
const BPF_RINGBUF_DISCARD_BIT: u32 = 1 << 30;
const BPF_RINGBUF_LEN_MASK: u32 = 0x3fff_ffff;
const BPF_RINGBUF_HDR_SZ: usize = 8;

const BPF_FUNC_MAP_LOOKUP_ELEM: i32 = 1;
const BPF_FUNC_MAP_UPDATE_ELEM: i32 = 2;
const BPF_FUNC_GET_CURRENT_PID_TGID: i32 = 14;
const BPF_FUNC_GET_CURRENT_TASK: i32 = 35;
const BPF_FUNC_PROBE_READ_KERNEL: i32 = 113;
const BPF_FUNC_RINGBUF_RESERVE: i32 = 131;
const BPF_FUNC_RINGBUF_SUBMIT: i32 = 132;
const BPF_FUNC_D_PATH: i32 = 147;

const IO_MAP_MAX_ENTRIES: u32 = 65_536;
const RINGBUF_BYTES: u32 = 256 * 1024;
const PATH_CAP: usize = 512;
const MAX_TRACKED_FILES_PER_PID: usize = 2_048;
const MAX_CACHED_CLOSED_PATHS: usize = 16_384;
const MAX_RINGBUF_DRAIN_PER_CALL: usize = 4_096;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
struct IoKey {
    start_boottime: u64,
    tgid: u32,
    s_dev: u32,
    i_ino: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct IoVal {
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TraceStats {
    total_transfers: u64,
    dropped_updates: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PathEvent {
    start_boottime: u64,
    tgid: u32,
    s_dev: u32,
    i_ino: u64,
    path_len: i64,
    path: [u8; PATH_CAP],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BpfInsn {
    code: u8,
    regs: u8,
    off: i16,
    imm: i32,
}

impl BpfInsn {
    const fn new(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Self {
        Self {
            code,
            regs: (dst & 0x0f) | ((src & 0x0f) << 4),
            off,
            imm,
        }
    }
}

#[repr(C)]
struct BpfAttrMapCreate {
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    map_flags: u32,
    inner_map_fd: u32,
    numa_node: u32,
    map_name: [u8; 16],
    _tail: [u8; 116],
}

#[repr(C)]
struct BpfAttrMapElem {
    map_fd: u32,
    _pad: u32,
    key: u64,
    value_or_next_key: u64,
    flags: u64,
    _tail: [u8; 128],
}

#[repr(C)]
struct BpfAttrProgLoad {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; 16],
    prog_ifindex: u32,
    expected_attach_type: u32,
    prog_btf_fd: u32,
    func_info_rec_size: u32,
    func_info: u64,
    func_info_cnt: u32,
    line_info_rec_size: u32,
    line_info: u64,
    line_info_cnt: u32,
    attach_btf_id: u32,
    attach_btf_obj_fd: u32,
    _tail: [u8; 44],
}

#[repr(C)]
struct BpfAttrLinkCreate {
    prog_fd: u32,
    target_fd: u32,
    attach_type: u32,
    flags: u32,
    target_btf_id: u32,
    _tail: [u8; 140],
}

#[repr(C)]
struct BpfAttrRawTpOpen {
    name: u64,
    prog_fd: u32,
    _pad: u32,
    cookie: u64,
    _tail: [u8; 136],
}

// ------------------------------------------------------------------ builder

const R0: u8 = 0;
const R1: u8 = 1;
const R2: u8 = 2;
const R3: u8 = 3;
const R4: u8 = 4;
const R6: u8 = 6;
const R7: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;

const BPF_LD: u8 = 0x00;
const BPF_LDX: u8 = 0x01;
const BPF_STX: u8 = 0x03;
const BPF_ALU: u8 = 0x04;
const BPF_JMP: u8 = 0x05;
const BPF_JMP32: u8 = 0x06;
const BPF_ALU64: u8 = 0x07;

const BPF_W: u8 = 0x00;
const BPF_H: u8 = 0x08;
const BPF_DW: u8 = 0x18;

const BPF_IMM: u8 = 0x00;
const BPF_MEM: u8 = 0x60;
const BPF_ATOMIC: u8 = 0xc0;

const BPF_ADD: u8 = 0x00;
const BPF_AND: u8 = 0x50;
const BPF_RSH: u8 = 0x70;
const BPF_MOV: u8 = 0xb0;

const BPF_JA: u8 = 0x00;
const BPF_JEQ: u8 = 0x10;
const BPF_JNE: u8 = 0x50;
const BPF_CALL: u8 = 0x80;
const BPF_EXIT: u8 = 0x90;
const BPF_JSLT: u8 = 0xc0;

const BPF_K: u8 = 0x00;
const BPF_X: u8 = 0x08;

struct Label {
    pos: Option<usize>,
    refs: Vec<usize>,
}

impl Label {
    fn new() -> Self {
        Self {
            pos: None,
            refs: Vec::new(),
        }
    }
}

struct BpfBuilder {
    insns: Vec<BpfInsn>,
}

impl BpfBuilder {
    fn new() -> Self {
        Self {
            insns: Vec::with_capacity(128),
        }
    }

    fn emit(&mut self, code: u8, dst: u8, src: u8, off: i16, imm: i32) {
        self.insns.push(BpfInsn::new(code, dst, src, off, imm));
    }

    fn mov64_reg(&mut self, dst: u8, src: u8) {
        self.emit(BPF_ALU64 | BPF_MOV | BPF_X, dst, src, 0, 0);
    }

    fn mov64_imm(&mut self, dst: u8, imm: i32) {
        self.emit(BPF_ALU64 | BPF_MOV | BPF_K, dst, 0, 0, imm);
    }

    fn mov32_imm(&mut self, dst: u8, imm: i32) {
        self.emit(BPF_ALU | BPF_MOV | BPF_K, dst, 0, 0, imm);
    }

    fn add64_imm(&mut self, dst: u8, imm: i32) {
        self.emit(BPF_ALU64 | BPF_ADD | BPF_K, dst, 0, 0, imm);
    }

    fn rsh64_imm(&mut self, dst: u8, imm: i32) {
        self.emit(BPF_ALU64 | BPF_RSH | BPF_K, dst, 0, 0, imm);
    }

    fn and32_imm(&mut self, dst: u8, imm: i32) {
        self.emit(BPF_ALU | BPF_AND | BPF_K, dst, 0, 0, imm);
    }

    fn ldx(&mut self, size: u8, dst: u8, src: u8, off: i16) {
        self.emit(BPF_LDX | size | BPF_MEM, dst, src, off, 0);
    }

    fn stx(&mut self, size: u8, dst: u8, src: u8, off: i16) {
        self.emit(BPF_STX | size | BPF_MEM, dst, src, off, 0);
    }

    fn atomic_add64(&mut self, dst: u8, off: i16, src: u8) {
        self.emit(BPF_STX | BPF_DW | BPF_ATOMIC, dst, src, off, 0);
    }

    fn ld_map_fd(&mut self, dst: u8, map_fd: RawFd) {
        self.emit(BPF_LD | BPF_DW | BPF_IMM, dst, BPF_PSEUDO_MAP_FD, 0, map_fd);
        self.emit(0, 0, 0, 0, 0);
    }

    fn call(&mut self, func_id: i32) {
        self.emit(BPF_JMP | BPF_CALL, 0, 0, 0, func_id);
    }

    fn exit(&mut self) {
        self.emit(BPF_JMP | BPF_EXIT, 0, 0, 0, 0);
    }

    fn jmp_to(&mut self, code: u8, dst: u8, imm: i32, label: &mut Label) {
        let idx = self.insns.len();
        let off = match label.pos {
            Some(target) => (target as isize - idx as isize - 1) as i16,
            None => {
                label.refs.push(idx);
                0
            }
        };
        self.emit(code, dst, 0, off, imm);
    }

    fn bind(&mut self, label: &mut Label) {
        let target = self.insns.len();
        label.pos = Some(target);
        for idx in label.refs.drain(..) {
            self.insns[idx].off = (target as isize - idx as isize - 1) as i16;
        }
    }

    fn probe_read_kernel_to_stack(&mut self, stack_off: i16, size: i32, src_reg: u8, src_off: u16) {
        if src_reg != R3 {
            self.mov64_reg(R3, src_reg);
        }
        if src_off != 0 {
            self.add64_imm(R3, i32::from(src_off));
        }
        self.mov64_reg(R1, R10);
        self.add64_imm(R1, i32::from(stack_off));
        self.mov32_imm(R2, size);
        self.call(BPF_FUNC_PROBE_READ_KERNEL);
    }

    fn finish(self) -> Vec<BpfInsn> {
        self.insns
    }
}

/// Emit the common prefix that validates a regular file and populates `IoKey`
/// at `r10 - 32 .. r10 - 8`.
///
/// Expects `r6` = `struct file *file` (already checked non-null) and `r8` = `0`.
/// Preserves `r6`, `r7`, `r8`. Clobbers `r0..r5`, `r9`.
fn emit_populate_regular_file_key(
    b: &mut BpfBuilder,
    layout: &KernelLayout,
    self_pid: u32,
    exit: &mut Label,
) {
    // tgid = bpf_get_current_pid_tgid() >> 32
    b.call(BPF_FUNC_GET_CURRENT_PID_TGID);
    b.rsh64_imm(R0, 32);
    b.jmp_to(BPF_JMP32 | BPF_JEQ | BPF_K, R0, 0, exit);
    b.jmp_to(BPF_JMP32 | BPF_JEQ | BPF_K, R0, self_pid as i32, exit);

    // Zero IoKey (r10 - 32 .. r10 - 8), then store tgid at r10 - 24.
    b.stx(BPF_DW, R10, R8, -32);
    b.stx(BPF_DW, R10, R8, -24);
    b.stx(BPF_DW, R10, R8, -16);
    b.stx(BPF_W, R10, R0, -24);

    // r9 = inode = *(file + off_f_inode)
    b.stx(BPF_DW, R10, R8, -8);
    b.probe_read_kernel_to_stack(-8, 8, R6, layout.off_file_f_inode);
    b.ldx(BPF_DW, R9, R10, -8);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R9, 0, exit);

    // mode = *(u16 *)(inode + off_i_mode); require S_ISREG (0xf000 == 0x8000)
    b.stx(BPF_DW, R10, R8, -8);
    b.probe_read_kernel_to_stack(-8, 2, R9, layout.off_inode_i_mode);
    b.ldx(BPF_H, R1, R10, -8);
    b.and32_imm(R1, 0xf000);
    b.jmp_to(BPF_JMP32 | BPF_JNE | BPF_K, R1, 0x8000, exit);

    // sb = *(inode + off_i_sb)
    b.stx(BPF_DW, R10, R8, -8);
    b.probe_read_kernel_to_stack(-8, 8, R9, layout.off_inode_i_sb);
    b.ldx(BPF_DW, R3, R10, -8);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R3, 0, exit);

    // key.s_dev = *(u32 *)(sb + off_s_dev) at r10 - 20
    b.probe_read_kernel_to_stack(-20, 4, R3, layout.off_sb_s_dev);

    // key.i_ino = *(u64 *)(inode + off_i_ino) at r10 - 16
    b.probe_read_kernel_to_stack(-16, 8, R9, layout.off_inode_i_ino);

    // task = bpf_get_current_task(); leader = *(task + off_group_leader)
    b.call(BPF_FUNC_GET_CURRENT_TASK);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, exit);
    b.stx(BPF_DW, R10, R8, -8);
    b.probe_read_kernel_to_stack(-8, 8, R0, layout.off_task_group_leader);
    b.ldx(BPF_DW, R3, R10, -8);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R3, 0, exit);

    // key.start_boottime = *(u64 *)(leader + off_start_boottime) at r10 - 32
    b.probe_read_kernel_to_stack(-32, 8, R3, layout.off_task_start_boottime);
}

fn emit_vfs_fexit_program(
    layout: &KernelLayout,
    ret_ctx_off: i16,
    is_write: bool,
    self_pid: u32,
    io_map_fd: RawFd,
    state_map_fd: RawFd,
) -> Vec<BpfInsn> {
    let mut b = BpfBuilder::new();
    let mut exit = Label::new();
    let mut not_found = Label::new();
    let mut record_drop = Label::new();
    let mut bump_total = Label::new();

    let (bytes_off, ops_off): (i16, i16) = if is_write { (8, 24) } else { (0, 16) };

    // r7 = (s64)ctx[ret_idx]; if r7 < 1 return 0
    b.ldx(BPF_DW, R7, R1, ret_ctx_off);
    b.jmp_to(BPF_JMP | BPF_JSLT | BPF_K, R7, 1, &mut exit);

    // r6 = ctx[0] (struct file *); if r6 == 0 return 0
    b.ldx(BPF_DW, R6, R1, 0);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R6, 0, &mut exit);

    b.mov64_imm(R8, 0);
    emit_populate_regular_file_key(&mut b, layout, self_pid, &mut exit);

    // val = bpf_map_lookup_elem(&io_map, &key)
    b.ld_map_fd(R1, io_map_fd);
    b.mov64_reg(R2, R10);
    b.add64_imm(R2, -32);
    b.call(BPF_FUNC_MAP_LOOKUP_ELEM);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, &mut not_found);

    // Hit: atomic add bytes and ops
    b.mov64_imm(R1, 1);
    b.atomic_add64(R0, bytes_off, R7);
    b.atomic_add64(R0, ops_off, R1);
    b.jmp_to(BPF_JMP | BPF_JA, 0, 0, &mut bump_total);

    // Miss: build IoVal at r10 - 64 .. r10 - 32
    b.bind(&mut not_found);
    b.stx(BPF_DW, R10, R8, -64);
    b.stx(BPF_DW, R10, R8, -56);
    b.stx(BPF_DW, R10, R8, -48);
    b.stx(BPF_DW, R10, R8, -40);
    b.mov64_imm(R1, 1);
    b.stx(BPF_DW, R10, R7, -64 + bytes_off);
    b.stx(BPF_DW, R10, R1, -64 + ops_off);

    b.ld_map_fd(R1, io_map_fd);
    b.mov64_reg(R2, R10);
    b.add64_imm(R2, -32);
    b.mov64_reg(R3, R10);
    b.add64_imm(R3, -64);
    b.mov64_imm(R4, BPF_NOEXIST as i32);
    b.call(BPF_FUNC_MAP_UPDATE_ELEM);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, &mut bump_total);

    // Update failed (likely concurrent first insert -> retry lookup once).
    b.ld_map_fd(R1, io_map_fd);
    b.mov64_reg(R2, R10);
    b.add64_imm(R2, -32);
    b.call(BPF_FUNC_MAP_LOOKUP_ELEM);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, &mut record_drop);
    b.mov64_imm(R1, 1);
    b.atomic_add64(R0, bytes_off, R7);
    b.atomic_add64(R0, ops_off, R1);
    b.jmp_to(BPF_JMP | BPF_JA, 0, 0, &mut bump_total);

    b.bind(&mut record_drop);
    b.stx(BPF_W, R10, R8, -8);
    b.ld_map_fd(R1, state_map_fd);
    b.mov64_reg(R2, R10);
    b.add64_imm(R2, -8);
    b.call(BPF_FUNC_MAP_LOOKUP_ELEM);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, &mut exit);
    b.mov64_imm(R1, 1);
    b.atomic_add64(R0, 8, R1);
    b.jmp_to(BPF_JMP | BPF_JA, 0, 0, &mut exit);

    b.bind(&mut bump_total);
    b.stx(BPF_W, R10, R8, -8);
    b.ld_map_fd(R1, state_map_fd);
    b.mov64_reg(R2, R10);
    b.add64_imm(R2, -8);
    b.call(BPF_FUNC_MAP_LOOKUP_ELEM);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, &mut exit);
    b.mov64_imm(R1, 1);
    b.atomic_add64(R0, 0, R1);

    b.bind(&mut exit);
    b.mov32_imm(R0, 0);
    b.exit();
    b.finish()
}

fn emit_filp_close_fentry_program(
    layout: &KernelLayout,
    self_pid: u32,
    io_map_fd: RawFd,
    path_rb_fd: RawFd,
) -> Vec<BpfInsn> {
    let mut b = BpfBuilder::new();
    let mut exit = Label::new();

    // r6 = ctx[0] (BTF PTR_TO_BTF_ID `struct file *`)
    b.ldx(BPF_DW, R6, R1, 0);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R6, 0, &mut exit);

    b.mov64_imm(R8, 0);
    emit_populate_regular_file_key(&mut b, layout, self_pid, &mut exit);

    // Only capture paths of files that actually have traced I/O in `io_map`.
    b.ld_map_fd(R1, io_map_fd);
    b.mov64_reg(R2, R10);
    b.add64_imm(R2, -32);
    b.call(BPF_FUNC_MAP_LOOKUP_ELEM);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, &mut exit);

    // ev = bpf_ringbuf_reserve(&path_rb, sizeof(PathEvent), 0)
    b.ld_map_fd(R1, path_rb_fd);
    b.mov64_imm(R2, std::mem::size_of::<PathEvent>() as i32);
    b.mov64_imm(R3, 0);
    b.call(BPF_FUNC_RINGBUF_RESERVE);
    b.jmp_to(BPF_JMP | BPF_JEQ | BPF_K, R0, 0, &mut exit);

    // Populate header fields in ringbuf record
    b.ldx(BPF_DW, R1, R10, -32);
    b.stx(BPF_DW, R0, R1, 0);
    b.ldx(BPF_W, R1, R10, -24);
    b.stx(BPF_W, R0, R1, 8);
    b.ldx(BPF_W, R1, R10, -20);
    b.stx(BPF_W, R0, R1, 12);
    b.ldx(BPF_DW, R1, R10, -16);
    b.stx(BPF_DW, R0, R1, 16);

    // bpf_d_path((struct path *)(r6 + off_f_path), ev->path, PATH_CAP)
    b.mov64_reg(R1, R6);
    b.add64_imm(R1, i32::from(layout.off_file_f_path));
    b.mov64_reg(R2, R0);
    b.add64_imm(R2, 32);
    b.mov32_imm(R3, PATH_CAP as i32);
    b.mov64_reg(R6, R0);
    b.call(BPF_FUNC_D_PATH);

    b.stx(BPF_DW, R6, R0, 24);
    b.mov64_reg(R1, R6);
    b.mov64_imm(R2, 0);
    b.call(BPF_FUNC_RINGBUF_SUBMIT);

    b.bind(&mut exit);
    b.mov32_imm(R0, 0);
    b.exit();
    b.finish()
}

// ------------------------------------------------------------------ syscalls

fn sys_bpf<T>(cmd: libc::c_long, attr: &mut T) -> std::io::Result<libc::c_long> {
    // SAFETY: `attr` is a `#[repr(C)]` kernel UAPI buffer of size `size_of::<T>()`.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_bpf,
            cmd,
            attr as *mut T as *mut libc::c_void,
            std::mem::size_of::<T>() as libc::c_uint,
        )
    };
    if ret < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(ret)
    }
}

fn copy_obj_name(name: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    let bytes = name.as_bytes();
    let len = bytes.len().min(15);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

fn create_map(
    map_type: u32,
    key_size: u32,
    value_size: u32,
    max_entries: u32,
    name: &str,
) -> Result<OwnedFd> {
    let mut attr = BpfAttrMapCreate {
        map_type,
        key_size,
        value_size,
        max_entries,
        map_flags: 0,
        inner_map_fd: 0,
        numa_node: 0,
        map_name: copy_obj_name(name),
        _tail: [0; 116],
    };
    let fd = sys_bpf(BPF_MAP_CREATE, &mut attr).map_err(|error| {
        tm_core::TmError::platform("bpf map create", format!("{name}: {error}"))
    })?;
    // SAFETY: `BPF_MAP_CREATE` returns a new owned file descriptor on >= 0.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

fn load_and_attach_tracing(
    name: &str,
    attach_btf_id: u32,
    attach_type: u32,
    insns: &[BpfInsn],
) -> Result<(OwnedFd, OwnedFd)> {
    let license = CString::new("Dual MIT/GPL").expect("no nul");
    let mut attr = BpfAttrProgLoad {
        prog_type: BPF_PROG_TYPE_TRACING,
        insn_cnt: insns.len() as u32,
        insns: insns.as_ptr() as u64,
        license: license.as_ptr() as u64,
        log_level: 0,
        log_size: 0,
        log_buf: 0,
        kern_version: 0,
        prog_flags: 0,
        prog_name: copy_obj_name(name),
        prog_ifindex: 0,
        expected_attach_type: attach_type,
        prog_btf_fd: 0,
        func_info_rec_size: 0,
        func_info: 0,
        func_info_cnt: 0,
        line_info_rec_size: 0,
        line_info: 0,
        line_info_cnt: 0,
        attach_btf_id,
        attach_btf_obj_fd: 0,
        _tail: [0; 44],
    };
    let prog_fd = match sys_bpf(BPF_PROG_LOAD, &mut attr) {
        Ok(fd) => fd,
        Err(first_err) => {
            // Re-run once with verifier logging so failures explain themselves.
            let mut log_buf = vec![0u8; 64 * 1024];
            attr.log_level = 1;
            attr.log_size = log_buf.len() as u32;
            attr.log_buf = log_buf.as_mut_ptr() as u64;
            let _ = sys_bpf(BPF_PROG_LOAD, &mut attr);
            let nul = log_buf
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(log_buf.len());
            let verifier_log = String::from_utf8_lossy(&log_buf[..nul]);
            let tail: String = verifier_log
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join(" | ");
            return Err(tm_core::TmError::platform(
                "bpf prog load",
                format!("{name}: {first_err}; verifier: {tail}"),
            ));
        }
    };
    // SAFETY: `BPF_PROG_LOAD` returned a valid owned fd.
    let prog = unsafe { OwnedFd::from_raw_fd(prog_fd as RawFd) };

    let mut link_attr = BpfAttrLinkCreate {
        prog_fd: prog.as_raw_fd() as u32,
        target_fd: 0,
        attach_type,
        flags: 0,
        target_btf_id: 0,
        _tail: [0; 140],
    };
    let link_fd = match sys_bpf(BPF_LINK_CREATE, &mut link_attr) {
        Ok(fd) => fd,
        Err(_) => {
            // Fallback for kernels 5.5-5.6 where tracing links used BPF_RAW_TRACEPOINT_OPEN.
            let mut raw_attr = BpfAttrRawTpOpen {
                name: 0,
                prog_fd: prog.as_raw_fd() as u32,
                _pad: 0,
                cookie: 0,
                _tail: [0; 136],
            };
            sys_bpf(BPF_RAW_TRACEPOINT_OPEN, &mut raw_attr).map_err(|error| {
                tm_core::TmError::platform("bpf attach", format!("{name}: {error}"))
            })?
        }
    };
    // SAFETY: `BPF_LINK_CREATE`/`BPF_RAW_TRACEPOINT_OPEN` returned a valid owned fd.
    let link = unsafe { OwnedFd::from_raw_fd(link_fd as RawFd) };
    Ok((prog, link))
}

fn map_lookup<K, V: Default>(map_fd: RawFd, key: &K) -> Option<V> {
    let mut value = V::default();
    let mut attr = BpfAttrMapElem {
        map_fd: map_fd as u32,
        _pad: 0,
        key: key as *const K as u64,
        value_or_next_key: &mut value as *mut V as u64,
        flags: 0,
        _tail: [0; 128],
    };
    sys_bpf(BPF_MAP_LOOKUP_ELEM, &mut attr).ok()?;
    Some(value)
}

fn map_next_key<K: Copy + Default>(map_fd: RawFd, prev: Option<&K>) -> Option<K> {
    let mut next = K::default();
    let mut attr = BpfAttrMapElem {
        map_fd: map_fd as u32,
        _pad: 0,
        key: prev.map_or(0, |k| k as *const K as u64),
        value_or_next_key: &mut next as *mut K as u64,
        flags: 0,
        _tail: [0; 128],
    };
    sys_bpf(BPF_MAP_GET_NEXT_KEY, &mut attr).ok()?;
    Some(next)
}

fn map_delete<K>(map_fd: RawFd, key: &K) {
    let mut attr = BpfAttrMapElem {
        map_fd: map_fd as u32,
        _pad: 0,
        key: key as *const K as u64,
        value_or_next_key: 0,
        flags: 0,
        _tail: [0; 128],
    };
    let _ = sys_bpf(BPF_MAP_DELETE_ELEM, &mut attr);
}

// ------------------------------------------------------------------ ringbuf

struct RingBuffer {
    consumer_ptr: *mut u8,
    consumer_len: usize,
    producer_ptr: *const u8,
    producer_len: usize,
    data_ptr: *const u8,
    mask: usize,
}

// SAFETY: `RingBuffer` owns private `mmap` regions accessed only while holding the
// `FileTraceState` mutex.
unsafe impl Send for RingBuffer {}

impl RingBuffer {
    fn map(rb_fd: RawFd, ring_bytes: usize) -> Result<Self> {
        if !ring_bytes.is_power_of_two() || ring_bytes == 0 {
            return Err(tm_core::TmError::platform(
                "ringbuf size",
                "ring buffer size must be a power of two",
            ));
        }
        // SAFETY: `sysconf(_SC_PAGESIZE)` has no preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        if page_size == 0 {
            return Err(tm_core::TmError::platform(
                "page size",
                "sysconf(_SC_PAGESIZE) returned 0",
            ));
        }
        // SAFETY: mapping the ringbuf consumer page (`offset = 0`).
        let consumer_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                page_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                rb_fd,
                0,
            )
        };
        if consumer_ptr == libc::MAP_FAILED {
            return Err(tm_core::TmError::platform(
                "mmap ringbuf consumer",
                std::io::Error::last_os_error().to_string(),
            ));
        }

        let producer_len = page_size + ring_bytes * 2;
        // SAFETY: mapping the ringbuf producer page + 2x mirrored data region.
        let producer_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                producer_len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                rb_fd,
                page_size as libc::off_t,
            )
        };
        if producer_ptr == libc::MAP_FAILED {
            // SAFETY: unmap `consumer_ptr` on failure.
            unsafe {
                libc::munmap(consumer_ptr, page_size);
            }
            return Err(tm_core::TmError::platform(
                "mmap ringbuf producer",
                std::io::Error::last_os_error().to_string(),
            ));
        }

        // SAFETY: `data_ptr` points to the mirrored data pages immediately after the producer page.
        let data_ptr = unsafe { (producer_ptr as *const u8).add(page_size) };
        Ok(Self {
            consumer_ptr: consumer_ptr as *mut u8,
            consumer_len: page_size,
            producer_ptr: producer_ptr as *const u8,
            producer_len,
            data_ptr,
            mask: ring_bytes - 1,
        })
    }

    fn drain<F: FnMut(&[u8])>(&mut self, max_records: usize, mut on_record: F) {
        // SAFETY: `consumer_ptr` and `producer_ptr` point to page-aligned `AtomicU64`
        // positions mapped for the lifetime of `RingBuffer`.
        unsafe {
            let cons_pos = &*(self.consumer_ptr as *const AtomicU64);
            let prod_pos = &*(self.producer_ptr as *const AtomicU64);
            drain_ringbuf_slices(
                cons_pos,
                prod_pos,
                std::slice::from_raw_parts(self.data_ptr, (self.mask + 1) * 2),
                self.mask,
                max_records,
                &mut on_record,
            );
        }
    }
}

impl Drop for RingBuffer {
    fn drop(&mut self) {
        // SAFETY: both pointers were returned by `mmap` with their stored lengths.
        unsafe {
            libc::munmap(self.producer_ptr as *mut libc::c_void, self.producer_len);
            libc::munmap(self.consumer_ptr as *mut libc::c_void, self.consumer_len);
        }
    }
}

fn drain_ringbuf_slices(
    cons_pos: &AtomicU64,
    prod_pos: &AtomicU64,
    mirrored_data: &[u8],
    mask: usize,
    max_records: usize,
    on_record: &mut dyn FnMut(&[u8]),
) {
    let mut cons = cons_pos.load(Ordering::Relaxed);
    let prod = prod_pos.load(Ordering::Acquire);
    let mut handled = 0usize;
    while cons < prod && handled < max_records {
        let offset = (cons as usize) & mask;
        let Some(hdr) = mirrored_data.get(offset..offset + 4) else {
            break;
        };
        let raw_len = u32::from_ne_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        if raw_len & BPF_RINGBUF_BUSY_BIT != 0 {
            break;
        }
        let len = (raw_len & BPF_RINGBUF_LEN_MASK) as usize;
        let step = ((BPF_RINGBUF_HDR_SZ + len + 7) & !7) as u64;
        if raw_len & BPF_RINGBUF_DISCARD_BIT == 0
            && let Some(payload) =
                mirrored_data.get(offset + BPF_RINGBUF_HDR_SZ..offset + BPF_RINGBUF_HDR_SZ + len)
        {
            on_record(payload);
        }
        cons = cons.saturating_add(step);
        cons_pos.store(cons, Ordering::Release);
        handled += 1;
    }
}

// ------------------------------------------------------------------ tracer

struct ActiveTracer {
    io_map: OwnedFd,
    state_map: OwnedFd,
    _path_rb_map: OwnedFd,
    ringbuf: RingBuffer,
    _progs_and_links: Vec<(OwnedFd, OwnedFd)>,
    started_at: Instant,
    started_boottime_ns: u64,
    tick_ns: u64,
    closed_paths: HashMap<IoKey, String>,
    last_prune: Instant,
}

/// Running root-side eBPF per-file tracer.
pub(crate) struct LinuxFileTrace {
    state: Mutex<Option<ActiveTracer>>,
}

impl LinuxFileTrace {
    /// Start the system-wide VFS eBPF tracer.
    ///
    /// Failures are returned to the caller so the service can log once and keep
    /// serving with per-file byte accounting left `Unavailable`.
    pub(crate) fn start() -> Result<Self> {
        let layout = btf::load_vmlinux_layout()
            .map_err(|error| tm_core::TmError::platform("btf load", error))?;

        let io_map = create_map(
            BPF_MAP_TYPE_HASH,
            std::mem::size_of::<IoKey>() as u32,
            std::mem::size_of::<IoVal>() as u32,
            IO_MAP_MAX_ENTRIES,
            "tm_file_io",
        )?;
        let state_map = create_map(
            BPF_MAP_TYPE_ARRAY,
            4,
            std::mem::size_of::<TraceStats>() as u32,
            1,
            "tm_io_state",
        )?;
        let path_rb_map = create_map(BPF_MAP_TYPE_RINGBUF, 0, 0, RINGBUF_BYTES, "tm_file_paths")?;
        let ringbuf = RingBuffer::map(path_rb_map.as_raw_fd(), RINGBUF_BYTES as usize)?;

        let self_pid = std::process::id();
        let io_fd = io_map.as_raw_fd();
        let state_fd = state_map.as_raw_fd();
        let rb_fd = path_rb_map.as_raw_fd();

        let mut progs_and_links = Vec::with_capacity(5);
        for (name, btf_id, ret_ctx_off, is_write) in [
            ("tm_vfs_read", layout.vfs_read_id, 32i16, false),
            ("tm_vfs_write", layout.vfs_write_id, 32i16, true),
            ("tm_vfs_readv", layout.vfs_readv_id, 40i16, false),
            ("tm_vfs_writev", layout.vfs_writev_id, 40i16, true),
        ] {
            let insns =
                emit_vfs_fexit_program(&layout, ret_ctx_off, is_write, self_pid, io_fd, state_fd);
            progs_and_links.push(load_and_attach_tracing(
                name,
                btf_id,
                BPF_TRACE_FEXIT,
                &insns,
            )?);
        }

        if let Some(filp_close_id) = layout.filp_close_id {
            let close_insns = emit_filp_close_fentry_program(&layout, self_pid, io_fd, rb_fd);
            match load_and_attach_tracing(
                "tm_filp_close",
                filp_close_id,
                BPF_TRACE_FENTRY,
                &close_insns,
            ) {
                Ok(pair) => progs_and_links.push(pair),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        "optional filp_close path tracker disabled; closed files will aggregate under unnamed"
                    );
                }
            }
        }

        // SAFETY: `sysconf(_SC_CLK_TCK)` has no preconditions.
        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
        let tick_ns = if clk_tck > 0 {
            1_000_000_000u64 / (clk_tck as u64)
        } else {
            10_000_000u64
        };
        let started_boottime_ns = clock_boottime_ns();
        let now = Instant::now();

        Ok(Self {
            state: Mutex::new(Some(ActiveTracer {
                io_map,
                state_map,
                _path_rb_map: path_rb_map,
                ringbuf,
                _progs_and_links: progs_and_links,
                started_at: now,
                started_boottime_ns,
                tick_ns,
                closed_paths: HashMap::new(),
                last_prune: now,
            })),
        })
    }

    pub(crate) fn disabled() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        tm_core::sync::lock(&self.state).is_some()
    }

    /// Periodic maintenance: drain closed-file path events and prune exited PIDs
    /// from the kernel map so `io_map` stays bounded to live processes.
    pub(crate) fn maintain(&self) {
        let mut guard = tm_core::sync::lock(&self.state);
        let Some(tracer) = guard.as_mut() else {
            return;
        };
        drain_closed_paths(tracer);
        if tracer.last_prune.elapsed() >= std::time::Duration::from_secs(2) {
            prune_exited_entries(tracer);
            tracer.last_prune = Instant::now();
        }
    }

    /// Snapshot per-file I/O for one verified process (`pid`, `start_ticks`).
    pub(crate) fn snapshot_process(
        &self,
        pid: u32,
        start_ticks: i64,
    ) -> Result<ProcessFileIoSnapshot> {
        let detailed = process_io::list_process_file_io_detailed(pid, Some(start_ticks))?;
        let handles_complete = detailed.coverage == HandleCoverage::Complete;

        let mut guard = tm_core::sync::lock(&self.state);
        let Some(tracer) = guard.as_mut() else {
            let files = detailed.files.into_iter().map(|row| row.file).collect();
            return Ok(unavailable_snapshot(
                files,
                detailed.counters,
                handles_complete,
            ));
        };

        drain_closed_paths(tracer);
        if tracer.last_prune.elapsed() >= std::time::Duration::from_secs(2) {
            prune_exited_entries(tracer);
            tracer.last_prune = Instant::now();
        }

        let stats =
            map_lookup::<u32, TraceStats>(tracer.state_map.as_raw_fd(), &0).unwrap_or_default();
        let target_ticks = u64::try_from(start_ticks).unwrap_or(0);
        let entries =
            collect_process_entries(tracer.io_map.as_raw_fd(), pid, target_ticks, tracer.tick_ns);

        Ok(build_measured_snapshot(
            detailed.files,
            detailed.counters,
            handles_complete,
            &entries,
            &tracer.closed_paths,
            stats,
            tracer
                .started_at
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            tracer.started_boottime_ns,
            target_ticks.saturating_mul(tracer.tick_ns),
        ))
    }
}

fn clock_boottime_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is valid writable storage for `clock_gettime`.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } == 0 {
        (ts.tv_sec.max(0) as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec.max(0) as u64)
    } else {
        0
    }
}

fn drain_closed_paths(tracer: &mut ActiveTracer) {
    let closed_paths = &mut tracer.closed_paths;
    tracer.ringbuf.drain(MAX_RINGBUF_DRAIN_PER_CALL, |payload| {
        if let Some((key, path)) = parse_path_event(payload) {
            if closed_paths.len() >= MAX_CACHED_CLOSED_PATHS && !closed_paths.contains_key(&key) {
                return;
            }
            closed_paths.insert(key, path);
        }
    });
}

fn parse_path_event(payload: &[u8]) -> Option<(IoKey, String)> {
    if payload.len() < 32 {
        return None;
    }
    let start_boottime = u64::from_ne_bytes(payload[0..8].try_into().ok()?);
    let tgid = u32::from_ne_bytes(payload[8..12].try_into().ok()?);
    let s_dev = u32::from_ne_bytes(payload[12..16].try_into().ok()?);
    let i_ino = u64::from_ne_bytes(payload[16..24].try_into().ok()?);
    let path_len = i64::from_ne_bytes(payload[24..32].try_into().ok()?);
    if path_len <= 1 {
        return None;
    }
    let avail = payload.len() - 32;
    let take = (path_len as usize).min(avail);
    let raw = &payload[32..32 + take];
    let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
    if end == 0 {
        return None;
    }
    let path = String::from_utf8_lossy(&raw[..end]).into_owned();
    if !path.starts_with('/') {
        return None;
    }
    Some((
        IoKey {
            start_boottime,
            tgid,
            s_dev,
            i_ino,
        },
        path,
    ))
}

fn collect_process_entries(
    io_map_fd: RawFd,
    pid: u32,
    target_ticks: u64,
    tick_ns: u64,
) -> Vec<(IoKey, IoVal)> {
    let mut out = Vec::new();
    let mut prev: Option<IoKey> = None;
    let mut seen = 0u32;
    while seen < IO_MAP_MAX_ENTRIES {
        let Some(key) = map_next_key::<IoKey>(io_map_fd, prev.as_ref()) else {
            break;
        };
        seen += 1;
        prev = Some(key);
        if key.tgid != pid {
            continue;
        }
        let key_ticks = key.start_boottime / tick_ns.max(1);
        if key_ticks.abs_diff(target_ticks) > 1 {
            continue;
        }
        if let Some(val) = map_lookup::<IoKey, IoVal>(io_map_fd, &key) {
            out.push((key, val));
        }
    }
    out
}

fn prune_exited_entries(tracer: &mut ActiveTracer) {
    let io_fd = tracer.io_map.as_raw_fd();
    let tick_ns = tracer.tick_ns.max(1);
    let mut live_ticks_by_pid: HashMap<u32, Option<u64>> = HashMap::new();
    let mut to_delete: Vec<IoKey> = Vec::new();
    let mut prev: Option<IoKey> = None;
    let mut seen = 0u32;
    while seen < IO_MAP_MAX_ENTRIES {
        let Some(key) = map_next_key::<IoKey>(io_fd, prev.as_ref()) else {
            break;
        };
        seen += 1;
        prev = Some(key);
        let live_ticks = *live_ticks_by_pid
            .entry(key.tgid)
            .or_insert_with(|| super::process_start_ticks_optional(key.tgid));
        let key_ticks = key.start_boottime / tick_ns;
        let still_alive = live_ticks.is_some_and(|t| t.abs_diff(key_ticks) <= 1);
        if !still_alive {
            to_delete.push(key);
        }
    }
    for key in &to_delete {
        map_delete(io_fd, key);
        tracer.closed_paths.remove(key);
    }
    tracer.closed_paths.retain(|key, _| {
        let live_ticks = *live_ticks_by_pid
            .entry(key.tgid)
            .or_insert_with(|| super::process_start_ticks_optional(key.tgid));
        let key_ticks = key.start_boottime / tick_ns;
        live_ticks.is_some_and(|t| t.abs_diff(key_ticks) <= 1)
    });
}

fn kernel_dev_to_userspace_dev(s_dev: u32) -> u64 {
    let major = s_dev >> 20;
    let minor = s_dev & 0x000f_ffff;
    libc::makedev(major, minor)
}

#[allow(clippy::too_many_arguments)]
fn build_measured_snapshot(
    open_rows: Vec<OpenHandleRow>,
    counters: ProcessIoCounters,
    mut handles_complete: bool,
    ebpf_entries: &[(IoKey, IoVal)],
    closed_paths: &HashMap<IoKey, String>,
    stats: TraceStats,
    since_ms: u64,
    trace_started_boottime_ns: u64,
    process_started_boottime_ns: u64,
) -> ProcessFileIoSnapshot {
    let measured = stats.total_transfers > 0;
    let state = if measured {
        FileIoAccountingState::Measured
    } else {
        FileIoAccountingState::Undelivered
    };

    let mut rows: Vec<ProcessFileIo> = Vec::with_capacity(open_rows.len());
    let mut by_dev_ino: HashMap<(u64, u64), usize> = HashMap::with_capacity(open_rows.len());
    let mut indices_by_ino: HashMap<u64, Vec<usize>> = HashMap::with_capacity(open_rows.len());

    for mut open in open_rows {
        if measured {
            open.file.read_bytes = Some(0);
            open.file.write_bytes = Some(0);
            open.file.read_ops = Some(0);
            open.file.write_ops = Some(0);
        }
        let idx = rows.len();
        by_dev_ino.insert((open.dev, open.ino), idx);
        indices_by_ino.entry(open.ino).or_default().push(idx);
        rows.push(open.file);
    }

    let mut total_read_bytes = 0u64;
    let mut total_write_bytes = 0u64;
    let mut total_read_ops = 0u64;
    let mut total_write_ops = 0u64;
    let mut unnamed_read_bytes = 0u64;
    let mut unnamed_write_bytes = 0u64;
    let mut omitted_read_bytes = 0u64;
    let mut omitted_write_bytes = 0u64;
    let mut unnamed_creates = 0u64;
    let mut dropped_paths = stats.dropped_updates;

    let mut closed_by_path: HashMap<String, usize> = HashMap::new();

    for (key, val) in ebpf_entries {
        total_read_bytes = total_read_bytes.saturating_add(val.read_bytes);
        total_write_bytes = total_write_bytes.saturating_add(val.write_bytes);
        total_read_ops = total_read_ops.saturating_add(val.read_ops);
        total_write_ops = total_write_ops.saturating_add(val.write_ops);

        let u_dev = kernel_dev_to_userspace_dev(key.s_dev);
        let matched_open = by_dev_ino.get(&(u_dev, key.i_ino)).copied().or_else(|| {
            // Anonymous kernel filesystems (major == 0, e.g. Btrfs subvolumes)
            // report a per-subvolume `st_dev` in `stat()` that differs from
            // `super_block.s_dev`. Match by inode when it names a unique open
            // file in this process.
            if (key.s_dev >> 20) == 0
                && let Some(cands) = indices_by_ino.get(&key.i_ino)
                && cands.len() == 1
            {
                return Some(cands[0]);
            }
            None
        });

        if let Some(idx) = matched_open {
            let row = &mut rows[idx];
            row.read_bytes = Some(row.read_bytes.unwrap_or(0).saturating_add(val.read_bytes));
            row.write_bytes = Some(row.write_bytes.unwrap_or(0).saturating_add(val.write_bytes));
            row.read_ops = Some(row.read_ops.unwrap_or(0).saturating_add(val.read_ops));
            row.write_ops = Some(row.write_ops.unwrap_or(0).saturating_add(val.write_ops));
            continue;
        }

        if let Some(path) = closed_paths.get(key) {
            if let Some(&idx) = closed_by_path.get(path) {
                let row = &mut rows[idx];
                row.read_bytes = Some(row.read_bytes.unwrap_or(0).saturating_add(val.read_bytes));
                row.write_bytes =
                    Some(row.write_bytes.unwrap_or(0).saturating_add(val.write_bytes));
                row.read_ops = Some(row.read_ops.unwrap_or(0).saturating_add(val.read_ops));
                row.write_ops = Some(row.write_ops.unwrap_or(0).saturating_add(val.write_ops));
            } else if rows.len() < MAX_TRACKED_FILES_PER_PID {
                let idx = rows.len();
                closed_by_path.insert(path.clone(), idx);
                rows.push(ProcessFileIo {
                    path: path.clone(),
                    read_bytes: Some(val.read_bytes),
                    write_bytes: Some(val.write_bytes),
                    read_ops: Some(val.read_ops),
                    write_ops: Some(val.write_ops),
                    file_size: None,
                    offset: None,
                    access_known: true,
                    is_read: false,
                    is_write: false,
                    open_handles: 0,
                    idle_ms: None,
                });
            } else {
                omitted_read_bytes = omitted_read_bytes.saturating_add(val.read_bytes);
                omitted_write_bytes = omitted_write_bytes.saturating_add(val.write_bytes);
                dropped_paths = dropped_paths.saturating_add(1);
                handles_complete = false;
            }
        } else {
            unnamed_read_bytes = unnamed_read_bytes.saturating_add(val.read_bytes);
            unnamed_write_bytes = unnamed_write_bytes.saturating_add(val.write_bytes);
            unnamed_creates = unnamed_creates.saturating_add(1);
        }
    }

    rows.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    let traced_since_process_start = process_started_boottime_ns >= trace_started_boottime_ns;
    let (counter_window_read_bytes, counter_window_write_bytes) = if traced_since_process_start {
        (counters.rchar, counters.wchar)
    } else {
        (None, None)
    };

    ProcessFileIoSnapshot {
        files: rows,
        accounting: FileIoAccounting {
            state,
            since_ms,
            read_bytes: total_read_bytes,
            write_bytes: total_write_bytes,
            read_ops: total_read_ops,
            write_ops: total_write_ops,
            unnamed_read_bytes,
            unnamed_write_bytes,
            omitted_read_bytes,
            omitted_write_bytes,
            counter_read_bytes: counters.read_bytes,
            counter_write_bytes: counters.write_bytes,
            events_lost: Some(0),
            cleared_file_objects: 0,
            unnamed_creates,
            dropped_paths,
            handles_complete,
            traced_since_process_start,
            counter_window_read_bytes,
            counter_window_write_bytes,
        },
    }
}

fn unavailable_snapshot(
    mut files: Vec<ProcessFileIo>,
    counters: ProcessIoCounters,
    handles_complete: bool,
) -> ProcessFileIoSnapshot {
    files.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    ProcessFileIoSnapshot {
        files,
        accounting: FileIoAccounting {
            state: FileIoAccountingState::Unavailable,
            counter_read_bytes: counters.read_bytes,
            counter_write_bytes: counters.write_bytes,
            handles_complete,
            ..FileIoAccounting::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layout() -> KernelLayout {
        KernelLayout {
            vfs_read_id: 10,
            vfs_write_id: 11,
            vfs_readv_id: 12,
            vfs_writev_id: 13,
            filp_close_id: Some(14),
            off_file_f_inode: 32,
            off_file_f_path: 64,
            off_inode_i_mode: 0,
            off_inode_i_ino: 64,
            off_inode_i_sb: 40,
            off_sb_s_dev: 16,
            off_task_group_leader: 3000,
            off_task_start_boottime: 3288,
        }
    }

    #[test]
    fn emitted_ebpf_programs_have_valid_jump_targets_and_exit() {
        let layout = sample_layout();
        for (ret_off, is_write) in [(32, false), (32, true), (40, false), (40, true)] {
            let prog = emit_vfs_fexit_program(&layout, ret_off, is_write, 999, 5, 6);
            assert!(prog.len() > 20 && prog.len() < 256);
            assert_eq!(prog.last().unwrap().code, BPF_JMP | BPF_EXIT);
            for (idx, insn) in prog.iter().enumerate() {
                let class = insn.code & 0x07;
                if (class == BPF_JMP || class == BPF_JMP32)
                    && insn.code != (BPF_JMP | BPF_CALL)
                    && insn.code != (BPF_JMP | BPF_EXIT)
                {
                    let target = idx as isize + 1 + insn.off as isize;
                    assert!(
                        target >= 0 && (target as usize) < prog.len(),
                        "jump at {idx} out of bounds: {target}"
                    );
                }
            }
        }
        let close_prog = emit_filp_close_fentry_program(&layout, 999, 5, 7);
        assert!(close_prog.len() > 20 && close_prog.len() < 256);
        assert_eq!(close_prog.last().unwrap().code, BPF_JMP | BPF_EXIT);
    }

    #[test]
    fn ringbuf_consumer_drains_normal_skips_discarded_and_stops_at_busy() {
        let cons = AtomicU64::new(0);
        let prod = AtomicU64::new(0);
        let mut ring = vec![0u8; 256];

        let write_rec = |buf: &mut [u8], pos: usize, hdr_flags: u32, data: &[u8]| -> usize {
            let len_word = (data.len() as u32) | hdr_flags;
            buf[pos..pos + 4].copy_from_slice(&len_word.to_ne_bytes());
            buf[pos + 4..pos + 8].copy_from_slice(&0u32.to_ne_bytes());
            buf[pos + 8..pos + 8 + data.len()].copy_from_slice(data);
            (BPF_RINGBUF_HDR_SZ + data.len() + 7) & !7
        };

        let mut p = 0usize;
        p += write_rec(&mut ring, p, 0, b"first");
        p += write_rec(&mut ring, p, BPF_RINGBUF_DISCARD_BIT, b"dropped");
        p += write_rec(&mut ring, p, BPF_RINGBUF_BUSY_BIT, b"busy");
        prod.store(p as u64, Ordering::Release);

        let mut mirrored = ring.clone();
        mirrored.extend_from_slice(&ring);

        let mut seen = Vec::new();
        drain_ringbuf_slices(&cons, &prod, &mirrored, 255, 64, &mut |rec| {
            seen.push(rec.to_vec());
        });
        assert_eq!(seen, vec![b"first".to_vec()]);
        // Consumer must stop right at the busy record, having consumed first + discarded.
        assert_eq!(cons.load(Ordering::Acquire), 32);
    }

    #[test]
    fn snapshot_attributes_open_btrfs_closed_and_unnamed_entries_honestly() {
        let open = vec![
            OpenHandleRow {
                file: ProcessFileIo {
                    path: "/var/log/app.log".into(),
                    read_bytes: None,
                    write_bytes: None,
                    read_ops: None,
                    write_ops: None,
                    file_size: Some(4096),
                    offset: Some(128),
                    access_known: true,
                    is_read: false,
                    is_write: true,
                    open_handles: 1,
                    idle_ms: None,
                },
                dev: libc::makedev(259, 5),
                ino: 42,
            },
            OpenHandleRow {
                file: ProcessFileIo {
                    path: "/home/user/btrfs.db".into(),
                    read_bytes: None,
                    write_bytes: None,
                    read_ops: None,
                    write_ops: None,
                    file_size: Some(8192),
                    offset: Some(0),
                    access_known: true,
                    is_read: true,
                    is_write: false,
                    open_handles: 1,
                    idle_ms: None,
                },
                dev: libc::makedev(0, 38),
                ino: 777,
            },
            OpenHandleRow {
                file: ProcessFileIo {
                    path: "/etc/idle.conf".into(),
                    read_bytes: None,
                    write_bytes: None,
                    read_ops: None,
                    write_ops: None,
                    file_size: Some(100),
                    offset: Some(0),
                    access_known: true,
                    is_read: true,
                    is_write: false,
                    open_handles: 1,
                    idle_ms: None,
                },
                dev: libc::makedev(259, 5),
                ino: 99,
            },
        ];

        let closed_key = IoKey {
            start_boottime: 5_000_000_000,
            tgid: 123,
            s_dev: (259 << 20) | 5,
            i_ino: 888,
        };
        let mut closed_paths = HashMap::new();
        closed_paths.insert(closed_key, "/tmp/short.tmp".to_string());

        let ebpf_entries = vec![
            (
                IoKey {
                    start_boottime: 5_000_000_000,
                    tgid: 123,
                    s_dev: (259 << 20) | 5,
                    i_ino: 42,
                },
                IoVal {
                    read_bytes: 0,
                    write_bytes: 512,
                    read_ops: 0,
                    write_ops: 2,
                },
            ),
            (
                IoKey {
                    start_boottime: 5_000_000_000,
                    tgid: 123,
                    s_dev: 34, // major 0, different minor from stat()'s 38
                    i_ino: 777,
                },
                IoVal {
                    read_bytes: 4096,
                    write_bytes: 0,
                    read_ops: 1,
                    write_ops: 0,
                },
            ),
            (
                closed_key,
                IoVal {
                    read_bytes: 128,
                    write_bytes: 256,
                    read_ops: 1,
                    write_ops: 1,
                },
            ),
            (
                IoKey {
                    start_boottime: 5_000_000_000,
                    tgid: 123,
                    s_dev: (259 << 20) | 5,
                    i_ino: 999,
                },
                IoVal {
                    read_bytes: 64,
                    write_bytes: 0,
                    read_ops: 1,
                    write_ops: 0,
                },
            ),
        ];

        let snap = build_measured_snapshot(
            open,
            ProcessIoCounters {
                rchar: Some(5000),
                wchar: Some(1000),
                read_bytes: Some(4096),
                write_bytes: Some(512),
            },
            true,
            &ebpf_entries,
            &closed_paths,
            TraceStats {
                total_transfers: 6,
                dropped_updates: 0,
            },
            1500,
            1_000_000_000,
            5_000_000_000,
        );

        assert_eq!(snap.accounting.state, FileIoAccountingState::Measured);
        assert!(snap.accounting.traced_since_process_start);
        assert_eq!(snap.accounting.counter_window_read_bytes, Some(5000));
        assert_eq!(snap.accounting.counter_window_write_bytes, Some(1000));
        assert_eq!(snap.accounting.unnamed_read_bytes, 64);
        assert_eq!(snap.accounting.unnamed_creates, 1);
        assert_eq!(snap.accounting.read_bytes, 4096 + 128 + 64);
        assert_eq!(snap.accounting.write_bytes, 512 + 256);

        let idle = snap
            .files
            .iter()
            .find(|f| f.path == "/etc/idle.conf")
            .unwrap();
        assert_eq!(idle.read_bytes, Some(0));
        assert_eq!(idle.write_bytes, Some(0));

        let closed = snap
            .files
            .iter()
            .find(|f| f.path == "/tmp/short.tmp")
            .unwrap();
        assert_eq!(closed.open_handles, 0);
        assert_eq!(closed.read_bytes, Some(128));
        assert_eq!(closed.write_bytes, Some(256));

        let btrfs = snap
            .files
            .iter()
            .find(|f| f.path == "/home/user/btrfs.db")
            .unwrap();
        assert_eq!(btrfs.read_bytes, Some(4096));
    }

    #[test]
    fn parse_path_event_handles_nul_termination_and_rejects_bad_lengths() {
        let mut buf = [0u8; 32 + 64];
        buf[0..8].copy_from_slice(&100u64.to_ne_bytes());
        buf[8..12].copy_from_slice(&42u32.to_ne_bytes());
        buf[12..16].copy_from_slice(&7u32.to_ne_bytes());
        buf[16..24].copy_from_slice(&99u64.to_ne_bytes());
        let path = b"/tmp/hello.txt\0";
        buf[24..32].copy_from_slice(&(path.len() as i64).to_ne_bytes());
        buf[32..32 + path.len()].copy_from_slice(path);

        let (key, parsed) = parse_path_event(&buf).expect("valid path event");
        assert_eq!(key.tgid, 42);
        assert_eq!(parsed, "/tmp/hello.txt");

        buf[24..32].copy_from_slice(&(-36i64).to_ne_bytes());
        assert!(parse_path_event(&buf).is_none());
    }
}
