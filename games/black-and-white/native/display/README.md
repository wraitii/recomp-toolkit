# Display options

`display_options.c` wraps `start_system` (0x00642d80),
`MiniDialogBoxOptions::Init` (0x00514f50),
`MiniDialogBoxOptions::InitControls` (0x00515690), and
`DialogBoxOptions::ControlCallback` (0x005140c0). Set
`RECOMP_DISPLAY_ORIGINAL=1` to retain the translated originals without seeding.

DIVERGENCE(original): missing ScreenW/H/D values are seeded to 1024/768/32
before startup reads them. Existing DWORDs remain intact. Selecting a resolution
saves ScreenW/H and ScreenD=32 on callback event 7, for the next launch.
The restart notice is always visible. The selector enumerates the shim's actual
32-bit modes at least 1024x768, including RECOMP_DDRAW_MODES, sorted by width
then height. Without an override, the host screen size in points, pixels and
half-size pixels is offered as well. The shared DirectDraw mode list includes
common 4:3, 5:4, 16:9 and 16:10 laptop sizes. It shows existing custom dimensions
without rounding or replacing them. The label advances; the arrows cycle both ways. Tab/Shift-Tab use the original
focus list; Enter, Space and Left/Right activate the focused selector.

Ghidra evidence: 0x00409250 registers each control with the active SetupBox
(+0x234 owner, +0x230 next, box +0x6c list); 0x004098b0 and 0x0040d260
construct the existing detail button and arrows. The 0x00515690 wrapper moves the
audio-enabled video heading/detail row from y=280/350 to y=250/320, with the
resolution at y=370 and smaller restart notice at y=408. Without audio, the
original detail row stays at y=120; the additions follow at y=170/208.
The original checkboxes remain at y=450. 0x005140c0 uses event 1 for
activation and event 7 for saving; other events retain the original callback.
The registry seam puts DWORDs before strings because the original reader stops
when an enumerated value exceeds its DWORD buffer.

Arrow evidence: the bytes at 0x009cee0c contain a single-space caption, not an
empty string. 0x0040ceb0 highlights a focused arrow only with an empty caption;
matching the original caption removes that persistent highlight. The new arrows
also use the original 32-unit size.

Keyboard evidence: 0x00408f80 routes scan codes through callback event 2 and
0x00411090 skips controls marked nonfocusable. 0x004093c0 unlinks destroyed
controls and clears focus. The callback finds its label through the live box
list before saving, avoiding stale label dereferences.

Remaining gaps: visual spacing, keyboard behavior in-game, localization (new strings
are English), full control destruction, and 32-bit game/UI rendering require
in-game verification. The user verified the first version in-game; this iteration was built without
launching the game. A reported one-time reset to minimum settings was ambiguous
and has no established cause; resolution writes only touch ScreenW/H/D.
