//! mmxrs patch (whole file): display-only widescreen margins.
//!
//! With `margin > 0` every rendered line also gets `margin` pixels left of
//! x 0 and right of x 255, drawn from the same registers (scroll, mode,
//! layer enables, colour math, brightness) the line itself used, so HDMA
//! effects carry over line by line. BG tiles come from the host's margin
//! map when it has an entry for the cell (`tiles[bg]`, keyed by the BG's
//! scroll-space position mod 1024), from VRAM otherwise. Sprites anywhere in
//! OAM whose pixels land in the margins are drawn there (no per-line limits).
//! Windows and the colour window use the nearest edge pixel's (x 0 or 255).
//! The host can add sprites that exist only here (`set_margin_sprites`:
//! objects the game culled from OAM), drawn behind every OAM sprite.
//! HUD slots (`set_hud_shift`) render `margin` px further left / right, so
//! the game's HUD sits at the wide picture's edges (both paths, clipped at
//! the wide frame's edges, never at their OAM position).
//! Not drawn in the margins: mode 7, mosaic, offset-per-tile (plain scroll),
//! hi-res and interlaced frames (those frames are output unwidened).
//!
//! Nothing here is in a save state or writes VRAM/OAM/CGRAM: `Wide` encodes
//! as nothing, clones as empty and survives `load_state` (api.rs).

use super::*;

/// Override map size in cells per side (1024 px of scroll space / 8).
pub const MARGIN_MAP_CELLS: usize = 128;
/// Largest margin per side (21:9 needs 136).
pub const MAX_WIDE_MARGIN: u16 = 192;

/// One rendered line's BG registers (research / the host's margin provider).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LineRegs {
    pub mode: u8,
    /// TM / TS layer bits (bit 4 = OBJ).
    pub main: u8,
    pub sub: u8,
    pub h_scroll: [u16; 4],
    pub v_scroll: [u16; 4],
    pub forced_blank: bool,
    pub brightness: u8,
    /// CGADSUB layer bits (bit 4 OBJ, bit 5 backdrop), 0 when math is never on.
    pub math: u8,
    pub fixed_color: u16,
    pub backdrop: u16,
}

/// A sprite drawn in the margins only, at a signed X (no 9-bit wrap). OAM
/// attribute byte as is (vhoopppN); `page` = a P2 shadow page instead of VRAM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MarginSprite {
    pub x: i16,
    pub y: u8,
    pub tile: u8,
    pub attr: u8,
    pub large: bool,
    pub page: Option<u8>,
    /// Coloured from `P2Tiles::palette` (as a slot in its `pal` mask).
    pub extra_palette: bool,
}

#[derive(Debug, Default)]
pub struct Wide {
    pub margin: u16,
    /// `(256 + 2 * margin) * 239` colours; the centre is copied from the
    /// normal frame buffer at frame end.
    pub buf: Vec<Color>,
    /// Per BG: `MARGIN_MAP_CELLS^2` cells, `0` = none (VRAM), else
    /// `0x1_0000 | tilemap entry`. Cell = `(y & 1023) / tile_h * 128 + (x & 1023) / tile_w`.
    pub tiles: [Option<Box<[u32]>>; 4],
    /// Record `LineRegs` per line (index `scanline - 1`).
    pub record: bool,
    pub lines: Vec<LineRegs>,
    /// Margin sprites (live) and the host's next set, which goes live with
    /// the game's next OAM upload like the P2 pages.
    pub sprites: Vec<MarginSprite>,
    pub next_sprites: Option<Vec<MarginSprite>>,
    /// OAM slots drawn `margin` px left / right (the HUD), live and next
    /// (goes live with the next OAM upload, like the margin sprites).
    pub hud: (u128, u128),
    pub next_hud: Option<(u128, u128)>,
    /// Scratch: the current line's margin BG pixels, `4 x 2 * margin`.
    bg_line: Vec<Pixel>,
}

/// Window results at a margin's edge pixel (x 0 or 255), which every pixel
/// of that margin uses: computed once per line and side.
#[derive(Clone, Copy)]
struct EdgeWindows {
    math: bool,
    obj: bool,
    bg: [bool; 4],
}

impl Clone for Wide {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl Encode for Wide {
    fn encode<E: bincode::enc::Encoder>(
        &self,
        _: &mut E,
    ) -> Result<(), bincode::error::EncodeError> {
        Ok(())
    }
}

impl<C> Decode<C> for Wide {
    fn decode<D: bincode::de::Decoder<Context = C>>(
        _: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        Ok(Self::default())
    }
}
bincode::impl_borrow_decode!(Wide);

impl Ppu {
    pub fn set_wide_margin(&mut self, margin: u16) {
        self.wide.margin = margin.min(MAX_WIDE_MARGIN);
    }

    pub fn wide_margin(&self) -> u16 {
        self.wide.margin
    }

    /// The host's margin map for `bg` (`None` clears it: VRAM everywhere).
    pub fn set_margin_tiles(&mut self, bg: usize, cells: Option<&[u32]>) {
        match cells {
            None => self.wide.tiles[bg] = None,
            Some(c) => {
                let t = self.wide.tiles[bg].get_or_insert_with(|| {
                    vec![0; MARGIN_MAP_CELLS * MARGIN_MAP_CELLS].into_boxed_slice()
                });
                t.copy_from_slice(&c[..MARGIN_MAP_CELLS * MARGIN_MAP_CELLS]);
            }
        }
    }

    pub fn set_margin_sprites(&mut self, sprites: Vec<MarginSprite>) {
        self.wide.next_sprites = Some(sprites);
    }

    /// OAM slots shown `margin` px left (`left`) / right (`right`) of their
    /// position, from the next OAM upload on. No effect without a margin.
    pub fn set_hud_shift(&mut self, left: u128, right: u128) {
        self.wide.next_hud = Some((left, right));
    }

    /// The live HUD shift masks.
    pub fn hud_shift(&self) -> (u128, u128) {
        self.wide.hud
    }

    /// X shift of OAM slot `idx`: -margin / +margin for HUD slots while the
    /// frame is widened, else 0.
    pub(super) fn hud_dx(&self, idx: u8) -> i16 {
        let m = self.wide.margin as i16;
        if m == 0 || self.state.h_hi_res_frame || self.state.v_hi_res_frame {
            return 0;
        }
        let (l, r) = self.wide.hud;
        if (l >> idx) & 1 != 0 {
            -m
        } else if (r >> idx) & 1 != 0 {
            m
        } else {
            0
        }
    }

    pub fn set_line_record(&mut self, on: bool) {
        self.wide.record = on;
    }

    /// The VRAM tilemap entry BG `bg` shows at BG-space `(x, y)` (current registers).
    pub fn bg_map_entry(&self, bg: usize, x: u16, y: u16) -> u16 {
        get_bg_map_entry(&self.vram, &self.registers, bg, x, y)
    }

    pub fn registers_debug(&self) -> String {
        format!("{:#?}", self.registers)
    }

    pub fn line_regs(&self) -> &[LineRegs] {
        &self.wide.lines
    }

    pub(crate) fn take_wide(&mut self) -> Wide {
        std::mem::take(&mut self.wide)
    }

    pub(crate) fn put_wide(&mut self, w: Wide) {
        self.wide = w;
    }

    /// The widened frame (centre = the normal frame) when the margin is on
    /// and the frame was plain 256-wide, non-interlaced.
    pub fn compose_wide(&mut self) -> Option<(&[Color], FrameSize)> {
        let m = usize::from(self.wide.margin);
        if m == 0 || self.state.h_hi_res_frame || self.state.v_hi_res_frame {
            return None;
        }
        let h = usize::from(self.registers.v_display_size.to_lines());
        let w = NORMAL_SCREEN_WIDTH + 2 * m;
        if self.wide.buf.len() != w * 239 {
            self.wide.buf = vec![Color::BLACK; w * 239];
        }
        for y in 0..h {
            let src = &self.frame_buffer[y * NORMAL_SCREEN_WIDTH..(y + 1) * NORMAL_SCREEN_WIDTH];
            self.wide.buf[y * w + m..y * w + m + NORMAL_SCREEN_WIDTH].copy_from_slice(src);
        }
        Some((
            &self.wide.buf[..w * h],
            FrameSize {
                width: w as u32,
                height: h as u32,
            },
        ))
    }

    fn record_line(&mut self, scanline: u16) {
        let r = &self.registers;
        let bits = |b: [bool; 4], o: bool| {
            b.iter()
                .enumerate()
                .fold(u8::from(o) << 4, |a, (i, &on)| a | u8::from(on) << i)
        };
        let math = if r.color_math_enabled_for_any_layer() {
            bits(r.bg_color_math_enabled, r.obj_color_math_enabled)
                | u8::from(r.backdrop_color_math_enabled) << 5
        } else {
            0
        };
        let l = LineRegs {
            mode: r.bg_mode as u8,
            main: bits(r.main_bg_enabled, r.main_obj_enabled),
            sub: bits(r.sub_bg_enabled, r.sub_obj_enabled),
            h_scroll: r.bg_h_scroll,
            v_scroll: r.bg_v_scroll,
            forced_blank: r.forced_blanking,
            brightness: r.brightness,
            math,
            fixed_color: r.sub_backdrop_color,
            backdrop: self.cgram[0],
        };
        let i = usize::from(scanline - 1);
        if self.wide.lines.len() <= i {
            self.wide.lines.resize(i + 1, LineRegs::default());
        }
        self.wide.lines[i] = l;
    }

    /// Called at the end of `render_current_line`: the margins of `scanline`
    /// (the left one only on the full render, `from_pixel == 0`).
    pub(super) fn render_wide_margins(&mut self, scanline: u16, from_pixel: u16) {
        if self.wide.record && from_pixel == 0 {
            self.record_line(scanline);
        }
        let m = i32::from(self.wide.margin);
        if m == 0
            || self.state.h_hi_res_frame
            || self.state.v_hi_res_frame
            || !(1..=239).contains(&scanline)
        {
            return;
        }
        let w = 256 + 2 * m as usize;
        if self.wide.buf.len() != w * 239 {
            self.wide.buf = vec![Color::BLACK; w * 239];
        }
        let row = usize::from(scanline - 1) * w;
        let xs = (if from_pixel == 0 { -m } else { 256 })..256 + m;
        let xs = xs.filter(|x| !(0..256).contains(x));
        if self.registers.forced_blanking {
            for sx in xs {
                self.wide.buf[row + (sx + m) as usize] = Color::BLACK;
            }
            return;
        }
        let objs = self.wide_obj_line(scanline, m);
        // BG pixels of the line, fetched once per 8-px tile.
        let n = 2 * m as usize;
        let mut bgs = std::mem::take(&mut self.wide.bg_line);
        bgs.clear();
        bgs.resize(4 * n, Pixel::TRANSPARENT);
        for (bg, line) in bgs.chunks_exact_mut(n).enumerate() {
            if self.wide_bg_used(bg) {
                let (left, right) = line.split_at_mut(m as usize);
                if from_pixel == 0 {
                    self.wide_bg_line(bg, scanline, -m, left);
                }
                self.wide_bg_line(bg, scanline, 256, right);
            }
        }
        let r = &self.registers;
        let win = |edge: u16| EdgeWindows {
            math: r.in_math_window(edge),
            obj: r.obj_in_window(edge),
            bg: std::array::from_fn(|bg| r.bg_in_window(bg, edge)),
        };
        let wins = [win(0), win(255)];
        for sx in xs {
            let i = (if sx < 0 { sx + m } else { sx - 256 + m }) as usize;
            let bg = std::array::from_fn(|b| bgs[b * n + i]);
            let c = self.wide_pixel(objs[i], bg, &wins[usize::from(sx >= 0)]);
            self.wide.buf[row + (sx + m) as usize] = c;
        }
        self.wide.bg_line = bgs;
    }

    /// Whether `wide_screen_pixel` can want BG `bg`'s pixels on this line.
    fn wide_bg_used(&self, bg: usize) -> bool {
        let r = &self.registers;
        let mode = r.bg_mode;
        let in_mode = match bg {
            0 => true,
            1 => mode.bg2_enabled(r.extbg_enabled),
            2 => mode.bg3_enabled(),
            _ => mode.bg4_enabled(),
        };
        mode != BgMode::Seven && in_mode && (r.main_bg_enabled[bg] || r.sub_bg_enabled[bg])
    }

    /// `obj` / `bg`: this pixel's OBJ and BG pixels; `win`: its margin's windows.
    fn wide_pixel(&self, obj: Pixel, bg: [Pixel; 4], win: &EdgeWindows) -> Color {
        let r = &self.registers;
        let main = self.wide_screen_pixel(Screen::Main, obj, bg, win);
        let sub_needed = r.sub_bg_obj_enabled && r.color_math_enabled_for_any_layer();
        let in_color_window = win.math;
        let mut main_px = main;
        let force_black = r.force_main_screen_black.enabled(in_color_window);
        if force_black {
            main_px.color = 0;
        }
        let math_layer = match main_px.layer {
            Layer::Bg1 => r.bg_color_math_enabled[0],
            Layer::Bg2 => r.bg_color_math_enabled[1],
            Layer::Bg3 => r.bg_color_math_enabled[2],
            Layer::Bg4 => r.bg_color_math_enabled[3],
            Layer::Obj => r.obj_color_math_enabled && main_px.palette & 7 >= 4,
            Layer::Backdrop => r.backdrop_color_math_enabled,
        };
        let color = if r.color_math_enabled.enabled(in_color_window) && math_layer {
            let (sub_color, sub_transparent) = if sub_needed {
                let p = self.wide_screen_pixel(Screen::Sub, obj, bg, win);
                (p.color, p.layer == Layer::Backdrop)
            } else {
                (r.sub_backdrop_color, false)
            };
            let divide = r.color_math_divide_enabled && !force_black && !sub_transparent;
            r.color_math_operation
                .apply(main_px.color, sub_color, divide)
        } else {
            main_px.color
        };
        convert_snes_color(color, r.brightness)
    }

    /// `bg`: this pixel's BG pixels (both screens use them).
    fn wide_screen_pixel(
        &self,
        screen: Screen,
        obj: Pixel,
        bg: [Pixel; 4],
        win: &EdgeWindows,
    ) -> RenderedPixel {
        let r = &self.registers;
        let (bg_enabled, bg_off_in_window, obj_enabled, obj_off_in_window, backdrop) = match screen
        {
            Screen::Main => (
                r.main_bg_enabled,
                r.main_bg_disabled_in_window,
                r.main_obj_enabled,
                r.main_obj_disabled_in_window,
                self.cgram[0],
            ),
            Screen::Sub => (
                r.sub_bg_enabled,
                r.sub_bg_disabled_in_window,
                r.sub_obj_enabled,
                r.sub_obj_disabled_in_window,
                r.sub_backdrop_color,
            ),
        };
        let mode = r.bg_mode;
        let m01 = matches!(mode, BgMode::Zero | BgMode::One);
        let mut pr = PriorityResolver::new();
        if obj_enabled && !(obj_off_in_window && win.obj) && !obj.is_transparent() {
            pr.add_obj(obj, m01);
        }
        if mode != BgMode::Seven {
            let on = |b: usize| bg_enabled[b] && !(bg_off_in_window[b] && win.bg[b]);
            if on(0) {
                let p = bg[0];
                if !p.is_transparent() {
                    pr.add_bg1(p, m01);
                }
            }
            if mode.bg2_enabled(r.extbg_enabled) && on(1) {
                let p = bg[1];
                if !p.is_transparent() {
                    pr.add_bg2(p, m01);
                }
            }
            if mode.bg3_enabled() && on(2) {
                let p = bg[2];
                if !p.is_transparent() {
                    pr.add_bg3(p, mode == BgMode::One && r.mode_1_bg3_priority);
                }
            }
            if mode.bg4_enabled() && on(3) {
                let p = bg[3];
                if !p.is_transparent() {
                    pr.add_bg4(p);
                }
            }
        }
        pr.get().map_or(
            RenderedPixel {
                color: backdrop,
                palette: 0,
                layer: Layer::Backdrop,
            },
            |(p, layer)| RenderedPixel {
                color: pixel_color(
                    &self.cgram,
                    &self.p2_tiles.0.palette,
                    layer,
                    mode,
                    r.direct_color_mode_enabled,
                    p,
                ),
                palette: p.palette,
                layer,
            },
        )
    }

    /// BG `bg`'s pixels at screen X `sx0..sx0 + out.len()` of `scanline`.
    /// The map entry and tile depend on `x >> 3` only (8- and 16-px tiles,
    /// fixed y), so they are fetched once per 8 px.
    fn wide_bg_line(&self, bg: usize, scanline: u16, sx0: i32, out: &mut [Pixel]) {
        let r = &self.registers;
        let mode = r.bg_mode;
        let bpp = match bg {
            0 => mode.bg1_bpp(),
            1 => mode.bg2_bpp(),
            2 => BitsPerPixel::BG3,
            _ => BitsPerPixel::BG4,
        };
        let y = scanline.wrapping_add(r.bg_v_scroll[bg]);
        let (tw, th) = get_bg_tile_size(mode, r.bg_tile_size[bg]);
        let map_row = usize::from((y & 1023) / th) * MARGIN_MAP_CELLS;
        let map = self.wide.tiles[bg].as_deref();
        let mut memo: Option<(u16, u16, &[u16])> = None;
        for (k, o) in out.iter_mut().enumerate() {
            let x = ((sx0 + k as i32) as u16).wrapping_add(r.bg_h_scroll[bg]);
            let (raw, tile) = match memo {
                Some((key, raw, tile)) if key == x >> 3 => (raw, tile),
                _ => {
                    let cell = map_row + usize::from((x & 1023) / tw);
                    let raw = match map.map(|t| t[cell]) {
                        Some(e) if e != 0 => e as u16,
                        _ => get_bg_map_entry(&self.vram, r, bg, x, y),
                    };
                    let tile = get_bg_tile(
                        &self.vram,
                        r,
                        bg,
                        x,
                        y,
                        bpp,
                        raw & 0x3FF,
                        raw.bit(14),
                        raw.bit(15),
                    );
                    memo = Some((x >> 3, raw, tile));
                    (raw, tile)
                }
            };
            let (x_flip, y_flip) = (raw.bit(14), raw.bit(15));
            let row = if y_flip { 7 - (y % 8) } else { y % 8 };
            let col = if x_flip { 7 - (x % 8) } else { x % 8 };
            let bit = (7 - col) as u8;
            let mut color = 0_u8;
            for plane in (0..bpp.bitplanes()).step_by(2) {
                let word = tile[row as usize + 4 * plane];
                color |= u8::from(word.bit(bit)) << plane;
                color |= u8::from(word.bit(bit + 8)) << (plane + 1);
            }
            *o = Pixel {
                palette: ((raw >> 10) & 7) as u8,
                color,
                priority: raw.bit(13).into(),
            };
        }
    }

    /// OBJ pixels of `scanline` in the margins: `[left m.., right m..]`.
    /// Sprites are evaluated on the previous line, as the main path does.
    /// Position: OAM X as the hardware reads it (9-bit, >= 256 is negative),
    /// and an X in `256..256 + m` also counts as itself (the right margin).
    /// Lower OAM index wins; no 32-sprite / 34-tile limits. The host's margin
    /// sprites go under all of OAM (earlier ones win among them). A HUD slot
    /// is at its signed X + its shift only.
    fn wide_obj_line(&self, scanline: u16, m: i32) -> Vec<Pixel> {
        let mut out = vec![Pixel::TRANSPARENT; 2 * m as usize];
        let r = &self.registers;
        if !(r.main_obj_enabled || r.sub_obj_enabled) {
            return out;
        }
        let ev = scanline - 1;
        let (sw, sh) = r.obj_tile_size.small_size();
        let (lw, lh) = r.obj_tile_size.large_size();
        for s in self.wide.sprites.iter().rev() {
            let (w, h) = if s.large { (lw, lh) } else { (sw, sh) };
            let page = s.page.map(|p| usize::from(p & 3) * P2_PAGE_WORDS);
            self.wide_obj(
                &mut out,
                m,
                ev,
                &[i32::from(s.x)],
                s.y,
                (w, h),
                s.tile,
                s.attr,
                page,
                s.extra_palette,
            );
        }
        for idx in (0..128u8).rev() {
            let [x_lsb, y] = self.oam_low[usize::from(idx) << 1].to_le_bytes();
            let [tile_lsb, attr] = self.oam_low[(usize::from(idx) << 1) + 1].to_le_bytes();
            let hi = self.oam_high[usize::from(idx >> 2)] >> (2 * (idx & 3));
            let size = if hi.bit(1) { (lw, lh) } else { (sw, sh) };
            let raw = u16::from_le_bytes([x_lsb, u8::from(hi.bit(0))]);
            let signed = |x: u16| {
                if x >= 256 {
                    i32::from(x) - 512
                } else {
                    i32::from(x)
                }
            };
            let hud = self.hud_dx(idx);
            let (xs, y): (&[i32], u8) = if hud != 0 {
                // HUD: at its own X + the shift only (unclipped: it is meant for the margins).
                (&[signed(raw) + i32::from(hud)], y)
            } else {
                let Some((x9, y)) = self.offset_sprite(idx, raw, y) else {
                    continue;
                };
                if x9 >= 256 && i32::from(x9) < 256 + m {
                    (&[signed(x9), i32::from(x9)], y)
                } else {
                    (&[signed(x9)], y)
                }
            };
            let p2 = (self.p2_tiles.0.slots >> idx) & 1 != 0;
            let page = p2.then(|| {
                (((self.p2_tiles.0.page1 >> idx) & 1) | ((self.p2_tiles.0.page2 >> idx) & 1) << 1)
                    as usize
                    * P2_PAGE_WORDS
            });
            let extra = (self.p2_tiles.0.pal >> idx) & 1 != 0;
            self.wide_obj(&mut out, m, ev, xs, y, size, tile_lsb, attr, page, extra);
        }
        out
    }

    /// One sprite's pixels on line `ev` at each X of `xs` into the margin
    /// line `out`; `page` = offset of its P2 shadow page; `extra`: coloured
    /// from the P2 palette.
    #[allow(clippy::too_many_arguments)]
    fn wide_obj(
        &self,
        out: &mut [Pixel],
        m: i32,
        ev: u16,
        xs: &[i32],
        y: u8,
        (w, h): (u16, u16),
        tile_lsb: u8,
        attr: u8,
        page: Option<usize>,
        extra: bool,
    ) {
        if !line_overlaps_sprite(y, h, ev) {
            return;
        }
        let r = &self.registers;
        let words = BitsPerPixel::OBJ.tile_size_words();
        let (x_flip, y_flip) = (attr.bit(6), attr.bit(7));
        let line = (ev as u8).wrapping_sub(y) & ((h - 1) as u8);
        let line = if y_flip { h as u8 - 1 - line } else { line };
        let base_tile = u16::from_le_bytes([tile_lsb, u8::from(attr.bit(0))]);
        for &base in xs {
            for col in 0..i32::from(w) {
                let px = base + col;
                let slot = if (-m..0).contains(&px) {
                    px + m
                } else if (256..256 + m).contains(&px) {
                    px - 256 + m
                } else {
                    continue;
                };
                let c = if x_flip { i32::from(w) - 1 - col } else { col } as u16;
                let mut t = base_tile;
                t = (t & !0xF) | (t.wrapping_add(c / 8) & 0xF);
                t = (t & !0xF0) | (t.wrapping_add(u16::from(line / 8) << 4) & 0xF0);
                let data = if let Some(p2_base) = page {
                    let o = p2_base + usize::from((t & 0x3F) % P2_PAGE_TILES) * words as usize;
                    &self.p2_tiles.0.page[o..o + words as usize]
                } else {
                    let base = r
                        .obj_tile_base_address
                        .wrapping_add(u16::from(t.bit(8)) * (256 * words + r.obj_tile_gap_size));
                    let a = (base.wrapping_add((t & 0xFF) * words) & VRAM_ADDRESS_MASK) as usize;
                    &self.vram[a..a + words as usize]
                };
                let row = u16::from(line % 8);
                let bit = 7 - (c % 8) as u8;
                let mut color = 0_u8;
                for i in 0..2 {
                    let word = data[(row + 8 * i) as usize];
                    color |= u8::from(word.bit(bit)) << (2 * i);
                    color |= u8::from(word.bit(bit + 8)) << (2 * i + 1);
                }
                if color != 0 {
                    let palette = (attr >> 1) & 7 | if extra { EXTRA_PALETTE } else { 0 };
                    out[slot as usize] = Pixel {
                        palette,
                        color,
                        priority: (attr >> 4) & 3,
                    };
                }
            }
        }
    }
}
