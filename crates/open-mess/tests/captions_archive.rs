//! The archive of Teams calls from their live captions (spec 2026-11-04-captions-archive), with
//! test-echo standing in for the Teams module.

// Test helpers outside #[test] functions aren't covered by clippy.toml's test allowances.
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

mod common;

use common::{ECHO, daemon, info};
use open_mess::permissions::{self, Write};
use open_mess::recording::archive::{self, Cue};
use open_mess::recording::{Meta, Recordings, Source};
use open_mess::tools::ToolError;
use serde_json::json;

fn captions(recordings: &Recordings, conversation: &str, threads: &[&str]) -> String {
    let meta = Meta {
        source: Source::Captions,
        start_ms: 1_700_000_000_000,
        account: Some("test-echo:a".into()),
        conversation: Some(conversation.into()),
        threads: threads.iter().map(|thread| (*thread).to_string()).collect(),
        call: Some("call-1".into()),
        ..Meta::default()
    };
    let id = recordings.create_with(&meta).unwrap();
    archive::append_cues(&recordings.path(&id).unwrap(), &[Cue::capped(0, "Ann", false, "said")]).unwrap();
    id
}

#[tokio::test]
async fn a_captions_recording_is_read_under_its_conversation_s_permission() {
    let d = daemon(&[ECHO]).await;
    d.accounts.add("test-echo", info("a")).unwrap();
    let recordings = Recordings::new(&d.dir.path().join("local"));
    let chat = captions(&recordings, "19:chat@thread.v2", &["19:chat@thread.v2", "19:group@thread.v2"]);
    let bare = captions(&recordings, "call:call-2", &[]);
    let read = |id: &str| d.registry.call("get_recording_transcript", json!({"recording": id}));
    let listed = || async { d.registry.call("list_recordings", json!({})).await.unwrap()["items"].as_array().unwrap().len() };
    assert_eq!(read(&chat).await.unwrap()["untrusted"]["cues"][0]["text"], "said");
    assert_eq!(listed().await, 2);

    let store = common::store(d.dir.path());
    // A call without a chat follows its own name's override, like any conversation.
    permissions::set(&store, "test-echo:a", Some("call:call-2"), false, Write::Never).unwrap();
    assert!(matches!(read(&bare).await, Err(ToolError::NotAllowed(_))));
    // The text of the chat it escalated to needs that chat too.
    permissions::set(&store, "test-echo:a", Some("19:group@thread.v2"), false, Write::Never).unwrap();
    assert!(matches!(read(&chat).await, Err(ToolError::NotAllowed(_))));
    assert_eq!(listed().await, 0);
    permissions::set(&store, "test-echo:a", Some("19:group@thread.v2"), true, Write::Never).unwrap();
    assert!(read(&chat).await.is_ok());

    // A removed account's recordings are the owner's only, though its permissions went with it.
    d.accounts.remove("test-echo:a").await.unwrap();
    assert!(matches!(read(&chat).await, Err(ToolError::NotAllowed(_))));
    assert_eq!(listed().await, 0);
}
