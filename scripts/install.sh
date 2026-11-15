#!/bin/sh
# envoy installer — download a release, put it on PATH, done.
#
#   curl -fsSL https://YOUR_HOST/envoy/install.sh | sh
#   curl -fsSL ... | sh -s -- --version 0.1.0
#   ./install.sh --base-url https://YOUR_HOST/envoy/releases
#
# The default release host is set below; override with --base-url or
# ENVOY_RELEASE_BASE. After install, `envoy` works in a new shell and starts
# its daemon on first use (no service manager required).
set -eu

DEFAULT_BASE="https://github.com/Elysia5250/envoy/releases/download"

BASE="${ENVOY_RELEASE_BASE:-$DEFAULT_BASE}"
VERSION="${ENVOY_VERSION:-}"
DIR="${ENVOY_INSTALL_DIR:-$HOME/.envoy/bin}"
ADD_PATH=1
INSTALL_SERVICE=0

say() { printf '  %s\n' "$*"; }
step() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m note\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
    cat <<'EOF'
envoy installer

  --version <v>      install a specific version (default: the release's `latest`)
  --base-url <url>   release host (default: $ENVOY_RELEASE_BASE or the GitHub URL)
  --dir <path>       install directory (default: ~/.envoy/bin)
  --no-path          do not modify shell startup files
  --service          also install login autostart (launchd / systemd --user)
  -h, --help         this text

Environment: ENVOY_VERSION, ENVOY_RELEASE_BASE, ENVOY_INSTALL_DIR
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
        --base-url) [ $# -ge 2 ] || die "--base-url needs a value"; BASE="${2%/}"; shift 2 ;;
        --dir) [ $# -ge 2 ] || die "--dir needs a value"; DIR="$2"; shift 2 ;;
        --no-path) ADD_PATH=0; shift ;;
        --service) INSTALL_SERVICE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (try --help)" ;;
    esac
done

command -v curl >/dev/null 2>&1 || die "curl is required"

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Darwin)
        case "$arch" in
            arm64) target="aarch64-apple-darwin" ;;
            x86_64) target="x86_64-apple-darwin" ;;
            *) die "unsupported macOS architecture: $arch" ;;
        esac
        ;;
    Linux)
        case "$arch" in
            x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
            aarch64|arm64) target="aarch64-unknown-linux-gnu" ;;
            *) die "unsupported Linux architecture: $arch" ;;
        esac
        ;;
    *) die "unsupported OS: $os (macOS and Linux only)" ;;
esac

if [ -z "$VERSION" ]; then
    step "resolving the latest version from $BASE"
    VERSION="$(curl -fsSL "$BASE/latest" 2>/dev/null | tr -d 'v \r\n' || true)"
    [ -n "$VERSION" ] || die "cannot read $BASE/latest; pass --version or --base-url"
fi
VERSION="${VERSION#v}"

name="envoy-v${VERSION}-${target}"
url="$BASE/v${VERSION}/${name}.tar.gz"
tmp="$(mktemp -d 2>/dev/null || mktemp -d -t envoy)"
trap 'rm -rf "$tmp"' EXIT

step "downloading $url"
curl -fsSL "$url" -o "$tmp/$name.tar.gz" || die "download failed: $url
  check --version / --base-url, or self-host with scripts/package.sh"

if curl -fsSL "$url.sha256" -o "$tmp/$name.tar.gz.sha256" 2>/dev/null; then
    step "verifying checksum"
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$tmp" && sha256sum -c "$name.tar.gz.sha256" >/dev/null) || die "checksum mismatch"
    elif command -v shasum >/dev/null 2>&1; then
        (cd "$tmp" && shasum -a 256 -c "$name.tar.gz.sha256" >/dev/null) || die "checksum mismatch"
    else
        warn "no sha256sum/shasum; skipping verification"
    fi
else
    warn "no .sha256 published; skipping verification"
fi

step "extracting"
tar -xzf "$tmp/$name.tar.gz" -C "$tmp" || die "cannot extract the release archive"
[ -f "$tmp/$name/envoy" ] || die "archive does not contain $name/envoy"

step "installing to $DIR"
mkdir -p "$DIR"
cp "$tmp/$name/envoy" "$DIR/envoy"
chmod 0755 "$DIR/envoy"

installed="$("$DIR/envoy" --version 2>/dev/null || true)"
[ -n "$installed" ] || die "the installed binary does not run on this machine"
say "$installed"

add_to_rc() {
    rc="$1"
    line="$2"
    if [ -f "$rc" ] && grep -Fq "$line" "$rc" 2>/dev/null; then
        return 0
    fi
    mkdir -p "$(dirname "$rc")"
    printf '\n# added by the envoy installer\n%s\n' "$line" >>"$rc"
    say "added to $rc"
}

if [ "$ADD_PATH" = 1 ]; then
    case ":$PATH:" in
        *":$DIR:"*) ;;
        *)
            step "adding $DIR to PATH"
            shell_name="$(basename "${SHELL:-sh}")"
            case "$shell_name" in
                zsh) add_to_rc "$HOME/.zshrc" "export PATH=\"$DIR:\$PATH\"" ;;
                fish) add_to_rc "$HOME/.config/fish/config.fish" "fish_add_path \"$DIR\"" ;;
                bash)
                    if [ "$os" = "Darwin" ]; then
                        add_to_rc "$HOME/.bash_profile" "export PATH=\"$DIR:\$PATH\""
                    else
                        add_to_rc "$HOME/.bashrc" "export PATH=\"$DIR:\$PATH\""
                    fi
                    ;;
                *) add_to_rc "$HOME/.profile" "export PATH=\"$DIR:\$PATH\"" ;;
            esac
            ;;
    esac
fi

if [ "$INSTALL_SERVICE" = 1 ]; then
    step "installing login autostart"
    "$DIR/envoy" daemon install || warn "autostart install failed; `envoy` will still start the daemon on first use"
fi

cat <<EOF

$(printf '\033[1;32m✓\033[0m') envoy $VERSION installed

  new shell:   envoy --version
  right now:   $DIR/envoy status
  first run:   envoy status          (auto-starts the daemon)
  try it:      envoy run "uname -a"
               envoy ui

Autostart at login:  envoy daemon install
Uninstall:           rm -rf "$DIR" "$HOME/.envoy"
EOF
