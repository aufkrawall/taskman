from pathlib import Path

path = Path(".github/scripts/apply_process_grouping.py")
text = path.read_text(encoding="utf-8")
old = '''def block(text: str) -> str:
    return dedent(text).lstrip("\\n")
'''
new = '''def block(text: str) -> str:
    text = text.lstrip("\\n")
    return "".join(
        line[8:] if line.startswith("        ") else line
        for line in text.splitlines(keepends=True)
    )
'''
if text.count(old) != 1:
    raise SystemExit("block helper: expected exactly one match")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
