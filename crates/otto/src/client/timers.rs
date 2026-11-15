//! Session timer list and cancel calls.

use super::*;
use crate::schedule::remind::StoredReminder;

impl Client {
    /// `GET /v1/sessions/{id}/timers`: the session's timers.
    pub async fn timers(&self, session_id: &str) -> Result<Vec<StoredReminder>, Error> {
        let response = self
            .send(
                self.http
                    .get(format!("{BASE}/v1/sessions/{session_id}/timers")),
            )
            .await?;
        #[derive(Deserialize)]
        struct Listed {
            timers: Vec<StoredReminder>,
        }
        Ok(decode::<Listed>(response).await?.timers)
    }

    /// `POST /v1/sessions/{id}/timers/{timer_id}/cancel`: the cancelled
    /// timer. Serve answers 404 for an unknown timer.
    pub(crate) async fn cancel_timer(
        &self,
        session_id: &str,
        timer_id: &str,
    ) -> Result<StoredReminder, Error> {
        let response = self
            .send(self.post(
                &format!("{BASE}/v1/sessions/{session_id}/timers/{timer_id}/cancel"),
                &json!({}),
            ))
            .await?;
        decode(response).await
    }
}
