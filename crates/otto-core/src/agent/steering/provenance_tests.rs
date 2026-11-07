use super::*;
use crate::agent::inbox::Inbox;
use crate::model::{InputDelivery, InputProvenance, InputSource};
#[test]
fn journaled_steering_keeps_identity_origin_and_delivery_on_restore() {
    let journal = Arc::new(Inbox::default());
    let steering = Arc::new(Steering::default());
    steering.set_journal(journal.clone());
    let run = steering.begin_turn("turn1", None);
    let accepted = steering
        .submit_input_with_provenance(
            steering.ticket().unwrap(),
            "same",
            Some("input1"),
            Some(InputProvenance::local(InputSource::Tui)),
        )
        .unwrap();
    let mut expected = InputProvenance::local(InputSource::Tui);
    expected.delivery = InputDelivery::Steering;
    assert_eq!(accepted.input_provenance, Some(expected));
    assert_eq!(
        journal.queued()[0].notification.input_provenance,
        Some(expected)
    );
    // A new process sees the durable journal before the previous process drops its run.
    let restored = Arc::new(Steering::default());
    restored.set_journal(journal);
    let restored_run = restored.begin_turn("turn2", Some("turn1"));
    let delivered = restored_run.take_or_close().unwrap();
    assert_eq!(delivered[0].input_id, "input1");
    assert_eq!(delivered[0].input_provenance, Some(expected));
    drop(run);
}
