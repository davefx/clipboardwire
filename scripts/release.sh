#!/usr/bin/env bash
# Usage: ./scripts/release.sh 0.4.4
#
# Bumps version across Cargo.toml, README.md, and CHANGELOG.md,
# commits, tags, and pushes. The Release workflow fires on the
# pushed tag and builds all artifacts; the Bump Homebrew workflow
# auto-fires after that.
set -euo pipefail

VERSION="${1:?Usage: $0 <version>  (e.g. 0.4.4)}"
TAG="v${VERSION}"
DATE="$(date +%Y-%m-%d)"
ROOT="$(git rev-parse --show-toplevel)"

# Sanity checks
if ! git diff --quiet || ! git diff --cached --quiet; then
  echo "ERROR: working tree is dirty. Commit or stash first." >&2
  exit 1
fi

if git rev-parse "$TAG" >/dev/null 2>&1; then
  echo "ERROR: tag $TAG already exists." >&2
  exit 1
fi

PREV_VERSION="$(grep '^version' "$ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')"
echo "Bumping $PREV_VERSION → $VERSION"

# 1. Cargo.toml workspace version
sed -i "s/^version = \"$PREV_VERSION\"/version = \"$VERSION\"/" "$ROOT/Cargo.toml"

# 2. Cargo.lock (regenerate)
cargo generate-lockfile --manifest-path "$ROOT/Cargo.toml"

# 3. README.md install URLs + "Currently shipping" line
sed -i "s/$PREV_VERSION/$VERSION/g" "$ROOT/README.md"

# 4. Android versionName + auto-increment versionCode
ANDROID_GRADLE="$ROOT/android/app/build.gradle.kts"
sed -i "s/versionName = \"$PREV_VERSION\"/versionName = \"$VERSION\"/" "$ANDROID_GRADLE"
OLD_CODE="$(grep 'versionCode' "$ANDROID_GRADLE" | sed 's/[^0-9]//g')"
NEW_CODE=$((OLD_CODE + 1))
sed -i "s/versionCode = $OLD_CODE/versionCode = $NEW_CODE/" "$ANDROID_GRADLE"
echo "Android: versionCode $OLD_CODE → $NEW_CODE, versionName → $VERSION"

# 5. CHANGELOG.md — insert new section before the first "## [" line only
sed -i "0,/^## \[/s//## [$VERSION] — $DATE\n\n### Changed\n- (fill in before pushing)\n\n## [/" "$ROOT/CHANGELOG.md"

# Add link reference
sed -i "/^\[$PREV_VERSION\]: /i\\
[$VERSION]: https://github.com/davefx/clipboardwire/releases/tag/$TAG" "$ROOT/CHANGELOG.md"

echo ""
echo "Files updated. Review the changes:"
echo "  git diff"
echo ""
echo "Edit CHANGELOG.md with the actual release notes, then run:"
echo "  git add -A && git commit -m '$TAG: <summary>'"
echo "  git tag -a $TAG -m '$TAG'"
echo "  git push && git push origin $TAG"
