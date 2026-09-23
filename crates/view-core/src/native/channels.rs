//! Every way nvim can draw chrome, mapped to the surface view draws there.
//!
//! A plugin reaches the screen only through channels nvim exposes, and that
//! set is enumerable from a running engine: the options it evaluates, the
//! `ext_*` capabilities a UI can take, the runtime functions a config can
//! replace, and a floating window parked over a region view paints. So a
//! surface view owns claims every channel that can draw it, and a plugin
//! released tomorrow reaches none of them without going through a channel
//! this table already names.
//!
//! Nothing here names a plugin, a Lua module or a buffer filetype. The
//! table is keyed on the surface and on nvim's own spelling of the channel,
//! which is what makes it answer for a plugin nobody has tested.
//!
//! [`NOT_CHROME`] is the other half of the enumeration: the channels view
//! deliberately leaves alone, each with the reason. Buffer decoration --
//! extmarks, virtual text, signs -- is drawn by the engine and painted as
//! it arrives, and the gutter belongs to the buffer window rather than to
//! view's chrome.

use crate::model::{Look, Panes};
use crate::msg::OptionValue;
use crate::native::ext::Ext;
use crate::native::surfaces::Surface;

/// The scope nvim reads an option at.
///
/// Deliberately not `#[non_exhaustive]`, like the three types below it: a
/// scope, a value type or a channel kind added here has to fail to compile
/// in the crate that performs takeovers, because a kind nothing performs is
/// a surface view believes it owns and does not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    /// One value for the session, read with an empty opts table.
    Global,
    /// One value per window, read with that window's handle.
    Window,
}

/// A held option's value, as a `const` table can spell it.
///
/// [`OptionValue::Str`] owns a `String`, which no `const` expression can
/// build, so a borrowed spell of the same three-variant domain is what
/// keeps a string-valued channel writable in the table at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelValue {
    /// A number option.
    Int(i64),
    /// A boolean option.
    Bool(bool),
    /// A string option.
    Str(&'static str),
    /// The two values one option takes, keyed by look.
    ///
    /// Resolved before the value reaches the wire, so no path can hold a
    /// look-keyed spelling: the derivation that reads the table copies
    /// this variant through untouched, and [`ChannelValue::wire`] is where
    /// it answers its own look's leg.
    ByLook {
        /// The value held under `panes = "nvim"`.
        nvim: &'static ChannelValue,
        /// The value held under `panes = "tiles"`.
        tiles: &'static ChannelValue,
    },
}

impl ChannelValue {
    /// This value as the wire carries it, under `look`.
    ///
    /// Total over both closed enums, so a fifth option type added to
    /// either is a compile error rather than a hold that sets nothing.
    #[must_use]
    pub fn wire(self, look: Look) -> OptionValue {
        match self {
            Self::Int(n) => OptionValue::Int(n),
            Self::Bool(b) => OptionValue::Bool(b),
            Self::Str(s) => OptionValue::Str(s.to_string()),
            Self::ByLook { nvim, tiles } => match look.panes {
                Panes::Nvim => nvim.wire(look),
                Panes::Tiles => tiles.wire(look),
            },
        }
    }

    /// Whether `look` holds this value whatever the owning feature's
    /// `[native]` switch says.
    ///
    /// The tiles leg of a look-keyed hold: the frame geometry is built on
    /// it, and the row nvim would draw for the user under it is one the
    /// tiles painter covers in every case, so handing the option back
    /// under tiles returns nothing the user can see. Under `panes = "nvim"`
    /// the switch decides, as it does for every other hold.
    #[must_use]
    pub fn held_by_look(self, look: Look) -> bool {
        matches!(self, Self::ByLook { .. }) && look.panes == Panes::Tiles
    }
}

/// A region of the grid a floating window can be parked over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Region {
    /// The rows at the foot of the grid the engine keeps for a command
    /// line.
    CmdlineBand,
    /// The grid's top-right corner, where view stacks its toasts.
    TopRightChrome,
}

/// One way a surface can be drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Channel {
    /// An option view sets and keeps at `value`, whatever writes it
    /// afterwards.
    Hold {
        /// The option name, exactly as `nvim_set_option_value` takes it.
        option: &'static str,
        /// Which scope the value is held at.
        scope: Scope,
        /// The value that leaves the surface to view.
        value: ChannelValue,
    },
    /// An option nvim evaluates only while another channel of the same
    /// surface leaves it a row to draw on, so holding that one holds this
    /// one too.
    Covered {
        /// The option name.
        option: &'static str,
        /// The channel of the same surface whose hold suppresses it.
        by: &'static str,
    },
    /// A capability nvim stops drawing the moment a UI asks for it at
    /// `nvim_ui_attach`.
    Attach(Ext),
    /// A runtime function a config can replace, held at the engine's own
    /// default.
    Replaced(&'static str),
    /// A floating window over the region this surface occupies.
    Float(Region),
}

impl Channel {
    /// nvim's own name for this channel, which is what a notice quotes and
    /// what the completeness walk matches an engine's option list against.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hold { option, .. } | Self::Covered { option, .. } => option,
            Self::Attach(ext) => ext.as_str(),
            Self::Replaced(global) => global,
            Self::Float(Region::CmdlineBand) => "a float over the command line",
            Self::Float(Region::TopRightChrome) => "a float over the message area",
        }
    }
}

/// One surface and every channel that can draw it.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceChannels {
    /// The surface these channels draw.
    pub surface: Surface,
    /// Every channel that reaches it, in the order a takeover applies
    /// them.
    pub channels: &'static [Channel],
}

/// Every surface view can own, with the channels that draw it.
///
/// A channel named by two surfaces is held only where both of them are
/// view's: the last grid row carries nvim's command line and its message
/// area both, so a session that handed either one back still needs the row
/// (`cmdheight`).
pub const CHANNELS: &[SurfaceChannels] = &[
    SurfaceChannels {
        surface: Surface::Cmdline,
        channels: &[
            Channel::Attach(Ext::Cmdline),
            Channel::Hold {
                option: "cmdheight",
                scope: Scope::Global,
                value: ChannelValue::Int(0),
            },
            Channel::Covered {
                option: "showmode",
                by: "cmdheight",
            },
            Channel::Covered {
                option: "showcmd",
                by: "cmdheight",
            },
            Channel::Covered {
                option: "ruler",
                by: "cmdheight",
            },
            Channel::Covered {
                option: "rulerformat",
                by: "cmdheight",
            },
            Channel::Float(Region::CmdlineBand),
        ],
    },
    SurfaceChannels {
        surface: Surface::Popupmenu,
        channels: &[
            Channel::Attach(Ext::Popupmenu),
            Channel::Covered {
                option: "ext_wildmenu",
                by: "ext_popupmenu",
            },
        ],
    },
    SurfaceChannels {
        surface: Surface::Messages,
        channels: &[
            Channel::Attach(Ext::Messages),
            Channel::Replaced("vim.notify"),
            Channel::Hold {
                option: "cmdheight",
                scope: Scope::Global,
                value: ChannelValue::Int(0),
            },
            Channel::Float(Region::TopRightChrome),
        ],
    },
    SurfaceChannels {
        surface: Surface::Tabline,
        channels: &[
            Channel::Attach(Ext::Tabline),
            // the one channel an attach does not cover: nvim draws a
            // window's own `winbar` inside that window's grid, so the row
            // survives every capability a UI takes at the top of the screen
            Channel::Hold {
                option: "winbar",
                scope: Scope::Window,
                value: ChannelValue::Str(""),
            },
            Channel::Covered {
                option: "tabline",
                by: "ext_tabline",
            },
            Channel::Covered {
                option: "showtabline",
                by: "ext_tabline",
            },
        ],
    },
    SurfaceChannels {
        surface: Surface::Statusline,
        channels: &[
            // keyed by look, and the only claim on the option whichever
            // leg answers: under `nvim` view draws the one bottom bar and
            // nvim draws no status line at all, and under tiles every
            // window gets a status row for the frame's bottom edge to
            // paint over. The tiles leg is held with the feature off as
            // well (`ChannelValue::held_by_look`), since the outer grid's
            // height counts on that row under the bottom tiles
            Channel::Hold {
                option: "laststatus",
                scope: Scope::Global,
                value: ChannelValue::ByLook {
                    nvim: &ChannelValue::Int(0),
                    tiles: &ChannelValue::Int(2),
                },
            },
            Channel::Covered {
                option: "statusline",
                by: "laststatus",
            },
        ],
    },
    // the frame is view's own paint over cells nvim already owns, so it
    // takes no channel: the row is here because the surface exists, and a
    // second claim on `laststatus` would leave the option nobody's to hold
    SurfaceChannels {
        surface: Surface::Frame,
        channels: &[],
    },
    SurfaceChannels {
        surface: Surface::Grid,
        channels: &[],
    },
];

/// A channel view leaves to the engine, and why.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Yielded {
    /// nvim's own name for it.
    pub channel: &'static str,
    /// What the user gets instead, in a sentence a notice could print.
    pub why: &'static str,
}

/// Every channel view deliberately leaves alone.
///
/// The other half of the enumeration [`CHANNELS`] opens: a channel on
/// neither list is one nobody has decided about, which is what
/// `view-harness`'s completeness walk fails on.
pub const NOT_CHROME: &[Yielded] = &[
    Yielded {
        channel: "statuscolumn",
        why: "the gutter belongs to the buffer window, and view paints it as the engine sends it",
    },
    Yielded {
        channel: "signcolumn",
        why: "signs are buffer decoration, painted as the engine sends them",
    },
    Yielded {
        channel: "foldcolumn",
        why: "fold marks are buffer decoration, painted as the engine sends them",
    },
    Yielded {
        channel: "number",
        why: "line numbers belong to the buffer window",
    },
    Yielded {
        channel: "relativenumber",
        why: "line numbers belong to the buffer window",
    },
    Yielded {
        channel: "colorcolumn",
        why: "a column rule belongs to the buffer window",
    },
    Yielded {
        channel: "title",
        why: "the terminal's own title bar is not a row of the grid",
    },
    Yielded {
        channel: "titlestring",
        why: "the terminal's own title bar is not a row of the grid",
    },
    Yielded {
        channel: "ext_linegrid",
        why: "the grid protocol itself, which externalizes no surface",
    },
    Yielded {
        channel: "ext_multigrid",
        why: "window addressing, which externalizes no surface",
    },
    Yielded {
        channel: "ext_hlstate",
        why: "highlight vocabulary, which externalizes no surface",
    },
    Yielded {
        channel: "ext_termcolors",
        why: "terminal palette vocabulary, which externalizes no surface",
    },
    Yielded {
        channel: "rgb",
        why: "the colour depth the attaching UI declares, which draws nothing of its own",
    },
    Yielded {
        channel: "vim.ui.select",
        why: "a list prompt view draws no replacement for, so the engine's own is what a \
               config gets",
    },
    Yielded {
        channel: "vim.ui.input",
        why: "a text prompt view draws no replacement for, so the engine's own is what a \
               config gets",
    },
];

/// `surface`'s channels, or an empty slice for a surface the table has no
/// row for -- which `every_surface_has_channels` denies.
#[must_use]
pub fn channels(surface: Surface) -> &'static [Channel] {
    CHANNELS
        .iter()
        .find(|row| row.surface == surface)
        .map_or(&[], |row| row.channels)
}

/// Every surface whose channel list names `option`, in table order.
///
/// A hold is issued only where every one of them is view's, which is what
/// keeps `cmdheight` with nvim for a session that kept nvim's messages.
pub fn claimants_of(option: &str) -> impl Iterator<Item = Surface> + '_ {
    CHANNELS
        .iter()
        .filter(move |row| row.channels.iter().any(|channel| channel.name() == option))
        .map(|row| row.surface)
}

/// Whether some surface claims `channel`.
#[must_use]
pub fn is_claimed(channel: &str) -> bool {
    claimants_of(channel).next().is_some()
}

/// Whether more than one surface claims `channel`, which makes it the
/// session's to hold rather than any one feature's: the surfaces that
/// claim it can be handed back one at a time, and the channel is still
/// needed while either of them is view's.
///
/// One spelling for one question. The takeover derivation leaves such a
/// channel out of a feature's plan and the session holds it against its
/// whole attach set, so two readings of "shared" would either hold it
/// twice or not at all.
#[must_use]
pub fn shared_by_surfaces(channel: &str) -> bool {
    claimants_of(channel).count() > 1
}

/// Every channel more than one surface claims, in table order and with no
/// repeats.
///
/// The session holds these against its whole attach set rather than any
/// feature's switch, so the crate that performs them and the crate that
/// enumerates the guards they install both read this one answer.
#[must_use]
pub fn session_held() -> Vec<Channel> {
    CHANNELS
        .iter()
        .flat_map(|entry| entry.channels.iter().copied())
        .filter(|channel| shared_by_surfaces(channel.name()))
        .fold(Vec::new(), |mut out, channel| {
            if !out.contains(&channel) {
                out.push(channel);
            }
            out
        })
}

/// Every channel of every surface `option` belongs to that nvim evaluates
/// only while something else leaves it a row to draw on, in table order and
/// with no repeats.
///
/// The hold of `option` is what reads them. A covered channel is never
/// held -- holding its coverer is what stops nvim drawing it -- but the
/// value a config left in it names whoever was drawing that surface, and
/// an option at nvim's own default names nobody: a status line plugin
/// writes `statusline` and leaves `laststatus` exactly where nvim put it,
/// so the hold alone has nothing to tell the user about.
#[must_use]
pub fn covered_beside(option: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for surface in claimants_of(option) {
        for channel in channels(surface) {
            if let Channel::Covered {
                option: covered, ..
            } = channel
            {
                if !out.contains(covered) {
                    out.push(covered);
                }
            }
        }
    }
    out
}

/// Whether view leaves `channel` to the engine on purpose.
#[must_use]
pub fn is_yielded(channel: &str) -> bool {
    yielded_reason(channel).is_some()
}

/// What a channel view leaves alone gives the user instead, or `None` for
/// a channel [`NOT_CHROME`] does not name.
///
/// The sentence the completeness walk prints beside every decision already
/// made, so a reader told that some new channel is undecided can see what
/// deciding one looks like.
#[must_use]
pub fn yielded_reason(channel: &str) -> Option<&'static str> {
    NOT_CHROME
        .iter()
        .find(|row| row.channel == channel)
        .map(|row| row.why)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn every_surface_has_channels() {
        for surface in [
            Surface::Cmdline,
            Surface::Popupmenu,
            Surface::Messages,
            Surface::Tabline,
            Surface::Statusline,
            Surface::Frame,
            Surface::Grid,
        ] {
            let rows = CHANNELS.iter().filter(|r| r.surface == surface).count();
            assert_eq!(rows, 1, "{surface:?} needs exactly one row of the table");
        }
        assert_eq!(
            CHANNELS.len(),
            7,
            "a surface added to the enum needs a row here, with the channels that draw it"
        );
    }

    /// A covered channel names one of its own surface's holds. A `by` that
    /// resolves to nothing is a channel nobody is holding, reported as
    /// accounted for.
    #[test]
    fn every_covered_channel_names_a_channel_of_its_own_surface() {
        for row in CHANNELS {
            for channel in row.channels {
                let Channel::Covered { option, by } = channel else {
                    continue;
                };
                assert!(
                    row.channels.iter().any(|other| match other {
                        Channel::Hold { option: held, .. } => held == by,
                        Channel::Attach(ext) => &ext.as_str() == by,
                        _ => false,
                    }),
                    "{option} is covered by {by}, which {:?} does not hold",
                    row.surface
                );
            }
        }
    }

    /// The covered channels a hold carries are its own surfaces' and
    /// nobody else's: a hold that carried another surface's would report a
    /// holder under a remedy that gives back a surface the user never
    /// asked about.
    #[test]
    fn a_holds_covered_channels_are_the_ones_its_own_surfaces_declare() {
        assert_eq!(covered_beside("laststatus"), vec!["statusline"]);
        assert_eq!(
            covered_beside("cmdheight"),
            vec!["showmode", "showcmd", "ruler", "rulerformat"]
        );
        assert_eq!(covered_beside("winbar"), vec!["tabline", "showtabline"]);
        assert!(covered_beside("vim.notify").is_empty());
        assert!(covered_beside("nothing-claims-this").is_empty());
    }

    #[test]
    fn no_channel_is_both_claimed_and_yielded() {
        for row in NOT_CHROME {
            assert!(
                !is_claimed(row.channel),
                "{} is both claimed by a surface and left to the engine",
                row.channel
            );
        }
    }

    /// The two surfaces of the last grid row. A hold issued for one of them
    /// alone takes the row a handed-back surface still draws on.
    #[test]
    fn the_command_line_row_is_claimed_by_both_surfaces_that_draw_on_it() {
        let claimants: Vec<Surface> = claimants_of("cmdheight").collect();
        assert_eq!(claimants, vec![Surface::Cmdline, Surface::Messages]);
    }

    #[test]
    fn the_window_local_row_a_capability_cannot_reach_is_held_per_window() {
        let winbar = channels(Surface::Tabline)
            .iter()
            .find(|channel| channel.name() == "winbar")
            .copied()
            .expect("the tab line claims winbar");
        assert_eq!(
            winbar,
            Channel::Hold {
                option: "winbar",
                scope: Scope::Window,
                value: ChannelValue::Str(""),
            }
        );
    }

    #[test]
    fn every_value_survives_the_table_to_wire_conversion() {
        let look = Look::default();
        assert_eq!(ChannelValue::Int(0).wire(look), OptionValue::Int(0));
        assert_eq!(
            ChannelValue::Bool(false).wire(look),
            OptionValue::Bool(false)
        );
        assert_eq!(
            ChannelValue::Str("").wire(look),
            OptionValue::Str(String::new())
        );
        let by_look = ChannelValue::ByLook {
            nvim: &ChannelValue::Int(0),
            tiles: &ChannelValue::Int(2),
        };
        assert_eq!(
            by_look.wire(Look::new(Panes::Nvim, true)),
            OptionValue::Int(0)
        );
        assert_eq!(
            by_look.wire(Look::new(Panes::Tiles, true)),
            OptionValue::Int(2)
        );
    }

    /// `laststatus` is the one option whose held value moves with the
    /// look, and it stays a single surface's claim whichever leg answers.
    ///
    /// A `Surface::Frame` row carrying a `Hold` of its own would make it
    /// shared, which skips it out of every feature's plan and hands it to
    /// `session_held()` -- so `[native] statusline = false` would stop
    /// handing the row back at all.
    #[test]
    fn laststatus_is_claimed_by_one_surface_whatever_the_look() {
        assert!(
            !shared_by_surfaces("laststatus"),
            "laststatus must stay one surface's to hold"
        );
        assert_eq!(claimants_of("laststatus").count(), 1);
        assert!(
            !session_held().iter().any(|c| c.name() == "laststatus"),
            "a session-held laststatus is one no feature's off switch reverses"
        );
    }

    /// Only a look-keyed value under tiles is held past its feature's
    /// switch.
    #[test]
    fn the_tiles_leg_of_a_look_keyed_hold_is_the_looks_own() {
        let by_look = ChannelValue::ByLook {
            nvim: &ChannelValue::Int(0),
            tiles: &ChannelValue::Int(2),
        };
        for gaps in [true, false] {
            assert!(by_look.held_by_look(Look::new(Panes::Tiles, gaps)));
            assert!(!by_look.held_by_look(Look::new(Panes::Nvim, gaps)));
            assert!(!ChannelValue::Int(2).held_by_look(Look::new(Panes::Tiles, gaps)));
        }
    }

    /// `held_by_look` applies its rule to any `ByLook` row under tiles, on
    /// grounds that hold for `laststatus` alone: the tiles geometry is
    /// built on it, and the tiles painter covers the row it draws in. A
    /// second look-keyed hold added to [`CHANNELS`] would inherit the same
    /// past-the-switch behavior with no decision behind it, so this walks
    /// the whole table and pins today's answer to the one member the
    /// grounds were written for.
    #[test]
    fn held_by_look_under_tiles_covers_exactly_laststatus() {
        let tiles = Look::new(Panes::Tiles, true);
        let mut held: Vec<&'static str> = CHANNELS
            .iter()
            .flat_map(|entry| entry.channels)
            .filter_map(|channel| match channel {
                Channel::Hold { option, value, .. } if value.held_by_look(tiles) => Some(*option),
                _ => None,
            })
            .collect();
        held.sort_unstable();
        held.dedup();
        assert_eq!(
            held,
            vec!["laststatus"],
            "a new look-keyed hold needs the same geometry grounds as laststatus \
             before it joins this list"
        );
    }

    /// A look flip away from tiles hands a look-held option back with a
    /// global release, which puts back one session-wide value. A
    /// window-local option keyed by look would need a per-window release.
    #[test]
    fn every_look_keyed_hold_is_global() {
        for entry in CHANNELS {
            for channel in entry.channels {
                if let Channel::Hold {
                    option,
                    scope,
                    value: ChannelValue::ByLook { .. },
                } = channel
                {
                    assert_eq!(
                        *scope,
                        Scope::Global,
                        "{option} is keyed by look at a scope the release cannot put back"
                    );
                }
            }
        }
    }
}
