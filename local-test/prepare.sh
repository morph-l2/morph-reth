#!/usr/bin/env bash

set -euo pipefail

# shellcheck disable=SC1091
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/common.sh"
cd "${REPO_ROOT}"

check_binary "${RETH_BIN}" "cargo build --release --bin morph-reth"
check_binary "${MORPHNODE_BIN}" "run: cd ../morph/node && make build"

mkdir -p "${RETH_DATA_DIR}"
mkdir -p "${NODE_HOME}"
mkdir -p "${NODE_HOME}/config"
mkdir -p "${NODE_HOME}/data"
mkdir -p "$(dirname "${RETH_LOG_FILE}")"
mkdir -p "$(dirname "${NODE_LOG_FILE}")"

if [[ ! -f "${JWT_SECRET}" ]]; then
  openssl rand -hex 32 > "${JWT_SECRET}"
  chmod 600 "${JWT_SECRET}"
fi

if [[ ! -f "${NODE_HOME}/config/config.toml" || ! -f "${NODE_HOME}/config/genesis.json" || ! -f "${NODE_HOME}/data/priv_validator_state.json" ]]; then
  if [[ "${DOWNLOAD_CONFIG_IF_MISSING}" == "1" ]]; then
    if ! command -v curl >/dev/null 2>&1; then
      echo "curl is required to download Morph config bundle."
      exit 1
    fi
    if ! command -v unzip >/dev/null 2>&1; then
      echo "unzip is required to extract Morph config bundle."
      exit 1
    fi

    mkdir -p "$(dirname "${CONFIG_ZIP_PATH}")"

    temp_extract_dir="$(mktemp -d "${SCRIPT_DIR}/config-prep.XXXXXX")"
    cleanup_temp() {
      if [[ "${KEEP_CONFIG_ARTIFACTS}" != "1" ]]; then
        rm -rf "${temp_extract_dir}"
        rm -f "${CONFIG_ZIP_PATH}"
      fi
    }
    trap cleanup_temp EXIT

    echo "Downloading ${MORPH_NETWORK} config bundle..."
    curl -fL "${CONFIG_ZIP_URL}" -o "${CONFIG_ZIP_PATH}"
    unzip -oq "${CONFIG_ZIP_PATH}" -d "${temp_extract_dir}"

    bundle_root=""
    for candidate in \
      "${temp_extract_dir}/data" \
      "${temp_extract_dir}/${MORPH_NETWORK}-data" \
      "${temp_extract_dir}"
    do
      if [[ -f "${candidate}/node-data/config/config.toml" && -f "${candidate}/node-data/config/genesis.json" ]]; then
        bundle_root="${candidate}"
        break
      fi
    done

    if [[ -z "${bundle_root}" ]]; then
      echo "Downloaded zip does not contain expected node-data config files."
      exit 1
    fi

    cp -f "${bundle_root}/node-data/config/config.toml" "${NODE_HOME}/config/config.toml"
    cp -f "${bundle_root}/node-data/config/genesis.json" "${NODE_HOME}/config/genesis.json"
    if [[ -f "${bundle_root}/node-data/config/addrbook.json" ]]; then
      cp -f "${bundle_root}/node-data/config/addrbook.json" "${NODE_HOME}/config/addrbook.json"
    fi
    if [[ -f "${bundle_root}/node-data/config/node_key.json" && ! -f "${NODE_HOME}/config/node_key.json" ]]; then
      cp -f "${bundle_root}/node-data/config/node_key.json" "${NODE_HOME}/config/node_key.json"
    fi
    if [[ -f "${bundle_root}/node-data/config/priv_validator_key.json" && ! -f "${NODE_HOME}/config/priv_validator_key.json" ]]; then
      cp -f "${bundle_root}/node-data/config/priv_validator_key.json" "${NODE_HOME}/config/priv_validator_key.json"
    fi
    if [[ -f "${bundle_root}/node-data/data/priv_validator_state.json" ]]; then
      cp -f "${bundle_root}/node-data/data/priv_validator_state.json" "${NODE_HOME}/data/priv_validator_state.json"
    fi
    echo "Config prepared at ${NODE_HOME} from ${CONFIG_ZIP_URL}"
  else
    echo "Warning: node-data is incomplete under ${NODE_HOME}."
    echo "Set DOWNLOAD_CONFIG_IF_MISSING=1 or prepare config files manually."
  fi
fi

# Tendermint needs this state file. Some published bundles do not include it.
if [[ ! -f "${NODE_HOME}/data/priv_validator_state.json" ]]; then
  cat > "${NODE_HOME}/data/priv_validator_state.json" <<'EOF'
{"height":"0","round":0,"step":0}
EOF
fi

# If the previous run failed with replay "wrong block number", suggest or trigger a clean reset.
if [[ -f "${NODE_LOG_FILE}" ]] && grep -q "wrong block number" "${NODE_LOG_FILE}"; then
  echo
  echo "Detected historical replay failure in ${NODE_LOG_FILE}: wrong block number"
  if [[ "${AUTO_RESET_ON_WRONG_BLOCK}" == "1" ]]; then
    echo "AUTO_RESET_ON_WRONG_BLOCK=1, resetting local sync state..."
    "${SCRIPT_DIR}/reset.sh" --yes
  else
    echo "If replay fails again, run: $(rel_path "${SCRIPT_DIR}")/reset.sh --yes"
  fi
fi

echo "Preparation finished."
echo "MORPH_NETWORK=${MORPH_NETWORK}"
echo "RETH_DATA_DIR=$(rel_path "${RETH_DATA_DIR}")"
echo "NODE_HOME=$(rel_path "${NODE_HOME}")"
echo "JWT_SECRET=$(rel_path "${JWT_SECRET}")"
