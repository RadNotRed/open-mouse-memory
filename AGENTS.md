# Open Mouse Memory agent guide

## Project

- Use the name `Open Mouse Memory`
- Use `open-mouse-memory` for package names, commands, files, and paths
- Keep Linux and Logitech HID++ behavior compatible
- Treat onboard writes as hardware-sensitive changes
- Do not add old project names

## Code style

- Use Rust 1.85 or newer
- Run `cargo fmt` after Rust changes
- Keep code comments short and lowercase
- Do not end code comments with periods
- Avoid unrelated changes
- Preserve existing user work and build caches

## Checks

Run these before finishing:

```bash
cargo fmt --all -- --check
cargo clippy --all-features --all-targets --locked -- -D warnings
cargo test --all-features --locked
bash -n packaging/appimage/build.sh
```

For packaging changes, also run:

```bash
./packaging/appimage/build.sh
```

## Commits

Use Conventional Commits:

```text
type(optional-scope): short lowercase summary
```

Common types:

- `feat` for a new feature
- `fix` for a bug fix
- `refactor` for internal code changes
- `perf` for performance improvements
- `docs` for documentation
- `test` for test changes
- `build` for build or dependency changes
- `ci` for workflow changes
- `chore` for maintenance

Examples:

```text
feat(gui): add onboard profile selector
fix(hidpp): read battery from the active device
ci: cache cargo builds
docs: add development instructions
```

Keep each commit focused. Use `!` and a `BREAKING CHANGE:` footer for incompatible changes.
