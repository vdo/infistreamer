use anyhow::Result;
use async_trait::async_trait;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::Redirect;
use axum_extra::extract::cookie::{Cookie, PrivateCookieJar, SameSite};

use crate::models::User;
use crate::AppState;

pub const SESSION_COOKIE: &str = "infistreamer_session";

pub fn hash_password(pw: &str) -> Result<String> {
    Ok(bcrypt::hash(pw, bcrypt::DEFAULT_COST)?)
}

pub fn verify_password(pw: &str, hash: &str) -> bool {
    bcrypt::verify(pw, hash).unwrap_or(false)
}

/// Build the signed session cookie carrying the user id.
pub fn session_cookie(user_id: i64) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, user_id.to_string()))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .build()
}

/// A cleared cookie used on logout.
pub fn cleared_cookie() -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, ""))
        .path("/")
        .http_only(true)
        .build()
}

/// Extractor that resolves the logged-in user, redirecting to `/login` when absent.
pub struct AuthUser {
    #[allow(dead_code)] // available to handlers; not all of them need it
    pub id: i64,
    pub username: String,
}

#[async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = Redirect;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = PrivateCookieJar::from_headers(&parts.headers, state.key.clone());
        let uid: i64 = jar
            .get(SESSION_COOKIE)
            .and_then(|c| c.value().parse().ok())
            .ok_or_else(|| Redirect::to("/login"))?;

        let user = sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = ?")
            .bind(uid)
            .fetch_optional(&state.db)
            .await
            .ok()
            .flatten()
            .ok_or_else(|| Redirect::to("/login"))?;

        Ok(AuthUser {
            id: user.id,
            username: user.username,
        })
    }
}
