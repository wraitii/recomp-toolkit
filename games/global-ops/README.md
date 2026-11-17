# Global Operations (2002)

Native port of **Global Operations** (Barking Dog Studios / Crave, LithTech Talon), using the
unofficial **Community Update 4.0**. Single player is playable: every campaign mission loads,
with keyboard and mouse, 2D and 3D sound, resolutions up to 1440×1080 and high textures. The intro
movie plays. Online play works as a client: the server list comes from OpenSpy, and joining
servers that do not verify CD keys works.

Hosting does not work yet. The Host tab's START button launches the separate dedicated server
(`goserver.exe`), which is not ported, and closes the game. Voice chat is disabled.

Ctrl+Option+M releases mouse capture. Menus use the normal cursor; in a mission the mouse turns the
view.

## The game files

Every official executable (EU disc 1.08g, patch 1.1, patch 2.0) is SafeDisc protected and cannot be
translated. The port targets Community Update 4.0, whose `Globalops.exe` is the 2.0 binary without
SafeDisc, extended with the update's own code. You need your own copy of the game on disc, plus the
update:

- **Disc:** Global Operations, Europe (En, Fr, De). Other releases are untested.
- **Community Update 4.0:** `glopsus4_0.msi`, preserved in the
  [Global Operations mod archive](https://archive.org/details/global-operations-mod-archive) on the
  Internet Archive.

On Windows, install from the disc and then run the update installer. On a Mac, build the same folder
from the disc and the update (`brew install unshield msitools`):

1. Extract the disc's InstallShield data: `unshield -d install x /Volumes/<disc>/Setup/data1.cab`.
   With a `.bin/.cue` image, convert it to an ISO first and extract that with `bsdtar`.
2. Extract the update: `msiextract -C update glopsus4_0.msi`.
3. Create the game folder, `Global Operations`, and fill it in this order:
   - `install/Data_Files/*` → `Global Operations/globalops/`
   - `install/Rez/globalops.rez` → `Global Operations/`
   - `install/Movies/` → `Global Operations/Movies/`, and also copy `GO.bik` and `binkplay.exe` into
     `Global Operations/` (the game looks for the intro there)
   - everything in `update/ProgramFiles64Folder/Crave/Global Operations/` → `Global Operations/`,
     replacing what is there.

Do not use the disc's `SafeDisc_-_Executable` component. Setup checks these files against
`game.toml`:

| File | SHA-256 |
| --- | --- |
| `Globalops.exe` | `c95c4a6feb2cda2afbb20a5629bf527438b11971946d29e9a5b140c7df5f64cb` |
| `globalops/cshell.dll` | `f5341c625bbb6266bbd6c628cef9862b148f5d84fa9adb02f35bdedca013ba16` |
| `globalops/object.lto` | `073aca7da30149c3c57fa416e1330b04a1226c877484431bc6b076696c1425a1` |
| `globalops/CRes.dll` | `f079ae7a130af3080cfd5f328fea19fdd70e73490e87ac1abea4f74609221d6c` |
| `ltmsg.dll` | `94d0b839fb97e41cb5a22996267a7519e87372219fec9e01ed3e9adc30a698f4` |

## Online play

Multiplayer → NET → UPDATE lists the internet servers through OpenSpy, the community replacement for
GameSpy that the 4.0 update points the game at. LAN lists local games and Specify joins by address.

Joining needs a CD key in the registry: the game hashes it into its join request and refuses to
connect without one. The original disc installer stored it; the 4.0 update does not. Add your key to
the port's registry file, `registry.json` in the app's profile folder
(`~/Library/Application Support/GlobalOpsRecomp/`, or `build/recomp/` for a developer build):

```json
{
  "HKEY_LOCAL_MACHINE\\Software\\Electronic Arts\\EA Games\\Global Operations\\ergc": {
    "": {"type": 1, "data": "XXXX-XXXXXXX-XXXXXXX-XXXX", "seq": 1}
  }
}
```

For build instructions and technical notes, see [DEVELOPMENT.md](DEVELOPMENT.md).
