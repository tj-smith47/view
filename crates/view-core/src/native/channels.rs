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
}

impl ChannelValue {
    /// This value as the wire carries it.
    ///
    /// Total over both closed enums, so a fourth option type added to
    /// either is a compile error rather than a hold that sets nothing.
    #[must_use]
    pub fn wire(self) -> OptionValue {
        match self {
            Self::Int(n) => OptionValue::Int(n),
            Self::Bool(b) => OptionValue::Bool(b),
            Self::Str(s) => OptionValue::Str(s.to_string()),
        }
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
            Channel::Hold {
                option: "laststatus",
                scope: Scope::Global,
                value: ChannelValue::Int(0),
            },
            Channel::Covered {
                option: "statusline",
                by: "laststatus",
            },
        ],
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

/// Whether view leaves `channel` to the engine on purpose.
#[must_use]
pub fn is_yielded(channel: &str) -> bool {
    NOT_CHROME.iter().any(|row| row.channel == channel)
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
            Surface::Grid,
        ] {
            let rows = CHANNELS.iter().filter(|r| r.surface == surface).count();
            assert_eq!(rows, 1, "{surface:?} needs exactly one row of the table");
        }
        assert_eq!(
            CHANNELS.len(),
            6,
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
        assert_eq!(ChannelValue::Int(0).wire(), OptionValue::Int(0));
        assert_eq!(ChannelValue::Bool(false).wire(), OptionValue::Bool(false));
        assert_eq!(
            ChannelValue::Str("").wire(),
            OptionValue::Str(String::new())
        );
    }
}
