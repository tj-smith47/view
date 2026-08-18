//! State for a modal confirm-class prompt: nvim's own `msg_show` question
//! (kind `"confirm"`) paired with the answer choices that arrive separately
//! on `cmdline_show`.
//!
//! Every shape captured live against the pinned engine -- `:confirm()`, the
//! swapfile ATTENTION dialog, and `inputlist()` -- arrives as the same
//! `msg_show` kind, `"confirm"`; the difference between a single-key choice
//! prompt and a free-text one is entirely in the paired `cmdline_show`
//! prompt string. See `docs/prompt-overlay-wire-capture.md` for the
//! verbatim captures this reads. `return_prompt` was captured too, and
//! found to no longer exist on the wire at all on the pinned build (removed
//! upstream between the version the as-built doc assumed and this one, per
//! nvim's own `news.txt`); `is_prompt`'s single `kind == "confirm"` check
//! already covers every reachable case, so there is no third code path to
//! add for it.

use std::path::{Path, PathBuf};

use crate::model::{CmdlineState, MessageEntry};

/// One accelerator choice parsed from nvim's own bracket/paren convention,
/// e.g. `[Y]es, (N)o: ` -> `{ key: 'y', label: "Yes", default: true }`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    /// The single key that selects this choice, lower-cased: nvim matches
    /// confirm() choices case-insensitively (`:help confirm()`).
    key: char,
    label: String,
    /// Whether this is the bracketed choice, selected by a bare `<CR>`.
    default: bool,
}

/// What this prompt is waiting for. Both wire shapes share one `msg_show`
/// kind; this is derived from the paired `cmdline_show` prompt text, the
/// only place the two actually differ (see the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Answer {
    /// The paired `cmdline_show` has not arrived yet. Refuses every key: a
    /// keystroke that races the two events must not reach the engine as an
    /// answer to choices view has not read yet.
    Pending,
    /// A `:confirm()`-style prompt, or the swapfile ATTENTION dialog, which
    /// captures identically: one key resolves it.
    Choices(Vec<Choice>),
    /// A prompt whose `cmdline_show` carries no bracket/paren accelerator
    /// list, so its answer is typed text echoed back by nvim itself on every
    /// `cmdline_show`, submitted with `<CR>`. Two wire-identical shapes share
    /// this variant: nvim's own `inputlist()` (`digits_only: true` -- only a
    /// digit, plus the keys its prompt text documents, resolve it) and the
    /// `nvim_echo(kind = "confirm") + vim.fn.input()` pattern the tree's
    /// create/rename prompts use (`digits_only: false` -- any single
    /// printable character is typed text). [`is_inputlist_prompt`]
    /// distinguishes them by nvim's own fixed `inputlist()` prompt string,
    /// the only place the two differ on the wire (see
    /// `docs/tree-input-prompt-wire-capture.md` for the live capture proving
    /// a plain `input()` carries no such text).
    FreeText { typed: String, digits_only: bool },
}

/// Where an already-open confirm-class prompt's answer goes.
///
/// Every prompt built through [`PromptState::from_entry`] is relayed from a
/// real nvim `msg_show`, blocked in its own input loop until the accepted
/// key reaches it as `RpcCall::Input`. [`PromptState::ai_trust_prompt`] is
/// the one exception: view raises that question itself, with nothing on the
/// wire waiting for an answer, so it resolves locally instead.
/// `update()`'s key-routing arm reads this to decide which of the two an
/// accepted key does, and the lazy-dismiss timing `Msg::Key` applies to a
/// resolved nvim prompt (see that arm's own doc) must never fire for the
/// local case, since there is no paired `cmdline_show` to have gone quiet.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    Engine,
    AiTrust { project_root: PathBuf, verb: String },
}

/// One open confirm-class prompt: the question text plus whatever choices
/// or free-text state its paired `cmdline_show` has revealed so far.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptState {
    message: String,
    answer: Answer,
    origin: Origin,
}

impl PromptState {
    /// `Some` for a `msg_show` that opens a confirm-class prompt (nvim's
    /// `"confirm"` kind -- see `MessageEntry::is_prompt`), `None` for every
    /// other message. The returned state answers no keys yet: its choices
    /// arrive separately, from the `cmdline_show` `update` feeds through
    /// [`PromptState::learn_cmdline`].
    #[must_use]
    pub fn from_entry(e: &MessageEntry) -> Option<Self> {
        if !e.is_prompt() {
            return None;
        }
        Some(Self {
            message: e.lines().join("\n"),
            answer: Answer::Pending,
            origin: Origin::Engine,
        })
    }

    /// The per-project AI trust confirm: view raises this itself the first
    /// time a session invokes an AI feature with `Model::ai_trusted` still
    /// `false`, rather than relaying an nvim `msg_show`. Its choices are
    /// known up front (`Yes`/`No`, `Yes` bracketed as the default), unlike
    /// [`PromptState::from_entry`]'s `Answer::Pending` start, since there is
    /// no paired `cmdline_show` to learn them from. `message` is the
    /// question text; owned by the caller (`update/mod.rs`'s
    /// `open_ai_trust_prompt`) so this module carries no AI-specific prose
    /// of its own. `verb` is the pending `Msg::FeatureInvoke`'s own field,
    /// carried through so an affirmative answer can re-dispatch the
    /// invocation this gate interrupted -- see [`PromptState::ai_trust_verb`].
    #[must_use]
    pub fn ai_trust_prompt(project_root: PathBuf, verb: String, message: String) -> Self {
        Self {
            message,
            answer: Answer::Choices(vec![
                Choice {
                    key: 'y',
                    label: "Yes".to_string(),
                    default: true,
                },
                Choice {
                    key: 'n',
                    label: "No".to_string(),
                    default: false,
                },
            ]),
            origin: Origin::AiTrust { project_root, verb },
        }
    }

    /// `Some` when this prompt resolves locally instead of forwarding an
    /// accepted key to the engine as `RpcCall::Input` -- the project root
    /// the AI trust decision applies to. See [`Origin`]'s own doc.
    #[must_use]
    pub(crate) fn ai_trust_project_root(&self) -> Option<&Path> {
        match &self.origin {
            Origin::Engine => None,
            Origin::AiTrust { project_root, .. } => Some(project_root),
        }
    }

    /// The `verb` half of an open [`PromptState::ai_trust_prompt`]'s pending
    /// `FeatureInvoke`, for a caller resolving the prompt to re-dispatch it
    /// on an affirmative answer -- `None` for the same engine-relayed case
    /// [`PromptState::ai_trust_project_root`] returns `None` for.
    #[must_use]
    pub(crate) fn ai_trust_verb(&self) -> Option<&str> {
        match &self.origin {
            Origin::Engine => None,
            Origin::AiTrust { verb, .. } => Some(verb.as_str()),
        }
    }

    /// Whether an already-[`accepts`](Self::accepts)-ed `notation` selects
    /// this prompt's bracketed default choice, for a caller resolving a
    /// [`PromptState::ai_trust_project_root`] prompt to the trusted/declined
    /// bool `Effect::AiTrustSet` carries. `<CR>` follows the default the
    /// same way [`PromptState::view`] reads it; every other accepted key --
    /// including `<Esc>` -- is the non-default answer, since a two-choice
    /// confirm has exactly one alternative to its default. `false` for a
    /// prompt with no choices yet (`Answer::Pending`) or free-text state,
    /// neither of which this method's one caller ever reaches.
    #[must_use]
    pub(crate) fn accepted_is_default(&self, notation: &str) -> bool {
        let Answer::Choices(choices) = &self.answer else {
            return false;
        };
        if notation == "<CR>" {
            return choices.iter().any(|c| c.default);
        }
        let Some(c) = notation.chars().next() else {
            return false;
        };
        choices
            .iter()
            .any(|choice| choice.default && choice.key == c.to_ascii_lowercase())
    }

    /// Reads this prompt's choices (or free-text echo) off the paired
    /// `cmdline_show`. Covers both the first arrival and every re-arm after
    /// an unmatched key, since the two look identical on the wire (captured
    /// in `docs/prompt-overlay-wire-capture.md`).
    pub(crate) fn learn_cmdline(&mut self, cmdline: &CmdlineState) {
        self.answer = match parse_choices(&cmdline.prompt) {
            Some(choices) => Answer::Choices(choices),
            None => Answer::FreeText {
                typed: cmdline.content.iter().map(|(_, t)| t.as_str()).collect(),
                digits_only: is_inputlist_prompt(&cmdline.prompt),
            },
        };
    }

    /// Whether `notation` is one of this prompt's captured choice keys.
    ///
    /// A choice prompt accepts its accelerator letters case-insensitively,
    /// `<CR>` (selects the bracketed default), and `<Esc>` (cancels) --
    /// all three empirically resolve the dialog rather than re-arming it.
    /// A free-text prompt accepts a digit, `<BS>`, `<CR>`, `<Esc>`, and `q`
    /// -- the keys its own prompt text documents as accepted
    /// ("...or click with the mouse (q or empty cancels)"). Every other key
    /// is refused: nvim re-arms the prompt rather than dismissing it, and
    /// the overlay must do the same.
    #[must_use]
    pub fn accepts(&self, notation: &str) -> bool {
        match &self.answer {
            Answer::Pending => false,
            Answer::Choices(choices) => {
                if notation == "<CR>" || notation == "<Esc>" {
                    return true;
                }
                let mut chars = notation.chars();
                let Some(c) = chars.next() else {
                    return false;
                };
                if chars.next().is_some() {
                    return false;
                }
                choices
                    .iter()
                    .any(|choice| choice.key == c.to_ascii_lowercase())
            }
            Answer::FreeText {
                digits_only: true, ..
            } => {
                matches!(notation, "<CR>" | "<Esc>" | "<BS>" | "q")
                    || notation
                        .chars()
                        .next()
                        .is_some_and(|c| notation.chars().count() == 1 && c.is_ascii_digit())
            }
            // an ordinary `input()` prompt (see `Answer::FreeText`'s doc):
            // `q` is not nvim's documented cancel key here the way it is for
            // `inputlist()` -- it is an ordinary character a filename can
            // contain -- so it falls through to the single-printable-char
            // check like any other letter rather than being special-cased
            Answer::FreeText {
                digits_only: false, ..
            } => {
                matches!(notation, "<CR>" | "<Esc>" | "<BS>")
                    || notation
                        .chars()
                        .next()
                        .is_some_and(|c| notation.chars().count() == 1 && !c.is_control())
            }
        }
    }

    /// This prompt's paint-facing projection.
    ///
    /// Single-style rows are enough: nvim's own accelerator convention
    /// already names one choice the default, and the existing
    /// selected-row highlight (the same mechanism a picker's candidate row
    /// uses) is exactly the right amount of emphasis for it. Nothing here
    /// needs a style change mid-row, so this does not need per-span style.
    #[must_use]
    pub fn view(&self) -> super::views::PromptView {
        let base = super::views::PromptView::new("Confirm", self.message.clone());
        match &self.answer {
            Answer::Pending => base,
            Answer::Choices(choices) => {
                let selected = choices.iter().position(|c| c.default);
                let view = base.with_choices(choices.iter().map(|c| c.label.clone()).collect());
                match selected {
                    Some(idx) => view.with_selected(idx),
                    None => view,
                }
            }
            Answer::FreeText { typed, .. } => base.with_input(typed.clone()),
        }
    }
}

/// Parses a `cmdline_show` prompt string into accelerator choices, or
/// `None` if it does not follow nvim's `[X]abel, (Y)abel: ` convention
/// (an `inputlist()` free-text prompt, captured as plain instructional
/// text with no leading bracket on its first segment).
fn parse_choices(prompt_text: &str) -> Option<Vec<Choice>> {
    let trimmed = prompt_text.trim_end();
    let trimmed = trimmed.strip_suffix(':').unwrap_or(trimmed).trim_end();
    if trimmed.is_empty() {
        return None;
    }
    let mut choices = Vec::new();
    for segment in trimmed.split(", ") {
        let mut chars = segment.chars();
        let open = chars.next()?;
        let (close, default) = match open {
            '[' => (']', true),
            '(' => (')', false),
            _ => return None,
        };
        let key = chars.next()?;
        if chars.next() != Some(close) {
            return None;
        }
        let rest: String = chars.collect();
        choices.push(Choice {
            key: key.to_ascii_lowercase(),
            label: format!("{key}{rest}"),
            default,
        });
    }
    (!choices.is_empty()).then_some(choices)
}

/// True when a `cmdline_show` prompt string is nvim's own `inputlist()`
/// prompt, identified by the fixed instructional suffix nvim appends to
/// every `inputlist()` invocation regardless of the caller's own text.
/// Anything else sharing the wire shape (the tree's `vim.fn.input()`
/// prompts, e.g. "New file: ") is a different, non-digit-only free-text
/// prompt and must not match this.
fn is_inputlist_prompt(prompt_text: &str) -> bool {
    prompt_text.contains("Type number and <Enter>")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(kind: &str, content: &str) -> MessageEntry {
        // MessageEntry has no public constructor -- built the same way
        // `model.rs`'s own tests do, through a real `Messages::push`.
        let mut messages = crate::model::Messages::default();
        messages.push(kind.to_string(), vec![(0, content.to_string())], false);
        messages.entries.into_iter().next().expect("just pushed")
    }

    fn cmdline_prompt(prompt: &str) -> CmdlineState {
        CmdlineState {
            content: vec![],
            pos: 0,
            firstc: String::new(),
            prompt: prompt.to_string(),
            indent: 0,
            level: 1,
        }
    }

    #[test]
    fn from_entry_is_none_for_a_non_prompt_message() {
        assert!(PromptState::from_entry(&entry("echomsg", "hi")).is_none());
    }

    #[test]
    fn from_entry_is_some_for_a_confirm_kind_message() {
        let state = PromptState::from_entry(&entry("confirm", "Save changes?"));
        assert!(state.is_some());
    }

    #[test]
    fn a_pending_prompt_accepts_nothing() {
        let state = PromptState::from_entry(&entry("confirm", "Save changes?")).unwrap();
        assert!(!state.accepts("y"));
        assert!(!state.accepts("<CR>"));
    }

    #[test]
    fn an_unmatched_key_is_refused_so_the_prompt_stays_open() {
        // a naive "any key resolves the prompt" implementation passes every
        // other test in this module and only this one catches it
        let mut state = PromptState::from_entry(&entry("confirm", "Save changes?")).unwrap();
        state.learn_cmdline(&cmdline_prompt("[Y]es, (N)o: "));
        assert!(!state.accepts("z"), "an unmatched key must not be accepted");
        assert!(!state.accepts("q"), "q is not one of this prompt's choices");
    }

    #[test]
    fn accepts_matches_captured_choice_keys_case_insensitively() {
        let mut state = PromptState::from_entry(&entry("confirm", "Save changes?")).unwrap();
        state.learn_cmdline(&cmdline_prompt("[Y]es, (N)o: "));
        assert!(state.accepts("y"));
        assert!(state.accepts("Y"));
        assert!(state.accepts("n"));
        assert!(state.accepts("N"));
        assert!(!state.accepts("a"));
    }

    #[test]
    fn accepts_cr_and_esc_on_a_choice_prompt() {
        let mut state = PromptState::from_entry(&entry("confirm", "Save changes?")).unwrap();
        state.learn_cmdline(&cmdline_prompt("[Y]es, (N)o: "));
        assert!(
            state.accepts("<CR>"),
            "bare <CR> selects the bracketed default"
        );
        assert!(state.accepts("<Esc>"), "<Esc> cancels the dialog");
    }

    #[test]
    fn the_attention_dialogs_five_choices_all_parse() {
        let mut state =
            PromptState::from_entry(&entry("confirm", "Found a swap file by the name \"...\""))
                .unwrap();
        state.learn_cmdline(&cmdline_prompt(
            "[O]pen Read-Only, (E)dit anyway, (R)ecover, (Q)uit, (A)bort: ",
        ));
        for key in ["o", "e", "r", "q", "a"] {
            assert!(state.accepts(key), "{key} should be an accepted choice");
        }
        assert!(!state.accepts("y"));
    }

    #[test]
    fn an_inputlist_prompt_is_free_text_not_choices() {
        let mut state =
            PromptState::from_entry(&entry("confirm", "Select an option:\n1. one\n2. two"))
                .unwrap();
        state.learn_cmdline(&cmdline_prompt(
            "Type number and <Enter> or click with the mouse (q or empty cancels): ",
        ));
        assert!(state.accepts("1"));
        assert!(state.accepts("2"));
        assert!(state.accepts("<CR>"));
        assert!(state.accepts("<BS>"));
        assert!(state.accepts("q"));
        assert!(!state.accepts("z"));
    }

    #[test]
    fn free_text_input_echoes_what_nvim_itself_reports_typed() {
        let mut state = PromptState::from_entry(&entry("confirm", "Select an option:")).unwrap();
        state.learn_cmdline(&CmdlineState {
            content: vec![(0, "1".to_string())],
            pos: 1,
            firstc: String::new(),
            prompt: "Type number and <Enter> or click with the mouse (q or empty cancels): "
                .to_string(),
            indent: 0,
            level: 1,
        });
        assert_eq!(state.view().input, "1");
    }

    #[test]
    fn view_carries_the_question_and_the_choices_with_the_default_selected() {
        let mut state = PromptState::from_entry(&entry("confirm", "Save changes?")).unwrap();
        state.learn_cmdline(&cmdline_prompt("[Y]es, (N)o: "));
        let view = state.view();
        assert_eq!(view.message, "Save changes?");
        assert_eq!(view.choices, vec!["Yes".to_string(), "No".to_string()]);
        assert_eq!(view.selected, Some(0));
    }

    #[test]
    fn a_pending_prompt_view_has_no_choices_yet() {
        let state = PromptState::from_entry(&entry("confirm", "Save changes?")).unwrap();
        let view = state.view();
        assert!(view.choices.is_empty());
        assert_eq!(view.selected, None);
    }
}
