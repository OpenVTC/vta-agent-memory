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

echo "Installing vta-agent-memory (${profile})…"

# `cargo install` rather than a copy into bin/. bin/ holds a committed shim that
# finds the binary wherever cargo put it — which is what makes the plugin work
# when Claude Code installs it from a marketplace, where bin/ is a fresh clone
# with no compiled artifacts in it.
if [[ "${profile}" == "debug" ]]; then
  cargo install --debug --path "${root}" --force
else
  cargo install --path "${root}" --force
fi

installed="${CARGO_HOME:-${HOME}/.cargo}/bin/vta-agent-memory"
if [[ ! -x "${installed}" ]]; then
  echo "error: expected a binary at ${installed}" >&2
  exit 1
fi
echo "Installed ${installed}"

config="${VTA_AGENT_MEMORY_CONFIG:-${XDG_CONFIG_HOME:-${HOME}/Library/Application Support}/vta-agent-memory/config.json}"
if [[ ! -f "${config}" ]]; then
  cat <<'EOF'

Enrol this machine. It mints a temporary identity for somebody with VTA admin
to authorize — they do not have to be on this machine:

  vta-agent-memory init --vta-did <did:…> --context agent-memory

…then, once the printed grant has been run:

  vta-agent-memory connect

If you hold admin here, `vta-agent-memory setup` does both at once.
EOF
fi

cat <<'EOF'

Add it to Claude Code (two steps — `install` alone cannot find a plugin whose
marketplace has not been added):

  claude plugin marketplace add OpenVTC/vta-agent-memory
  claude plugin install vta-agent-memory@vta-agent-memory
EOF
