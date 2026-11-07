//! Session compaction, context, trace, skills and wake replies.

use super::*;

impl Client {
    pub async fn memory_command(
        &self,
        session: &str,
        name: &str,
        args: &str,
    ) -> Result<String, Error> {
        let response = self
            .send(
                self.http
                    .post(format!("{BASE}/v1/sessions/{session}/local-commands"))
                    .header("content-type", "application/json")
                    .body(serde_json::json!({"name": name, "args": args}).to_string()),
            )
            .await?;
        let value: serde_json::Value = decode(response).await?;
        Ok(value
            .get("output")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned())
    }

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

    pub async fn wake_replies(&self, session_id: &str, after: u64) -> Result<WakeReplies, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/wake-replies"))
                    .query(&[("after", after)]),
            )
            .await?;
        decode(response).await
    }

    pub async fn context(&self, session_id: &str) -> Result<ContextReport, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/context")),
            )
            .await?;
        decode(response).await
    }

    pub async fn trace(&self, session_id: &str) -> Result<otto_core::session::trace::Trace, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/trace")),
            )
            .await?;
        decode(response).await
    }

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
}
