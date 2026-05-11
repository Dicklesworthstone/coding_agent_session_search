#!/usr/bin/env bash
set -euo pipefail

version_file="${UBS_VERSION_FILE:-.github/workflows/ubs-version.txt}"
install_dir="${UBS_INSTALL_DIR:-$HOME/.local/bin}"

if [ -f "$version_file" ]; then
  ubs_version="$(tr -d '[:space:]' < "$version_file")"
else
  ubs_version="latest"
fi

if [ -z "$ubs_version" ] || [ "$ubs_version" = "latest" ]; then
  installer_ref="main"
else
  installer_ref="v$ubs_version"
fi

installer_url="https://raw.githubusercontent.com/Dicklesworthstone/ultimate_bug_scanner/${installer_ref}/install.sh"
installer_path="${RUNNER_TEMP:-/tmp}/ubs-install-${installer_ref}.sh"

echo "Installing Ultimate Bug Scanner from ${installer_url}"
curl -fsSL "$installer_url" -o "$installer_path"
bash "$installer_path" \
  --non-interactive \
  --skip-hooks \
  --skip-version-check \
  --install-dir "$install_dir"

if [ -n "${GITHUB_PATH:-}" ]; then
  echo "$install_dir" >> "$GITHUB_PATH"
fi

export PATH="$install_dir:$PATH"
ubs --version
ubs doctor
