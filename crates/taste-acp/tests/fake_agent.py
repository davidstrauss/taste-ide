#!/usr/bin/env python3
"""A minimal scripted ACP agent for integration-testing taste-ide's client.

Speaks newline-delimited JSON-RPC 2.0 on stdio, protocol v1. Covers the
session lifecycle the IDE drives: initialize -> session/new -> session/prompt
(streams one message chunk, then ends the turn) and session/set_mode.
"""
import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def respond(req_id, result):
    send({"jsonrpc": "2.0", "id": req_id, "result": result})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")

    if method == "initialize":
        respond(req_id, {
            "protocolVersion": 1,
            "agentCapabilities": {},
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
        notify("session/update", {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "hello from fake agent"},
            },
        })
        respond(req_id, {"stopReason": "end_turn"})
    elif method == "session/set_mode":
        respond(req_id, {})
    elif req_id is not None:
        send({"jsonrpc": "2.0", "id": req_id,
              "error": {"code": -32601, "message": f"unhandled: {method}"}})
