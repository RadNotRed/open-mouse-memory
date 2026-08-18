# 🖱️ Open Mouse Memory

A lightweight Linux app for managing mouse onboard memory.

Tested with the Logitech PRO X SUPERLIGHT 2. Other HID++ mice may work but still need testing.

## ✨ Features

- Read battery, DPI, polling rate, and onboard profiles
- Save DPI stages, polling rate, and button assignments to the mouse
- Use the desktop app, CLI, or tray icon
- Cache device details for a faster startup

## 🧰 Requirements

- Linux
- Rust 1.85 or newer
- `libudev` and `pkg-config`

Ubuntu and Debian:

```bash
sudo apt install libudev-dev pkg-config
```

## 🔨 Build

```bash
cargo build --release --locked
```

## 🚀 Run

Desktop app:

```bash
./target/release/open-mouse-memory-gui
```

CLI:

```bash
./target/release/open-mouse-memory --help
./target/release/open-mouse-memory battery --details
```

## 🔐 Mouse access

The app can ask for permission when it finds an inaccessible mouse. You can also install the rule manually:

```bash
sudo install -Dm644 packaging/udev/70-open-mouse-memory.rules /etc/udev/rules.d/70-open-mouse-memory.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --action=add --subsystem-match=hidraw
```

Reconnect the mouse or receiver after installing the rule.

## 👩‍💻 Development

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets --locked -- -D warnings
cargo test --all-features --locked
```

Build the AppImage:

```bash
./packaging/appimage/build.sh
```

GitHub Actions checks formatting, linting, tests, and AppImage builds.

## 📄 License

Open Mouse Memory is licensed under GPL-3.0-only. See [LICENSE](LICENSE).
