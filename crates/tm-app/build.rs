fn main() {
    #[cfg(target_os = "windows")]
    {
        let res = std::path::Path::new("assets/app.res");
        if res.exists() {
            println!(
                "cargo:rustc-link-arg={}",
                res.canonicalize().unwrap().display()
            );
        }
    }
}
