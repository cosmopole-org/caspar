//! The per-execution write-ahead buffer behind the raw `dbOp` host calls.
//!
//! Every in-process runtime (wasm, javascript, …) exposes the same four raw
//! link-level operations to its guest — `put` / `del` / `get` / `getByPrefix` —
//! and they must behave identically in all of them: reads see the VM's own
//! uncommitted writes first, writes accumulate locally, and the whole buffer
//! lands through one atomic `state_apply_ops` at commit.
//!
//! This lives in the SDK rather than in each plugin because two copies of a
//! read-your-own-writes overlay is two chances for them to drift, and a drift
//! here is a creature that reads a different database depending on which
//! runtime it was deployed to.

use std::collections::BTreeMap;

use crate::host::{host, log, KvOp};

/// One buffered raw operation.
#[derive(Clone, Debug)]
pub struct DbOp {
    /// `"put"` or `"del"`.
    pub type_: String,
    pub key: String,
    pub val: String,
}

/// Per-execution write-ahead buffer for raw link-level db ops.
///
/// Reads are served from the local overlay first (read-your-own-writes) and
/// fall through to node state via the SDK host interface; writes accumulate
/// locally and are committed atomically through [`crate::host::VmHost::state_apply_ops`].
#[derive(Default)]
pub struct Trx {
    pub store: BTreeMap<String, String>,
    pub newly_created: BTreeMap<String, bool>,
    pub newly_deleted: BTreeMap<String, bool>,
    pub ops: Vec<DbOp>,
}

impl Trx {
    pub fn new() -> Self {
        Trx {
            store: BTreeMap::new(),
            newly_created: BTreeMap::new(),
            newly_deleted: BTreeMap::new(),
            ops: Vec::new(),
        }
    }

    pub fn put(&mut self, key: String, val: String) {
        self.ops.push(DbOp {
            type_: "put".to_string(),
            key: key.clone(),
            val: val.clone(),
        });
        self.store.insert(key.clone(), val);
        self.newly_created.insert(key.clone(), true);
        self.newly_deleted.remove(&key);
    }

    pub fn get_by_prefix(&mut self, prefix: String) -> Vec<String> {
        // First collect any locally-buffered values matching the prefix.
        let mut result: Vec<String> = self
            .store
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v.clone())
            .collect();

        let fetched = match host() {
            Some(h) => h.state_get_by_prefix(&prefix),
            None => Vec::new(),
        };
        for v in fetched {
            if !result.contains(&v) {
                result.push(v);
            }
        }
        result
    }

    pub fn get(&mut self, key: String) -> String {
        if let Some(value) = self.store.get(&key) {
            return value.clone();
        }
        if self.newly_deleted.contains_key(&key) {
            return "".to_string();
        }
        let value = match host() {
            Some(h) => h.state_get(&key),
            None => String::new(),
        };
        self.store.insert(key, value.clone());
        value
    }

    pub fn del(&mut self, key: String) {
        let k = key;
        self.ops.push(DbOp {
            type_: "del".to_string(),
            key: k.clone(),
            val: String::new(),
        });
        self.store.remove(&k);
        self.newly_created.remove(&k);
        self.newly_deleted.insert(k, true);
    }

    /// Commit all buffered ops atomically through the node's consensus-aware
    /// state layer.
    pub fn commit_as_offchain(&mut self) {
        if self.ops.is_empty() {
            return;
        }
        let ops: Vec<KvOp> = self
            .ops
            .iter()
            .map(|o| KvOp {
                op: o.type_.clone(),
                key: o.key.clone(),
                val: o.val.clone(),
            })
            .collect();
        if let Some(h) = host() {
            if let Err(e) = h.state_apply_ops(&ops) {
                log(format!("vm trx commit failed: {}", e));
                return;
            }
            self.ops.clear();
            log("committed transaction successfully.".to_string());
        }
    }
}
