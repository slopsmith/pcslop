pcslop is free software under the GNU General Public License, version 2 or
later (pcslop (GPL-2.0).txt). The source code of every build is at
https://github.com/slopsmith/pcslop; About shows the commit this one was
built from. The versions and build options of everything below are in
flake.nix and flake.lock in pcslop's source at the same commit.

This app also contains:

- SeaBIOS and SeaVGABIOS (bios.bin, vgabios.bin), under the GNU Lesser
  General Public License, version 3 (LGPL-3.0.txt, which builds on
  GPL-3.0.txt). Source code: https://www.seabios.org, with pcslop's patch in
  nix/seabios-i440bx.patch.
- GLib (libglib-2.0) and GNU gettext's libintl, under the GNU Lesser General
  Public License, version 2.1 or later (LGPL-2.1.txt). Source code:
  https://download.gnome.org/sources/glib/ and https://ftp.gnu.org/gnu/gettext/.
- PCRE2 (libpcre2-8), under the BSD 3-Clause license with the PCRE2
  exception (PCRE2 (BSD-3-Clause).txt).
- libslirp, under the BSD 3-Clause license (libslirp (BSD-3-Clause).txt).
- Rust crates, each under its own license, listed in Rust crates.txt.
- System logos shown next to machines. They are trademarks of their owners.
  The Windows, Ubuntu, Debian and Fedora logos are public domain on Wikimedia
  Commons. Tux is by Larry Ewing (lewing@isc.tamu.edu), drawn with The GIMP.
