#!/usr/bin/env python3
"""nvddi code generator.

Reads the WDK display headers (dispmprt.h, d3dkmddi.h, d3dkmdt.h, d3dukmdt.h)
and produces everything that depends on DDI signatures and structure layouts:

  OUT/wdkinc/*.h       the WDK headers, patched for GCC (enum constants >= 2^31
                       get an (int) cast so the enums stay 4 bytes like MSVC)
  OUT/nvddi_gen.c      one wrapper per function-pointer slot of every wrapped
                       table, slot/argument descriptors, capture rules, schema
  OUT/nvddi_gen.h      table ids and counts
  OUT/ndl_layouts.json struct layouts, enums and typedefs for decoder/ndldump.py

Type facts come from two compilers so nothing is hand-copied:
  clang (MSVC ABI, unpatched headers) dumps record layouts and the typedef AST;
  mingw gcc (the compiler that builds the driver, patched headers) answers
  probe programs (sizeof, __builtin_classify_type, __builtin_types_compatible_p)
  whose results are read back from the object file without running anything.
gen/layoutcheck.py compares the two compilers' offsets for every captured field.

Usage: gen.py --wdk DIR --out DIR [--cc x86_64-w64-mingw32-gcc] [--clang clang]
"""

import argparse
import json
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import rules as R  # noqa: E402  (capture rule configuration)

WDK_HEADERS = ["dispmprt.h", "d3dkmddi.h", "d3dkmdt.h", "d3dukmdt.h"]
MINGW = "/usr/x86_64-w64-mingw32/include"

SHIM = """\
/* target Windows 10+ kernel APIs in the mingw-w64 ddk headers */
#ifndef _WIN32_WINNT
#define _WIN32_WINNT 0x0A00
#endif
#ifndef NTDDI_VERSION
#define NTDDI_VERSION 0x0A00000C
#endif
/* SAL annotations the mingw-w64 headers do not define */
#ifndef _Maybenull_
#define _Maybenull_
#endif
#ifndef _Pre_opt_bytecap_
#define _Pre_opt_bytecap_(x)
#endif
#ifndef static_assert
#define static_assert _Static_assert
#endif
"""

PRELUDE = """\
#include "wdkshim.h"
#include <ntddk.h>
#include <dispmprt.h>
#include <d3dkmddi.h>
"""

# Wrapped function-pointer tables.  name: (C typedef, tag, id, how the copy is sized)
TABLES = [
    ("DRIVER_INITIALIZATION_DATA", "_DRIVER_INITIALIZATION_DATA", 0, "ddi"),
    ("DXGKRNL_INTERFACE", "_DXGKRNL_INTERFACE", 1, "sizefield"),
    ("DXGK_VIDPN_INTERFACE", "_DXGK_VIDPN_INTERFACE", 2, "static"),
    ("DXGK_VIDPNTOPOLOGY_INTERFACE", "_DXGK_VIDPNTOPOLOGY_INTERFACE", 3, "static"),
    ("DXGK_VIDPNSOURCEMODESET_INTERFACE", "_DXGK_VIDPNSOURCEMODESET_INTERFACE", 4, "static"),
    ("DXGK_VIDPNTARGETMODESET_INTERFACE", "_DXGK_VIDPNTARGETMODESET_INTERFACE", 5, "static"),
    ("DXGK_MONITOR_INTERFACE_V2", "_DXGK_MONITOR_INTERFACE_V2", 6, "static"),
    ("DXGK_MONITORSOURCEMODESET_INTERFACE", "_DXGK_MONITORSOURCEMODESET_INTERFACE", 7, "static"),
    ("DXGK_MONITORFREQUENCYRANGESET_INTERFACE", "_DXGK_MONITORFREQUENCYRANGESET_INTERFACE", 8, "static"),
    ("DXGK_MONITORDESCRIPTORSET_INTERFACE", "_DXGK_MONITORDESCRIPTORSET_INTERFACE", 9, "static"),
]
# A table type that appears behind an out-parameter of a different (smaller,
# prefix-compatible) type is wrapped as the superset type.
TABLE_ALIAS = {"DXGK_MONITOR_INTERFACE": "DXGK_MONITOR_INTERFACE_V2"}
# Highest table "Version" field value the superset type is known to cover.
TABLE_MAXVER = {"DXGK_VIDPN_INTERFACE": 2, "DXGK_MONITOR_INTERFACE_V2": 2}

# __builtin_classify_type results (GCC)
CL_VOID, CL_INT, CL_CHAR, CL_ENUM, CL_BOOL, CL_PTR = 0, 1, 2, 3, 4, 5
CL_REAL, CL_FUNC, CL_RECORD, CL_UNION, CL_ARRAY = 8, 10, 12, 13, 14
SCALAR_CLASSES = (CL_INT, CL_CHAR, CL_ENUM, CL_BOOL, CL_PTR)

AK = {"VAL": 0, "PREC": 1, "PSCALAR": 2, "PVOID": 3, "PPREC": 4, "BYVAL": 5, "PPTABLE": 6, "PTABLE": 7}
AF_CONST, AF_BUF, AF_POSTRULES, AF_SIZE16 = 1, 2, 4, 8

RK = {"bytes": 1, "count": 2, "one": 3, "ptrarray": 4, "written": 5, "range": 6, "mdl": 7,
      "adl": 8, "product": 9}
W_PRE, W_POST = 1, 2


def run(cmd, **kw):
    r = subprocess.run(cmd, capture_output=True, text=True, **kw)
    if r.returncode != 0:
        sys.stderr.write(r.stdout[-4000:] + r.stderr[-8000:])
        raise SystemExit("command failed: %s" % " ".join(cmd[:6]))
    return r.stdout


# ---------------------------------------------------------------- headers

def prep_headers(wdk, out):
    raw = os.path.join(out, "wdkraw")
    pat = os.path.join(out, "wdkinc")
    os.makedirs(raw, exist_ok=True)
    os.makedirs(pat, exist_ok=True)
    enum_re = re.compile(r"^(\s*[A-Za-z_0-9]+\s*=\s*)(0x[89A-Fa-f][0-9A-Fa-f]{7})\b", re.M)
    for h in WDK_HEADERS:
        src = open(os.path.join(wdk, h), encoding="latin-1").read()
        open(os.path.join(raw, h), "w", encoding="latin-1").write(src)
        open(os.path.join(pat, h), "w", encoding="latin-1").write(enum_re.sub(r"\1(int)\2", src))
    for d in (raw, pat):
        open(os.path.join(d, "wdkshim.h"), "w").write(SHIM)
    stub = os.path.join(out, "clangstub")
    os.makedirs(stub, exist_ok=True)
    open(os.path.join(stub, "intrin.h"), "w").write("/* clang layout pass: no intrinsics needed */\n")
    return raw, pat, stub


def clang_cmd(clang, raw, stub):
    return [clang, "--target=x86_64-pc-windows-msvc", "-D__GNUC__=4", "-fms-extensions",
            "-Wno-everything", "-I" + stub, "-I" + raw, "-I" + MINGW + "/ddk", "-I" + MINGW]


def gcc_cmd(cc, pat):
    return [cc, "-fms-extensions", "-w", "-I" + pat, "-I" + MINGW + "/ddk"]


# ---------------------------------------------------------------- clang dumps

def parse_layouts(text):
    """Record layouts from clang -fdump-record-layouts-complete."""
    recs = {}
    for blk in text.split("*** Dumping AST Record Layout")[1:]:
        lines = [l for l in blk.split("\n") if "|" in l]
        if not lines:
            continue
        m = re.match(r"\s*0 \| (struct|union) (.+?)\s*$", lines[0])
        if not m:
            continue
        kw, tag = m.group(1), m.group(2)
        size = None
        fields = []
        for l in lines[1:]:
            left, right = l.split("|", 1)
            left = left.strip()
            ms = re.search(r"\[sizeof=(\d+)", right)
            if ms and not left:
                size = int(ms.group(1))
                break
            if not left:
                continue
            mo = re.match(r"(\d+)(?::(\d+)-(\d+))?$", left)
            if not mo:
                continue
            body = right.rstrip()
            indent = len(body) - len(body.lstrip(" "))
            depth = (indent - 1) // 2
            body = body.strip()
            off = int(mo.group(1))
            bits = (int(mo.group(2)), int(mo.group(3))) if mo.group(2) is not None else None
            # "type name" or "union X::(anonymous at ...)" (anonymous member)
            anon = re.match(r"^(struct|union) .*\((anonymous|unnamed) (struct |union )?at [^)]*\)\s*$", body)
            if anon and "(anonymous" in body:
                fields.append({"off": off, "depth": depth, "type": body, "name": None, "bits": bits,
                               "kind": anon.group(1)})
                continue
            mm = re.match(r"^(.*?)\s*([A-Za-z_][A-Za-z_0-9]*)$", body)
            if not mm:
                continue
            fields.append({"off": off, "depth": depth, "type": mm.group(1).strip(), "name": mm.group(2),
                           "bits": bits, "kind": None})
        if size is not None and tag not in recs:
            recs[tag] = {"kw": kw, "size": size, "fields": fields}
    return recs


def ast_objects(text):
    dec = json.JSONDecoder()
    i, n = 0, len(text)
    while i < n:
        while i < n and text[i] in " \r\n\t":
            i += 1
        if i >= n:
            break
        if text.startswith("Dumping", i):
            i = text.index("\n", i) + 1
            continue
        o, i = dec.raw_decode(text, i)
        yield o


def load_ast(cmd, src, filters):
    typedefs, enums, enum_typedef = {}, {}, {}
    for f in filters:
        out = run(cmd + ["-fsyntax-only", "-Xclang", "-ast-dump=json", "-Xclang", "-ast-dump-filter=" + f, src])
        for o in ast_objects(out):
            k = o.get("kind")
            if k == "TypedefDecl" and "name" in o:
                typedefs[o["name"]] = o["type"]["qualType"]
            elif k == "EnumDecl" and o.get("name"):
                vals, nxt = {}, 0
                for c in o.get("inner", []):
                    if c.get("kind") != "EnumConstantDecl":
                        continue
                    v = None
                    for e in c.get("inner", []):
                        if "value" in e:
                            v = int(e["value"])
                    if v is None:
                        v = nxt
                    vals[c["name"]] = v
                    nxt = v + 1
                enums[o["name"]] = vals
    for name, q in typedefs.items():
        m = re.match(r"^enum (\w+)$", q)
        if m:
            enum_typedef[name] = m.group(1)
    return typedefs, enums, enum_typedef


# ---------------------------------------------------------------- signatures

def split_top(s, sep=","):
    out, depth, cur = [], 0, ""
    for ch in s:
        if ch in "([":
            depth += 1
        elif ch in ")]":
            depth -= 1
        if ch == sep and depth == 0:
            out.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur)
    return [x.strip() for x in out]


def strip_attrs(q):
    return re.sub(r"\s*__attribute__\(\([^()]*(\([^()]*\))?[^()]*\)\)", "", q).strip()


def parse_fn_type(q):
    """'RET (P1, P2)' or 'RET (*)(P1, P2)' -> (ret, [params]) or None."""
    q = strip_attrs(q)
    if not q.endswith(")"):
        return None
    depth, i = 0, len(q) - 1
    while i >= 0:
        if q[i] == ")":
            depth += 1
        elif q[i] == "(":
            depth -= 1
            if depth == 0:
                break
        i -= 1
    params = q[i + 1:-1]
    head = q[:i].strip()
    if head.endswith("(*)"):
        head = head[:-3].strip()
    elif head.endswith(")"):
        return None
    ps = split_top(params)
    if ps == ["void"]:
        ps = []
    return head, ps


def resolve_fn(typedefs, tname, depth=0):
    """Field type name -> (ret, params, fn_typedef_name) following typedefs."""
    if depth > 6:
        return None
    t = tname.strip()
    t = re.sub(r"^const\s+", "", t)
    q = typedefs.get(t)
    if q is None:
        r = parse_fn_type(t)
        return (r[0], r[1], None) if r else None
    r = parse_fn_type(q)
    if r:
        return r[0], r[1], t
    m = re.match(r"^(\w+)\s*\*$", strip_attrs(q))
    if m:
        r = resolve_fn(typedefs, m.group(1), depth + 1)
        if r:
            return r[0], r[1], r[2] or m.group(1)
    return None


def header_param_info(htext, fname):
    """Parameter names and IRQL annotation for typedef fname from the raw header text."""
    m = None
    for mm in re.finditer(r"(\(\s*(?:APIENTRY\s+)?(?:CALLBACK\s+)?\*\s*%s\s*\)|\b%s)\s*\(" % (fname, fname), htext):
        start = htext.rfind("typedef", 0, mm.start())
        if start >= 0 and ";" not in htext[start:mm.start()]:
            m = mm
            break
    if not m:
        return None, None
    i = m.end()
    depth, j = 1, i
    while j < len(htext) and depth:
        if htext[j] == "(":
            depth += 1
        elif htext[j] == ")":
            depth -= 1
        j += 1
    params = htext[i:j - 1]
    names = []
    for p in split_top(params):
        toks = re.findall(r"[A-Za-z_][A-Za-z_0-9]*", re.sub(r"\[[^\]]*\]", "", p))
        names.append(toks[-1] if toks else None)
    if names in (["VOID"], ["void"]):
        names = []
    pre = htext[htext.rfind("typedef", 0, m.start()):m.start()]
    irql = None
    mi = re.search(r"_IRQL_requires_(max_|min_|DXGK_)?\(([A-Z_]+)\)", pre)
    if mi:
        irql = ("<=" if mi.group(1) == "max_" else ">=" if mi.group(1) == "min_" else "") + mi.group(2)
    return names, irql


def strip_comments(t):
    t = re.sub(r"/\*.*?\*/", " ", t, flags=re.S)
    return re.sub(r"//[^\n]*", "", t)


# ---------------------------------------------------------------- gcc probes

def probe(cc_cmd, out, name, exprs):
    """Evaluate integer constant expressions with gcc; values read from the object."""
    if not exprs:
        return []
    src = os.path.join(out, name + ".c")
    obj = os.path.join(out, name + ".o")
    with open(src, "w") as f:
        f.write(PRELUDE)
        f.write("#pragma GCC diagnostic ignored \"-Wpointer-arith\"\n")
        f.write("const long long nvddi_probe[%d] = {\n" % len(exprs))
        for e in exprs:
            f.write("  (long long)(%s),\n" % e)
        f.write("};\n")
    run(cc_cmd + ["-c", src, "-o", obj])
    dump = run(["x86_64-w64-mingw32-objdump", "-s", "-j", ".rdata", obj])
    data = bytearray()
    for line in dump.split("\n"):
        m = re.match(r"^ ([0-9a-f]{4,}) ((?:[0-9a-f]{2,8} ){1,4})", line)
        if m:
            for w in m.group(2).split():
                data += bytes.fromhex(w)
    vals = []
    for i in range(len(exprs)):
        vals.append(int.from_bytes(data[i * 8:i * 8 + 8], "little", signed=True))
    if len(data) < len(exprs) * 8:
        raise SystemExit("probe %s: short read" % name)
    return vals


# ---------------------------------------------------------------- type names

TYPE_PREFIXES = ("IN_", "OUT_", "INOUT_", "CONST_", "DEREF_", "OPT_", "ECOUNT_")


def desugar(typedefs, t, depth=0):
    """Type text -> (base name, pointer depth, const-at-pointee) best effort."""
    t = strip_attrs(t).strip()
    const = False
    stars = 0
    while True:
        t = t.strip()
        if t.endswith("*"):
            stars += 1
            t = t[:-1].strip()
            if t.endswith("const"):
                t = t[:-5].strip()
            continue
        break
    if re.match(r"^const\b", t):
        const = stars > 0 or const
        t = t[5:].strip()
    t = re.sub(r"^(struct|union|enum)\s+", "", t)
    if depth < 8 and t in typedefs:
        q = typedefs[t]
        if parse_fn_type(q):
            return t, stars, const
        b, s2, c2 = desugar(typedefs, q, depth + 1)
        if s2 == 0:
            return b, stars, const or c2
        return b, stars + s2, (c2 if stars == 0 else const) or (c2 and stars == 0)
    return t, stars, const


# ---------------------------------------------------------------- main generation

class Gen:
    def __init__(self, a):
        self.a = a
        self.out = a.out
        os.makedirs(self.out, exist_ok=True)
        self.raw, self.pat, self.stub = prep_headers(a.wdk, self.out)
        self.ccmd = clang_cmd(a.clang, self.raw, self.stub)
        self.gcmd = gcc_cmd(a.cc, self.pat)
        src = os.path.join(self.out, "all.c")
        open(src, "w").write(PRELUDE + "int nvddi_all;\n")
        self.allc = src
        lay = run(self.ccmd + ["-fsyntax-only", "-Xclang", "-fdump-record-layouts-complete", src])
        self.recs = parse_layouts(lay)
        self.typedefs, self.enums, self.enum_typedef = load_ast(
            self.ccmd, src, ["DXGK", "D3D", "IN_", "OUT_", "DEREF_", "OPT_", "DRIVER_INIT", "PDEVICE",
                             "DEVICE_OBJECT", "MDL", "PHYSICAL", "LARGE_INTEGER", "KSYNCHRONIZE",
                             "INTERFACE", "VIDEO", "DEVICE_POWER", "POWER_ACTION", "ACPI"])
        self.htext = strip_comments("\n".join(
            open(os.path.join(self.raw, h), encoding="latin-1").read() for h in WDK_HEADERS))

    # record lookup by typedef or tag
    def rec_tag(self, name):
        if name in self.recs:
            return name
        q = self.typedefs.get(name)
        if q:
            m = re.match(r"^(?:struct|union)\s+(\w+)$", q.strip())
            if m and m.group(1) in self.recs:
                return m.group(1)
            if re.match(r"^\w+$", q.strip()):
                return self.rec_tag(q.strip())
        if "_" + name in self.recs:
            return "_" + name
        return None

    def run_all(self):
        self.build_tables()
        self.classify_args()
        self.build_rules()
        self.emit_c()
        self.emit_layouts()

    # -------------------------------------------------------------- tables
    def build_tables(self):
        self.tables = []
        for (cname, tag, tid, how) in TABLES:
            rec = self.recs.get(tag)
            if rec is None:
                raise SystemExit("table %s not in layouts" % tag)
            slots = []
            for f in rec["fields"]:
                if f["depth"] != 1 or f["name"] is None or f["bits"]:
                    continue
                fn = resolve_fn(self.typedefs, f["type"])
                if not fn:
                    continue
                ret, params, fname = fn
                names, irql = (None, None)
                if fname:
                    names, irql = header_param_info(self.htext, fname)
                if not names or len(names) != len(params):
                    names = ["a%d" % i for i in range(len(params))]
                slots.append({"field": f["name"], "off": f["off"], "ftype": f["type"], "ret": ret,
                              "params": params, "pnames": names, "irql": irql or "", "fname": fname or ""})
            for i, s in enumerate(slots):
                s["index"] = i
            self.tables.append({"cname": cname, "tag": tag, "id": tid, "how": how, "slots": slots,
                                "size": rec["size"]})
        self.table_by_cname = {t["cname"]: t for t in self.tables}

    # -------------------------------------------------------------- argument classification
    def classify_args(self):
        types = []
        for t in self.tables:
            for s in t["slots"]:
                for p in s["params"]:
                    if p not in types:
                        types.append(p)
                if s["ret"] not in types and s["ret"] not in ("void", "VOID"):
                    types.append(s["ret"])
        ex = []
        for ty in types:
            ex.append("__builtin_classify_type(*(__typeof__(%s)*)0)" % ty)
            ex.append("sizeof(__typeof__(%s))" % ty)
        v = probe(self.gcmd, self.out, "probe1", ex)
        self.tinfo = {}
        for i, ty in enumerate(types):
            self.tinfo[ty] = {"cls": v[2 * i], "size": v[2 * i + 1]}
        # pointer types: pointee facts
        ptrs = [ty for ty in types if self.tinfo[ty]["cls"] == CL_PTR]
        ex = ["__builtin_types_compatible_p(__typeof__(*(*(__typeof__(%s)*)0)), void)" % ty for ty in ptrs]
        v = probe(self.gcmd, self.out, "probe2", ex)
        for ty, isvoid in zip(ptrs, v):
            ti = self.tinfo[ty]
            ti["pvoid"], ti["pcls"], ti["psize"] = isvoid, CL_VOID, 0
        nonvoid = [ty for ty in ptrs if not self.tinfo[ty]["pvoid"]]
        ex = []
        for ty in nonvoid:
            d = "(*(*(__typeof__(%s)*)0))" % ty
            ex.append("__builtin_classify_type(%s)" % d)
            ex.append("sizeof(%s)" % d)
        v = probe(self.gcmd, self.out, "probe2b", ex)
        for i, ty in enumerate(nonvoid):
            self.tinfo[ty]["pcls"], self.tinfo[ty]["psize"] = v[2 * i], v[2 * i + 1]
        # candidate record names for pointees, verified with types_compatible
        cand_ex, cand = [], []
        for ty in ptrs:
            ti = self.tinfo[ty]
            base, stars, const = desugar(self.typedefs, ty)
            ti["const"] = const or bool(re.search(r"\bconst\b|CONST", ty))
            ti["base"], ti["stars"] = base, stars
            if ti["pcls"] in (CL_RECORD, CL_UNION) and stars == 1:
                tag = self.rec_tag(base)
                if tag:
                    cand.append((ty, "rec", tag, base))
                    cand_ex.append("__builtin_types_compatible_p(__typeof__(*(*(__typeof__(%s)*)0)), %s %s)"
                                   % (ty, self.recs[tag]["kw"], tag))
            elif ti["pcls"] == CL_PTR and stars == 2:
                tag = self.rec_tag(base)
                if tag:
                    cand.append((ty, "rec2", tag, base))
                    cand_ex.append("__builtin_types_compatible_p(__typeof__(**(*(__typeof__(%s)*)0)), %s %s)"
                                   % (ty, self.recs[tag]["kw"], tag))
        v = probe(self.gcmd, self.out, "probe3", cand_ex)
        for (ty, what, tag, base), ok in zip(cand, v):
            if ok:
                self.tinfo[ty][what] = tag
                self.tinfo[ty][what + "_name"] = base
        # final per-slot argument descriptors
        tag2table = {t["tag"]: t for t in self.tables}
        for alias, real in TABLE_ALIAS.items():
            tg = self.rec_tag(alias)
            if tg:
                tag2table[tg] = self.table_by_cname[real]
        self.tag2table = tag2table
        for t in self.tables:
            for s in t["slots"]:
                args = []
                for i, p in enumerate(s["params"]):
                    ti = self.tinfo[p]
                    a = {"type": p, "name": s["pnames"][i], "kind": "VAL", "flags": 0, "size": ti["size"],
                         "rec": None, "table": None, "lenarg": 0xFF}
                    if ti["cls"] == CL_PTR:
                        if ti.get("const"):
                            a["flags"] |= AF_CONST
                        if ti.get("rec") and ti["rec"] in R.NEVER_FOLLOW and ti["rec"] not in tag2table:
                            a["kind"] = "VAL"
                            a["size"] = 8
                        elif ti.get("rec"):
                            a["rec"] = ti["rec"]
                            a["size"] = self.recs[ti["rec"]]["size"]
                            a["kind"] = "PREC"
                            if ti["rec"] in tag2table and tag2table[ti["rec"]]["id"] != 0:
                                a["kind"] = "PTABLE"
                                a["table"] = tag2table[ti["rec"]]["id"]
                        elif ti.get("rec2"):
                            a["rec"] = ti["rec2"]
                            a["size"] = self.recs[ti["rec2"]]["size"]
                            a["kind"] = "PPREC"
                            if ti["rec2"] in tag2table:
                                a["kind"] = "PPTABLE"
                                a["table"] = tag2table[ti["rec2"]]["id"]
                        elif ti["pvoid"]:
                            a["kind"] = "PVOID"
                            a["size"] = 0
                        elif ti["pcls"] in SCALAR_CLASSES and ti["psize"] <= 64:
                            a["kind"] = "PSCALAR"
                            a["size"] = ti["psize"]
                        else:
                            a["kind"] = "VAL"
                            a["size"] = 8
                    elif ti["cls"] not in SCALAR_CLASSES:
                        a["kind"] = "BYVAL"
                    args.append(a)
                # buffer/length pairs among parameters
                for i, a in enumerate(args):
                    if a["kind"] not in ("PVOID", "PSCALAR", "PREC"):
                        continue
                    n = a["name"] or ""
                    want = R.length_param_names(s["fname"], n)
                    for j, b in enumerate(args):
                        if j != i and b["kind"] == "VAL" and b["name"] in want:
                            a["lenarg"] = j
                            a["flags"] |= AF_BUF
                            break
                s["args"] = args
                rt = s["ret"]
                if rt in ("void", "VOID"):
                    s["retkind"], s["retsize"] = 0, 0
                else:
                    ti = self.tinfo[rt]
                    s["retkind"] = 1 if ti["cls"] in SCALAR_CLASSES else 2
                    s["retsize"] = ti["size"]

    # -------------------------------------------------------------- capture rules
    def field_path_type(self, tag, path):
        """Find field by dotted path (anonymous members transparent)."""
        rec = self.recs[tag]
        parts = path.split(".")
        stack = []
        for f in rec["fields"]:
            d = f["depth"]
            while stack and stack[-1][0] >= d:
                stack.pop()
            stack.append((d, f["name"]))
            names = [n for (_, n) in stack if n]
            if names == parts and f["name"] == parts[-1]:
                return f
        return None

    def build_rules(self):
        self.rulesets = []    # [{"tag", "name", "rules": [...]}]
        self.rs_index = {}
        self.rules_flat = []
        for t in self.tables:
            for s in t["slots"]:
                for a in s["args"]:
                    if a["kind"] in ("PREC", "PPREC") and a["rec"]:
                        a["rs"] = self.ruleset_for(a["rec"], 0)
                    else:
                        a["rs"] = 0xFFFF
        for t in self.tables:
            for s in t["slots"]:
                for a in s["args"]:
                    if a["rec"] == "_INTERFACE":
                        a["flags"] |= AF_SIZE16
                    if a["rs"] != 0xFFFF and any(r["kind"] == "written" or r["when"] & W_POST
                                                 for r in self.rulesets[a["rs"]]["rules"]):
                        a["flags"] |= AF_POSTRULES

    def ruleset_for(self, tag, depth):
        if tag in self.rs_index:
            return self.rs_index[tag]
        name = tag[1:] if tag.startswith("_") else tag
        idx = len(self.rulesets)
        self.rs_index[tag] = idx
        rs = {"tag": tag, "name": name, "rules": [], "size": self.recs[tag]["size"]}
        self.rulesets.append(rs)
        spec = R.EXPLICIT.get(name)
        if spec is None:
            spec = self.auto_rules(tag)
        for r in spec:
            rr = self.make_rule(tag, name, r, depth)
            if rr:
                rs["rules"].append(rr)
        return idx

    def auto_rules(self, tag):
        rec = self.recs[tag]
        top = [f for f in rec["fields"] if f["depth"] == 1 and f["name"]]
        names = {f["name"]: f for f in top}
        rules = []
        for f in top:
            n = f["name"]
            if not (f["type"].endswith("*") or f["type"] in R.POINTER_TYPEDEFS):
                continue
            if f["type"].endswith("**") or "(*)" in f["type"]:
                continue
            base = n[1:] if (n.startswith("p") and len(n) > 1 and n[1].isupper()) else n
            got = None
            bt0, st0, _ = desugar(self.typedefs, f["type"])
            if base.endswith("List") and st0 == 1 and self.rec_tag(bt0) and base + "Size" in names:
                got = (n, "count", base + "Size")
            for cand in () if got else (base + "Size", base + "DataSize", "SizeOf" + base, base + "Length", base + "ByteSize",
                         base + "SizeInBytes", base + "BufferSize"):
                if cand in names and cand != n:
                    got = (n, "bytes", cand)
                    break
            if not got:
                stem = re.sub(r"(Info|List|Array|s)$", "", base)
                for cand in ("Num" + base, "Num" + base + "s", base + "Count", "Num" + stem + "s", "Num" + stem,
                             stem + "Count", base + "Size"):
                    if cand in names and cand != n:
                        got = (n, "count", cand)
                        break
            if not got:
                bt, stars, _ = desugar(self.typedefs, f["type"])
                rt = self.rec_tag(bt) if stars == 1 else None
                if rt and rt not in R.NEVER_FOLLOW:
                    got = (n, "one", None)
            if got:
                const = bool(re.search(r"\bconst\b|CONST", f["type"]))
                rules.append(got + ((W_PRE if const else W_PRE | W_POST),))
        return rules

    def make_rule(self, tag, name, r, depth):
        # r = (path, kind, lenpath, when[, extras dict])
        path, kind, lenpath, when = r[0], r[1], r[2], r[3]
        ex = r[4] if len(r) > 4 else {}
        f = self.field_path_type(tag, path)
        if f is None:
            sys.stderr.write("rule %s.%s: no such field, skipped\n" % (name, path))
            return None
        lf = self.field_path_type(tag, lenpath) if lenpath else None
        if lenpath and lf is None:
            sys.stderr.write("rule %s.%s: length field %s missing, skipped\n" % (name, path, lenpath))
            return None
        bt, stars, _ = desugar(self.typedefs, f["type"])
        elem_tag = None
        if kind == "ptrarray":
            elem_tag = self.rec_tag(bt) if stars == 2 else None
        elif kind in ("count", "one", "written") and stars == 1:
            elem_tag = self.rec_tag(bt)
        if ex.get("elem"):
            elem_tag = self.rec_tag(ex["elem"])
            if not elem_tag:
                raise SystemExit("rule %s.%s: unknown elem %s" % (name, path, ex["elem"]))
        sub = 0xFFFF
        if elem_tag and depth < 2 and elem_tag not in R.NEVER_FOLLOW and kind != "written":
            sub = self.ruleset_for(elem_tag, depth + 1)
        cond = ex.get("cond")
        if cond and len(cond) > 2:
            raise SystemExit("rule %s.%s: at most two conditions" % (name, path))
        ptype = f["type"]
        elem_size_known = (ptype.endswith("*") and not re.search(r"\bvoid\s*\*$", ptype)) or \
            (ptype in R.POINTER_TYPEDEFS and ptype not in ("PVOID", "LPVOID"))
        rid = len(self.rules_flat)
        rule = {"id": rid, "path": path, "kind": kind, "len": lenpath, "when": when, "elem": elem_tag,
                "elem_name": (elem_tag[1:] if elem_tag and elem_tag.startswith("_") else elem_tag) or
                ("u8" if kind in ("bytes", "range") else bt),
                "sub": sub, "cond": cond, "end": ex.get("end"), "mult": ex.get("mult", 1),
                "owner": name, "tagkw": self.recs[tag]["kw"], "tag": tag, "maxcount": ex.get("maxcount", 0),
                "elem_size_known": elem_size_known, "elem_override": ex.get("elem")}
        self.rules_flat.append(rule)
        return rule

    # -------------------------------------------------------------- C output
    def emit_c(self):
        o = []
        w = o.append
        w("/* generated by gen/gen.py from the WDK display headers - do not edit */")
        w('#include "nvddi.h"')
        w("#pragma GCC diagnostic ignored \"-Wpointer-arith\"")
        w("#pragma GCC diagnostic ignored \"-Wint-to-pointer-cast\"")
        w("#pragma GCC diagnostic ignored \"-Wpointer-to-int-cast\"")
        w("#pragma GCC diagnostic ignored \"-Wint-conversion\"")
        w("#pragma GCC diagnostic ignored \"-Wdiscarded-qualifiers\"")
        w("#pragma GCC diagnostic ignored \"-Wbad-function-cast\"")
        w("")
        # rules
        w("const NVDDI_RULE g_nvddi_rules[] = {")
        for r in self.rules_flat:
            T = "%s %s" % (r["tagkw"], r["tag"])
            P = "((%s*)0)->%s" % (T, r["path"])
            fields = [".id = %d" % r["id"], ".kind = %d" % RK[r["kind"]], ".when = %d" % r["when"],
                      ".ptr_off = (USHORT)__builtin_offsetof(%s, %s)" % (T, r["path"]),
                      ".sub = %d" % r["sub"], ".mult = %d" % r["mult"], ".maxcount = %d" % r["maxcount"]]
            if r["len"]:
                L = "((%s*)0)->%s" % (T, r["len"])
                fields.append(".len_off = (USHORT)__builtin_offsetof(%s, %s)" % (T, r["len"]))
                fields.append(".len_size = (UCHAR)sizeof(%s)" % L)
            if r["end"]:
                E = "((%s*)0)->%s" % (T, r["end"])
                fields.append(".end_off = (USHORT)__builtin_offsetof(%s, %s)" % (T, r["end"]))
                fields.append(".end_size = (UCHAR)sizeof(%s)" % E)
            if r["elem_override"]:
                fields.append(".elem_size = (ULONG)sizeof(%s %s)" % (self.recs[r["elem"]]["kw"], r["elem"]))
            elif r["kind"] in ("count", "one", "written") and r["elem_size_known"]:
                fields.append(".elem_size = (ULONG)sizeof(*%s)" % P)
            elif r["kind"] == "ptrarray":
                fields.append(".elem_size = (ULONG)sizeof(**%s)" % P)
            else:
                fields.append(".elem_size = 1")
            for ci, (cpath, cval) in enumerate(r["cond"] or []):
                sfx = "" if ci == 0 else "2"
                fields.append(".cond%s_off = (USHORT)__builtin_offsetof(%s, %s)" % (sfx, T, cpath))
                fields.append(".cond%s_size = (UCHAR)sizeof(((%s*)0)->%s)" % (sfx, T, cpath))
                fields.append(".cond%s_val = %d" % (sfx, cval))
            w("  { %s }," % ", ".join(fields))
        if not self.rules_flat:
            w("  { 0 },")
        w("};")
        w("")
        w("const NVDDI_RULESET g_nvddi_rulesets[] = {")
        for rs in self.rulesets:
            first = rs["rules"][0]["id"] if rs["rules"] else 0
            w("  { %d, %d, (ULONG)sizeof(%s %s) }, /* %s */" % (
                first, len(rs["rules"]), self.recs[rs["tag"]]["kw"], rs["tag"], rs["name"]))
        w("};")
        w("const ULONG g_nvddi_nrulesets = %d;" % len(self.rulesets))
        w("")
        # per-table originals, wrappers, slot descriptors
        for t in self.tables:
            n = len(t["slots"])
            tid = t["id"]
            w("/* ---- table %d: %s (%d slots) */" % (tid, t["cname"], n))
            w("void *g_nvddi_orig_%d[%d];" % (tid, max(n, 1)))
            w("extern const NVDDI_SLOT g_nvddi_slots_%d[%d];" % (tid, max(n, 1)))
            for s in t["slots"]:
                self.emit_wrapper(w, t, s)
            w("const NVDDI_SLOT g_nvddi_slots_%d[%d] = {" % (tid, max(n, 1)))
            for s in t["slots"]:
                args = []
                for a in s["args"]:
                    size = "(ULONG)sizeof(%s %s)" % (self.recs[a["rec"]]["kw"], a["rec"]) if a["rec"] else str(a["size"])
                    args.append("{ %d, %d, %d, %d, %s, %d }" % (
                        AK[a["kind"]], a["flags"], a["rs"], a["lenarg"], size,
                        a["table"] if a["table"] is not None else 0xFF))
                w("  { %d, %d, %d, %d, %d, (USHORT)__builtin_offsetof(%s, %s), (void*)w_%d_%d, \"%s\", { %s } },"
                  % (tid, s["index"], len(s["args"]), s["retkind"], s["retsize"], t["cname"], s["field"],
                     tid, s["index"], s["field"], ", ".join(args) if args else "{0}"))
            if not t["slots"]:
                w("  { 0 },")
            w("};")
            w("")
        w("const NVDDI_TABLE g_nvddi_tables[NVDDI_NTABLES] = {")
        for t in self.tables:
            how = {"ddi": 0, "sizefield": 1, "static": 2}[t["how"]]
            w("  { %d, %d, %d, (ULONG)sizeof(%s), %d, g_nvddi_slots_%d, g_nvddi_orig_%d, \"%s\" },"
              % (t["id"], len(t["slots"]), how, t["cname"], TABLE_MAXVER.get(t["cname"], 0), t["id"], t["id"],
                 t["cname"]))
        w("};")
        w("")
        schema = self.schema_text()
        open(os.path.join(self.out, "schema.txt"), "w").write(schema + "\n")
        w("const char g_nvddi_schema[] =")
        for line in schema.split("\n"):
            w('  "%s\\n"' % line.replace("\\", "\\\\").replace('"', '\\"'))
        w(";")
        w("const ULONG g_nvddi_schema_len = sizeof(g_nvddi_schema) - 1;")
        open(os.path.join(self.out, "nvddi_gen.c"), "w").write("\n".join(o) + "\n")
        h = ["/* generated by gen/gen.py - do not edit */", "#pragma once",
             "#define NVDDI_NTABLES %d" % len(self.tables),
             "#define NVDDI_NRULES %d" % max(len(self.rules_flat), 1)]
        for t in self.tables:
            h.append("#define NVDDI_T_%s %d" % (t["cname"], t["id"]))
            h.append("#define NVDDI_N_%s %d" % (t["cname"], len(t["slots"])))
            for s in t["slots"]:
                h.append("#define NVDDI_S_%d_%s %d" % (t["id"], s["field"], s["index"]))
        open(os.path.join(self.out, "nvddi_gen.h"), "w").write("\n".join(h) + "\n")

    def emit_wrapper(self, w, t, s):
        tid, idx = t["id"], s["index"]
        n = len(s["args"])
        decl = ", ".join("__typeof__(%s) a%d" % (a["type"], i) for i, a in enumerate(s["args"])) or "void"
        rt = s["ret"]
        rdecl = "void" if s["retkind"] == 0 else "__typeof__(%s)" % rt
        w("static %s NVDDI_WRAPCC w_%d_%d(%s)" % (rdecl, tid, idx, decl))
        w("{")
        w("    NVDDI_CALL c;")
        w("    ULONG_PTR av[%d];" % max(n, 1))
        if s["retkind"]:
            w("    __typeof__(%s) r;" % rt)
            w("    ULONG64 rv = 0;")
        for i, a in enumerate(s["args"]):
            if a["kind"] == "BYVAL":
                w("    av[%d] = 0; __builtin_memcpy(&av[%d], &a%d, sizeof(a%d) < 8 ? sizeof(a%d) : 8);" % (i, i, i, i, i))
            else:
                w("    av[%d] = (ULONG_PTR)a%d;" % (i, i))
        w("    nvddi_pre(&c, &g_nvddi_slots_%d[%d], av);" % (tid, idx))
        call_args = []
        for i, a in enumerate(s["args"]):
            if a["kind"] == "BYVAL":
                call_args.append("a%d" % i)
            else:
                call_args.append("(__typeof__(%s))av[%d]" % (a["type"], i))
        fp = "((__typeof__(((%s*)0)->%s))g_nvddi_orig_%d[%d])" % (t["cname"], s["field"], tid, idx)
        if s["retkind"]:
            w("    r = %s(%s);" % (fp, ", ".join(call_args)))
            if s["retkind"] == 1:
                w("    rv = (ULONG64)(ULONG_PTR)r;")
            else:
                w("    __builtin_memcpy(&rv, &r, sizeof(r) < 8 ? sizeof(r) : 8);")
            w("    nvddi_post(&c, av, rv);")
            w("    return r;")
        else:
            w("    %s(%s);" % (fp, ", ".join(call_args)))
            w("    nvddi_post(&c, av, 0);")
        w("}")

    def schema_text(self):
        L = ["NDL-SCHEMA 1", "WDK %s" % os.path.basename(os.path.normpath(self.a.wdk))]
        kinds = {v: k for k, v in AK.items()}
        for t in self.tables:
            L.append("T %d %s %s %d %d" % (t["id"], t["cname"], t["tag"], len(t["slots"]), t["size"]))
            for s in t["slots"]:
                L.append("S %d %d %s %d %d %d %s %s %s" % (
                    t["id"], s["index"], s["field"], s["off"], len(s["args"]), s["retkind"], s["irql"] or "-",
                    s["fname"] or "-", s["ret"]))
                for i, a in enumerate(s["args"]):
                    rec = a["rec"][1:] if a["rec"] and a["rec"].startswith("_") else (a["rec"] or "-")
                    L.append("A %d %d %d %s %s %d %d %d %d %s %s" % (
                        t["id"], s["index"], i, a["name"] or "a%d" % i, a["kind"], a["flags"], a["size"],
                        a["rs"], a["lenarg"], rec, a["type"]))
        for i, rs in enumerate(self.rulesets):
            L.append("RS %d %s %d" % (i, rs["name"], rs["size"]))
        for r in self.rules_flat:
            L.append("R %d %s %s %s %d %s %d %s %s" % (
                r["id"], r["owner"], r["path"], r["kind"], r["when"], r["elem_name"] or "-", r["sub"],
                r["len"] or "-", ",".join("%s=%d" % c for c in r["cond"]) if r["cond"] else "-"))
        return "\n".join(L)

    # -------------------------------------------------------------- decoder layouts
    def emit_layouts(self):
        keep = {}
        want = re.compile(r"^_?(DXGK|D3D|DRIVER_INIT|DXGKRNL|DXGKARG|LARGE_INTEGER|LUID|GUID|tagRECT|tagPOINT|"
                          r"MDL|DISPLAYID|VIDEO|ACPI|_?D3DKMDT|DEVICE_POWER|POWER_|QUERY_INTERFACE|INTERFACE|"
                          r"PCI_COMMON|DXGI|_DXGK)")
        for tag, rec in self.recs.items():
            if not re.match(r"^\w+$", tag) or not want.match(tag):
                continue
            keep[tag] = rec
        # field sizes via gcc probe: paths through anonymous members are transparent
        ex, idx = [], []
        for tag, rec in keep.items():
            stack = []
            for fi, f in enumerate(rec["fields"]):
                d = f["depth"]
                while stack and stack[-1][0] >= d:
                    stack.pop()
                stack.append((d, f["name"]))
                if f["name"] is None or f["bits"]:
                    continue
                path = ".".join(n for (_, n) in stack if n)
                ex.append("sizeof(((%s %s*)0)->%s)" % (rec["kw"], tag, path))
                idx.append((tag, fi, path))
        try:
            vals = probe(self.gcmd, self.out, "probe_fields", ex)
        except SystemExit:
            vals = None
            sys.stderr.write("field size probe failed; decoder falls back to offset gaps\n")
        if vals:
            for (tag, fi, path), v in zip(idx, vals):
                keep[tag]["fields"][fi]["size"] = v
        out = {"records": {}, "typedefs": {}, "enums": {}, "enum_typedefs": self.enum_typedef}
        for tag, rec in keep.items():
            out["records"][tag] = {"kw": rec["kw"], "size": rec["size"], "fields": [
                [f["off"], f["depth"], f["name"], f["type"], f.get("size"),
                 list(f["bits"]) if f["bits"] else None] for f in rec["fields"]]}
        for k, v in self.typedefs.items():
            if re.match(r"^(struct|union|enum) \w+$", v) or re.match(r"^\w+( \*)?$", v):
                out["typedefs"][k] = v
        for k, v in self.enums.items():
            if k.startswith("_") or k[:1].isupper():
                out["enums"][k] = v
        with open(os.path.join(self.out, "ndl_layouts.json"), "w") as f:
            json.dump(out, f, separators=(",", ":"), sort_keys=True)
        # offsets/sizes that the capture rules depend on, for layoutcheck.py
        chk = []
        for r in self.rules_flat:
            T = "%s %s" % (r["tagkw"], r["tag"])
            for p in [r["path"], r["len"], r["end"]] + [c[0] for c in (r["cond"] or [])]:
                if p:
                    chk.append("__builtin_offsetof(%s, %s)" % (T, p))
        for rs in self.rulesets:
            chk.append("sizeof(%s %s)" % (self.recs[rs["tag"]]["kw"], rs["tag"]))
        for t in self.tables:
            chk.append("sizeof(%s)" % t["cname"])
            for s in t["slots"]:
                chk.append("__builtin_offsetof(%s, %s)" % (t["cname"], s["field"]))
        with open(os.path.join(self.out, "layoutcheck.txt"), "w") as f:
            f.write("\n".join(chk) + "\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--wdk", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--cc", default="x86_64-w64-mingw32-gcc")
    ap.add_argument("--clang", default="clang")
    a = ap.parse_args()
    g = Gen(a)
    g.run_all()
    n = sum(len(t["slots"]) for t in g.tables)
    print("gen: %d tables, %d slots, %d rulesets, %d rules" % (
        len(g.tables), n, len(g.rulesets), len(g.rules_flat)))


if __name__ == "__main__":
    main()
