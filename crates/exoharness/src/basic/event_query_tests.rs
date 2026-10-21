use super::*;

#[tokio::test]
async fn event_pages_skip_old_blobs_and_order_overlapping_batches_before_limiting() -> Result<()> {
    let store = BasicObjectStore::in_memory();
    let directory = Path::new("thread");
    let mut ids = (0..25).map(|_| Uuid7::now()).collect::<Vec<_>>();
    ids.sort();
    let thread_id = Uuid7::now();
    let session = Uuid7::now();
    let make = |id, error| Event {
        id,
        thread_id,
        session_id: Some(session),
        turn_id: None,
        created_at: id.timestamp().unwrap(),
        data: if error {
            EventData::Error {
                message: "test".into(),
                metadata: None,
            }
        } else {
            EventData::TurnStarted { user_id: None }
        },
    };
    // These blobs must not be decoded for a page between the cursor and limit.
    store
        .put_bytes(
            directory.join("events").join(format!("{}.json", ids[0])),
            b"unread old blob".to_vec(),
        )
        .await?;
    store
        .put_json(
            event_batch_path(directory, ids[1], ids[3]),
            &StoredEventBatch {
                events: vec![make(ids[1], false), make(ids[3], false)],
            },
        )
        .await?;
    store
        .put_json(
            directory.join("events").join(format!("{}.json", ids[2])),
            &make(ids[2], true),
        )
        .await?;
    for id in &ids[4..24] {
        store
            .put_json(
                directory.join("events").join(format!("{id}.json")),
                &make(*id, false),
            )
            .await?;
    }
    store
        .put_bytes(
            directory.join("events").join(format!("{}.json", ids[24])),
            b"unread future blob".to_vec(),
        )
        .await?;
    for (direction, cursor, limit, kinds, expected) in [
        (
            EventQueryDirection::Asc,
            ids[0],
            2,
            None,
            vec![ids[1], ids[2]],
        ),
        (EventQueryDirection::Asc, ids[2], 1, None, vec![ids[3]]),
        (
            EventQueryDirection::Desc,
            ids[4],
            2,
            None,
            vec![ids[3], ids[2]],
        ),
        (
            EventQueryDirection::Asc,
            ids[0],
            1,
            Some(vec![crate::EventKind::ERROR]),
            vec![ids[2]],
        ),
    ] {
        let events = load_event_query(
            &store,
            directory,
            &EventQuery {
                cursor: Some(cursor),
                direction: Some(direction),
                limit: Some(limit),
                session_id: Some(session),
                types: kinds,
                ..Default::default()
            },
        )
        .await?;
        assert_eq!(
            events.iter().map(|event| event.id).collect::<Vec<_>>(),
            expected
        );
    }
    Ok(())
}
