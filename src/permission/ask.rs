use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub type AskSender = mpsc::Sender<AskRequest>;
pub type AskReceiver = mpsc::Receiver<AskRequest>;

#[derive(Debug)]
pub struct AskRequest {
    pub tool: String,
    pub input: String,
    /// Why an `approval_provider` flagged this call, when the prompt is an
    /// escalated evaluator denial (dirge-r16x). `None` for an ordinary
    /// permission prompt. Shown to the user so they know what the evaluator
    /// objected to before they decide.
    pub reason: Option<String>,
    pub reply: oneshot::Sender<UserDecision>,
}

#[derive(Debug, Clone)]
pub enum UserDecision {
    AllowOnce,
    AllowAlways(String),
    Deny,
}
