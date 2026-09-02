//! fal Direct Server adapter. The only accepted training body is the closed,
//! synthetic [`HostedSyntheticPayload`](crate::fal::HostedSyntheticPayload).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use tokio::sync::Semaphore;

use crate::{
    cancel::{CancelToken, Cancellation},
    fal::{HostedSyntheticPayload, HostedTrainingOutcome},
};

const REQUEST_ID_HEADER: &str = "x-fal-request-id";
const MAX_SERVER_BODY: usize = 64 * 1024;
const MAX_JOBS: usize = 1024;
const MAX_CONCURRENT_JOBS: usize = 1;
/// Extra time allowed beyond a job's own declared
/// `budget.max_wall_time_seconds` before the server force-fails it and
/// reclaims its bookkeeping. Covers checkpoint I/O and executor overhead
/// past the training loop's own deadline.
const SERVER_EXECUTION_GRACE_SECONDS: u64 = 60;

/// Synchronous typed executor used inside `spawn_blocking`. Production binds
/// this to the same Burn trainer as the local CLI; tests use a deterministic
/// fake without compiling a tensor backend.
pub trait SyntheticJobExecutor: Send + Sync + 'static {
    /// Execute exactly one validated synthetic plan.
    fn execute(
        &self,
        payload: HostedSyntheticPayload,
        cancellation: &CancelToken,
    ) -> Result<HostedTrainingOutcome, String>;
}

#[derive(Clone)]
struct ServerState {
    executor: Arc<dyn SyntheticJobExecutor>,
    jobs: Arc<Mutex<HashMap<String, JobState>>>,
    execution_slots: Arc<Semaphore>,
    webhook_secret: Arc<str>,
}

#[derive(Clone)]
enum JobState {
    Running {
        digest: crate::config::Sha256Digest,
        cancel: CancelToken,
        expires_at_ms: u64,
    },
    Complete {
        digest: crate::config::Sha256Digest,
        result: Box<HostedTrainingOutcome>,
    },
    Failed {
        digest: crate::config::Sha256Digest,
        expires_at_ms: u64,
    },
}

/// Health response used by fal deployment probes.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    /// Service readiness.
    pub status: &'static str,
    /// Protocol limitation.
    pub training_mode: &'static str,
}

/// Constructs the Direct Server router.
///
/// `webhook_secret` binds `/train` and `/train/cancel` to fal's routing
/// layer: every request to those routes must carry
/// `Authorization: Bearer <webhook_secret>`, checked in constant time.
/// Before this, the only "authentication" was that `x-fal-request-id` was
/// well-formed and the body matched the closed synthetic schema -- neither
/// of which requires the caller to be fal at all. `/health` stays
/// unauthenticated, matching this workspace's existing precedent
/// (`wifi-densepose-sensing-server`'s bearer-auth layer) of never gating
/// liveness probes.
pub fn router(
    executor: Arc<dyn SyntheticJobExecutor>,
    webhook_secret: impl Into<Arc<str>>,
) -> Router {
    let state = ServerState {
        executor,
        jobs: Arc::new(Mutex::new(HashMap::new())),
        execution_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_JOBS)),
        webhook_secret: webhook_secret.into(),
    };
    Router::new()
        .route("/health", get(health))
        .route("/train", post(train))
        .route("/train/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(MAX_SERVER_BODY))
        .with_state(state)
}

/// Length-then-byte constant-time compare, so a mismatch does not leak how
/// many leading bytes matched through response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authenticated(headers: &HeaderMap, webhook_secret: &str) -> bool {
    let Some(presented) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
    else {
        return false;
    };
    constant_time_eq(presented.as_bytes(), webhook_secret.as_bytes())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        training_mode: "synthetic_only",
    })
}

async fn train(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if !authenticated(&headers, &state.webhook_secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let request_id = match request_id(&headers) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let payload: HostedSyntheticPayload = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid synthetic payload").into_response(),
    };
    if payload.validate_for_worker(unix_time_millis()).is_err() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            "rejected synthetic payload",
        )
            .into_response();
    }
    let (cancel, execution_slot) = {
        let mut jobs = match state.jobs.lock() {
            Ok(value) => value,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let now_ms = unix_time_millis();
        jobs.retain(|_, job| job_is_still_relevant(job, now_ms));
        match jobs.get(&request_id) {
            Some(JobState::Complete { digest, result }) if *digest == payload.request_digest => {
                return Json(result.as_ref().clone()).into_response();
            }
            Some(JobState::Running { digest, .. }) if *digest == payload.request_digest => {
                return (StatusCode::CONFLICT, "request already running").into_response();
            }
            Some(JobState::Failed { digest, .. }) if *digest == payload.request_digest => {
                // X-Fal-No-Retry makes fal itself avoid retrying; a repeated
                // delivery receives the same terminal answer.
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "request previously failed",
                )
                    .into_response();
            }
            Some(_) => return (StatusCode::CONFLICT, "request id digest conflict").into_response(),
            None if jobs.len() >= MAX_JOBS => return StatusCode::TOO_MANY_REQUESTS.into_response(),
            None => {}
        }
        let execution_slot = match Arc::clone(&state.execution_slots).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return StatusCode::TOO_MANY_REQUESTS.into_response(),
        };
        let token = CancelToken::new();
        jobs.insert(
            request_id.clone(),
            JobState::Running {
                digest: payload.request_digest,
                cancel: token.clone(),
                expires_at_ms: payload.expires_at_ms,
            },
        );
        (token, execution_slot)
    };

    let executor = Arc::clone(&state.executor);
    let execute_cancel = cancel.clone();
    let _execution_slot = execution_slot;
    let deadline = Duration::from_secs(
        payload
            .budget
            .max_wall_time_seconds
            .saturating_add(SERVER_EXECUTION_GRACE_SECONDS),
    );
    let join = tokio::task::spawn_blocking(move || executor.execute(payload, &execute_cancel));
    let result = match tokio::time::timeout(deadline, join).await {
        Ok(joined) => joined,
        Err(_) => {
            // The wall-clock deadline elapsed. A `spawn_blocking` OS thread
            // cannot be force-killed from here, but signal cooperative
            // cancellation and recover the server's own bookkeeping (job
            // state and, once this handler returns, the execution-slot
            // permit) so one wedged job cannot wedge the whole server.
            cancel.cancel();
            let mut jobs = match state.jobs.lock() {
                Ok(value) => value,
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
            if let Some(JobState::Running {
                digest,
                expires_at_ms,
                ..
            }) = jobs.get(&request_id)
            {
                let (digest, expires_at_ms) = (*digest, *expires_at_ms);
                jobs.insert(
                    request_id,
                    JobState::Failed {
                        digest,
                        expires_at_ms,
                    },
                );
            }
            return (StatusCode::GATEWAY_TIMEOUT, "training deadline exceeded").into_response();
        }
    };
    let mut jobs = match state.jobs.lock() {
        Ok(value) => value,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match result {
        Ok(Ok(value)) => {
            let digest = match jobs.get(&request_id) {
                Some(JobState::Running { digest, .. }) => *digest,
                _ => return StatusCode::CONFLICT.into_response(),
            };
            jobs.insert(
                request_id,
                JobState::Complete {
                    digest,
                    result: Box::new(value.clone()),
                },
            );
            Json(value).into_response()
        }
        Ok(Err(_)) => {
            if let Some(JobState::Running {
                digest,
                expires_at_ms,
                ..
            }) = jobs.get(&request_id)
            {
                let (digest, expires_at_ms) = (*digest, *expires_at_ms);
                jobs.insert(
                    request_id,
                    JobState::Failed {
                        digest,
                        expires_at_ms,
                    },
                );
            }
            if cancel.is_cancelled() {
                (
                    StatusCode::from_u16(499).expect("valid cancellation status"),
                    "training cancelled",
                )
                    .into_response()
            } else {
                (StatusCode::UNPROCESSABLE_ENTITY, "training failed").into_response()
            }
        }
        Err(_) => {
            if let Some(JobState::Running {
                digest,
                expires_at_ms,
                ..
            }) = jobs.get(&request_id)
            {
                let (digest, expires_at_ms) = (*digest, *expires_at_ms);
                jobs.insert(
                    request_id,
                    JobState::Failed {
                        digest,
                        expires_at_ms,
                    },
                );
            }
            (StatusCode::INTERNAL_SERVER_ERROR, "training task failed").into_response()
        }
    }
}

async fn cancel(State(state): State<ServerState>, headers: HeaderMap) -> impl IntoResponse {
    if !authenticated(&headers, &state.webhook_secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let request_id = match request_id(&headers) {
        Ok(value) => value,
        Err(status) => return status.into_response(),
    };
    let jobs = match state.jobs.lock() {
        Ok(value) => value,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match jobs.get(&request_id) {
        Some(JobState::Running { cancel, .. }) => {
            cancel.cancel();
            StatusCode::ACCEPTED.into_response()
        }
        Some(JobState::Complete { .. } | JobState::Failed { .. }) => StatusCode::OK.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn request_id(headers: &HeaderMap) -> Result<String, StatusCode> {
    let value = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(value.to_owned())
}

/// Whether `job` is still worth keeping in the in-memory table at `now_ms`.
/// A `Running` entry past its own declared expiry is wedged: the wall-clock
/// timeout in `train` should have already force-failed it. Pruning it here
/// too is defense in depth against that path somehow not running (e.g. the
/// task was aborted rather than completing its post-await cleanup) --
/// otherwise a single wedged entry would occupy a job slot and reject every
/// retry of that request id forever.
fn job_is_still_relevant(job: &JobState, now_ms: u64) -> bool {
    match job {
        JobState::Running { expires_at_ms, .. } => *expires_at_ms > now_ms,
        JobState::Complete { result, .. } => result.artifacts_expire_at_ms() > now_ms,
        JobState::Failed { expires_at_ms, .. } => *expires_at_ms > now_ms,
    }
}

fn unix_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    const TEST_SECRET: &str = "test-webhook-secret";

    struct NeverCalled;
    impl SyntheticJobExecutor for NeverCalled {
        fn execute(
            &self,
            _: HostedSyntheticPayload,
            _: &CancelToken,
        ) -> Result<HostedTrainingOutcome, String> {
            panic!("invalid request reached executor")
        }
    }

    fn authed(builder: axum::http::request::Builder) -> axum::http::request::Builder {
        builder.header(AUTHORIZATION, format!("Bearer {TEST_SECRET}"))
    }

    #[tokio::test]
    async fn train_requires_matching_webhook_secret() {
        let unauthenticated = router(Arc::new(NeverCalled), TEST_SECRET)
            .oneshot(
                Request::post("/train")
                    .header(REQUEST_ID_HEADER, "req-1")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let wrong_secret = router(Arc::new(NeverCalled), TEST_SECRET)
            .oneshot(
                Request::post("/train")
                    .header(AUTHORIZATION, "Bearer not-the-secret")
                    .header(REQUEST_ID_HEADER, "req-1")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_secret.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cancel_requires_matching_webhook_secret() {
        let response = router(Arc::new(NeverCalled), TEST_SECRET)
            .oneshot(
                Request::post("/train/cancel")
                    .header(REQUEST_ID_HEADER, "req-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn direct_server_train_requires_request_id_header() {
        let response = router(Arc::new(NeverCalled), TEST_SECRET)
            .oneshot(
                authed(Request::post("/train"))
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn request_id_rejects_path_injection() {
        let response = router(Arc::new(NeverCalled), TEST_SECRET)
            .oneshot(
                authed(Request::post("/train"))
                    .header(REQUEST_ID_HEADER, "bad/path")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_cancel_is_not_success() {
        let response = router(Arc::new(NeverCalled), TEST_SECRET)
            .oneshot(
                authed(Request::post("/train/cancel"))
                    .header(REQUEST_ID_HEADER, "req-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn privacy_external_dataset_payload_is_denied() {
        let body = r#"{"dataset_path":"/data/customer.jsonl","tenant":"x"}"#;
        let response = router(Arc::new(NeverCalled), TEST_SECRET)
            .oneshot(
                authed(Request::post("/train"))
                    .header(REQUEST_ID_HEADER, "req-2")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn server_allows_one_training_execution() {
        let slots = Arc::new(Semaphore::new(MAX_CONCURRENT_JOBS));
        let first = Arc::clone(&slots).try_acquire_owned().unwrap();
        assert!(Arc::clone(&slots).try_acquire_owned().is_err());
        drop(first);
        assert!(slots.try_acquire_owned().is_ok());
    }

    #[test]
    fn constant_time_eq_matches_exact_bytes_only() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secre1"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"secret"));
    }

    #[test]
    fn authenticated_requires_exact_bearer_match() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer right".parse().unwrap());
        assert!(authenticated(&headers, "right"));
        assert!(!authenticated(&headers, "wrong"));

        let mut malformed = HeaderMap::new();
        malformed.insert(AUTHORIZATION, "right".parse().unwrap());
        assert!(!authenticated(&malformed, "right"));

        assert!(!authenticated(&HeaderMap::new(), "right"));
    }

    #[test]
    fn expired_running_job_is_pruned_alongside_terminal_states() {
        let running_expired = JobState::Running {
            digest: crate::config::Sha256Digest::of_bytes(b"job"),
            cancel: CancelToken::new(),
            expires_at_ms: 100,
        };
        let running_live = JobState::Running {
            digest: crate::config::Sha256Digest::of_bytes(b"job"),
            cancel: CancelToken::new(),
            expires_at_ms: 1_000,
        };
        assert!(!job_is_still_relevant(&running_expired, 500));
        assert!(job_is_still_relevant(&running_live, 500));
    }
}
