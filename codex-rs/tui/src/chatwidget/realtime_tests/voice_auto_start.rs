//! `[realtime] auto_start` starts voice for a new thread, or explains why it cannot.

use super::super::set_realtime_voice_supported_for_test;
use super::super::voice_status::VOICE_UNSUPPORTED_AUTO_START_NOTICE;
use super::*;
use crate::app_event::VoiceControl;
use pretty_assertions::assert_eq;

fn auto_start_requests(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> Vec<Option<ThreadId>> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::VoiceControl {
                thread_id,
                control: VoiceControl::AutoStart,
            } => Some(thread_id),
            _ => None,
        })
        .collect()
}

fn history_text(
    chat: &mut ChatWidget,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> String {
    commit_realtime_history_events(chat, events);
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::InsertHistoryCell(cell) => Some(
                cell.display_lines(/*width*/ 200)
                    .into_iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn auto_start_off_never_requests_or_starts_voice() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    set_realtime_voice_supported_for_test(Some(true));
    chat.thread_id = Some(ThreadId::new());
    assert!(
        !chat.config.realtime.auto_start,
        "upstream default stays off"
    );

    chat.request_realtime_auto_start();
    let started = chat.auto_start_realtime_conversation();
    set_realtime_voice_supported_for_test(None);

    assert!(!started);
    assert_eq!(auto_start_requests(&mut events), Vec::new());
    assert_eq!(
        chat.realtime_conversation.phase,
        RealtimeConversationPhase::Inactive
    );
    assert!(ops.try_recv().is_err());
}

#[tokio::test]
async fn auto_start_on_requests_voice_for_the_new_thread() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.config.realtime.auto_start = true;

    chat.request_realtime_auto_start();

    assert_eq!(auto_start_requests(&mut events), vec![Some(thread_id)]);
}

#[tokio::test]
async fn auto_start_on_starts_voice_when_the_runtime_is_supported() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    set_realtime_voice_supported_for_test(Some(true));
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.config.realtime.auto_start = true;

    let started = chat.auto_start_realtime_conversation();
    set_realtime_voice_supported_for_test(None);

    assert!(started);
    assert_eq!(
        chat.realtime_conversation.phase,
        RealtimeConversationPhase::Starting
    );
    assert_eq!(chat.realtime_conversation.thread_id, Some(thread_id));
    // A running call is never started twice.
    assert!(!chat.auto_start_realtime_conversation());
    chat.reset_realtime_conversation();
    assert!(!history_text(&mut chat, &mut events).contains("Voice auto-start skipped"));
}

#[tokio::test]
async fn auto_start_on_explains_an_unsupported_build_in_one_line() {
    let (mut chat, _sender, mut events, mut ops) = make_chatwidget_manual_with_sender().await;
    set_realtime_voice_supported_for_test(Some(false));
    chat.thread_id = Some(ThreadId::new());
    chat.config.realtime.auto_start = true;

    let started = chat.auto_start_realtime_conversation();
    set_realtime_voice_supported_for_test(None);

    assert!(!started);
    assert_eq!(
        chat.realtime_conversation.phase,
        RealtimeConversationPhase::Inactive
    );
    let history = history_text(&mut chat, &mut events);
    assert_eq!(
        history.matches(VOICE_UNSUPPORTED_AUTO_START_NOTICE).count(),
        1,
        "{history}"
    );
    assert!(VOICE_UNSUPPORTED_AUTO_START_NOTICE.contains("voice host"));
    assert!(VOICE_UNSUPPORTED_AUTO_START_NOTICE.contains("GStreamer"));
    assert!(!VOICE_UNSUPPORTED_AUTO_START_NOTICE.contains('\n'));
    assert!(ops.try_recv().is_err());
}

#[tokio::test]
async fn auto_start_stays_quiet_in_side_conversations() {
    let (mut chat, _sender, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    set_realtime_voice_supported_for_test(Some(false));
    chat.thread_id = Some(ThreadId::new());
    chat.config.realtime.auto_start = true;
    chat.set_side_conversation_active(/*active*/ true);

    let started = chat.auto_start_realtime_conversation();
    set_realtime_voice_supported_for_test(None);

    assert!(!started);
    assert_eq!(history_text(&mut chat, &mut events), "");
}
