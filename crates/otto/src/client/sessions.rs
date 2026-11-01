//! Session, turn, model, fork and rename calls.

use super::*;

impl Client {
    /// `GET /healthz`.
    pub async fn healthz(&self) -> Result<(), Error> {
        self.send(self.http.get(format!("{BASE}/healthz")))
            .await
            .map(drop)
    }

    /// `POST /v1/sessions`: a new session in `workspace`, or with `resume`
    /// the stored session of that id. Without `workspace`, serve uses its
    /// startup workspace for a new session and searches every loaded
    /// workspace for a resumed one. Returns the session id.
    pub async fn open_session(
        &self,
        workspace: Option<&str>,
        resume: Option<&str>,
    ) -> Result<String, Error> {
        let mut body = json!({});
        if let Some(workspace) = workspace {
            body["workspace"] = json!(workspace);
        }
        if let Some(id) = resume {
            body["resume"] = json!(id);
        }
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions"), &body))
            .await?;
        #[derive(Deserialize)]
        struct Opened {
            id: String,
        }
        Ok(decode::<Opened>(response).await?.id)
    }

    /// `GET /v1/sessions?workspace=...`.
    pub async fn list_sessions(&self, workspace: &str) -> Result<Vec<SessionRow>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions"))
                    .query(&[("workspace", workspace)]),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            sessions: Vec<SessionRow>,
        }
        Ok(decode::<Listed>(response).await?.sessions)
    }

    /// `GET /v1/sessions/{id}/history`; with `before_turn`, only the
    /// messages that existed when that turn started.
    pub async fn history(
        &self,
        session_id: &str,
        before_turn: Option<&str>,
    ) -> Result<Vec<Message>, Error> {
        let mut request = self
            .http
            .get(format!("{BASE}/v1/sessions/{session_id}/history"));
        if let Some(turn_id) = before_turn {
            request = request.query(&[("before_turn", turn_id)]);
        }
        decode(self.send(request).await?).await
    }

    /// `POST /v1/sessions/{id}/turns` with `queue: true` and origin
    /// `terminal`, for the attach TUI. `image` is an image block; its data
    /// and MIME type go in the `image` field.
    pub async fn start_turn(
        &self,
        session_id: &str,
        text: &str,
        image: Option<&Block>,
    ) -> Result<TurnStream, Error> {
        self.start_turn_with_channel(
            session_id,
            text,
            image,
            None,
            Some(&Origin::terminal()),
            None,
            None,
        )
        .await
    }

    /// Starts a queued turn with optional per-turn chat channel metadata and
    /// origin. Without an origin the server records the turn as `web`.
    /// `request_id` is sent as `X-Request-Id` for log correlation;
    /// `input_id` is the body's idempotency key, so a retry is not run twice.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_turn_with_channel(
        &self,
        session_id: &str,
        text: &str,
        image: Option<&Block>,
        channel: Option<Channel>,
        origin: Option<&Origin>,
        request_id: Option<&str>,
        input_id: Option<&str>,
    ) -> Result<TurnStream, Error> {
        let mut body = json!({ "text": text, "queue": true });
        if let Some(input_id) = input_id {
            body["input_id"] = json!(input_id);
        }
        if let Some(channel) = channel {
            body["channel"] = serde_json::to_value(channel).expect("Channel serializes");
        }
        if let Some(origin) = origin {
            body["origin"] = json!(origin.as_str());
        }
        if let Some(image) = image {
            body["image"] = json!({ "data": image.data, "mime_type": image.mime_type });
        }
        let mut request = self.post(&format!("{BASE}/v1/sessions/{session_id}/turns"), &body);
        if let Some(request_id) = request_id {
            request = request.header("x-request-id", request_id);
        }
        let response = self.send(request).await?;
        let turn_id = response
            .headers()
            .get(TURN_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| unreachable_text("response has no Otto-Turn-Id header"))?;
        Ok(TurnStream::new(turn_id, response))
    }

    /// `GET /v1/sessions/{id}/turns/{turn_id}/events`: the events with an
    /// index above `after`, or from the first event when `None`, then the
    /// live ones.
    pub async fn turn_events(
        &self,
        session_id: &str,
        turn_id: &str,
        after: Option<usize>,
    ) -> Result<TurnStream, Error> {
        let mut request = self.http.get(format!(
            "{BASE}/v1/sessions/{session_id}/turns/{turn_id}/events"
        ));
        if let Some(after) = after {
            request = request.query(&[("after", after)]);
        }
        let response = self.send(request).await?;
        Ok(TurnStream::new(turn_id.to_string(), response))
    }

    /// `GET /v1/status`: snapshots of every open session in serve.
    pub async fn status(&self) -> Result<StatusStream, Error> {
        let response = self
            .send(self.http.get(format!("{BASE}/v1/status")))
            .await?;
        Ok(StatusStream {
            body: SseBody::new(response),
        })
    }

    /// `GET /v1/sessions/{id}`. `session_path` is empty, `reason` is the
    /// default and `pending` is false: the wire carries none of them.
    pub async fn session(&self, session_id: &str) -> Result<crate::app::Info, Error> {
        let response = self
            .send(self.http.get(format!("{BASE}/v1/sessions/{session_id}")))
            .await?;
        let wire: SessionWire = decode(response).await?;
        Ok(wire.into_info())
    }

    /// `POST /v1/sessions/{id}/model`: run the session on another configured
    /// model selector and return the session's new info. The configuration
    /// file and every other session are unchanged.
    pub async fn switch_model(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<crate::app::Info, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/model"),
                &json!({ "model": model }),
            ))
            .await?;
        let wire: SessionWire = decode(response).await?;
        Ok(wire.into_info())
    }

    /// `GET /v1/config/providers`: the configured model selectors, sorted.
    pub async fn models(&self) -> Result<Vec<String>, Error> {
        #[derive(Deserialize)]
        struct Model {
            selector: String,
        }
        #[derive(Deserialize)]
        struct Provider {
            #[serde(default)]
            models: Vec<Model>,
        }
        #[derive(Deserialize)]
        struct Reply {
            providers: Vec<Provider>,
        }
        let response = self
            .send(self.http.get(format!("{BASE}/v1/config/providers")))
            .await?;
        let mut selectors: Vec<String> = decode::<Reply>(response)
            .await?
            .providers
            .into_iter()
            .flat_map(|provider| provider.models)
            .map(|model| model.selector)
            .collect();
        selectors.sort();
        Ok(selectors)
    }

    /// `GET /v1/sessions/{id}/fork-points`, oldest first, as (entry id,
    /// prompt) pairs.
    pub async fn fork_points(&self, session_id: &str) -> Result<Vec<(String, String)>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/fork-points")),
            )
            .await?;
        #[derive(Deserialize)]
        struct Point {
            entry_id: String,
            prompt: String,
        }
        #[derive(Deserialize)]
        struct Points {
            fork_points: Vec<Point>,
        }
        Ok(decode::<Points>(response)
            .await?
            .fork_points
            .into_iter()
            .map(|point| (point.entry_id, point.prompt))
            .collect())
    }

    /// `POST /v1/sessions/{id}/fork`: the fork's session id and the prompt it
    /// stops before.
    pub async fn fork_session(
        &self,
        session_id: &str,
        before: Option<&str>,
    ) -> Result<(String, Option<String>), Error> {
        let body = match before {
            Some(before) => json!({ "before": before }),
            None => json!({}),
        };
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions/{session_id}/fork"), &body))
            .await?;
        #[derive(Deserialize)]
        struct Forked {
            id: String,
            #[serde(default)]
            prompt: Option<String>,
        }
        let forked = decode::<Forked>(response).await?;
        Ok((forked.id, forked.prompt))
    }

    /// `PATCH /v1/sessions/{id}`.
    pub async fn rename_session(&self, session_id: &str, name: &str) -> Result<(), Error> {
        self.send(
            self.http
                .patch(format!("{BASE}/v1/sessions/{session_id}"))
                .header(CONTENT_TYPE, "application/json")
                .body(json!({ "name": name }).to_string()),
        )
        .await
        .map(drop)
    }
}
