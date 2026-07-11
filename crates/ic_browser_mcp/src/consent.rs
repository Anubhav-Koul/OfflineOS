//! Asking the human before typing into a sensitive field.
//!
//! ## Why this exists here, and not in the agent runtime
//!
//! `CLAUDE.md` Phase 4 requires that "sensitive actions (fill on password/payment
//! fields) must route through the approval flow". The Reborn runtime's approval
//! flow **does not run**: `default_permission: Ask` is stamped on every discovered
//! MCP tool and then never read, no production authorizer ever returns
//! `Decision::RequireApproval`, and every active capability is minted a standing,
//! non-expiring grant. The machinery exists and is unreachable. (Reported upstream;
//! see `docs/desktop/core-patches.md`.)
//!
//! So the gate is enforced **here** — in the sidecar, which is the last thing
//! between the model and the keyboard. This is deliberate: the model cannot route
//! around it, because the sidecar decides, not the prompt. It is the same move as
//! CP-3 — enforce at the boundary we own.
//!
//! ## The channel
//!
//! The sidecar's stdout/stdin pipes, which the widget already owns as the parent
//! process. No new port, and no authentication to get wrong: a pipe is reachable
//! only by the parent. One line out, one line back:
//!
//! ```text
//! stdout →  IC_BROWSER_MCP_APPROVAL {"id":1,"url":"https://…","field":"Password", …}
//! stdin  ←  IC_BROWSER_MCP_DECISION {"id":1,"approved":false}
//! ```
//!
//! ## Fail closed, everywhere
//!
//! Every path that is not an explicit human "yes" is a **no**:
//!
//! - no approval channel at all (the sidecar run standalone) → **deny**;
//! - the widget never answers within [`APPROVAL_TIMEOUT`] → **deny**;
//! - the widget's answer is malformed, or the pipe closes → **deny**;
//! - the field could not be classified confidently → we *ask* (see [`crate::classify`]).
//!
//! A denial is a *recoverable* tool error, not a crash: the model is told the user
//! declined and can carry on doing something else.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::{Mutex, oneshot};

/// The stdout line that carries an approval request to the widget.
pub const APPROVAL_PREFIX: &str = "IC_BROWSER_MCP_APPROVAL ";
/// The stdin line that carries the human's answer back.
pub const DECISION_PREFIX: &str = "IC_BROWSER_MCP_DECISION ";

/// How long the human has to answer before we give up and deny.
///
/// Generous: reading a prompt, looking at the browser window, and deciding takes
/// real time. But not unbounded — a request nobody ever answers must not wedge the
/// agent forever.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// One request for permission to type into a field.
///
/// Carries **what will be typed and where**, not just "allow a fill?" — a consent
/// prompt the user cannot evaluate is not consent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FillApproval {
    /// Correlates the decision.
    pub id: u64,
    /// The page the field is on. The user needs to see this: a fill on
    /// `paypa1.com` is the whole attack.
    pub url: String,
    /// Whether the page is HTTPS. A password field on plain HTTP is worth flagging
    /// even when the user would otherwise approve.
    pub secure: bool,
    /// A human name for the field — its label, placeholder, or name attribute.
    pub field: String,
    /// The raw selector, for when the label is unhelpful.
    pub selector: String,
    /// Exactly what would be typed.
    ///
    /// Shown to the user, and **never logged** — see [`FillApproval::redacted`].
    pub value: String,
    /// Why this needed asking, in plain words.
    pub reason: String,
}

impl FillApproval {
    /// A copy safe to put in a log line: the value is what we are trying to
    /// protect, so it never leaves the prompt.
    pub fn redacted(&self) -> String {
        format!(
            "fill {:?} on {} ({} chars) — {}",
            self.field,
            self.url,
            self.value.chars().count(),
            self.reason
        )
    }
}

/// The human's answer.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Decision {
    /// The request this answers.
    pub id: u64,
    /// `true` only on an explicit "allow".
    pub approved: bool,
}

/// Asks a human to approve a fill.
#[async_trait]
pub trait Approver: Send + Sync + 'static {
    /// Returns `true` only if a human explicitly approved. Every other outcome —
    /// timeout, closed channel, no channel — must return `false`.
    async fn approve(&self, request: FillApproval) -> bool;
}

/// The fallback when there is no one to ask: deny.
///
/// This is what runs if the sidecar is launched standalone (no widget parent). It
/// means a sensitive fill fails with "no one to ask" rather than proceeding
/// unprompted — which is the entire point.
pub struct DenyAll;

#[async_trait]
impl Approver for DenyAll {
    async fn approve(&self, request: FillApproval) -> bool {
        tracing::warn!(
            request = %request.redacted(),
            "denying a sensitive fill: no approval channel is configured"
        );
        false
    }
}

/// Asks the parent process over stdout/stdin.
pub struct StdioApprover {
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<bool>>>>,
    stdout: Mutex<tokio::io::Stdout>,
}

impl StdioApprover {
    /// Wire up the approver and start reading decisions from stdin.
    pub fn new() -> Arc<Self> {
        let approver = Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            stdout: Mutex::new(tokio::io::stdout()),
        });
        approver.clone().spawn_decision_reader(tokio::io::stdin());
        approver
    }

    /// Read `IC_BROWSER_MCP_DECISION` lines and wake whoever is waiting.
    ///
    /// When the pipe closes, every waiter is dropped — and a dropped
    /// `oneshot::Sender` resolves the receiver to `Err`, which
    /// [`StdioApprover::approve`] treats as a denial. So losing the parent denies
    /// in-flight requests rather than hanging them.
    fn spawn_decision_reader<R>(self: Arc<Self>, reader: R)
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Some(payload) = line.strip_prefix(DECISION_PREFIX) else {
                    continue;
                };
                match serde_json::from_str::<Decision>(payload.trim()) {
                    Ok(decision) => {
                        if let Some(waiter) = self.pending.lock().await.remove(&decision.id) {
                            let _ = waiter.send(decision.approved);
                        }
                    }
                    // A malformed decision is not an approval.
                    Err(error) => {
                        tracing::warn!(%error, "ignoring a malformed approval decision");
                    }
                }
            }
            tracing::warn!("the approval channel closed; further sensitive fills will be denied");
            // Deny every in-flight request *now*, rather than letting each one wait
            // out its full timeout. Dropping the map does not suffice: the `Arc<Self>`
            // outlives this task (a waiter holds one), so the map is not dropped
            // here. Draining it drops each `Sender`, which resolves its receiver to
            // `Err` — a denial. Without this a fill in flight when the widget dies
            // hangs for the whole `APPROVAL_TIMEOUT`.
            self.pending.lock().await.clear();
        });
    }

    /// The next request id.
    pub fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

#[async_trait]
impl Approver for StdioApprover {
    async fn approve(&self, mut request: FillApproval) -> bool {
        request.id = self.next_id();
        let id = request.id;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let Ok(encoded) = serde_json::to_string(&request) else {
            self.pending.lock().await.remove(&id);
            return false;
        };

        // Ask.
        {
            let mut stdout = self.stdout.lock().await;
            let line = format!("{APPROVAL_PREFIX}{encoded}\n");
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                self.pending.lock().await.remove(&id);
                tracing::warn!("could not reach the approval channel; denying");
                return false;
            }
        }
        tracing::info!(request = %request.redacted(), "waiting for the user to approve a fill");

        // Wait. Every failure mode below is a denial.
        match tokio::time::timeout(APPROVAL_TIMEOUT, rx).await {
            Ok(Ok(true)) => true,
            Ok(Ok(false)) => {
                tracing::info!(id, "the user declined a fill");
                false
            }
            // The sender was dropped: the channel closed under us.
            Ok(Err(_)) => {
                tracing::warn!(id, "the approval channel closed while waiting; denying");
                false
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                tracing::warn!(id, "no answer within the approval timeout; denying");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn with_no_channel_a_sensitive_fill_is_denied() {
        let approved = DenyAll
            .approve(FillApproval {
                id: 0,
                url: "https://bank.example/login".into(),
                secure: true,
                field: "Password".into(),
                selector: "#pw".into(),
                value: "hunter2".into(),
                reason: "password field".into(),
            })
            .await;
        assert!(!approved, "no one to ask must mean no");
    }

    /// The value is the thing being protected; it must never reach a log.
    #[test]
    fn the_redacted_form_never_carries_the_value() {
        let request = FillApproval {
            id: 1,
            url: "https://bank.example/login".into(),
            secure: true,
            field: "Password".into(),
            selector: "#pw".into(),
            value: "correct-horse-battery-staple".into(),
            reason: "password field".into(),
        };
        let redacted = request.redacted();
        assert!(
            !redacted.contains("correct-horse"),
            "the value leaked into a log line: {redacted}"
        );
        assert!(redacted.contains("28 chars"), "{redacted}");
    }

    /// Drive the real approver over an in-memory pipe standing in for the widget.
    async fn answer_with(decision_line: Option<String>) -> bool {
        let approver = Arc::new(StdioApprover {
            next_id: AtomicU64::new(1),
            pending: Arc::new(Mutex::new(HashMap::new())),
            stdout: Mutex::new(tokio::io::stdout()),
        });

        let (mut writer, reader) = tokio::io::duplex(1024);
        approver.clone().spawn_decision_reader(reader);

        let request = FillApproval {
            id: 0,
            url: "https://example.com".into(),
            secure: true,
            field: "Password".into(),
            selector: "#pw".into(),
            value: "s3cret".into(),
            reason: "password field".into(),
        };

        let waiting = tokio::spawn({
            let approver = Arc::clone(&approver);
            async move { approver.approve(request).await }
        });

        // Let the request register before answering it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(line) = decision_line {
            writer.write_all(line.as_bytes()).await.expect("write");
            writer.flush().await.expect("flush");
        }
        drop(writer);

        waiting.await.expect("join")
    }

    #[tokio::test]
    async fn an_explicit_yes_approves() {
        assert!(
            answer_with(Some(format!(
                "{DECISION_PREFIX}{{\"id\":1,\"approved\":true}}\n"
            )))
            .await
        );
    }

    #[tokio::test]
    async fn an_explicit_no_denies() {
        assert!(
            !answer_with(Some(format!(
                "{DECISION_PREFIX}{{\"id\":1,\"approved\":false}}\n"
            )))
            .await
        );
    }

    /// The channel dying must not hang the agent, and must not approve.
    #[tokio::test]
    async fn a_closed_channel_denies_rather_than_hanging() {
        assert!(!answer_with(None).await);
    }

    /// Garbage on the wire is not consent.
    #[tokio::test]
    async fn a_malformed_decision_denies() {
        assert!(!answer_with(Some(format!("{DECISION_PREFIX}not-json\n"))).await);
    }

    /// A decision for a *different* request must not satisfy this one — otherwise
    /// approving a benign fill would silently authorize a sensitive one.
    #[tokio::test]
    async fn a_decision_for_another_request_does_not_approve_this_one() {
        assert!(
            !answer_with(Some(format!(
                "{DECISION_PREFIX}{{\"id\":999,\"approved\":true}}\n"
            )))
            .await
        );
    }
}
