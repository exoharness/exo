use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Result;
use executor::{
    AgentHandle, ConversationHandle, EventData, EventId, EventKind, EventQuery,
    EventQueryDirection, ExecutionStreamEvent, Runtime, SandboxId, SandboxProvider, SendRequest,
    SessionId, SnapshotId, StartSandboxRequest,
};
use lingua::universal::{UserContent, UserContentPart};
use lingua::{Message, UniversalStreamChunk};
use rustyline::error::ReadlineError;
use rustyline::history::{History, MemHistory, SearchDirection, SearchResult};
use rustyline::{Cmd, Config, Editor, KeyCode, KeyEvent, Modifiers};
use tokio::runtime::Handle;
use tokio_stream::StreamExt;

use crate::render::{
    ASSISTANT_LABEL, Verbosity, compact_timestamp, print_transcript, render_assistant_content,
    render_tool_call, render_tool_result,
};
use crate::run_sandbox_shell_command;
use crate::turn_display::{TurnProgress, UsageTotals, UsageTracker, interruptible};
use executor::harness::HarnessTurnKey;

const DEFAULT_SHELL_PROGRAM: &str = "/bin/bash";
const REMOTE_HISTORY_BASE: usize = 1_000_000;
const REMOTE_HISTORY_PAGE_SIZE: u32 = 32;

pub async fn run_chat_repl(
    runtime: Arc<Runtime>,
    agent: Arc<dyn AgentHandle>,
    conversation: Arc<dyn ConversationHandle>,
    verbosity: Verbosity,
) -> Result<()> {
    let mut repl = ChatRepl::new(runtime, agent, conversation, verbosity)?;
    interruptible(repl.print_transcript()).await?;
    while interruptible(repl.run()).await?.is_none() {}
    Ok(())
}

pub async fn run_prompt(
    runtime: Arc<Runtime>,
    agent: Arc<dyn AgentHandle>,
    conversation: Arc<dyn ConversationHandle>,
    verbosity: Verbosity,
    prompt: &str,
) -> Result<()> {
    let mut repl = ChatRepl::new(runtime, agent, conversation, verbosity)?;
    repl.send(prompt).await?;
    repl.end_session().await
}

struct ChatHistory {
    state: Mutex<ChatHistoryState>,
}

struct ChatHistoryState {
    conversation: Arc<dyn ConversationHandle>,
    remote_cutoff: Option<EventId>,
    remote_cursor: Option<EventId>,
    remote_entries: Vec<String>,
    remote_exhausted: bool,
    local_history: MemHistory,
}

impl ChatHistory {
    fn new(conversation: Arc<dyn ConversationHandle>, remote_cutoff: Option<EventId>) -> Self {
        Self {
            state: Mutex::new(ChatHistoryState {
                conversation,
                remote_cutoff,
                remote_cursor: None,
                remote_entries: Vec::new(),
                remote_exhausted: false,
                local_history: MemHistory::with_config(Config::default()),
            }),
        }
    }

    fn remote_enabled(state: &ChatHistoryState) -> bool {
        state.remote_cutoff.is_some()
    }

    fn remote_len_sentinel(state: &ChatHistoryState) -> usize {
        if Self::remote_enabled(state) {
            REMOTE_HISTORY_BASE
        } else {
            0
        }
    }

    fn history_len(state: &ChatHistoryState) -> usize {
        Self::remote_len_sentinel(state).saturating_add(state.local_history.len())
    }

    fn ensure_remote_position_loaded(&self, remote_position: usize) -> rustyline::Result<()> {
        loop {
            let (conversation, cursor, cutoff, loaded_len, exhausted, enabled) = {
                let state = self.state.lock().expect("chat history poisoned");
                (
                    Arc::clone(&state.conversation),
                    state.remote_cursor,
                    state.remote_cutoff,
                    state.remote_entries.len(),
                    state.remote_exhausted,
                    Self::remote_enabled(&state),
                )
            };

            if !enabled || loaded_len > remote_position || exhausted {
                return Ok(());
            }

            let page = fetch_remote_user_messages(&conversation, cursor, cutoff)?;
            let mut state = self.state.lock().expect("chat history poisoned");
            state.remote_cursor = page.cursor;
            if page.cursor.is_none() {
                state.remote_exhausted = true;
            }
            if page.entries.is_empty() {
                if state.remote_exhausted {
                    return Ok(());
                }
                continue;
            }
            state.remote_entries.extend(page.entries);
        }
    }

    fn get_entry(
        &self,
        index: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let remote_len_sentinel = {
            let state = self.state.lock().expect("chat history poisoned");
            Self::remote_len_sentinel(&state)
        };

        if index >= remote_len_sentinel {
            let state = self.state.lock().expect("chat history poisoned");
            let local_index = index - remote_len_sentinel;
            return Ok(state
                .local_history
                .get(local_index, dir)?
                .map(|result| SearchResult {
                    entry: Cow::Owned(result.entry.into_owned()),
                    idx: result.idx,
                    pos: result.pos,
                }));
        }

        let remote_position = remote_len_sentinel - 1 - index;
        self.ensure_remote_position_loaded(remote_position)?;

        let state = self.state.lock().expect("chat history poisoned");
        Ok(state
            .remote_entries
            .get(remote_position)
            .cloned()
            .map(|entry| SearchResult {
                entry: Cow::Owned(entry),
                idx: index,
                pos: 0,
            }))
    }
}

impl History for ChatHistory {
    fn get(
        &self,
        index: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        self.get_entry(index, dir)
    }

    fn add(&mut self, line: &str) -> rustyline::Result<bool> {
        self.add_owned(line.to_string())
    }

    fn add_owned(&mut self, line: String) -> rustyline::Result<bool> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.add_owned(line)
    }

    fn len(&self) -> usize {
        let state = self.state.lock().expect("chat history poisoned");
        Self::history_len(&state)
    }

    fn is_empty(&self) -> bool {
        let state = self.state.lock().expect("chat history poisoned");
        !Self::remote_enabled(&state) && state.local_history.is_empty()
    }

    fn set_max_len(&mut self, len: usize) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.set_max_len(len)
    }

    fn ignore_dups(&mut self, yes: bool) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.ignore_dups(yes)
    }

    fn ignore_space(&mut self, yes: bool) {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.ignore_space(yes);
    }

    fn save(&mut self, path: &Path) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.save(path)
    }

    fn append(&mut self, path: &Path) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.append(path)
    }

    fn load(&mut self, path: &Path) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.local_history.load(path)
    }

    fn clear(&mut self) -> rustyline::Result<()> {
        let mut state = self.state.lock().expect("chat history poisoned");
        state.remote_cursor = None;
        state.remote_entries.clear();
        state.remote_exhausted = false;
        state.local_history.clear()
    }

    fn search(
        &self,
        term: &str,
        start: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let mut index = start;
        loop {
            let Some(result) = self.get(index, dir)? else {
                return Ok(None);
            };
            if let Some(pos) = result.entry.find(term) {
                return Ok(Some(SearchResult {
                    entry: result.entry,
                    idx: result.idx,
                    pos,
                }));
            }
            match dir {
                SearchDirection::Forward => {
                    index += 1;
                    if index >= self.len() {
                        return Ok(None);
                    }
                }
                SearchDirection::Reverse => {
                    if index == 0 {
                        return Ok(None);
                    }
                    index -= 1;
                }
            }
        }
    }

    fn starts_with(
        &self,
        term: &str,
        start: usize,
        dir: SearchDirection,
    ) -> rustyline::Result<Option<SearchResult<'_>>> {
        let mut index = start;
        loop {
            let Some(result) = self.get(index, dir)? else {
                return Ok(None);
            };
            if result.entry.starts_with(term) {
                return Ok(Some(SearchResult {
                    entry: result.entry,
                    idx: result.idx,
                    pos: 0,
                }));
            }
            match dir {
                SearchDirection::Forward => {
                    index += 1;
                    if index >= self.len() {
                        return Ok(None);
                    }
                }
                SearchDirection::Reverse => {
                    if index == 0 {
                        return Ok(None);
                    }
                    index -= 1;
                }
            }
        }
    }
}

struct RemoteHistoryPage {
    entries: Vec<String>,
    cursor: Option<EventId>,
}

fn fetch_remote_user_messages(
    conversation: &Arc<dyn ConversationHandle>,
    cursor: Option<EventId>,
    cutoff: Option<EventId>,
) -> rustyline::Result<RemoteHistoryPage> {
    let runtime = Handle::try_current().map_err(|error| {
        ReadlineError::Io(io::Error::other(format!(
            "failed to access tokio runtime for chat history: {error}"
        )))
    })?;
    let result = tokio::task::block_in_place(|| {
        runtime.block_on(conversation.get_events(Some(EventQuery {
            cursor,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(REMOTE_HISTORY_PAGE_SIZE),
            session_id: None,
            turn_id: None,
            types: None,
        })))
    })
    .map_err(|error| {
        ReadlineError::Io(io::Error::other(format!(
            "failed to fetch chat history events: {error}"
        )))
    })?;

    let mut entries = Vec::new();
    for event in result.events {
        if cutoff.is_some_and(|event_id| event.id > event_id) {
            continue;
        }
        if let EventData::Messages { messages, .. } = event.data {
            for message in messages.into_iter().rev() {
                if let Message::User { content } = message {
                    let text = render_user_content_for_history(&content);
                    if !text.trim().is_empty() {
                        entries.push(text);
                    }
                }
            }
        }
    }

    Ok(RemoteHistoryPage {
        entries,
        cursor: result.cursor,
    })
}

struct ChatRepl {
    runtime: Arc<Runtime>,
    active_turn: Option<HarnessTurnKey>,
    agent: Arc<dyn AgentHandle>,
    conversation: Arc<dyn ConversationHandle>,
    editor: Editor<(), ChatHistory>,
    session_id: Option<SessionId>,
    watch_after: Option<EventId>,
    usage: UsageTracker,
    verbosity: Verbosity,
}

impl ChatRepl {
    fn new(
        runtime: Arc<Runtime>,
        agent: Arc<dyn AgentHandle>,
        conversation: Arc<dyn ConversationHandle>,
        verbosity: Verbosity,
    ) -> Result<Self> {
        let latest_event_id = conversation.record().latest_event_id;
        let history = ChatHistory::new(conversation.clone(), latest_event_id);
        let mut editor = Editor::with_history(Config::default(), history)?;
        editor.bind_sequence(KeyEvent(KeyCode::Enter, Modifiers::ALT), Cmd::Newline);
        Ok(Self {
            runtime,
            active_turn: None,
            agent,
            conversation,
            editor,
            session_id: None,
            watch_after: latest_event_id,
            usage: UsageTracker::default(),
            verbosity,
        })
    }

    async fn print_transcript(&self) -> Result<()> {
        let messages = executor::materialize_conversation_messages(&*self.conversation).await?;
        print_transcript(&messages, self.verbosity);
        Ok(())
    }

    async fn run(&mut self) -> Result<()> {
        loop {
            let prompt = format!("{}> ", self.conversation.record().slug);
            match self.editor.readline(&prompt) {
                Ok(line) => {
                    let trimmed = line.trim();
                    if matches!(trimmed, "/quit" | "/exit") {
                        break;
                    }
                    if io::stdin().is_terminal()
                        && let Err(error) = self.print_pending_events().await
                    {
                        println!("event update failed: {error:#}");
                    }
                    if trimmed.is_empty() {
                        continue;
                    }
                    match trimmed {
                        "/history" => self.print_transcript().await?,
                        "/cost" | "/usage" => {
                            print_lines(cost_lines(self.conversation.as_ref()).await);
                        }
                        "/help" => print_help(),
                        "/verbosity" => {
                            println!("verbosity: {}", self.verbosity);
                            println!("usage: /verbosity <minimal|compact|full>");
                        }
                        other if other.starts_with("/verbosity ") => {
                            let arg = other
                                .strip_prefix("/verbosity ")
                                .expect("prefix checked")
                                .trim();
                            match arg.parse::<Verbosity>() {
                                Ok(verbosity) => {
                                    self.verbosity = verbosity;
                                    println!("verbosity set to {verbosity}");
                                }
                                Err(error) => println!("{error}"),
                            }
                        }
                        "/shell" | "/sandbox" => {
                            println!("usage: /shell <command>");
                            println!("alias: /sandbox <command>");
                        }
                        other
                            if other
                                .strip_prefix("/shell ")
                                .or_else(|| other.strip_prefix("/sandbox "))
                                .is_some() =>
                        {
                            let command = other
                                .strip_prefix("/shell ")
                                .or_else(|| other.strip_prefix("/sandbox "))
                                .expect("shell prefix should exist")
                                .trim();
                            if command.is_empty() {
                                println!("usage: /shell <command>");
                                println!("alias: /sandbox <command>");
                            } else {
                                self.editor.add_history_entry(line.as_str())?;
                                self.run_shell(command).await?;
                            }
                        }
                        "/snapshot" => {
                            print_lines(snapshot_lines(self.conversation.as_ref(), None).await);
                        }
                        other if other.starts_with("/snapshot ") => {
                            let arg = other
                                .strip_prefix("/snapshot ")
                                .expect("prefix checked")
                                .trim();
                            if arg.is_empty() {
                                println!("usage: /snapshot [<sandbox-id>]");
                            } else if arg.contains(char::is_whitespace) {
                                println!("/snapshot takes at most one sandbox id; got: {arg:?}");
                            } else {
                                print_lines(
                                    snapshot_lines(
                                        self.conversation.as_ref(),
                                        Some(arg.to_string()),
                                    )
                                    .await,
                                );
                            }
                        }
                        "/snapshots" => {
                            print_lines(snapshots_lines(self.conversation.as_ref()).await);
                        }
                        "/teleport" => {
                            println!("usage: /teleport <provider> (e.g. /teleport daytona)");
                        }
                        other if other.starts_with("/teleport ") => {
                            let arg = other
                                .strip_prefix("/teleport ")
                                .expect("prefix checked")
                                .trim();
                            if arg.is_empty() || arg.contains(char::is_whitespace) {
                                println!("usage: /teleport <provider> (e.g. /teleport daytona)");
                            } else {
                                match self.teleport_sandbox(arg).await {
                                    Ok((sandbox_id, provider)) => {
                                        println!("sandbox {sandbox_id} teleported to {provider}");
                                    }
                                    Err(error) => println!("teleport failed: {error:#}"),
                                }
                            }
                        }
                        other if other.starts_with("/rewind ") => {
                            let arg = other
                                .strip_prefix("/rewind ")
                                .expect("prefix checked")
                                .trim();
                            if arg.is_empty() {
                                println!("usage: /rewind <snapshot-id>");
                            } else if arg.contains(char::is_whitespace) {
                                println!("/rewind takes exactly one snapshot id; got: {arg:?}");
                            } else {
                                print_lines(rewind_lines(self.conversation.as_ref(), arg).await);
                            }
                        }
                        _ => {
                            self.editor.add_history_entry(line.as_str())?;
                            if let Err(error) = self.send(trimmed).await {
                                println!();
                                println!("turn failed: {error:#}");
                            }
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    println!();
                    continue;
                }
                Err(ReadlineError::Eof) => {
                    println!();
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }

        self.end_session().await
    }

    async fn end_session(&mut self) -> Result<()> {
        if let Some(session_id) = self.session_id.take() {
            self.runtime
                .end_session(self.conversation.as_ref(), session_id)
                .await?;
        }
        Ok(())
    }

    /// Teleport the conversation's live sandbox to another provider: snapshot
    /// it where it runs now, then restore that snapshot under the target
    /// provider. Prints progress between the slow steps, which is why the
    /// line-mode repl doesn't use the TUI's silent `teleport_lines`.
    async fn teleport_sandbox(&self, provider_str: &str) -> Result<(SandboxId, SandboxProvider)> {
        let provider = provider_str
            .parse::<SandboxProvider>()
            .map_err(|error| anyhow::anyhow!("invalid provider `{provider_str}`: {error}"))?;
        let sandbox_id = latest_sandbox_id(self.conversation.as_ref())
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("no sandbox has been created in this conversation yet")
            })?;
        println!("snapshotting sandbox {sandbox_id}...");
        let snapshot_id = self
            .conversation
            .snapshot_sandbox(sandbox_id.clone())
            .await?;
        println!("snapshot {snapshot_id} captured; restoring on {provider}...");
        self.conversation
            .start_sandbox(StartSandboxRequest {
                id: sandbox_id.clone(),
                snapshot_id,
                idle_seconds: None,
                provider: Some(provider.clone()),
            })
            .await?;
        Ok((sandbox_id, provider))
    }

    async fn send(&mut self, input: &str) -> Result<()> {
        let result = self.send_inner(input).await;
        if result.as_ref().err().is_some_and(|error| {
            error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.kind() == io::ErrorKind::Interrupted)
        }) && let Some(key) = self.active_turn.take()
        {
            self.runtime.cancel(key).await?;
        }
        self.active_turn = None;
        result
    }

    async fn send_inner(&mut self, input: &str) -> Result<()> {
        let started = Instant::now();
        let mut progress = TurnProgress::new();
        progress
            .wait(self.usage.refresh(self.conversation.as_ref(), None))
            .await?;
        let (turn, mut stream) = progress
            .wait(self.runtime.start_turn(
                Arc::clone(&self.agent),
                Arc::clone(&self.conversation),
                SendRequest {
                    input: vec![Message::User {
                        content: UserContent::String(input.to_string()),
                    }],
                    session_id: self.session_id,
                },
                true,
                None,
            ))
            .await?;
        self.active_turn = Some(HarnessTurnKey::new(self.conversation.record().id, turn.id));
        progress.set_status(Some("Waiting for model".to_string()));
        let mut stdout = io::stdout();
        let mut printed_assistant = false;
        let mut streamed_text = String::new();
        let mut ttft = None;
        let mut completed_turn = None;
        // Tool names by call id, so results (which only carry the id) can be
        // labeled in compact mode.
        let mut pending_tool_calls: HashMap<String, String> = HashMap::new();
        while let Some(event) = progress
            .wait(async { stream.next().await.transpose() })
            .await?
        {
            match event {
                ExecutionStreamEvent::FirstChunk { .. } => {
                    ttft.get_or_insert_with(|| started.elapsed());
                    progress.set_status(Some("Thinking".to_string()));
                }
                ExecutionStreamEvent::Chunk(chunk) => {
                    let text = chunk_text(&chunk);
                    if text.is_empty() {
                        continue;
                    }
                    ttft.get_or_insert_with(|| started.elapsed());
                    progress.set_status(None);
                    if !printed_assistant {
                        print!("{} {ASSISTANT_LABEL}: ", compact_timestamp());
                        stdout.flush()?;
                        printed_assistant = true;
                    }
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                    streamed_text.push_str(&text);
                }
                ExecutionStreamEvent::ToolCall {
                    tool_call_id,
                    tool_name,
                    arguments,
                } => {
                    if printed_assistant && !streamed_text.ends_with('\n') {
                        println!();
                        printed_assistant = false;
                        streamed_text.clear();
                    }
                    if let Some(rendered) = render_tool_call(&tool_name, &arguments, self.verbosity)
                    {
                        println!("{rendered}");
                        printed_assistant = false;
                        streamed_text.clear();
                    }
                    progress.set_status(Some(format!("Running tool {tool_name}")));
                    pending_tool_calls.insert(tool_call_id, tool_name);
                }
                ExecutionStreamEvent::ToolResult {
                    tool_call_id,
                    result,
                } => {
                    let tool_name = pending_tool_calls
                        .remove(&tool_call_id)
                        .unwrap_or_else(|| "tool".to_string());
                    if let Some(rendered) = render_tool_result(&tool_name, &result, self.verbosity)
                    {
                        println!("{rendered}");
                    }
                    progress.set_status(Some(if pending_tool_calls.is_empty() {
                        "Waiting for model".to_string()
                    } else {
                        "Running tools".to_string()
                    }));
                }
                ExecutionStreamEvent::Completed(result) => {
                    self.session_id = Some(result.session_id);
                    self.watch_after = Some(result.latest_event_id);
                    completed_turn = Some(result.turn_id);
                }
            }
        }
        let elapsed = started.elapsed();

        if printed_assistant {
            println!();
        } else if let Some(last_message) =
            executor::materialize_conversation_messages(&*self.conversation)
                .await?
                .last()
                .cloned()
            && let Message::Assistant { content, .. } = last_message
        {
            let rendered = render_assistant_content(&content, self.verbosity);
            if !rendered.is_empty() {
                println!("{} {ASSISTANT_LABEL}: {}", compact_timestamp(), rendered);
            }
        }
        progress.set_status(Some("Finishing turn".to_string()));
        match progress
            .wait(
                self.usage
                    .refresh(self.conversation.as_ref(), completed_turn),
            )
            .await
        {
            Ok(usage) => println!("{}", usage.display(&self.usage.total, ttft, elapsed)),
            Err(error) => {
                println!(
                    "{}",
                    UsageTotals::default().display(&self.usage.total, ttft, elapsed)
                );
                println!("usage summary failed: {error:#}");
            }
        }
        println!();
        Ok(())
    }

    async fn print_pending_events(&mut self) -> Result<()> {
        let conversation = &self.conversation;
        loop {
            let result = conversation
                .get_events(Some(EventQuery {
                    cursor: self.watch_after,
                    direction: Some(EventQueryDirection::Asc),
                    limit: Some(100),
                    session_id: None,
                    turn_id: None,
                    types: None,
                }))
                .await?;
            if result.events.is_empty() {
                return Ok(());
            }
            for event in result.events {
                self.watch_after = Some(event.id);
                print_lines(render_external_event(&event.data, self.verbosity));
            }
        }
    }

    async fn run_shell(&self, command: &str) -> Result<()> {
        let output = shell_output(
            self.runtime.as_ref(),
            self.agent.as_ref(),
            self.conversation.as_ref(),
            command.to_string(),
        )
        .await?;
        io::stdout().write_all(output.stdout.as_bytes())?;
        io::stderr().write_all(output.stderr.as_bytes())?;
        if output.exit_code != 0 {
            println!("[exit {}]", output.exit_code);
        }
        Ok(())
    }
}

pub(crate) fn chunk_text(chunk: &UniversalStreamChunk) -> String {
    let mut text = String::new();
    for choice in &chunk.choices {
        if let Some(delta) = choice.delta_view()
            && let Some(content) = delta.content
        {
            text.push_str(&content);
        }
    }
    text
}

pub(crate) fn render_external_event(data: &EventData, verbosity: Verbosity) -> Vec<String> {
    let EventData::Messages { messages, .. } = data else {
        return Vec::new();
    };
    messages
        .iter()
        .filter_map(|message| match message {
            Message::User { content } => render_external_user_content(content)
                .map(|rendered| format!("{} user: {rendered}", compact_timestamp())),
            Message::Assistant { content, .. } => {
                let rendered = render_assistant_content(content, verbosity);
                (!rendered.is_empty())
                    .then(|| format!("{} {ASSISTANT_LABEL}: {rendered}", compact_timestamp()))
            }
            _ => None,
        })
        .collect()
}

fn render_external_user_content(content: &UserContent) -> Option<String> {
    let rendered = render_user_content_for_history(content);
    let trimmed = rendered.trim();
    if trimmed.starts_with("Scheduled task `") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn render_user_content_for_history(content: &UserContent) -> String {
    match content {
        UserContent::String(text) => text.clone(),
        UserContent::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                UserContentPart::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
    }
}

fn print_lines(lines: Vec<String>) {
    for line in lines {
        println!("{line}");
    }
}

fn print_help() {
    println!("repl commands:");
    println!("  /quit | /exit        exit the repl");
    println!("  /history             reprint the conversation transcript");
    println!("  /verbosity <level>   set tool output detail: minimal, compact, or full");
    println!("  /cost | /usage       summarize token usage and dollar cost");
    println!("  /snapshot [<id>]     snapshot a sandbox in this conversation");
    println!("                       (defaults to the latest one if no id is given)");
    println!("  /snapshots           list snapshots taken in this conversation");
    println!("  /rewind <id>         restore the sandbox to a previous snapshot");
    println!("  /teleport <provider> move the live sandbox to another provider");
    println!("                       (e.g. /teleport daytona: snapshot + restore there)");
    println!("  /help                show this message");
}

/// `/cost` output: summarize token usage and dollar cost for the conversation
/// from the `usage` records on its `messages` events. Paginates so it covers
/// the whole conversation, not just one page.
pub(crate) async fn cost_lines(conversation: &dyn ConversationHandle) -> Vec<String> {
    match cost_summary(conversation).await {
        Ok(lines) => lines,
        Err(error) => vec![format!("cost summary failed: {error:#}")],
    }
}

async fn cost_summary(conversation: &dyn ConversationHandle) -> Result<Vec<String>> {
    let mut tracker = UsageTracker::default();
    tracker.refresh(conversation, None).await?;
    Ok(tracker.cost_lines())
}

/// `/snapshot` output: snapshot the given sandbox, or the conversation's
/// latest one when no id is given.
pub(crate) async fn snapshot_lines(
    conversation: &dyn ConversationHandle,
    explicit_id: Option<SandboxId>,
) -> Vec<String> {
    match snapshot_sandbox(conversation, explicit_id).await {
        Ok(snapshot_id) => vec![format!("snapshot {snapshot_id}")],
        Err(error) => vec![format!("snapshot failed: {error:#}")],
    }
}

async fn snapshot_sandbox(
    conversation: &dyn ConversationHandle,
    explicit_id: Option<SandboxId>,
) -> Result<SnapshotId> {
    let sandbox_id = match explicit_id {
        Some(id) => id,
        None => latest_sandbox_id(conversation).await?.ok_or_else(|| {
            anyhow::anyhow!("no sandbox has been created in this conversation yet")
        })?,
    };
    let id = conversation.snapshot_sandbox(sandbox_id).await?;
    Ok(id)
}

/// `/snapshots` output: every snapshot taken in the conversation.
pub(crate) async fn snapshots_lines(conversation: &dyn ConversationHandle) -> Vec<String> {
    match list_snapshots(conversation).await {
        Ok(snapshots) if snapshots.is_empty() => {
            vec!["no snapshots yet for this conversation".to_string()]
        }
        Ok(snapshots) => {
            let mut lines = vec!["SNAPSHOT\tTAKEN\tSANDBOX".to_string()];
            for (snapshot_id, sandbox_id) in snapshots {
                // Snapshot ids are uuid7, so creation time is embedded in the
                // id itself.
                let taken = snapshot_id
                    .timestamp()
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
                    .unwrap_or_else(|| "-".to_string());
                lines.push(format!("{snapshot_id}\t{taken}\t{sandbox_id}"));
            }
            lines
        }
        Err(error) => vec![format!("listing snapshots failed: {error:#}")],
    }
}

/// `/rewind` output: restore the conversation's sandbox to a previous
/// snapshot. Stops the current container, decodes the snapshot payload, and
/// starts a fresh container from that state.
pub(crate) async fn rewind_lines(
    conversation: &dyn ConversationHandle,
    snapshot_id: &str,
) -> Vec<String> {
    match rewind_to_snapshot(conversation, snapshot_id).await {
        Ok(()) => vec![format!("rewound to snapshot {snapshot_id}")],
        Err(error) => vec![format!("rewind failed: {error:#}")],
    }
}

async fn rewind_to_snapshot(
    conversation: &dyn ConversationHandle,
    snapshot_id_str: &str,
) -> Result<()> {
    let snapshot_id = snapshot_id_str
        .parse::<SnapshotId>()
        .map_err(|error| anyhow::anyhow!("invalid snapshot id `{snapshot_id_str}`: {error}"))?;
    let sandbox_id = sandbox_id_for_snapshot(conversation, snapshot_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("snapshot {snapshot_id} not found in this conversation"))?;
    conversation
        .start_sandbox(StartSandboxRequest {
            id: sandbox_id,
            snapshot_id,
            idle_seconds: None,
            provider: None,
        })
        .await?;
    Ok(())
}

/// `/teleport` output: move the conversation's live sandbox to another
/// provider by snapshotting it where it runs now, then restoring that
/// snapshot under the target provider. The sandbox id is stable across the
/// move; only the backend (and the machine actually running the container)
/// changes.
pub(crate) async fn teleport_lines(
    conversation: &dyn ConversationHandle,
    provider: &str,
) -> Vec<String> {
    match teleport_sandbox(conversation, provider).await {
        Ok((sandbox_id, provider)) => {
            vec![format!("sandbox {sandbox_id} teleported to {provider}")]
        }
        Err(error) => vec![format!("teleport failed: {error:#}")],
    }
}

async fn teleport_sandbox(
    conversation: &dyn ConversationHandle,
    provider_str: &str,
) -> Result<(SandboxId, SandboxProvider)> {
    let provider = provider_str
        .parse::<SandboxProvider>()
        .map_err(|error| anyhow::anyhow!("invalid provider `{provider_str}`: {error}"))?;
    let sandbox_id = latest_sandbox_id(conversation)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no sandbox has been created in this conversation yet"))?;
    let snapshot_id = conversation.snapshot_sandbox(sandbox_id.clone()).await?;
    conversation
        .start_sandbox(StartSandboxRequest {
            id: sandbox_id.clone(),
            snapshot_id,
            idle_seconds: None,
            provider: Some(provider.clone()),
        })
        .await?;
    Ok((sandbox_id, provider))
}

/// `/shell` output as display lines. The line-mode repl streams raw bytes to
/// stdout/stderr instead; this is for UIs that own the screen.
pub(crate) async fn shell_lines(
    harness: &Runtime,
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    command: String,
) -> Vec<String> {
    match shell_output(harness, agent, conversation, command).await {
        Ok(output) => {
            let mut lines: Vec<String> = output
                .stdout
                .lines()
                .chain(output.stderr.lines())
                .map(str::to_string)
                .collect();
            if output.exit_code != 0 {
                lines.push(format!("[exit {}]", output.exit_code));
            }
            lines
        }
        Err(error) => vec![format!("shell failed: {error:#}")],
    }
}

async fn shell_output(
    harness: &Runtime,
    agent: &dyn AgentHandle,
    conversation: &dyn ConversationHandle,
    command: String,
) -> Result<crate::SandboxShellOutput> {
    let mut config = executor::load_conversation_config(conversation).await?;
    if config.shell_program.is_none() {
        config.shell_program = Some(DEFAULT_SHELL_PROGRAM.to_string());
        harness
            .put_conversation_config(conversation, config)
            .await?;
    }
    run_sandbox_shell_command(agent, conversation, command).await
}

/// Walk the conversation's event log to find the latest `SandboxCreated`
/// event, returning the sandbox id. Returns `None` if no sandbox has been
/// created yet (e.g. nothing has been chatted with).
async fn latest_sandbox_id(conversation: &dyn ConversationHandle) -> Result<Option<SandboxId>> {
    let result = conversation
        .get_events(Some(EventQuery {
            cursor: None,
            direction: Some(EventQueryDirection::Desc),
            limit: Some(1),
            session_id: None,
            turn_id: None,
            types: Some(vec![EventKind::SANDBOX_CREATED]),
        }))
        .await?;
    let Some(event) = result.events.into_iter().next() else {
        return Ok(None);
    };
    match event.data {
        EventData::SandboxCreated { sandbox_id, .. } => Ok(Some(sandbox_id)),
        other => anyhow::bail!(
            "type-filtered query for {} returned unexpected variant {}",
            EventKind::SANDBOX_CREATED.as_str(),
            other.kind().as_str(),
        ),
    }
}

/// All snapshots taken in the conversation, oldest-first. Each tuple is
/// `(snapshot_id, sandbox_id_it_was_taken_from)`.
async fn list_snapshots(
    conversation: &dyn ConversationHandle,
) -> Result<Vec<(SnapshotId, SandboxId)>> {
    let mut out = Vec::new();
    let mut cursor: Option<EventId> = None;
    loop {
        let result = conversation
            .get_events(Some(EventQuery {
                cursor,
                direction: Some(EventQueryDirection::Asc),
                limit: Some(100),
                session_id: None,
                turn_id: None,
                types: Some(vec![EventKind::SANDBOX_SNAPSHOTTED]),
            }))
            .await?;
        let events_empty = result.events.is_empty();
        for event in result.events {
            match event.data {
                EventData::SandboxSnapshotted {
                    sandbox_id,
                    snapshot_id,
                } => out.push((snapshot_id, sandbox_id)),
                other => {
                    anyhow::bail!(
                        "type-filtered query for {} returned unexpected variant {}",
                        EventKind::SANDBOX_SNAPSHOTTED.as_str(),
                        other.kind().as_str(),
                    );
                }
            }
        }
        if events_empty || result.cursor.is_none() {
            break;
        }
        cursor = result.cursor;
    }
    Ok(out)
}

/// Find the sandbox a particular snapshot was taken from, by scanning the
/// `SandboxSnapshotted` events.
async fn sandbox_id_for_snapshot(
    conversation: &dyn ConversationHandle,
    target: SnapshotId,
) -> Result<Option<SandboxId>> {
    let snapshots = list_snapshots(conversation).await?;
    Ok(snapshots
        .into_iter()
        .find(|(snapshot_id, _)| *snapshot_id == target)
        .map(|(_, sandbox_id)| sandbox_id))
}

#[cfg(test)]
mod tests {
    use super::{Verbosity, render_external_event, render_user_content_for_history};
    use executor::EventData;
    use lingua::Message;
    use lingua::universal::UserContent;

    #[test]
    fn renders_scheduled_task_wakeup_user_messages() {
        let rendered = render_external_event(
            &EventData::Messages {
                messages: vec![Message::User {
                    content: UserContent::String(
                        "Scheduled task `joke` completed.\n\nstdout preview:\nhello".to_string(),
                    ),
                }],
                response_id: None,
                usage: None,
            },
            Verbosity::Compact,
        );

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].contains("user: Scheduled task `joke` completed."));
        assert!(rendered[0].contains("stdout preview:\nhello"));
    }

    #[test]
    fn renders_user_string_content_for_history() {
        let content = UserContent::String("first second".to_string());

        assert_eq!(render_user_content_for_history(&content), "first second");
    }
}
