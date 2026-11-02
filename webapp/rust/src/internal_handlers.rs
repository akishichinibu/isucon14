use crate::http::HttpResp;
use crate::{AppState, Error};

pub async fn internal_get_matching(state: &AppState) -> Result<HttpResp, Error> {
    run_matching(state).await?;
    Ok(HttpResp::empty(204))
}

pub async fn run_matching(state: &AppState) -> Result<(), Error> {
    if !state.matching_enabled {
        return Ok(());
    }
    let _guard = state.matching_lock.lock().await;
    match_inner(state).await
}

pub async fn try_run_matching(state: &AppState) -> Result<(), Error> {
    if !state.matching_enabled {
        return Ok(());
    }
    let Ok(_guard) = state.matching_lock.try_lock() else {
        return Ok(());
    };
    match_inner(state).await
}

async fn match_inner(state: &AppState) -> Result<(), Error> {
    let mut world = state.world.write().unwrap();
    let _ = world.match_once();
    Ok(())
}
