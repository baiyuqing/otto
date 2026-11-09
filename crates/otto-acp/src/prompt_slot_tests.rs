use super::*;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn prompt_and_wake_owners_exclude_each_other_and_reset_on_end() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let prompt = slot.begin(&parent).unwrap();
    assert!(slot.begin(&parent).is_none());
    assert!(slot.begin_wake(&parent).is_none());
    assert!(slot.wait_for_wake(&parent).is_none());
    slot.active_cancel().cancel();
    assert!(
        prompt.is_cancelled(),
        "active cancellation targets the prompt"
    );

    slot.end();
    let wake = slot.begin_wake(&parent).unwrap();
    assert!(!wake.is_cancelled(), "a new owner gets a fresh token");
    assert!(slot.begin(&parent).is_none());
    assert!(slot.begin_wake(&parent).is_none());
    slot.active_cancel().cancel();
    assert!(
        wake.is_cancelled(),
        "active cancellation also targets wakes"
    );

    slot.end();
    assert!(slot.begin(&parent).is_some());
    assert!(
        slot.wait_for_wake(&parent).is_none(),
        "prompt ownership resets wake admission"
    );
    slot.end();
    assert!(slot.begin_wake(&parent).is_some());
}

#[tokio::test]
async fn wake_waiter_claims_only_after_the_owner_releases() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let wake = slot.begin_wake(&parent).unwrap();
    let waiting = slot.wait_for_wake(&parent).unwrap();
    assert!(slot.wait_for_wake(&parent).is_none());
    let mut claim = Box::pin(slot.claim_after_wake(&waiting));
    assert!(poll_once(claim.as_mut()).is_pending());
    assert!(!wake.is_cancelled());

    slot.end();
    assert!(
        slot.begin_wake(&parent).is_none(),
        "the registered waiter excludes another wake"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), claim)
            .await
            .unwrap()
    );
    assert!(
        slot.begin(&parent).is_none(),
        "the claimed prompt now owns the slot"
    );
    assert!(
        slot.wait_for_wake(&parent).is_none(),
        "the claim becomes a prompt, not a wake"
    );
    slot.active_cancel().cancel();
    assert!(
        waiting.is_cancelled(),
        "the claim installs the waiting token"
    );
}

#[tokio::test]
async fn cancel_signals_wake_and_waiter_without_releasing_the_owner() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let wake = slot.begin_wake(&parent).unwrap();
    let waiting = slot.wait_for_wake(&parent).unwrap();
    let mut claim = Box::pin(slot.claim_after_wake(&waiting));
    assert!(poll_once(claim.as_mut()).is_pending());
    let released = slot.released.subscribe();

    assert_eq!(slot.cancel(), 0);
    assert_eq!(slot.cancel(), 0, "repeated cancellation is harmless");
    assert!(wake.is_cancelled());
    assert!(waiting.is_cancelled());
    assert!(
        !released.has_changed().unwrap(),
        "cancellation is not owner completion"
    );
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), claim)
            .await
            .unwrap()
    );
    assert!(
        slot.begin(&parent).is_none(),
        "the canceled wake still holds the slot"
    );
    assert!(slot.begin_wake(&parent).is_none());

    let replacement = slot
        .wait_for_wake(&parent)
        .expect("the canceled waiter was removed");
    let mut replacement_claim = Box::pin(slot.claim_after_wake(&replacement));
    assert!(poll_once(replacement_claim.as_mut()).is_pending());
    slot.end();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), replacement_claim)
            .await
            .unwrap()
    );
    assert!(!replacement.is_cancelled());
}

#[tokio::test]
async fn a_parent_canceled_waiter_is_removed_without_ending_the_wake() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let waiting_parent = CancellationToken::new();
    let wake = slot.begin_wake(&parent).unwrap();
    let waiting = slot.wait_for_wake(&waiting_parent).unwrap();
    waiting_parent.cancel();
    assert!(
        !tokio::time::timeout(Duration::from_secs(2), slot.claim_after_wake(&waiting))
            .await
            .unwrap()
    );
    assert!(!wake.is_cancelled());
    assert!(slot.begin(&parent).is_none());
    let replacement = slot.wait_for_wake(&parent).unwrap();
    slot.end();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), slot.claim_after_wake(&replacement))
            .await
            .unwrap()
    );
}

#[test]
fn repeated_prompt_cancellation_retains_ownership_until_end() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let prompt = slot.begin(&parent).unwrap();
    assert_eq!(slot.cancel(), 0);
    assert_eq!(slot.cancel(), 0);
    assert!(prompt.is_cancelled());
    assert!(slot.begin(&parent).is_none());
    assert!(slot.begin_wake(&parent).is_none());
    assert!(slot.wait_for_wake(&parent).is_none());
    slot.end();
    assert!(!slot.begin(&parent).unwrap().is_cancelled());
}

#[test]
fn queue_capacity_and_cancel_count_preserve_the_drain_owner() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let enqueue = |input_id: String| {
        slot.enqueue_with_provenance("queued".into(), (None, None), Some(input_id), None)
    };
    assert_eq!(enqueue("first".into()), Ok(true));
    assert_eq!(enqueue("first".into()), Ok(false));
    for index in 1..ACP_QUEUE_LIMIT {
        assert_eq!(enqueue(format!("queued_{index}")), Ok(false));
    }
    assert!(enqueue("overflow".into()).is_err());
    // Capacity is checked before deduplication, including a restored duplicate.
    assert!(enqueue("first".into()).is_err());
    assert_eq!(
        slot.cancel(),
        ACP_QUEUE_LIMIT,
        "deduplicated work is counted only once"
    );
    assert_eq!(slot.cancel(), 0);
    assert!(
        slot.begin(&parent).is_none(),
        "clearing the queue does not end its drain owner"
    );
    assert!(slot.begin_wake(&parent).is_none());
    assert_eq!(
        enqueue("after_cancel".into()),
        Ok(false),
        "the existing worker owns new work"
    );
    assert_eq!(slot.cancel(), 1);
}

#[tokio::test]
async fn followup_handles_hold_wake_handoff_until_fifo_admission_and_completion() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    slot.begin_wake(&parent).unwrap();
    let waiting = slot.wait_for_wake(&parent).unwrap();
    let (first, _) = slot.begin_followup(&parent).unwrap();
    let (second, _) = slot.begin_followup(&parent).unwrap();
    let mut second_admission = Box::pin(slot.wait_followup_admission(second));
    assert!(poll_once(second_admission.as_mut()).is_pending());
    let mut claim = Box::pin(slot.claim_after_wake(&waiting));
    slot.end();
    assert!(
        poll_once(claim.as_mut()).is_pending(),
        "followups hold the idle activity"
    );
    assert!(slot.begin(&parent).is_none());
    assert!(slot.begin_wake(&parent).is_none());

    tokio::time::timeout(Duration::from_secs(2), slot.wait_followup_admission(first))
        .await
        .unwrap();
    slot.finish_followup_admission(first);
    tokio::time::timeout(Duration::from_secs(2), second_admission)
        .await
        .unwrap();
    slot.finish_followup_admission(second);
    assert!(
        poll_once(claim.as_mut()).is_pending(),
        "admission does not complete a followup"
    );
    slot.end_followup(first);
    assert!(
        poll_once(claim.as_mut()).is_pending(),
        "the second handle still holds admission"
    );
    slot.end_followup(second);
    assert!(
        tokio::time::timeout(Duration::from_secs(2), claim)
            .await
            .unwrap()
    );
}

#[test]
fn canceled_followup_handles_block_new_owners_until_each_handle_ends() {
    let slot = PromptSlot::default();
    let parent = CancellationToken::new();
    let (first, first_token) = slot.begin_followup(&parent).unwrap();
    let (second, second_token) = slot.begin_followup(&parent).unwrap();
    assert_eq!(slot.cancel(), 0);
    assert_eq!(slot.cancel(), 0);
    assert!(first_token.is_cancelled());
    assert!(second_token.is_cancelled());
    assert!(slot.begin(&parent).is_none());
    assert!(slot.begin_wake(&parent).is_none());
    slot.end_followup(first);
    assert!(
        slot.begin(&parent).is_none(),
        "the second canceled handle remains owned"
    );
    slot.end_followup(second);
    assert!(slot.begin_wake(&parent).is_some());
}
