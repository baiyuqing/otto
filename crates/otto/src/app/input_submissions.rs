//! Recoverable local submission intent, separate from proof of execution.
use super::Controller;
use otto_core::agent::redactor::Redactor;
use otto_core::model::{InputDelivery, InputProvenance, InputSource};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Submission {
    pub id: String,
    pub session: String,
    pub workspace: String,
    pub text: String,
    pub provenance: InputProvenance,
    pub state: String,
}

impl Controller {
    pub(crate) fn input_submissions(&self) -> Result<Vec<Submission>, String> {
        let session = self.current_session_opt().ok_or("session is closed")?;
        let info = self.info();
        let mut records: Vec<Submission> = Vec::new();
        for encoded in session.handle().input_submission_events()? {
            let record: Submission = serde_json::from_str(&encoded)
                .map_err(|error| format!("read input recovery records: {error}"))?;
            if record.session != info.session_id || record.workspace != info.workspace {
                continue;
            }
            if let Some(previous) = records.iter_mut().find(|previous| previous.id == record.id) {
                *previous = record;
            } else {
                records.push(record);
            }
        }
        Ok(records)
    }

    pub(crate) fn prepare_input(
        &self,
        text: &str,
        delivery: InputDelivery,
    ) -> Result<Submission, String> {
        let id = crate::runtime::builder::random_id().map_err(|error| error.to_string())?;
        let mut provenance = InputProvenance::local(InputSource::Tui);
        provenance.delivery = delivery;
        let info = self.info();
        let record = Submission {
            id,
            session: info.session_id,
            workspace: info.workspace,
            text: text.into(),
            provenance,
            state: "pending".into(),
        };
        self.save_input_submission(&record)?;
        Ok(record)
    }

    pub(crate) fn update_input_submission(
        &self,
        id: &str,
        state: &str,
        delivery: Option<InputDelivery>,
    ) -> Result<(), String> {
        let mut records = self.input_submissions()?;
        let record = records
            .iter_mut()
            .find(|record| record.id == id)
            .ok_or("input ID is not in this session")?;
        record.state = state.into();
        if let Some(delivery) = delivery {
            record.provenance.delivery = delivery;
        }
        self.save_input_submission(record)
    }

    fn save_input_submission(&self, record: &Submission) -> Result<(), String> {
        let session = self.current_session_opt().ok_or("session is closed")?;
        let info = self.info();
        if record.session != info.session_id || record.workspace != info.workspace {
            return Err("session changed; input recovery record was not written".into());
        }
        let encoded = serde_json::to_string(record).map_err(|error| error.to_string())?;
        let safe = Redactor::new(&self.builder.secret_values(None))
            .redact_record_json(&encoded)
            .get()
            .to_owned();
        session
            .handle()
            .append_input_submissions(&safe)
            .map_err(|error| format!("save input recovery record: {error}"))
    }
}
