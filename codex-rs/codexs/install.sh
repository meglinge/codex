#!/usr/bin/env bash
# codexs installer (Linux x86_64).
#
#   curl -fsSL https://raw.githubusercontent.com/meglinge/codex/codexs/codex-rs/codexs/install.sh | bash
#
# Environment overrides:
#   CODEXS_VERSION      release version to install, e.g. 0.153.4 (default: latest release)
#   CODEXS_INSTALL_DIR  where binaries + config live      (default: ~/.codexs)
#   CODEXS_BIN_DIR      where the `codexs` symlink goes    (default: ~/.local/bin)
#   CODEXS_REPO         GitHub repo publishing the releases (default: meglinge/codex)
set -euo pipefail

REPO="${CODEXS_REPO:-meglinge/codex}"
VERSION="${CODEXS_VERSION:-latest}"
INSTALL_DIR="${CODEXS_INSTALL_DIR:-$HOME/.codexs}"
BIN_DIR="${CODEXS_BIN_DIR:-$HOME/.local/bin}"
TARGET="x86_64-unknown-linux-gnu"

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

case "$(uname -s)" in
  Linux) ;;
  *) die "codexs prebuilt binaries are only published for Linux and Windows; use install.ps1 on Windows" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) ;;
  *) die "unsupported architecture $(uname -m): only x86_64 builds are published" ;;
esac
command -v curl >/dev/null || die "curl is required"
command -v tar >/dev/null || die "tar is required"

if [[ "$VERSION" == "latest" ]]; then
  api="https://api.github.com/repos/${REPO}/releases/latest"
else
  VERSION="${VERSION#codexs-v}"
  VERSION="${VERSION#v}"
  api="https://api.github.com/repos/${REPO}/releases/tags/codexs-v${VERSION}"
fi

log "Resolving release (${api##*/})"
release_json="$(curl -fsSL -H 'Accept: application/vnd.github+json' "$api")" \
  || die "could not fetch release metadata from $api"
url="$(printf '%s' "$release_json" \
  | grep -o "\"browser_download_url\": *\"[^\"]*codexs-[^\"]*-${TARGET}\.tar\.gz\"" \
  | head -n1 | sed -E 's/.*"(https:[^"]+)"$/\1/')"
[[ -n "$url" ]] || die "no ${TARGET} asset found in that release"
archive="${url##*/}"
version="${archive#codexs-}"
version="${version%-${TARGET}.tar.gz}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

log "Downloading codexs ${version} (${archive})"
curl -fL --progress-bar -o "$tmp/$archive" "$url"

log "Installing into ${INSTALL_DIR}/bin"
tar -xzf "$tmp/$archive" -C "$tmp"
src="$tmp/codexs-${version}-${TARGET}"
[[ -x "$src/codexs" ]] || die "archive did not contain a codexs binary"
mkdir -p "$INSTALL_DIR/bin" "$BIN_DIR"
# Helper binaries (codex-code-mode-host, bwrap) must stay next to codexs;
# replace the whole bin directory so stale helpers never linger.
rm -rf "$INSTALL_DIR/bin.new"
mkdir -p "$INSTALL_DIR/bin.new"
cp -a "$src"/. "$INSTALL_DIR/bin.new/"
rm -rf "$INSTALL_DIR/bin.old"
mv "$INSTALL_DIR/bin" "$INSTALL_DIR/bin.old"
mv "$INSTALL_DIR/bin.new" "$INSTALL_DIR/bin"
rm -rf "$INSTALL_DIR/bin.old"
chmod +x "$INSTALL_DIR/bin/codexs" "$INSTALL_DIR/bin/codex-code-mode-host" "$INSTALL_DIR/bin/bwrap" 2>/dev/null || true

ln -sfn "$INSTALL_DIR/bin/codexs" "$BIN_DIR/codexs"

if [[ ! -f "$INSTALL_DIR/codexs.toml" ]]; then
  cp "$INSTALL_DIR/bin/codexs.example.toml" "$INSTALL_DIR/codexs.toml"
  log "Created ${INSTALL_DIR}/codexs.toml from the example - edit accounts / api_keys before starting"
fi

log "Installed: $("$INSTALL_DIR/bin/codexs" --version)"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '\n\033[1;33mnote:\033[0m %s is not on your PATH. Add this to your shell profile:\n\n    export PATH="%s:$PATH"\n\n' "$BIN_DIR" "$BIN_DIR" ;;
esac
cat <<EOF

Next steps:
  1. Edit ${INSTALL_DIR}/codexs.toml (accounts -> CODEX_HOME with auth.json from \`codex login\`, api_keys).
  2. Run:  codexs            # config lookup: \$CODEXS_CONFIG, ./codexs.toml, ${INSTALL_DIR}/codexs.toml
EOF
