//! DXGI Output Duplication wrapper (Windows-only).
//!
//! References:
//!   - design.md §6.1 ("Capture Layer")
//!   - HANDOFF.md §1.2 (Sunshine MUST-adopt tricks)
//!   - HANDOFF.md §4.4b (DPI-awareness, multi-format DDA list)
//!
//! Sunshine `display_base.cpp` figured out most of the operational pitfalls
//! ten years ago; the comments below cite the specific tricks rather than
//! re-deriving them.

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Foundation::{DXGI_STATUS_OCCLUDED, E_INVALIDARG, S_OK, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Graphics::Dxgi::{
    Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R8G8B8A8_UNORM,
    },
    IDXGIOutput, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_DENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};
use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED};

use super::cursor_shape::{decode_shape, CursorShape};
use crate::d3d11::D3d11Context;
use crate::error::{EngineError, EngineResult};
use crate::monitors::MonitorInfo;

// SAFETY: All COM objects inside live on a single thread (the pipeline
// capture thread). DDA + D3D11 device with SetMultithreadProtected serialise
// access. Send is the move from the main thread that constructed the
// capturer to the pipeline thread; never &-shared across threads.
unsafe impl Send for DxgiCapturer {}

/// Holds an `IDXGIOutputDuplication` against a specific output, with
/// transparent recovery from `DXGI_ERROR_ACCESS_LOST` /
/// `DXGI_ERROR_ACCESS_DENIED`.
///
/// The recovery is a RETRY LOOP, never a single attempt (the UAC bug): a
/// secure-desktop switch — any UAC prompt, Ctrl+Alt+Del, the lock screen —
/// takes the duplication away with `ACCESS_LOST`, and while that desktop
/// is up `DuplicateOutput` itself fails (`E_ACCESSDENIED`, "the
/// application does not have access to the current desktop"; measured
/// `DXGI_ERROR_INVALID_CALL` on the NVIDIA output of the same switch). A
/// reinit that returned that error killed the capture thread: the tablet
/// kept the last frame, cursor baked in where it was, and the pen moved
/// nothing on screen until the session was restarted. Now a lost
/// duplication is re-created on later ticks (`ReinitState`, 100 ms
/// doubling to 1 s between attempts), the loop re-encodes the keepalive
/// meanwhile, and the first frame of the fresh duplication carries the
/// cursor shape again (DDA sends it on a new duplication's first frame).
/// The secure desktop itself is never captured — Windows denies that to
/// anything but SYSTEM — so the tablet shows the last frame while a
/// prompt is up, and comes back the moment it closes.
pub struct DxgiCapturer {
    /// Re-opened on every reinit (the duplication is bound to a specific
    /// IDXGIOutput instance; if the desktop session changes, we re-EnumOutputs).
    monitor: MonitorInfo,
    /// The D3D11 context whose device backs this duplication. The output
    /// MUST belong to the same adapter as `ctx.adapter` — checked in `new`.
    ctx: D3d11Context,
    /// `None` while the duplication is lost and not yet re-created.
    duplication: Option<IDXGIOutputDuplication>,
    reinit: ReinitState,
    width: u32,
    height: u32,
    /// True iff a frame is currently held (between `acquire` and `release`).
    /// IDXGIOutputDuplication forbids re-acquiring while one is held.
    frame_held: bool,
    /// True iff we successfully called `SetThreadExecutionState` with
    /// `ES_DISPLAY_REQUIRED` and need to clear it on drop.
    display_required: bool,
}

/// First wait between two re-creation attempts of a lost duplication.
const REINIT_DELAY_MIN: Duration = Duration::from_millis(100);
/// Ceiling of the doubling wait. A UAC prompt is answered in seconds; a
/// lock screen can stay up for hours, and one attempt a second is nothing.
const REINIT_DELAY_MAX: Duration = Duration::from_secs(1);

/// Where the capturer is in re-creating a lost duplication.
#[derive(Clone, Copy, Debug)]
struct ReinitState {
    /// When the duplication was lost; `None` while it is healthy.
    lost_at: Option<Instant>,
    /// Earliest time of the next attempt.
    next_attempt: Instant,
    /// Wait to schedule after the next failure.
    delay: Duration,
    /// Failed attempts since `lost_at`.
    attempts: u32,
}

impl ReinitState {
    fn healthy() -> Self {
        Self {
            lost_at: None,
            next_attempt: Instant::now(),
            delay: REINIT_DELAY_MIN,
            attempts: 0,
        }
    }

    /// Record a failed attempt at `now` and schedule the next one.
    fn failed(&mut self, now: Instant) {
        self.attempts += 1;
        self.next_attempt = now + self.delay;
        self.delay = next_reinit_delay(self.delay);
    }
}

/// The wait after one more failed re-creation: double it, capped at
/// `REINIT_DELAY_MAX`.
fn next_reinit_delay(prev: Duration) -> Duration {
    (prev * 2).min(REINIT_DELAY_MAX)
}

/// `true` for the `AcquireNextFrame` results that mean "this duplication
/// is gone, make a new one" rather than "no frame yet" or "fatal".
fn is_duplication_lost(code: windows::core::HRESULT) -> bool {
    code == DXGI_ERROR_ACCESS_LOST
        || code == DXGI_ERROR_ACCESS_DENIED
        || code == DXGI_STATUS_OCCLUDED
        || code == DXGI_ERROR_INVALID_CALL
}

/// One acquired DDA frame. Drops automatically release the duplication so the
/// next `acquire_frame` call works.
pub struct AcquiredFrame<'a> {
    pub texture: ID3D11Texture2D,
    pub captured_at: Instant,
    pub frame_info: DXGI_OUTDUPL_FRAME_INFO,
    capturer: &'a mut DxgiCapturer,
}

/// Cursor screen position reported alongside one DDA frame.
///
/// `visible == false` means the OS thinks the cursor is on a different
/// monitor, or hidden — the compositor should skip the blit.
/// Coordinates are in the duplicated output's local pixel space, with
/// origin (0,0) at the top-left of THIS monitor (not the virtual screen).
#[derive(Clone, Copy, Debug)]
pub struct PointerPosition {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

impl<'a> AcquiredFrame<'a> {
    /// True iff this frame's `LastPresentTime == 0`, meaning DDA had no new
    /// content but woke us up because the cursor moved. Encoder pipelines
    /// generally treat this as "no new frame, reuse keepalive".
    pub fn is_cursor_only(&self) -> bool {
        self.frame_info.LastPresentTime == 0
    }

    /// Cursor position iff DDA reports a non-zero `LastMouseUpdateTime`
    /// (i.e. the cursor moved since the previous frame). When `None`, the
    /// caller should reuse whatever position it last cached. The `visible`
    /// flag distinguishes "cursor is on this monitor" from "cursor is
    /// elsewhere or hidden" — the compositor blits only when visible.
    pub fn pointer_position(&self) -> Option<PointerPosition> {
        if self.frame_info.LastMouseUpdateTime == 0 {
            return None;
        }
        Some(PointerPosition {
            x: self.frame_info.PointerPosition.Position.x,
            y: self.frame_info.PointerPosition.Position.y,
            visible: self.frame_info.PointerPosition.Visible.as_bool(),
        })
    }

    /// Pull the latest cursor shape from DDA, if this frame includes one.
    ///
    /// `frame_info.PointerShapeBufferSize == 0` means "no shape change
    /// this frame, reuse the one you already have." For non-zero values we
    /// allocate (or reuse) a buffer of that size, call `GetFramePointerShape`,
    /// and decode into the engine-side BGRA representation.
    ///
    /// Returns `Ok(None)` when no shape was provided this frame.
    pub fn take_shape_update(&mut self) -> EngineResult<Option<CursorShape>> {
        let needed = self.frame_info.PointerShapeBufferSize;
        if needed == 0 {
            return Ok(None);
        }
        // A frame is only ever held on a live duplication.
        let Some(duplication) = self.capturer.duplication.as_ref() else {
            return Ok(None);
        };
        let mut buf = vec![0u8; needed as usize];
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let mut required: u32 = 0;
        unsafe {
            duplication.GetFramePointerShape(
                needed,
                buf.as_mut_ptr() as *mut _,
                &mut required,
                &mut info,
            )?;
        }
        // The buffer may not be entirely filled — `required` is the actual
        // payload length when smaller than `needed`. Truncate so decode
        // sees only valid bytes.
        if (required as usize) < buf.len() {
            buf.truncate(required as usize);
        }
        let shape = decode_shape(
            info.Type,
            info.Width,
            info.Height,
            info.Pitch,
            info.HotSpot.x,
            info.HotSpot.y,
            &buf,
        )?;
        Ok(Some(shape))
    }
}

impl Drop for AcquiredFrame<'_> {
    fn drop(&mut self) {
        self.capturer.release_held_frame();
    }
}

impl DxgiCapturer {
    /// Create a capturer for the given monitor. The provided `D3d11Context`
    /// MUST be on the same adapter that owns the monitor (LUID-equality);
    /// otherwise `DuplicateOutput1` fails with E_INVALIDARG.
    pub fn new(ctx: D3d11Context, monitor: MonitorInfo) -> EngineResult<Self> {
        if ctx.adapter_luid != monitor.adapter_luid {
            return Err(EngineError::AdapterMismatch {
                output_luid: monitor.adapter_luid,
                device_luid: ctx.adapter_luid,
            });
        }

        let output = monitor.open_output(&ctx.adapter)?;
        let duplication = create_duplication(&output, &ctx)?;

        // Sunshine display_base.cpp:239 — without ES_DISPLAY_REQUIRED, an idle
        // desktop sleeps the monitor → AcquireNextFrame returns ACCESS_LOST →
        // reinit wakes the monitor → infinite cycle. Set per-thread for the
        // capturer's lifetime; clear in Drop. SetThreadExecutionState returns
        // 0 on failure (NOT a HRESULT).
        let prev = unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED) };
        let display_required = prev.0 != 0;

        Ok(Self {
            width: monitor.width,
            height: monitor.height,
            monitor,
            ctx,
            duplication: Some(duplication),
            reinit: ReinitState::healthy(),
            frame_held: false,
            display_required,
        })
    }

    /// `true` while the duplication is lost and being re-created.
    pub fn is_lost(&self) -> bool {
        self.duplication.is_none()
    }

    /// Release the frame `acquire_frame` handed out, if one is still held.
    fn release_held_frame(&mut self) {
        if self.frame_held {
            if let Some(d) = self.duplication.as_ref() {
                let _ = unsafe { d.ReleaseFrame() };
            }
            self.frame_held = false;
        }
    }

    pub fn output_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn monitor(&self) -> &MonitorInfo {
        &self.monitor
    }

    pub fn d3d11(&self) -> &D3d11Context {
        &self.ctx
    }

    /// Block for up to `timeout` waiting for the next frame.
    ///
    /// Returns:
    ///   - `Ok(Some(frame))` — frame ready; drop the `AcquiredFrame` to release.
    ///   - `Ok(None)` — DDA timeout (no new content within `timeout`), or the
    ///     duplication is lost and being re-created (`is_lost`). Caller
    ///     typically falls back to keepalive frame.
    ///   - `Err(EngineError::Win32)` — a fatal HRESULT that is not a lost
    ///     duplication (device removed, bad resource).
    pub fn acquire_frame(&mut self, timeout: Duration) -> EngineResult<Option<AcquiredFrame<'_>>> {
        // Defensive: previous AcquiredFrame must have been dropped. If we
        // ever see this, fix the caller — DDA refuses re-acquire otherwise.
        self.release_held_frame();

        if self.duplication.is_none() {
            // Lost. Wait out the acquire timeout the way DDA would have
            // (keeps the caller's loop at its normal cadence), then try to
            // re-create once the backoff allows.
            let now = Instant::now();
            if now < self.reinit.next_attempt {
                std::thread::sleep(timeout.min(self.reinit.next_attempt - now));
                return Ok(None);
            }
            self.try_reinit();
            if self.duplication.is_none() {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            // Fresh duplication: fall through and acquire from it.
        }

        let timeout_ms = clamp_timeout_ms(timeout);
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;

        let r = unsafe {
            self.duplication
                .as_ref()
                .expect("duplication re-created above")
                .AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource)
        };

        match r {
            Ok(()) => {
                self.frame_held = true;
                let resource = resource.ok_or_else(|| {
                    EngineError::Win32(windows::core::Error::from_hresult(E_INVALIDARG))
                })?;
                let texture: ID3D11Texture2D = resource.cast()?;
                Ok(Some(AcquiredFrame {
                    texture,
                    captured_at: Instant::now(),
                    frame_info,
                    capturer: self,
                }))
            }
            Err(e)
                if e.code() == DXGI_ERROR_WAIT_TIMEOUT || e.code().0 == WAIT_TIMEOUT.0 as i32 =>
            {
                Ok(None)
            }
            Err(e) if is_duplication_lost(e.code()) => {
                // Another fullscreen app stole the duplication, or the
                // desktop was switched (UAC, lock screen). Drop it and try
                // once right away; a failure schedules the retry loop
                // instead of ending the capture thread.
                self.mark_lost(e.code());
                self.try_reinit();
                Ok(None)
            }
            Err(e) => Err(EngineError::Win32(e)),
        }
    }

    /// Forget a duplication that `AcquireNextFrame` reported gone.
    fn mark_lost(&mut self, code: windows::core::HRESULT) {
        self.release_held_frame();
        self.duplication = None;
        let now = Instant::now();
        self.reinit = ReinitState {
            lost_at: Some(now),
            next_attempt: now,
            delay: REINIT_DELAY_MIN,
            attempts: 0,
        };
        eprintln!(
            "[dxgi] duplication of {} lost (0x{:08X}); re-creating",
            self.monitor.device_name, code.0 as u32
        );
    }

    /// One re-creation attempt of a lost duplication; schedules the next
    /// one on failure, logs the recovery on success.
    fn try_reinit(&mut self) {
        let now = Instant::now();
        match self.reinit() {
            Ok(()) => {
                if let Some(lost_at) = self.reinit.lost_at {
                    eprintln!(
                        "[dxgi] duplication of {} re-created after {:.1} s, {} failed attempt(s)",
                        self.monitor.device_name,
                        now.duration_since(lost_at).as_secs_f64(),
                        self.reinit.attempts
                    );
                }
                self.reinit = ReinitState::healthy();
            }
            Err(e) => {
                self.reinit.failed(now);
                if self.reinit.attempts == 1 {
                    eprintln!(
                        "[dxgi] re-create of {} failed ({e:?}); retrying until it works",
                        self.monitor.device_name
                    );
                }
            }
        }
    }

    /// Tear down the existing duplication and create a new one against the
    /// same output. Used after `DXGI_ERROR_ACCESS_LOST` / `_ACCESS_DENIED`.
    /// On failure the capturer is left WITHOUT a duplication (`is_lost`);
    /// `acquire_frame` keeps retrying.
    pub fn reinit(&mut self) -> EngineResult<()> {
        self.release_held_frame();
        self.duplication = None;
        let output = self.monitor.open_output(&self.ctx.adapter)?;
        self.duplication = Some(create_duplication(&output, &self.ctx)?);
        Ok(())
    }
}

impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        self.release_held_frame();
        if self.display_required {
            let _ = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        }
    }
}

/// Run `IDXGIOutput5::DuplicateOutput1` with the design's 4-format scan-out
/// preference list (gate-2 finding: a single-format list silently fails on
/// some configurations and falls back to `IDXGIOutput1::DuplicateOutput`).
/// Falls back to `IDXGIOutput1::DuplicateOutput` if the Output5 path errors
/// or the interface isn't available.
fn create_duplication(
    output: &IDXGIOutput,
    ctx: &D3d11Context,
) -> EngineResult<IDXGIOutputDuplication> {
    if let Ok(o5) = output.cast::<IDXGIOutput5>() {
        let formats = [
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            DXGI_FORMAT_R10G10B10A2_UNORM,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
        ];
        match unsafe { o5.DuplicateOutput1(&ctx.device, 0, &formats) } {
            Ok(d) => return Ok(d),
            Err(e) => {
                // Some adapters (older Intel, virtual displays) reject Output5;
                // fall through to the simpler API.
                let _ = e;
            }
        }
    }
    let o1: IDXGIOutput1 = output.cast()?;
    Ok(unsafe { o1.DuplicateOutput(&ctx.device)? })
}

fn clamp_timeout_ms(d: Duration) -> u32 {
    let ms = d.as_millis();
    if ms == 0 {
        // 0 means "poll, return immediately if no frame" — the kernel APIs
        // accept 0 explicitly.
        return 0;
    }
    if ms > u32::MAX as u128 {
        u32::MAX
    } else {
        ms as u32
    }
}

// Suppress "unused" for symbols only consumed via `match arms`.
#[allow(dead_code)]
fn _ok_alias() -> windows::core::HRESULT {
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d11::create_dxgi_factory;
    use crate::monitors;

    /// The retry schedule of a lost duplication: 100 ms, 200, 400, 800,
    /// then 1 s for ever — a UAC prompt is back within a few attempts, a
    /// lock screen costs one attempt a second.
    #[test]
    fn reinit_backoff_doubles_to_one_second() {
        let mut d = REINIT_DELAY_MIN;
        let mut seen = Vec::new();
        for _ in 0..6 {
            seen.push(d);
            d = next_reinit_delay(d);
        }
        assert_eq!(
            seen,
            [100, 200, 400, 800, 1000, 1000]
                .map(Duration::from_millis)
                .to_vec()
        );

        let t0 = Instant::now();
        let mut st = ReinitState {
            lost_at: Some(t0),
            next_attempt: t0,
            delay: REINIT_DELAY_MIN,
            attempts: 0,
        };
        st.failed(t0);
        assert_eq!(st.attempts, 1);
        assert_eq!(st.next_attempt, t0 + Duration::from_millis(100));
        st.failed(t0 + Duration::from_millis(100));
        assert_eq!(st.attempts, 2);
        assert_eq!(st.next_attempt, t0 + Duration::from_millis(300));
        assert!(ReinitState::healthy().lost_at.is_none());
    }

    /// The HRESULTs the UAC switch produced on this machine — ACCESS_LOST
    /// from the acquire, then E_ACCESSDENIED from the re-create on the
    /// Intel output and DXGI_ERROR_INVALID_CALL on the NVIDIA one — all
    /// mean "re-create", none of them "give up".
    #[test]
    fn lost_set_covers_the_secure_desktop_switch() {
        assert!(is_duplication_lost(DXGI_ERROR_ACCESS_LOST));
        assert!(is_duplication_lost(DXGI_ERROR_ACCESS_DENIED));
        assert!(is_duplication_lost(DXGI_STATUS_OCCLUDED));
        assert!(is_duplication_lost(DXGI_ERROR_INVALID_CALL));
        assert!(!is_duplication_lost(DXGI_ERROR_WAIT_TIMEOUT));
        assert!(!is_duplication_lost(
            windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED
        ));
    }

    /// End-to-end: open the first attached output and grab one frame. The
    /// timeout is generous (500 ms); CI runs may legitimately have a static
    /// desktop and time out, so a None result is also acceptable.
    #[test]
    #[ignore = "requires real D3D11 hardware (DXGI Desktop Duplication); GitHub windows-latest VM has no GPU"]
    fn capture_one_frame() {
        let _g = crate::test_lock::DDA_LOCK.lock().unwrap();
        let factory = create_dxgi_factory().expect("factory");
        let mons = monitors::enumerate(&factory).expect("enumerate");
        let mon = mons
            .iter()
            .find(|m| m.attached_to_desktop && !m.adapter_is_software)
            .expect("at least one attached non-software output")
            .clone();
        let adapter = mon.open_adapter(&factory).expect("open adapter");
        let ctx = D3d11Context::create_on_adapter(adapter).expect("d3d11 ctx");
        let mut cap = DxgiCapturer::new(ctx, mon).expect("capturer");
        let (w, h) = cap.output_size();
        assert!(w > 0 && h > 0, "output size was zero");

        // First call may legitimately hit ACCESS_LOST during session setup
        // (DDA can race); accept either Ok(Some), Ok(None), or one retry.
        let mut got_frame_or_timeout = false;
        for _ in 0..3 {
            match cap.acquire_frame(Duration::from_millis(500)) {
                Ok(_) => {
                    got_frame_or_timeout = true;
                    break;
                }
                Err(EngineError::Win32(_)) => continue,
                Err(e) => panic!("non-Win32 error: {e:?}"),
            }
        }
        assert!(
            got_frame_or_timeout,
            "DDA never returned a frame or timeout"
        );
    }
}
