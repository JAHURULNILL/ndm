#!/usr/bin/env python3
"""Generate THIRD_PARTY_LICENSES.md from cargo metadata.

Covers every third-party crate in the root+core tree and both
shell trees, plus the runtime components the engine downloads
(yt-dlp / deno / ffmpeg). Run from the repo root before a
release; the output is committed and shipped in the packages.
"""
import json
import subprocess
import sys

OWN = {"ndm", "ndm-core", "ndm-shell-gtk", "ndm-shell-win", "shell-gtk", "shell-win"}


def crates(manifest):
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--manifest-path", manifest],
        capture_output=True, text=True, check=True).stdout
    meta = json.loads(out)
    found = {}
    for p in meta["packages"]:
        if p["name"] in OWN:
            continue
        found[p["name"]] = (p["version"], p.get("license") or "see crate",
                            p.get("repository") or "")
    return found


def main():
    all_crates = {}
    for manifest in ["Cargo.toml", "shell-gtk/Cargo.toml", "shell-win/Cargo.toml"]:
        try:
            for name, info in crates(manifest).items():
                all_crates.setdefault(name, info)
        except Exception as e:  # a tree that does not resolve here is noted, not hidden
            print(f"warning: {manifest}: {e}", file=sys.stderr)
    lines = []
    lines.append("# Third-party licenses")
    lines.append("")
    lines.append("NDM (Nill Download Manager) is © Nill, released under")
    lines.append("MIT OR Apache-2.0. It builds on the open-source work below;")
    lines.append("this file is shipped with every package so the credit and")
    lines.append("the terms travel with the binaries.")
    lines.append("")
    lines.append("## Runtime components (downloaded on first use, hash-verified)")
    lines.append("")
    lines.append("| Component | Used for | License | Upstream |")
    lines.append("|---|---|---|---|")
    lines.append("| yt-dlp | video-site fallback resolver | Unlicense | https://github.com/yt-dlp/yt-dlp |")
    lines.append("| Deno | JavaScript runtime yt-dlp needs on some sites | MIT | https://deno.com |")
    lines.append("| FFmpeg | merging video+audio streams | LGPL-2.1-or-later (some builds GPL-2.0-or-later) | https://ffmpeg.org |")
    lines.append("")
    lines.append("## Rust crates")
    lines.append("")
    lines.append("| Crate | Version | License |")
    lines.append("|---|---|---|")
    for name in sorted(all_crates):
        version, license_name, _repo = all_crates[name]
        lines.append(f"| {name} | {version} | {license_name} |")
    lines.append("")
    lines.append(f"{len(all_crates)} crates in total. Each crate's full license text")
    lines.append("ships inside its package on crates.io and in the cargo registry")
    lines.append("cache of any build machine; the SPDX identifiers above name the")
    lines.append("exact terms (MIT, Apache-2.0, MPL-2.0, BSD-2-Clause,")
    lines.append("BSD-3-Clause, ISC, Unlicense, Zlib, Unicode-3.0, 0BSD).")
    lines.append("")
    with open("THIRD_PARTY_LICENSES.md", "w") as f:
        f.write("\n".join(lines))
    print(f"wrote THIRD_PARTY_LICENSES.md with {len(all_crates)} crates")


if __name__ == "__main__":
    main()
