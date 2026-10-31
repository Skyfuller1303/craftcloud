//! HTTP-cancellation-resistant wrappers for long-running FigCraft operations.

use std::{future::Future, sync::Arc};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use tokio::sync::OwnedSemaphorePermit;

pub(crate) const MAX_FIGCRAFT_JOBS: usize = 4;

use crate::{ApiState, error_response, figcraft, session::AuthenticatedSession};

fn admit(state: &ApiState, headers: &HeaderMap) -> Result<OwnedSemaphorePermit, Box<Response>> {
    let Some(services) = &state.services else {
        return Err(Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "FIGCRAFT_UNAVAILABLE",
            "FigCraft live editing is unavailable.",
            headers,
        )));
    };
    if services.is_draining() {
        return Err(Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVER_DRAINING",
            "The server is shutting down.",
            headers,
        )));
    }
    let permit = match try_job_permit(Arc::clone(&services.figcraft_job_permits)) {
        Ok(permit) => permit,
        Err(()) => {
            return Err(Box::new(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "FIGCRAFT_BUSY",
                "FigCraft is handling its maximum number of operations.",
                headers,
            )));
        }
    };
    if services.is_draining() {
        return Err(Box::new(error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "SERVER_DRAINING",
            "The server is shutting down.",
            headers,
        )));
    }
    Ok(permit)
}

fn try_job_permit(permits: Arc<tokio::sync::Semaphore>) -> Result<OwnedSemaphorePermit, ()> {
    permits.try_acquire_owned().map_err(|_| ())
}

async fn await_job(
    headers: HeaderMap,
    permit: OwnedSemaphorePermit,
    run: impl Future<Output = Response> + Send + 'static,
) -> Response {
    let task = tokio::spawn(async move {
        let _permit = permit;
        run.await
    });
    match task.await {
        Ok(response) => response,
        Err(_) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "FIGCRAFT_UNAVAILABLE",
            "The FigCraft operation could not be completed.",
            &headers,
        ),
    }
}

pub(crate) async fn open(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(response) =
        figcraft::preauthorize(&state, &headers, &session, &raw, "figcraft.live.open").await
    {
        return *response;
    }
    let permit = match admit(&state, &headers) {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    let job_state = state.clone();
    let job_headers = headers.clone();
    await_job(headers, permit, async move {
        figcraft::open(State(job_state), job_headers, session, Path(raw), body).await
    })
    .await
}

pub(crate) async fn snapshot(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    if let Err(response) =
        figcraft::preauthorize(&state, &headers, &session, &raw, "figcraft.live.snapshot").await
    {
        return *response;
    }
    let permit = match admit(&state, &headers) {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    let job_state = state.clone();
    let job_headers = headers.clone();
    await_job(headers, permit, async move {
        figcraft::snapshot(State(job_state), job_headers, session, Path(raw)).await
    })
    .await
}

pub(crate) async fn stop(
    State(state): State<ApiState>,
    headers: HeaderMap,
    session: AuthenticatedSession,
    Path(raw): Path<String>,
) -> Response {
    if let Err(response) =
        figcraft::preauthorize(&state, &headers, &session, &raw, "figcraft.live.stop").await
    {
        return *response;
    }
    let permit = match admit(&state, &headers) {
        Ok(permit) => permit,
        Err(response) => return *response,
    };
    let job_state = state.clone();
    let job_headers = headers.clone();
    await_job(headers, permit, async move {
        figcraft::stop(State(job_state), job_headers, session, Path(raw)).await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn figcraft_jobs_fail_fast_at_the_configured_limit() {
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_FIGCRAFT_JOBS));
        let mut held = (0..MAX_FIGCRAFT_JOBS)
            .map(|_| try_job_permit(Arc::clone(&permits)).unwrap())
            .collect::<Vec<_>>();
        assert!(try_job_permit(Arc::clone(&permits)).is_err());
        drop(held.pop());
        assert!(try_job_permit(permits).is_ok());
    }

    #[tokio::test]
    async fn timed_out_drain_does_not_abort_a_slow_job() {
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_FIGCRAFT_JOBS));
        let job = try_job_permit(Arc::clone(&permits)).unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(10),
            crate::services::wait_for_figcraft_jobs(Arc::clone(&permits)),
        )
        .await;
        assert!(result.is_err(), "drain waits for the admitted operation");
        assert_eq!(permits.available_permits(), MAX_FIGCRAFT_JOBS - 1);
        drop(job);
        tokio::time::timeout(
            Duration::from_secs(1),
            crate::services::wait_for_figcraft_jobs(Arc::clone(&permits)),
        )
        .await
        .expect("drain completes after the operation releases its permit");
        assert_eq!(permits.available_permits(), MAX_FIGCRAFT_JOBS);
    }
}
