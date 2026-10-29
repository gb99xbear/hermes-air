#!/usr/bin/env bash
# ==============================================================================
# Hermes Air 🍃 - One-Line Universal Installer Script
# Lightweight, high-performance personal AI agent and gateway written in Rust.
# ==============================================================================

set -euo pipefail

# ANSI Color Codes
BOLD="\033[1m"
GREEN="\033[1;32m"
CYAN="\033[1;36m"
YELLOW="\033[1;33m"
RED="\033[1;31m"
DIM="\033[2m"
RESET="\033[0m"

echo -e "${CYAN}====================================================${RESET}"
echo -e "${GREEN}🌿 Hermes Air 🍃 - One-Line Universal Installer${RESET}"
echo -e "${DIM}High-Performance Rust Personal AI Agent & Gateway${RESET}"
echo -e "${CYAN}====================================================${RESET}\n"

# 1. Detect Architecture and OS
OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
ARCH="$(uname -m)"

case "$ARCH" in
    x86_64|amd64)
        TARGET_ARCH="x86_64"
        ;;
    aarch64|arm64)
        TARGET_ARCH="aarch64"
        ;;
    *)
        echo -e "${RED}❌ Unsupported architecture: ${ARCH}${RESET}"
        exit 1
        ;;
esac

case "$OS" in
    linux)
        TARGET_OS="linux"
        ;;
    darwin)
        TARGET_OS="darwin"
        ;;
    *)
        echo -e "${RED}❌ Unsupported operating system: ${OS}${RESET}"
        exit 1
        ;;
esac

echo -e "🖥️  Detected platform: ${BOLD}${TARGET_OS} (${TARGET_ARCH})${RESET}"

# 2. Check for Rust Toolchain (cargo & rustc)
if ! command -v cargo &>/dev/null || ! command -v rustc &>/dev/null; then
    echo -e "${YELLOW}⚠️  Rust toolchain not detected.${RESET}"
    echo -e "⏳ Installing official Rust toolchain via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # Source environment immediately
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck source=/dev/null
        source "$HOME/.cargo/env"
    fi
fi

if ! command -v cargo &>/dev/null; then
    echo -e "${RED}❌ Cargo is still not available in PATH. Please run 'source ~/.cargo/env' and retry.${RESET}"
    exit 1
fi

RUST_VER="$(rustc --version)"
echo -e "🦀 Toolchain ready: ${DIM}${RUST_VER}${RESET}"

# 3. Locate source directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ ! -f "${SCRIPT_DIR}/Cargo.toml" ]; then
    # If run via curl pipe, clone into temporary or ~/.hermes/hermes-air
    INSTALL_DIR="${HOME}/.hermes/hermes-air"
    echo -e "📦 Cloning Hermes Air source repository into ${INSTALL_DIR}..."
    mkdir -p "${HOME}/.hermes"
    if [ -d "${INSTALL_DIR}/.git" ]; then
        git -C "${INSTALL_DIR}" pull --quiet
    else
        git clone --quiet https://github.com/nousresearch/hermes-agent.git "${INSTALL_DIR}" || true
    fi
    SRC_DIR="${INSTALL_DIR}"
else
    SRC_DIR="${SCRIPT_DIR}"
fi

# 4. Build in Release Mode
echo -e "⚙️  Building ${BOLD}hermes-air${RESET} in release mode (optimizations enabled)..."
cd "${SRC_DIR}"
cargo build --release -p hermes-gateway

RELEASE_BIN="${SRC_DIR}/target/release/hermes-gateway"
if [ ! -f "${RELEASE_BIN}" ]; then
    echo -e "${RED}❌ Release binary not found at ${RELEASE_BIN}${RESET}"
    exit 1
fi

# 5. Install Binary to ~/.local/bin/hermes-air
INSTALL_BIN_DIR="${HOME}/.local/bin"
mkdir -p "${INSTALL_BIN_DIR}"
TARGET_BIN="${INSTALL_BIN_DIR}/hermes-air"

cp -f "${RELEASE_BIN}" "${TARGET_BIN}"
chmod +x "${TARGET_BIN}"

# Also install hermes-gateway alias
ln -sf "${TARGET_BIN}" "${INSTALL_BIN_DIR}/hermes-gateway"

echo -e "✅ Installed binary to: ${BOLD}${TARGET_BIN}${RESET}"

# 6. Ensure ~/.local/bin is in PATH
SHELL_CONFIG=""
if [ -n "${ZSH_VERSION:-}" ] || [ "$(basename "${SHELL:-}")" = "zsh" ]; then
    SHELL_CONFIG="${HOME}/.zshrc"
elif [ -f "${HOME}/.bashrc" ]; then
    SHELL_CONFIG="${HOME}/.bashrc"
elif [ -f "${HOME}/.bash_profile" ]; then
    SHELL_CONFIG="${HOME}/.bash_profile"
elif [ -f "${HOME}/.profile" ]; then
    SHELL_CONFIG="${HOME}/.profile"
fi

if [[ ":${PATH}:" != *":${INSTALL_BIN_DIR}:"* ]]; then
    echo -e "🔍 Adding ${INSTALL_BIN_DIR} to PATH in ${SHELL_CONFIG:-~/.bashrc}..."
    if [ -n "${SHELL_CONFIG}" ] && [ -f "${SHELL_CONFIG}" ]; then
        if ! grep -q "export PATH=.*\.local/bin" "${SHELL_CONFIG}"; then
            echo -e '\n# Hermes Air' >> "${SHELL_CONFIG}"
            echo -e 'export PATH="$HOME/.local/bin:$PATH"' >> "${SHELL_CONFIG}"
        fi
    fi
    export PATH="${INSTALL_BIN_DIR}:${PATH}"
fi

# 7. Generate default config.toml if absent
CONFIG_DEST="config.toml"
if [ ! -f "${CONFIG_DEST}" ]; then
    if [ -f "config.example.toml" ]; then
        echo -e "📄 Creating initial config from config.example.toml..."
        cp config.example.toml "${CONFIG_DEST}"
    else
        echo -e "📄 Generating initial ${CONFIG_DEST}..."
        cat << 'CFG_EOF' > "${CONFIG_DEST}"
[gateway]
name = "hermes-gateway"
version = "0.1.0"
max_concurrent_sessions = 100
default_profile = "default"
shutdown_timeout_secs = 30
restart_on_config_change = false

[profiles.default]
name = "Hermes"
model_provider = "default"
model = "ag/gemini-3.8-flash"
temperature = 0.7
max_tokens = 4096
system_prompt = "You are Hermes, a helpful, capable, and unfiltered personal AI assistant."
tools = ["terminal", "read_file", "write_file"]
skills = []
mcp_servers = []
platform_overrides = {}

[session_store]
backend = "memory"
local_cache_size = 1000
default_ttl_secs = 86400

[logging]
level = "info"
format = "text"
output = "stdout"
file = "hermes-gateway.log"
max_file_size_mb = 100
max_files = 5
CFG_EOF
    fi
fi

# 8. Friendly Completion Message
echo ""
echo -e "${GREEN}✨ Hermes Air 🍃 successfully installed!${RESET}"
echo -e "👉 Run ${BOLD}'hermes-air'${RESET} to start chatting interactively."
echo -e "👉 Run ${BOLD}'hermes-air -q \"your query\"'${RESET} for one-shot CLI execution."
echo -e "👉 Run ${BOLD}'hermes-air gateway'${RESET} to run the multi-platform gateway daemon."
echo ""
