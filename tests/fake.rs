use ouro_console::{client::*, fake::FakeControl};
use serde_json::json;
fn req(id: &str) -> Request {
    Request {
        id: id.into(),
        actor: "operator".into(),
        reason: "investigate".into(),
    }
}
#[tokio::test]
async fn owned_tree_cancellation_keeps_terminal_evidence_and_independent_runs() {
    let c = FakeControl::new();
    c.cancel_tree(&req("cancel"), "run-002").await.unwrap();
    for id in ["run-002", "run-007", "run-008"] {
        let i = c.inspect(id).await.unwrap().unwrap();
        assert_eq!(i.run.state, RunState::Cancelled);
        assert!(i.lease.is_none() && i.waits.is_empty());
    }
    assert_eq!(
        c.inspect("run-009").await.unwrap().unwrap().run.state,
        RunState::Completed
    );
    assert_eq!(
        c.inspect("run-001").await.unwrap().unwrap().run.state,
        RunState::Ready
    );
    assert!(
        !c.inspect("run-002")
            .await
            .unwrap()
            .unwrap()
            .calls
            .is_empty()
    );
    let workers = c.workers(None, 10).await.unwrap().items;
    assert_eq!(workers[0].held, 0);
    assert!(!workers[0].stale && workers[1].stale);
}
#[tokio::test]
async fn commands_replay_original_reply_and_reject_conflicting_ids() {
    let c = FakeControl::new();
    let r = req("stop");
    assert_eq!(
        c.emergency_stop(&r, true).await.unwrap(),
        Reply::Changed(true)
    );
    assert_eq!(
        c.emergency_stop(&r, true).await.unwrap(),
        Reply::Changed(true)
    );
    assert!(matches!(
        c.emergency_stop(&r, false).await,
        Err(ControlError::Conflict(_))
    ));
    let mut changed = r.clone();
    changed.reason = "changed".into();
    assert!(matches!(
        c.emergency_stop(&changed, true).await,
        Err(ControlError::Conflict(_))
    ));
    assert_eq!(c.journal(None, None, 100).await.unwrap().items.len(), 2);
    assert_eq!(
        c.entry("stop").await.unwrap().unwrap().reply,
        Reply::Changed(true)
    );
    let mut blank = req("blank");
    blank.reason = " ".into();
    assert!(c.emergency_stop(&blank, true).await.is_err());
}
#[tokio::test]
async fn pause_resume_preserves_epoch_breaker_counts_and_automatic_trip() {
    let c = FakeControl::new();
    let before = c.definition("cleanup").await.unwrap().unwrap();
    c.resume(&req("resume"), "cleanup").await.unwrap();
    let after = c.definition("cleanup").await.unwrap().unwrap();
    assert_eq!(before.epoch, after.epoch);
    assert_eq!(before.faults, after.faults);
    assert!(after.tripped.is_none());
    assert_eq!(
        c.pause(&req("pause-tripped"), "archive", "test")
            .await
            .unwrap(),
        Reply::Changed(false)
    );
    assert!(matches!(
        c.resume(&req("auto"), "archive").await,
        Err(ControlError::Conflict(_))
    ));
    assert!(c.entry("auto").await.unwrap().is_none());
}
#[tokio::test]
async fn bounded_pages_allow_empty_matches_and_bind_cursors_to_query() {
    let c = FakeControl::new();
    let filter = Runs {
        state: Some(RunState::Running),
        ..Default::default()
    };
    let mut cursor = None;
    let mut ids = vec![];
    let mut saw_empty = false;
    loop {
        let page = c.list(&filter, cursor.as_ref(), 1).await.unwrap();
        saw_empty |= page.items.is_empty() && page.next.is_some();
        ids.extend(page.items.into_iter().map(|r| r.id));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert!(saw_empty);
    assert_eq!(ids, vec!["run-002"]);
    let cursor = c.list(&filter, None, 1).await.unwrap().next.unwrap();
    assert!(c.list(&Runs::default(), Some(&cursor), 1).await.is_err());
    assert!(
        c.list(&filter, Some(&Cursor("bogus".into())), 1)
            .await
            .is_err()
    );
    for limit in [0, 1001] {
        assert!(c.list(&filter, None, limit).await.is_err());
    }
    let children = c
        .list(
            &Runs {
                parent: Some("run-002".into()),
                ..Default::default()
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(children.items.len(), 2);
}
#[tokio::test]
async fn history_and_counts_are_bounded_and_filtered() {
    let c = FakeControl::new();
    assert!(c.history(&Runs::default(), None, 100).await.is_err());
    assert!(c.counts(&Runs::default(), false, None, 100).await.is_err());
    let filter = Runs {
        definition: Some("review".into()),
        since_ms: Some(0),
        before_ms: Some(now_ms() + 1000),
        ..Default::default()
    };
    let counts = c.counts(&filter, false, None, 1).await.unwrap();
    assert!(counts.next.is_some());
    assert_eq!(counts.states.values().sum::<usize>(), 1);
    let history = c.history(&filter, None, 100).await.unwrap();
    assert_eq!(history.items.len(), 2);
    assert!(history.items.iter().all(|r| r.state.terminal()));
    let counts = c.counts(&filter, true, None, 100).await.unwrap();
    assert_eq!(counts.states.get("completed"), Some(&1));
    assert_eq!(counts.states.get("cancelled"), Some(&1));
}
#[tokio::test]
async fn responses_validate_exact_wait_and_replay_event_identity_after_close() {
    let c = FakeControl::new();
    let event = json!({"source":"tests","id":"signal","type":"transform.signal","subject":"sample-42","data":{"ok":true}});
    let mut wrong = event.clone();
    wrong["subject"] = json!("other");
    assert!(
        c.respond(&req("wrong"), "run-007", "/signal", wrong)
            .await
            .is_err()
    );
    assert!(c.entry("wrong").await.unwrap().is_none());
    c.respond(&req("answer"), "run-007", "/signal", event.clone())
        .await
        .unwrap();
    assert_eq!(
        c.inspect("run-007").await.unwrap().unwrap().run.state,
        RunState::Ready
    );
    assert_eq!(
        c.respond(
            &req("retry-new-request"),
            "run-007",
            "/signal",
            event.clone()
        )
        .await
        .unwrap(),
        Reply::Responded(Some("run-007".into()))
    );
    assert_eq!(
        c.inspect("run-007").await.unwrap().unwrap().mailbox.len(),
        1
    );
    let mut changed = event;
    changed["data"] = json!({"ok":false});
    assert!(matches!(
        c.respond(&req("conflict"), "run-007", "/signal", changed)
            .await,
        Err(ControlError::Conflict(_))
    ));
    assert_eq!(
        c.journal(Some("run-007"), None, 100)
            .await
            .unwrap()
            .items
            .len(),
        2
    );
}
#[tokio::test]
async fn waits_filters_and_paging_follow_continuations() {
    let c = FakeControl::new();
    let first = c.waits(&Waits::default(), None, 1).await.unwrap();
    assert_eq!(first.items.len(), 1);
    assert!(first.next.is_some());
    let second = c
        .waits(&Waits::default(), first.next.as_ref(), 1)
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert!(second.next.is_none());
    let filter = Waits {
        event_type: Some("review.decision".into()),
        definition: Some("review".into()),
        ..Default::default()
    };
    let page = c.waits(&filter, None, 100).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].run, "run-003");
    assert!(c.waits(&filter, first.next.as_ref(), 1).await.is_err());
}
