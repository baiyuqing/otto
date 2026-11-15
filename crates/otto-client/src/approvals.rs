//! Approval, permission, input admission and steering calls.

use super::*;

impl Client {
    #[allow(clippy::too_many_arguments)]
    pub async fn shared_approval(
        &self,
        method: &str,
        session: &str,
        id: &str,
        revision: u64,
        option: &str,
        feedback: &str,
        after: i64,
    ) -> Result<Value, Error> {
        let response = match method {
            "_otto/approvals/list" => self.send(self.http.get(format!("{BASE}/v1/approvals")).query(&[("session_id",session)])).await?,
            "_otto/approvals/get" => self.send(self.http.get(format!("{BASE}/v1/approvals/{id}")).query(&[("session_id",session)])).await?,
            "_otto/approvals/watch" => self.send(self.http.get(format!("{BASE}/v1/sessions/{session}/approval-events")).query(&[("session_id",session),("after",&after.to_string())])).await?,
            _ => self.send(self.post(&format!("{BASE}/v1/approvals/{id}/{}",if method.ends_with("withdraw") {"withdraw"} else {"decision"}),
                &json!({"session_id":session,"revision":revision,"option_id":option,"feedback":feedback}))).await?,
        };
        serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
            .map_err(unreachable_error)
    }

    /// Reads the session's approval mode, or sets it when `mode` is given.
    /// `None` keeps the older server's boolean reading: a response without
    /// `mode` means AUTO when `automatic` is true and MANUAL otherwise.
    pub async fn approval_mode(
        &self,
        session: &str,
        mode: Option<otto_core::approval::ApprovalMode>,
    ) -> Result<otto_core::approval::ApprovalMode, Error> {
        self.approval_mode_response(session, mode)
            .await
            .map(|(mode, _)| mode)
    }

    async fn approval_mode_response(
        &self,
        session: &str,
        mode: Option<otto_core::approval::ApprovalMode>,
    ) -> Result<
        (
            otto_core::approval::ApprovalMode,
            Option<otto_core::approval::ApprovalPolicy>,
        ),
        Error,
    > {
        let url = format!("{BASE}/v1/sessions/{session}/approval-mode");
        let request = match mode {
            Some(mode) => self
                .http
                .put(url)
                .header(CONTENT_TYPE, "application/json")
                .body(json!({"mode":mode.as_str()}).to_string()),
            None => self.http.get(url),
        };
        #[derive(Deserialize)]
        struct Mode {
            #[serde(default)]
            automatic: bool,
            #[serde(default)]
            mode: Option<otto_core::approval::ApprovalMode>,
            #[serde(default)]
            policy: Option<otto_core::approval::ApprovalPolicy>,
        }
        let response = self.send(request).await?;
        let body: Mode =
            serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
                .map_err(unreachable_error)?;
        Ok((
            body.mode.unwrap_or(if body.automatic {
                otto_core::approval::ApprovalMode::Auto
            } else {
                otto_core::approval::ApprovalMode::Manual
            }),
            body.policy,
        ))
    }

    pub async fn approval_policy(
        &self,
        session: &str,
        update: Option<(
            u64,
            otto_core::approval::ApprovalPolicy,
            otto_core::approval::ApprovalReviewer,
        )>,
    ) -> Result<otto_core::approval::ApprovalSettings, Error> {
        let url = format!("{BASE}/v1/sessions/{session}/approval-policy");
        let request = match update {
            Some((revision, policy, reviewer)) => self
                .http
                .put(url)
                .header(CONTENT_TYPE, "application/json")
                .body(
                    json!({"expected_revision":revision,"policy":policy,"reviewer":reviewer})
                        .to_string(),
                ),
            None => self.http.get(url),
        };
        #[derive(Deserialize)]
        struct Body {
            policy: otto_core::approval::ApprovalPolicy,
            reviewer: otto_core::approval::ApprovalReviewer,
            revision: u64,
        }
        let response = self.send(request).await?;
        let body: Body =
            serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
                .map_err(unreachable_error)?;
        Ok(otto_core::approval::ApprovalSettings {
            policy: body.policy,
            reviewer: body.reviewer,
            revision: body.revision,
        })
    }

    pub async fn approval_activity(
        &self,
        session: &str,
    ) -> Result<otto::approval::ApprovalActivity, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/approvals"))
                    .query(&[("session_id", session)]),
            )
            .await?;
        #[derive(Deserialize)]
        struct Body {
            requests: Vec<otto_core::approval::Request>,
            cursor: i64,
            #[serde(default)]
            reviewing: Vec<otto_core::approval::Request>,
            #[serde(default)]
            recent_decisions: Vec<otto_core::approval::Request>,
            #[serde(default)]
            counts: Option<otto::approval::ApprovalCounts>,
        }
        let body: Body = decode(response).await?;
        let requests: Vec<_> = body
            .requests
            .into_iter()
            .filter(otto_core::approval::Request::is_human_pending)
            .collect();
        let counts = body.counts.unwrap_or(otto::approval::ApprovalCounts {
            human_pending: requests.len(),
            reviewing: body.reviewing.len(),
            application_failed: 0,
        });
        Ok(otto::approval::ApprovalActivity {
            requests,
            cursor: body.cursor,
            reviewing: body.reviewing,
            recent_decisions: body.recent_decisions,
            counts,
        })
    }

    pub async fn permissions_command(&self, session: &str, args: &str) -> Result<String, Error> {
        let current = self.approval_policy(session, None).await?;
        let settings = match otto::app::parse_permissions_command(args, current)
            .map_err(Error::Unreachable)?
        {
            Some(otto::app::PermissionsChange::Preset(mode)) => {
                self.approval_mode_response(session, Some(mode)).await?;
                self.approval_policy(session, None).await?
            }
            Some(otto::app::PermissionsChange::Settings(policy, reviewer)) => {
                self.approval_policy(session, Some((current.revision, policy, reviewer)))
                    .await?
            }
            None => current,
        };
        let activity = self.approval_activity(session).await?;
        Ok(format!(
            "{}\n{} human pending · {} reviewing · {} application failed",
            otto::app::format_permissions(settings),
            activity.counts.human_pending,
            activity.counts.reviewing,
            activity.counts.application_failed
        ))
    }

    /// `POST /v1/sessions/{id}/approvals/{approval_id}` with
    /// `{"decision":"allow","always":true}`: allows the waiting command and
    /// excludes it from the sandbox. Returns the follow-up prompt the caller
    /// submits so the model retries the command.
    pub async fn approve_always(
        &self,
        session_id: &str,
        approval_id: &str,
    ) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct Decided {
            #[serde(default)]
            followup: String,
        }
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/approvals/{approval_id}"),
                &json!({ "decision": "allow", "always": true }),
            ))
            .await?;
        Ok(decode::<Decided>(response).await?.followup)
    }

    pub async fn approval_message(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<Option<otto::app::approval_control::ControlReply>, Error> {
        self.approval_message_with_channel(session_id, text, None)
            .await
    }

    pub(crate) async fn approval_message_with_channel(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
    ) -> Result<Option<otto::app::approval_control::ControlReply>, Error> {
        self.approval_message_with_identity(session_id, text, channel, None)
            .await
    }

    pub async fn approval_message_with_identity(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
        input_id: Option<&str>,
    ) -> Result<Option<otto::app::approval_control::ControlReply>, Error> {
        let mut body = json!({"text": text});
        if let Some(id) = input_id {
            body["input_id"] = json!(id);
        }
        if let Some(source) = self.client_source {
            body["client_source"] = json!(source);
        }

        if let Some(channel) = channel {
            body["channel"] = serde_json::to_value(channel).expect("Channel serializes");
        }
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/approvals/message"),
                &body,
            ))
            .await?;
        serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
            .map_err(unreachable_error)
    }

    /// `POST /v1/sessions/{id}/inputs`. Deterministically steers an active
    /// task, reports idle for an explicit new turn, and never queues silently.
    pub async fn admit_input(
        &self,
        session_id: &str,
        text: &str,
        input_id: Option<&str>,
    ) -> Result<ActiveInputAdmission, Error> {
        let mut body = json!({"text": text});
        if let Some(source) = self.client_source {
            body["client_source"] = json!(source);
        }
        if let Some(input_id) = input_id {
            body["input_id"] = json!(input_id);
        }
        let response = self
            .send(self.post(&format!("{BASE}/v1/sessions/{session_id}/inputs"), &body))
            .await?;
        decode(response).await
    }

    /// `POST /v1/sessions/{id}/tasks/steer`. Accepts guidance only for the
    /// active task in this session. This explicit compatibility endpoint is
    /// intentionally stricter than [`Self::admit_input`].
    pub async fn steer(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<otto_core::agent::steering::Message, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/tasks/steer"),
                &json!({"text": text, "client_source": self.client_source}),
            ))
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/tasks/steer`: transient statuses for guidance
    /// accepted by the active run. These records disappear after the run ends.
    pub async fn steering_messages(
        &self,
        session_id: &str,
    ) -> Result<Vec<otto_core::agent::steering::Message>, Error> {
        decode(
            self.send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/tasks/steer")),
            )
            .await?,
        )
        .await
    }

    /// Withdraw one queued steer before delivery at the next safe boundary.
    pub async fn withdraw_steering(
        &self,
        session_id: &str,
        message_id: u64,
    ) -> Result<otto_core::agent::steering::Message, Error> {
        decode(
            self.send(self.http.delete(format!(
                "{BASE}/v1/sessions/{session_id}/tasks/steer/{message_id}"
            )))
            .await?,
        )
        .await
    }

    /// `POST /v1/sessions/{id}/tasks/steer/interrupt`: the number of queued
    /// guidance messages the run will deliver.
    pub async fn interrupt_steering(&self, session_id: &str) -> Result<u32, Error> {
        #[derive(Deserialize)]
        struct Interrupted {
            queued_guidance: u32,
        }
        let response = self
            .send(self.http.post(format!(
                "{BASE}/v1/sessions/{session_id}/tasks/steer/interrupt"
            )))
            .await?;
        Ok(decode::<Interrupted>(response).await?.queued_guidance)
    }

    /// `POST /v1/sessions/{id}/tasks/control`. `None` means the caller should
    /// route the message as an ordinary prompt.
    #[cfg(test)]
    pub async fn control_message_with_channel(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
    ) -> Result<Option<otto::app::approval_control::ControlReply>, Error> {
        self.control_message_with_identity(session_id, text, channel, None)
            .await
    }

    pub async fn control_message_with_identity(
        &self,
        session_id: &str,
        text: &str,
        channel: Option<Channel>,
        input_id: Option<&str>,
    ) -> Result<Option<otto::app::approval_control::ControlReply>, Error> {
        let mut body = json!({"text": text});
        if let Some(id) = input_id {
            body["input_id"] = json!(id);
        }
        if let Some(source) = self.client_source {
            body["client_source"] = json!(source);
        }

        if let Some(channel) = channel {
            body["channel"] = serde_json::to_value(channel).expect("Channel serializes");
        }
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/tasks/control"),
                &body,
            ))
            .await?;
        serde_json::from_slice(&response.bytes().await.map_err(unreachable_error)?)
            .map_err(unreachable_error)
    }
}
