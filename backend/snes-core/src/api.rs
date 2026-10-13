//! SNES public interface and main loop

use crate::apu::{Apu, ApuTickEffect};
pub use crate::apu::{set_spc_trace, SPC_CYCLES, SPC_TRACE}; // mmxrs patch: SPC700 trace (debug)
use crate::audio::AudioResampler;
use crate::bus::Bus;
use crate::input::SnesInputs;
use crate::memory::dma::{DmaStatus, DmaUnit};
use crate::memory::{CpuInternalRegisters, Memory};
pub use crate::ppu::wide::{LineRegs, MARGIN_MAP_CELLS, MAX_WIDE_MARGIN, MarginSprite};
pub use crate::ppu::{P2_PAGE_TILES, P2_PAGE_WORDS, P2_PAGES, P2Tiles}; // mmxrs patch
use crate::ppu::{Ppu, PpuTickEffect}; // mmxrs patch
// lttprs: OBJ coverage buffer layout
pub use crate::ppu::obj_hooks::{
    COVERAGE_HEIGHT as OBJ_COVERAGE_HEIGHT, COVERAGE_WIDTH as OBJ_COVERAGE_WIDTH,
    NO_OBJ as OBJ_COVERAGE_NONE, OBJ_COLOR_MATH as OBJ_COVERAGE_COLOR_MATH,
};
use crate::{bus, ppu};
use bincode::error::EncodeError;
use bincode::{Decode, Encode};
use crc::Crc;
use jgenesis_common::frontend::{
    AudioOutput, Color, EmulatorConfigTrait, EmulatorTrait, InputPoller, NtscPerFrameParams,
    PartialClone, RenderFrameOptions, Renderer, SaveWriter, TickEffect, TimingMode,
};
use jgenesis_proc_macros::{ConfigDisplay, FakeDecode, FakeEncode};
use snes_config::{AudioInterpolationMode, SnesAspectRatio, SnesButton};
use snes_coprocessors::st018::St018LoadError;
use std::fmt::{Debug, Display};
use std::num::NonZeroU64;
use std::{io, mem};
use thiserror::Error;
use wdc65816_emu::core::Wdc65816;
use wdc65816_emu::traits::BusInterface;

const MEMORY_REFRESH_MCLK: u64 = 536;
const MEMORY_REFRESH_CYCLES: u64 = 40;

// mmxrs patch: master clocks between power-on/reset and the CPU's first cycle
const CPU_STARTUP_MCLK: u64 = 186;

const CRC: Crc<u32> = Crc::<u32>::new(&crc::CRC_32_ISO_HDLC);

/// Runtime compatibility policy. These profiles preserve the two original forks' timing.
/// Host configuration: not encoded in legacy save states; retained by `load_state`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, FakeEncode, FakeDecode)]
pub enum TimingProfile {
    #[default]
    MmxHardware,
    LttpLegacy,
}
impl Display for TimingProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl TimingProfile {
    pub(crate) fn hardware(self) -> bool {
        self == Self::MmxHardware
    }
}

#[derive(Debug, Clone, Copy, Encode, Decode, ConfigDisplay)]
pub struct SnesEmulatorConfig {
    pub timing_profile: TimingProfile,
    pub forced_timing_mode: Option<TimingMode>,
    pub aspect_ratio: SnesAspectRatio,
    pub deinterlace: bool,
    pub audio_interpolation: AudioInterpolationMode,
    pub audio_60hz_hack: bool,
    pub gsu_overclock_factor: NonZeroU64,
    pub allow_opposing_joypad_directions: bool,
    pub mouse_sensitivity: f64,
}

impl EmulatorConfigTrait for SnesEmulatorConfig {
    fn with_overclocking_disabled(&self) -> Self {
        Self {
            gsu_overclock_factor: NonZeroU64::new(1).unwrap(),
            ..*self
        }
    }
}

impl Default for SnesEmulatorConfig {
    fn default() -> Self {
        Self {
            timing_profile: TimingProfile::default(),
            forced_timing_mode: None,
            aspect_ratio: SnesAspectRatio::default(),
            deinterlace: true,
            audio_interpolation: AudioInterpolationMode::default(),
            audio_60hz_hack: false,
            gsu_overclock_factor: NonZeroU64::new(1).unwrap(),
            allow_opposing_joypad_directions: false,
            mouse_sensitivity: 1.0,
        }
    }
}

pub type CoprocessorRomFn =
    dyn Fn() -> Result<Vec<u8>, (io::Error, String)> + Send + Sync + 'static;

#[derive(Default, FakeEncode, FakeDecode)]
pub struct CoprocessorRoms {
    pub dsp1: Option<Box<CoprocessorRomFn>>,
    pub dsp2: Option<Box<CoprocessorRomFn>>,
    pub dsp3: Option<Box<CoprocessorRomFn>>,
    pub dsp4: Option<Box<CoprocessorRomFn>>,
    pub st010: Option<Box<CoprocessorRomFn>>,
    pub st011: Option<Box<CoprocessorRomFn>>,
    pub st018: Option<Box<CoprocessorRomFn>>,
}

impl CoprocessorRoms {
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

#[derive(Debug, Error)]
pub enum SnesError<RErr, AErr, SErr> {
    #[error("Error rendering frame: {0}")]
    Render(RErr),
    #[error("Error outputting audio samples: {0}")]
    AudioOutput(AErr),
    #[error("Error persisting save file: {0}")]
    SaveWrite(SErr),
    #[error("Error encoding save file bytes: {0}")]
    SaveEncode(#[from] EncodeError),
}

#[derive(Debug, Error)]
pub enum SnesLoadError {
    #[error("ROM is only {size} bytes; all SNES ROM chips should be at least 32768 bytes")]
    RomTooSmall { size: usize },
    #[error("Cannot load DSP-1 cartridge because DSP-1 ROM is not configured")]
    MissingDsp1Rom,
    #[error("Cannot load DSP-2 cartridge because DSP-2 ROM is not configured")]
    MissingDsp2Rom,
    #[error("Cannot load DSP-3 cartridge because DSP-3 ROM is not configured")]
    MissingDsp3Rom,
    #[error("Cannot load DSP-4 cartridge because DSP-4 ROM is not configured")]
    MissingDsp4Rom,
    #[error("Cannot load ST010 cartridge because ST010 ROM is not configured")]
    MissingSt010Rom,
    #[error("Cannot load ST011 cartridge because ST011 ROM is not configured")]
    MissingSt011Rom,
    #[error("Cannot load ST018 cartridge because ST018 ROM is not configured")]
    MissingSt018Rom,
    #[error("Failed to load required coprocessor ROM from '{path}': {source}")]
    CoprocessorRomLoad {
        #[source]
        source: io::Error,
        path: String,
    },
    #[error("Invalid ST018 coprocessor ROM: {0}")]
    St018RomLoad(St018LoadError),
}

pub type SnesLoadResult<T> = Result<T, SnesLoadError>;

macro_rules! new_bus {
    ($self:expr) => {
        Bus {
            memory: &mut $self.memory,
            cpu_registers: &mut $self.cpu_registers,
            ppu: &mut $self.ppu,
            apu: &mut $self.apu,
            latched_interrupts: $self.latched_interrupts,
            access_master_cycles: 0,
            pending_write: None,
            apu_sync: false,
            apu_pre_ticked: 0,
            apu_sample: None,
        }
    };
}

#[derive(Debug, Clone, Copy, Encode, Decode)]
pub(crate) struct LatchedInterrupts {
    pub nmi: bool,
    pub irq: bool,
}

/// mmxrs patch: CPU overclock. Every CPU bus cycle (6/8/12 master cycles:
/// fast / slow / joypad ports, idle = fast) costs `mclk * 2 / 3` (4/5/8) or
/// `mclk / 2` (3/4/6) master cycles instead, so the CPU runs more
/// instructions per scanline while the PPU, APU, DMA/HDMA, the WRAM refresh
/// and the H/V timers keep hardware timing (all in master cycles). A host
/// setting: not in the bincode encoding (encodes nothing, decodes `Off`) and
/// kept across `load_state`, so a save state runs at the loading game's
/// factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum CpuOverclock {
    #[default]
    Off,
    X1_5,
    X2,
}

impl CpuOverclock {
    #[inline]
    #[must_use]
    pub fn scale(self, mclk: u64) -> u64 {
        match self {
            Self::Off => mclk,
            Self::X1_5 => mclk * 2 / 3,
            Self::X2 => mclk / 2,
        }
    }
}

impl std::str::FromStr for CpuOverclock {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "off" | "1" | "none" => Ok(Self::Off),
            "1.5" => Ok(Self::X1_5),
            "2" => Ok(Self::X2),
            _ => Err(format!("overclock must be off, 1.5 or 2, not {s:?}")),
        }
    }
}

impl Display for CpuOverclock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::X1_5 => "1.5",
            Self::X2 => "2",
        })
    }
}

impl Encode for CpuOverclock {
    fn encode<E: bincode::enc::Encoder>(&self, _: &mut E) -> Result<(), EncodeError> {
        Ok(())
    }
}

impl<C> Decode<C> for CpuOverclock {
    fn decode<D: bincode::de::Decoder<Context = C>>(
        _: &mut D,
    ) -> Result<Self, bincode::error::DecodeError> {
        Ok(Self::Off)
    }
}
bincode::impl_borrow_decode!(CpuOverclock);

/// mmxrs patch: see [`SnesEmulator::cpu_snapshot`].
pub struct CpuSnapshot(Wdc65816, CpuInternalRegisters, u8, DmaUnit);

#[derive(Encode, Decode, PartialClone)]
pub struct SnesEmulator {
    main_cpu: Wdc65816,
    cpu_registers: CpuInternalRegisters,
    dma_unit: DmaUnit,
    #[partial_clone(partial)]
    memory: Memory,
    ppu: Ppu,
    apu: Apu,
    audio_resampler: AudioResampler,
    total_master_cycles: u64,
    latched_interrupts: Option<LatchedInterrupts>,
    memory_refresh_pending: bool,
    timing_mode: TimingMode,
    aspect_ratio: SnesAspectRatio,
    frame_count: u64,
    last_sram_checksum: u32,
    // lttprs: routine traps (see `traps` module docs)
    traps: crate::traps::TrapSet,
    // Following fields only stored here to enable hard reset
    #[partial_clone(default)]
    coprocessor_roms: CoprocessorRoms,
    emulator_config: SnesEmulatorConfig,
    cpu_overclock: CpuOverclock, // mmxrs patch
}

impl SnesEmulator {
    /// # Errors
    ///
    /// This function will return an error if it is unable to load the cartridge ROM for any reason.
    pub fn create<S: SaveWriter>(
        rom: Vec<u8>,
        config: SnesEmulatorConfig,
        coprocessor_roms: CoprocessorRoms,
        save_writer: &mut S,
    ) -> SnesLoadResult<Self> {
        let main_cpu = Wdc65816::new();
        let cpu_registers = CpuInternalRegisters::new(&config);
        let dma_unit = DmaUnit::new();

        let initial_sram = save_writer.load_bytes("sav").ok();
        let sram_checksum = initial_sram.as_ref().map_or(0, |sram| CRC.checksum(sram));
        let memory = Memory::create(
            rom,
            initial_sram,
            &coprocessor_roms,
            config.forced_timing_mode,
            config.gsu_overclock_factor,
            save_writer,
        )?;

        let timing_mode = config
            .forced_timing_mode
            .unwrap_or_else(|| memory.cartridge_timing_mode());
        let ppu = Ppu::new(timing_mode, config);
        let apu = Apu::new(timing_mode, config);

        log::info!("Running with timing/display mode {timing_mode}");

        let mut emulator = Self {
            main_cpu,
            cpu_registers,
            dma_unit,
            memory,
            ppu,
            apu,
            audio_resampler: AudioResampler::new(config.timing_profile),
            total_master_cycles: 0,
            latched_interrupts: None,
            memory_refresh_pending: false,
            timing_mode,
            aspect_ratio: config.aspect_ratio,
            frame_count: 0,
            last_sram_checksum: sram_checksum,
            traps: crate::traps::TrapSet::default(),
            coprocessor_roms,
            emulator_config: config,
            cpu_overclock: CpuOverclock::Off, // mmxrs patch
        };

        // Reset CPU so that execution starts from the right place
        emulator.main_cpu.reset(&mut new_bus!(emulator));
        emulator.run_startup_delay(); // mmxrs patch

        Ok(emulator)
    }

    /// mmxrs patch: the CPU leaves reset `CPU_STARTUP_MCLK` master clocks after the
    /// PPU, APU and timers start (as Mesen 2 has it): run everything but the CPU for
    /// that long. Without it the game's whole frame timing is shifted by those clocks
    /// against hardware from power-on. docs/oracle.md.
    fn run_startup_delay(&mut self) {
        if !self.emulator_config.timing_profile.hardware() {
            return;
        }
        let n = CPU_STARTUP_MCLK;
        let mut done = 0;
        while done < n {
            let step = 2.min(n - done);
            if let ApuTickEffect::OutputSample(l, r) = self.apu.tick(step) {
                self.audio_resampler.collect_sample(l, r);
            }
            self.memory.tick(step);
            let _ = self.ppu.tick(step);
            self.cpu_registers
                .tick(step, &self.ppu, &SnesInputs::default());
            self.total_master_cycles += step;
            done += step;
        }
    }

    // mmxrs patch begin: hooks for mmx-core (routine traps, co-op, inspection)
    #[must_use]
    pub fn cpu_registers(&self) -> &wdc65816_emu::core::Registers {
        self.main_cpu.registers()
    }

    pub fn set_cpu_registers(&mut self, registers: wdc65816_emu::core::Registers) {
        self.main_cpu.set_registers(registers);
    }

    /// True between instructions (next CPU tick fetches an opcode or starts an interrupt).
    #[must_use]
    pub fn at_instruction_boundary(&self) -> bool {
        !self.main_cpu.is_mid_instruction()
    }

    /// True when the next CPU tick fetches the opcode at `PBR:PC`, i.e. the CPU is at a boundary
    /// and will not start an NMI/IRQ, stay in WAI/STP, or reset instead. Same trick as
    /// `bus::next_cpu_cycle_mclk`: tick a clone of the CPU against a bus that has the real
    /// interrupt lines but no side effects. An opcode fetch is the only first cycle that
    /// increments PC (hardware interrupts re-read PC without incrementing).
    /// DMA is not considered: a pending DMA still runs before the fetch.
    #[must_use]
    pub fn next_tick_fetches_opcode(&mut self) -> bool {
        struct Probe<'a, 'b>(&'a Bus<'b>);

        impl BusInterface for Probe<'_, '_> {
            fn read(&mut self, _address: u32) -> u8 {
                0
            }
            fn write(&mut self, _address: u32, _value: u8) {}
            fn idle(&mut self) {}
            fn nmi(&self) -> bool {
                self.0.nmi()
            }
            fn acknowledge_nmi(&mut self) {}
            fn irq(&self) -> bool {
                self.0.irq()
            }
            fn halt(&self) -> bool {
                self.0.halt()
            }
            fn reset(&self) -> bool {
                self.0.reset()
            }
        }

        if self.main_cpu.is_mid_instruction() {
            return false;
        }
        let pc = self.main_cpu.registers().pc;
        let mut cpu = self.main_cpu.clone();
        cpu.tick(&mut Probe(&new_bus!(self)));
        cpu.is_mid_instruction() && cpu.registers().pc != pc
    }

    pub fn set_cpu_overclock(&mut self, o: CpuOverclock) {
        self.cpu_overclock = o;
    }

    #[must_use]
    pub fn cpu_overclock(&self) -> CpuOverclock {
        self.cpu_overclock
    }

    /// Master clock (debug: lag-frame bisection).
    #[must_use]
    pub fn master_cycles(&self) -> u64 {
        self.total_master_cycles
    }

    /// mmxrs patch: PPU position `(scanline, master cycles into the line)` (debug: oracle traces).
    #[must_use]
    pub fn ppu_position(&self) -> (u16, u64) {
        (self.ppu.scanline(), self.ppu.scanline_master_cycles())
    }

    #[must_use]
    pub fn wram(&self) -> &[u8; 0x20000] {
        self.memory.main_ram()
    }

    pub fn wram_mut(&mut self) -> &mut [u8; 0x20000] {
        self.memory.main_ram_mut()
    }

    #[must_use]
    pub fn oam_bytes(&self) -> [u8; 544] {
        self.ppu.oam_bytes()
    }

    #[must_use]
    pub fn cgram(&self) -> [u16; 256] {
        self.ppu.cgram()
    }

    // mmxrs patch: SPC700 audio RAM (64 KiB) and DSP registers ($00-$7F)
    #[must_use]
    pub fn spc_ram(&self) -> &[u8] {
        self.apu.audio_ram()
    }

    #[must_use]
    pub fn dsp_register(&self, address: u8) -> u8 {
        self.apu.dsp_register(address)
    }

    /// Research knob, process-wide: bit n mutes DSP voice n in the output.
    pub fn set_voice_mute(mask: u8) {
        crate::apu::dsp::VOICE_MUTE.store(mask, std::sync::atomic::Ordering::Relaxed);
    }

    // mmxrs patch: raw VRAM words and OBSEL (sprite tile base, gap; word addresses)
    #[must_use]
    pub fn vram(&self) -> &[u16; 0x8000] {
        self.ppu.vram()
    }

    #[must_use]
    pub fn obsel(&self) -> (u16, u16) {
        self.ppu.obsel()
    }

    // mmxrs patch: P2 shadow sprite tile page (see `ppu::P2Tiles`)
    pub fn p2_tile_page_mut(&mut self) -> &mut [u16; ppu::P2_PAGE_WORDS * ppu::P2_PAGES] {
        self.ppu.p2_tile_page_mut()
    }

    pub fn set_p2_tile_slots(&mut self, mask: u128, page1: u128, page2: u128) {
        self.ppu.set_p2_tile_slots(mask, page1, page2);
    }

    pub fn set_p2_tile_slots_4p(&mut self, mask: u128, page1: u128, page2: u128, page4: u128) {
        self.ppu.set_p2_tile_slots_4p(mask, page1, page2, page4);
    }

    /// The slots in `mask` are drawn with `palette` (a ninth sprite palette),
    /// from the next OAM upload, with the page. See `ppu::P2Tiles::pal`.
    pub fn set_p2_palette(&mut self, mask: u128, palette: [u16; 16]) {
        self.ppu.set_p2_palette(mask, palette);
    }

    pub fn set_p3_palette(&mut self, mask: u128, palette: [u16; 16]) {
        self.ppu.set_p3_palette(mask, palette);
    }

    pub fn set_p4_palette(&mut self, mask: u128, palette: [u16; 16]) {
        self.ppu.set_p4_palette(mask, palette);
    }

    /// The whole P2 page state (live and pending). Not in the bincode
    /// encoding of `SnesEmulator` (old save states stay loadable): the host
    /// encodes it next to the state and puts it back with `set_p2_tiles`.
    #[must_use]
    pub fn p2_tiles(&self) -> &P2Tiles {
        self.ppu.p2_tiles()
    }

    pub fn set_p2_tiles(&mut self, t: P2Tiles) {
        self.ppu.set_p2_tiles(t);
    }

    // mmxrs patch: display-only offset for the OAM slots in `slots` (render
    // time only; not in OAM or any save state)
    pub fn set_sprite_offset(&mut self, slots: u128, dx: i16, dy: i16) {
        self.ppu.set_sprite_offset(slots, dx, dy);
    }

    // mmxrs patch: display-only widescreen (see `ppu::wide`)
    pub fn set_wide_margin(&mut self, margin: u16) {
        self.ppu.set_wide_margin(margin);
    }

    #[must_use]
    pub fn wide_margin(&self) -> u16 {
        self.ppu.wide_margin()
    }

    pub fn set_margin_tiles(&mut self, bg: usize, cells: Option<&[u32]>) {
        self.ppu.set_margin_tiles(bg, cells);
    }

    pub fn set_line_record(&mut self, on: bool) {
        self.ppu.set_line_record(on);
    }

    /// Margin sprites for the next frames (live from the next OAM upload).
    pub fn set_margin_sprites(&mut self, sprites: Vec<MarginSprite>) {
        self.ppu.set_margin_sprites(sprites);
    }

    /// Widescreen HUD: OAM slots drawn `margin` px left / right (live from
    /// the next OAM upload; no effect without a margin).
    pub fn set_hud_shift(&mut self, left: u128, right: u128) {
        self.ppu.set_hud_shift(left, right);
    }

    #[must_use]
    pub fn hud_shift(&self) -> (u128, u128) {
        self.ppu.hud_shift()
    }

    /// The CPU as a whole (registers and the between-cycle temporaries a
    /// save state holds), the 5A22 registers, the open bus and the DMA
    /// unit, so a display-only isolated call leaves the save state (and the
    /// timing of what follows) as it found it. The DMA unit matters: an
    /// isolated call that reaches an HDMA start or reload point runs that
    /// HDMA inside the call ([`Self::run_cpu_isolated`] ticks the DMA unit);
    /// without putting the unit back, the real HDMA (and its CPU stall)
    /// would not happen at its real time.
    #[must_use]
    pub fn cpu_snapshot(&self) -> CpuSnapshot {
        CpuSnapshot(
            self.main_cpu.clone(),
            self.cpu_registers.clone(),
            self.memory.cpu_open_bus(),
            self.dma_unit.clone(),
        )
    }

    pub fn restore_cpu_snapshot(&mut self, s: CpuSnapshot) {
        self.main_cpu = s.0;
        self.cpu_registers = s.1;
        self.memory.set_cpu_open_bus(s.2);
        self.dma_unit = s.3;
    }

    #[must_use]
    pub fn line_regs(&self) -> &[ppu::wide::LineRegs] {
        self.ppu.line_regs()
    }

    /// The VRAM tilemap entry BG `bg` shows at BG-space `(x, y)`.
    #[must_use]
    pub fn bg_map_entry(&self, bg: usize, x: u16, y: u16) -> u16 {
        self.ppu.bg_map_entry(bg, x, y)
    }

    /// Research: the PPU registers, `{:#?}`.
    #[must_use]
    pub fn ppu_registers_debug(&self) -> String {
        self.ppu.registers_debug()
    }

    /// Tick only the main CPU until it sits at an instruction boundary with
    /// `PBR:PC == until_pbr_pc` (returns true) or `max_cycles` CPU ticks pass
    /// (returns false). PPU, APU, coprocessors, DMA and `total_master_cycles`
    /// do not advance; deferred writes are applied after each tick.
    pub fn run_cpu_isolated(&mut self, until_pbr_pc: u32, max_cycles: u32) -> bool {
        for _ in 0..max_cycles {
            if !self.main_cpu.is_mid_instruction() {
                let r = self.main_cpu.registers();
                if (u32::from(r.pbr) << 16) | u32::from(r.pc) == until_pbr_pc {
                    return true;
                }
            }
            let mut bus = new_bus!(self);
            self.main_cpu.tick(&mut bus);
            // The multiply/divide unit is clocked by CPU cycles, so it keeps running.
            bus.cpu_registers.tick_cpu_cycle();
            if let Some((address, value)) = bus.pending_write {
                bus.apply_write(address, value);
            }
            // A general-purpose DMA the routine starts ($420B) must complete
            // before its next instruction reads the destination, as on
            // hardware (the CPU is halted). Run it to completion here; the PPU
            // and APU still do not advance.
            loop {
                let mut bus = new_bus!(self);
                match self.dma_unit.tick(
                    &mut bus,
                    self.total_master_cycles,
                    bus::next_cpu_cycle_mclk(&self.main_cpu),
                ) {
                    DmaStatus::None => break,
                    DmaStatus::InProgress { .. } => {}
                }
            }
        }
        false
    }
    pub fn cgram_ref(&self) -> &[u16] {
        self.ppu.cgram_ref()
    }
    // mmxrs patch end

    /// Arms a trap at a 24-bit program address (`bank << 16 | pc`).
    pub fn add_trap(&mut self, address: u32) {
        self.traps.add(address & 0xFF_FFFF);
    }

    pub fn remove_trap(&mut self, address: u32) {
        self.traps.remove(address & 0xFF_FFFF);
    }

    pub fn clear_traps(&mut self) {
        self.traps.clear();
    }

    /// Returns the trapped address if the CPU stopped at an armed trap during the last tick.
    /// The host must run its routine (and move PC off the trap, e.g. by emulating RTS/RTL)
    /// or call [`Self::bypass_trap_once`] to execute the original code.
    pub fn take_trap(&mut self) -> Option<u32> {
        self.traps.take_pending()
    }

    /// Lets the CPU execute the original code at `address` the next time it is reached.
    pub fn bypass_trap_once(&mut self, address: u32) {
        self.traps.bypass_once(address & 0xFF_FFFF);
    }

    /// Parks the main CPU at `pc` for a sequence of CPU cycles of the given lengths (master
    /// cycles each, with [`crate::traps::STALL_INSTRUCTION_END`] set on the last cycle of each
    /// instruction, where interrupts are sampled), as recorded from the original routine. DMA and DRAM refresh interleave
    /// as usual, interrupts are still taken, and handlers (or a thread they switch to) do not
    /// use up the sequence. Used to give a ported routine the exact timing of the original.
    pub fn stall_cycles(&mut self, pc: u32, cycles: impl IntoIterator<Item = u8>) {
        self.traps.set_stall(pc & 0xFF_FFFF, cycles);
    }

    /// Length in master cycles of the last tick if it was a main CPU cycle (None for DMA or
    /// DRAM refresh ticks).
    #[must_use]
    pub fn last_cpu_cycle(&self) -> Option<u8> {
        self.traps.last_cpu_cycle
    }

    /// Main CPU interrupt nesting depth (interrupt entries minus RTIs).
    #[must_use]
    pub fn cpu_interrupt_depth(&self) -> u16 {
        self.main_cpu.interrupt_depth()
    }

    #[must_use]
    pub fn cpu_mid_instruction(&self) -> bool {
        self.main_cpu.is_mid_instruction()
    }

    /// Side-effecting bus read through the full SNES memory map (registers included).
    pub fn bus_read(&mut self, address: u32) -> u8 {
        new_bus!(self).read(address & 0xFF_FFFF)
    }

    /// Side-effecting bus write through the full SNES memory map (registers included).
    pub fn bus_write(&mut self, address: u32, value: u8) {
        new_bus!(self).apply_write(address & 0xFF_FFFF, value);
    }

    #[must_use]
    pub fn sram(&self) -> Option<&[u8]> {
        self.memory.sram()
    }

    pub fn sram_mut(&mut self) -> Option<&mut [u8]> {
        self.memory.sram_mut()
    }

    /// Mutable ROM image (plain cartridges only; `None` for coprocessor carts). Patching it
    /// takes effect on the next fetch/DMA from that address.
    pub fn rom_mut(&mut self) -> Option<&mut [u8]> {
        self.memory.rom_mut()
    }

    /// (OAM low table as words, OAM high table bytes)
    #[must_use]
    pub fn oam(&self) -> (&[u16], &[u8]) {
        self.ppu.oam()
    }

    #[must_use]
    pub fn frame_buffer(&self) -> &[Color] {
        self.ppu.frame_buffer()
    }

    #[must_use]
    pub fn frame_size(&self) -> jgenesis_common::frontend::FrameSize {
        self.ppu.frame_size()
    }

    /// Hides every OAM entry whose 9-bit character number (its own OAM name field, i.e. the
    /// top-left character of a large sprite) has its bit set in the 512-bit `mask`; such
    /// entries render fully transparent but still count against the per-line sprite limits.
    /// `None` (or an all-zero mask) hides nothing. Host state: not part of save states, and
    /// kept across [`EmulatorTrait::load_state`].
    pub fn set_obj_hide_mask(&mut self, mask: Option<[u64; 8]>) {
        self.ppu.set_obj_hide_mask(mask);
    }

    #[must_use]
    pub fn obj_hide_mask(&self) -> Option<[u64; 8]> {
        self.ppu.obj_hide_mask()
    }

    /// Starts or stops recording per-pixel OBJ coverage of the main screen
    /// ([`Self::obj_visible`], [`Self::obj_front`]). Off by default (no cost).
    pub fn set_obj_coverage(&mut self, on: bool) {
        self.ppu.set_obj_coverage(on);
    }

    /// [`OBJ_COVERAGE_WIDTH`] x [`OBJ_COVERAGE_HEIGHT`], row = logical scanline - 1 (main-screen
    /// x even in hi-res; interlaced frames record logical lines): bit `p` set if an OBJ pixel of
    /// priority `p` would be the visible main-screen pixel there (beats the backgrounds, not
    /// clipped by windows / forced black). Bit 7 ([`OBJ_COVERAGE_COLOR_MATH`]) is set where
    /// such a pixel would also get color math if its palette is 4..=7. `None` while off.
    #[must_use]
    pub fn obj_visible(&self) -> Option<&[u8]> {
        self.ppu.obj_visible()
    }

    /// Same layout as [`Self::obj_visible`]: OAM index of the visible non-hidden OBJ pixel,
    /// [`OBJ_COVERAGE_NONE`] where the visible pixel is not an OBJ.
    #[must_use]
    pub fn obj_front(&self) -> Option<&[u8]> {
        self.ppu.obj_front()
    }

    /// INIDISP when the last frame completed: (master brightness 0..=15, forced blank).
    #[must_use]
    pub fn frame_inidisp(&self) -> (u8, bool) {
        self.ppu.frame_inidisp()
    }

    #[must_use]
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }

    #[must_use]
    pub fn cartridge_title(&mut self) -> String {
        self.memory.cartridge_title()
    }

    #[inline]
    #[must_use]
    pub fn has_sram(&self) -> bool {
        self.memory.has_battery_backed_sram()
    }

    pub fn copy_cgram(&self, out: &mut [Color]) {
        self.ppu.copy_cgram(out);
    }

    pub fn copy_vram_2bpp(&self, out: &mut [Color], palette: u8, row_len: usize) {
        self.ppu.copy_vram_2bpp(out, palette, row_len);
    }

    pub fn copy_vram_4bpp(&self, out: &mut [Color], palette: u8, row_len: usize) {
        self.ppu.copy_vram_4bpp(out, palette, row_len);
    }

    pub fn copy_vram_8bpp(&self, out: &mut [Color], row_len: usize) {
        self.ppu.copy_vram_8bpp(out, row_len);
    }

    pub fn copy_vram_mode7(&self, out: &mut [Color], row_len: usize) {
        self.ppu.copy_vram_mode7(out, row_len);
    }

    fn render_options(&self) -> RenderFrameOptions {
        let frame_size = self.ppu.frame_size();
        let aspect_ratio = self.aspect_ratio.to_pixel_aspect_ratio(frame_size);

        RenderFrameOptions {
            pixel_aspect_ratio: aspect_ratio,
            composite_params: Some(self.ppu.composite_params()),
            ntsc_per_frame_params: Some(NtscPerFrameParams {
                frame_phase_offset: 2 * self.ppu.frame_start_cycles(),
                per_line_phase_offset: 2 * ppu::MCLKS_PER_NORMAL_SCANLINE,
            }),
            ..RenderFrameOptions::default()
        }
    }
}

impl EmulatorTrait for SnesEmulator {
    type Button = SnesButton;
    type Inputs = SnesInputs;
    type Config = SnesEmulatorConfig;
    type SaveState = Self;

    type Err<
        RErr: Debug + Display + Send + Sync + 'static,
        AErr: Debug + Display + Send + Sync + 'static,
        SErr: Debug + Display + Send + Sync + 'static,
    > = SnesError<RErr, AErr, SErr>;

    fn tick<R, A, I, S>(
        &mut self,
        renderer: &mut R,
        audio_output: &mut A,
        input_poller: &mut I,
        save_writer: &mut S,
    ) -> Result<TickEffect, Self::Err<R::Err, A::Err, S::Err>>
    where
        R: Renderer,
        A: AudioOutput,
        I: InputPoller<Self::Inputs>,
        S: SaveWriter,
    {
        let mut apu_pre: (u64, Option<(f64, f64)>) = (0, None); // mmxrs patch
        // lttprs: is the CPU parked at a host stall address (see `traps`)?
        let pc = {
            let r = self.main_cpu.registers();
            (u32::from(r.pbr) << 16) | u32::from(r.pc)
        };
        let boundary = !self.main_cpu.is_mid_instruction();
        let stall_cycle = if boundary {
            self.traps.next_stall_cycle(pc)
        } else {
            0
        };
        self.traps.last_cpu_cycle = None;

        let (master_cycles_elapsed, pending_write) = if self.memory_refresh_pending {
            // The CPU (including DMA) halts for 40 cycles partway through every scanline so that
            // the system can refresh DRAM (used for work RAM)
            self.memory_refresh_pending = false;
            if self.emulator_config.timing_profile.hardware() {
                self.dma_unit.exclude_refresh(MEMORY_REFRESH_CYCLES);
            } // mmxrs patch
            (MEMORY_REFRESH_CYCLES, None)
        } else {
            let mut bus = new_bus!(self);
            bus.apu_sync = self.emulator_config.timing_profile.hardware(); // mmxrs patch
            // mmxrs patch: overclock; DMA aligns its end to the scaled CPU cycle
            let oc = self.cpu_overclock;
            let next_mclk = bus::next_cpu_cycle_mclk(&self.main_cpu);

            match self
                .dma_unit
                .tick(&mut bus, self.total_master_cycles, move |b: &Bus<'_>| {
                    oc.scale(next_mclk(b))
                }) {
                DmaStatus::None => {
                    // lttprs: host stalls and routine traps, both at instruction boundaries
                    if stall_cycle > 0 && !self.main_cpu.interrupt_triggered() {
                        // Idle for exactly the recorded length of the original's next cycle,
                        // sampling interrupts only where the original ended an instruction
                        if stall_cycle & crate::traps::STALL_INSTRUCTION_END != 0 {
                            self.main_cpu.poll_interrupts(&mut bus);
                        }
                        bus.idle();
                        bus.access_master_cycles =
                            (stall_cycle & !crate::traps::STALL_INSTRUCTION_END).into();
                        self.traps.pop_stall_cycle(pc);
                    } else if boundary && self.traps.check(pc) {
                        // Burn one idle cycle; the host runs the routine before the next tick
                        bus.idle();
                    } else {
                        // DMA not in progress, tick CPU
                        self.main_cpu.tick(&mut bus);
                    }
                    self.traps.last_cpu_cycle = u8::try_from(bus.access_master_cycles).ok();
                    self.latched_interrupts = None;

                    bus.cpu_registers.tick_cpu_cycle();
                    apu_pre = (bus.apu_pre_ticked, bus.apu_sample); // mmxrs patch

                    (oc.scale(bus.access_master_cycles), bus.pending_write)
                }
                DmaStatus::InProgress {
                    master_cycles_elapsed,
                } => {
                    // Latch interrupt lines at the start of DMA to emulate interrupt tests being
                    // delayed by one cycle after the DMA ends.
                    // Wild Guns depends on this
                    if self.latched_interrupts.is_none() {
                        self.latched_interrupts = Some(LatchedInterrupts {
                            nmi: bus.nmi(),
                            irq: bus.irq(),
                        });
                    }

                    (master_cycles_elapsed, None)
                }
            }
        };
        debug_assert!(master_cycles_elapsed > 0);

        // Copy WRIO from CPU to PPU for possible H/V counter latching
        self.ppu.update_wrio(self.cpu_registers.wrio_register());

        // Possibly latch H/V counter from the controller (e.g. Super Scope)
        if let Some((h, v)) = self.cpu_registers.controller_hv_latch() {
            self.ppu
                .update_controller_hv_latch(h, v, master_cycles_elapsed);
        }

        // mmxrs patch: minus the part of the cycle a CPU read of the APU ports already ran
        let mut collected = false;
        if let Some((l, r)) = apu_pre.1 {
            self.audio_resampler.collect_sample(l, r);
            collected = true;
        }
        if let ApuTickEffect::OutputSample(sample_l, sample_r) = self
            .apu
            .tick(master_cycles_elapsed - apu_pre.0.min(master_cycles_elapsed))
        {
            self.audio_resampler.collect_sample(sample_l, sample_r);
            collected = true;
        }

        // mmxrs perf: the resampler only has output right after it took a sample in
        if collected {
            self.audio_resampler
                .output_samples(audio_output)
                .map_err(SnesError::AudioOutput)?;
        }

        self.memory.tick(master_cycles_elapsed);

        let prev_scanline_mclk = self.ppu.scanline_master_cycles();
        let mut tick_effect = TickEffect::None;
        if self.ppu.tick(master_cycles_elapsed) == PpuTickEffect::FrameComplete {
            // mmxrs patch: the widened frame when widescreen is on
            let (fps, options) = (self.target_fps(), self.render_options());
            match self.ppu.compose_wide() {
                Some((buf, size)) => renderer.render_frame(buf, size, fps, options),
                None => renderer.render_frame(
                    self.ppu.frame_buffer(),
                    self.ppu.frame_size(),
                    fps,
                    options,
                ),
            }
            .map_err(SnesError::Render)?;

            // Only persist SRAM if it's changed since the last write, and only check ~twice per
            // second because of the checksum calculation
            if self.memory.has_battery_backed_sram()
                && let Some(sram) = self.memory.sram()
                && self.frame_count.is_multiple_of(30)
            {
                let checksum = CRC.checksum(sram);
                if checksum != self.last_sram_checksum {
                    save_writer
                        .persist_bytes("sav", sram)
                        .map_err(SnesError::SaveWrite)?;
                    self.memory
                        .write_auxiliary_save_files(save_writer)
                        .map_err(SnesError::SaveWrite)?;

                    self.last_sram_checksum = checksum;
                }
            }

            self.frame_count += 1;
            tick_effect = TickEffect::FrameRendered;
        }

        let inputs = input_poller.poll();
        self.cpu_registers
            .tick(master_cycles_elapsed, &self.ppu, inputs);

        // CPU reads are applied before advancing other components but CPU writes are applied after.
        // This fixes freezing in Rendering Ranger R2
        // TODO this is slightly inaccurate - reads should happen 4 mclks before writes, not a full CPU cycle
        if let Some((address, value)) = pending_write {
            new_bus!(self).apply_write(address, value);
        }

        self.total_master_cycles += master_cycles_elapsed;

        if prev_scanline_mclk < MEMORY_REFRESH_MCLK
            && self.ppu.scanline_master_cycles() >= MEMORY_REFRESH_MCLK
        {
            self.memory_refresh_pending = true;
        }

        Ok(tick_effect)
    }

    fn force_render<R>(&mut self, renderer: &mut R) -> Result<(), R::Err>
    where
        R: Renderer,
    {
        let frame_size = self.ppu.frame_size();
        renderer.render_frame(
            self.ppu.frame_buffer(),
            frame_size,
            self.target_fps(),
            self.render_options(),
        )
    }

    fn reload_config(&mut self, config: &Self::Config) {
        // Timing is a per-instance compatibility contract. Changing ordinary video/audio
        // preferences must not switch CPU/APU clocks or invalidate saved resampler history.
        let config = &SnesEmulatorConfig {
            timing_profile: self.emulator_config.timing_profile,
            ..*config
        };
        self.aspect_ratio = config.aspect_ratio;
        self.ppu.update_config(*config);
        self.apu.update_config(*config);
        self.memory
            .update_gsu_overclock_factor(config.gsu_overclock_factor);
        self.cpu_registers.reload_config(config);

        self.emulator_config = *config;
    }

    fn soft_reset(&mut self) {
        log::info!("Soft resetting");

        // Reset memory before CPU because some coprocessors (Super FX) block access to the
        // RESET interrupt vector while the coprocessor is running
        self.memory.reset();

        self.main_cpu.reset(&mut new_bus!(self));
        self.cpu_registers.reset();
        self.ppu.reset();
        self.apu.reset();
        self.run_startup_delay(); // mmxrs patch
    }

    fn hard_reset<S: SaveWriter>(&mut self, save_writer: &mut S) {
        log::info!("Hard resetting");

        let rom = self.memory.take_rom();

        let coprocessor_roms = mem::take(&mut self.coprocessor_roms);
        let oc = self.cpu_overclock; // mmxrs patch
        *self = Self::create(rom, self.emulator_config, coprocessor_roms, save_writer)
            .expect("Hard resetting should never fail to load");
        self.cpu_overclock = oc; // mmxrs patch
    }

    fn load_state(&mut self, mut state: Self::SaveState) {
        state.memory.take_rom_from(&mut self.memory);
        // Legacy codecs intentionally omit the host timing policy.
        state.emulator_config.timing_profile = self.emulator_config.timing_profile;
        state.ppu.timing_profile = self.emulator_config.timing_profile;
        state.apu.timing_profile = self.emulator_config.timing_profile;
        state.coprocessor_roms = mem::take(&mut self.coprocessor_roms);
        let wide = self.ppu.take_wide(); // mmxrs patch: widescreen is a frontend setting
        state.cpu_overclock = self.cpu_overclock; // mmxrs patch: so is the overclock
        // lttprs: OBJ hooks are host state, not part of the save state
        state.ppu.adopt_obj_hooks(&mut self.ppu);
        *self = state;
        self.ppu.put_wide(wide); // mmxrs patch
    }

    fn to_save_state(&self) -> Self::SaveState {
        self.partial_clone()
    }

    fn target_fps(&self) -> f64 {
        match (self.timing_mode, self.emulator_config.audio_60hz_hack) {
            (TimingMode::Ntsc, true) => 60.0,
            (TimingMode::Ntsc, false) => 60.0988,
            (TimingMode::Pal, true) => 50.0,
            (TimingMode::Pal, false) => 50.007,
        }
    }

    fn update_audio_output_frequency(&mut self, output_frequency: u64) {
        self.audio_resampler
            .update_output_frequency(output_frequency);
    }
}

/// Pre-RecompArch MMX snapshot layout. Game-specific snapshot tails remain owned by MMX.
#[derive(Encode, Decode)]
pub struct LegacyMmxSaveState {
    main_cpu: wdc65816_emu::core::LegacyMmxCpu,
    cpu_registers: CpuInternalRegisters,
    dma_unit: DmaUnit,
    memory: Memory,
    ppu: Ppu,
    apu: Apu,
    audio_resampler: AudioResampler,
    total_master_cycles: u64,
    latched_interrupts: Option<LatchedInterrupts>,
    memory_refresh_pending: bool,
    timing_mode: TimingMode,
    aspect_ratio: SnesAspectRatio,
    frame_count: u64,
    last_sram_checksum: u32,
    // Following fields only stored here to enable hard reset
    coprocessor_roms: CoprocessorRoms,
    emulator_config: SnesEmulatorConfig,
    cpu_overclock: CpuOverclock, // mmxrs patch
}
impl From<SnesEmulator> for LegacyMmxSaveState {
    fn from(emu: SnesEmulator) -> Self {
        let SnesEmulator {
            main_cpu,
            cpu_registers,
            dma_unit,
            memory,
            ppu,
            apu,
            audio_resampler,
            total_master_cycles,
            latched_interrupts,
            memory_refresh_pending,
            timing_mode,
            aspect_ratio,
            frame_count,
            last_sram_checksum,
            coprocessor_roms,
            emulator_config,
            cpu_overclock,
            ..
        } = emu;
        Self {
            main_cpu: main_cpu.into(),
            cpu_registers,
            dma_unit,
            memory,
            ppu,
            apu,
            audio_resampler,
            total_master_cycles,
            latched_interrupts,
            memory_refresh_pending,
            timing_mode,
            aspect_ratio,
            frame_count,
            last_sram_checksum,
            coprocessor_roms,
            emulator_config,
            cpu_overclock,
        }
    }
}
impl From<LegacyMmxSaveState> for SnesEmulator {
    fn from(emu: LegacyMmxSaveState) -> Self {
        let LegacyMmxSaveState {
            main_cpu,
            cpu_registers,
            dma_unit,
            memory,
            ppu,
            apu,
            audio_resampler,
            total_master_cycles,
            latched_interrupts,
            memory_refresh_pending,
            timing_mode,
            aspect_ratio,
            frame_count,
            last_sram_checksum,
            coprocessor_roms,
            emulator_config,
            cpu_overclock,
        } = emu;
        Self {
            main_cpu: main_cpu.into(),
            cpu_registers,
            dma_unit,
            memory,
            ppu,
            apu,
            audio_resampler,
            total_master_cycles,
            latched_interrupts,
            memory_refresh_pending,
            timing_mode,
            aspect_ratio,
            frame_count,
            last_sram_checksum,
            coprocessor_roms,
            emulator_config,
            cpu_overclock,
            traps: crate::traps::TrapSet::default(),
        }
    }
}
impl SnesEmulator {
    pub fn to_legacy_mmx_save_state(&self) -> LegacyMmxSaveState {
        self.partial_clone().into()
    }
}
