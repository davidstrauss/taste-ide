# The notebook kernel's bridge, run INSIDE an environment's container.
#
# The IDE starts this with `podman exec` (crate::kernel) and speaks JSON
# lines with it over stdin and stdout. It starts the notebook's kernel
# through jupyter_client, the Jupyter project's own client library (it
# ships with ipykernel), and relays the Jupyter messaging protocol: the
# kernel, its ZeroMQ sockets, and every byte of project code it runs stay
# in the container. Nothing here listens on a network socket the IDE
# reaches; the kernel's ports are on the container's own loopback.
#
# In (one JSON object per line):
#   {"op": "start", "kernel": "python3", "cwd": "/path"}
#   {"op": "execute", "cell": "<id>", "code": "..."}
#   {"op": "comm_msg", "comm_id": ..., "data": {...}, "buffers": [base64, ...]}
#   {"op": "interrupt"} | {"op": "restart"} | {"op": "shutdown"}
# Out:
#   {"ev": "ready", "kernel": ..., "display_name": ..., "language": ...}
#   {"ev": "failed", "message": ..., "kernels": [...]}
#   {"ev": "output", "cell": id | null, "parent": msg_id, "type": msg_type,
#    "content": {...}}
#   {"ev": "comm", "type": "comm_open" | "comm_msg" | "comm_close",
#    "content": {...}, "buffers": [base64, ...], "parent": msg_id}
#   {"ev": "reply", "cell": id, "content": {...}}
#   {"ev": "state", "state": "busy" | "idle" | "restarting" | "dead"}
#
# End of input shuts the kernel down: the IDE closing the page, or the
# IDE itself going, must not leave a kernel running in the container.
import base64
import json
import os
import sys
import threading

out_lock = threading.Lock()


def say(**message):
    line = json.dumps(message, default=str)
    with out_lock:
        sys.stdout.write(line + "\n")
        sys.stdout.flush()


try:
    from jupyter_client.kernelspec import KernelSpecManager, NoSuchKernel
    from jupyter_client.manager import KernelManager
except Exception as e:  # noqa: BLE001 - any import failure is the same answer
    say(ev="failed", message="jupyter_client is not importable here: %s" % e, kernels=[])
    sys.exit(0)

km = None
kc = None
cells = {}  # parent msg_id -> the IDE's cell id
ours = set()  # every msg_id this bridge sent, cells' and widgets' alike
cells_lock = threading.Lock()
# A ZeroMQ socket is not safe across threads: the shell socket is read by
# pump_shell and written by main, so each holds this while it does.
shell_lock = threading.Lock()
stopping = threading.Event()


def kernel_names():
    try:
        return sorted(KernelSpecManager().find_kernel_specs().keys())
    except Exception:  # noqa: BLE001
        return []


def start(name, cwd):
    global km, kc
    names = kernel_names()
    if not name:
        name = "python3" if "python3" in names or not names else names[0]
    try:
        km = KernelManager(kernel_name=name)
        spec = km.kernel_spec
        km.start_kernel(cwd=cwd or None)
        kc = km.client()
        kc.start_channels()
        kc.wait_for_ready(timeout=120)
    except NoSuchKernel:
        say(ev="failed", message="no kernel named %r in this environment" % name, kernels=names)
        return False
    except Exception as e:  # noqa: BLE001
        say(ev="failed", message="the kernel did not start: %s" % e, kernels=names)
        return False
    say(
        ev="ready",
        kernel=name,
        display_name=getattr(spec, "display_name", name),
        language=getattr(spec, "language", ""),
    )
    return True


def parent_of(message):
    return (message.get("parent_header") or {}).get("msg_id")


def cell_of(message):
    with cells_lock:
        return cells.get(parent_of(message))


def is_ours(message):
    with cells_lock:
        return parent_of(message) in ours


def encoded(buffers):
    return [base64.b64encode(bytes(b)).decode("ascii") for b in buffers or []]


def pump_iopub():
    while not stopping.is_set():
        try:
            message = kc.get_iopub_msg(timeout=0.5)
        except Exception:  # noqa: BLE001 - queue.Empty, or channels closing
            if km is not None and not km.is_alive() and not stopping.is_set():
                say(ev="state", state="dead")
                return
            continue
        kind = message.get("msg_type") or message["header"].get("msg_type")
        content = message.get("content") or {}
        if kind == "status":
            if is_ours(message) or content.get("execution_state") == "restarting":
                say(ev="state", state=content.get("execution_state"))
            continue
        # Widgets: their models open, change, and close on comms, whoever
        # set the change off — a cell, a slider's update, or the kernel
        # itself — so every comm message goes over.
        if kind in ("comm_open", "comm_msg", "comm_close"):
            say(
                ev="comm",
                type=kind,
                content=content,
                buffers=encoded(message.get("buffers")),
                parent=parent_of(message),
            )
            continue
        # An output goes over when this bridge set it off: a cell's, or a
        # widget callback's, which an Output widget may be capturing.
        if not is_ours(message):
            continue
        say(
            ev="output",
            cell=cell_of(message),
            parent=parent_of(message),
            type=kind,
            content=content,
        )


def pump_shell():
    while not stopping.is_set():
        try:
            with shell_lock:
                message = kc.get_shell_msg(timeout=0.05)
        except Exception:  # noqa: BLE001 - queue.Empty
            stopping.wait(0.02)
            continue
        if (message.get("msg_type") or message["header"].get("msg_type")) != "execute_reply":
            continue
        cell = cell_of(message)
        if cell is not None:
            say(ev="reply", cell=cell, content=message.get("content") or {})


def main():
    started = False
    for line in sys.stdin:
        try:
            request = json.loads(line)
        except ValueError:
            continue
        op = request.get("op")
        if op == "start" and not started:
            if not start(request.get("kernel"), request.get("cwd")):
                break
            started = True
            threading.Thread(target=pump_iopub, daemon=True).start()
            threading.Thread(target=pump_shell, daemon=True).start()
        elif not started:
            continue
        elif op == "execute":
            with shell_lock:
                msg_id = kc.execute(
                    request.get("code", ""), store_history=True, allow_stdin=False
                )
            with cells_lock:
                cells[msg_id] = request.get("cell")
                ours.add(msg_id)
        elif op == "comm_msg":
            # A widget's change, from the IDE's view of it, to its model in
            # the kernel; binary buffers ride along as bytes.
            message = kc.session.msg(
                "comm_msg",
                {"comm_id": request.get("comm_id"), "data": request.get("data") or {}},
            )
            buffers = [base64.b64decode(b) for b in request.get("buffers") or []]
            with cells_lock:
                ours.add(message["header"]["msg_id"])
            with shell_lock:
                kc.session.send(kc.shell_channel.socket, message, buffers=buffers)
        elif op == "interrupt":
            km.interrupt_kernel()
        elif op == "restart":
            say(ev="state", state="restarting")
            km.restart_kernel(now=False)
            kc.wait_for_ready(timeout=120)
            say(ev="state", state="idle")
        elif op == "shutdown":
            break
    stopping.set()
    if km is not None:
        try:
            kc.stop_channels()
        except Exception:  # noqa: BLE001
            pass
        try:
            km.shutdown_kernel(now=True)
        except Exception:  # noqa: BLE001
            pass


main()
# Straight out: the kernel is down, and jupyter_client's finalizers only
# print tracebacks about sockets already closed.
sys.stdout.flush()
os._exit(0)
