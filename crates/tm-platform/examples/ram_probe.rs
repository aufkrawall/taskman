//! Prints the SMBIOS RAM facts the Memory page shows (speed, slots, form
//! factor, part number). Run: `cargo run -p tm-platform --example ram_probe`.

#[cfg(target_os = "windows")]
fn main() {
    let ram = tm_platform::win::memory_info::probe();
    println!("{ram:#?}");
    println!(
        "installed: {:.1} GB",
        ram.installed_bytes as f64 / 1024.0 / 1024.0 / 1024.0
    );
}

#[cfg(not(target_os = "windows"))]
fn main() {
    println!("Windows-only example");
}
