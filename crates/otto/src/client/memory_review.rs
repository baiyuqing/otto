//! Memory candidate review over the session memory routes, for clients that
//! hold no controller of their own (`otto acp --attach`).

use super::*;

impl Client {
    /// `GET /v1/sessions/{id}/memory/candidates`: one page of candidates that
    /// wait for a human decision, as serve's JSON (`candidates`,
    /// `next_cursor`).
    pub async fn memory_candidates(
        &self,
        session_id: &str,
        cursor: &str,
        limit: usize,
    ) -> Result<Value, Error> {
        let limit = limit.to_string();
        let mut query = vec![("limit", limit.as_str())];
        if !cursor.is_empty() {
            query.push(("cursor", cursor));
        }
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/memory/candidates"))
                    .query(&query),
            )
            .await?;
        decode(response).await
    }

    /// `POST /v1/sessions/{id}/memory/candidates/{candidate_id}/review` with
    /// `decision` `accept` or `reject`, as serve's JSON (`decision`,
    /// `candidate`, optional `record` and `tombstone`).
    pub async fn review_memory_candidate(
        &self,
        session_id: &str,
        candidate_id: &str,
        decision: &str,
    ) -> Result<Value, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/memory/candidates/{candidate_id}/review"),
                &json!({ "decision": decision }),
            ))
            .await?;
        decode(response).await
    }
}
