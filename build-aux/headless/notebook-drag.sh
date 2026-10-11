#!/usr/bin/env bash
# A notebook run and its interact slider dragged, in the real GUI against a
# real kernel — no VM needed.
#
# When the IDE runs inside a container, that container is the primary
# environment (the self-hosting case in taste_core::ExecContext), so a
# container with Jupyter and Task in it is all a kernel needs. This builds
# one — this repo's devcontainer image plus ipykernel, ipywidgets, Seaborn,
# pandas, and go-task — and runs the probe in it, against a scratch
# workspace whose Taskfile defines the `jupyter:kernel` task as an agent
# would (taste_core::kernel_task): TASTE_PROBE_NOTEBOOK=drag opens a copy of
# build-aux/headless/fixtures/widgets.ipynb, runs every cell — which starts
# the task and connects to its kernel — drags the
# first slider through its range at sixty changes a second, and prints
# what the widgets did (the updates the throttle let through, the echoes
# set aside, where the slider's model and the Output widget ended up).
#
# Run it ON THE HOST, from the workspace root, after the devcontainer image
# exists (`podman build -t taste-ide-devcontainer .devcontainer`):
#
#   build-aux/headless/notebook-drag.sh
#
# The shot lands in /tmp/notebook-drag.png.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"
IMAGE=taste-ide-notebook-test

podman build -q -t "$IMAGE" -f - . >/dev/null <<'EOF'
FROM localhost/taste-ide-devcontainer
USER root
RUN dnf install -y python3-ipykernel python3-ipywidgets python3-seaborn python3-pandas go-task && dnf clean all
USER dev
EOF

OUT="$(mktemp -d)"
trap 'rm -rf "$OUT"' EXIT
cat > "$OUT/run.sh" <<'EOF'
set -u
CARGO_BUILD_JOBS=8 cargo build -p taste-app 2>&1 | grep -E "^error" -A5 || true
export XDG_RUNTIME_DIR=/tmp/run; mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
rm -f /tmp/.X11-unix/X9; Xvfb :9 -screen 0 1440x900x24 >/tmp/xvfb.log 2>&1 &
while [ ! -e /tmp/.X11-unix/X9 ]; do sleep 0.1; done
# The workspace: the notebook, and the kernel's task.
WS=/tmp/notebook-ws
rm -rf "$WS"; mkdir -p "$WS"
cp build-aux/headless/fixtures/widgets.ipynb "$WS/"
cat > "$WS/Taskfile.yml" <<'YAML'
version: "3"
tasks:
  jupyter:kernel:
    desc: The Jupyter kernel the IDE's notebooks run in
    cmds:
      - python3 -m ipykernel_launcher -f /tmp/taste-ide-kernel.json
YAML
git -C "$WS" init -q && git -C "$WS" add -A && git -C "$WS" -c user.name=t -c user.email=t@t commit -qm ws
cd "$WS"
env DISPLAY=:9 GDK_BACKEND=x11 ADW_DEBUG_COLOR_SCHEME=prefer-dark TASTE_PROBE_CHECK=1 \
  TASTE_PROBE_DELAY_MS=45000 TASTE_PROBE_NOTEBOOK=drag TASTE_PROBE_PREVIEW=widgets.ipynb \
  timeout 200 dbus-run-session -- /workspaces/taste-ide/target/debug/taste-ide "$WS" > /out/run.log 2>&1
cp /tmp/probe-window.png /out/notebook-drag.png 2>/dev/null || true
grep "probe notebook" /out/run.log || { echo "no report; see the run's log:"; tail -30 /out/run.log; }
EOF
podman run --rm --userns=keep-id:uid=1000,gid=1000 --memory=16g --memory-swap=16g \
  -v "$ROOT:/workspaces/taste-ide:z" -v taste-ide-cargo:/home/dev/.cargo -v "$OUT:/out:z" \
  -w /workspaces/taste-ide "$IMAGE" bash /out/run.sh
cp "$OUT/notebook-drag.png" /tmp/notebook-drag.png 2>/dev/null && echo "shot: /tmp/notebook-drag.png"
