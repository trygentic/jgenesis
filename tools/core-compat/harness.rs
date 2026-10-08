// SPDX-License-Identifier: GPL-3.0-only
// Headless comparison against the original MMX/LTTP forks; no game data is included.
use jgenesis_common::frontend::{
    AudioOutput, Color, ConstantInputPoller, EmulatorTrait, FrameSize, RenderFrameOptions,
    Renderer, SaveWriter, TickEffect,
};
use snes_config::SnesButton;
use snes_core::api::{CoprocessorRoms, SnesEmulator, SnesEmulatorConfig};
use snes_core::input::SnesInputs;
use std::convert::Infallible;

fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
#[derive(Default)]
struct Sink {
    video: u64,
    audio: Vec<u8>,
}
impl Renderer for Sink {
    type Err = Infallible;
    fn render_frame(
        &mut self,
        pixels: &[Color],
        size: FrameSize,
        _: f64,
        _: RenderFrameOptions,
    ) -> Result<(), Self::Err> {
        let mut bytes = Vec::new();
        bytes.extend(size.width.to_le_bytes());
        bytes.extend(size.height.to_le_bytes());
        for p in pixels {
            bytes.extend([p.r, p.g, p.b]);
        }
        self.video = hash(&bytes);
        Ok(())
    }
}
impl AudioOutput for Sink {
    type Err = Infallible;
    fn push_sample(&mut self, l: f64, r: f64) -> Result<(), Self::Err> {
        self.audio.extend(l.to_le_bytes());
        self.audio.extend(r.to_le_bytes());
        Ok(())
    }
}
struct Save;
impl SaveWriter for Save {
    type Err = String;
    fn load_bytes(&mut self, _: &str) -> Result<Vec<u8>, String> {
        Err("empty".into())
    }
    fn persist_bytes(&mut self, _: &str, _: &[u8]) -> Result<(), String> {
        Ok(())
    }
    fn load_serialized<D: bincode::Decode<()>>(&mut self, _: &str) -> Result<D, String> {
        Err("empty".into())
    }
    fn persist_serialized<E: bincode::Encode>(&mut self, _: &str, _: E) -> Result<(), String> {
        Ok(())
    }
}
fn synthetic() -> Vec<u8> {
    let mut rom = vec![0; 0x8000];
    let code = [
        0x78, 0x18, 0xfb, 0xc2, 0x30, 0xa2, 0xff, 0x1f, 0x9a, 0xe2, 0x20, 0xa9, 0x80, 0x8d, 0x00,
        0x42, 0xc2, 0x20, 0xee, 0x00, 0x01, 0x80, 0xfb,
    ];
    rom[..code.len()].copy_from_slice(&code);
    rom[0x50] = 0x40;
    rom[0x7fc0..0x7fd5].copy_from_slice(b"CORE COMPAT TEST     ");
    rom[0x7fd5] = 0x20;
    rom[0x7fd7] = 5;
    rom[0x7fd9] = 1;
    for vector in [0x7fea, 0x7ffa] {
        rom[vector..vector + 2].copy_from_slice(&[0x50, 0x80]);
    }
    rom[0x7ffc..0x7ffe].copy_from_slice(&[0, 0x80]);
    rom[0x7fdc..0x7fe0].copy_from_slice(&[0xff, 0xff, 0, 0]);
    rom
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let profile = &args[1];
    let output = std::path::Path::new(&args[2]);
    let rom = if args[3] == "synthetic" {
        synthetic()
    } else {
        std::fs::read(&args[3]).unwrap()
    };
    let frames: usize = args[4].parse().unwrap();
    #[allow(unused_mut)]
    let mut config = SnesEmulatorConfig::default();
    #[cfg(feature = "shared")]
    {
        config.timing_profile = if profile == "mmx" {
            snes_core::api::TimingProfile::MmxHardware
        } else {
            snes_core::api::TimingProfile::LttpLegacy
        };
    }
    let mut save = Save;
    let mut emu = SnesEmulator::create(rom, config, CoprocessorRoms::default(), &mut save).unwrap();
    emu.wram_mut().fill(0);
    if args.len() > 5 {
        let bytes = std::fs::read(&args[5]).unwrap();
        #[cfg(all(feature = "shared", feature = "mmx"))]
        let (state, n) = bincode::decode_from_slice::<snes_core::api::LegacyMmxSaveState, _>(
            &bytes,
            bincode::config::standard(),
        )
        .unwrap();
        #[cfg(not(all(feature = "shared", feature = "mmx")))]
        let (state, n) =
            bincode::decode_from_slice::<SnesEmulator, _>(&bytes, bincode::config::standard())
                .unwrap();
        assert_eq!(n, bytes.len());
        #[cfg(all(feature = "shared", feature = "mmx"))]
        let state = state.into();
        emu.load_state(state);
    }
    let mut video = Sink::default();
    let mut audio = Sink::default();
    let mut trace = String::new();
    for frame in 0..frames {
        let mut inputs = SnesInputs::default();
        inputs.p1.set_field(SnesButton::Start, frame % 60 < 4);
        loop {
            if emu
                .tick(
                    &mut video,
                    &mut audio,
                    &mut ConstantInputPoller(&inputs),
                    &mut save,
                )
                .unwrap()
                == TickEffect::FrameRendered
            {
                break;
            }
        }
        trace.push_str(&format!(
            "{frame} {:016x} {:016x} {:016x}\n",
            hash(emu.wram()),
            video.video,
            hash(&audio.audio)
        ));
        audio.audio.clear();
    }
    #[cfg(all(feature = "shared", feature = "mmx"))]
    let state = emu.to_legacy_mmx_save_state();
    #[cfg(not(all(feature = "shared", feature = "mmx")))]
    let state = emu.to_save_state();
    let bytes = bincode::encode_to_vec(state, bincode::config::standard()).unwrap();
    std::fs::write(output.with_extension("state"), bytes).unwrap();
    std::fs::write(output.with_extension("trace"), trace).unwrap();
    println!("{profile}: {frames} frames -> {}", output.display());
}
