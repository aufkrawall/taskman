from pathlib import Path

path = Path("crates/tm-app/src/tabs/processes.rs")
text = path.read_text(encoding="utf-8")
old = """impl Subtree {
    fn values(&self, pid: u32) -> [f64; 4] {
        self.values.get(&pid).copied().unwrap_or([0.0; 4])
    }

    fn efficiency(&self, pid: u32) -> bool {
"""
new = """impl Subtree {
    fn efficiency(&self, pid: u32) -> bool {
"""
count = text.count(old)
if count != 1:
    raise SystemExit(f"expected exactly one Subtree::values block, found {count}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
