//! Compaction, reflection, skill, context and trace calls.

use super::*;

impl Client {
    /// `POST /v1/sessions/{id}/compact`. `None` when serve answered 200 with
    /// an empty body, which it does when shutdown cancelled the compaction.
    pub async fn compact(
        &self,
        session_id: &str,
        focus: &str,
    ) -> Result<Option<WireCompaction>, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/compact"),
                &json!({ "focus": focus }),
            ))
            .await?;
        let body = response.bytes().await.map_err(unreachable_error)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| unreachable_text(format!("decode response: {error}")))
    }

    /// `POST /v1/sessions/{id}/reflect`. The request id binds a later cancel
    /// to this reflection only.
    pub async fn reflect(
        &self,
        session_id: &str,
        focus: &str,
        request_id: &str,
    ) -> Result<Option<ReflectionResult>, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/reflect"),
                &json!({ "focus": focus, "request_id": request_id }),
            ))
            .await?;
        let body = response.bytes().await.map_err(unreachable_error)?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| unreachable_text(format!("decode response: {error}")))
    }

    /// Cancels only the reflection with this request id. A missing or stale
    /// id has no effect on another operation in the session.
    pub async fn cancel_reflection(&self, session_id: &str, request_id: &str) -> Result<(), Error> {
        self.send(self.post(
            &format!("{BASE}/v1/sessions/{session_id}/reflect/cancel"),
            &json!({ "request_id": request_id }),
        ))
        .await
        .map(drop)
    }

    /// `GET /v1/sessions/{id}/reflection/skills`.
    pub async fn generated_skills(&self, session_id: &str) -> Result<GeneratedSkillList, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/reflection/skills")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/reflection/runs`.
    pub async fn reflection_runs(&self, session_id: &str) -> Result<ReflectionRunList, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/reflection/runs")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/reflection/runs/{run}`; `None` when serve has
    /// no such run log.
    pub async fn reflection_run(
        &self,
        session_id: &str,
        run: &str,
    ) -> Result<Option<ReflectionRunLog>, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static Otto base URL");
        url.path_segments_mut()
            .expect("base URL supports path segments")
            .extend(["v1", "sessions", session_id, "reflection", "runs", run]);
        match self.send(self.http.get(url.as_str())).await {
            Ok(response) => decode(response).await.map(Some),
            Err(Error::Http { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn skill_manager(
        &self,
        session: &str,
    ) -> Result<crate::app::management::SkillManager, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static base");
        url.path_segments_mut().expect("base URL").extend([
            "v1",
            "sessions",
            session,
            "skill-manager",
        ]);
        decode(self.send(self.http.get(url.as_str())).await?).await
    }

    pub(crate) async fn skill_info(
        &self,
        session: &str,
        name: &str,
    ) -> Result<crate::app::management::SkillInfo, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static base");
        url.path_segments_mut().expect("base URL").extend([
            "v1",
            "sessions",
            session,
            "skill-manager",
            name,
        ]);
        decode(self.send(self.http.get(url.as_str())).await?).await
    }

    pub(crate) async fn set_skill_enabled(
        &self,
        session: &str,
        name: &str,
        enabled: bool,
    ) -> Result<String, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static base");
        url.path_segments_mut().expect("base URL").extend([
            "v1",
            "sessions",
            session,
            "skills",
            name,
            if enabled { "enable" } else { "disable" },
        ]);
        let response = self.send(self.post(url.as_str(), &json!({}))).await?;
        let _: serde_json::Value = decode(response).await?;
        Ok(format!(
            "skill {name} preference is {}; restart Otto to apply the active catalog (activation also requires [skills].enabled = true)",
            if enabled { "enabled" } else { "disabled" }
        ))
    }

    /// `POST /v1/sessions/{id}/reflection/skills/{name}/revert`.
    pub async fn revert_generated_skill(
        &self,
        session_id: &str,
        name: &str,
    ) -> Result<String, Error> {
        let mut url = reqwest::Url::parse(BASE).expect("static Otto base URL");
        url.path_segments_mut()
            .expect("base URL supports path segments")
            .extend([
                "v1",
                "sessions",
                session_id,
                "reflection",
                "skills",
                name,
                "revert",
            ]);
        let response = self.send(self.post(url.as_str(), &json!({}))).await?;
        #[derive(Deserialize)]
        struct Reverted {
            result: String,
        }
        Ok(decode::<Reverted>(response).await?.result)
    }

    /// `GET /v1/sessions/{id}/notices?after=...`.
    pub async fn reflection_notices(
        &self,
        session_id: &str,
        after: u64,
    ) -> Result<ReflectionNotices, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/notices"))
                    .query(&[("after", after)]),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/wake-replies?after=...`.
    pub async fn wake_replies(
        &self,
        session_id: &str,
        after: u64,
    ) -> Result<ReflectionNotices, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/wake-replies"))
                    .query(&[("after", after)]),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/context`.
    pub async fn context(&self, session_id: &str) -> Result<ContextReport, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/context")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/trace`.
    pub async fn trace(&self, session_id: &str) -> Result<otto_core::session::trace::Trace, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/trace")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/trace/{span_id}`. `None` means the session or
    /// the span is not found.
    pub async fn trace_span(
        &self,
        session_id: &str,
        span_id: &str,
    ) -> Result<Option<otto_core::session::trace::SpanContent>, Error> {
        let span_id = percent_encode_segment(span_id);
        let result = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/trace/{span_id}")),
            )
            .await;
        let response = match result {
            Ok(response) => response,
            Err(Error::Http { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(decode(response).await?))
    }
}
