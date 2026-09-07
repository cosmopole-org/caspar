//! Multi-instance integration test of the geo-distributed cluster.
//!
//! Boots **three real `ClusterService` instances inside one process** — each
//! with its own RocksDB, raft log, stub node core, and HTTP listener on
//! 127.0.0.1 — then drives the actual product surface:
//!
//! 1. cluster formation through the orchestration API (`/cluster/add-peer`),
//! 2. shell-style write-set replication through the real `TrxWrapper` commit
//!    hook (instance 1 is the process-global service, exactly like production),
//! 3. local-scope suppression (non-distributed VM commits stay local),
//! 4. distributed creature deployment (`Deploy` artifact → files + links +
//!    VMM assign materialized on the other instances),
//! 5. config get/set + status/ping/nearest endpoints.
//!
//! Everything runs sequentially inside one `#[test]` because the process can
//! host only one *global* cluster handle (the `CLUSTER` OnceCell), mirroring
//! a production instance.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Value};

use crate::core::actor::model::trx::TrxWrapper;
use crate::models::core::ICore;
use crate::models::ports::storage::IStorage;
use crate::models::ports::tools::ITools;
use crate::models::transaction::ITrx;

use super::command::DeployArtifact;
use super::config::ClusterConfig;

// ────────────────────────── stub node core ──────────────────────────────────

struct StubStorage {
    root: String,
    kv: crate::models::ports::storage::KvDb,
    ts: crate::models::ports::storage::TsDb,
}

impl StubStorage {
    fn new(dir: &str) -> Arc<Self> {
        std::fs::create_dir_all(dir).unwrap();
        let kv: crate::models::ports::storage::KvDb =
            Arc::new(rocksdb::TransactionDB::open_default(dir).expect("stub kvdb"));
        let manager = r2d2_postgres::PostgresConnectionManager::new(
            "host=127.0.0.1 port=1 user=x dbname=x".parse().unwrap(),
            postgres::tls::NoTls,
        );
        let ts = r2d2::Builder::new().build_unchecked(manager);
        Arc::new(StubStorage {
            root: dir.to_string(),
            kv,
            ts,
        })
    }
}

impl IStorage for StubStorage {
    fn storage_root(&self) -> String {
        self.root.clone()
    }
    fn kv_db(&self) -> crate::models::ports::storage::KvDb {
        self.kv.clone()
    }
    fn ts_db(&self) -> crate::models::ports::storage::TsDb {
        self.ts.clone()
    }
    fn gen_id(&self, _t: &dyn ITrx, _: &str) -> String {
        String::new()
    }
    fn log_time_sieries(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &[String],
        _: i64,
    ) -> anyhow::Result<crate::models::packet::LogPacket> {
        Ok(Default::default())
    }
    fn update_log(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &str,
        _: i64,
    ) -> crate::models::packet::LogPacket {
        Default::default()
    }
    fn read_store_logs(
        &self,
        _: &str,
        _: &crate::models::packet::LogQuery,
    ) -> anyhow::Result<Vec<crate::models::packet::LogPacket>> {
        Ok(Vec::new())
    }
    fn pick_store_logs(&self, _: &str, _: Vec<String>) -> Vec<crate::models::packet::LogPacket> {
        Vec::new()
    }
    fn log_vm(&self, _: &str, _: &str, _: &str, _: i64) -> crate::models::packet::BuildPacket {
        Default::default()
    }
    fn read_vm_logs(
        &self,
        _: &str,
        _: &str,
        _: i64,
        _: i64,
    ) -> Vec<crate::models::packet::BuildPacket> {
        Vec::new()
    }
}

/// Filesystem-backed stub of the parts of `IFile` the cluster applier uses.
struct StubFile;

impl crate::models::ports::file::IFile for StubFile {
    fn check_file_from_storage(&self, _: &str, _: &str, _: &str) -> bool {
        false
    }
    fn save_file_to_storage(
        &self,
        _: &str,
        _: &crate::compat::multipart::FileHeader,
        _: &str,
        _: &str,
    ) -> Result<()> {
        unimplemented!()
    }
    fn save_data_to_storage(&self, _: &str, _: &[u8], _: &str, _: &str, _: &[bool]) -> Result<()> {
        unimplemented!()
    }
    fn save_tar_file_item_to_storage(
        &self,
        _: &str,
        _: &mut dyn std::io::Read,
        _: &str,
        _: &str,
    ) -> Result<()> {
        unimplemented!()
    }
    fn read_file_from_storage(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>> {
        unimplemented!()
    }
    fn check_file_from_global_storage(&self, _: &str, _: &str) -> bool {
        false
    }
    fn read_file_from_global_storage(&self, _: &str, _: &str) -> Result<String> {
        unimplemented!()
    }
    fn save_file_to_global_storage(
        &self,
        _: &str,
        _: &crate::compat::multipart::FileHeader,
        _: &str,
        _: bool,
    ) -> Result<()> {
        unimplemented!()
    }
    // The one call the deploy applier makes: `storage_root` here is the
    // target folder (same contract as the production driver's deploy path).
    fn save_data_to_global_storage(
        &self,
        folder: &str,
        data: &[u8],
        key: &str,
        _overwrite: bool,
    ) -> Result<()> {
        std::fs::create_dir_all(folder)?;
        std::fs::write(PathBuf::from(folder).join(key), data)?;
        Ok(())
    }
    fn delete_file_from_global_storage(&self, _: &str, _: &str, _: bool) -> Result<()> {
        unimplemented!()
    }
    fn read_file_by_path(&self, path: &str) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        std::fs::File::open(path)?.read_to_end(&mut buf)?;
        Ok(buf)
    }
}

/// Records `assign` / `build_vm_image` calls; everything else is unreachable
/// from the cluster paths under test.
#[derive(Default)]
struct StubVmm {
    assigned: Mutex<Vec<String>>,
    builds: Mutex<Vec<(String, String, String)>>,
}

impl crate::models::ports::vmm::IVmm for StubVmm {
    fn assign(&self, machine_id: &str) {
        self.assigned.lock().unwrap().push(machine_id.to_string());
    }
    fn run_vm(&self, _: &str, _: &str, _: &str) {}
    fn terminate_vm(&self, _: &str) {}
    fn build_vm_image(&self, machine_id: &str, entity_id: &str, _build_path: &str, build_type: &str) {
        self.builds.lock().unwrap().push((
            machine_id.to_string(),
            entity_id.to_string(),
            build_type.to_string(),
        ));
    }
    fn execute_chain_trxs_group(&self, _: Vec<crate::models::worker::Trx>) {}
    fn execute_chain_effects(&self, _: &str) {}
    fn close_kvdb(&self) {}
    fn vm_callback(&self, _: &str) -> (String, i64) {
        (String::new(), 0)
    }
    fn start_docker_gateway(&self, _: i64) {}
    fn start_http_ingress(&self, _: i64) {}
    fn register_vm_container(&self, _: &str, _: &str, _: &str, _: &str, _: &str, _: &str) {}
    fn unregister_vm_container(&self, _: &str) {}
    fn identify_container_by_ip(&self, _: &str) -> Option<(String, String, String, String, String)> {
        None
    }
    fn push_signal_to_machine(&self, _: &str, _: &str, _: &Value) -> usize {
        0
    }
    fn push_signal_to_entity(&self, _: &str, _: &str, _: &str, _: &Value) -> usize {
        0
    }
    fn queue_pending_signal(&self, _: &str, _: &str, _: &str, _: &Value) {}
    fn begin_cold_spawn(&self, _: &str, _: &str) -> bool {
        true
    }
    fn register_vm_context(&self, _: &str, _: &str, _: &str) {}
    fn unregister_vm_context(&self, _: &str) {}
    fn get_vm_context(&self, _: &str) -> Option<(String, String)> {
        None
    }
    fn begin_vm_trx(&self, _: &str) {}
    fn commit_vm_trx(&self, _: &str) {}
    fn vm_db_op(&self, _: &str, _: &str, _: &str, _: &str, _: &str) -> Result<String, String> {
        Err("stub".into())
    }
    fn vm_db_commit_explicit(&self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn acquire_resource_lock(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn release_resource_lock(&self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn host_action_micro(&self, _: &str, _: &Value, _: i64) -> (String, i64) {
        (String::new(), 0)
    }
    fn exec_shell_action(&self, _: &str, _: &Value) -> String {
        String::new()
    }
    fn host_action_resource_store(&self, _: &str, _: &Value, _: i64) -> (String, i64) {
        (String::new(), 0)
    }
    fn host_action_resource_entity_create(&self, _: &Value, _: i64) -> (String, i64) {
        (String::new(), 0)
    }
    fn host_action_resource_entity_delete(&self, _: &Value, _: i64) -> (String, i64) {
        (String::new(), 0)
    }
    fn host_action_store(&self, _: &str, _: &Value, _: i64) -> (String, i64) {
        (String::new(), 0)
    }
    fn host_action_creature(&self, _: &str, _: &Value, _: i64) -> (String, i64) {
        (String::new(), 0)
    }
    fn host_action_program(&self, _: &str, _: &Value, _: i64) -> (String, i64) {
        (String::new(), 0)
    }
    fn supported_runtimes(&self) -> Vec<String> {
        vec!["docker".into()]
    }
    fn is_supported_runtime(&self, _: &str) -> bool {
        true
    }
    fn is_managed_runtime(&self, _: &str) -> bool {
        false
    }
    fn runtime_supports_chain_trxs(&self, _: &str) -> bool {
        false
    }
    fn runtime_deploy_spec(&self, _: &str) -> Option<Value> {
        None
    }
    fn plan_run_entity(&self, _: &str, _: &Value) -> Result<Value, String> {
        Err("stub".into())
    }
    fn plan_stop_entity(&self, _: &str, _: &Value) -> Result<Value, String> {
        Err("stub".into())
    }
    fn plan_delete_entity(&self, _: &str, _: &Value) -> Result<Value, String> {
        Err("stub".into())
    }
    fn delete_vm_instance(&self, _: &Value) -> Value {
        Value::Null
    }
    fn forward_http(&self, _: &Value) -> Value {
        Value::Null
    }
    fn resolve_http_route(&self, _: &str, _: &str) -> Option<Value> {
        None
    }
}

struct StubTools {
    storage: Arc<dyn IStorage>,
    file: Arc<dyn crate::models::ports::file::IFile>,
    vmm: Arc<StubVmm>,
}

impl ITools for StubTools {
    fn security(&self) -> Arc<dyn crate::models::ports::security::ISecurity> {
        unimplemented!("security unused by cluster tests")
    }
    fn signaler(&self) -> Arc<dyn crate::models::ports::signaler::ISignaler> {
        unimplemented!("signaler unused by cluster tests")
    }
    fn storage(&self) -> Arc<dyn IStorage> {
        self.storage.clone()
    }
    fn network(&self) -> Arc<dyn crate::models::ports::network::INetwork> {
        unimplemented!("network unused by cluster tests")
    }
    fn file(&self) -> Arc<dyn crate::models::ports::file::IFile> {
        self.file.clone()
    }
    fn vmm(&self) -> Arc<dyn crate::models::ports::vmm::IVmm> {
        self.vmm.clone()
    }
    fn rate_limiter(&self) -> Arc<dyn crate::models::ports::ratelimit::IRateLimiter> {
        // Cluster tests never exercise client-request admission; hand back a
        // real limiter so the trait is satisfied without a bespoke stub.
        crate::drivers::ratelimit::RateLimiter::new(
            crate::drivers::ratelimit::RateLimiterConfig::default(),
        )
    }
}

struct StubCore {
    id: String,
    storage: Arc<dyn IStorage>,
    tools: Arc<dyn ITools>,
}

impl StubCore {
    fn new(id: &str, dir: &str) -> (Arc<StubCore>, Arc<StubVmm>) {
        let storage: Arc<dyn IStorage> = StubStorage::new(&format!("{}/kvdb", dir));
        let vmm = Arc::new(StubVmm::default());
        let tools: Arc<dyn ITools> = Arc::new(StubTools {
            storage: StubStorage::new_with_root(storage.clone(), dir),
            file: Arc::new(StubFile),
            vmm: vmm.clone(),
        });
        (
            Arc::new(StubCore {
                id: id.to_string(),
                storage: tools.storage(),
                tools,
            }),
            vmm,
        )
    }
}

impl StubStorage {
    /// Wrap an existing stub storage but report `root` as the instance's
    /// working directory (so `apply_deploy_artifact` writes files there).
    fn new_with_root(inner: Arc<dyn IStorage>, root: &str) -> Arc<dyn IStorage> {
        Arc::new(RootedStorage {
            inner,
            root: root.to_string(),
        })
    }
}

struct RootedStorage {
    inner: Arc<dyn IStorage>,
    root: String,
}

impl IStorage for RootedStorage {
    fn storage_root(&self) -> String {
        self.root.clone()
    }
    fn kv_db(&self) -> crate::models::ports::storage::KvDb {
        self.inner.kv_db()
    }
    fn ts_db(&self) -> crate::models::ports::storage::TsDb {
        self.inner.ts_db()
    }
    fn gen_id(&self, t: &dyn ITrx, o: &str) -> String {
        self.inner.gen_id(t, o)
    }
    fn log_time_sieries(
        &self,
        a: &str,
        b: &str,
        c: &str,
        tags: &[String],
        d: i64,
    ) -> anyhow::Result<crate::models::packet::LogPacket> {
        self.inner.log_time_sieries(a, b, c, tags, d)
    }
    fn update_log(
        &self,
        a: &str,
        b: &str,
        c: &str,
        d: &str,
        e: i64,
    ) -> crate::models::packet::LogPacket {
        self.inner.update_log(a, b, c, d, e)
    }
    fn read_store_logs(
        &self,
        a: &str,
        q: &crate::models::packet::LogQuery,
    ) -> anyhow::Result<Vec<crate::models::packet::LogPacket>> {
        self.inner.read_store_logs(a, q)
    }
    fn pick_store_logs(&self, a: &str, b: Vec<String>) -> Vec<crate::models::packet::LogPacket> {
        self.inner.pick_store_logs(a, b)
    }
    fn log_vm(&self, a: &str, b: &str, c: &str, d: i64) -> crate::models::packet::BuildPacket {
        self.inner.log_vm(a, b, c, d)
    }
    fn read_vm_logs(
        &self,
        a: &str,
        b: &str,
        c: i64,
        d: i64,
    ) -> Vec<crate::models::packet::BuildPacket> {
        self.inner.read_vm_logs(a, b, c, d)
    }
}

impl ICore for StubCore {
    fn owner_id(&self) -> String {
        String::new()
    }
    fn id(&self) -> String {
        self.id.clone()
    }
    fn gods(&self) -> Vec<String> {
        Vec::new()
    }
    fn add_god(&self, _: &str) {}
    fn tools(&self) -> Arc<dyn ITools> {
        self.tools.clone()
    }
    fn free_nodes(&self) -> HashMap<String, bool> {
        HashMap::new()
    }
    fn add_free_node(&self, _: &str) {}
    fn actor(&self) -> Arc<dyn crate::models::action::actor::IActor> {
        unimplemented!()
    }
    fn load(&self, _: Vec<String>, _: HashMap<String, Value>) {}
    fn close(&self) {}
    fn plant_chain_trigger(&self, _: i64, _: &str, _: &str, _: &str, _: &str, _: &str) {}
    fn app_pending_trxs(&self) {}
    fn ip_addr(&self) -> String {
        String::new()
    }
    fn modify_state(&self, readonly: bool, mut fn_: crate::models::action::TrxClosure) {
        let core: Arc<dyn ICore> = Arc::new(StubCore {
            id: self.id.clone(),
            storage: self.storage.clone(),
            tools: self.tools.clone(),
        });
        let trx = TrxWrapper::new(core, self.storage.clone(), readonly);
        let res = fn_(&*trx);
        if res.is_ok() {
            trx.commit();
        } else {
            trx.discard();
        }
    }
    fn modify_state_securly_with_source(
        &self,
        _: bool,
        _: Arc<dyn crate::models::info::IInfo>,
        _: &str,
        _: crate::models::core::StateClosure,
    ) {
    }
    fn modify_state_securly(
        &self,
        _: bool,
        _: Arc<dyn crate::models::info::IInfo>,
        _: crate::models::core::StateClosure,
    ) {
    }
    fn sign_packet(&self, _: &[u8]) -> String {
        String::new()
    }
    fn sign_packet_as_owner(&self, _: &[u8]) -> String {
        String::new()
    }
    fn execution_cost_per_second(&self) -> i64 {
        0
    }
    fn vm_ram_cost_per_mb_per_minute(&self) -> i64 {
        0
    }
    fn vm_cpu_core_cost_per_minute(&self) -> i64 {
        0
    }
    fn vm_disk_cost_per_gb_per_minute(&self) -> i64 {
        0
    }
    fn globe(&self) -> Arc<dyn crate::models::globe::IGlobe> {
        unimplemented!()
    }
    fn begin_vm_trx(&self, _vm_id: &str) -> Arc<dyn ITrx> {
        unimplemented!()
    }
    fn end_vm_trx(&self, _vm_id: &str) {}
}

// ────────────────────────── harness helpers ────────────────────────────────

struct Instance {
    svc: Arc<super::ClusterService>,
    core: Arc<StubCore>,
    vmm: Arc<StubVmm>,
    addr: String,
    #[allow(dead_code)]
    dir: String,
}

static PORT_BASE: AtomicU64 = AtomicU64::new(17640);

fn boot_instance(node_id: u64, register_global: bool) -> Instance {
    let port = PORT_BASE.fetch_add(1, Ordering::SeqCst);
    let addr = format!("127.0.0.1:{}", port);
    let dir = format!(
        "/tmp/caspar-cluster-test-{}/node{}",
        std::process::id(),
        node_id
    );
    std::fs::create_dir_all(&dir).unwrap();
    let (core, vmm) = StubCore::new(&format!("origin-{}", node_id), &dir);

    let mut cfg = ClusterConfig::default();
    cfg.enabled = true;
    // Only the seed instance bootstraps; joiners must stay pristine until
    // the seed introduces them (the production join flow).
    cfg.bootstrap = node_id == 1;
    cfg.node_id = node_id;
    cfg.node_name = format!("test-node-{}", node_id);
    cfg.region = format!("region-{}", node_id);
    cfg.listen_addr = addr.clone();
    cfg.advertise_addr = addr.clone();
    cfg.heartbeat_interval_ms = 100;
    cfg.election_timeout_min_ms = 300;
    cfg.election_timeout_max_ms = 600;
    cfg.rtt_probe_interval_secs = 3;
    let config_path = PathBuf::from(&dir).join("cluster.json");

    let app: Arc<dyn ICore> = core.clone();
    let svc = if register_global {
        // Mirrors production `start`: builds the service AND registers it as
        // the process-global handle consulted by TrxWrapper commits.
        let svc = super::start_service(app, cfg, config_path).expect("start service");
        let _ = super::CLUSTER.set(svc.clone());
        svc
    } else {
        super::start_service(app, cfg, config_path).expect("start service")
    };
    Instance {
        svc,
        core,
        vmm,
        addr,
        dir,
    }
}

fn http_post(addr: &str, path: &str, body: &Value) -> Value {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .post(format!("http://{}{}", addr, path))
        .timeout(Duration::from_secs(60))
        .json(body)
        .send()
        .expect("http post");
    resp.json().expect("json response")
}

fn http_get(addr: &str, path: &str) -> Value {
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!("http://{}{}", addr, path))
        .timeout(Duration::from_secs(10))
        .send()
        .expect("http get");
    resp.json().expect("json response")
}

fn wait_for(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for: {}", what);
}

fn kv_get(inst: &Instance, key: &str) -> Option<Vec<u8>> {
    inst.core.storage.kv_db().get(key.as_bytes()).ok().flatten()
}

// ────────────────────────── the integration test ───────────────────────────

#[test]
fn three_instance_cluster_replication() {
    // Instance 1 is this "process's node" (global handle); 2 and 3 play the
    // remote instances of the same Caspar node.
    let n1 = boot_instance(1, true);
    let n2 = boot_instance(2, false);
    let n3 = boot_instance(3, false);

    // ── Phase 1: formation via the orchestration API ────────────────────
    wait_for("n1 self-election", Duration::from_secs(10), || {
        http_get(&n1.addr, "/cluster/status")["currentLeader"] == json!(1)
    });
    let added = http_post(
        &n1.addr,
        "/cluster/add-peer",
        &json!({"id": 2, "addr": n2.addr, "region": "region-2"}),
    );
    assert_eq!(added["ok"], json!(true), "add-peer 2 failed: {}", added);
    let added = http_post(
        &n1.addr,
        "/cluster/add-peer",
        &json!({"id": 3, "addr": n3.addr, "region": "region-3"}),
    );
    assert_eq!(added["ok"], json!(true), "add-peer 3 failed: {}", added);

    let status = http_get(&n1.addr, "/cluster/status");
    assert_eq!(status["currentLeader"], json!(1));
    let peers = http_get(&n1.addr, "/cluster/peers");
    assert_eq!(peers["peers"].as_array().unwrap().len(), 2);

    // Followers learn the leader through raft heartbeats.
    wait_for("n2 sees leader", Duration::from_secs(10), || {
        http_get(&n2.addr, "/cluster/status")["currentLeader"] == json!(1)
    });
    wait_for("n3 sees leader", Duration::from_secs(10), || {
        http_get(&n3.addr, "/cluster/status")["currentLeader"] == json!(1)
    });

    // ── Phase 2: shell write-set replication (TrxWrapper hook) ──────────
    // A write committed on instance 1 through the production transaction
    // wrapper must appear in the RocksDB of instances 2 and 3.
    n1.core.modify_state(
        false,
        Box::new(|trx: &dyn ITrx| {
            trx.put_link("User::alice", "registered");
            trx.put_string("obj::Machine::m1::owner", "alice");
            Ok(())
        }),
    );
    wait_for("kv replication to n2+n3", Duration::from_secs(15), || {
        kv_get(&n2, "link::User::alice").as_deref() == Some(b"registered")
            && kv_get(&n3, "link::User::alice").as_deref() == Some(b"registered")
            && kv_get(&n2, "obj::Machine::m1::owner").as_deref() == Some(b"alice")
    });

    // Deletes replicate too.
    n1.core.modify_state(
        false,
        Box::new(|trx: &dyn ITrx| {
            trx.del_key("link::User::alice");
            Ok(())
        }),
    );
    wait_for("delete replication", Duration::from_secs(15), || {
        kv_get(&n2, "link::User::alice").is_none() && kv_get(&n3, "link::User::alice").is_none()
    });

    // ── Phase 3: local-scope suppression (non-distributed VM commit) ────
    // A commit wrapped in a replicate=false scope (what the VMM does for
    // local-mode VMs) must stay on instance 1 even though the cluster is up.
    super::with_replication_scope(false, || {
        n1.core.modify_state(
            false,
            Box::new(|trx: &dyn ITrx| {
                trx.put_link("localVm::secret", "stays-home");
                Ok(())
            }),
        );
    });
    // Marker committed *after* the suppressed write; FIFO ordering means once
    // the marker lands remotely, the suppressed key had its chance to leak.
    n1.core.modify_state(
        false,
        Box::new(|trx: &dyn ITrx| {
            trx.put_link("marker::after-local", "yes");
            Ok(())
        }),
    );
    wait_for("ordering marker", Duration::from_secs(15), || {
        kv_get(&n2, "link::marker::after-local").is_some()
    });
    assert!(
        kv_get(&n1, "link::localVm::secret").is_some(),
        "local write must exist on the origin instance"
    );
    assert!(
        kv_get(&n2, "link::localVm::secret").is_none()
            && kv_get(&n3, "link::localVm::secret").is_none(),
        "local-scope write must NOT replicate"
    );

    // ── Phase 4: distributed creature deployment ────────────────────────
    let artifact = DeployArtifact {
        program_id: "prog-42".into(),
        entity_id: "api".into(),
        entity_type: "docker".into(),
        machine_id: "machine-7".into(),
        runtime: "docker".into(),
        path: "".into(),
        comment: "geo api".into(),
        primary_file_name: "project.tar".into(),
        files: vec![(
            "project.tar".into(),
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                b"tar-bytes-of-creature",
            ),
        )],
        set_entity_links: true,
        build_on_deploy: true,
        gateway_route: "".into(),
        gateway_vm_id: "".into(),
    };
    super::propose_deploy(artifact);

    wait_for("deploy materializes on n2+n3", Duration::from_secs(20), || {
        !n2.vmm.assigned.lock().unwrap().is_empty()
            && !n3.vmm.assigned.lock().unwrap().is_empty()
    });
    for inst in [&n2, &n3] {
        // Artifact file written on the remote instance.
        let file = format!(
            "{}/machines/prog-42/entities/api/project.tar",
            inst.dir
        );
        assert_eq!(
            std::fs::read(&file).expect("replicated artifact file"),
            b"tar-bytes-of-creature",
            "artifact bytes must match on {}",
            inst.svc.node_id
        );
        // Runtime links + distribution scope restored.
        assert_eq!(
            kv_get(inst, "link::vmEntityType::prog-42::api").as_deref(),
            Some(b"docker".as_ref())
        );
        assert_eq!(
            kv_get(inst, "link::vmDistribution::prog-42").as_deref(),
            Some(b"cluster".as_ref())
        );
        // Program record restored (model `push` writes obj columns).
        assert!(
            kv_get(inst, "obj::Program::prog-42::|").is_some(),
            "program record must exist on instance {}",
            inst.svc.node_id
        );
        assert_eq!(
            kv_get(inst, "obj::Program::prog-42::runtime").as_deref(),
            Some(b"docker".as_ref())
        );
        // VMM effects: signal listener registered + image build started.
        assert_eq!(inst.vmm.assigned.lock().unwrap().as_slice(), ["prog-42"]);
        assert_eq!(
            inst.vmm.builds.lock().unwrap().first(),
            Some(&("prog-42".to_string(), "api".to_string(), "docker".to_string()))
        );
    }
    // The origin instance skips re-application (it already deployed locally):
    assert!(n1.vmm.assigned.lock().unwrap().is_empty());

    // ── Phase 5: config + telemetry endpoints ───────────────────────────
    let ping = http_get(&n2.addr, "/cluster/ping");
    assert_eq!(ping["nodeId"], json!(2));
    assert_eq!(ping["region"], json!("region-2"));

    let set = http_post(
        &n1.addr,
        "/cluster/config",
        &json!({"key": "heartbeat_interval_ms", "value": "150"}),
    );
    assert_eq!(set["heartbeat_interval_ms"], json!(150));
    let cfg = http_get(&n1.addr, "/cluster/config");
    assert_eq!(cfg["heartbeat_interval_ms"], json!(150));
    assert_eq!(cfg["peers"]["2"]["addr"], json!(n2.addr));

    // Replicated free-form knob reaches the other instances' state machines.
    let _ = http_post(
        &n1.addr,
        "/cluster/config",
        &json!({"key": "extra.maintenanceWindow", "value": "\"02:00-04:00Z\""}),
    );

    // RTT prober fills the nearest-peer table (probe interval is 3s).
    wait_for("rtt table", Duration::from_secs(15), || {
        let nearest = http_get(&n1.addr, "/cluster/nearest");
        nearest["peers"]
            .as_array()
            .map(|p| {
                p.len() == 2
                    && p.iter()
                        .all(|e| e["reachable"] == json!(true) && e["rtt_ms"].is_number())
            })
            .unwrap_or(false)
    });

    // ── Phase 6: follower proposal forwarding ───────────────────────────
    // A command proposed on a follower must be forwarded to the leader and
    // still replicate everywhere (this is the edge-instance write path).
    let forwarded = http_post(
        &n2.addr,
        "/cluster/propose",
        &json!({
            "type": "kv",
            "origin": 2,
            "ops": [{"op": "put", "key": "link::fromFollower", "value_b64":
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"hi")}]
        }),
    );
    assert_eq!(forwarded["ok"], json!(true), "follower forward: {}", forwarded);
    wait_for("follower write on n1+n3", Duration::from_secs(15), || {
        kv_get(&n1, "link::fromFollower").as_deref() == Some(b"hi".as_ref())
            && kv_get(&n3, "link::fromFollower").as_deref() == Some(b"hi".as_ref())
    });
    // Origin (n2) skips self-application of its own batch — it would have
    // written locally before proposing in the production path.

    // ── Phase 7: leader failover ────────────────────────────────────────
    // Take the leader's raft down; the two surviving voters still hold a
    // quorum, elect a new leader among themselves, and keep replicating.
    n1.svc
        .runtime()
        .block_on(n1.svc.raft().shutdown())
        .expect("shutdown n1 raft");
    wait_for("new leader among n2/n3", Duration::from_secs(20), || {
        let l2 = http_get(&n2.addr, "/cluster/status")["currentLeader"].clone();
        let l3 = http_get(&n3.addr, "/cluster/status")["currentLeader"].clone();
        (l2 == json!(2) || l2 == json!(3)) && l2 == l3
    });
    let survivor_leader = http_get(&n2.addr, "/cluster/status")["currentLeader"]
        .as_u64()
        .unwrap();
    let submit_to = if survivor_leader == 2 { &n2 } else { &n3 };
    let other = if survivor_leader == 2 { &n3 } else { &n2 };
    let after = http_post(
        &submit_to.addr,
        "/cluster/propose",
        &json!({
            "type": "kv",
            "origin": 99,
            "ops": [{"op": "put", "key": "link::afterFailover", "value_b64":
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"ok")}]
        }),
    );
    assert_eq!(after["ok"], json!(true), "post-failover write: {}", after);
    wait_for("post-failover replication", Duration::from_secs(15), || {
        kv_get(other, "link::afterFailover").as_deref() == Some(b"ok".as_ref())
            && kv_get(submit_to, "link::afterFailover").as_deref() == Some(b"ok".as_ref())
    });
}

#[test]
fn cluster_listener_enforces_auth_token() {
    let port = PORT_BASE.fetch_add(1, Ordering::SeqCst);
    let addr = format!("127.0.0.1:{}", port);
    let dir = format!(
        "/tmp/caspar-cluster-test-{}/auth-node",
        std::process::id()
    );
    std::fs::create_dir_all(&dir).unwrap();
    let (core, _vmm) = StubCore::new("origin-auth", &dir);
    let mut cfg = ClusterConfig::default();
    cfg.enabled = true;
    cfg.bootstrap = true;
    cfg.node_id = 9;
    cfg.listen_addr = addr.clone();
    cfg.advertise_addr = addr.clone();
    cfg.auth_token = "sekret".into();
    let app: Arc<dyn ICore> = core;
    let _svc =
        super::start_service(app, cfg, PathBuf::from(&dir).join("cluster.json")).unwrap();

    let client = reqwest::blocking::Client::new();
    // No token → 401.
    let resp = client
        .get(format!("http://{}/cluster/status", addr))
        .timeout(Duration::from_secs(5))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
    // Correct token → 200 with real payload.
    let resp = client
        .get(format!("http://{}/cluster/status", addr))
        .header("x-caspar-cluster-token", "sekret")
        .timeout(Duration::from_secs(5))
        .send()
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: Value = resp.json().unwrap();
    assert_eq!(body["nodeId"], json!(9));
}
