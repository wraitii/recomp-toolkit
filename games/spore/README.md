# Spore (2008)

Work in progress on a native port of the GOG **SPORE Collection**, using the
Galactic Adventures 3.1.0.22 executable. Performance work is ongoing.
Networking is currently unsupported across the toolkit.

## Status

The executable is pinned to `SporebinEP1/SporeApp.exe` (Galactic Adventures
3.1.0.22), the executable GOG launches for the collection. It loads the base
`Data/` content together with `DataEP1/`; `SporeBin/SporeApp.exe` is the base-game
fallback and is not the target.

The port fixes an original Creature Stage crash that can occur when the player
dies while a creature is mid-attack.

For build instructions and technical notes, see [DEVELOPMENT.md](https://github.com/wraitii/recomp-toolkit/blob/main/games/spore/DEVELOPMENT.md).
