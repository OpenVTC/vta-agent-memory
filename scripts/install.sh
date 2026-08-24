#!/usr/bin/env bash
#
# Build the memory binary and place it where the plugin expects it.
#
# The plugin's `.mcp.json` and `hooks.json` both invoke
# `${CLAUDE_PLUGIN_ROOT}/bin/vta-agent-memory`, an absolute path inside the
# plugin directory, rather than relying on `PATH`. That is deliberate: Claude
# Code launches MCP servers and hooks with an environment that need not match an
# interactive shell, and a plugin that works in the terminal but not when
# launched is the worst version of this to debug.
#
# Usage: scripts/install.sh [--debug]

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
profile="release"
profile_flag="--release"

if [[ "${1:-}" == "--debug" ]]; then
  profile="debug"
  profile_flag=""
fi

echo "Building vta-agent-memory (${profile})…"
# shellcheck disable=SC2086 # profile_flag is intentionally word-split (may be empty)
cargo build --manifest-path "${root}/Cargo.toml" ${profile_flag}

mkdir -p "${root}/bin"
built="${root}/target/${profile}/vta-agent-memory"
if [[ ! -x "${built}" ]]; then
  echo "error: expected a binary at ${built}" >&2
  exit 1
fi

# Copy rather than symlink: a symlink into `target/` breaks the moment someone
# runs `cargo clean`, and it breaks silently — the plugin just stops having
# memory.
install -m 0755 "${built}" "${root}/bin/vta-agent-memory"
echo "Installed ${root}/bin/vta-agent-memory"

if [[ ! -f "${VTA_AGENT_MEMORY_CONFIG:-${XDG_CONFIG_HOME:-${HOME}/.config}/vta-agent-memory/config.json}" ]]; then
  cat <<'EOF'

Not configured yet. It bootstraps from a VTA you have already logged into
with `pnm` on this machine:

  bin/vta-agent-memory setup                      # your default pnm VTA
  bin/vta-agent-memory setup --vta did:webvh:...  # a specific one, by DID

Then check it:

  bin/vta-agent-memory doctor
EOF
fi
