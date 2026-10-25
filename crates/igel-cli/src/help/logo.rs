//! A terminal-sized rendering sampled from assets/logo.png.
//! Two pixels share each cell; spaces keep the terminal's own background.

use colored::{Color, Colorize};

pub(super) const WIDTH: usize = 28;
const PIXELS: [&str; 24] = [
    "             oooo           ",
    "        ooo   qqqoo         ",
    "      ooqqqooooqqqoo        ",
    "       oqqqqqqoqqqqoo       ",
    "       ooqqqqqqqqqqqo o     ",
    "      ooooqqqqqqqqqqqooo    ",
    "    oqqqqqqqqqqqqqqqqoqqo   ",
    "  ooqqqqqqqqqqqqqqqqqqqqo   ",
    "  oooqqqqqqqqqqqqqqqqqqqo   ",
    "    ooqqqqqqqqqooqqooqqqo   ",
    "   ooqqqqqqqqqqqoffffoqqo   ",
    "  oqqqqqqqqqqqqqoqffffoqo   ",
    " oqqqqqqqqqqqqqqoofffffoo   ",
    "oqqqqqqqqqqqqqqoqqfffqffo   ",
    "oooooqqqqqqqqqqqqfffqoqfqo  ",
    "   oqqqqqqqqqqqoqffffqfffqoo",
    "  oqqqqqqqqqqooqffffffffffoo",
    "  oqqqqqqqooqffffffffffffqo ",
    " oqqoqqqoqfffffffffffffqoo  ",
    " ooooqqoqfffffffffffqooo    ",
    "    oqqqffffffffffffo       ",
    "     ooffffqqqqfffffo       ",
    "      offfo    offffq       ",
    "      ooqoo     oqqqo       ",
];

fn color(pixel: u8) -> Color {
    match pixel {
        b'f' => Color::TrueColor {
            r: 255,
            g: 239,
            b: 213,
        },
        b'q' => Color::TrueColor {
            r: 215,
            g: 154,
            b: 98,
        },
        _ => Color::TrueColor {
            r: 49,
            g: 32,
            b: 28,
        },
    }
}

pub(super) fn lines() -> Vec<String> {
    PIXELS
        .chunks_exact(2)
        .map(|pair| {
            pair[0]
                .bytes()
                .zip(pair[1].bytes())
                .map(|(top, bottom)| {
                    if !crate::ui::colors() {
                        // Keep the face and eye distinct when colour is disabled.
                        let ink = |p| p != b' ' && p != b'f';
                        return match (ink(top), ink(bottom)) {
                            (false, false) => " ",
                            (true, false) => "▀",
                            (false, true) => "▄",
                            (true, true) => "█",
                        }
                        .to_owned();
                    }
                    match (top, bottom) {
                        (b' ', b' ') => " ".to_owned(),
                        (b' ', bottom) => "▄".color(color(bottom)).to_string(),
                        (top, b' ') => "▀".color(color(top)).to_string(),
                        (top, bottom) => "▀".color(color(top)).on_color(color(bottom)).to_string(),
                    }
                })
                .collect()
        })
        .collect()
}
