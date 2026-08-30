//! Phase-0 probe for the DDA-side cursor compositor.
//!
//! Question this answers: with the VDD running `HardwareCursor=true`
//! (the OS does NOT paint the cursor into the framebuffer for that
//! virtual monitor), does `IDXGIOutputDuplication::AcquireNextFrame`
//! still surface the cursor's position and shape via
//! `DXGI_OUTDUPL_FRAME_INFO::PointerPosition` and `PointerShapeBufferSize`?
//!
//! If yes, we can keep the cursor as a pure overlay (no DWM compose
//! step on the virtual display) and blit it ourselves into the captured
//! frame just before encoding — saving one frame of compositor latency
//! that the current `HardwareCursor=false` workaround pays. If no, we
//! keep the workaround as-is.
//!
//! How to run:
//!   1. With the GUI built from this branch, enable the VDD (start a
//!      session — the probe doesn't manage the VDD lifecycle, it just
//!      observes whatever monitor you point it at).
//!   2. `cargo run -p penflow-core --example cursor_probe --release`
//!      (no args → lists monitors, exits)
//!      `cargo run -p penflow-core --example cursor_probe --release -- <idx>`
//!      where `<idx>` is the monitor index from the listing.
//!   3. Move your mouse over the virtual display. Watch stderr.
//!
//! What you want to see:
//!   - `pointer pos: visible=true (X,Y)` lines printed when the cursor is
//!     over the chosen monitor.
//!   - `shape buffer size` non-zero on cursor-shape changes (hovering
//!     edits → I-beam, etc.).
//!
//! Exit: Ctrl+C. The probe runs until killed.

use std::env;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use penflow_core::capture::dxgi::DxgiCapturer;
use penflow_core::d3d11::{create_dxgi_factory, D3d11Context};
use penflow_core::monitors;

fn main() -> ExitCode {
    let factory = match create_dxgi_factory() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("create_dxgi_factory failed: {e:?}");
            return ExitCode::from(2);
        }
    };
    let mons = match monitors::enumerate(&factory) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("enumerate monitors failed: {e:?}");
            return ExitCode::from(2);
        }
    };
    let attached: Vec<_> = mons.iter().filter(|m| m.attached_to_desktop).collect();
    if attached.is_empty() {
        eprintln!("no attached monitors");
        return ExitCode::from(2);
    }
    println!("[monitors]");
    for (i, m) in attached.iter().enumerate() {
        println!(
            "  [{i}] {} on {} ({}x{}) {}",
            m.device_name,
            m.adapter_name,
            m.width,
            m.height,
            if m.looks_virtual { "[virtual]" } else { "" }
        );
    }

    let args: Vec<String> = env::args().collect();
    let idx: usize = match args.get(1).and_then(|s| s.parse().ok()) {
        Some(i) => i,
        None => {
            println!();
            println!("Usage: cargo run --example cursor_probe -- <idx>");
            println!("Pick the [virtual] monitor index above.");
            return ExitCode::SUCCESS;
        }
    };
    let mon = match attached.get(idx) {
        Some(m) => (*m).clone(),
        None => {
            eprintln!("idx {idx} out of range (have {} entries)", attached.len());
            return ExitCode::from(2);
        }
    };
    println!(
        "[selected] {} {}x{} on {}",
        mon.device_name, mon.width, mon.height, mon.adapter_name
    );

    let adapter = match mon.open_adapter(&factory) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("open_adapter: {e:?}");
            return ExitCode::from(2);
        }
    };
    let ctx = match D3d11Context::create_on_adapter(adapter) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("d3d11 ctx: {e:?}");
            return ExitCode::from(2);
        }
    };
    let mut cap = match DxgiCapturer::new(ctx, mon) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("DxgiCapturer: {e:?}");
            return ExitCode::from(2);
        }
    };

    println!();
    println!("[probe] running. Move the mouse over the chosen monitor.");
    println!("[probe] Ctrl+C to exit.");
    println!();

    // Track previous values so we only print on change — at 120 fps the
    // DDA loop produces a lot of frames and a per-frame log is unreadable.
    let mut prev_visible: Option<bool> = None;
    let mut prev_pos: Option<(i32, i32)> = None;
    let mut last_shape_log: Option<Instant> = None;
    let mut frames_seen: u64 = 0;
    let mut frames_with_shape: u64 = 0;
    // Cursor-only accounting. This is the question that decides whether the
    // pipeline can keep sourcing the pointer from DDA at all: on a static
    // desktop, does moving the mouse produce frames (LastPresentTime == 0,
    // LastMouseUpdateTime != 0), and how far apart are they? Gaps in the
    // hundreds of ms mean the cursor is only carried by content frames,
    // which is exactly "mouse lags, pen doesn't".
    let mut cursor_only_frames: u64 = 0;
    let mut content_frames: u64 = 0;
    let mut last_pos_update: Option<Instant> = None;
    let mut gaps_ms: Vec<u64> = Vec::new();
    let start = Instant::now();
    let mut last_summary = start;

    loop {
        let acquired = match cap.acquire_frame(Duration::from_millis(200)) {
            Ok(opt) => opt,
            Err(e) => {
                eprintln!("[probe] acquire err: {e:?}");
                return ExitCode::from(2);
            }
        };
        if let Some(frame) = acquired {
            frames_seen += 1;
            if frame.is_cursor_only() {
                cursor_only_frames += 1;
            } else {
                content_frames += 1;
            }
            if frame.pointer_position().is_some() {
                let now = Instant::now();
                if let Some(prev) = last_pos_update {
                    gaps_ms.push(now.duration_since(prev).as_millis() as u64);
                }
                last_pos_update = Some(now);
            }
            let info = &frame.frame_info;
            let visible = info.PointerPosition.Visible.as_bool();
            let pos = (
                info.PointerPosition.Position.x,
                info.PointerPosition.Position.y,
            );

            if Some(visible) != prev_visible {
                println!(
                    "[probe] visible: {} -> {}  pos=({},{})",
                    prev_visible
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".into()),
                    visible,
                    pos.0,
                    pos.1
                );
                prev_visible = Some(visible);
            } else if visible && Some(pos) != prev_pos {
                // Throttle position prints to ~10 Hz so they're skimmable.
                if last_shape_log
                    .map(|t| t.elapsed() > Duration::from_millis(100))
                    .unwrap_or(true)
                {
                    println!("[probe] pos -> ({},{})", pos.0, pos.1);
                    last_shape_log = Some(Instant::now());
                    prev_pos = Some(pos);
                }
            }

            if info.PointerShapeBufferSize > 0 {
                frames_with_shape += 1;
                println!(
                    "[probe] *** PointerShapeBufferSize={} on this frame (shape changed) ***",
                    info.PointerShapeBufferSize
                );
            }
            // Drop the frame to release the duplication; we don't need pixels.
            drop(frame);
        }

        if last_summary.elapsed() > Duration::from_secs(5) {
            gaps_ms.sort_unstable();
            let pick = |q: f64| -> u64 {
                if gaps_ms.is_empty() {
                    0
                } else {
                    let i = ((gaps_ms.len() - 1) as f64 * q).round() as usize;
                    gaps_ms[i]
                }
            };
            println!(
                "[probe] (5s) frames={frames_seen} content={content_frames}                  cursor_only={cursor_only_frames} shape={frames_with_shape}                  pointer_updates={} gap_ms p50={} p95={} max={} elapsed={:.1}s",
                gaps_ms.len(),
                pick(0.50),
                pick(0.95),
                gaps_ms.last().copied().unwrap_or(0),
                start.elapsed().as_secs_f64()
            );
            gaps_ms.clear();
            last_summary = Instant::now();
        }
    }
}
