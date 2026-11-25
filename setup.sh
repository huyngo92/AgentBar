#!/bin/bash
set -e

echo "=== Claude Gateway Switcher Setup (Cross-Platform) ==="

# 1. Check Prerequisites
if ! command -v node &> /dev/null; then
    echo "⚠ Node.js is not installed. Please install it first."
    exit 1
fi

if ! command -v rustc &> /dev/null; then
    echo "⚠ Rust is not installed. Please install it: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi

# 2. Install and Build Tauri App
echo "Building Tauri Application..."
cd tauri-app
npm install
npm run tauri build
cd ..

# 3. Setup CLI (Legacy support)
echo "Setting up CLI..."
chmod +x claude-switch.mjs
sudo ln -sf "$(pwd)/claude-switch.mjs" /usr/local/bin/claude-switch 2>/dev/null || {
    echo "⚠ Could not symlink to /usr/local/bin. Use: alias claude-switch='node $(pwd)/claude-switch.mjs'"
}

echo ""
echo "=== Done ==="
echo "Application: Check your Applications folder or build artifacts."
echo "CLI: claude-switch list|use|add|rm|current"
echo ""
echo "To auto-start Tauri app on login, add the built .app to System Settings → Login Items."
