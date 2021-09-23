// Copyright 2021 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use anyhow::Context;
use oauth2_types::pkce;
use serde::Serialize;
use sqlx::{Executor, FromRow, Postgres};
use thiserror::Error;
use warp::reject::Reject;

#[derive(FromRow, Serialize)]
pub struct OAuth2Code {
    id: i64,
    oauth2_session_id: i64,
    pub code: String,
    code_challenge: Option<String>,
    code_challenge_method: Option<i16>,
}

pub async fn add_code(
    executor: impl Executor<'_, Database = Postgres>,
    oauth2_session_id: i64,
    code: &str,
    code_challenge: &Option<pkce::Request>,
) -> anyhow::Result<OAuth2Code> {
    let code_challenge_method = code_challenge
        .as_ref()
        .map(|c| c.code_challenge_method as i16);
    let code_challenge = code_challenge.as_ref().map(|c| &c.code_challenge);
    sqlx::query_as!(
        OAuth2Code,
        r#"
            INSERT INTO oauth2_codes
                (oauth2_session_id, code, code_challenge_method, code_challenge)
            VALUES
                ($1, $2, $3, $4)
            RETURNING
                id, oauth2_session_id, code, code_challenge_method, code_challenge
        "#,
        oauth2_session_id,
        code,
        code_challenge_method,
        code_challenge,
    )
    .fetch_one(executor)
    .await
    .context("could not insert oauth2 authorization code")
}

pub struct OAuth2CodeLookup {
    pub id: i64,
    pub oauth2_session_id: i64,
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: String,
    pub nonce: Option<String>,
}

#[derive(Debug, Error)]
#[error("failed to lookup oauth2 code")]
pub struct CodeLookupError(#[from] sqlx::Error);

impl Reject for CodeLookupError {}

impl CodeLookupError {
    #[must_use]
    pub fn not_found(&self) -> bool {
        matches!(self.0, sqlx::Error::RowNotFound)
    }
}

pub async fn lookup_code(
    executor: impl Executor<'_, Database = Postgres>,
    code: &str,
) -> Result<OAuth2CodeLookup, CodeLookupError> {
    let res = sqlx::query_as!(
        OAuth2CodeLookup,
        r#"
            SELECT
                oc.id,
                os.id        AS "oauth2_session_id!",
                os.client_id AS "client_id!",
                os.redirect_uri,
                os.scope     AS "scope!",
                os.nonce
            FROM oauth2_codes oc
            INNER JOIN oauth2_sessions os
              ON os.id = oc.oauth2_session_id
            WHERE oc.code = $1
        "#,
        code,
    )
    .fetch_one(executor)
    .await?;

    Ok(res)
}

pub async fn consume_code(
    executor: impl Executor<'_, Database = Postgres>,
    code_id: i64,
) -> anyhow::Result<()> {
    // TODO: mark the code as invalid instead to allow invalidating the whole
    // session on code reuse
    let res = sqlx::query!(
        r#"
            DELETE FROM oauth2_codes
            WHERE id = $1
        "#,
        code_id,
    )
    .execute(executor)
    .await
    .context("could not consume authorization code")?;

    if res.rows_affected() == 1 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "no row were affected when consuming authorization code"
        ))
    }
}
