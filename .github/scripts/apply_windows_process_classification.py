from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, found {count}")
    return text.replace(old, new, 1)


# Core classifier: ancestry is not ownership. A platform must positively mark
# an OS-owned process instead of treating every child of a system launcher as
# a system process.
path = Path("crates/tm-core/src/classify.rs")
text = path.read_text(encoding="utf-8")
text = replace_once(
    text,
    '''    /// Chain from direct parent up to root (ppid, ppid-of-ppid, ...).\n    pub ancestor_names: &'a [&'a str],\n    pub has_window: bool,\n    /// Session 0 on Windows / uid 0 root daemon context.\n    pub system_session: bool,\n''',
    '''    pub has_window: bool,\n    /// Positive platform signal that this executable is OS-owned system\n    /// infrastructure. Privilege, Session 0, or a system-process ancestor\n    /// alone are deliberately not enough.\n    pub system_process: bool,\n''',
    "ClassifyInput fields",
)
text = replace_once(
    text,
    '''    // Anything in the services/session-infrastructure ancestry is a Windows process.\n    let ancestor_is_system = input\n        .ancestor_names\n        .iter()\n        .any(|a| is_system_name(&normalize(a)));\n    if ancestor_is_system {\n        return ProcCategory::System;\n    }\n\n    if input.has_window {\n        return ProcCategory::App;\n    }\n\n    if input.system_session {\n        return ProcCategory::System;\n    }\n''',
    '''    if input.system_process {\n        return ProcCategory::System;\n    }\n\n    if input.has_window {\n        return ProcCategory::App;\n    }\n''',
    "classifier ancestry rule",
)
text = text.replace('ancestor_names: &anc,\n                has_window: true,\n                system_session: false,', 'has_window: true,\n                system_process: false,')
text = text.replace('ancestor_names: &[],\n                has_window: false,\n                system_session: false,', 'has_window: false,\n                system_process: false,')
text = text.replace('ancestor_names: &anc,\n                has_window: false,\n                system_session: false,', 'has_window: false,\n                system_process: false,')
text = text.replace('ancestor_names: &[],\n                has_window: false,\n                system_session: true,', 'has_window: false,\n                system_process: true,')
text = replace_once(
    text,
    '''    #[test]\n    fn child_of_services_is_system() {\n        let anc = ["services.exe"];\n        assert_eq!(\n            classify(ClassifyInput {\n                pid: 3000,\n                name: "spoolsv.exe",\n                has_window: false,\n                system_process: false,\n            }),\n            ProcCategory::System\n        );\n    }\n''',
    '''    #[test]\n    fn ancestry_does_not_turn_a_third_party_service_into_windows() {\n        assert_eq!(\n            classify(ClassifyInput {\n                pid: 3000,\n                name: "vendor-service.exe",\n                has_window: false,\n                system_process: false,\n            }),\n            ProcCategory::Background\n        );\n    }\n\n    #[test]\n    fn positive_os_component_signal_is_system() {\n        assert_eq!(\n            classify(ClassifyInput {\n                pid: 3000,\n                name: "WmiPrvSE.exe",\n                has_window: false,\n                system_process: true,\n            }),\n            ProcCategory::System\n        );\n    }\n''',
    "classifier regression tests",
)
# Remove test-local ancestor declarations made obsolete by the input change.
text = text.replace('        let anc = ["explorer.exe"];\n', '')
path.write_text(text, encoding="utf-8")

# Linux/macOS callers no longer build ancestor chains solely for classification.
path = Path("crates/tm-platform/src/linux/mod.rs")
text = path.read_text(encoding="utf-8")
old = '''        let n_procs = self.sys.processes().len();\n        let mut name_by_pid = HashMap::with_capacity(n_procs);\n        for (pid, p) in self.sys.processes() {\n            name_by_pid.insert(pid.as_u32(), p.name().to_string_lossy().into_owned());\n        }\n\n        let mut processes = Vec::with_capacity(n_procs);\n'''
text = replace_once(text, old, '        let n_procs = self.sys.processes().len();\n        let mut processes = Vec::with_capacity(n_procs);\n', "linux name map")
old = '''            let mut anc: Vec<&str> = Vec::new();\n            let mut cur = p.parent().map(|x| x.as_u32());\n            let mut hops = 0;\n            while let Some(ppid) = cur {\n                hops += 1;\n                if hops > 8 {\n                    break;\n                }\n                match name_by_pid.get(&ppid) {\n                    Some(n) => {\n                        anc.push(n.as_str());\n                        cur = self\n                            .sys\n                            .process(sysinfo::Pid::from_u32(ppid))\n                            .and_then(|pp| pp.parent())\n                            .map(|x| x.as_u32());\n                    }\n                    None => break,\n                }\n            }\n\n'''
text = replace_once(text, old, '', "linux ancestor walk")
text = replace_once(
    text,
    '''                ancestor_names: &anc,\n                has_window,\n                system_session: kernel_thread,\n''',
    '''                has_window,\n                system_process: kernel_thread,\n''',
    "linux classifier call",
)
path.write_text(text, encoding="utf-8")

path = Path("crates/tm-platform/src/macos/mod.rs")
text = path.read_text(encoding="utf-8")
old = '''        let n_procs = self.sys.processes().len();\n        let mut name_by_pid = HashMap::with_capacity(n_procs);\n        for (pid, p) in self.sys.processes() {\n            name_by_pid.insert(pid.as_u32(), p.name().to_string_lossy().into_owned());\n        }\n\n        let mut processes = Vec::with_capacity(n_procs);\n'''
text = replace_once(text, old, '        let n_procs = self.sys.processes().len();\n        let mut processes = Vec::with_capacity(n_procs);\n', "mac name map")
old = '''            let mut anc: Vec<&str> = Vec::new();\n            let mut cur = p.parent().map(|x| x.as_u32());\n            let mut hops = 0;\n            while let Some(ppid) = cur {\n                hops += 1;\n                if hops > 8 {\n                    break;\n                }\n                match name_by_pid.get(&ppid) {\n                    Some(n) => {\n                        anc.push(n.as_str());\n                        cur = self\n                            .sys\n                            .process(sysinfo::Pid::from_u32(ppid))\n                            .and_then(|pp| pp.parent())\n                            .map(|x| x.as_u32());\n                    }\n                    None => break,\n                }\n            }\n\n            let is_windowserver_child = anc.iter().any(|a| a == "WindowServer" || a == "launchd");\n'''
text = replace_once(text, old, '', "mac ancestor walk")
text = replace_once(
    text,
    '''                ancestor_names: &anc,\n                has_window: false,\n                system_session: name == "kernel_task" || name == "launchd",\n''',
    '''                has_window: false,\n                system_process: name == "kernel_task" || name == "launchd",\n''',
    "mac classifier call",
)
text = replace_once(text, '            let _ = is_windowserver_child;\n', '', "mac dead windowserver flag")
path.write_text(text, encoding="utf-8")

# Windows: classify the Windows-process group by positive executable ownership,
# not by Session 0 or service ancestry.
path = Path("crates/tm-platform/src/win/sampler.rs")
text = path.read_text(encoding="utf-8")
old = '''    // --- refined classification with real ancestors -------------------------\n    for i in 0..processes.len() {\n        let mut anc: Vec<&str> = Vec::new();\n        let mut cur_pid = processes[i].ppid;\n        let mut hops = 0usize;\n        while let Some(ppid) = cur_pid {\n            hops += 1;\n            if hops > 8 || ppid == processes[i].pid {\n                break;\n            }\n            match idx_by_pid.get(&ppid) {\n                Some(&j) => {\n                    anc.push(processes[j].name.as_str());\n                    cur_pid = processes[j].ppid;\n                }\n                None => break,\n            }\n        }\n        let name = processes[i].name.clone();\n        let input = classify::ClassifyInput {\n            pid: processes[i].pid,\n            name: &name,\n            ancestor_names: &anc,\n            has_window: processes[i].has_window,\n            system_session: processes[i].session_id.is_some_and(|s| s == 0),\n        };\n        let cat = classify::classify(input);\n        processes[i].category = cat;\n    }\n'''
new = '''    // --- refined classification ---------------------------------------------\n    // Session 0 and services.exe ancestry mean "service context", not\n    // "belongs to Windows". Third-party updaters, launchers and helpers run\n    // there too. Only explicit core image names (handled by tm-core) or a\n    // Microsoft-owned executable from a Windows-owned path get System.\n    for p in processes.iter_mut() {\n        let system_process = is_windows_os_component(p);\n        let name = p.name.clone();\n        p.category = classify::classify(classify::ClassifyInput {\n            pid: p.pid,\n            name: &name,\n            has_window: p.has_window,\n            system_process,\n        });\n    }\n'''
text = replace_once(text, old, new, "windows classifier loop")
anchor = '''fn decay_pseudo(slot: &mut Option<HeldPseudoRow>) {\n'''
helper = r'''fn normalize_windows_path(path: &std::path::Path) -> String {
    path.to_string_lossy()
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_ascii_lowercase()
}

fn microsoft_company(company: Option<&str>) -> bool {
    company.is_some_and(|company| {
        let company = company.trim();
        company.eq_ignore_ascii_case("Microsoft Corporation")
            || company.eq_ignore_ascii_case("Microsoft Windows")
    })
}

/// Positive ownership test for the Processes page's "Windows processes"
/// section. Merely running in Session 0, as SYSTEM, or below services.exe is
/// insufficient: third-party services do all three. Windows-owned executable
/// paths also require Microsoft file-version metadata, so a vendor binary in
/// System32 does not become a Windows process just because of its location.
fn is_windows_os_component(p: &ProcessEntry) -> bool {
    let system_root = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows"));
    is_windows_os_component_under(p.company.as_deref(), p.exe_path.as_deref(), &system_root)
}

fn is_windows_os_component_under(
    company: Option<&str>,
    exe_path: Option<&std::path::Path>,
    system_root: &std::path::Path,
) -> bool {
    if !microsoft_company(company) {
        return false;
    }
    let Some(exe_path) = exe_path else {
        return false;
    };
    let path = normalize_windows_path(exe_path);
    let root = normalize_windows_path(system_root);
    if path == root || path.starts_with(&(root + "\\")) {
        return true;
    }

    // Defender's engine is intentionally outside %SystemRoot% on current
    // Windows builds, but it is still first-party Windows infrastructure.
    path.contains(r"\programdata\microsoft\windows defender\")
        || path.contains(r"\program files\windows defender\")
        || path.contains(r"\program files (x86)\windows defender\")
}

'''
text = replace_once(text, anchor, helper + anchor, "windows ownership helper")

# Regression tests for exactly the failure mode in the live screenshot.
test_anchor = '''    #[test]\n    fn interrupt_measurement_is_authoritative_and_residual_never_double_counts() {\n'''
new_tests = r'''    #[test]
    fn windows_process_group_uses_os_ownership_not_service_ancestry() {
        let mut services = ProcessEntry::new(100, "services.exe");
        services.session_id = Some(0);

        let mut battle = ProcessEntry::new(200, "Agent.exe");
        battle.display = "Battle.net Update Agent".into();
        battle.ppid = Some(100);
        battle.session_id = Some(0);
        battle.company = Some("Blizzard Entertainment".into());
        battle.exe_path = Some(r"C:\ProgramData\Battle.net\Agent\Agent.exe".into());

        let mut steam = ProcessEntry::new(300, "steamwebhelper.exe");
        steam.display = "Steam Client WebHelper".into();
        steam.ppid = Some(100);
        steam.session_id = Some(0);
        steam.company = Some("Valve Corporation".into());
        steam.exe_path = Some(r"C:\Program Files (x86)\Steam\bin\cef\steamwebhelper.exe".into());

        let mut taskman = ProcessEntry::new(400, "taskman.exe");
        taskman.ppid = Some(100);
        taskman.session_id = Some(1);
        taskman.has_window = true;
        taskman.exe_path = Some(r"C:\Users\dev\taskman.exe".into());

        let mut wmi = ProcessEntry::new(500, "WmiPrvSE.exe");
        wmi.ppid = Some(100);
        wmi.session_id = Some(0);
        wmi.company = Some("Microsoft Corporation".into());
        wmi.exe_path = Some(r"C:\Windows\System32\wbem\WmiPrvSE.exe".into());

        let mut processes = vec![services, battle, steam, taskman, wmi];
        refine_categories_and_group_apps(&mut processes);

        let category = |pid| processes.iter().find(|p| p.pid == pid).unwrap().category;
        assert_eq!(category(100), ProcCategory::System);
        assert_eq!(category(200), ProcCategory::Background);
        assert_eq!(category(300), ProcCategory::Background);
        assert_eq!(category(400), ProcCategory::App);
        assert_eq!(category(500), ProcCategory::System);
    }

    #[test]
    fn windows_owned_path_requires_microsoft_metadata() {
        let root = std::path::Path::new(r"C:\Windows");
        assert!(is_windows_os_component_under(
            Some("Microsoft Corporation"),
            Some(std::path::Path::new(r"C:\Windows\System32\dllhost.exe")),
            root,
        ));
        assert!(is_windows_os_component_under(
            Some("Microsoft Corporation"),
            Some(std::path::Path::new(
                r"C:\ProgramData\Microsoft\Windows Defender\Platform\MsMpEng.exe"
            )),
            root,
        ));
        assert!(!is_windows_os_component_under(
            Some("Valve Corporation"),
            Some(std::path::Path::new(r"C:\Windows\System32\vendor.exe")),
            root,
        ));
        assert!(!is_windows_os_component_under(
            Some("Microsoft Corporation"),
            Some(std::path::Path::new(
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
            )),
            root,
        ));
    }

'''
text = replace_once(text, test_anchor, new_tests + test_anchor, "windows classification tests")
path.write_text(text, encoding="utf-8")

# Durable repo knowledge: Windows-process membership is positive ownership,
# independent of generic process ancestry.
path = Path("llm-wiki/repo-map.md")
text = path.read_text(encoding="utf-8")
old = '''  - Windows service executable. Starts under SCM as delayed-auto LocalSystem,\n'''
new = '''  - Windows service executable. Starts under SCM as delayed-auto LocalSystem,\n'''
# Put the classification note next to the Processes-page semantics where it is
# most likely to be consulted before future grouping changes.
needle = '''  parent group separately (`sibling_run_key`), including `svchost.exe` under `services.exe`.\n'''
replacement = needle + '''  Windows-process membership is NOT inherited from Session 0 or system ancestry:\n  the Windows sampler requires a core system image name or Microsoft metadata plus\n  a Windows-owned executable path, so third-party SCM services remain Background.\n'''
text = replace_once(text, needle, replacement, "repo map Windows classification")
path.write_text(text, encoding="utf-8")

path = Path("llm-wiki/log/recent.md")
text = path.read_text(encoding="utf-8")
entry = '''## 2026-09-07 — Windows-process classification requires OS ownership\n\n1. **Session 0 is not Windows ownership.** The sampler no longer classifies every\n   Session-0 process or every descendant of `services.exe`/another system process\n   as a Windows process. Third-party services such as Battle.net Update Agent and\n   Steam helpers now remain Background unless they independently qualify as Apps.\n2. **Positive first-party evidence.** Core system image names remain System. Other\n   Windows components require Microsoft file-version metadata plus a Windows-owned\n   executable path (`%SystemRoot%`, with Defender's first-party ProgramData/Program\n   Files locations covered explicitly). A vendor binary in System32 is not enough.\n3. **Foreground visibility is preserved.** A visible non-Windows executable launched\n   below a system/service process is classified as an App rather than being hidden in\n   the Windows section. Regression coverage includes Battle.net, Steam WebHelper,\n   TaskMan itself, WMI Provider Host, Defender/path ownership, and vendor-in-System32.\n\n'''
text = replace_once(text, '# Recent Activity\n\n', '# Recent Activity\n\n' + entry, "recent log header")
path.write_text(text, encoding="utf-8")
