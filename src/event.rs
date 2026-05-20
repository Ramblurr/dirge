use compact_str::CompactString;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Token(CompactString),
    Reasoning(CompactString),
    ToolCall {
        name: CompactString,
        args: serde_json::Value,
    },
    ToolResult {
        output: CompactString,
    },
    Error(CompactString),
    Done {
        response: CompactString,
        tokens: u64,
        cost: f64,
    },
    /// The runner observed an interjection request at a tool-result boundary
    /// and stopped the stream cleanly. Whatever assistant text had streamed
    /// so far is captured in `partial_response`. The UI is expected to
    /// commit it as an assistant message and then drain its interjection
    /// queue as the next user turn.
    Interjected {
        partial_response: CompactString,
        tokens: u64,
    },
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Key(crossterm::event::KeyEvent),
    ScrollUp,
    ScrollDown,
    #[allow(dead_code)]
    MouseDown {
        row: u16,
        col: u16,
    },
    #[allow(dead_code)]
    MouseDrag {
        row: u16,
        col: u16,
    },
    #[allow(dead_code)]
    MouseUp {
        row: u16,
        col: u16,
    },
    Paste(String),
}
