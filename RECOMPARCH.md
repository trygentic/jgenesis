# Shared jgenesis SNES core

These nine Rust crates form one shared implementation consumed by MMX and LTTP. They are
GPL-3.0-only; LICENSE contains the full license. Original upstream authorship and fork
patch comments are retained. This is a source dependency, not yet an installable shared
runtime or a libretro core. The launcher package does not link these crates.

## Provenance

Both game forks originated at jsgroth/jgenesis commit
`cc10b2bdd32deb51f1f7a15efa18bae2ba20a41f` (0.14.1).
RecompArch reconciliation date: 2026-10-08. This import reconciles:

- ryan-z/mmxrs-private main `ca5e0d0681b1b428e35785dfcf4a96c36dfdc723`, vendor/jgenesis.
- troyedwardsjr/lttprs main `5dfa739ed2c95108983b115e6a7d76ba03a2e4d0`, emu/.

Changes include the union of MMX isolated execution/overclocking, P2 graphics, widescreen
and timing fixes with LTTP routine traps, interrupt tracking, cycle stalls and OBJ hooks.
Consumers pin this repository by immutable Git revision; all core packages must use the
same revision. Transitive sibling paths resolve within this repository.

## Compatibility contracts

- `TimingProfile::MmxHardware` is the default, preserving MMX's startup, NMI/IRQ, DMA/APU
  synchronization and audio clock behavior.
- `TimingProfile::LttpLegacy` preserves LTTP's previous clocks and interrupt edges. Select
  it at creation. The timing profile belongs to the live instance: preference reload and
  state load preserve it; choose another profile by creating another emulator.
- Generic SnesEmulator serialization retains LTTP's serialized CPU interrupt depth and
  TrapSet layout. Host-only MMX rendering preferences remain outside the state.
- `LegacyMmxSaveState` / `to_legacy_mmx_save_state()` preserve MMX's original bincode wire
  layout, without LTTP's added CPU depth/TrapSet fields. Convert a decoded legacy state
  with `.into()` before `load_state`. This is a data codec, not a second emulator.
- `cgram()` retains MMX's owned-array API; `cgram_ref()` provides LTTP's borrowed view.
- No game routine replacements, ROM maps, mods, launchers or game saves move into the core.

Power-on WRAM is deterministic and zero-filled, as required by MMX netplay. LTTP already
zeroes WRAM in its wrapper. Direct core consumers should choose their own initialization
policy if they depend on unspecified power-on contents.

## Verification

Run `cargo test -p snes-core -p snes-coprocessors -p snes-config -p wdc65816-emu -p spc700-emu -p arm7tdmi-emu -p dsp -p jgenesis-common -p jgenesis-proc-macros`. Consumer integration tests exercise
traps, isolated execution, graphics, save/load and multiplayer behavior. For differential
checks against the original forks, including old/new state cross-loading, run:

```sh
python3 tools/core-compat/compare.py --mmx /path/to/mmxrs-private --lttp /path/to/lttprs
```

The original checkouts must retain their old vendored cores. Optional `--mmx-rom` and
`--lttp-rom` accept user-owned local ROM paths; `--frames 1500` exercises boot/title input.
The harness compares every frame's WRAM, video and exact f64 audio bytes, final serialized
state bytes, and 30 frames of continuation after decoding the other implementation's state.
Synthetic inputs require no commercial data. Temporary states/ROM-derived traces are
created outside the repository and removed afterward; ROMs are never copied or committed.


This private source fork preserves upstream history and directory structure. RecompArch remains a separate launcher. Only the nine shared SNES dependency crates are validated for this reconciliation; other upstream engines/frontends are retained but are not part of this migration.
