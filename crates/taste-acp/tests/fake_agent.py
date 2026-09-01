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

Three more verbs exist for the relocation test, and each answers
differently depending on WHERE the agent process was started: "/env NAME"
reports an environment variable, "/exists PATH" whether a path is visible,
and "/get URL" makes a real HTTP request the way the adapter would — at
ANTHROPIC_BASE_URL, presenting ANTHROPIC_AUTH_TOKEN.
"""
import json
import os
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def respond(req_id, result):
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


next_request_id = [100]


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

    if method == "initialize":
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
