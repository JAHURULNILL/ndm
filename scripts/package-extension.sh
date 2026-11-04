#!/bin/sh
# Build the two extension packages from extension/:
#   ndm-extension-<version>.zip          Chromium/Brave/Edge (service worker)
#   ndm-extension-firefox-<version>.zip  Firefox (background scripts)
# The Firefox manifest is REGENERATED from manifest.json on every
# run (background.service_worker -> background.scripts with
# fb-classify.js preloaded, "key" dropped — AMO rejects it; the
# gecko id pins the add-on identity), so the two packages can
# never drift apart. Both zips are checked against the manifest:
# every file the manifest names must be inside the zip (the same
# guard the release workflow runs).
set -eu
cd "$(dirname "$0")/.."
EXT=extension
OUT="${1:-dist-extension}"
VERSION=$(python3 -c "import json; print(json.load(open('$EXT/manifest.json'))['version'])")
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

python3 - "$EXT" <<'PYEOF'
import json, sys, collections
ext = sys.argv[1]
src = json.load(open(f'{ext}/manifest.json'),
                object_pairs_hook=collections.OrderedDict)
ff = collections.OrderedDict()
for k, v in src.items():
    if k == 'key':
        continue
    if k == 'background':
        ff['background'] = {"scripts": ["fb-classify.js", "background.js"]}
    else:
        ff[k] = v
with open(f'{ext}/manifest.firefox.json', 'w') as f:
    json.dump(ff, f, indent=2)
    f.write('\n')
PYEOF

# The four host files ride along in both zips: they are the
# manual host-install path (templates + installer script) for
# anyone who loads the extension without the app package — the
# release zip has always carried them.
HOST_FILES="com.ndm.host.chrome.json.tmpl com.ndm.host.firefox.json.tmpl \
install-host.sh README.md"
FILES="manifest.json background.js content.js fb-classify.js page-hook.js \
popup.html popup.js icon16.png icon48.png icon128.png \
$HOST_FILES"

# zip exits 0 even when a named file is missing from the folder —
# it just leaves it out of the archive. Check the list against the
# source tree up front and fail loudly instead: a zip that ships
# without a file the manifest names is how 2.3.2 broke.
for f in $FILES; do
  if [ ! -f "$EXT/$f" ]; then
    echo "package-extension: $EXT/$f is in the package list but missing on disk" >&2
    exit 1
  fi
done

rm -f "$OUT/ndm-extension-$VERSION.zip"
(cd "$EXT" && zip -q -X "$OUT/ndm-extension-$VERSION.zip" $FILES)

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
(cd "$EXT" && cp $FILES "$TMP"/)
cp "$EXT/manifest.firefox.json" "$TMP/manifest.json"
rm -f "$OUT/ndm-extension-firefox-$VERSION.zip"
(cd "$TMP" && zip -q -X "$OUT/ndm-extension-firefox-$VERSION.zip" $FILES)

python3 - "$OUT" "$VERSION" <<'PYEOF'
import json, sys, zipfile
out, version = sys.argv[1], sys.argv[2]
# the host support files are not named by the manifest, but both
# zips must carry them (see FILES in the shell part)
HOST_FILES = {
    'com.ndm.host.chrome.json.tmpl', 'com.ndm.host.firefox.json.tmpl',
    'install-host.sh', 'README.md',
}
for name in (f'ndm-extension-{version}.zip',
             f'ndm-extension-firefox-{version}.zip'):
    z = zipfile.ZipFile(f'{out}/{name}')
    names = set(z.namelist())
    manifest = json.loads(z.read('manifest.json'))
    # EVERY file the manifest names: icons (both maps), background,
    # action popup + its icons, and all content-script js
    need = set()
    need.update((manifest.get('icons') or {}).values())
    for cs in manifest.get('content_scripts', []):
        need.update(cs.get('js', []))
    bg = manifest.get('background', {})
    if bg.get('service_worker'):
        need.add(bg['service_worker'])
    need.update(bg.get('scripts', []))
    action = manifest.get('action') or {}
    if action.get('default_popup'):
        need.add(action['default_popup'])
    need.update((action.get('default_icon') or {}).values())
    need.update(HOST_FILES)
    missing = need - names
    if missing:
        sys.exit(f'{name}: files missing from zip: {sorted(missing)}')
    print(f'{name}: OK ({len(names)} files)')
PYEOF
echo "packages in $OUT/"
