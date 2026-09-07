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
/// Version string reported to Modal. It identifies this client in Modal's
/// logs; it is not a compatibility assertion.
const CLIENT_VERSION: &str = "caspar-vm-modal/0.1.0";

/// The authenticated stub type produced by [`connect`].
pub(crate) type ModalStub = ModalClientClient<
    InterceptedService<Channel, AuthInterceptor>,
>;

#[derive(Clone)]
pub(crate) struct AuthInterceptor {
    token_id: String,
    token_secret: String,
    environment: String,
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
        set(md, "x-modal-client-version", CLIENT_VERSION);
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
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> Result<F::Output, String> {
    Ok(runtime()?.block_on(fut))
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
            // `connect_lazy` avoids a blocking connect inside the OnceLock
            // initializer; the first RPC establishes the connection and
            // surfaces any failure as that call's error.
            Ok((endpoint.connect_lazy(), creds))
        })
        .as_ref()
        .map_err(|e| e.clone())?;

    let interceptor = AuthInterceptor {
        token_id: creds.token_id.clone(),
        token_secret: creds.token_secret.clone(),
        environment: creds.environment.clone(),
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
