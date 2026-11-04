//! Canonical palette shared by the daemon's replies and the client's renderer.
use alacritty_terminal::term::cell::{Cell, Flags};
use vte::ansi::{Color, NamedColor, Rgb};

pub const COLOR_COUNT: usize = 269;

pub fn default_palette() -> Vec<u32> {
    let mut colors = vec![0; COLOR_COUNT];
    colors[..16].copy_from_slice(&[
        0x27262b, 0xe5534b, 0x5dbb63, 0xd9b35f, 0x3d8bfd, 0xb88be5, 0x59b8c7, 0xdfe0e5, 0x6c6d72,
        0xff7b72, 0x7dd98c, 0xf0cf85, 0x79b3ff, 0xd2afff, 0x82d5df, 0xffffff,
    ]);
    const CUBE: [u32; 6] = [0, 95, 135, 175, 215, 255];
    for (i, color) in colors.iter_mut().enumerate().take(232).skip(16) {
        let n = i - 16;
        *color = (CUBE[n / 36] << 16) | (CUBE[(n / 6) % 6] << 8) | CUBE[n % 6];
    }
    for (i, color) in colors.iter_mut().enumerate().take(256).skip(232) {
        let gray = 8 + 10 * (i as u32 - 232);
        *color = (gray << 16) | (gray << 8) | gray;
    }
    colors[NamedColor::Foreground as usize] = 0xdfe0e5;
    colors[NamedColor::Background as usize] = 0x1d1e21;
    colors[NamedColor::Cursor as usize] = 0xdfe0e5;
    for i in 0..8 {
        colors[NamedColor::DimBlack as usize + i] = dim(colors[i]);
    }
    colors[NamedColor::BrightForeground as usize] = 0xffffff;
    colors[NamedColor::DimForeground as usize] = dim(0xdfe0e5);
    colors
}

pub fn pack(rgb: Rgb) -> u32 {
    ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32
}

pub fn unpack(rgb: u32) -> Rgb {
    Rgb {
        r: (rgb >> 16) as u8,
        g: (rgb >> 8) as u8,
        b: rgb as u8,
    }
}

fn dim(rgb: u32) -> u32 {
    let r = ((rgb >> 16) & 255) * 2 / 3;
    let g = ((rgb >> 8) & 255) * 2 / 3;
    let b = (rgb & 255) * 2 / 3;
    (r << 16) | (g << 8) | b
}

pub fn resolve(color: Color, flags: Flags, colors: &[u32]) -> u32 {
    match color {
        Color::Spec(rgb) => {
            let rgb = pack(rgb);
            if flags.contains(Flags::DIM) {
                dim(rgb)
            } else {
                rgb
            }
        }
        Color::Named(mut name) => {
            if flags.contains(Flags::DIM) {
                name = name.to_dim();
            } else if flags.contains(Flags::BOLD) {
                name = name.to_bright();
            }
            colors[name as usize]
        }
        Color::Indexed(index) => {
            let index = if flags.contains(Flags::BOLD) && index < 8 && !flags.contains(Flags::DIM) {
                index as usize + 8
            } else {
                index as usize
            };
            let rgb = colors[index];
            if flags.contains(Flags::DIM) {
                dim(rgb)
            } else {
                rgb
            }
        }
    }
}

pub fn cell_colors(cell: &Cell, colors: &[u32]) -> (u32, u32) {
    let mut foreground = resolve(cell.fg, cell.flags, colors);
    let mut background = resolve(cell.bg, Flags::empty(), colors);
    if cell.flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut foreground, &mut background);
    }
    if cell.flags.contains(Flags::HIDDEN) {
        foreground = background;
    }
    (foreground, background)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conceal_and_reverse_work_with_truecolor_and_bold() {
        let colors = default_palette();
        let cell = Cell {
            c: 'x',
            fg: Color::Indexed(1),
            bg: Color::Spec(Rgb { r: 1, g: 2, b: 3 }),
            flags: Flags::BOLD | Flags::INVERSE,
            ..Cell::default()
        };
        assert_eq!(cell_colors(&cell, &colors), (0x010203, colors[9]));
        let hidden = Cell {
            flags: cell.flags | Flags::HIDDEN,
            ..cell
        };
        assert_eq!(cell_colors(&hidden, &colors), (colors[9], colors[9]));
    }
}
