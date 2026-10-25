//! Read-only delivery task status. Delivery tasks deliberately do not share
//! the ordinary sub-agent task schema or routes.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Serialize;

use super::{Server, internal_error, json_response, not_found};

/// Capability-free projection of a delivery task. In particular it excludes
/// source/target/worktree paths, sessions other than the scoped source session,
/// sandbox details and every execution handle.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Status {
    pub approval_id: String,
    pub state: String,
    pub result: String,
    pub error: String,
}

#[derive(Serialize)]
struct ResponseBody {
    delivery: Status,
}

pub async fn get(
    State(server): State<Arc<Server>>,
    Path((source_session, approval_id)): Path<(String, String)>,
) -> Response {
    match server
        .factory
        .delivery_status(&source_session, &approval_id)
    {
        Ok(Some(status)) => json_response(StatusCode::OK, &ResponseBody { delivery: status }),
        Ok(None) => not_found("delivery not found"),
        Err(error) => internal_error(&server.log, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use otto_core::session::ListResult;

    struct Factory;

    #[async_trait::async_trait]
    impl super::super::Factory for Factory {
        async fn create(&self, _workspace: Option<&str>) -> Result<crate::app::Controller, String> {
            Err("unused".into())
        }

        async fn open(
            &self,
            _id: &str,
            _workspace: Option<&str>,
        ) -> Result<crate::app::Controller, String> {
            Err("unused".into())
        }

        async fn list(&self, _workspace: Option<&str>) -> Option<Result<ListResult, String>> {
            Some(Ok(ListResult::default()))
        }

        fn delivery_status(
            &self,
            source_session: &str,
            approval_id: &str,
        ) -> Result<Option<Status>, String> {
            Ok(
                (source_session == "source-session" && approval_id == "approval-1").then(|| {
                    Status {
                        approval_id: approval_id.into(),
                        state: "succeeded".into(),
                        result: "completed safely".into(),
                        error: String::new(),
                    }
                }),
            )
        }

        async fn workspaces(&self) -> super::super::WorkspaceList {
            super::super::WorkspaceList {
                startup: String::new(),
                roots: Vec::new(),
                loaded: Vec::new(),
            }
        }

        async fn load_workspace(
            &self,
            _path: &str,
        ) -> Result<(super::super::WorkspaceInfo, bool), super::super::WorkspaceLoadError> {
            Err(super::super::WorkspaceLoadError::NotAdmitted(
                "unused".into(),
            ))
        }

        async fn remove_workspace(
            &self,
            _path: &str,
        ) -> Result<(), super::super::WorkspaceRemoveError> {
            Err(super::super::WorkspaceRemoveError::NotFound)
        }

        async fn workflow_controller(
            &self,
            _workspace: Option<&str>,
        ) -> Option<Arc<crate::workflow::Controller>> {
            None
        }

        async fn workflow_controllers(&self) -> Vec<Arc<crate::workflow::Controller>> {
            Vec::new()
        }
    }

    fn server() -> Arc<Server> {
        Server::new(super::super::Options {
            info: super::super::Info::default(),
            factory: Arc::new(Factory),
            config_path: Default::default(),
            token: String::new(),
            remote_listener: false,
            logger: None,
            workflows: None,
            schedule: None,
            session_idle_timeout: None,
            max_open_sessions: None,
        })
    }

    #[tokio::test]
    async fn status_is_scoped_and_contains_no_capability_fields() {
        let response = get(
            State(server()),
            Path(("source-session".into(), "approval-1".into())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let delivery = &body["delivery"];
        assert_eq!(delivery["approval_id"], "approval-1");
        assert_eq!(delivery["state"], "succeeded");
        assert_eq!(delivery["result"], "completed safely");
        assert!(delivery.get("source_workspace").is_none());
        assert!(delivery.get("target_repository").is_none());
        assert!(delivery.get("worktree").is_none());
        assert!(delivery.get("session_id").is_none());

        let response = get(
            State(server()),
            Path(("other-session".into(), "approval-1".into())),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
