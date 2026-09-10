//! Hotspot probe: prints DDA's `PointerPosition`, `GetCursorPos`
//! (monitor-local) and the current shape's hotspot side by side.
//!
//! Answers whether DDA reports the bitmap's top-left (hotspot already
//! applied) or the hotspot point. Measured 2026-09-11 on a physical
//! monitor and on the VDD: `GetCursorPos - DDA == HotSpot` on both, so
//! the position IS the top-left and the compositor must not subtract the
//! hotspot again (it did until then, which drew Photoshop's centre-hotspot
//! brush ring above the pen).
//!
//! Run: `cargo run -p penflow-core --release --example hotspot_probe`
//! (lists monitors), then `-- <idx>` and move the mouse over that monitor.
use std::env;
use std::time::Duration;

use penflow_core::capture::dxgi::DxgiCapturer;
use penflow_core::d3d11::{create_dxgi_factory, D3d11Context};
use penflow_core::monitors;
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

fn main() {
    let factory = create_dxgi_factory().unwrap();
    let mons = monitors::enumerate(&factory).unwrap();
    let attached: Vec<_> = mons.iter().filter(|m| m.attached_to_desktop).collect();
    for (i, m) in attached.iter().enumerate() {
        println!(
            "[{i}] {} {}x{} rect={:?} virtual={}",
            m.device_name, m.width, m.height, m.desktop_coords, m.looks_virtual
        );
    }
    let idx: usize = match env::args().nth(1) {
        Some(s) => s.parse().unwrap(),
        None => return,
    };
    let mon = attached[idx].clone();
    let (ox, oy) = (mon.desktop_coords.0, mon.desktop_coords.1);
    let adapter = mon.open_adapter(&factory).unwrap();
    let ctx = D3d11Context::create_on_adapter(adapter).unwrap();
    let mut cap = DxgiCapturer::new(ctx, mon).unwrap();
    let (mut hx, mut hy) = (0i32, 0i32);
    let mut prev = None;
    loop {
        let acquired = match cap.acquire_frame(Duration::from_millis(200)) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("acquire {e:?}");
                return;
            }
        };
        if let Some(mut frame) = acquired {
            if let Ok(Some(s)) = frame.take_shape_update() {
                hx = s.hot_x;
                hy = s.hot_y;
                println!(
                    "SHAPE kind={:?} {}x{} hot=({},{})",
                    s.kind, s.width, s.height, hx, hy
                );
            }
            if let Some(p) = frame.pointer_position() {
                let mut pt = POINT::default();
                unsafe {
                    GetCursorPos(&mut pt).ok().unwrap();
                }
                let (gx, gy) = (pt.x - ox, pt.y - oy);
                let line = (p.x, p.y, gx, gy);
                if prev != Some(line) {
                    println!(
                        "DDA=({},{}) GetCursorPos=({},{}) diff=({},{}) hot=({},{}) visible={}",
                        p.x,
                        p.y,
                        gx,
                        gy,
                        gx - p.x,
                        gy - p.y,
                        hx,
                        hy,
                        p.visible
                    );
                    prev = Some(line);
                }
            }
        }
    }
}
