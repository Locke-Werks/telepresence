#!/usr/bin/env python3
"""Make TelePresence.app self-contained.

Copies the GStreamer plugins and plugin scanner into the bundle, then walks
every Mach-O in it and pulls in each dylib it links that lives outside the
bundle (GStreamer.framework, Homebrew, /usr/local). Copied dylibs go flat into
Contents/Frameworks under an @rpath/<name> install name, and every Mach-O gets
exactly one LC_RPATH pointing at Contents/Frameworks.

Run after macdeployqt, which has already placed Qt's frameworks and plugins.

    bundle_deps.py <App.app> <gst-root> <plugin> [<plugin> ...]
    bundle_deps.py --verify <App.app>

<gst-root> is .../GStreamer.framework/Versions/1.0. A plugin is the short name,
e.g. "coreelements" for lib/gstreamer-1.0/libgstcoreelements.dylib.
"""

import os
import shutil
import struct
import subprocess
import sys

MACHO_MAGICS = {
    0xFEEDFACE, 0xCEFAEDFE, 0xFEEDFACF, 0xCFFAEDFE,  # thin 32/64, both endians
    0xCAFEBABE, 0xBEBAFECA,                          # fat
}

# Anything referencing these after bundling would only work on the build box.
FORBIDDEN = ("/Library/Frameworks/", "/opt/homebrew/", "/usr/local/", "/Users/")


def is_macho(path):
    if os.path.islink(path) or not os.path.isfile(path):
        return False
    with open(path, "rb") as f:
        head = f.read(4)
    if len(head) < 4:
        return False
    magic = struct.unpack(">I", head)[0]
    if magic in (0xCAFEBABE, 0xBEBAFECA):
        # Java class files share the fat magic; the next word tells them apart.
        with open(path, "rb") as f:
            f.seek(4)
            nfat = struct.unpack(">I", f.read(4))[0]
        return 0 < nfat < 20
    return magic in MACHO_MAGICS


def run(*args):
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def load_commands(path):
    """Return (install_id, rpaths, deps) for a Mach-O, merged across slices."""
    out = run("otool", "-l", path)
    install_id, rpaths, deps = None, [], []
    cmd = None
    for line in out.splitlines():
        s = line.strip()
        if s.startswith("cmd "):
            cmd = s.split()[1]
        elif s.startswith("name ") and cmd in ("LC_LOAD_DYLIB", "LC_LOAD_WEAK_DYLIB",
                                               "LC_REEXPORT_DYLIB", "LC_LAZY_LOAD_DYLIB"):
            name = s[5:].rsplit(" (offset", 1)[0]
            if name not in deps:
                deps.append(name)
        elif s.startswith("name ") and cmd == "LC_ID_DYLIB":
            install_id = s[5:].rsplit(" (offset", 1)[0]
        elif s.startswith("path ") and cmd == "LC_RPATH":
            p = s[5:].rsplit(" (offset", 1)[0]
            if p not in rpaths:
                rpaths.append(p)
    return install_id, rpaths, deps


def framework_root(path):
    """/x/QtPdf.framework/Versions/A/QtPdf -> /x/QtPdf.framework, else None.

    Only a framework's own binary counts. GStreamer.framework/.../lib/*.dylib
    are ordinary dylibs that happen to live in a framework and are copied flat;
    copying that framework whole would drag in all of GStreamer.
    """
    parts = path.split(os.sep)
    for i in range(len(parts) - 1, -1, -1):
        if parts[i].endswith(".framework"):
            name = parts[i][: -len(".framework")]
            rest = parts[i + 1:]
            if rest == [name] or (len(rest) == 3 and rest[0] == "Versions" and rest[2] == name):
                return os.sep.join(parts[: i + 1])
            return None
    return None


def is_system(dep):
    return dep.startswith("/usr/lib/") or dep.startswith("/System/")


class Bundler:
    def __init__(self, app, gst_root):
        self.app = os.path.abspath(app)
        self.contents = os.path.join(self.app, "Contents")
        self.frameworks = os.path.join(self.contents, "Frameworks")
        self.exe_dir = os.path.join(self.contents, "MacOS")
        self.gst_root = gst_root
        # Where each bundled file came from, so its own @loader_path rpaths can
        # still be expanded against the original location.
        self.origin = {}
        self.fallback_dirs = [os.path.join(gst_root, "lib")] if gst_root else []
        # Homebrew splits Qt across kegs, so macdeployqt misses frameworks that
        # live outside qtbase. They are all linked into /opt/homebrew/lib.
        qt_libs = os.environ.get("QT_INSTALL_LIBS")
        if qt_libs:
            self.fallback_dirs.append(qt_libs)
        for d in ("/opt/homebrew/lib", "/usr/local/lib"):
            if os.path.isdir(d):
                self.fallback_dirs.append(d)

    def expand(self, token_path, loader_dir):
        p = token_path.replace("@loader_path", loader_dir)
        p = p.replace("@executable_path", self.exe_dir)
        return os.path.normpath(p)

    def resolve(self, dep, path, rpaths):
        """Find the file a dependency refers to. Returns an absolute path or None."""
        here = os.path.dirname(path)
        src_dir = os.path.dirname(self.origin.get(path, path))
        if dep.startswith("@rpath/"):
            tail = dep[len("@rpath/"):]
            candidates = []
            for rp in rpaths:
                for base in (here, src_dir):
                    candidates.append(os.path.join(self.expand(rp, base), tail))
            candidates.append(os.path.join(self.frameworks, tail))
            candidates.append(os.path.join(self.frameworks, os.path.basename(tail)))
            if self.gst_root:
                candidates.append(os.path.join(self.gst_root, tail))
            for d in self.fallback_dirs:
                candidates.append(os.path.join(d, tail))
                candidates.append(os.path.join(d, os.path.basename(tail)))
        elif dep.startswith("@loader_path/") or dep.startswith("@executable_path/"):
            candidates = [self.expand(dep, here), self.expand(dep, src_dir)]
        else:
            candidates = [dep]
        for c in candidates:
            if os.path.exists(c):
                return os.path.realpath(c) if not c.startswith(self.app) else c
        return None

    def inside(self, p):
        return os.path.abspath(p).startswith(self.app + os.sep)

    def rpath_for(self, path):
        rel = os.path.relpath(self.frameworks, os.path.dirname(path))
        return "@loader_path" if rel == "." else "@loader_path/" + rel

    def copy_in(self, src, dst_dir):
        dst = os.path.join(dst_dir, os.path.basename(src))
        if not os.path.exists(dst):
            os.makedirs(dst_dir, exist_ok=True)
            shutil.copy2(src, dst)
            os.chmod(dst, 0o755)
            self.origin[dst] = src
        elif dst in self.origin and os.path.realpath(self.origin[dst]) != os.path.realpath(src):
            # Two different libraries with one name (Homebrew glib next to
            # GStreamer's glib, say). The first copy wins for both.
            print(f"warning: {os.path.basename(src)} from {src} merged with {self.origin[dst]}",
                  file=sys.stderr)
        return dst

    def all_machos(self):
        for root, _dirs, files in os.walk(self.app):
            for f in files:
                p = os.path.join(root, f)
                if is_macho(p):
                    yield p

    def fix(self, path):
        install_id, rpaths, deps = load_commands(path)
        changes = []
        for dep in deps:
            if is_system(dep):
                continue
            target = self.resolve(dep, path, rpaths)
            if target is None:
                raise SystemExit(f"error: {path}: cannot resolve {dep}")
            if self.inside(target):
                # Already in the bundle. Leave framework-style refs alone; flat
                # dylibs are normalised to @rpath/<name>.
                if os.path.dirname(target) == self.frameworks and dep != "@rpath/" + os.path.basename(target):
                    changes += ["-change", dep, "@rpath/" + os.path.basename(target)]
                continue
            fw = framework_root(target)
            if fw:
                # Frameworks are copied whole and keep their own layout.
                inner = os.path.relpath(target, os.path.dirname(fw))
                dst = os.path.join(self.frameworks, os.path.basename(fw))
                if not os.path.exists(dst):
                    shutil.copytree(fw, dst, symlinks=True,
                                    ignore=shutil.ignore_patterns("Headers", "*.prl", "*_debug*"))
                    self.origin[os.path.join(self.frameworks, inner)] = target
                    self.queue.append(os.path.join(self.frameworks, inner))
                if dep != "@rpath/" + inner:
                    changes += ["-change", dep, "@rpath/" + inner]
                continue
            copied = self.copy_in(target, self.frameworks)
            self.queue.append(copied)
            changes += ["-change", dep, "@rpath/" + os.path.basename(copied)]

        if install_id is not None and os.path.dirname(path) == self.frameworks:
            want = "@rpath/" + os.path.basename(path)
            if install_id != want:
                changes += ["-id", want]

        want_rpath = self.rpath_for(path)
        for rp in rpaths:
            if rp != want_rpath:
                changes += ["-delete_rpath", rp]
        if want_rpath not in rpaths:
            changes += ["-add_rpath", want_rpath]

        if changes:
            subprocess.run(["install_name_tool", *changes, path], check=True,
                           stderr=subprocess.PIPE)

    def bundle(self, plugins):
        plugin_dir = os.path.join(self.contents, "PlugIns", "gstreamer")
        missing = []
        for name in plugins:
            src = os.path.join(self.gst_root, "lib", "gstreamer-1.0", f"libgst{name}.dylib")
            if not os.path.exists(src):
                missing.append(src)
                continue
            self.copy_in(src, plugin_dir)
        if missing:
            raise SystemExit("error: GStreamer plugins not found:\n  " + "\n  ".join(missing))
        scanner = os.path.join(self.gst_root, "libexec", "gstreamer-1.0", "gst-plugin-scanner")
        self.copy_in(scanner, self.exe_dir)

        self.queue = list(self.all_machos())
        seen = set()
        while self.queue:
            p = self.queue.pop()
            if p in seen:
                continue
            seen.add(p)
            self.fix(p)


def verify(app):
    """Fail if any Mach-O still points outside the bundle or has a dangling @rpath."""
    b = Bundler(app, None)
    b.fallback_dirs = []
    bad = []
    count = 0
    for p in b.all_machos():
        count += 1
        _id, rpaths, deps = load_commands(p)
        for item in rpaths + deps:
            if not is_system(item) and any(f in item for f in FORBIDDEN):
                bad.append(f"{p}: references {item}")
        for rp in rpaths:
            if not rp.startswith("@"):
                bad.append(f"{p}: absolute rpath {rp}")
        for dep in deps:
            if is_system(dep):
                continue
            if dep.startswith("@rpath/"):
                tail = dep[7:]
                ok = any(os.path.exists(os.path.join(b.expand(rp, os.path.dirname(p)), tail))
                         for rp in rpaths)
                if not ok:
                    bad.append(f"{p}: {dep} does not resolve inside the bundle")
            elif dep.startswith("@"):
                if not os.path.exists(b.expand(dep, os.path.dirname(p))):
                    bad.append(f"{p}: {dep} does not resolve inside the bundle")
            else:
                bad.append(f"{p}: absolute non-system dependency {dep}")
    if bad:
        print("\n".join(bad), file=sys.stderr)
        raise SystemExit(f"verify: {len(bad)} problem(s) in {count} Mach-O files")
    print(f"verify: {count} Mach-O files, all dependencies inside the bundle or the OS")


def main(argv):
    if len(argv) >= 2 and argv[0] == "--verify":
        verify(argv[1])
        return
    if len(argv) < 3:
        raise SystemExit(__doc__)
    app, gst_root, plugins = argv[0], argv[1], argv[2:]
    Bundler(app, gst_root).bundle(plugins)
    verify(app)


if __name__ == "__main__":
    main(sys.argv[1:])
