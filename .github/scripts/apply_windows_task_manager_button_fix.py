from pathlib import Path

path = Path("crates/tm-platform/src/win/core_service.rs")
text = path.read_text(encoding="utf-8")

impl_anchor = "impl PlatformActions for BrokeredActions {"
impl_pos = text.find(impl_anchor)
if impl_pos < 0:
    raise SystemExit("BrokeredActions PlatformActions impl not found")

method_anchor = "    fn task_manager_replacement_state(&self) -> TaskManagerReplacementState {"
method_pos = text.find(method_anchor, impl_pos)
if method_pos < 0:
    raise SystemExit("task_manager_replacement_state anchor not found in BrokeredActions impl")

insertion = """    fn launch_native_task_manager(&self) -> Result<()> {\n        self.local.launch_native_task_manager()\n    }\n\n"""
if insertion in text[impl_pos:method_pos]:
    raise SystemExit("launch_native_task_manager delegation already present")

text = text[:method_pos] + insertion + text[method_pos:]
path.write_text(text, encoding="utf-8")
