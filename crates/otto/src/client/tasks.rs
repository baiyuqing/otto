//! Sub-agent task list, detail and history calls.

use super::*;

impl Client {
    /// Bounded cumulative read-only child output for one admitted session.
    pub async fn observer_snapshot(
        &self,
        session: &str,
    ) -> Result<crate::app::tasks::ObserverSnapshot, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session}/tasks/observer")),
            )
            .await?;
        decode(response).await
    }

    /// `GET /v1/sessions/{id}/tasks`: the open session's sub-agent tasks.
    /// `prompt` and `context` are empty; the wire omits them.
    pub async fn session_tasks(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::subagent::tasks::Task>, Error> {
        Ok(self
            .session_task_records(session_id)
            .await?
            .into_iter()
            .map(crate::app::tasks::from_wire)
            .collect())
    }

    /// `GET /v1/sessions/{id}/tasks` as the wire records, which is what
    /// the management reply `Tasks` carries.
    pub async fn session_task_records(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::app::tasks::Task>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/tasks")),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            tasks: Vec<crate::app::tasks::Task>,
        }
        Ok(decode::<Listed>(response).await?.tasks)
    }

    /// `GET /v1/sessions/{id}/tasks/{task_id}`: one task record; the
    /// `history` field of the reply is ignored. `None` on 404.
    pub async fn session_task_record(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<Option<crate::app::tasks::Task>, Error> {
        let result = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/tasks/{task_id}")),
            )
            .await;
        match result {
            Ok(response) => Ok(Some(decode(response).await?)),
            Err(Error::Http { status: 404, .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// `POST /v1/sessions/{id}/tasks/{task_id}/cancel`: the task after the
    /// cancel. Serve answers 409 `task_done` for a finished task and 404 for
    /// an unknown one.
    pub async fn cancel_task(
        &self,
        session_id: &str,
        task_id: &str,
    ) -> Result<crate::app::tasks::Task, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/tasks/{task_id}/cancel"),
                &json!({}),
            ))
            .await?;
        decode(response).await
    }

    /// `GET /v1/tasks`, from `tasks.db`; only the `Some` fields of `query`
    /// are sent.
    pub async fn tasks(&self, query: &ListQuery) -> Result<ListResult, Error> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(parent_session) = &query.parent_session {
            params.push(("parent_session", parent_session.clone()));
        }
        if let Some(status) = &query.status {
            params.push(("status", status.clone()));
        }
        if let Some(workspace) = &query.workspace {
            params.push(("workspace", workspace.clone()));
        }
        if let Some(limit) = query.limit {
            params.push(("limit", limit.to_string()));
        }
        if let Some(before) = &query.before {
            params.push(("before", before.clone()));
        }
        let response = self
            .send(self.http.get(format!("{BASE}/v1/tasks")).query(&params))
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            tasks: Vec<TaskRow>,
            #[serde(default)]
            next_before: String,
        }
        let listed: Listed = decode(response).await?;
        Ok(ListResult {
            tasks: listed.tasks,
            next_before: listed.next_before,
        })
    }

    /// `GET /v1/tasks/{parent_session}/{task_id}`; `None` on 404.
    pub async fn task(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<TaskRow>, Error> {
        Ok(self
            .task_detail(parent_session, task_id)
            .await?
            .map(|detail| detail.task))
    }

    /// `GET /v1/tasks/{parent_session}/{task_id}`, including its server-read
    /// child transcript. `None` means the task row is not found.
    pub async fn task_detail(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Option<TaskDetail>, Error> {
        let result = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/tasks/{parent_session}/{task_id}")),
            )
            .await;
        let response = match result {
            Ok(response) => response,
            Err(Error::Http { status: 404, .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(decode(response).await?))
    }

    /// The server-read transcript for a recorded sub-agent task.
    /// A missing task or transcript is returned as a 404 error.
    pub async fn task_history(
        &self,
        parent_session: &str,
        task_id: &str,
    ) -> Result<Vec<Message>, Error> {
        let Some(detail) = self.task_detail(parent_session, task_id).await? else {
            return Err(Error::Http {
                status: 404,
                code: "not_found".into(),
                message: "task not found".into(),
            });
        };
        if detail.transcript_missing {
            return Err(Error::Http {
                status: 404,
                code: "transcript_missing".into(),
                message: "task transcript is missing or unavailable".into(),
            });
        }
        Ok(detail.history)
    }
}
