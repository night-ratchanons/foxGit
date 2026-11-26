#!/usr/bin/env python3
"""Write THIRD-PARTY-LICENSES.txt: every crate (and bundled C library / font) shipped in the
FoxGit binary, with the license texts found in its source. Identical texts are printed once.

    python3 scripts/third-party-licenses.py > THIRD-PARTY-LICENSES.txt
"""
import hashlib, json, os, re, subprocess, sys

meta = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--format-version", "1", "--locked"],
    cwd=os.path.join(os.path.dirname(__file__), "..")))
pkgs = {p["id"]: p for p in meta["packages"]}
nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
root = meta["resolve"]["root"]

# Everything reachable through normal/build dependencies (dev-dependencies are not shipped).
seen, stack = set(), [root]
while stack:
    cur = stack.pop()
    for d in nodes[cur]["deps"]:
        if any(k["kind"] in (None, "build") for k in d["dep_kinds"]) and d["pkg"] not in seen:
            seen.add(d["pkg"])
            stack.append(d["pkg"])

NAME = re.compile(r"(licen[cs]e|copying|notice|copyright|^ofl|^ufl|unlicense)", re.I)
SKIP_DIRS = {"tests", "test", "benches", "examples", "target", ".git", "docs", "doc"}

def license_files(pkg_dir):
    out = []
    for dirpath, dirs, files in os.walk(pkg_dir):
        depth = os.path.relpath(dirpath, pkg_dir).count(os.sep) + (dirpath != pkg_dir)
        dirs[:] = [d for d in dirs if d not in SKIP_DIRS and depth < 3]
        for f in sorted(files):
            font_note = os.path.basename(dirpath) == "fonts" and f.endswith(".txt")
            if font_note or (NAME.search(f) and not f.endswith((".rs", ".py", ".sh", ".toml", ".json", ".html"))):
                path = os.path.join(dirpath, f)
                if os.path.getsize(path) < 200_000:
                    out.append(path)
    return out

texts = {}   # hash -> (text, [crate labels])
entries = []
for pid in sorted(seen, key=lambda i: (pkgs[i]["name"].lower(), pkgs[i]["version"])):
    p = pkgs[pid]
    label = f'{p["name"]} {p["version"]}'
    pkg_dir = os.path.dirname(p["manifest_path"])
    files = license_files(pkg_dir)
    entries.append((label, p.get("license") or "see license file", p.get("repository") or ""))
    for f in files:
        try:
            t = open(f, encoding="utf-8", errors="replace").read().strip()
        except OSError:
            continue
        h = hashlib.sha256(re.sub(r"\s+", " ", t).encode()).hexdigest()
        texts.setdefault(h, (t, []))[1].append(f'{label} ({os.path.relpath(f, pkg_dir)})')

w = sys.stdout.write
w("FoxGit — third-party software\n")
w("=" * 72 + "\n\n")
w("FoxGit is MIT licensed (see LICENSE). It includes the open-source components below,\n")
w("including libgit2 (GPLv2 with linking exception) and the fonts bundled by egui.\n\n")
w(f"Components ({len(entries)}):\n\n")
for label, lic, repo in entries:
    w(f"  {label:<48} {lic}\n")
w("\n\nLicense texts\n" + "=" * 72 + "\n")
for t, users in sorted(texts.values(), key=lambda x: x[1][0].lower()):
    w("\n" + "-" * 72 + "\n")
    w("Used by:\n" + "".join(f"  - {u}\n" for u in users) + "\n")
    w(t + "\n")
