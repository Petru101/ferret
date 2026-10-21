// Captures frames of the attached game's main window with Windows.Graphics.Capture: only that
// window, never the rest of the desktop (as the portal does on Linux). No picker: Ferret
// knows the game, so it takes the game's largest visible top-level window.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::core::{Interface, BOOL};
use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindow, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible, GW_OWNER};

pub struct WindowCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    /// The last frame seen (width, height, RGB): Windows only sends a frame when the window
    /// changes, so a game that stopped drawing is read from it.
    last: Mutex<Option<(u32, u32, Vec<u8>)>>,
}

/// The game's main window: its largest visible window that no other window owns.
fn main_window(pid: u32) -> Option<HWND> {
    struct Search {
        pid: u32,
        best: Option<(i64, HWND)>,
    }
    unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> BOOL {
        let s = unsafe { &mut *(data.0 as *mut Search) };
        let mut owner = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
        let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|o| !o.is_invalid());
        if owner == s.pid && unsafe { IsWindowVisible(hwnd) }.as_bool() && !owned {
            let mut r = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut r) }.is_ok() {
                let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
                if s.best.is_none_or(|(a, _)| area > a) {
                    s.best = Some((area, hwnd));
                }
            }
        }
        BOOL(1)
    }
    let mut s = Search { pid, best: None };
    unsafe { EnumWindows(Some(each), LPARAM(&mut s as *mut Search as isize)) }.ok();
    s.best.map(|(_, h)| h)
}

fn err(e: windows::core::Error) -> String {
    e.message().to_string()
}

impl WindowCapture {
    pub fn start(pid: Option<u32>) -> Result<Self, String> {
        let pid = pid.ok_or("no game is open")?;
        let hwnd = main_window(pid).ok_or("the game has no window open")?;
        // Already initialized on this thread is fine.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.ok();
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(err)?;
        let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device.ok_or("no Direct3D device")?, context.ok_or("no Direct3D context")?);
        let dxgi: IDXGIDevice = device.cast().map_err(err)?;
        let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(err)?.cast().map_err(err)?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>().map_err(err)?;
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.map_err(err)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, item.Size().map_err(err)?)
            .map_err(err)?;
        let session = pool.CreateCaptureSession(&item).map_err(err)?;
        session.SetIsCursorCaptureEnabled(false).ok();
        // Windows 11: no yellow border around the game while Ferret watches it.
        session.SetIsBorderRequired(false).ok();
        session.StartCapture().map_err(err)?;
        Ok(Self { device, context, pool, session, last: Mutex::new(None) })
    }

    /// No window is remembered between runs on Windows.
    pub fn forget_window() {}

    /// The newest frame, as RGB, when one came since the last look.
    fn newest(&self) -> Result<Option<(u32, u32, Vec<u8>)>, String> {
        let mut frame = None;
        while let Ok(f) = self.pool.TryGetNextFrame() {
            frame = Some(f);
        }
        let Some(frame) = frame else { return Ok(None) };
        let size = frame.ContentSize().map_err(err)?;
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface().map_err(err)?.cast().map_err(err)?;
        let texture: ID3D11Texture2D = unsafe { access.GetInterface() }.map_err(err)?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) }.map_err(err)?;
        let staging = staging.ok_or("no staging texture")?;
        unsafe { self.context.CopyResource(&staging, &texture) };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.map_err(err)?;
        // The window may have shrunk since the pool was made: only its content counts.
        let (w, h) = ((size.Width as u32).min(desc.Width), (size.Height as u32).min(desc.Height));
        let mut rgb = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h as usize {
            let row = unsafe { std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize), w as usize * 4) };
            for px in row.chunks_exact(4) {
                rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
        }
        unsafe { self.context.Unmap(&staging, 0) };
        // A resized window needs a pool of its new size for the next frames.
        if size.Width as u32 != desc.Width || size.Height as u32 != desc.Height {
            let winrt: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&self.device.cast::<IDXGIDevice>().map_err(err)?) }
                .map_err(err)?
                .cast()
                .map_err(err)?;
            self.pool.Recreate(&winrt, DirectXPixelFormat::B8G8R8A8UIntNormalized, 2, size).ok();
        }
        Ok(Some((w, h, rgb)))
    }

    /// Saves the window's current frame as a PNG.
    pub fn grab(&self, out: &Path) -> Result<(), String> {
        let start = Instant::now();
        let mut got = self.newest()?;
        // Right after the start (or a resize) the first frame may not have come yet.
        while got.is_none() && self.last.lock().unwrap().is_none() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
            got = self.newest()?;
        }
        let mut last = self.last.lock().unwrap();
        if got.is_some() {
            *last = got;
        }
        let (w, h, rgb) = last.as_ref().ok_or("could not grab a frame: the game window sent none")?;
        let img = image::RgbImage::from_raw(*w, *h, rgb.clone()).ok_or("could not grab a frame: bad size")?;
        img.save(out).map_err(|e| e.to_string())
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        self.session.Close().ok();
        self.pool.Close().ok();
    }
}
