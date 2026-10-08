//! lttprs: host hooks into OBJ rendering, used by the outfit compositor.
//!
//! - A hide mask over the 512 OBJ character numbers: an OAM entry whose own character number
//!   (its OAM name field, i.e. the top-left character of a large sprite) is masked is fetched
//!   as fully transparent. It still takes its sprite / tile slots on the line, so the 32-sprite
//!   and 34-tile limits behave exactly as without the mask.
//! - Per-pixel coverage of the last rendered frame (main screen): which OBJ priorities would win
//!   over the backgrounds at each pixel, and the OAM index of the visible OBJ pixel.
//! - INIDISP (brightness / forced blank) latched when the frame completes.
//!
//! All of this is host state: it is never serialized (save states encode nothing for it and
//! decode the defaults), and [`crate::api::SnesEmulator`] carries it across `load_state`.

use crate::ppu::registers::BgMode;
use crate::ppu::{HiResMode, Layer, Ppu, PriorityResolver};
use bincode::de::{BorrowDecoder, Decoder};
use bincode::enc::Encoder;
use bincode::error::{DecodeError, EncodeError};
use bincode::{BorrowDecode, Decode, Encode};

/// Width of the coverage buffers (one entry per main-screen x, also in hi-res modes).
pub const COVERAGE_WIDTH: usize = 256;
/// Height of the coverage buffers: one row per logical scanline 1..=239 (row = scanline - 1).
/// Interlaced frames record logical lines, i.e. what the even field / even output lines show.
pub const COVERAGE_HEIGHT: usize = 239;

/// "No OBJ pixel" in [`Coverage::front`].
pub const NO_OBJ: u8 = 0xFF;
/// Flag in [`Coverage::visible`] (besides the priority bits 0..=3): color math changes OBJ
/// pixels of palettes 4..=7 here (it is enabled and not a no-op add/subtract of 0), so their
/// final color is not the plain CGRAM color.
pub const OBJ_COLOR_MATH: u8 = 0x80;

#[derive(Debug, Clone)]
pub(super) struct Coverage {
    /// Bit `p` set: an OBJ pixel of priority `p` would be the visible main-screen pixel here.
    /// Plus [`OBJ_COLOR_MATH`].
    pub visible: Vec<u8>,
    /// OAM index of the visible (frontmost, non-hidden) OBJ pixel here, [`NO_OBJ`] if none.
    pub front: Vec<u8>,
    /// Background-only priority resolution of the line being rendered.
    pub bg_only: Vec<PriorityResolver>,
}

impl Coverage {
    fn new() -> Self {
        Self {
            visible: vec![0; COVERAGE_WIDTH * COVERAGE_HEIGHT],
            front: vec![NO_OBJ; COVERAGE_WIDTH * COVERAGE_HEIGHT],
            bg_only: vec![PriorityResolver::new(); COVERAGE_WIDTH],
        }
    }

    pub fn clear_row(&mut self, scanline: u16, from_x: usize) {
        let Some(row) = coverage_row(scanline) else {
            return;
        };
        let at = row * COVERAGE_WIDTH;
        self.visible[at + from_x..at + COVERAGE_WIDTH].fill(0);
        self.front[at + from_x..at + COVERAGE_WIDTH].fill(NO_OBJ);
    }
}

/// Row of a logical scanline (1-based) in the coverage buffers.
pub(super) fn coverage_row(scanline: u16) -> Option<usize> {
    let row = usize::from(scanline).checked_sub(1)?;
    (row < COVERAGE_HEIGHT).then_some(row)
}

#[derive(Debug, Clone)]
pub(super) struct ObjHooks {
    /// 512-bit mask over OBJ character numbers; `None` = nothing hidden.
    pub hide: Option<[u64; 8]>,
    pub coverage: Option<Box<Coverage>>,
    /// OAM index of each tile in `SpriteProcessor::fetched_tiles` (same order).
    pub fetched_oam: Vec<u8>,
    /// OAM index of each pixel in `Buffers::obj_pixels` ([`NO_OBJ`] = transparent).
    pub line_oam: [u8; COVERAGE_WIDTH],
    /// INIDISP master brightness (0..=15) when the last frame completed.
    pub frame_brightness: u8,
    /// INIDISP forced blank when the last frame completed.
    pub frame_forced_blank: bool,
}

impl Default for ObjHooks {
    fn default() -> Self {
        Self {
            hide: None,
            coverage: None,
            fetched_oam: Vec::with_capacity(super::MAX_SPRITE_TILES_PER_LINE),
            line_oam: [NO_OBJ; COVERAGE_WIDTH],
            frame_brightness: 0,
            frame_forced_blank: true,
        }
    }
}

impl ObjHooks {
    /// Whether an OAM entry with character number `chr` (9 bits) is hidden.
    #[inline(always)]
    pub fn is_hidden(&self, chr: u16) -> bool {
        self.hide.as_ref().is_some_and(|mask| {
            let chr = usize::from(chr & 0x1FF);
            (mask[chr >> 6] >> (chr & 63)) & 1 != 0
        })
    }

    pub fn set_coverage(&mut self, on: bool) {
        match (on, self.coverage.is_some()) {
            (true, false) => self.coverage = Some(Box::new(Coverage::new())),
            (false, true) => self.coverage = None,
            _ => {}
        }
    }
}

// Host state: save states carry nothing for it.
impl Encode for ObjHooks {
    fn encode<E: Encoder>(&self, _encoder: &mut E) -> Result<(), EncodeError> {
        Ok(())
    }
}

impl<Context> Decode<Context> for ObjHooks {
    fn decode<D: Decoder<Context = Context>>(_decoder: &mut D) -> Result<Self, DecodeError> {
        Ok(Self::default())
    }
}

impl<'de, Context> BorrowDecode<'de, Context> for ObjHooks {
    fn borrow_decode<D: BorrowDecoder<'de, Context = Context>>(
        _decoder: &mut D,
    ) -> Result<Self, DecodeError> {
        Ok(Self::default())
    }
}

/// Resolver priority of an OBJ pixel of priority `p` (see [`PriorityResolver::add_obj`]).
fn obj_layer_priority(p: u8, is_mode_0_or_1: bool) -> u8 {
    match (is_mode_0_or_1, p) {
        (_, 3) => PriorityResolver::OBJ3,
        (true, 2) => PriorityResolver::MODE_01_OBJ2,
        (true, 1) => PriorityResolver::MODE_01_OBJ1,
        (true, _) => PriorityResolver::MODE_01_OBJ0,
        (false, 2) => PriorityResolver::MODE_27_OBJ2,
        (false, 1) => PriorityResolver::MODE_27_OBJ1,
        (false, _) => PriorityResolver::MODE_27_OBJ0,
    }
}

impl Ppu {
    /// Records the main screen's OBJ coverage of the current line from `from_x` on. Call right
    /// after the main screen was resolved (`render_screen_pixels(Screen::Main, ..)`).
    pub(super) fn update_obj_coverage(&mut self, hi_res_mode: HiResMode, from_x: usize) {
        let scanline = self.state.scanline;
        let Some(coverage) = self.obj_hooks.coverage.as_mut() else {
            return;
        };
        let Some(row) = coverage_row(scanline) else {
            return;
        };
        let at = row * COVERAGE_WIDTH;

        let is_mode_0_or_1 = matches!(self.registers.bg_mode, BgMode::Zero | BgMode::One);
        let layer_priorities: [u8; 4] =
            std::array::from_fn(|p| obj_layer_priority(p as u8, is_mode_0_or_1));
        // Window x of main-screen pixel x (see `render_screen_pixels`).
        let window_shift = usize::from(hi_res_mode == HiResMode::True);
        let window_offset = usize::from(hi_res_mode == HiResMode::True);

        for x in from_x..COVERAGE_WIDTH {
            let (mut visible, mut front) = (0_u8, NO_OBJ);

            let in_color_window = self.registers.in_math_window(x as u16);
            let clipped_black = self
                .registers
                .force_main_screen_black
                .enabled(in_color_window);
            let obj_allowed = self.registers.main_obj_enabled
                && !(self.registers.main_obj_disabled_in_window
                    && self
                        .registers
                        .obj_in_window(((x << window_shift) | window_offset) as u16));

            if obj_allowed && !clipped_black {
                let bg_min = coverage.bg_only[x].min_priority;
                for (p, &lp) in layer_priorities.iter().enumerate() {
                    if lp < bg_min {
                        visible |= 1 << p;
                    }
                }
                if visible != 0
                    && self.registers.obj_color_math_enabled
                    && self.registers.color_math_enabled.enabled(in_color_window)
                {
                    // Same operands as `render_scanline`; adding or subtracting 0 without
                    // halving leaves the color as it is.
                    let (sub_color, sub_transparent) = if self.registers.sub_bg_obj_enabled {
                        let sub = self.buffers.sub_screen_rendered_pixels[x];
                        (sub.color, sub.layer == Layer::Backdrop)
                    } else {
                        (self.registers.sub_backdrop_color, false)
                    };
                    let divide = self.registers.color_math_divide_enabled && !sub_transparent;
                    if divide || sub_color != 0 {
                        visible |= OBJ_COLOR_MATH;
                    }
                }
                let resolved = self.buffers.main_screen_pixels[x];
                if resolved.min_layer == Layer::Obj && resolved.min_priority != u8::MAX {
                    front = self.obj_hooks.line_oam[x];
                }
            }

            coverage.visible[at + x] = visible;
            coverage.front[at + x] = front;
        }
    }

    // ---- host API ----

    /// Hides every OAM entry whose character number (bit `chr` of the 512-bit mask) is set;
    /// `None` hides nothing.
    pub fn set_obj_hide_mask(&mut self, mask: Option<[u64; 8]>) {
        self.obj_hooks.hide = mask.filter(|m| m.iter().any(|&w| w != 0));
    }

    #[must_use]
    pub fn obj_hide_mask(&self) -> Option<[u64; 8]> {
        self.obj_hooks.hide
    }

    /// Starts (or stops) recording OBJ coverage; the buffers fill as lines render.
    pub fn set_obj_coverage(&mut self, on: bool) {
        self.obj_hooks.set_coverage(on);
    }

    /// Per pixel ([`COVERAGE_WIDTH`] x [`COVERAGE_HEIGHT`]): bit `p` set if an OBJ pixel of
    /// priority `p` would be the visible main-screen pixel there.
    #[must_use]
    pub fn obj_visible(&self) -> Option<&[u8]> {
        self.obj_hooks
            .coverage
            .as_ref()
            .map(|c| c.visible.as_slice())
    }

    /// Per pixel: OAM index of the visible non-hidden OBJ pixel, [`NO_OBJ`] if none.
    #[must_use]
    pub fn obj_front(&self) -> Option<&[u8]> {
        self.obj_hooks.coverage.as_ref().map(|c| c.front.as_slice())
    }

    /// INIDISP of the last completed frame: (master brightness 0..=15, forced blank).
    #[must_use]
    pub fn frame_inidisp(&self) -> (u8, bool) {
        (
            self.obj_hooks.frame_brightness,
            self.obj_hooks.frame_forced_blank,
        )
    }

    /// Carries the host hooks of `old` (the PPU being replaced by a state load) over to this
    /// one. Per-line scratch is not carried: it belongs to the old state's line.
    pub(crate) fn adopt_obj_hooks(&mut self, old: &mut Self) {
        let hooks = std::mem::take(&mut old.obj_hooks);
        self.obj_hooks.hide = hooks.hide;
        self.obj_hooks.coverage = hooks.coverage;
        self.obj_hooks.frame_brightness = hooks.frame_brightness;
        self.obj_hooks.frame_forced_blank = hooks.frame_forced_blank;
    }
}
