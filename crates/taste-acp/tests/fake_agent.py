#!/usr/bin/env python3
"""A minimal scripted ACP agent for integration-testing taste-ide's client.

Speaks newline-delimited JSON-RPC 2.0 on stdio, protocol v1. Covers the
session lifecycle the IDE drives: initialize -> session/new -> session/prompt
(streams one message chunk, then ends the turn) and session/set_mode.

session/load replays one history message and then answers, covering the
restore path the IDE uses after any restart that ended the agent process;
the id "expired-session" fails instead, for the fallback.

A prompt of the form "/read <path>" makes the agent do what a real agent
does with a file: ask the CLIENT for it (fs/read_text_file) and stream back
what came, so the client's side of that exchange is covered end to end.
"/write <path> <text>" is the same for fs/write_text_file, streaming back
"OK" or the client's refusal.

Four more verbs exist for the relocation test, and each answers differently
depending on WHERE the agent process was started: "/env NAME" reports an
environment variable, "/exists PATH" whether a path is visible, "/get URL"
makes a real HTTP request the way the adapter would — at
ANTHROPIC_BASE_URL, presenting ANTHROPIC_AUTH_TOKEN — and "/mcp TOOL" calls
one of the IDE's own MCP tools by spawning the stdio bridge the client
registered in session/new, exactly as a real adapter does. That last one is
the only way to prove the IDE's tools actually reach a relocated agent:
everything else about MCP can look fine while the bridge dials a socket
nothing answers on.

"/term COMMAND" drives the CLIENT-SERVED terminal extension the way the
pinned Claude adapter does when the client advertises it: terminal/create,
terminal/wait_for_exit, terminal/output, terminal/release, reporting the
exit status and what the command printed. "/termcap" reports whether the
client advertised the capability at all, which is what makes the
container-mode/safe-mode split testable from the agent's side.

"/termhold COMMAND" creates a terminal and returns its id WITHOUT waiting,
so a test can watch a long-running command while it runs — the case the
console's Kill button exists for. "/termstatus ID" and "/termrelease ID"
finish the job afterwards.
"""
import json
import os
import subprocess
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def respond(req_id, result):
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


next_request_id = [100]

# What the client said it can do, from initialize. A real adapter uses
# client terminals only when they are advertised, and so does this.
client_capabilities = {}

# The MCP servers the client registered for this session. A real adapter
# spawns these and speaks newline-delimited JSON-RPC over their stdio; so
# does /mcp below.
mcp_servers = []


def call_mcp(tool):
    """Spawn the registered MCP bridge and call one tool through it.

    Deliberately the whole handshake — initialize, then tools/call — over a
    freshly spawned bridge, because that is what a real adapter does and
    because the failure being tested for (a bridge that connects to nothing)
    only shows up when something waits for an answer.
    """
    if not mcp_servers:
        return "mcp ERROR: the client registered no MCP server"
    server = mcp_servers[0]
    argv = [server["command"]] + list(server.get("args", []))
    try:
        proc = subprocess.Popen(
            argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, text=True,
        )
    except Exception as e:  # noqa: BLE001 - the message IS the result
        return "mcp ERROR: cannot spawn the bridge: %s" % e

    def rpc(request_id, method_name, params):
        proc.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": request_id,
            "method": method_name, "params": params,
        }) + "\n")
        proc.stdin.flush()
        while True:
            reply = proc.stdout.readline()
            if not reply:
                return None
            reply = json.loads(reply)
            if reply.get("id") == request_id:
                return reply

    try:
        if rpc(1, "initialize", {"protocolVersion": "2024-11-05"}) is None:
            return "mcp ERROR: the bridge closed before initialize: %s" % (
                proc.stderr.read(200),
            )
        reply = rpc(2, "tools/call", {"name": tool, "arguments": {}})
        if reply is None:
            return "mcp ERROR: no answer to %s: %s" % (tool, proc.stderr.read(200))
        if "error" in reply:
            return "mcp ERROR: " + json.dumps(reply["error"])
        content = reply.get("result", {}).get("content", [])
        text = "".join(c.get("text", "") for c in content)
        return "mcp " + text
    finally:
        proc.kill()


def call_client(method, params):
    """Send a request to the client and wait for its response."""
    request_id = next_request_id[0]
    next_request_id[0] += 1
    send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
    while True:
        line = sys.stdin.readline()
        if not line:
            sys.exit(0)  # client went away mid-request
        reply = json.loads(line) if line.strip() else None
        if reply and reply.get("id") == request_id and "method" not in reply:
            if "error" in reply:
                # Whole object: the useful detail is in `data`, not the
                # generic JSON-RPC message.
                return {"error": json.dumps(reply["error"])}
            return reply.get("result", {})


while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")

    if method == "session/new" or method == "session/load":
        # Remember the bridge the client registered, so /mcp can use it the
        # way a real adapter would.
        mcp_servers[:] = msg.get("params", {}).get("mcpServers", [])

    if method == "initialize":
        client_capabilities.update(msg.get("params", {}).get("clientCapabilities", {}))
        respond(req_id, {
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": True},
            "authMethods": [],
        })
    elif method == "session/new":
        respond(req_id, {
            "sessionId": "sess-1",
            "modes": {
                "currentModeId": "normal",
                "availableModes": [
                    {"id": "normal", "name": "Normal"},
                    {"id": "yolo", "name": "Yolo"},
                ],
            },
        })
    elif method == "session/prompt":
        session_id = msg["params"]["sessionId"]
        blocks = msg["params"].get("prompt", [])
        prompt = next(
            (b.get("text", "") for b in blocks if b.get("type") == "text"), ""
        )

        def reply(text, _session=session_id, _req=req_id):
            """Stream one chunk and end the turn — the whole of an answer."""
            notify("session/update", {
                "sessionId": _session,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                },
            })
            respond(_req, {"stopReason": "end_turn"})

        if prompt.startswith("/write "):
            path, _, text = prompt[len("/write "):].partition(" ")
            result = call_client("fs/write_text_file", {
                "sessionId": session_id,
                "path": path,
                "content": text,
            })
            notify("session/update", {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": "ERROR: " + str(result["error"])
                        if "error" in result else "OK",
                    },
                },
            })
            respond(req_id, {"stopReason": "end_turn"})
            continue
        # The relocation verbs: what the agent can see of the world it was
        # started in. A relocated agent runs inside its environment's
        # container, and every one of these answers differently there.
        if prompt.startswith("/env "):
            reply("env " + os.environ.get(prompt[len("/env "):], "<unset>"))
            continue
        if prompt.startswith("/exists "):
            path = prompt[len("/exists "):]
            reply("exists " + ("yes" if os.path.exists(path) else "no"))
            continue
        if prompt.startswith("/mcp "):
            reply(call_mcp(prompt[len("/mcp "):].strip()))
            continue
        if prompt == "/termcap":
            # Exactly the check a real adapter makes before it prefers a
            # client terminal over spawning something itself.
            reply("termcap " + ("yes" if client_capabilities.get("terminal") else "no"))
            continue
        if prompt.startswith("/termhold "):
            argv = prompt[len("/termhold "):].split(" ")
            created = call_client("terminal/create", {
                "sessionId": session_id,
                "command": argv[0],
                "args": argv[1:],
            })
            if "error" in created:
                reply("term ERROR " + str(created["error"]))
                continue
            reply("termhold " + created["terminalId"])
            continue
        if prompt.startswith("/termstatus "):
            got = call_client("terminal/output", {
                "sessionId": session_id,
                "terminalId": prompt[len("/termstatus "):].strip(),
            })
            if "error" in got:
                reply("term ERROR " + str(got["error"]))
                continue
            status = got.get("exitStatus")
            reply("termstatus %s %s" % (
                "running" if status is None else json.dumps(status),
                got.get("output", "").replace("\n", " ").strip(),
            ))
            continue
        if prompt.startswith("/termrelease "):
            released = call_client("terminal/release", {
                "sessionId": session_id,
                "terminalId": prompt[len("/termrelease "):].strip(),
            })
            reply("termrelease " + ("ERROR " + str(released["error"])
                                    if "error" in released else "ok"))
            continue
        if prompt.startswith("/term "):
            argv = prompt[len("/term "):].split(" ")
            created = call_client("terminal/create", {
                "sessionId": session_id,
                "command": argv[0],
                "args": argv[1:],
            })
            if "error" in created:
                reply("term ERROR " + str(created["error"]))
                continue
            terminal_id = created["terminalId"]
            # Exactly the adapter's order: park on the exit, then read.
            exit_status = call_client("terminal/wait_for_exit", {
                "sessionId": session_id,
                "terminalId": terminal_id,
            })
            got = call_client("terminal/output", {
                "sessionId": session_id,
                "terminalId": terminal_id,
            })
            call_client("terminal/release", {
                "sessionId": session_id,
                "terminalId": terminal_id,
            })
            reply("term %s %s %s" % (
                terminal_id,
                json.dumps(exit_status.get("exitStatus", exit_status)),
                got.get("output", "").replace("\n", " ").strip(),
            ))
            continue
        if prompt.startswith("/get "):
            # A real API call, made the way the adapter makes one: at
            # ANTHROPIC_BASE_URL, presenting ANTHROPIC_AUTH_TOKEN. Inside a
            # container that only works if something forwarded the IDE's
            # proxy in there.
            import urllib.request
            import urllib.error
            request = urllib.request.Request(
                prompt[len("/get "):],
                headers={
                    "authorization":
                        "Bearer " + os.environ.get("ANTHROPIC_AUTH_TOKEN", ""),
                },
            )
            try:
                with urllib.request.urlopen(request, timeout=20) as response:
                    reply("get %d %s" % (
                        response.status,
                        response.read(200).decode("utf-8", "replace"),
                    ))
            except urllib.error.HTTPError as e:
                reply("get %d %s" % (e.code, e.read(200).decode("utf-8", "replace")))
            except Exception as e:  # noqa: BLE001 - the message IS the result
                reply("get ERROR " + type(e).__name__ + ": " + str(e))
            continue
        if prompt.startswith("/read "):
            result = call_client("fs/read_text_file", {
                "sessionId": session_id,
                "path": prompt[len("/read "):],
            })
            notify("session/update", {
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {
                        "type": "text",
                        "text": result.get("content", "ERROR: " + str(result.get("error"))),
                    },
                },
            })
            respond(req_id, {"stopReason": "end_turn"})
            continue
        notify("session/update", {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "hello from fake agent"},
            },
        })
        respond(req_id, {"stopReason": "end_turn"})
    elif method == "session/load":
        session_id = msg["params"]["sessionId"]
        if session_id == "expired-session":
            send({"jsonrpc": "2.0", "id": req_id,
                  "error": {"code": -32602, "message": "no such session"}})
            continue
        # Replay history as ordinary updates BEFORE responding, the way a
        # real adapter does (getOrCreateSession, then replaySessionHistory):
        # the client renders them as the conversation coming back.
        notify("session/update", {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "replayed: earlier answer"},
            },
        })
        respond(req_id, {
            "modes": {
                "currentModeId": "normal",
                "availableModes": [
                    {"id": "normal", "name": "Normal"},
                    {"id": "yolo", "name": "Yolo"},
                ],
            },
        })
    elif method == "session/set_mode":
        respond(req_id, {})
    elif req_id is not None:
        send({"jsonrpc": "2.0", "id": req_id,
              "error": {"code": -32601, "message": f"unhandled: {method}"}})
