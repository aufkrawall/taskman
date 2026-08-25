fn main() {
    let loc = tm_platform::detect_locale();
    println!("detected: {:?}", loc);
    tm_core::locale::init(loc);
    println!("format_mb(6902700) = {}", tm_core::format::format_mb(6902700));
    println!("is_german = {}", tm_core::locale::is_german());
}
