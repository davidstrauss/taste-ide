# Flatpak packaging

The normal path is the IDE's header-bar Flatpak button (build → install →
launch, streaming into the Flatpak console tab). This file covers the
internals and the manual fallback.

## Files

- **`net.davidstrauss.Taste.json`** — the flatpak-builder manifest. The vte
  module is pinned by tarball sha256 (the GNOME runtime doesn't ship libvte
  for apps); bump the URL + hash together when updating.
- **`cargo-sources.json`** — generated from `Cargo.lock`, committed so
  builds are reproducible offline. Regenerate whenever `Cargo.lock`
  changes — in the devcontainer, since the host has no Python deps:

  ```sh
  curl -sfLO https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
  podman run --rm -v "$PWD:/workspaces/taste-ide:Z" --user root taste-ide-devcontainer \
    bash -c 'dnf install -y -q python3-aiohttp python3-tomlkit >/dev/null &&
             cd /workspaces/taste-ide &&
             python3 flatpak-cargo-generator.py Cargo.lock \
               -o build-aux/flatpak/cargo-sources.json'
  rm flatpak-cargo-generator.py
  ```

- `build/`, `repo/`, `.flatpak-builder/` — build artifacts, git-ignored.

## Manual build (what the IDE button runs)

```sh
flatpak install flathub org.flatpak.Builder   # one time

flatpak run org.flatpak.Builder \
  --force-clean --user --install --install-deps-from=flathub \
  --state-dir=build-aux/flatpak/.flatpak-builder \
  build-aux/flatpak/build \
  build-aux/flatpak/net.davidstrauss.Taste.json

flatpak run net.davidstrauss.Taste
```

## Sandbox notes (see docs/ARCHITECTURE.md → Flatpak)

- `--talk-name=org.freedesktop.Flatpak` is required: podman, agent
  subprocesses, and `git push` all run on the host via `flatpak-spawn --host`.
- `--filesystem=home` is a stopgap until workspace selection goes through
  the file portal.
