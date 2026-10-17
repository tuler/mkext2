#!/usr/bin/env bash
# Publish the per-platform npm packages of @tuler/mkext2 (and, with --main,
# the main package too) from build artifacts.
#
#   scripts/publish-npm.sh <artifacts-dir> [--main]
#
# <artifacts-dir> holds the `mkext2.<platform>.node` files and, optionally,
# freshly generated `index.js`/`index.d.ts` (any directory layout: the
# output of `gh run download` works as is).
#
# In CI the main package is published by Sampo afterwards, so only the
# platform packages are published here. For the first, manual release pass
# --main to publish everything.
#
# Packages whose version is already on npm are skipped, so the script can be
# re-run safely. Set NPM_PUBLISH_DRY_RUN=1 to pass --dry-run to npm.
set -euo pipefail

if [ $# -lt 1 ]; then
  echo "usage: $0 <artifacts-dir> [--main]" >&2
  exit 2
fi
artifacts=$(cd "$1" && pwd)
with_main=false
[ "${2:-}" = "--main" ] && with_main=true

root=$(cd "$(dirname "$0")/.." && pwd)
pkg="$root/crates/mkext2-node"
version=$(node -p "require('$pkg/package.json').version")

dry_run=()
if [ "${NPM_PUBLISH_DRY_RUN:-}" = "1" ]; then
  dry_run=(--dry-run)
  echo "[dry-run] nothing will be published"
fi

find_artifact() { # file name
  find "$artifacts" -type f -name "$1" | head -n 1
}

published() { # name version
  [ -n "$(npm view "$1@$2" version 2>/dev/null || true)" ]
}

# A newly published version can take a minute or more to appear in the
# registry's metadata. The release workflow's next step (Sampo's publish
# plan) skips a package only if it sees the version, so wait until every
# package published here shows it, in both the full metadata (`npm view`) and
# the abbreviated metadata that installers read.
visible() { # name version
  npm view "$1@$2" version --prefer-online 2>/dev/null | grep -qxF "$2" || return 1
  curl -fsS -H 'Accept: application/vnd.npm.install-v1+json' -H 'Cache-Control: no-cache' \
    "https://registry.npmjs.org/${1/\//%2f}" 2>/dev/null |
    node -e "let s = ''; process.stdin.on('data', (d) => (s += d)).on('end', () => process.exit(JSON.parse(s).versions?.[process.argv[1]] ? 0 : 1))" "$2"
}

wait_visible() { # name version
  local deadline=$((SECONDS + ${NPM_VISIBLE_TIMEOUT:-600}))
  until visible "$1" "$2"; do
    if [ $SECONDS -ge $deadline ]; then
      echo "error: $1@$2 was published but is still not visible on npm" >&2
      return 1
    fi
    sleep 10
  done
  echo "visible on npm: $1@$2"
}
just_published=()

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# Check everything before publishing anything.
for dir in "$pkg"/npm/*/; do
  manifest="$dir/package.json"
  v=$(node -p "require('$manifest').version")
  if [ "$v" != "$version" ]; then
    echo "error: $manifest is at $v but @tuler/mkext2 is at $version" >&2
    exit 1
  fi
  file=$(node -p "require('$manifest').main")
  if [ -z "$(find_artifact "$file")" ]; then
    echo "error: missing artifact $file in $artifacts" >&2
    exit 1
  fi
done

for dir in "$pkg"/npm/*/; do
  manifest="$dir/package.json"
  name=$(node -p "require('$manifest').name")
  file=$(node -p "require('$manifest').main")
  if published "$name" "$version"; then
    echo "skip $name@$version: already on npm"
    continue
  fi
  out="$work/$(basename "$dir")"
  mkdir -p "$out"
  cp "$manifest" "$out/package.json"
  if [ -f "$dir/README.md" ]; then cp "$dir/README.md" "$out/"; fi
  cp "$(find_artifact "$file")" "$out/$file"
  echo "publishing $name@$version"
  (cd "$out" && npm publish ${dry_run[@]+"${dry_run[@]}"})
  just_published+=("$name")
done

if $with_main; then
  name=$(node -p "require('$pkg/package.json').name")
  if published "$name" "$version"; then
    echo "skip $name@$version: already on npm"
  else
    out="$work/main"
    mkdir -p "$out"
    cp "$pkg/package.json" "$pkg/README.md" "$pkg/index.js" "$pkg/index.d.ts" "$out/"
    # prefer the loader generated for this version, when provided
    for f in index.js index.d.ts; do
      generated=$(find_artifact "$f")
      if [ -n "$generated" ]; then cp "$generated" "$out/$f"; fi
    done
    echo "publishing $name@$version"
    (cd "$out" && npm publish ${dry_run[@]+"${dry_run[@]}"})
    just_published+=("$name")
  fi
fi

if [ ${#dry_run[@]} -eq 0 ]; then
  for name in ${just_published[@]+"${just_published[@]}"}; do
    wait_visible "$name" "$version"
  done
fi

echo "npm publish step done (version $version)"
