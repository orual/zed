use std::{path::Path, sync::Arc, time::Duration};

use crate::AgentServer;
use acp_thread::{
    AcpThread, AgentThreadEntry, ToolCall, ToolCallConfirmation, ToolCallContent, ToolCallStatus,
};
use agentic_coding_protocol as acp;
use anyhow::Result;
use futures::{FutureExt, StreamExt, channel::mpsc, select};
use gpui::{AsyncApp, Entity, TestAppContext};
use indoc::indoc;
use project::{FakeFs, Project};
use serde_json::json;
use settings::SettingsStore;
use util::path;

use crate::{AgentServerCommand, AgentServerVersion, StdioAgentServer};

pub async fn test_basic(command: AgentServerCommand, cx: &mut TestAppContext) {
    let fs = init_test(cx).await;
    let project = Project::test(fs, [], cx).await;
    let thread = new_test_thread(command, project.clone(), "/private/tmp", cx).await;

    thread
        .update(cx, |thread, cx| thread.send_raw("Hello from Zed!", cx))
        .await
        .unwrap();

    thread.read_with(cx, |thread, _| {
        assert_eq!(thread.entries().len(), 2);
        assert!(matches!(
            thread.entries()[0],
            AgentThreadEntry::UserMessage(_)
        ));
        assert!(matches!(
            thread.entries()[1],
            AgentThreadEntry::AssistantMessage(_)
        ));
    });
}

pub async fn test_path_mentions(command: AgentServerCommand, cx: &mut TestAppContext) {
    let _fs = init_test(cx).await;

    let tempdir = tempfile::tempdir().unwrap();
    std::fs::write(
        tempdir.path().join("foo.rs"),
        indoc! {"
            fn main() {
                println!(\"Hello, world!\");
            }
        "},
    )
    .expect("failed to write file");
    let project = Project::example([tempdir.path()], &mut cx.to_async()).await;
    let thread = new_test_thread(command, project.clone(), tempdir.path(), cx).await;
    thread
        .update(cx, |thread, cx| {
            thread.send(
                acp::SendUserMessageParams {
                    chunks: vec![
                        acp::UserMessageChunk::Text {
                            text: "Read the file ".into(),
                        },
                        acp::UserMessageChunk::Path {
                            path: Path::new("foo.rs").into(),
                        },
                        acp::UserMessageChunk::Text {
                            text: " and tell me what the content of the println! is".into(),
                        },
                    ],
                },
                cx,
            )
        })
        .await
        .unwrap();

    thread.read_with(cx, |thread, cx| {
        assert_eq!(thread.entries().len(), 3);
        assert!(matches!(
            thread.entries()[0],
            AgentThreadEntry::UserMessage(_)
        ));
        assert!(matches!(thread.entries()[1], AgentThreadEntry::ToolCall(_)));
        let AgentThreadEntry::AssistantMessage(assistant_message) = &thread.entries()[2] else {
            panic!("Expected AssistantMessage")
        };
        assert!(
            assistant_message.to_markdown(cx).contains("Hello, world!"),
            "unexpected assistant message: {:?}",
            assistant_message.to_markdown(cx)
        );
    });
}

pub async fn test_tool_call(command: AgentServerCommand, cx: &mut TestAppContext) {
    let fs = init_test(cx).await;
    fs.insert_tree(
        path!("/private/tmp"),
        json!({"foo": "Lorem ipsum dolor", "bar": "bar", "baz": "baz"}),
    )
    .await;
    let project = Project::test(fs, [path!("/private/tmp").as_ref()], cx).await;
    let thread = new_test_thread(command, project.clone(), "/private/tmp", cx).await;

    thread
        .update(cx, |thread, cx| {
            thread.send_raw(
                "Read the '/private/tmp/foo' file and tell me what you see.",
                cx,
            )
        })
        .await
        .unwrap();
    thread.read_with(cx, |thread, _cx| {
        assert!(matches!(
            &thread.entries()[2],
            AgentThreadEntry::ToolCall(ToolCall {
                status: ToolCallStatus::Allowed { .. },
                ..
            })
        ));

        assert!(matches!(
            thread.entries()[3],
            AgentThreadEntry::AssistantMessage(_)
        ));
    });
}

pub async fn test_tool_call_with_confirmation(
    command: AgentServerCommand,
    cx: &mut TestAppContext,
) {
    let fs = init_test(cx).await;
    let project = Project::test(fs, [path!("/private/tmp").as_ref()], cx).await;
    let thread = new_test_thread(command, project.clone(), "/private/tmp", cx).await;
    let full_turn = thread.update(cx, |thread, cx| {
        thread.send_raw(r#"Run `echo "Hello, world!"`"#, cx)
    });

    run_until_first_tool_call(&thread, cx).await;

    let tool_call_id = thread.read_with(cx, |thread, _cx| {
        let AgentThreadEntry::ToolCall(ToolCall {
            id,
            status:
                ToolCallStatus::WaitingForConfirmation {
                    confirmation: ToolCallConfirmation::Execute { root_command, .. },
                    ..
                },
            ..
        }) = &thread.entries()[2]
        else {
            panic!();
        };

        assert_eq!(root_command, "echo");

        *id
    });

    thread.update(cx, |thread, cx| {
        thread.authorize_tool_call(tool_call_id, acp::ToolCallConfirmationOutcome::Allow, cx);

        assert!(matches!(
            &thread.entries()[2],
            AgentThreadEntry::ToolCall(ToolCall {
                status: ToolCallStatus::Allowed { .. },
                ..
            })
        ));
    });

    full_turn.await.unwrap();

    thread.read_with(cx, |thread, cx| {
        let AgentThreadEntry::ToolCall(ToolCall {
            content: Some(ToolCallContent::Markdown { markdown }),
            status: ToolCallStatus::Allowed { .. },
            ..
        }) = &thread.entries()[2]
        else {
            panic!();
        };

        markdown.read_with(cx, |md, _cx| {
            assert!(
                md.source().contains("Hello, world!"),
                r#"Expected '{}' to contain "Hello, world!""#,
                md.source()
            );
        });
    });
}

pub async fn test_cancel(command: AgentServerCommand, cx: &mut TestAppContext) {
    let fs = init_test(cx).await;

    let project = Project::test(fs, [path!("/private/tmp").as_ref()], cx).await;
    let thread = new_test_thread(command, project.clone(), "/private/tmp", cx).await;
    let full_turn = thread.update(cx, |thread, cx| {
        thread.send_raw(r#"Run `echo "Hello, world!"`"#, cx)
    });

    let first_tool_call_ix = run_until_first_tool_call(&thread, cx).await;

    thread.read_with(cx, |thread, _cx| {
        let AgentThreadEntry::ToolCall(ToolCall {
            id,
            status:
                ToolCallStatus::WaitingForConfirmation {
                    confirmation: ToolCallConfirmation::Execute { root_command, .. },
                    ..
                },
            ..
        }) = &thread.entries()[first_tool_call_ix]
        else {
            panic!("{:?}", thread.entries()[1]);
        };

        assert_eq!(root_command, "echo");

        *id
    });

    thread
        .update(cx, |thread, cx| thread.cancel(cx))
        .await
        .unwrap();
    full_turn.await.unwrap();
    thread.read_with(cx, |thread, _| {
        let AgentThreadEntry::ToolCall(ToolCall {
            status: ToolCallStatus::Canceled,
            ..
        }) = &thread.entries()[first_tool_call_ix]
        else {
            panic!();
        };
    });

    thread
        .update(cx, |thread, cx| {
            thread.send_raw(r#"Stop running and say goodbye to me."#, cx)
        })
        .await
        .unwrap();
    thread.read_with(cx, |thread, _| {
        assert!(matches!(
            &thread.entries().last().unwrap(),
            AgentThreadEntry::AssistantMessage(..),
        ))
    });
}

#[macro_export]
macro_rules! common_e2e_tests {
    ($command:expr) => {
        #[::gpui::test]
        #[cfg_attr(not(feature = "e2e"), ignore)]
        async fn basic(cx: &mut ::gpui::TestAppContext) {
            $crate::e2e_tests::test_basic($command, cx).await;
        }

        #[::gpui::test]
        #[cfg_attr(not(feature = "e2e"), ignore)]
        async fn path_mentions(cx: &mut ::gpui::TestAppContext) {
            $crate::e2e_tests::test_path_mentions($command, cx).await;
        }

        #[::gpui::test]
        #[cfg_attr(not(feature = "e2e"), ignore)]
        async fn tool_call(cx: &mut ::gpui::TestAppContext) {
            $crate::e2e_tests::test_tool_call($command, cx).await;
        }

        #[::gpui::test]
        #[cfg_attr(not(feature = "e2e"), ignore)]
        async fn tool_call_with_confirmation(cx: &mut ::gpui::TestAppContext) {
            $crate::e2e_tests::test_tool_call_with_confirmation($command, cx).await;
        }

        #[::gpui::test]
        #[cfg_attr(not(feature = "e2e"), ignore)]
        async fn cancel(cx: &mut ::gpui::TestAppContext) {
            $crate::e2e_tests::test_cancel($command, cx).await;
        }
    };
}

// Helpers

pub async fn init_test(cx: &mut TestAppContext) -> Arc<FakeFs> {
    env_logger::try_init().ok();

    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        Project::init_settings(cx);
        language::init(cx);
    });

    cx.executor().allow_parking();

    FakeFs::new(cx.executor())
}

pub async fn new_test_thread(
    command: AgentServerCommand,
    project: Entity<Project>,
    current_dir: impl AsRef<Path>,
    cx: &mut TestAppContext,
) -> Entity<AcpThread> {
    #[derive(Clone)]
    struct TestServer {
        command: AgentServerCommand,
    }

    impl StdioAgentServer for TestServer {
        async fn command(
            &self,
            _project: &Entity<Project>,
            _cx: &mut AsyncApp,
        ) -> Result<AgentServerCommand> {
            Ok(self.command.clone())
        }

        async fn version(&self, _command: &AgentServerCommand) -> Result<AgentServerVersion> {
            Ok(AgentServerVersion::Supported)
        }

        fn logo(&self) -> ui::IconName {
            ui::IconName::Hammer
        }

        fn name(&self) -> &'static str {
            "test"
        }

        fn empty_state_headline(&self) -> &'static str {
            "test"
        }

        fn empty_state_message(&self) -> &'static str {
            "test"
        }

        fn supports_always_allow(&self) -> bool {
            true
        }
    }

    let thread = cx
        .update(|cx| TestServer { command }.new_thread(current_dir.as_ref(), &project, cx))
        .await
        .unwrap();

    thread
        .update(cx, |thread, _| thread.initialize())
        .await
        .unwrap();
    thread
}

pub async fn run_until_first_tool_call(
    thread: &Entity<AcpThread>,
    cx: &mut TestAppContext,
) -> usize {
    let (mut tx, mut rx) = mpsc::channel::<usize>(1);

    let subscription = cx.update(|cx| {
        cx.subscribe(thread, move |thread, _, cx| {
            for (ix, entry) in thread.read(cx).entries().iter().enumerate() {
                if matches!(entry, AgentThreadEntry::ToolCall(_)) {
                    return tx.try_send(ix).unwrap();
                }
            }
        })
    });

    select! {
        // We have to use a smol timer here because
        // cx.background_executor().timer isn't real in the test context
        _ = futures::FutureExt::fuse(smol::Timer::after(Duration::from_secs(10))) => {
            panic!("Timeout waiting for tool call")
        }
        ix = rx.next().fuse() => {
            drop(subscription);
            ix.unwrap()
        }
    }
}
