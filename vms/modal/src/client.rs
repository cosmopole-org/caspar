//! The Modal gRPC control-plane client.
//!
//! Modal ships no Rust SDK, so this speaks its gRPC API directly, against the
//! vendored proto slice in `proto/modal.proto`. Modal states that direct gRPC
//! use carries no compatibility guarantee — when a call breaks upstream,
//! re-slice the proto (`scripts/slice_modal_proto.py`) and adjust here.
//!
//! Plugin methods are synchronous (the `VmPlugin` contract is), so the async
//! client is driven from one process-wide Tokio runtime rather than a runtime
//! per call: creating a multi-thread runtime for every exec would cost more
//! than the RPC it wraps.

use std::sync::OnceLock;
use std::time::Duration;

use tonic::metadata::MetadataValue;
use tonic::service::interceptor::InterceptedService;
use tonic::transport::{Channel, ClientTlsConfig};
use tonic::Request;

use crate::proto::modal_client_client::ModalClientClient;

/// Modal API credentials and the workspace context every call runs in.
#[derive(Clone, Debug)]
pub(crate) struct ModalCredentials {
    pub(crate) token_id: String,
    pub(crate) token_secret: String,
    pub(crate) environment: String,
    pub(crate) server_url: String,
}

impl ModalCredentials {
    /// Read credentials from the node's environment.
    ///
    /// `MODAL_API_KEY` is the single-variable form the deploy uses —
    /// `<token-id>:<token-secret>`, the same pair Modal's dashboard issues.
    /// The split `MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET` form (what Modal's
    /// own tooling exports) is accepted too, so a host that already has a
    /// Modal profile configured needs no new variable.
    pub(crate) fn from_env() -> Result<Self, String> {
        let mut token_id = env_trimmed("MODAL_TOKEN_ID");
        let mut token_secret = env_trimmed("MODAL_TOKEN_SECRET");

        if token_id.is_empty() || token_secret.is_empty() {
            let api_key = env_trimmed("MODAL_API_KEY");
            if !api_key.is_empty() {
                match api_key.split_once(':') {
                    Some((id, secret)) => {
                        token_id = id.trim().to_string();
                        token_secret = secret.trim().to_string();
                    }
                    None => {
                        return Err(
                            "MODAL_API_KEY must be '<token-id>:<token-secret>'".to_string()
                        )
                    }
                }
            }
        }

        if token_id.is_empty() || token_secret.is_empty() {
            return Err(
                "modal runtime is not configured: set MODAL_API_KEY (or MODAL_TOKEN_ID + MODAL_TOKEN_SECRET)"
                    .to_string(),
            );
        }

        let server_url = {
            let raw = env_trimmed("MODAL_SERVER_URL");
            if raw.is_empty() {
                "https://api.modal.com:443".to_string()
            } else {
                raw
            }
        };

        Ok(Self {
            token_id,
            token_secret,
            environment: env_trimmed("MODAL_ENVIRONMENT"),
            server_url,
        })
    }
}

fn env_trimmed(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
}

/// Client type Modal's server expects in `x-modal-client-type`
/// (`CLIENT_TYPE_CLIENT`).
const CLIENT_TYPE_CLIENT: &str = "1";
/// Version string reported to Modal in `x-modal-client-version`.
///
/// Modal PARSES this and refuses anything it cannot read as a version of a
/// supported client — `FailedPrecondition: Invalid client version` — so it is
/// not a free-form identifier, however much it looks like one. A name-and-slash
/// string ("caspar-vm-modal/0.1.0") is rejected outright, which is why this is
/// a bare semver: it is a compatibility assertion, and Modal enforces it.
///
/// Modal raises its minimum supported client over time, so this is
/// overridable from the environment: a node can be moved onto an accepted
/// version without waiting for a release of this plugin.
const DEFAULT_CLIENT_VERSION: &str = "1.0.0";

pub(crate) fn client_version() -> String {
    let configured = env_trimmed("MODAL_CLIENT_VERSION");
    if configured.is_empty() {
        DEFAULT_CLIENT_VERSION.to_string()
    } else {
        configured
    }
}

/// The authenticated stub type produced by [`connect`].
pub(crate) type ModalStub = ModalClientClient<
    InterceptedService<Channel, AuthInterceptor>,
>;

#[derive(Clone)]
pub(crate) struct AuthInterceptor {
    token_id: String,
    token_secret: String,
    environment: String,
    client_version: String,
}

impl tonic::service::Interceptor for AuthInterceptor {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, tonic::Status> {
        let md = req.metadata_mut();
        let set = |md: &mut tonic::metadata::MetadataMap, key: &'static str, value: &str| {
            if let Ok(v) = MetadataValue::try_from(value) {
                md.insert(key, v);
            }
        };
        set(md, "x-modal-token-id", &self.token_id);
        set(md, "x-modal-token-secret", &self.token_secret);
        set(md, "x-modal-client-type", CLIENT_TYPE_CLIENT);
        set(md, "x-modal-client-version", &self.client_version);
        if !self.environment.is_empty() {
            set(md, "x-modal-environment", &self.environment);
        }
        Ok(req)
    }
}

/// The process-wide runtime every Modal call is driven on.
fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RT: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("caspar-modal")
            .build()
            .map_err(|e| format!("modal runtime init failed: {}", e))
    })
    .as_ref()
    .map_err(|e| e.clone())
}

/// Run one async Modal operation to completion from synchronous plugin code.
///
/// Every caller is a plain node thread — the node's network layer and its VM
/// workers are `std::thread`s, not Tokio tasks — so this is an ordinary
/// `block_on` on the Modal runtime. A caller that was already inside some other
/// runtime is reported rather than attempted: `block_on` from within a runtime
/// panics, and a named error is worth more than a panic surfacing as "vmm panic"
/// on a file listing. (The futures here borrow their stub, so they cannot simply
/// be spawned onto another runtime instead.)
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> Result<F::Output, String> {
    let rt = runtime()?;
    if tokio::runtime::Handle::try_current().is_ok() {
        return Err(
            "modal operations cannot be driven from inside another Tokio runtime".to_string(),
        );
    }
    Ok(rt.block_on(fut))
}

/// A connected, authenticated Modal client plus the context its requests carry.
pub(crate) struct ModalConn {
    pub(crate) stub: ModalStub,
    pub(crate) environment: String,
}

/// Connect to Modal with the node's configured credentials.
///
/// The channel is lazily connected and cheaply cloneable, so it is built once
/// and reused: Modal's control plane is remote, and re-establishing TLS per
/// operation would put a round trip in front of every exec.
pub(crate) fn connect() -> Result<ModalConn, String> {
    static CHANNEL: OnceLock<Result<(Channel, ModalCredentials), String>> = OnceLock::new();
    let (channel, creds) = CHANNEL
        .get_or_init(|| {
            let creds = ModalCredentials::from_env()?;
            let tls = ClientTlsConfig::new().with_enabled_roots();
            let endpoint = Channel::from_shared(creds.server_url.clone())
                .map_err(|e| format!("invalid MODAL_SERVER_URL: {}", e))?
                .tls_config(tls)
                .map_err(|e| format!("modal TLS config failed: {}", e))?
                .connect_timeout(Duration::from_secs(20))
                .timeout(Duration::from_secs(120))
                .http2_keep_alive_interval(Duration::from_secs(30))
                .keep_alive_while_idle(true);
            // BUILDING the channel is itself an async-runtime operation, not
            // just using it: `connect_lazy` hands tonic's buffer worker to the
            // ambient executor with `tokio::spawn`, which panics outright when
            // there is no runtime on this thread — "there is no reactor
            // running, must be called from the context of a Tokio 1.x runtime".
            //
            // Every caller here is a plain node thread (the node's own network
            // layer and its VM workers are `std::thread`s), so there never was
            // one, and the panic came back as a failed VM op with a Tokio
            // message in it. Enter the Modal runtime for the construction, so
            // the worker is spawned onto the same runtime that will later drive
            // the calls.
            //
            // `connect_lazy` rather than `connect` is still deliberate: it keeps
            // a blocking network round trip out of this initializer, and the
            // first RPC surfaces a connection failure as that call's error.
            let _guard = runtime()?.enter();
            Ok((endpoint.connect_lazy(), creds))
        })
        .as_ref()
        .map_err(|e| e.clone())?;

    let interceptor = AuthInterceptor {
        token_id: creds.token_id.clone(),
        token_secret: creds.token_secret.clone(),
        environment: creds.environment.clone(),
        // Read per connect, not once per process, so a node that has to move
        // onto a different accepted version only needs a restart.
        client_version: client_version(),
    };
    Ok(ModalConn {
        stub: ModalClientClient::with_interceptor(channel.clone(), interceptor)
            // Sandbox images and file payloads exceed tonic's 4 MiB default.
            .max_decoding_message_size(64 * 1024 * 1024)
            .max_encoding_message_size(64 * 1024 * 1024),
        environment: creds.environment.clone(),
    })
}

/// Whether the node is configured to talk to Modal at all. Used to fail a
/// modal operation with a clear message instead of a transport error.
pub(crate) fn is_configured() -> bool {
    ModalCredentials::from_env().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Building the Modal channel must not need a Tokio runtime on the CALLER's
    /// thread.
    ///
    /// This is the regression that took the platform's sandboxes down: every
    /// entry point into this plugin is a plain node thread, `connect_lazy`
    /// spawns tonic's buffer worker with `tokio::spawn`, and that panics with
    /// "there is no reactor running, must be called from the context of a Tokio
    /// 1.x runtime" when no runtime is entered. The panic came back to the
    /// client as `vmm panic: …` on a file listing, and no sandbox was ever
    /// created. The test runs `connect` on a bare thread, which is exactly what
    /// the node does.
    #[test]
    fn connects_from_a_thread_with_no_tokio_runtime() {
        // Placeholders only when the process has no real credentials. The
        // channel and the credentials behind it are cached process-wide, so
        // overwriting a configured token here would break every live test that
        // runs after this one in the same binary.
        if !ModalCredentials::from_env().is_ok() {
            std::env::set_var("MODAL_TOKEN_ID", "ak-test");
            std::env::set_var("MODAL_TOKEN_SECRET", "as-test");
        }

        let built = std::thread::spawn(|| {
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "the test thread must have no runtime, like the node's own threads",
            );
            connect().map(|_| ())
        })
        .join();

        match built {
            Ok(Ok(())) => {}
            Ok(Err(e)) => panic!("connect failed: {}", e),
            Err(_) => panic!("connect panicked on a thread with no Tokio runtime"),
        }
    }
}
