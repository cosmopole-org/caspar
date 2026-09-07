#!/usr/bin/env python3
"""Slice Modal's api.proto down to the RPCs the Caspar modal VM plugin uses.

The full proto is ~5k lines of Modal's entire control plane. Protobuf is
wire-compatible field-by-field, so a subset carrying the same field numbers
and types talks to the same server while keeping the generated Rust (and the
blast radius of an upstream change) small.
"""
import re, sys, collections

SRC = sys.argv[1]
OUT = sys.argv[2]

src = open(SRC).read()

def top_level_blocks(src):
    """Yield (kind, name, body_text) for each top-level message/enum."""
    out = {}
    for m in re.finditer(r'^(message|enum)\s+(\w+)\s*\{', src, re.M):
        kind, name = m.group(1), m.group(2)
        i = m.start(); depth = 0; j = m.end() - 1
        while True:
            c = src[j]
            if c == '{': depth += 1
            elif c == '}':
                depth -= 1
                if depth == 0: break
            j += 1
        out[name] = (kind, src[i:j+1])
    return out

blocks = top_level_blocks(src)

IDENT = re.compile(r'\b([A-Z][A-Za-z0-9_]*)\b')

def deps(name):
    kind, body = blocks[name]
    found = set()
    # strip comments and string literals so prose does not pull in types
    body = re.sub(r'//[^\n]*', '', body)
    body = re.sub(r'"[^"]*"', '""', body)
    for ident in IDENT.findall(body):
        if ident != name and ident in blocks:
            found.add(ident)
    return found

SEEDS = [
    # app + image + volume
    "AppGetOrCreateRequest", "AppGetOrCreateResponse",
    "AppStopRequest",
    "ImageGetOrCreateRequest", "ImageGetOrCreateResponse",
    "ImageJoinStreamingRequest", "ImageJoinStreamingResponse",
    "VolumeGetOrCreateRequest", "VolumeGetOrCreateResponse",
    "VolumeDeleteRequest",
    # sandbox lifecycle
    "SandboxCreateRequest", "SandboxCreateResponse",
    "SandboxWaitRequest", "SandboxWaitResponse",
    "SandboxWaitUntilReadyRequest", "SandboxWaitUntilReadyResponse",
    "SandboxTerminateRequest", "SandboxTerminateResponse",
    "SandboxGetTaskIdRequest", "SandboxGetTaskIdResponse",
    "SandboxListRequest", "SandboxListResponse",
    "SandboxTagsSetRequest",
    "SandboxGetTunnelsRequest", "SandboxGetTunnelsResponse",
    "SandboxCreateConnectTokenRequest", "SandboxCreateConnectTokenResponse",
    "SandboxSnapshotFsRequest", "SandboxSnapshotFsResponse",
    "SandboxRestoreRequest", "SandboxRestoreResponse",
    "SandboxGetLogsRequest",
    # exec + filesystem
    "ContainerExecRequest", "ContainerExecResponse",
    "ContainerExecGetOutputRequest", "RuntimeOutputBatch",
    "ContainerExecWaitRequest", "ContainerExecWaitResponse",
    "ContainerFilesystemExecRequest", "ContainerFilesystemExecResponse",
    "ContainerFilesystemExecGetOutputRequest", "FilesystemRuntimeOutputBatch",
    "TaskLogsBatch",
]

missing = [s for s in SEEDS if s not in blocks]
if missing:
    print("MISSING SEEDS:", missing, file=sys.stderr)
    sys.exit(1)

closure = set()
queue = collections.deque(SEEDS)
while queue:
    n = queue.popleft()
    if n in closure: continue
    closure.add(n)
    for d in deps(n):
        if d not in closure:
            queue.append(d)

RPCS = """  rpc AppGetOrCreate(AppGetOrCreateRequest) returns (AppGetOrCreateResponse);
  rpc AppStop(AppStopRequest) returns (google.protobuf.Empty);
  rpc ImageGetOrCreate(ImageGetOrCreateRequest) returns (ImageGetOrCreateResponse);
  rpc ImageJoinStreaming(ImageJoinStreamingRequest) returns (stream ImageJoinStreamingResponse);
  rpc VolumeGetOrCreate(VolumeGetOrCreateRequest) returns (VolumeGetOrCreateResponse);
  rpc VolumeDelete(VolumeDeleteRequest) returns (google.protobuf.Empty);
  rpc SandboxCreate(SandboxCreateRequest) returns (SandboxCreateResponse);
  rpc SandboxWait(SandboxWaitRequest) returns (SandboxWaitResponse);
  rpc SandboxWaitUntilReady(SandboxWaitUntilReadyRequest) returns (SandboxWaitUntilReadyResponse);
  rpc SandboxTerminate(SandboxTerminateRequest) returns (SandboxTerminateResponse);
  rpc SandboxGetTaskId(SandboxGetTaskIdRequest) returns (SandboxGetTaskIdResponse);
  rpc SandboxList(SandboxListRequest) returns (SandboxListResponse);
  rpc SandboxTagsSet(SandboxTagsSetRequest) returns (google.protobuf.Empty);
  rpc SandboxGetTunnels(SandboxGetTunnelsRequest) returns (SandboxGetTunnelsResponse);
  rpc SandboxCreateConnectToken(SandboxCreateConnectTokenRequest) returns (SandboxCreateConnectTokenResponse);
  rpc SandboxSnapshotFs(SandboxSnapshotFsRequest) returns (SandboxSnapshotFsResponse);
  rpc SandboxRestore(SandboxRestoreRequest) returns (SandboxRestoreResponse);
  rpc SandboxGetLogs(SandboxGetLogsRequest) returns (stream TaskLogsBatch);
  rpc ContainerExec(ContainerExecRequest) returns (ContainerExecResponse);
  rpc ContainerExecGetOutput(ContainerExecGetOutputRequest) returns (stream RuntimeOutputBatch);
  rpc ContainerExecWait(ContainerExecWaitRequest) returns (ContainerExecWaitResponse);
  rpc ContainerFilesystemExec(ContainerFilesystemExecRequest) returns (ContainerFilesystemExecResponse);
  rpc ContainerFilesystemExecGetOutput(ContainerFilesystemExecGetOutputRequest) returns (stream FilesystemRuntimeOutputBatch);
"""

header = '''// GENERATED SLICE — do not hand-edit.
//
// Produced by scripts/slice_modal_proto.py from modal-labs/modal-client's
// modal_proto/api.proto. It carries only the messages and RPCs the Caspar
// modal VM plugin calls, with upstream field numbers preserved, so it is wire
// compatible with Modal's server while keeping the generated Rust small.
//
// Modal states that direct gRPC use carries no compatibility guarantee; when
// an upstream change breaks a call, re-run the slicer against the new proto.

syntax = "proto3";

package modal.client;

import "google/protobuf/empty.proto";
import "google/protobuf/struct.proto";
import "google/protobuf/timestamp.proto";
import "google/protobuf/wrappers.proto";
import "google/protobuf/any.proto";

'''

parts = [header]
for name in sorted(closure):
    parts.append(blocks[name][1])
    parts.append("\n\n")
parts.append("service ModalClient {\n")
parts.append(RPCS)
parts.append("}\n")

open(OUT, "w").write("".join(parts))
print("sliced %d definitions -> %s" % (len(closure), OUT))
