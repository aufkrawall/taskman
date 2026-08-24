//! One-shot thread snapshot: pid -> thread count, plus system-wide total.

use std::collections::HashMap;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};

pub fn thread_counts() -> HashMap<u32, u32> {
    let mut map: HashMap<u32, u32> = HashMap::new();
    unsafe {
        let Ok(handle) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
            tracing::debug!("CreateToolhelp32Snapshot(threads) failed");
            return map;
        };
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        if Thread32First(handle, &mut entry).is_ok() {
            loop {
                *map.entry(entry.th32OwnerProcessID).or_insert(0) += 1;
                if Thread32Next(handle, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(handle);
    }
    map
}
