# AGENTS.md

This is an ESP32-C3 Rust firmware project for a small SSD1306 128x64 OLED desk hologram clock.

Key context:

- Main firmware entry point: `src/bin/main.rs`.
- Shared app/rendering code: `src/lib.rs`, `src/render.rs`, `src/weather.rs`, `src/net.rs`.
- The OLED is viewed through a dichroic cube, so final display orientation matters. Check `src/render.rs` before changing coordinate transforms.
- The renderer is intentionally integer-only and `no_std` friendly. Avoid floats and heap-heavy drawing paths.
- The display is tiny and monochrome. Favor one readable primary time plus simple depth cues over dense text or noisy decorations.
- The mascot should stay small and out of the main clock area.
- Build verification is `cargo build`. `cargo test` is not useful for this firmware target because the Rust test harness is unavailable.

When making changes, keep edits scoped and preserve the existing embedded-graphics style.
