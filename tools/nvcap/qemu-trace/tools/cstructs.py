"""Minimal C structure layout parser for NVIDIA's RM headers (used by
gen_tables.py). Handles what the SDK/RPC headers use: typedef'd and tagged
structs/unions, inline nested structs/unions, arrays with constant
expressions, bitfields, NV_DECLARE_ALIGNED / NV_ALIGN_BYTES, enums, simple
typedef aliases. Natural (x86-64) alignment. Anything it cannot resolve is
left out, so the decoder falls back to hex for it.

Output layout per struct:
  {"s": size, "a": align, "f": [[name, offset, kind, elem_size, count,
                                  sub, bit_offset, bits], ...]}
kind: u unsigned, i signed, c char, b bool, p pointer/NvP64, f float,
      S struct/union (sub = its name), e enum (4 bytes).
count 0 = flexible array member.
"""
import re

PRIM = {
    "NvU8": ("u", 1), "NvS8": ("i", 1), "NvV8": ("u", 1), "char": ("c", 1),
    "NvBool": ("b", 1), "NvU16": ("u", 2), "NvS16": ("i", 2),
    "NvV16": ("u", 2), "NvU32": ("u", 4), "NvS32": ("i", 4),
    "NvV32": ("u", 4), "NvHandle": ("u", 4), "NvF32": ("f", 4),
    "int": ("i", 4), "unsigned": ("u", 4), "NvU64": ("u", 8),
    "NvS64": ("i", 8), "NvP64": ("p", 8), "NvLength": ("u", 8),
    "RmPhysAddr": ("u", 8), "NvUPtr": ("u", 8), "NvF64": ("f", 8),
    "NvUFXP64": ("u", 8), "LibosAddress": ("u", 8), "NvRmctrlCmd": ("u", 4),
    "NvProcessorUuid": None, "uint8_t": ("u", 1), "uint16_t": ("u", 2),
    "uint32_t": ("u", 4), "uint64_t": ("u", 8), "int32_t": ("i", 4),
    "int64_t": ("i", 8), "size_t": ("u", 8), "NvTypedHandle": None,
    "NvSPtr": ("i", 8), "NvOffset": ("u", 8), "LwU32": ("u", 4),
    "float": ("f", 4), "double": ("f", 8), "long": ("i", 8),
}
PRIM = {k: v for k, v in PRIM.items() if v}

RE_COMMENT = re.compile(r"/\*.*?\*/|//[^\n]*", re.S)
RE_DEFINE = re.compile(r"^\s*#\s*define\s+([A-Za-z_]\w*)\s+(.+?)\s*$", re.M)
RE_ENUM = re.compile(r"\benum\s*\w*\s*\{([^{}]*)\}", re.S)
RE_TOKEN = re.compile(r"[A-Za-z_]\w*|0[xX][0-9a-fA-F]+[uUlL]*|\d+[uUlL]*|"
                      r"<<|>>|[{}()\[\];:,*=+\-/|&~<>?!%^]")


def strip_comments(text):
    return RE_COMMENT.sub(" ", text)


def _strip_macro(text, macro, keep_first_arg):
    """Remove NAME(args): keep the first argument or drop the whole call."""
    out = []
    i = 0
    while True:
        j = text.find(macro, i)
        if j < 0:
            out.append(text[i:])
            break
        k = j + len(macro)
        while k < len(text) and text[k] in " \t\n":
            k += 1
        if k >= len(text) or text[k] != "(" or \
                (j > 0 and (text[j - 1].isalnum() or text[j - 1] == "_")):
            out.append(text[i:k])
            i = k
            continue
        depth, m, comma = 0, k, None
        while m < len(text):
            if text[m] == "(":
                depth += 1
            elif text[m] == ")":
                depth -= 1
                if depth == 0:
                    break
            elif text[m] == "," and depth == 1 and comma is None:
                comma = m
            m += 1
        out.append(text[i:j])
        if keep_first_arg:
            out.append(" " + text[k + 1:comma if comma else m] + " ")
        i = m + 1
    return "".join(out)


RE_EXTERN_C = re.compile(r'extern\s*"C"\s*\{')
RE_STRING = re.compile(r'"(?:[^"\\\n]|\\.)*"')


def preprocess(text):
    text = strip_comments(text)
    text = RE_EXTERN_C.sub(" ", text)
    text = RE_STRING.sub(" 0 ", text)
    text = text.replace("\\\n", " ")
    text = _strip_macro(text, "NV_DECLARE_ALIGNED", True)
    text = _strip_macro(text, "NV_ALIGN_BYTES", False)
    text = _strip_macro(text, "__attribute__", False)
    return text


class Consts:
    """Integer #define constants, evaluated lazily."""

    def __init__(self):
        self.raw = {}
        self.val = {}

    def add_text(self, text):
        text = strip_comments(text)
        for name, expr in RE_DEFINE.findall(text):
            if "(" in name:
                continue
            self.raw.setdefault(name, expr)
        # enumerators: NAME [= expr], ... -> sequential values
        for body in RE_ENUM.findall(text):
            prev = None
            for item in body.split(","):
                item = item.strip()
                if not item or item.startswith("#"):
                    continue
                m = re.match(r"([A-Za-z_]\w*)\s*(?:=\s*(.+))?$", item, re.S)
                if not m:
                    break
                name, expr = m.group(1), m.group(2)
                if expr is None:
                    expr = "0" if prev is None else f"({prev})+1"
                self.raw.setdefault(name, expr)
                prev = name

    def eval(self, expr, depth=0):
        if depth > 20:
            return None
        toks = RE_TOKEN.findall(expr)
        out = []
        i = 0
        while i < len(toks):
            t = toks[i]
            if t[0].isdigit():
                out.append(str(int(t.rstrip("uUlL"), 0)))
            elif t[0].isalpha() or t[0] == "_":
                # casts like (NvU32) and the U/L suffix macros
                if t in PRIM or t in ("unsigned", "signed", "const"):
                    if out and out[-1] == "(" and i + 1 < len(toks) and \
                            toks[i + 1] == ")":
                        out.pop()
                        i += 2
                        continue
                    return None
                if t in ("NVBIT", "BIT", "NVBIT32", "NVBIT64") and \
                        i + 1 < len(toks) and toks[i + 1] == "(":
                    out.append("1<<")
                    i += 1
                    continue
                if t in ("NV_MAX", "NV_MIN") and i + 1 < len(toks):
                    out.append("max" if t == "NV_MAX" else "min")
                    i += 1
                    continue
                v = self.get(t, depth + 1)
                if v is None:
                    return None
                out.append(str(v))
            elif t == "/":
                out.append("//")
            elif t in ("?", "!", "^"):
                return None
            else:
                out.append(t)
            i += 1
        try:
            v = eval("".join(out), {"__builtins__": {}},
                     {"max": max, "min": min})
        except Exception:
            return None
        return v if isinstance(v, int) else None

    def get(self, name, depth=0):
        if name in self.val:
            return self.val[name]
        expr = self.raw.get(name)
        if expr is None:
            return None
        self.val[name] = None                 # recursion guard
        v = self.eval(expr, depth)
        self.val[name] = v
        return v


class Parser:
    def __init__(self, consts):
        self.c = consts
        self.defs = {}        # name -> ("struct"/"union", members) | alias
        self.aliases = {}     # typedef name -> target type name
        self.enums = set()
        self.anon = 0

    # ---- parsing ----
    def add_text(self, text):
        toks = RE_TOKEN.findall(preprocess(
            "\n".join(l for l in text.splitlines()
                      if not l.lstrip().startswith("#"))))
        i, n = 0, len(toks)
        while i < n:
            t = toks[i]
            if t == "typedef":
                i = self._typedef(toks, i + 1)
            elif t in ("struct", "union") and i + 2 < n and \
                    toks[i + 1] not in "{" and toks[i + 2] == "{":
                name = toks[i + 1]
                mem, j = self._body(toks, i + 3)
                if mem is not None:
                    self.defs.setdefault(name, (t, mem))
                i = j
            elif t == "{":                    # skip function bodies etc.
                i = self._skip_block(toks, i)
            else:
                i += 1

    def _skip_block(self, toks, i):
        depth = 0
        while i < len(toks):
            if toks[i] == "{":
                depth += 1
            elif toks[i] == "}":
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        return i

    def _typedef(self, toks, i):
        n = len(toks)
        if i < n and toks[i] in ("struct", "union", "enum"):
            kind = toks[i]
            tag = None
            i += 1
            if i < n and toks[i] != "{":
                tag = toks[i]
                i += 1
            if i < n and toks[i] == "{":
                if kind == "enum":
                    i = self._skip_block(toks, i)
                    names, i = self._decl_names(toks, i)
                    for nm in names:
                        self.enums.add(nm)
                    if tag:
                        self.enums.add(tag)
                    return i
                mem, i = self._body(toks, i + 1)
                names, i = self._decl_names(toks, i)
                if mem is None:
                    return i
                key = tag or (names[0] if names else None)
                if key:
                    self.defs.setdefault(key, (kind, mem))
                    for nm in names:
                        if nm != key:
                            self.aliases.setdefault(nm, key)
                return i
            # typedef struct TAG NAME;
            names, i = self._decl_names(toks, i)
            for nm in names:
                if kind == "enum":
                    self.enums.add(nm)
                elif tag and nm != tag:
                    self.aliases.setdefault(nm, tag)
            return i
        # typedef TYPE NAME;  (single-token types only)
        j = i
        while j < n and toks[j] != ";":
            j += 1
        seg = [t for t in toks[i:j] if t not in ("const", "volatile")]
        if len(seg) == 2 and "(" not in seg:
            self.aliases.setdefault(seg[1], seg[0])
        elif len(seg) == 3 and seg[1] == "*":
            self.aliases.setdefault(seg[2], "NvP64")
        return j + 1

    def _decl_names(self, toks, i):
        names = []
        n = len(toks)
        while i < n and toks[i] != ";":
            if re.match(r"[A-Za-z_]\w*$", toks[i]) and toks[i] != "const":
                names.append(toks[i])
            if toks[i] == "{":
                i = self._skip_block(toks, i)
                continue
            i += 1
        return names, i + 1

    def _body(self, toks, i):
        """Parse members up to the matching '}'. Returns (members, index
        after '}'); members is None when something is unparseable."""
        members = []
        ok = True
        n = len(toks)
        while i < n and toks[i] != "}":
            t = toks[i]
            if t == ";":
                i += 1
                continue
            if t in ("struct", "union") and (toks[i + 1] == "{" or
                                             (i + 2 < n and toks[i + 2] == "{")):
                kind = t
                j = i + 1
                if toks[j] != "{":
                    j += 1
                sub, j = self._body(toks, j + 1)
                self.anon += 1
                aname = f"__anon{self.anon}"
                if sub is None:
                    ok = False
                else:
                    self.defs[aname] = (kind, sub)
                decls, j = self._declarators(toks, j)
                if decls is None:
                    ok = False
                elif decls:
                    for d in decls:
                        members.append((aname,) + d)
                else:                           # anonymous member: flatten
                    members.append((aname, "", [], None))
                i = j
                continue
            # TYPE declarators ;
            j = i
            typ = []
            while j < n and toks[j] not in (";", "}") and \
                    not (typ and re.match(r"[A-Za-z_]\w*$", toks[j]) and
                         toks[j + 1] in (";", "[", ",", ":")):
                if toks[j] not in ("const", "volatile", "struct", "union",
                                   "enum", "signed"):
                    typ.append(toks[j])
                if toks[j] == "(":            # function pointer
                    ok = False
                    while j < n and toks[j] != ";":
                        j += 1
                    break
                j += 1
            if not ok and j < n and toks[j] == ";":
                i = j + 1
                continue
            ptr = "*" in typ
            typ = [t for t in typ if t != "*"]
            if typ == ["unsigned", "int"] or typ == ["unsigned", "long"]:
                typ = ["unsigned"]
            elif typ == ["unsigned", "char"]:
                typ = ["NvU8"]
            elif typ == ["unsigned", "short"]:
                typ = ["NvU16"]
            elif len(typ) == 2 and typ[0] == "unsigned":
                typ = [typ[1]]
            if len(typ) != 1:
                ok = False
                while j < n and toks[j] not in (";", "}"):
                    j += 1
                i = j + 1 if j < n and toks[j] == ";" else j
                continue
            decls, j = self._declarators(toks, j)
            if decls is None:
                ok = False
            else:
                for d in decls:
                    members.append(("NvP64" if ptr else typ[0],) + d)
            i = j
        return (members if ok else None), i + 1

    def _declarators(self, toks, i):
        """name [dims] [: bits] {, ...} ; -> list of (name, dims, bits)."""
        out = []
        n = len(toks)
        while i < n and toks[i] != ";":
            if toks[i] == ",":
                i += 1
                continue
            if toks[i] == "*":
                i += 1
                continue
            name = toks[i]
            if not re.match(r"[A-Za-z_]\w*$", name):
                return None, self._to_semi(toks, i)
            i += 1
            dims = []
            bits = None
            while i < n and toks[i] == "[":
                j = i + 1
                depth = 1
                while j < n and depth:
                    if toks[j] == "[":
                        depth += 1
                    elif toks[j] == "]":
                        depth -= 1
                    j += 1
                expr = " ".join(toks[i + 1:j - 1])
                if not expr:
                    dims.append(0)
                else:
                    v = self.c.eval(expr)
                    if v is None:
                        return None, self._to_semi(toks, j)
                    dims.append(v)
                i = j
            if i < n and toks[i] == ":":
                v = self.c.eval(toks[i + 1])
                if v is None:
                    return None, self._to_semi(toks, i)
                bits = v
                i += 2
            out.append((name, dims, bits))
        return out, i + 1

    def _to_semi(self, toks, i):
        while i < len(toks) and toks[i] != ";":
            i += 1
        return i + 1

    # ---- layout ----
    def resolve(self, name, seen=None):
        seen = seen or set()
        for _ in range(16):
            if name in PRIM or name in self.defs:
                return name
            if name in self.enums:
                return "__enum"
            if name in self.aliases and name not in seen:
                seen.add(name)
                name = self.aliases[name]
                continue
            return None
        return None

    def layout_all(self):
        self.lay = {}
        for name in list(self.defs):
            self._layout(name, set())
        out = {k: v for k, v in self.lay.items() if v is not None}
        for alias, target in self.aliases.items():
            t = self.resolve(target)
            if t in out and alias not in out:
                out[alias] = {"alias": t}
        return out

    def _type_info(self, tname, stack):
        t = self.resolve(tname)
        if t is None:
            return None
        if t == "__enum":
            return ("e", 4, 4, None)
        if t in PRIM:
            k, sz = PRIM[t]
            return (k, sz, sz, None)
        lay = self._layout(t, stack)
        if lay is None:
            return None
        return ("S", lay["s"], lay["a"], t)

    def _layout(self, name, stack):
        if name in self.lay:
            return self.lay[name]
        if name in stack:
            return None
        stack = stack | {name}
        kind, members = self.defs[name]
        fields = []
        off = 0
        size = 0
        align = 1
        bf_unit = None              # (offset, unit size, used bits)
        for typ, mname, dims, bits in members:
            info = self._type_info(typ, stack)
            if info is None:
                self.lay[name] = None
                return None
            k, esz, eal, sub = info
            count = 1
            flex = False
            for dv in dims:
                if dv == 0:
                    flex = True
                count *= dv
            if bits is not None and kind == "struct":
                if bits == 0:
                    bf_unit = None
                    continue
                if bf_unit and bf_unit[1] == esz and bf_unit[2] + bits <= esz * 8:
                    uoff, usz, used = bf_unit
                else:
                    off = (off + eal - 1) // eal * eal
                    uoff, usz, used = off, esz, 0
                    off += esz
                fields.append([mname, uoff, k, esz, 1, None, used, bits])
                bf_unit = (uoff, usz, used + bits)
                align = max(align, eal)
                size = max(size, off)
                continue
            bf_unit = None
            if kind == "union":
                fo = 0
            else:
                off = (off + eal - 1) // eal * eal
                fo = off
            if mname == "" and sub:          # anonymous struct/union member
                for f in self.lay[sub]["f"]:
                    fields.append([f[0], fo + f[1]] + f[2:])
            else:
                fields.append([mname, fo, k, esz, 0 if flex else count, sub,
                               None, None])
            total = 0 if flex else esz * count
            align = max(align, eal)
            if kind == "union":
                size = max(size, total)
            else:
                off += total
                size = off
        size = (size + align - 1) // align * align
        res = {"s": size, "a": align, "f": fields}
        self.lay[name] = res
        return res
