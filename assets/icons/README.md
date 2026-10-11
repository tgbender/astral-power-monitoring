# GPU monitor icons

Original, mathematically drawn GPU icons under the repository Unlicense. The
normal icon is teal with a white fan/lightning bolt and six gold power contacts.
The warning icon uses amber with an exclamation triangle, so warning state has
a distinct shape as well as color. Both have transparent, antialiased edges.

ICO files include 16, 20, 24, 32, 40, 48, 64, and 256 pixel images. Windows uses
the appropriate small image for the tray; the executable, taskbar, and startup
shortcut share the normal icon. The UI loads three icon handles once and frees
them on exit rather than allocating a new image on every tray update.

Reproduce from the repository root:

```powershell
rustc --edition 2024 tools/generate_icons.rs -o dist/generate-icons.exe
& .\dist\generate-icons.exe
```

The application build uses Windows SDK `rc.exe` from PATH to embed the assets.
