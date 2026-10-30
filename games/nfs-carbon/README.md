# Need for Speed: Carbon

The game config and code map currently live in the parent project's `metadata/`.

`native/motion_blur.c` wraps the blur calculator at `0x7133C0` and renderer at
`0x726430`. `DIVERGENCE(original):` with Motion Blur enabled, blur fades in and
out over 250 ms of monotonic real time around Speedbreaker flag `0xA9922C`.
Verified CE 1.4 disassembly shows `0x693C50` writes that flag, `0x726430` checks
the calculator's `AL` result, and `0x726854` reads effect field `+0xB8` for
`composite_blur`. The wrapper scales that strength during the blur pass and
restores its exact original bits afterward for shared effects. Original speed
thresholds, resource allocation and capability checks remain; thresholds can
still end the effect before the fade completes. Set
`RECOMP_MOTION_BLUR_ORIGINAL=1` to retain original behavior. The user visually
confirmed both the Speedbreaker-only behavior and the 250 ms fade.
No options-menu changes are included.
