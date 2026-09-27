//! Routing one keypress to whatever owns the keyboard: the engine, a native
//! pane, or the overlay on top of the stack, with the chord bindings, the
//! permission gate and the Meta reading every one of them shares.

use crate::model::{Focus, Model, OverlayKind};
use crate::msg::{Effect, RpcCall};
use crate::native::ai_panel::TranscriptScroll;
use crate::native::geometry::NativeSurface;
use crate::native::keys::{Action, Resolved};
use crate::native::submit_hold::Sequence;
use crate::native::toast::HoldOutcome;

use super::{ai, message_history_key, path_to_wire, surfaces};

/// Whether `notation` may reach past an unanswered permission request to
/// the panel's own arm beneath it -- un-entering the panel, interrupting a
/// turn, dismissing a crash banner, or re-widthing the panel itself.
///
/// The resize pair is here rather than being swallowed the way a scroll key
/// is, because the two are not the same kind of key: a scroll moves a
/// transcript window the question is painted over, so it would land
/// somewhere the reader cannot see, while a width decides nothing the
/// question owns and re-lays out whatever is on screen -- a panel too
/// narrow to read the request in is exactly when a user reaches for it.
///
/// A closed list rather than "any `<...>` notation" on purpose. The
/// composer's own named keys are edits like any other keystroke: `<CR>`
/// starts a turn, `<BS>`, `<lt>` (nvim's escape for a literal `<`, see
/// `keys::encode_key`) and the bound line break type into the prompt.
/// Letting those through because they are spelled with angle brackets
/// would leave the composer editable behind a decision the user has not
/// made yet -- with `<CR>` able to start a second turn on top of the one
/// whose permission request is still on screen -- while the plain
/// characters beside them are swallowed.
///
/// Takes the model because one entry is conditional: `<C-d>` is two keys
/// wearing one notation, and only the banner-dismissing one is a way out.
/// Takes `binding` already resolved because the answer for a chord depends
/// on the prefix the previous keystroke left waiting, which is state this
/// predicate must not consume.
pub(super) fn reaches_past_a_panel_owner(
    model: &Model,
    notation: &str,
    binding: Option<Resolved>,
) -> bool {
    // A chord's first key is here too, though it moves nothing: the prefix
    // it leaves waiting is recorded before this gate either way, so what
    // reaching past buys it is only that it takes the same path as the
    // press that completes it rather than a second, silent one. The
    // composer's own line break is not a way past anything: the composer is
    // exactly what a panel owner is standing in front of.
    if matches!(
        binding,
        Some(Resolved::Pending | Resolved::Act(Action::Resize(_)))
    ) {
        return true;
    }
    match notation {
        "<Esc>" | "<C-c>" => true,
        // A way out only while there is a banner to dismiss. With none,
        // this is the half-page scroll key (see [`ai_scroll_for`]), which
        // is not a way out of anything: letting it through would scroll a
        // transcript the pending question is painted over, while every
        // other scroll key at the same question is swallowed.
        "<C-d>" => model.ai_panel().local_error.is_some(),
        _ => false,
    }
}

/// What `notation` means to the focused surface, consuming whichever chord
/// prefix the previous keystroke left waiting.
///
/// The one place a key is resolved against the bindings: the tree, the
/// agent panel and its composer share one configured set
/// ([`Model::key_bindings`]), and a second reading of it here or there is
/// how one surface comes to answer a key another ignores. The prefix is
/// taken rather than read because a keystroke consumes it whatever it turns
/// out to mean -- an unmatched follower falls through to its ordinary
/// handling with nothing left pending behind it.
///
/// A resize's direction reads the way nvim's own `<C-w><` and `<C-w>>` do
/// -- left narrows and right widens whichever edge the sidebar is pinned to
/// -- rather than following the moving edge, which would invert between the
/// tree and the panel.
pub(super) fn take_binding(model: &mut Model, notation: &str) -> Option<Resolved> {
    let pending = model.pending_chord.take();
    let resolved = model.key_bindings.resolve(pending.as_deref(), notation);
    if resolved == Some(Resolved::Pending) {
        model.pending_chord = Some(notation.to_string());
    }
    resolved
}

/// Which way `notation` moves the AI panel's transcript window, or `None`
/// for a key that does not scroll it.
///
/// Every one of them is a named notation the composer cannot type, which is
/// what lets the transcript scroll while a half-written prompt sits on the
/// composer line. `<C-d>` is here too, but its own arm in `route_key`
/// reaches it first and gives a crash banner the key while one is up.
pub(super) fn ai_scroll_for(notation: &str) -> Option<TranscriptScroll> {
    match notation {
        "<PageUp>" => Some(TranscriptScroll::PageBack),
        "<PageDown>" => Some(TranscriptScroll::PageForward),
        "<C-u>" => Some(TranscriptScroll::HalfPageBack),
        "<C-d>" => Some(TranscriptScroll::HalfPageForward),
        _ => None,
    }
}

/// Scrolls the AI panel's transcript for one scroll key, reporting whether
/// the window moved.
///
/// Carries no guard of its own against a permission prompt owning the
/// keys: [`reaches_past_a_panel_owner`] is the single place that decides
/// what gets this far, so a second opinion here could only ever disagree
/// with it.
pub(super) fn scroll_ai_transcript(model: &mut Model, notation: &str) -> bool {
    let Some(scroll) = ai_scroll_for(notation) else {
        return false;
    };
    let (height, width) = ai_panel_size(model);
    model
        .ai_panel_mut()
        .scroll_transcript(scroll, height, width)
}

/// The open AI panel's own resolved size in terminal cells, rows first --
/// the same numbers `view-surface` paints it at, so a page moves the window
/// by exactly what the panel last drew, over the rows a wrapped composer
/// left it.
fn ai_panel_size(model: &Model) -> (usize, usize) {
    model
        .overlays()
        .iter()
        .rev()
        .find(|overlay| matches!(overlay.kind, OverlayKind::Ai))
        .map_or((0, 0), |overlay| {
            let rect = model.overlay_rect(overlay);
            (usize::from(rect.height), usize::from(rect.width))
        })
}

/// Routes one keypress to whatever currently owns the keyboard, after
/// [`note_supervision_choice`](super::supervision::note_supervision_choice)
/// has had its look at it.
///
/// Split out of [`update`](super::update)'s `Msg::Key` arm so that the supervision modal's
/// bookkeeping can run first without consuming the key: this is the routing
/// a keypress gets whether or not that modal is on screen, which is the
/// whole of what makes that modal free to answer.
///
/// `modal_was_open` is that modal's state as the key *arrived*, which the
/// caller has to carry in because the bookkeeping above may already have
/// closed it.
fn route_key(model: &mut Model, notation: String, modal_was_open: bool) -> Vec<Effect> {
    // a sequence is held by the surface it was typed on, and a key that
    // reaches any other finds nothing held
    if !holds_sequences(model) {
        model.submit_hold.take_sequence();
    }
    let cmdline_open = model.engine.cmdline.is_some();
    model.dirty |= model
        .engine
        .messages
        .resolve_startup_hold(HoldOutcome::Release);
    // the fallback, not the rule: a prompt view itself answered retires on
    // the `cmdline_hide` that key causes (see `UiEvent::CmdlineHide`), and
    // this catches only the prompt nothing view sent resolved -- nvim's own
    // Lua answering its own question, where view forwarded no key to set
    // the flag that arm reads. The question's own entry goes with the
    // overlay: a prompt routes `Route::Prompt` and so owns no idle timer,
    // and nvim sends no msg_clear on resolution, so nothing else would ever
    // take it down.
    // excludes the AI trust prompt and the external-write conflict prompt:
    // neither has a paired cmdline_show to have gone quiet, so
    // `cmdline_open` reads `false` for either from the moment it opens, and
    // this heuristic would otherwise pop it before the answer arm below
    // ever sees the keystroke meant to resolve it
    if !cmdline_open && relays_a_prompt(model) {
        model.pop_focused_overlay();
        let _ = model.engine.messages.dismiss_answered_prompt();
        model.dirty = true;
    }
    // <Esc> closes a picker sitting directly on top of the stack.
    // Checked here, ahead of the focus match below, so a picker
    // buried under a still-open prompt (the stacking rule a modal
    // prompt keeps its focus, see `OverlayKind::Picker`'s doc) never
    // sees this: `focused_overlay_mut` names the prompt in that case,
    // not the picker, and the pattern below simply does not match.
    if notation == "<Esc>"
        && matches!(
            model.focused_overlay_mut().map(|ov| &ov.kind),
            Some(OverlayKind::Picker(_))
        )
    {
        model.pop_focused_overlay();
        // without this the closed picker stays on screen until some
        // unrelated event repaints: the paint loop's `if model.dirty`
        // gate is the only repaint trigger, and popping an overlay
        // produces no engine redraw to trip it
        model.dirty = true;
        // tells the matcher worker to drop its live Session so a
        // Files scan still walking a huge tree does not keep
        // running unobserved -- see Effect::PickerClose's doc; the
        // session-replacement path in the worker only fires on a
        // later query for a different source, which closing here
        // may never produce
        return vec![Effect::PickerClose];
    }
    match model.focus() {
        // `native_pane_focus()` reads nvim's own cursor grid, and the
        // palette carries no grid of its own to put that cursor on,
        // so this arm never actually sees `Focus::Pane(Palette)`; it is
        // grouped with the engine because typing into the command line is
        // nvim's own input-capturing mode either way, and the match needs
        // every `NativeSurface` named somewhere.
        Focus::Engine | Focus::Pane(NativeSurface::Palette) => {
            // A sticky error outlives every incidental keypress by design
            // (`MessageEntry::is_persistent`), which without a way out is a
            // box that occludes the buffer until some later error happens to
            // replace it. `<Esc>` is the way out: it is the key a reader
            // already reaches for to cancel, and the same reflex clears
            // highlights and the message line in the configs nvim users
            // arrive from.
            //
            // The key still reaches nvim either way, so nothing an `<Esc>`
            // does in the engine is shadowed and a session with no toast up
            // pays one string compare for this.
            //
            // Deliberately not narrowed to normal mode, which is what it
            // used to be. The only mode signal a UI gets is `mode_change`,
            // and that is nvim's *cursor shape* mode, which a plugin can
            // leave standing at a value `mode()` disagrees with: on a
            // default heavy launch with a config drawing its own cmdline
            // and messages -- the launch this notice exists for -- nvim
            // reports `replace` at rest and never corrects it, while
            // `mode()` answers `n` throughout (compat, a heavy
            // unaccommodated launch whose config takes `guicursor` over and
            // hands it back around its own cmdline). A mode-gated dismissal
            // is therefore not merely approximate there, it is absent: the
            // one way out of a box across the top of the buffer never
            // fires, for the whole session.
            //
            // What that costs is real and smaller: an `<Esc>` leaving
            // insert or visual takes a standing wire error with it, so a
            // sticky message can go before it has been read. Its text
            // stays readable in the message history (`<leader>fm`), and a
            // way out that always works is worth more than a dismissal
            // that is always deliberate.
            //
            // What it must not cost is a notice view raised about a
            // condition that is still true: its standing-ness is the
            // claim, and unlike the text it does not come back. Those
            // carry a family, `dismiss_sticky` leaves them alone, and
            // their own way down is the rule above -- any input, `<Esc>`
            // included, once the line has stood its reading window.
            //
            // The busy modal is excluded outright. It offers `<Esc>` as its
            // own dismissal (`SupervisionChoice::Dismiss`), and the error
            // standing behind it is usually the account of why the engine
            // wedged in the first place -- taking both with one keystroke
            // spends an answer the user made on a dismissal they did not
            // (the same reasoning `note_supervision_choice` already applies
            // to a focused overlay under the modal).
            if notation == "<Esc>" && !modal_was_open && model.engine.messages.dismiss_sticky() {
                model.dirty = true;
            }
            engine_input(model, notation)
        }
        Focus::Pane(NativeSurface::Tree) => {
            sequence_key(model, notation, modal_was_open, surfaces::tree_key)
        }
        Focus::Pane(NativeSurface::Agent) => surfaces::agent_pane_key(model, &notation),
        Focus::Pane(NativeSurface::Notifications) => sequence_key(
            model,
            notation,
            modal_was_open,
            surfaces::notifications_pane_key,
        ),
        Focus::Native(_) => match model.focused_overlay_mut().map(|ov| &mut ov.kind) {
            // an nvim-relayed prompt answers by feeding the engine a
            // keystroke -- the engine is blocked in its own input
            // loop, not on an RpcRequest, so this is the one Native
            // arm that still reaches RpcCall::Input. The AI trust
            // prompt is the one exception (see PromptState's Origin
            // doc): it resolves locally, so this returns
            // Effect::AiTrustSet instead of forwarding the key.
            Some(OverlayKind::Prompt(p)) => {
                if !p.accepts(&notation) {
                    return Vec::new();
                }
                if let Some(project_root) = p.ai_trust_project_root() {
                    let project_root = project_root.to_path_buf();
                    let verb = p.ai_trust_verb().unwrap_or_default().to_string();
                    let trusted = p.accepted_is_default(&notation);
                    model.pop_focused_overlay();
                    model.dirty = true;
                    return vec![Effect::AiTrustSet {
                        project_root,
                        trusted,
                        verb,
                    }];
                }
                // the external-write conflict prompt resolves locally too,
                // on the same terms the AI trust prompt does: "Reload" (the
                // bracketed default) re-drives `RpcCall::Checktime` with
                // `force: true` (see that field's own doc for why a bare
                // second checktime cannot re-decide what the first already
                // did), and "Keep local" issues nothing at all -- the
                // buffer's local edits are exactly what checktime's own
                // conflict branch already guaranteed it left untouched
                if let Some(path) = p.external_write_conflict_path() {
                    let path = path.to_path_buf();
                    let reload = p.accepted_is_default(&notation);
                    model.pop_focused_overlay();
                    model.dirty = true;
                    if !reload {
                        return Vec::new();
                    }
                    let request_id = model.next_checktime_request_id();
                    return vec![Effect::Rpc(RpcCall::Checktime {
                        request_id,
                        paths: vec![path_to_wire(&path)],
                        force: true,
                    })];
                }
                // recorded before the key leaves, so the `cmdline_hide` it
                // causes can tell a resolution from the wire-identical
                // re-arm an unmatched key produces
                p.note_answer(&notation);
                vec![Effect::Rpc(RpcCall::Input { notation })]
            }
            // every other key edits the query and re-asks the
            // matcher worker; edit_query itself decides what a
            // notation means (a plain char, <BS>, or a no-op it
            // still bumps the generation for), so this arm never
            // inspects notation itself.
            Some(OverlayKind::Picker(p)) => {
                let generation = p.edit_query(&notation);
                vec![picker_query(p, generation)]
            }
            Some(OverlayKind::Tree(_)) => {
                sequence_key(model, notation, modal_was_open, surfaces::tree_key)
            }
            // A pending permission request blocks the issuing agent's own
            // turn until answered; its digits and <Esc> reach it here
            // because `model.focus()` only ever names this overlay once
            // the user has deliberately entered it (`AiPanelState::focused`,
            // consulted by `Model::takes_focus_now`) -- never merely by
            // being open, and never by side effect of whatever mode the
            // engine happens to be in. With the panel not entered,
            // `model.focus()` is `Focus::Engine` instead, so every key --
            // including a digit as an ordinary engine count -- reaches
            // nvim through this same `match`'s `Focus::Engine` arm
            // untouched.
            Some(OverlayKind::Ai) => {
                // Resolved here rather than inside the panel's own handler:
                // resolving consumes the chord prefix the previous keystroke
                // left waiting, which must happen exactly once per keystroke
                // whatever the key turns out to mean.
                let binding = take_binding(model, &notation);
                ai::ai_panel_key(model, &notation, binding)
            }
            // Guarded on the notation rather than handling <Esc> itself:
            // closing is the same pop for every overlay, and the fallback
            // below is where that lives, so the history's own arm answers
            // only the keys that are its own.
            Some(OverlayKind::MessageHistory(_)) if notation != "<Esc>" => {
                sequence_key(model, notation, modal_was_open, message_history_key)
            }
            // the key belongs to the overlay on top of the stack,
            // and no other overlay kind carries a key handler yet,
            // so consuming it is the whole of that routing. <Esc>
            // closes exactly that one overlay, which is why it pops
            // rather than clearing: an overlay underneath it keeps
            // the keyboard.
            _ => {
                if notation == "<Esc>" {
                    model.pop_focused_overlay();
                    model.dirty = true;
                }
                Vec::new()
            }
        },
    }
}

/// One `Effect::PickerQuery` for the session `p` is in, at `generation`.
///
/// The single place the matcher worker's request is assembled, so a key
/// edit and a paste cannot come to disagree about what one of its fields
/// means.
pub(super) fn picker_query(p: &crate::native::picker::PickerState, generation: u64) -> Effect {
    Effect::PickerQuery {
        generation,
        needle: p.query().to_string(),
        source: p.source().clone(),
        resolved: None,
    }
}

/// Retires the Prompt overlay if it is the topmost one, and the question it
/// was asking with it -- the shared close every definitive resolution takes,
/// whether it arrives as a tree prompt-reply RPC (create/rename/delete) or as
/// the `cmdline_hide` an answered engine prompt comes back as.
///
/// The question goes with the box because it is a message entry like any
/// other, held on screen only by the cmdline that was asking it: leaving it
/// behind trades a lingering overlay for a lingering toast reading the same
/// words. nvim sends no `msg_clear` when a prompt resolves, so this is the
/// only point that ends it.
pub(super) fn dismiss_top_prompt(model: &mut Model) {
    if matches!(
        model.focused_overlay_mut().map(|ov| &ov.kind),
        Some(OverlayKind::Prompt(_))
    ) {
        model.pop_focused_overlay();
        let _ = model.engine.messages.dismiss_answered_prompt();
    }
}

/// `notation` sent to nvim as typed, with the command-line tracking the
/// typed-ahead hold keeps over every key the engine receives.
fn engine_input(model: &mut Model, notation: String) -> Vec<Effect> {
    let mut effects = crate::native::submit_hold::fold_engine_key(model, &notation);
    effects.insert(0, Effect::Rpc(RpcCall::Input { notation }));
    effects
}

/// Routes one key, reading a Meta key no binding claims as its `<Esc>`
/// first while a surface of view's own holds the keyboard.
///
/// A terminal sends Alt+x and a quick `<Esc>` then `x` as the same bytes,
/// so an `<Esc>` typed quickly before `:` arrives as `<M-:>`. The `<Esc>`
/// leaves the surface, and the Meta key follows it whole to nvim, which
/// runs a mapping of it (a desktop chord, a user's own `<M-h>`) and reads
/// an unmapped one as `<Esc>` and its key, as it does in insert mode. At
/// a prompt nvim relays, where nvim's own input loop reads the keys, the
/// Meta key is sent in the `<Esc>`'s place. At a pending permission the
/// question keeps the keyboard, and the Meta key answers nothing and types
/// nothing.
///
/// A chord nvim maps to one of view's own verbs goes to nvim whole from
/// any surface of view's own (see [`forward_invocation`]).
pub(super) fn route_unescaped(
    model: &mut Model,
    notation: String,
    modal_was_open: bool,
) -> Vec<Effect> {
    let native = !matches!(
        model.focus(),
        Focus::Engine | Focus::Pane(NativeSurface::Palette)
    );
    // the lookup is spent only while a surface of view's own holds the
    // keyboard
    let unbound = native
        && model
            .key_bindings
            .resolve(model.pending_chord.as_deref(), &notation)
            .is_none();
    if unbound && !relays_a_prompt(model) {
        if let Some(feature) = model.submit_hold.invokes(&notation).map(str::to_owned) {
            return forward_invocation(model, vec![notation], Some(&feature), modal_was_open);
        }
    }
    match crate::native::keys::escaped_key(&notation) {
        Some(_) if unbound && permission_owns_the_keys(model) => Vec::new(),
        Some(key) if unbound => {
            let mut effects = route_key(model, "<Esc>".to_string(), modal_was_open);
            if let Some(sent) = effects.iter_mut().find_map(|effect| match effect {
                Effect::Rpc(RpcCall::Input { notation: sent }) if sent == "<Esc>" => Some(sent),
                _ => None,
            }) {
                *sent = notation;
                return effects;
            }
            // a windowed surface's `<Esc>` moves nvim's cursor, which
            // `focus()` sees only once nvim redraws it there
            let left_for_nvim = model.focus() == Focus::Engine
                || effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::Rpc(RpcCall::FocusPreviousWindow)));
            if left_for_nvim {
                effects.extend(engine_input(model, notation));
            } else {
                effects.extend(route_key(model, key, modal_was_open));
            }
            effects
        }
        _ => route_key(model, notation, modal_was_open),
    }
}

/// Whether the focused overlay is a prompt nvim relays from its own input
/// loop, which reads every key itself.
fn relays_a_prompt(model: &Model) -> bool {
    matches!(
        model.focused_overlay().map(|ov| &ov.kind),
        Some(OverlayKind::Prompt(p))
            if p.ai_trust_project_root().is_none()
                && p.external_write_conflict_path().is_none()
    )
}

/// Whether the agent panel has the keyboard with a permission request
/// waiting on it, where only the keys [`reaches_past_a_panel_owner`] names
/// and the question's own answers act.
fn permission_owns_the_keys(model: &Model) -> bool {
    let panel = match model.focus() {
        Focus::Pane(NativeSurface::Agent) => true,
        Focus::Native(_) => matches!(
            model.focused_overlay().map(|ov| &ov.kind),
            Some(OverlayKind::Ai)
        ),
        _ => false,
    };
    panel && model.ai_panel().pending_permission.is_some()
}

/// A surface's own key handler, `None` for a key it does not answer.
type Answer = fn(&mut Model, &str) -> Option<Vec<Effect>>;

/// The handler of the focused surface when it answers keys of its own and
/// types no text, so a key it does not answer may begin a mapped sequence.
fn sequence_answer(model: &Model) -> Option<Answer> {
    match model.focus() {
        Focus::Pane(NativeSurface::Tree) => Some(surfaces::tree_key),
        Focus::Pane(NativeSurface::Notifications) => Some(surfaces::notifications_pane_key),
        Focus::Native(_) => match &model.focused_overlay()?.kind {
            OverlayKind::Tree(_) => Some(surfaces::tree_key),
            OverlayKind::MessageHistory(_) => Some(message_history_key),
            _ => None,
        },
        _ => None,
    }
}

fn holds_sequences(model: &Model) -> bool {
    sequence_answer(model).is_some()
}

/// Whether the user's own mappings reach nvim from the focused surface:
/// one with a window of its own, where nvim's cursor is, as it is in a
/// buffer tile.
fn passes_user_keys(model: &Model) -> bool {
    matches!(model.focus(), Focus::Pane(_))
}

/// A key for a surface that answers keys of its own and types no text.
///
/// `answer` is the surface's own handler. A key it does not answer that
/// begins a key sequence nvim maps is held, and the keys after it with
/// it, until they spell the whole sequence, which goes to nvim (see
/// [`forward_invocation`]), or part from every sequence, or nvim's own
/// `'timeoutlen'` passes (see [`resolve_held`]). So `<Space>uf` in the tree
/// floats it, and `<Space>ax` with no `<Space>a` sequence mapped opens the
/// tree's create prompt on the `a`. The sequences are view's own
/// invocations, and on a surface with a window of its own the user's own
/// mappings too.
fn sequence_key(
    model: &mut Model,
    notation: String,
    modal_was_open: bool,
    answer: Answer,
) -> Vec<Effect> {
    let mut keys = model.submit_hold.take_sequence();
    if keys.is_empty() {
        if let Some(effects) = answer(model, &notation) {
            return effects;
        }
    }
    keys.push(notation);
    match model.submit_hold.sequence(&keys, passes_user_keys(model)) {
        Sequence::Prefix => model.submit_hold.keep_sequence(keys),
        Sequence::Neither => {
            let Some(last) = keys.pop() else {
                return Vec::new();
            };
            if keys.is_empty() {
                return Vec::new();
            }
            let mut effects = resolve_held(model, keys, modal_was_open, answer);
            effects.extend(sequence_key(model, last, modal_was_open, answer));
            effects
        }
        Sequence::Complete(_) | Sequence::User => resolve_held(model, keys, modal_was_open, answer),
    }
}

/// Sends held `keys` where nvim sends them once no longer sequence can
/// follow: to the mapping they spell, or else back to the surface, which
/// answers each key after the first.
fn resolve_held(
    model: &mut Model,
    keys: Vec<String>,
    modal_was_open: bool,
    answer: Answer,
) -> Vec<Effect> {
    match model.submit_hold.spelled(&keys, passes_user_keys(model)) {
        Sequence::Complete(feature) => {
            forward_invocation(model, keys, Some(&feature), modal_was_open)
        }
        Sequence::User => forward_invocation(model, keys, None, modal_was_open),
        Sequence::Prefix | Sequence::Neither => {
            // the first key is one the surface answered nothing to
            let mut effects = Vec::new();
            for key in keys.into_iter().skip(1) {
                effects.extend(answer(model, &key).unwrap_or_default());
            }
            effects
        }
    }
}

/// Resolves the keys the focused surface holds once nvim's
/// `'timeoutlen'` has passed on them.
pub(super) fn expire_sequence(model: &mut Model, generation: u64) -> Vec<Effect> {
    let Some(answer) = sequence_answer(model) else {
        return Vec::new();
    };
    let keys = model.submit_hold.take_expired_sequence(generation);
    if keys.is_empty() {
        return Vec::new();
    }
    resolve_held(model, keys, false, answer)
}

/// Sends `keys`, which spell a key sequence nvim maps, from a surface of
/// view's own. A sequence invoking `feature` arms the typed-ahead hold
/// behind it; a user's own mapping, `feature` `None`, arms nothing.
///
/// From a surface with a window of its own the cursor stays in that
/// window, so a verb acting on the focused tile acts on the surface. A
/// floating surface is left first, the way its `<Esc>` leaves it, so a
/// verb that moves the cursor or the tabpage takes the keyboard with it.
/// Two stay: a surface invoking its own feature, whose toggle is what
/// closes it, and a panel with a permission request waiting, whose
/// `<Esc>` would cancel the request.
///
/// A window-command prefix the tree or a panel was holding goes out ahead
/// of the keys, as it does ahead of any other key those surfaces pass on.
fn forward_invocation(
    model: &mut Model,
    keys: Vec<String>,
    feature: Option<&str>,
    modal_was_open: bool,
) -> Vec<Effect> {
    let prefix = model.pending_chord.take();
    let mut effects = Vec::new();
    if matches!(model.focus(), Focus::Pane(_)) {
        if let Some(prefix) = prefix.filter(|prefix| prefix == "<C-w>") {
            effects.extend(engine_input(model, prefix));
        }
    } else if focused_feature(model) != feature && !permission_owns_the_keys(model) {
        effects.extend(route_key(model, "<Esc>".to_string(), modal_was_open));
    }
    for key in keys {
        effects.extend(engine_input(model, key));
    }
    effects
}

/// The feature whose floating surface holds the keyboard.
fn focused_feature(model: &Model) -> Option<&'static str> {
    match model.focused_overlay().map(|ov| &ov.kind)? {
        OverlayKind::Tree(_) => Some("tree"),
        OverlayKind::Ai => Some("ai"),
        OverlayKind::MessageHistory(_) => Some("notifications"),
        OverlayKind::Picker(_) => Some("picker"),
        _ => None,
    }
}
