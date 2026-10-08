#!/usr/bin/env python3
# Writes THIRD-PARTY-LICENSES.txt: every Rust crate built into ferret and ferret-helper, with the
# license texts their licenses ask to ship with binaries. Run from the repo root after changing
# dependencies (needs vendor/, see the README's Building): python3 data/third-party/update.py
import hashlib, os, re, subprocess

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HERE = os.path.join(ROOT, 'data', 'third-party')
VENDOR = os.path.join(ROOT, 'vendor')

# Crates whose packages ship no license file: texts from their repositories (missing/SOURCE).
MISSING = {
    'flatbuffers': {'Apache-2.0': 'flatbuffers-LICENSE'},
    'rten*': {'MIT': 'rten-LICENSE-MIT', 'Apache-2.0': 'rten-LICENSE-APACHE'},
}

# License files outside a crate's top folder that its LICENSE points to.
EXTRA_FILES = {'ring': ['src/polyfill/once_cell/LICENSE-MIT']}

# For "A OR B", the license Ferret uses: the first of these that is offered.
PREFERRED = ['MIT', 'Zlib', 'ISC', 'BSD-3-Clause', 'BSD-2-Clause', 'Apache-2.0']

# How to recognise each license's text in a crate's files.
SIGNS = {
    'MIT': 'Permission is hereby granted, free of charge',
    'Apache-2.0': 'Apache License',
    'ISC': 'Permission to use, copy, modify, and/or distribute',
    'Zlib': "provided 'as-is'",
    'BSD-3-Clause': 'Redistribution and use in source and binary forms',
    'Unicode-3.0': 'UNICODE LICENSE',
    'CDLA-Permissive-2.0': 'Community Data License Agreement',
}


def crates():
    seen = {}
    for pkg in ('ferret', 'ferret-helper'):
        out = subprocess.run(
            [os.path.join(ROOT, 'run', 'cargo.sh'), 'tree', '-p', pkg, '-e', 'normal', '--prefix', 'none',
             '--target', 'x86_64-unknown-linux-gnu', '-f', '{p}|{l}'],
            cwd=ROOT, capture_output=True, text=True, check=True).stdout
        for line in out.splitlines():
            line = line.replace(' (*)', '').replace(' (proc-macro)', '').strip()
            if not line or '(/' in line:  # Ferret's own crates
                continue
            p, lic = line.split('|', 1)
            name, ver = p.rsplit(' v', 1)
            seen[(name, ver)] = lic.replace('/', ' OR ')
    return sorted(seen.items())


def chosen(expr):
    # The licenses whose terms apply: every part of an AND, the preferred choice of an OR.
    parts = [p.strip().strip('()') for p in re.split(r'\s+AND\s+', expr)]
    out = []
    for part in parts:
        offers = [o.strip().strip('()') for o in re.split(r'\s+OR\s+', part)]
        offers = [o.split(' WITH ')[0] for o in offers]
        pick = next((p for p in PREFERRED if p in offers), offers[0])
        out.append(pick)
    return out


def texts(name, ver, expr, lics):
    d = os.path.join(VENDOR, f'{name}-{ver}')
    files = sorted(f for f in os.listdir(d) if re.match(r'(?i)(licen[cs]e|copying|unlicense|notice)', f))
    found = []
    if ' AND ' in expr:
        # Several licenses at once (ring): every license file the crate ships, as it ships them.
        for f in files + EXTRA_FILES.get(name, []):
            found.append((f'{name}/{f}', open(os.path.join(d, f), errors='replace').read()))
        return found
    for lic in lics:
        extra = next((v for k, v in MISSING.items() if k == name or (k.endswith('*') and name.startswith(k[:-1]))), None)
        if extra and lic in extra:
            found.append((lic, open(os.path.join(HERE, 'missing', extra[lic])).read()))
            continue
        match = None
        for f in files:
            t = open(os.path.join(d, f), errors='replace').read()
            if SIGNS.get(lic, lic) in t:
                match = t
                break
        if match is None:
            raise SystemExit(f'{name} {ver}: no {lic} text among {files}; add one to missing/')
        found.append((lic, match))
    # NOTICE files (Apache-2.0 asks to pass them on).
    for f in files:
        if f.upper().startswith('NOTICE'):
            found.append(('NOTICE', open(os.path.join(d, f), errors='replace').read()))
    return found


def main():
    rows, groups = [], {}
    for (name, ver), expr in crates():
        lics = chosen(expr)
        rows.append(f'{name} {ver}: {expr}' + (f' (used under {" and ".join(lics)})' if ' OR ' in expr else ''))
        for lic, text in texts(name, ver, expr, lics):
            text = text.strip('\n') + '\n'
            key = hashlib.sha256(text.encode()).hexdigest()
            groups.setdefault(key, (lic, text, []))[2].append(f'{name} {ver}')
    out = [
        'Third-party software in Ferret',
        '==============================',
        '',
        'Ferret is free software under GPL-3.0-or-later (LICENSE). It is built from the Rust crates',
        'below. A crate offered under several licenses is used under the one named after "used',
        'under". Their license texts follow, each with the crates it covers. The OCR models',
        "(PaddleOCR, Apache-2.0) and AreWeAntiCheatYet's game list (MIT) have their own license",
        'files next to this one.',
        '',
        'Crates',
        '------',
        '',
        *rows,
        '',
        'License texts',
        '-------------',
    ]
    for lic, text, users in sorted(groups.values(), key=lambda g: (g[0], g[2][0])):
        out += ['', '=' * 78, f'{lic}: {", ".join(users)}', '=' * 78, '', text.rstrip('\n')]
    with open(os.path.join(HERE, 'THIRD-PARTY-LICENSES.txt'), 'w') as f:
        f.write('\n'.join(out) + '\n')
    print(f'{len(rows)} crates, {len(groups)} license texts')


main()
