#!/usr/bin/env bash
# Build the release artifacts that scripts/install.sh expects:
#
#   dist/jade-v<version>-<target>.tar.gz
#   dist/jade-v<version>-<target>.tar.gz.sha256
#   dist/latest                     # contains <version>
#
# Upload the whole dist/ directory to any static host, then point the
# installer at it:
#
#   ./scripts/install.sh --base-url https://YOUR_HOST/jade/releases
#
# Cross builds: pass --target and have the Rust target installed.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

TARGET=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET="${2:?--target needs a value}"; shift 2 ;;
        -h|--help) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done

VERSION="$(sed -n '/^\[workspace.package\]/,/^\[/p' Cargo.toml | sed -n 's/^version = "\(.*\)"/\1/p' | head -1)"
[[ -n "$VERSION" ]] || { echo "cannot read the workspace version" >&2; exit 1; }

if [[ -z "$TARGET" ]]; then
    TARGET="$(rustc -vV | awk '/^host: /{print $2}')"
    BUILD_ARGS=(--release -p jade-cli)
else
    BUILD_ARGS=(--release -p jade-cli --target "$TARGET")
fi

echo "==> building jade $VERSION for $TARGET"
cargo build "${BUILD_ARGS[@]}"

BIN="target/${TARGET}/release/jade"
[[ -f "$BIN" ]] || BIN="target/release/jade"
[[ -f "$BIN" ]] || { echo "binary not found at $BIN" >&2; exit 1; }

NAME="jade-v${VERSION}-${TARGET}"
DIST="$ROOT/dist"
CHANNEL="$DIST/v${VERSION}"
STAGE="$DIST/stage/$NAME"
rm -rf "$STAGE"
rm -f "$CHANNEL/$NAME.tar.gz" "$CHANNEL/$NAME.tar.gz.sha256"
mkdir -p "$STAGE" "$CHANNEL"

install -m 0755 "$BIN" "$STAGE/jade"
if [[ -f LICENSE ]]; then
    cp LICENSE "$STAGE/LICENSE"
fi
tar -C "$DIST/stage" -czf "$CHANNEL/$NAME.tar.gz" "$NAME"
rm -rf "$DIST/stage"

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$CHANNEL" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
else
    (cd "$CHANNEL" && shasum -a 256 "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
fi

printf '%s\n' "$VERSION" > "$DIST/latest"

echo
echo "artifacts:"
find "$DIST" -type f | sort
echo
echo "upload the contents of dist/ to \$BASE/ (so \$BASE/latest and \$BASE/v${VERSION}/... exist)"
echo "install:  ./scripts/install.sh --base-url https://YOUR_HOST/jade/releases --version $VERSION"
