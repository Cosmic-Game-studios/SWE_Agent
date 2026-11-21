//! Placeholder, replaced by the vendor CLI engines.

use swe_protocol::{EventKind, ROOT_AGENT, SessionMeta, TurnOutcome};

use crate::agent::Env;
use crate::session::UserInput;

pub(crate) async fn run_turn(env: &Env, meta: &SessionMeta, _input: UserInput) -> (TurnOutcome, Option<String>) {
    let message = format!("engine `{}` is not available yet", meta.engine.as_str());
    env.session.emit(ROOT_AGENT, EventKind::Error { message: message.clone() });
    (TurnOutcome::Error, Some(message))
}
