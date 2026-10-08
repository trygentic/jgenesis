#!/usr/bin/env python3
"""Compare frame/RAM/audio traces and state codecs against both original forks.
No ROMs or generated states enter the repository. Requires the original source checkouts.
"""
import argparse, os, pathlib, subprocess, tempfile
p=argparse.ArgumentParser()
p.add_argument('--mmx',type=pathlib.Path,required=True,help='Original MMX repository')
p.add_argument('--lttp',type=pathlib.Path,required=True,help='Original LTTP repository')
p.add_argument('--mmx-rom',type=pathlib.Path)
p.add_argument('--lttp-rom',type=pathlib.Path)
p.add_argument('--frames',type=int,default=300)
a=p.parse_args()
root=pathlib.Path(__file__).resolve().parents[2]
locations={'snes-core':'backend/snes-core','snes-config':'config/snes-config','jgenesis-common':'common/jgenesis-common'}
source=pathlib.Path(__file__).with_name('harness.rs').read_text()
with tempfile.TemporaryDirectory(prefix='recomparch-compat-') as tmp:
 tmp=pathlib.Path(tmp)
 env=os.environ.copy();env['CARGO_TARGET_DIR']=str(root/'target/compat');env['CARGO_BUILD_BUILD_DIR']=str(root/'target/compat-build')
 for profile,base,rom in [('mmx',a.mmx/'vendor/jgenesis',a.mmx_rom),('lttp',a.lttp/'emu',a.lttp_rom)]:
  builds={}
  for name,core in [('old',base),('shared',root)]:
   folder=tmp/f'{profile}-{name}';folder.mkdir();(folder/'src').mkdir();(folder/'src/main.rs').write_text(source)
   deps='\n'.join(f'{crate} = {{ path = "{core / (locations[crate] if name == "shared" else crate)}" }}' for crate in ['snes-core','snes-config','jgenesis-common'])
   (folder/'Cargo.toml').write_text(f'[package]\nname="compat-{profile}-{name}"\nversion="0.1.0"\nedition="2024"\n[features]\nshared=[]\nmmx=[]\n[dependencies]\nbincode="2"\n{deps}\n')
   features=['--features',','.join((["shared"] if name=='shared' else [])+(["mmx"] if profile=='mmx' else []))] if name=='shared' or profile=='mmx' else []
   subprocess.run(['cargo','build','--offline','--release','--manifest-path',str(folder/'Cargo.toml'),*features],env=env,check=True)
   builds[name]=root/f'target/compat/release/compat-{profile}-{name}'
  for label,input_rom,frames in [('synthetic','synthetic',20)]+([('rom',str(rom.resolve()),a.frames)] if rom else []):
   outputs={}
   for name,exe in builds.items():
    output=tmp/f'{profile}-{label}-{name}';outputs[name]=output
    subprocess.run([str(exe),profile,str(output),input_rom,str(frames)],check=True)
   for extension in ['trace','state']:
    old=outputs['old'].with_suffix('.'+extension).read_bytes();new=outputs['shared'].with_suffix('.'+extension).read_bytes()
    if old!=new:
     i=next((i for i,(x,y) in enumerate(zip(old,new)) if x!=y),min(len(old),len(new)))
     raise SystemExit(f'{profile}/{label}: {extension} differs at byte {i}; lengths {len(old)}, {len(new)}')
   # Decode each fork's state in the other implementation and compare resumed execution.
   for name,exe in builds.items():
    other='old' if name=='shared' else 'shared'
    subprocess.run([str(exe),profile,str(tmp/f'{profile}-{label}-resumed-{name}'),input_rom,'30',str(outputs[other].with_suffix('.state'))],check=True)
   for ext in ['trace','state']:
    assert (tmp/f'{profile}-{label}-resumed-old').with_suffix('.'+ext).read_bytes()==(tmp/f'{profile}-{label}-resumed-shared').with_suffix('.'+ext).read_bytes(),f'{profile}/{label}: resumed {ext} mismatch'
   print(f'PASS {profile}/{label}: identical RAM, video, audio, state bytes and cross-core continuation',flush=True)
