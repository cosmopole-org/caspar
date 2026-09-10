// A complete javascript creature, deployable as-is.
//
//   caspar-client vm.init javascript ./counter
//   caspar-client programs.deploy <programId> ./counter javascript '{}'
//
// One self-contained script: a creature entity carries no imports, because
// `acceptsExtraFiles` is false and there is no loader inside the VM. Bundle
// anything larger to a single file (esbuild `--format=iife --bundle`).

/** The document this creature keeps for one counter. */
function key(id) {
  return "Json::Counter::" + id;
}

/**
 * Every signal to this program lands here. `inputJson` is the packet the node
 * delivered; the return value becomes the VM's output.
 *
 * Reads and writes go through the per-VM JSON transaction (`getJson`/`putJson`),
 * so everything this run touches commits atomically when it ends — or not at
 * all, if it throws.
 */
globalThis.update = function (inputJson) {
  var packet = {};
  try {
    packet = JSON.parse(inputJson || "{}");
  } catch (e) {
    return { ok: false, error: "the signal payload is not JSON" };
  }

  // A signal's own payload is a JSON string inside `data`.
  var payload = {};
  try {
    payload = typeof packet.data === "string" ? JSON.parse(packet.data) : packet.data || {};
  } catch (e) {
    payload = {};
  }

  var id = String(payload.id || "default");
  var by = Number(payload.by || 1);
  if (!isFinite(by)) {
    return { ok: false, error: "`by` must be a number" };
  }

  var current = hostCall("getJson", { key: key(id), path: "doc" });
  var n = Number((current.data && current.data.n) || 0) + by;

  hostCall("putJson", {
    key: key(id),
    path: "doc",
    data: { n: n, updatedAt: Date.now(), by: caspar.programId },
  });

  console.log("counter", id, "->", n);

  return { ok: true, id: id, n: n };
};
