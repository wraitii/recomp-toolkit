# Recomp Toolkit

Build native versions of supported classic PC games from your own copies.
Recomp Toolkit checks your game files, recompiles the game and packages it as
a native app for your computer. Launch each game's app directly, with its own
settings and game files.

The desktop app handles setup and builds, with a library view for convenience.

## Platforms

Apple silicon (ARM) Macs are the main target. Recompiling these 32-bit x86 games
as native ARM apps avoids reliance on Rosetta as Apple
[phases out its general-purpose support](https://support.apple.com/en-gb/102527).

Intel Mac, Windows and Linux targets are available through source builds but
are untested.

### Optimizing for x86

Better performance on x86 hosts is a goal, likely by retaining most of the
original x86 code and rewriting only where necessary. This work is not yet
implemented. Wine likely outperforms the recompilation on these hosts today,
though the toolkit's graphics backend may offer advantages. These performance
comparisons have not been benchmarked.

## Get started

[Download Recomp Toolkit](https://github.com/wraitii/recomp-kit/releases/tag/latest-nightly)
for Apple silicon Macs running macOS 14 or later. You'll need your own copy of
one of the games below and Apple's Command Line Tools (`xcode-select --install`).
The app includes the other build dependencies; GOG installer imports also need
`innoextract` 1.9 or later (`brew install innoextract` with Homebrew).

1. Open Recomp Toolkit and choose a game.
2. Choose your installed game folder, a ZIP of it, or a supported GOG installer.
   The app checks that your copy matches the required version.
3. Choose where to install the native app. Follow any preparation instructions,
   then choose **Install**. Building happens on your computer and can take
   several minutes or longer; leave room for the game files and temporary builds.
4. Open the finished game's app, or choose **Start game** in Recomp Toolkit.

## Games

The testing labels below describe single-player gameplay testing coverage.
Networking is currently unsupported across the toolkit. Each game's page lists the required
release and preparation steps.

| Game | Status | Required version |
| --- | --- | --- |
| [Black & White](games/black-and-white/README.md) | Playable (lightly tested) | CD with Fan Patch 1.42 |
| [Tom Clancy's Ghost Recon](games/ghost-recon/README.md) | Playable (well tested) | GOG 1.4 |
| [Civilization IV: Beyond the Sword](games/civ4-bts/README.md) | Playable (lightly tested) | GOG 3.19 |
| [4x4 Evolution 2](games/4x4-evo2/README.md) | Playable (lightly tested) | PC 1.00.139 |
| [Railroad Tycoon 3](games/railroad-tycoon-3/README.md) | Playable (well tested) | GOG 1.05 |
| [Spore](games/spore/README.md) | Playable (lightly tested) | GOG Collection, Galactic Adventures 3.1.0.22 |

Performance is generally good, with occasional slowdowns still being worked on,
mostly in Civilization IV and Spore. Game execution is currently single-threaded:
the game's threads run one at a time. This has little impact on the games ported
so far. Rare crashes from incorrect code mappings are also possible.

## Your installed apps

**Game files.** You supply the games; downloads contain no commercial game files.
The app copies your files into the destination you choose. Keep the whole
installed folder together, including the game app, its Configure app,
`original/` and `.recomp-toolkit/`.

**Installers and patches.** Supported GOG Inno installers can be extracted
without running them. Keep accompanying BIN files beside the installer.
Other installers and disc images need preparation outside Recomp Toolkit;
select the resulting installed folder afterward. Apply any required patches
before importing, including Black & White's Fan Patch 1.42.

**Settings.** Open the installed game's Configure app to adjust its settings
and controller mappings. Launch the game directly whenever you want to play;
Recomp Toolkit doesn't need to be open.

**Trouble installing?** Check the required version on the game's page. If build
tools are missing, follow the app's instructions and choose **Check again**.
For a failed installation, expand **Installation details** and use **Try again**
after resolving the reported problem. Include the game version and installation
log when [reporting an issue](https://github.com/wraitii/recomp-kit/issues).

## How it works

Static recompilation translates the original game's 32-bit x86 Windows machine
code into native code ahead of time. The toolkit supplies the operating-system,
graphics and audio interfaces the game expects. Translated code runs natively,
with a p-code interpreter for missing code and no JIT. Each port uses your game's
original data and translation metadata for the exact executable it supports.

### How this differs from Wine

Wine and bottle-based ports run the original Windows executable through a
compatibility layer. The toolkit instead recompiles it for your CPU, allowing
32-bit x86 games to run natively on other architectures—for example, on ARM
without an x86 translation layer. Its own runtime and graphics adapters focus
on Direct3D 7–9-era games, using modern backends such as Vulkan and Metal.
Both approaches support game-specific workarounds, but the kit also makes
individual game functions directly replaceable
with native code for deeper optimizations and new features. This requires dedicated
work for each supported version; performance and compatibility depend on the port.

## Development and porting

The desktop app is the recommended way to install supported games. To work on
the toolkit or bring another game to it, start with [Contributing](CONTRIBUTING.md):

- [Build from source](CONTRIBUTING.md#prerequisites), including the desktop app.
- [Port a new game](CONTRIBUTING.md#add-a-game).
- Explore the [architecture](docs/architecture.md), [code guide](docs/code-guide.md)
  and [translator](docs/translator.md).

Source build targets also include Intel Macs, Windows, Linux, iOS and Android;
these are untested.
The packaged desktop app targets Apple silicon Macs.
