#!/usr/bin/env python3
# genfs.py: for each vertex-shader SPIR-V, write a fragment shader that consumes every VS output (so the
# driver cannot drop them), and print "vs.spv fs.spv attrs" where attrs = loc:kind,... for the vertex inputs.
import subprocess, sys, re, os
lines=[]
for vs in sys.argv[1:]:
    dis=subprocess.run(['spirv-dis','--raw-id',vs],capture_output=True,text=True).stdout
    types={}; deco={}; vars_=[]
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
        return (k+'?',1)
    ins=[]; outs=[]; ok=True
    for vid,tid,sc in vars_:
        d=deco.get(vid,{})
        if 'BuiltIn' in d or 'Location' not in d: continue
        t,n=base(tid)
        if t[1:]!='32': ok=False; break
        if sc=='Input': ins.append(f"{d['Location']}:{t[0]}{n}")
        else:
            g={'f':['float','vec2','vec3','vec4'],'i':['int','ivec2','ivec3','ivec4'],'u':['uint','uvec2','uvec3','uvec4']}[t[0]][n-1]
            flat='flat ' if ('Flat' in d or t[0]!='f') else ''
            comp=f", component={d['Component']}" if d.get('Component') else ''
            outs.append((f"layout(location={d['Location']}{comp}) {flat}in {g} i{vid};", f"i{vid}", t[0], n))
    if not ok: print(os.path.basename(vs),'SKIP',file=sys.stderr); continue
    sumexpr=" + ".join(f"vec4({'float' if n==1 else 'vec'+str(n)}({v}){', 0.0'*(4-n)})" for _,v,t,n in outs) or "vec4(0.0)"
    glsl="#version 450\n"+"\n".join(o[0] for o in outs)+"\nlayout(location=0) out vec4 c;\nvoid main() { c = "+sumexpr+"; }\n"
    h=os.path.basename(vs)[:-4]; src=f"fsv/{h}.frag"; spv=f"fsv/{h}.fs.spv"
    open(src,'w').write(glsl)
    r=subprocess.run(['glslangValidator','-V','--target-env','vulkan1.3','-o',spv,src],capture_output=True,text=True)
    if r.returncode: print(h,'FSFAIL',r.stdout[:200],file=sys.stderr); continue
    lines.append(f"{vs} {spv} {','.join(ins) or '-'}")
print("\n".join(lines))
