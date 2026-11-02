#!/usr/bin/env python3
# pe-import-audit.py — PE import auditor for the NDM native programme.
#
# Implements the NATIVE-PLAN.md risk-#4 mitigation ("Windows 7/8 API
# discipline"): parse a Windows .exe/.dll, list every imported DLL
# and function, and FAIL (exit 1) if any import is a function that
# does not exist on Windows 8.0 — i.e. it was introduced in
# Windows 8.1 or Windows 10+. Also reports the PE subsystem
# version (target: 6.0 / 6.1) and the machine type; a subsystem
# version above 6.1 also fails (override with
# --allow-newer-subsystem to downgrade that one check to a
# warning).
#
# Dependency-free: python3 standard library only. No network.
#
# Usage:
#   python3 pe-import-audit.py <file.exe|file.dll> [--json]
#   python3 pe-import-audit.py --self-test
#
# Exit codes: 0 = PASS, 1 = FAIL (blocklisted import and/or
# subsystem version too new), 2 = not a PE / unreadable / usage.
#
# ------------------------------------------------------------------
# BLOCKLIST POLICY — read before extending.
#
# A function is blocklisted ONLY when its introduction version is
# documented with high confidence as Windows 8.1 (6.3) or later.
# A short, accurate blocklist beats a long, guessed one: anything
# uncertain is deliberately EXCLUDED and listed under
# "considered and excluded" below, with the reason.
#
# Considered and EXCLUDED (allowed on Windows 8.0 or earlier, or
# introduction version not certain enough to fail a build on):
#   CreateFile2            — introduced in Windows 8.0. ALLOWED.
#   WaitOnAddress,
#   WakeByAddressSingle,
#   WakeByAddressAll       — introduced in Windows 8.0. ALLOWED.
#   GetSystemTimePreciseAsFileTime — Windows 8.0. ALLOWED.
#   GetTickCount64         — Windows Vista. ALLOWED.
#   CopyFile2              — Windows 8.0. ALLOWED.
#   PrefetchVirtualMemory  — Windows 8.0. ALLOWED.
#   GetProcessMitigationPolicy / SetProcessMitigationPolicy
#                          — Windows 8.0. ALLOWED.
#   PathCch* (pathcch)     — Windows 8.0. ALLOWED.
#   GetCurrentPackageId / GetCurrentPackageFullName family
#                          — mostly Windows 8.0, but a few members
#                            are 8.1; per-member certainty is not
#                            high enough. EXCLUDED (uncertain).
#   GetTempPath2A/W        — documentation is inconsistent about
#                            whether this is a Windows 10 or a
#                            Windows 11 / Server 2022 addition.
#                            EXCLUDED (uncertain) — if NDM ever
#                            imports it, check manually.
#   SetProcessValidCallTargets, GetPointerFramePenInfoHistory,
#   OpenThemeDataForDpi, GetDpiForMonitorInternal,
#   RoGetAgileReference, EventSetInformation
#                          — introduction versions not certain
#                            enough. EXCLUDED (uncertain).
#                            EventSetInformation is suspected
#                            Windows 10, but that could not be
#                            verified from a citable source while
#                            writing this tool; it appears in the
#                            wild (e.g. WebView2Loader.dll) only
#                            as a DELAY import, which is the safe
#                            pattern — a delay-loaded import does
#                            not stop the binary loading on old
#                            Windows, it only fails if called.
# ------------------------------------------------------------------

import json
import struct
import sys

# (dll_lowercase_or_None, function) -> (introduced_in, note)
# dll None means: blocklisted in ANY dll (the name is unique
# enough). A dll-specific entry takes precedence in reporting.
BLOCKLIST = {
    # --- DPI awareness family (the realistic trap for a Win32 GUI
    # --- shell: every one of these looks like it "should" be old)
    (None, "GetDpiForWindow"): (
        "Windows 10, version 1607",
        "user32 DPI API; use GetDeviceCaps(LOGPIXELSX) on Win7/8"),
    (None, "GetDpiForSystem"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "SystemParametersInfoForDpi"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "AdjustWindowRectExForDpi"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "EnableNonClientDpiScaling"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "SetProcessDpiAwarenessContext"): (
        "Windows 10, version 1607",
        "user32; SetProcessDPIAware (Vista) is the old-Windows path"),
    (None, "GetProcessDpiAwarenessContext"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "SetThreadDpiAwarenessContext"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "GetThreadDpiAwarenessContext"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "GetWindowDpiAwarenessContext"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "GetAwarenessFromDpiAwarenessContext"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "AreDpiAwarenessContextsEqual"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "IsValidDpiAwarenessContext"): (
        "Windows 10, version 1607", "user32 DPI API"),
    (None, "GetDpiForMonitor"): (
        "Windows 8.1", "shcore DPI API; no per-monitor DPI on 8.0"),
    (None, "SetProcessDpiAwareness"): (
        "Windows 8.1",
        "shcore; SetProcessDPIAware (Vista) is the old-Windows path"),
    (None, "GetProcessDpiAwareness"): (
        "Windows 8.1", "shcore DPI API"),
    (None, "GetScaleFactorForMonitor"): (
        "Windows 8.1", "shcore scaling API"),
    (None, "RegisterScaleChangeNotifications"): (
        "Windows 8.1", "shcore scaling API"),
    (None, "RevokeScaleChangeNotifications"): (
        "Windows 8.1", "shcore scaling API"),
    (None, "GetDisplayAutoRotationPreferences"): (
        "Windows 8.1", "user32 display API"),
    (None, "SetDisplayAutoRotationPreferences"): (
        "Windows 8.1", "user32 display API"),
    # --- kernel32 / kernelbase, Windows 10
    (None, "IsWow64Process2"): (
        "Windows 10, version 1511",
        "kernel32; IsWow64Process (XP SP2) is the old-Windows path"),
    (None, "GetSystemCpuSetInformation"): (
        "Windows 10", "kernel32 CPU-set API"),
    (None, "SetThreadSelectedCpuSets"): (
        "Windows 10", "kernel32 CPU-set API"),
    (None, "GetThreadSelectedCpuSets"): (
        "Windows 10", "kernel32 CPU-set API"),
    (None, "VirtualAlloc2"): (
        "Windows 10",
        "kernelbase NUMA/placeholder allocation API"),
    (None, "VirtualAlloc2FromApp"): (
        "Windows 10", "kernelbase allocation API"),
    (None, "OfferVirtualMemory"): (
        "Windows 10", "kernel32 memory-offer API"),
    (None, "ReclaimVirtualMemory"): (
        "Windows 10", "kernel32 memory-offer API"),
    # --- kernel32, Windows 8.1
    (None, "DiscardVirtualMemory"): (
        "Windows 8.1", "kernel32; VirtualAlloc/MEM_RESET is older"),
}

MACHINE_TYPES = {
    0x014C: "I386 (x86)",
    0x8664: "AMD64 (x86-64)",
    0x01C0: "ARM",
    0x01C4: "ARMv7 (ARMNT)",
    0xAA64: "ARM64",
    0x0200: "IA64",
}

SUBSYSTEMS = {
    1: "Native", 2: "Windows GUI", 3: "Windows CUI",
    5: "OS/2 CUI", 7: "POSIX CUI", 9: "Windows CE GUI",
    10: "EFI application", 14: "Xbox", 16: "Windows boot application",
}

# Subsystem-version ceiling: Windows 7 = 6.1, Windows Vista = 6.0.
MAX_SUBSYSTEM = (6, 1)


class PEError(Exception):
    pass


def _u16(data, off):
    return struct.unpack_from("<H", data, off)[0]


def _u32(data, off):
    return struct.unpack_from("<I", data, off)[0]


def _u64(data, off):
    return struct.unpack_from("<Q", data, off)[0]


def _cstring(data, off):
    end = data.find(b"\x00", off)
    if end < 0:
        raise PEError("unterminated string at offset %#x" % off)
    return data[off:end].decode("ascii", errors="replace")


class Section:
    def __init__(self, data, off):
        self.virtual_size = _u32(data, off + 8)
        self.virtual_address = _u32(data, off + 12)
        self.raw_size = _u32(data, off + 16)
        self.raw_ptr = _u32(data, off + 20)

    def rva_to_offset(self, rva):
        span = max(self.virtual_size, self.raw_size)
        if self.virtual_address <= rva < self.virtual_address + span:
            delta = rva - self.virtual_address
            if delta >= self.raw_size:
                return None  # in the virtual tail (BSS): no file bytes
            return self.raw_ptr + delta
        return None


class PEFile:
    def __init__(self, data):
        self.data = data
        if len(data) < 0x40 or data[0:2] != b"MZ":
            raise PEError("missing MZ (DOS) signature — not a PE file")
        pe_off = _u32(data, 0x3C)
        if data[pe_off:pe_off + 4] != b"PE\x00\x00":
            raise PEError("missing PE\\0\\0 signature at %#x" % pe_off)
        coff = pe_off + 4
        self.machine = _u16(data, coff)
        num_sections = _u16(data, coff + 2)
        opt_size = _u16(data, coff + 16)
        opt = coff + 20
        magic = _u16(data, opt)
        if magic == 0x10B:
            self.is64 = False
        elif magic == 0x20B:
            self.is64 = True
        else:
            raise PEError("unknown optional-header magic %#x" % magic)
        # Optional-header layout is identical for these fields in
        # PE32 and PE32+ up to BaseOfCode; parse sequentially.
        p = opt
        p += 2 + 2 + 4 + 4 + 4 + 4 + 4      # magic..BaseOfCode
        if not self.is64:
            p += 4                           # BaseOfData (PE32 only)
        p += 8 if self.is64 else 4           # ImageBase
        p += 4 + 4                           # Section/FileAlignment
        self.os_version = (_u16(data, p), _u16(data, p + 2))
        p += 4                               # OS version
        p += 4                               # Image version
        self.subsystem_version = (_u16(data, p), _u16(data, p + 2))
        p += 4                               # Subsystem version
        p += 4 + 4 + 4 + 4                   # Win32Ver, SizeOfImage,
        #                                      SizeOfHeaders, CheckSum
        self.subsystem = _u16(data, p)
        # Data directories: PE32 at opt+96, PE32+ at opt+112.
        dd = opt + (112 if self.is64 else 96)
        self.dirs = []
        for i in range(16):
            if dd + i * 8 + 8 <= opt + opt_size:
                self.dirs.append((_u32(data, dd + i * 8),
                                  _u32(data, dd + i * 8 + 4)))
            else:
                self.dirs.append((0, 0))
        sec_off = opt + opt_size
        self.sections = [Section(data, sec_off + i * 40)
                         for i in range(num_sections)]

    def rva_to_offset(self, rva):
        if rva < 0x1000:  # headers are mapped 1:1
            return rva if rva < len(self.data) else None
        for s in self.sections:
            off = s.rva_to_offset(rva)
            if off is not None:
                return off
        return None

    def _read_thunks(self, thunk_rva):
        names = []
        if thunk_rva == 0:
            return names
        off = self.rva_to_offset(thunk_rva)
        if off is None:
            raise PEError("thunk RVA %#x not mapped" % thunk_rva)
        size = 8 if self.is64 else 4
        ordinal_flag = 0x8000000000000000 if self.is64 else 0x80000000
        while True:
            val = _u64(self.data, off) if self.is64 else _u32(self.data, off)
            off += size
            if val == 0:
                break
            if val & ordinal_flag:
                names.append("#%d (ordinal)" % (val & 0xFFFF))
            else:
                noff = self.rva_to_offset(val & 0x7FFFFFFF)
                if noff is None:
                    names.append("<unmapped name RVA %#x>" % val)
                else:
                    names.append(_cstring(self.data, noff + 2))  # skip Hint
            if len(names) > 100000:
                raise PEError("import thunk runaway")
        return names

    def imports(self, dir_index=1):
        """Return [(dll_name, [func, ...]), ...] for the import dir
        (20-byte IMAGE_IMPORT_DESCRIPTOR array)."""
        if dir_index >= len(self.dirs):
            return []
        rva, _size = self.dirs[dir_index]
        if rva == 0:
            return []
        off = self.rva_to_offset(rva)
        if off is None:
            raise PEError("import directory RVA %#x not mapped" % rva)
        out = []
        while True:
            (oft, _ts, _fwd, name_rva, ft) = struct.unpack_from(
                "<IIIII", self.data, off)
            off += 20
            if oft == 0 and name_rva == 0 and ft == 0:
                break
            noff = self.rva_to_offset(name_rva)
            dll = _cstring(self.data, noff) if noff is not None else "<?>"
            funcs = self._read_thunks(oft if oft != 0 else ft)
            out.append((dll, funcs))
            if len(out) > 10000:
                raise PEError("import descriptor runaway")
        return out

    def delay_imports(self):
        """Return [(dll_name, [func, ...]), ...] for the delay-import
        dir (directory 13). Its descriptors are a DIFFERENT struct —
        32-byte ImgDelayDescr: grAttrs, szName(RVA), phmod, pIAT,
        pINT, pBoundIAT, pUnloadIAT, dwTimeStamp — not the 20-byte
        import descriptor, so it needs its own walk."""
        if len(self.dirs) <= 13:
            return []
        rva, _size = self.dirs[13]
        if rva == 0:
            return []
        off = self.rva_to_offset(rva)
        if off is None:
            raise PEError("delay-import directory RVA %#x not mapped" % rva)
        out = []
        while True:
            fields = struct.unpack_from("<IIIIIIII", self.data, off)
            off += 32
            (_attrs, name_rva, _phmod, iat_rva, int_rva,
             _bound, _unload, _ts) = fields
            if name_rva == 0 and int_rva == 0 and iat_rva == 0:
                break
            noff = self.rva_to_offset(name_rva)
            dll = _cstring(self.data, noff) if noff is not None else "<?>"
            funcs = self._read_thunks(int_rva if int_rva != 0 else iat_rva)
            out.append((dll, funcs))
            if len(out) > 10000:
                raise PEError("delay-import descriptor runaway")
        return out


def blocklist_hit(dll, func):
    if func.startswith("#"):
        return None  # ordinals cannot be name-checked
    for key in ((dll.lower(), func), (None, func)):
        if key in BLOCKLIST:
            return BLOCKLIST[key]
    return None


def audit(path, data=None):
    if data is None:
        with open(path, "rb") as f:
            data = f.read()
    pe = PEFile(data)
    result = {
        "file": path,
        "machine": MACHINE_TYPES.get(
            pe.machine, "unknown (%#x)" % pe.machine),
        "machine_raw": pe.machine,
        "pe_format": "PE32+ (64-bit)" if pe.is64 else "PE32 (32-bit)",
        "subsystem": SUBSYSTEMS.get(pe.subsystem, str(pe.subsystem)),
        "subsystem_version": "%d.%d" % pe.subsystem_version,
        "os_version": "%d.%d" % pe.os_version,
        "imports": [],          # regular imports (directory 1)
        "delay_imports": [],    # delay-loaded imports (directory 13)
        "violations": [],
        "warnings": [],
    }
    for label, entries in (("imports", pe.imports(1)),
                           ("delay_imports", pe.delay_imports())):
        for dll, funcs in entries:
            entry = {"dll": dll, "functions": funcs}
            result[label].append(entry)
            for fn in funcs:
                hit = blocklist_hit(dll, fn)
                if hit:
                    result["violations"].append({
                        "dll": dll, "function": fn,
                        "introduced_in": hit[0], "note": hit[1],
                        "via": "delay-import" if label == "delay_imports" else "import",
                    })
    if pe.subsystem_version > MAX_SUBSYSTEM:
        result["warnings"].append(
            "subsystem version %s exceeds target %d.%d — the loader "
            "on Windows 7/8.0 will refuse or mis-handle this binary"
            % (result["subsystem_version"], MAX_SUBSYSTEM[0],
               MAX_SUBSYSTEM[1]))
    return result


def print_report(r, subsystem_is_failure):
    print("PE import audit: %s" % r["file"])
    print("  Machine:            %s" % r["machine"])
    print("  Format:             %s" % r["pe_format"])
    print("  Subsystem:          %s" % r["subsystem"])
    print("  Subsystem version:  %s (target <= %d.%d)"
          % (r["subsystem_version"], MAX_SUBSYSTEM[0], MAX_SUBSYSTEM[1]))
    print("  OS version field:   %s" % r["os_version"])
    total = 0
    for label, title in (("imports", "Imports"),
                         ("delay_imports", "Delay imports")):
        if not r[label]:
            continue
        print("  %s:" % title)
        for e in r[label]:
            total += len(e["functions"])
            print("    %s (%d functions)" % (e["dll"], len(e["functions"])))
            for fn in e["functions"]:
                print("      %s" % fn)
    print("  Total imported functions: %d" % total)
    failed = bool(r["violations"]) or (
        subsystem_is_failure and bool(r["warnings"]))
    for v in r["violations"]:
        print("  BLOCKED: %s!%s — introduced in %s (%s)%s"
              % (v["dll"], v["function"], v["introduced_in"],
                 v["note"],
                 " [delay-import]" if v["via"] == "delay-import" else ""))
    for w in r["warnings"]:
        print("  %s: %s" % ("BLOCKED" if subsystem_is_failure else "WARNING", w))
    print("  Verdict: %s" % ("FAIL" if failed else "PASS"))
    return failed


# ------------------------------------------------------------------
# Self-test: build a minimal synthetic PE32+ in memory — one clean
# fixture (only pre-8.0 imports) and one poisoned fixture (adds
# GetDpiForWindow) — and check the auditor's verdicts on both.
# ------------------------------------------------------------------

def _synthetic_pe(extra_user32_funcs):
    # Layout: headers at 0, one section ".idata" at RVA 0x1000 /
    # file offset 0x200, holding the import directory, ILT/IAT,
    # hint/name strings and DLL names.
    import struct as st
    buf = bytearray(0x200 + 0x1000)
    buf[0:2] = b"MZ"
    st.pack_into("<I", buf, 0x3C, 0x80)               # e_lfanew
    buf[0x80:0x84] = b"PE\x00\x00"
    coff = 0x84
    st.pack_into("<HH", buf, coff, 0x8664, 1)         # machine, 1 section
    st.pack_into("<H", buf, coff + 16, 240)           # SizeOfOptionalHeader
    opt = coff + 20
    st.pack_into("<H", buf, opt, 0x20B)               # PE32+
    # Subsystem version 6.1 at opt+52 for PE32+ (see parser offsets:
    # sequential walk lands there); set via the same walk:
    p = opt + 2 + 2 + 4 + 4 + 4 + 4 + 4 + 8 + 4 + 4   # ..FileAlignment
    st.pack_into("<HH", buf, p, 6, 0)                 # OS version 6.0
    st.pack_into("<HH", buf, p + 8, 6, 1)             # Subsystem ver 6.1
    st.pack_into("<H", buf, p + 8 + 4 + 4 + 4 + 4 + 4, 2)  # Subsystem=GUI
    dd = opt + 112
    st.pack_into("<II", buf, dd + 8, 0x1000, 40)      # import dir, 2 desc
    sec = opt + 240
    buf[sec:sec + 8] = b".idata\x00\x00"
    st.pack_into("<IIII", buf, sec + 8, 0x1000, 0x1000, 0x1000, 0x200)

    base = 0x200          # file offset of RVA 0x1000
    cur = base + 60       # after 3 descriptors (2 real + terminator)

    def put_name(s):
        nonlocal cur
        off = cur
        buf[off:off + len(s) + 1] = s.encode() + b"\x00"
        cur += len(s) + 1
        if cur % 2:
            cur += 1
        return 0x1000 + (off - base)

    def put_hint_name(s):
        nonlocal cur
        off = cur
        st.pack_into("<H", buf, off, 0)
        buf[off + 2:off + 2 + len(s) + 1] = s.encode() + b"\x00"
        cur += 2 + len(s) + 1
        if cur % 2:
            cur += 1
        return 0x1000 + (off - base)

    def put_thunks(funcs):
        nonlocal cur
        off = cur
        rvas = [put_hint_name(f) for f in funcs]
        off = cur
        for r in rvas:
            st.pack_into("<Q", buf, cur, r)
            cur += 8
        st.pack_into("<Q", buf, cur, 0)
        cur += 8
        return 0x1000 + (off - base)

    descs = []
    for dll, funcs in (("KERNEL32.dll", ["GetTickCount64", "CreateFile2",
                                         "WaitOnAddress"]),
                       ("USER32.dll", ["CreateWindowExW"] +
                        list(extra_user32_funcs))):
        ilt = put_thunks(funcs)
        name_rva = put_name(dll)
        descs.append((ilt, name_rva))
    for i, (ilt, name_rva) in enumerate(descs):
        st.pack_into("<IIIII", buf, base + i * 20, ilt, 0, 0, name_rva, ilt)
    return bytes(buf)


def self_test():
    clean = audit("<synthetic-clean>", _synthetic_pe([]))
    if clean["violations"]:
        print("SELF-TEST FAIL: clean fixture flagged: %s"
              % clean["violations"])
        return 1
    if clean["subsystem_version"] != "6.1" or "AMD64" not in clean["machine"]:
        print("SELF-TEST FAIL: header parse wrong: %s" % clean)
        return 1
    dlls = [e["dll"] for e in clean["imports"]]
    if dlls != ["KERNEL32.dll", "USER32.dll"]:
        print("SELF-TEST FAIL: dll list wrong: %s" % dlls)
        return 1
    poisoned = audit("<synthetic-poisoned>",
                     _synthetic_pe(["GetDpiForWindow"]))
    hits = [v["function"] for v in poisoned["violations"]]
    if hits != ["GetDpiForWindow"]:
        print("SELF-TEST FAIL: poisoned fixture hits wrong: %s" % hits)
        return 1
    print("SELF-TEST PASS: clean fixture passes (KERNEL32.dll + "
          "USER32.dll, subsystem 6.1, GetTickCount64/CreateFile2/"
          "WaitOnAddress correctly ALLOWED as Windows <=8.0); "
          "poisoned fixture correctly flags GetDpiForWindow "
          "(Windows 10, version 1607).")
    return 0


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    flags = {a for a in argv[1:] if a.startswith("--")}
    if "--self-test" in flags:
        return self_test()
    if len(args) != 1:
        print(__doc__.split("Usage:")[1].split("Exit codes")[0]
              if __doc__ else "usage: pe-import-audit.py <pe-file>")
        return 2
    subsystem_is_failure = "--allow-newer-subsystem" not in flags
    try:
        r = audit(args[0])
    except (OSError, PEError) as e:
        print("ERROR: %s: %s" % (args[0], e))
        return 2
    if "--json" in flags:
        r2 = dict(r)
        r2["verdict"] = "FAIL" if (
            r["violations"] or (subsystem_is_failure and r["warnings"])
        ) else "PASS"
        print(json.dumps(r2, indent=2))
        return 0 if r2["verdict"] == "PASS" else 1
    return 1 if print_report(r, subsystem_is_failure) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
