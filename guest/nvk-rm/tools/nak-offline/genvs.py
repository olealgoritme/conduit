#!/usr/bin/env python3
# genvs.py: for each fragment-shader SPIR-V, write a minimal GLSL vertex shader whose outputs match the
# fragment shader's inputs (location/component/type/flat), compile it with glslangValidator, and print a
# line "fs.spv vs.spv <color-output kinds> <depth>" for nvstats' graphics mode.
import subprocess, sys, re, os
out_lines=[]
for fs in sys.argv[1:]:
    dis=subprocess.run(['spirv-dis','--raw-id',fs],capture_output=True,text=True).stdout
    if 'OpEntryPoint Fragment' not in dis: continue
    types={}; ptr={}; deco={}; vars_=[]
    for l in dis.splitlines():
        m=re.match(r'\s*%(\d+) = OpType(\w+)(.*)',l)
        if m: types[m.group(1)]=(m.group(2),m.group(3).split()); continue
        m=re.match(r'\s*OpDecorate %(\d+) (\w+)\s*(\d*)',l)
        if m: deco.setdefault(m.group(1),{})[m.group(2)]=m.group(3); continue
        m=re.match(r'\s*%(\d+) = OpVariable %(\d+) (Input|Output)',l)
        if m: vars_.append((m.group(1),m.group(2),m.group(3)))
    def base(tid):
        k,a=types[tid]
        if k=='Pointer': return base(a[1].lstrip('%'))
        if k=='Vector': b=base(a[0].lstrip('%')); return (b[0],int(a[1]))
        if k=='Float': return ('f'+a[0],1)
        if k=='Int': return (('i' if a[1]=='1' else 'u')+a[0],1)
        if k=='Array': b=base(a[0].lstrip('%')); return (b[0]+'[]',b[1])
        return (k,1)
    vs_out=[]; colors={}; depth=0; ok=True
    for vid,tid,sc in vars_:
        d=deco.get(vid,{})
        if 'BuiltIn' in d:
            continue
        if 'Location' not in d: continue
        t,n=base(tid)
        if sc=='Input':
            if '[]' in t or t[1:]!='32': ok=False; break
            g={'f':['float','vec2','vec3','vec4'],'i':['int','ivec2','ivec3','ivec4'],'u':['uint','uvec2','uvec3','uvec4']}[t[0]][n-1]
            flat='flat ' if ('Flat' in d or t[0]!='f') else ''
            comp=f", component={d['Component']}" if d.get('Component') else ''
            vs_out.append(f"layout(location={d['Location']}{comp}) {flat}out {g} o{vid};")
        else:
            colors[int(d['Location'])]=t[0]
    for l in dis.splitlines():
        if re.search(r'OpDecorate %\d+ BuiltIn FragDepth',l): depth=1
    if not ok: print(f"{os.path.basename(fs)} SKIP(interface)", file=sys.stderr); continue
    body="\n".join(vs_out)
    assigns="\n".join(f"  o{x.split(' o')[-1].rstrip(';')} = {x.split('out ')[1].split()[0]}(0);" for x in vs_out)
    glsl=f"#version 450\n{body}\nvoid main() {{\n  gl_Position = vec4(0.0);\n{assigns}\n}}\n"
    h=os.path.basename(fs)[:-4]; src=f"vs/{h}.vert"; spv=f"vs/{h}.vs.spv"
    open(src,'w').write(glsl)
    r=subprocess.run(['glslangValidator','-V','--target-env','vulkan1.3','-o',spv,src],capture_output=True,text=True)
    if r.returncode: print(f"{h} VSFAIL {r.stdout.strip()[:200]}",file=sys.stderr); continue
    kinds=''.join(colors.get(i,'-') for i in range(max(colors)+1)) if colors else '-'
    out_lines.append(f"{fs} {spv} {kinds} {depth}")
print("\n".join(out_lines))
