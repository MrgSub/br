#!/usr/bin/env bash
# br installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/MrgSub/br/main/install.sh | bash
#
# Or pin a specific version:
#   curl -fsSL https://raw.githubusercontent.com/MrgSub/br/main/install.sh | BR_VERSION=v0.1.0 bash
#
# Or install to a custom directory:
#   curl -fsSL .../install.sh | BR_INSTALL_DIR=/opt/br/bin bash
#
# Env vars:
#   BR_VERSION       Tag to install (e.g. `v0.1.0`). Default: latest release.
#   BR_INSTALL_DIR   Install destination. Default: $HOME/.local/bin.
#   BR_NO_MODIFY_PATH=1   Skip the shell-rc PATH suggestion at the end.

set -euo pipefail

# ── colors / logging ──────────────────────────────────────────────────────
if [[ -t 2 ]] && command -v tput >/dev/null 2>&1; then
  c_bold=$(tput bold); c_dim=$(tput dim); c_red=$(tput setaf 1)
  c_green=$(tput setaf 2); c_yellow=$(tput setaf 3); c_reset=$(tput sgr0)
else
  c_bold=""; c_dim=""; c_red=""; c_green=""; c_yellow=""; c_reset=""
fi
log()  { printf '%s>%s %s\n' "$c_bold" "$c_reset" "$*" >&2; }
warn() { printf '%s>%s %s%s%s\n' "$c_bold$c_yellow" "$c_reset" "$c_yellow" "$*" "$c_reset" >&2; }
err()  { printf '%serror:%s %s\n' "$c_bold$c_red" "$c_reset" "$*" >&2; }
ok()   { printf '%s>%s %s%s%s\n' "$c_bold$c_green" "$c_reset" "$c_green" "$*" "$c_reset" >&2; }

# ── platform detect ───────────────────────────────────────────────────────
os=$(uname -s)
arch=$(uname -m)
case "$os/$arch" in
  Darwin/arm64)
    target="aarch64-apple-darwin"
    ;;
  Darwin/x86_64)
    err "br doesn't currently ship an x86_64 macOS build (only Apple Silicon)."
    err "Build from source: https://github.com/MrgSub/br#option-b-build-from-source"
    exit 1
    ;;
  Linux/*)
    err "br is macOS-only for now (depends on WKWebView)."
    err "Linux port is ~2-3 days; see docs/next-steps.md caveat #9."
    exit 1
    ;;
  *)
    err "unsupported platform: $os/$arch"
    exit 1
    ;;
esac

# ── tools ─────────────────────────────────────────────────────────────────
need() { command -v "$1" >/dev/null 2>&1 || { err "missing required tool: $1"; exit 1; }; }
need curl
need shasum
need uname

# ── version resolution ────────────────────────────────────────────────────
repo="MrgSub/br"
version="${BR_VERSION:-}"
if [[ -z "$version" ]]; then
  log "resolving latest release..."
  # Hit the public API; no auth needed for public repos. Follow redirects
  # so we land on the canonical /releases/latest URL, which has the tag in
  # the Location header.
  resolved=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
    "https://github.com/$repo/releases/latest" || true)
  version="${resolved##*/tag/}"
  if [[ -z "$version" || "$version" == "$resolved" ]]; then
    err "couldn't determine latest version. Set BR_VERSION explicitly."
    exit 1
  fi
fi

# ── download ──────────────────────────────────────────────────────────────
asset="br-${version}-${target}"
base="https://github.com/$repo/releases/download/$version"
bin_url="$base/$asset"
sha_url="$base/$asset.sha256"

tmp=$(mktemp -d -t br-install.XXXXXX)
trap 'rm -rf "$tmp"' EXIT

log "downloading $asset ($version)"
if ! curl -fsSL --progress-bar -o "$tmp/br" "$bin_url" >&2; then
  err "download failed: $bin_url"
  err "check that $version exists on https://github.com/$repo/releases"
  exit 1
fi

# ── checksum ──────────────────────────────────────────────────────────────
log "verifying checksum"
if curl -fsSL -o "$tmp/br.sha256" "$sha_url"; then
  # Release assets store the sha256 with the original filename; rewrite
  # to point at our local copy before invoking shasum -c.
  expected=$(awk '{print $1}' "$tmp/br.sha256")
  printf '%s  %s\n' "$expected" "$tmp/br" > "$tmp/br.sha256.local"
  if ! shasum -a 256 -c "$tmp/br.sha256.local" >/dev/null 2>&1; then
    err "checksum mismatch on $asset"
    err "expected: $expected"
    err "got:      $(shasum -a 256 "$tmp/br" | awk '{print $1}')"
    exit 1
  fi
else
  warn "no .sha256 sidecar at $sha_url; skipping checksum"
fi

# ── install ───────────────────────────────────────────────────────────────
install_dir="${BR_INSTALL_DIR:-$HOME/.local/bin}"
mkdir -p "$install_dir"
install_path="$install_dir/br"

# Atomic-ish: chmod first, then mv. Avoids brief window where users see a
# half-written executable.
chmod 0755 "$tmp/br"
mv "$tmp/br" "$install_path"

# Strip macOS Gatekeeper quarantine. Without this the first launch from
# Finder/Terminal pops the "downloaded from internet" dialog. Safe no-op
# if the attribute isn't set.
if [[ "$os" == "Darwin" ]]; then
  xattr -d com.apple.quarantine "$install_path" 2>/dev/null || true
fi

# ── verify ────────────────────────────────────────────────────────────────
if ! "$install_path" --version >/dev/null 2>&1; then
  err "installed binary at $install_path doesn't run."
  err "if Gatekeeper is blocking it, try: xattr -d com.apple.quarantine $install_path"
  exit 1
fi
installed_ver=$("$install_path" --version 2>/dev/null | head -1)

ok "installed $installed_ver at $install_path"

# ── PATH check ────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$install_dir:"*)
    ok "$install_dir is on your PATH; you can run \`br fetch <url>\` now."
    ;;
  *)
    if [[ "${BR_NO_MODIFY_PATH:-0}" == "1" ]]; then
      log "$install_dir is not on your PATH. Add it manually."
    else
      cat >&2 <<EOF

${c_yellow}${c_bold}Note:${c_reset} ${c_yellow}$install_dir is not on your PATH.${c_reset}

Add it with one of:

    ${c_dim}# bash:${c_reset}
    echo 'export PATH="$install_dir:\$PATH"' >> ~/.bashrc

    ${c_dim}# zsh (default on macOS):${c_reset}
    echo 'export PATH="$install_dir:\$PATH"' >> ~/.zshrc

    ${c_dim}# fish:${c_reset}
    fish_add_path $install_dir

Then open a new shell, or run \`source ~/.zshrc\` (or equivalent).

You can also invoke it directly: ${c_bold}$install_path fetch <url>${c_reset}
EOF
    fi
    ;;
esac

cat >&2 <<EOF

Try it:
    ${c_bold}br fetch https://example.com/${c_reset}
    ${c_bold}br fetch https://en.wikipedia.org/wiki/Rust_(programming_language)${c_reset}

Docs: https://github.com/$repo
EOF
