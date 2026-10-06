"""Check the tracked source tree without printing any credential values."""
import json
import re
import subprocess
import sys
from pathlib import Path

root = Path(__file__).resolve().parent.parent
paths = subprocess.check_output(['git', 'ls-files', '-z'], cwd=root).decode().split('\0')
blocked_directories = {
    '.codex', '.cc-switch', 'credential-backup', 'codex-desktop-recovery',
    'node_modules', 'target', 'dist', 'work', 'outputs', 'backups',
}
blocked_names = {'auth.json', 'codex_oauth_auth.json', 'codex_managed_oauth_live_auth.json'}
blocked_extensions = {'.db', '.db-wal', '.db-shm', '.sqlite', '.sqlite3', '.pdb', '.lnk', '.exe', '.dll', '.pem', '.p12', '.pfx'}
patterns = {
    'private-key': re.compile(r'-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----'),
    'github-token': re.compile(r'(?<![A-Za-z0-9_])(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{50,})'),
    'aws-access-key': re.compile(r'(?<![A-Za-z0-9_])(?:AKIA|ASIA)[A-Z0-9]{16}(?![A-Za-z0-9_])'),
    'context7-key': re.compile(r'(?<![A-Za-z0-9_])ctx7sk-[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}'),
    'google-client-id': re.compile(r'(?<![A-Za-z0-9_])[0-9]{8,}-[a-z0-9]{20,}\.apps\.googleusercontent\.com'),
    'google-client-secret': re.compile(r'(?<![A-Za-z0-9_])GOC' + r'SPX-[A-Za-z0-9_-]{20,}'),
    'forbidden-font': re.compile(r'\bGeor' + r'gia(?: Pro)?\b|MS Go' + r'thic|ＭＳ ゴ' + r'シック', re.I),
}
failures = []
checked = 0
for relative in paths:
    if not relative:
        continue
    path = root / relative
    parts = Path(relative).parts
    if any(part in blocked_directories for part in parts) or Path(relative).name in blocked_names or Path(relative).suffix.lower() in blocked_extensions or Path(relative).name.startswith('.env'):
        failures.append({'file': relative, 'category': 'runtime-or-credential-file'})
        continue
    if path.is_symlink() or not path.is_file():
        failures.append({'file': relative, 'category': 'unexpected-source-entry'})
        continue
    checked += 1
    try:
        content = path.read_text(encoding='utf-8-sig')
    except UnicodeError:
        continue
    for category, pattern in patterns.items():
        for match in pattern.finditer(content):
            # The upstream S3 unit tests use the canonical public AWS example.
            # Allow that exact fixture in its test module, not other keys or
            # all test files. It is never a real credential.
            if (
                category == 'aws-access-key'
                and relative == 'src-tauri/src/services/s3.rs'
                and match.group() == 'AKIA' + 'IOSFODNN7EXAMPLE'
                and '#[cfg(test)]' in content[:match.start()]
            ):
                continue
            failures.append({'file': relative, 'line': content.count('\n', 0, match.start()) + 1, 'category': category})

print(json.dumps({'checkedFiles': checked, 'findings': failures}, ensure_ascii=False))
sys.exit(1 if failures else 0)
