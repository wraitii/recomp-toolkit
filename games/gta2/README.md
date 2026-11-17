# Grand Theft Auto 2 (1999)

Native port of **Grand Theft Auto 2** (DMA Design / Rockstar Games), using the free
Rockstar Classics release of 2004. The intro movie, menus and gameplay work, with keyboard
controls and sound, at 640×480 on the Direct3D renderer.

The menu's **Options** opens the separate GTA2 Manager program in the original; the port does
not run it, so Options leaves you in the menu. Multiplayer (DirectPlay) is untested.

Ctrl+Option+M releases mouse capture.

## The game files

Rockstar distributed GTA2 for free as `GTA2INSTALLER.ZIP`, which holds an InstallShield 2K2
installer (`GTA2.exe`). On Windows, run the installer and select the installed folder. On a Mac:

1. Unzip `GTA2INSTALLER.ZIP`.
2. The installer's files are stored, uncompressed, after its executable: each entry is a file
   name, relative path, version and decimal size, each NUL-terminated, then the bytes. Extract
   them to get `Disk1/` (`data1.cab`, `data1.hdr`, `data2.cab`, …).
3. `brew install unshield`, then `unshield -d install x Disk1/data1.cab`.
4. `install/App_Executables/` is the game folder: `gta2.exe`, `d3ddll.dll`, `Dmavideo.dll` and
   `data/`.

Setup checks these files against `game.toml`:

| File | SHA-256 |
| --- | --- |
| `gta2.exe` | `420af867036b1a66ef94c54b8db2e7e654da359031987c6602bcfa74972e089f` |
| `d3ddll.dll` | `c2567d616617039caa2dd609568aaa51c9dbaa6f994a2d241df7e68b7a815666` |
| `Dmavideo.dll` | `f5eb2e2f8904d47bba0c095bcdc6764a53e186c3297338898c57e907c395ce1d` |

The registry defaults in `game.toml` stand in for GTA2 Manager's first-run settings: the Direct3D
renderer (`d3ddll.dll`) on the DirectDraw video driver (`dmavideo.dll`) at 640×480.

For build instructions and technical notes, see [DEVELOPMENT.md](DEVELOPMENT.md).
