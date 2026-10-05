//! Which failed async replies the reader writes to the log.
//!
//! Every async waiter turns an error reply into its safe default, so the
//! log line is the only record that the request failed. A request that
//! keeps failing the same way, such as a liveness probe sent every two
//! seconds, writes that line once: the next one is written when the same
//! kind of request succeeds and then fails again, or fails with another
//! error.

use std::collections::HashMap;
use std::mem::Discriminant;

use rmpv::Value;

use super::Waiter;

/// The error each kind of async waiter last failed with, for the kinds
/// whose last reply was a failure.
#[derive(Default)]
pub(super) struct FailureLog {
    last: HashMap<Discriminant<Waiter>, Value>,
}

impl FailureLog {
    /// Folds one reply into the record, answering whether it starts a run
    /// of failures and so has to be logged.
    ///
    /// A synchronous `Reply` waiter is never logged, because its caller is
    /// handed the error itself, and a reply no waiter claimed has no kind.
    pub(super) fn starts_a_run(&mut self, waiter: Option<&Waiter>, error: &Value) -> bool {
        let Some(waiter) = waiter.filter(|w| !matches!(w, Waiter::Reply(_))) else {
            return false;
        };
        let kind = std::mem::discriminant(waiter);
        if *error == Value::Nil {
            if !self.last.is_empty() {
                self.last.remove(&kind);
            }
            return false;
        }
        if self.last.get(&kind) == Some(error) {
            return false;
        }
        self.last.insert(kind, error.clone());
        true
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::handle::CheckTimeCall;
    use std::sync::mpsc;
    use view_core::native::geometry::NativeSurface;

    /// One waiter of every kind, at the index [`kind_index`] gives it.
    fn every_kind() -> Vec<Waiter> {
        vec![
            Waiter::Reply(mpsc::channel().0),
            Waiter::HlProbe { generation: 1 },
            Waiter::AccentProbe { generation: 1 },
            Waiter::Heartbeat { generation: 1 },
            Waiter::MappingClaims,
            Waiter::Takeover,
            Waiter::NotifySinkProbe,
            Waiter::SwapRecovery { generation: 1 },
            Waiter::BufferList { generation: 1 },
            Waiter::NativeWindow {
                generation: 1,
                surface: NativeSurface::Tree,
            },
            Waiter::LoadHidden {
                generation: 1,
                path: "p".to_owned(),
            },
            Waiter::Preview {
                generation: 1,
                path: "p".to_owned(),
            },
            Waiter::FloatRows { win: 1 },
            Waiter::Rename { generation: 1 },
            Waiter::CreatePrompt { generation: 1 },
            Waiter::RenamePrompt {
                generation: 1,
                old_path: "p".to_owned(),
            },
            Waiter::DeleteConfirm {
                generation: 1,
                path: "p".to_owned(),
            },
            Waiter::AiFs {
                request_id: 1,
                write: false,
            },
            Waiter::Checktime(CheckTimeCall {
                request_id: 1,
                paths: Vec::new(),
                forced: false,
            }),
            Waiter::Opened { generation: 1 },
        ]
    }

    /// Exhaustive with no wildcard, so a waiter kind added to the enum
    /// fails to compile here until [`every_kind`] carries a sample of it.
    fn kind_index(waiter: &Waiter) -> usize {
        match waiter {
            Waiter::Reply(_) => 0,
            Waiter::HlProbe { .. } => 1,
            Waiter::AccentProbe { .. } => 2,
            Waiter::Heartbeat { .. } => 3,
            Waiter::MappingClaims => 4,
            Waiter::Takeover => 5,
            Waiter::NotifySinkProbe => 6,
            Waiter::SwapRecovery { .. } => 7,
            Waiter::BufferList { .. } => 8,
            Waiter::NativeWindow { .. } => 9,
            Waiter::LoadHidden { .. } => 10,
            Waiter::Preview { .. } => 11,
            Waiter::FloatRows { .. } => 12,
            Waiter::Rename { .. } => 13,
            Waiter::CreatePrompt { .. } => 14,
            Waiter::RenamePrompt { .. } => 15,
            Waiter::DeleteConfirm { .. } => 16,
            Waiter::AiFs { .. } => 17,
            Waiter::Checktime(_) => 18,
            Waiter::Opened { .. } => 19,
        }
    }

    #[test]
    fn every_async_kind_logs_its_failure_and_a_reply_never_does() {
        let kinds = every_kind();
        let indices: Vec<usize> = kinds.iter().map(kind_index).collect();
        assert_eq!(
            indices,
            (0..kinds.len()).collect::<Vec<_>>(),
            "every_kind carries one sample of each kind, in kind_index order"
        );
        let mut log = FailureLog::default();
        let error = Value::from("boom");
        for waiter in &kinds {
            let logged = log.starts_a_run(Some(waiter), &error);
            let is_reply = matches!(waiter, Waiter::Reply(_));
            assert_eq!(
                logged, !is_reply,
                "{waiter:?}: a failed async reply is logged, and a \
                 reply's caller holds its own error"
            );
        }
        assert!(
            !log.starts_a_run(None, &error),
            "an unclaimed reply has no kind"
        );
    }

    #[test]
    fn a_run_of_one_failure_logs_once_until_it_answers_or_changes() {
        let mut log = FailureLog::default();
        let beat = Waiter::Heartbeat { generation: 1 };
        let other = Waiter::Preview {
            generation: 1,
            path: "p".to_owned(),
        };
        let boom = Value::from("boom");
        assert!(log.starts_a_run(Some(&beat), &boom), "the first failure");
        assert!(
            !log.starts_a_run(Some(&beat), &boom),
            "the same failure again"
        );
        assert!(
            log.starts_a_run(Some(&other), &boom),
            "another kind failing the same way starts its own run"
        );
        assert!(
            log.starts_a_run(Some(&beat), &Value::from("bang")),
            "a different error starts a new run"
        );
        assert!(!log.starts_a_run(Some(&beat), &Value::Nil), "a success");
        assert!(
            log.starts_a_run(Some(&beat), &Value::from("bang")),
            "the first failure after a success"
        );
    }
}
