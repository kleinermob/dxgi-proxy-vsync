//! DXGI proxy DLL – forces VSync off for DX11 / DX12 games.
//!
//! ===========================================================================
//! DEVNOTE: The Architecture of Re-Enabling Tearing in Modern Windows
//! ===========================================================================
//!
//! Simply hooking the `Present` function and forcefully passing `SyncInterval=0`
//! is **no longer sufficient** to actually disable VSync on Windows 10/11.
//!
//! Modern DirectX 12 games (and Direct3D 11 games using the DXGI Flip Model, like
//! Universal Windows Platform (UWP) / GDK apps such as Minecraft Bedrock) are
//! beholden to the Desktop Window Manager (DWM). If a Flip Model swap chain is
//! created without explicit tearing support, the DWM will inherently VSync the
//! presentation queue.
//!
//! To force the DWM to allow uncapped FPS (frame tearing) we must intervene
//! in a highly specific sequence:
//!
//! 1. **At Swap Chain Creation (`CreateSwapChain...`)**: The game engine asks
//!    Windows to build the memory surfaces. We secretly intercept this request
//!    and inject `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING` into the descriptor.
//!    (If the swapchain isn't born with this flag, DWM ignores all future requests).
//!    NOTE: We only inject this flag when the swap effect is Flip-model based;
//!    legacy DISCARD/SEQUENTIAL effects do not support tearing and will fail.
//!
//! 2. **At Resize (`ResizeBuffers` / `ResizeBuffers1`)**: DirectX holds a strict,
//!    fatal rule: if a swap chain was born with the tearing flag, every single
//!    subsequent call to `ResizeBuffers` or `ResizeBuffers1` (triggered by resizing
//!    the window or pressing F11) **must exactly include that flag**. Since the
//!    engine doesn't know we secretly injected the flag at birth, it will call
//!    ResizeBuffers without it. If we don't intercept this and reinject the flag,
//!    DXGI crashes the app instantly with `DXGI_ERROR_INVALID_CALL`.
//!
//!    CRITICAL: D3D12 engines (e.g. Minecraft Bedrock D3D12) call `ResizeBuffers1`
//!    (IDXGISwapChain3, vtable slot 39) rather than the legacy `ResizeBuffers`
//!    (IDXGISwapChain, vtable slot 13). D3D11 engines use slot 13. We must hook
//!    BOTH, or D3D12 fullscreen<->windowed transitions will crash.
//!
//! 3. **At Presentation (`Present` / `Present1`)**: Finally, we intercept the
//!    frame delivery, set `SyncInterval=0`, and forcefully append
//!    `DXGI_PRESENT_ALLOW_TEARING`.
//!
//! Drop the compiled `dxgi.dll` next to a game's `.exe`. Overlays (MSI Afterburner,
//! Discord, Steam, etc.) keep working because we hand out the *real* factory
//! pointers - overlay injectors hook the same vtable after us.
//!
//! ===========================================================================
//! DEVNOTE: Vtable Index Reference (64-bit COM, inheritance-flattened)
//! ===========================================================================
//!
//! IUnknown:             [0]  QueryInterface
//!                       [1]  AddRef
//!                       [2]  Release
//! IDXGIObject:          [3]  SetPrivateData
//!                       [4]  SetPrivateDataInterface
//!                       [5]  GetPrivateData
//!                       [6]  GetParent
//! IDXGIDeviceSubObject: [7]  GetDevice
//! IDXGISwapChain:       [8]  Present                   <- hooked
//!                       [9]  GetBuffer
//!                       [10] SetFullscreenState
//!                       [11] GetFullscreenState
//!                       [12] GetDesc
//!                       [13] ResizeBuffers              <- hooked (D3D11 path)
//!                       [14] ResizeTarget
//!                       [15] GetContainingOutput
//!                       [16] GetFrameStatistics
//!                       [17] GetLastPresentCount
//! IDXGISwapChain1:      [18] GetDesc1
//!                       [19] GetFullscreenDesc
//!                       [20] GetHwnd
//!                       [21] GetCoreWindow
//!                       [22] Present1                  <- hooked
//!                       [23] IsTemporaryMonoSupported
//!                       [24] GetRestrictToOutput
//!                       [25] SetBackgroundColor
//!                       [26] GetBackgroundColor
//!                       [27] SetRotation
//!                       [28] GetRotation
//! IDXGISwapChain2:      [29] SetSourceSize
//!                       [30] GetSourceSize
//!                       [31] SetMaximumFrameLatency
//!                       [32] GetMaximumFrameLatency
//!                       [33] GetFrameLatencyWaitableObject
//!                       [34] SetMatrixTransform
//!                       [35] GetMatrixTransform
//! IDXGISwapChain3:      [36] GetCurrentBackBufferIndex
//!                       [37] CheckColorSpaceSupport
//!                       [38] SetColorSpace1
//!                       [39] ResizeBuffers1             <- hooked (D3D12 path)
//! IDXGISwapChain4:      [40] SetHDRMetaData
//!
//! IDXGIFactory (IUnknown[0-2] + IDXGIObject[3-6] -> 7 slots before):
//!   [7]  EnumAdapters
//!   [8]  MakeWindowAssociation
//!   [9]  GetWindowAssociation
//!   [10] CreateSwapChain              <- hooked
//!   [11] CreateSoftwareAdapter
//! IDXGIFactory1:
//!   [12] EnumAdapters1
//!   [13] IsCurrent
//! IDXGIFactory2:
//!   [14] IsWindowedStereoEnabled
//!   [15] CreateSwapChainForHwnd       <- hooked
//!   [16] CreateSwapChainForCoreWindow <- hooked
//!   [17] GetSharedResourceAdapterLuid
//!   [18] RegisterStereoStatusWindow
//!   [19] RegisterStereoStatusEvent
//!   [20] UnregisterStereoStatus
//!   [21] RegisterOcclusionStatusWindow
//!   [22] RegisterOcclusionStatusEvent
//!   [23] UnregisterOcclusionStatus
//!   [24] CreateSwapChainForComposition<- hooked
//! IDXGIFactory3:
//!   [25] GetCreationFlags
//! IDXGIFactory4:
//!   [26] EnumAdapterByLuid
//!   [27] EnumWarpAdapter
//! IDXGIFactory5:
//!   [28] CheckFeatureSupport

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use log::{error, info, warn};
use simplelog::*;

use windows::core::{GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING = 2048 = 0x800
const DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING: u32 = 0x800;

/// DXGI_PRESENT_ALLOW_TEARING = 0x200
const DXGI_PRESENT_ALLOW_TEARING_FLAG: u32 = 0x200;

/// DXGI_ERROR_INVALID_CALL = 0x887A0001
const DXGI_ERROR_INVALID_CALL: i32 = 0x887A0001_u32 as i32;

// ---------------------------------------------------------------------------
// Function-pointer types for the COM vtable entries we hook.
// ---------------------------------------------------------------------------

type PresentFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    sync: u32,
    flags: u32,
) -> HRESULT;

type Present1Fn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    sync: u32,
    flags: u32,
    params: *const DXGI_PRESENT_PARAMETERS,
) -> HRESULT;

type ResizeBuffersFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    buffer_count: u32,
    width: u32,
    height: u32,
    new_format: i32,
    swap_chain_flags: u32,
) -> HRESULT;

/// IDXGISwapChain3::ResizeBuffers1 — the D3D12-era resize path.
///
/// DEVNOTE: D3D12 engines call this instead of the legacy ResizeBuffers.
/// The extra parameters are node_mask (one entry per GPU node) and
/// present_queue (array of D3D12 command queue pointers, one per buffer).
/// We pass them through completely unchanged; we only touch swap_chain_flags.
type ResizeBuffers1Fn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    buffer_count: u32,
    width: u32,
    height: u32,
    new_format: i32,
    swap_chain_flags: u32,
    node_mask: *const u32,
    present_queue: *const *mut core::ffi::c_void,
) -> HRESULT;

type CreateSwapChainFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    desc: *mut DXGI_SWAP_CHAIN_DESC,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT;

type CreateSwapChainForHwndFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    hwnd: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen_desc: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT;

type CreateSwapChainForCoreWindowFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    window: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT;

type CreateSwapChainForCompositionFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT;

type QueryInterfaceFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    riid: *const GUID,
    ppv: *mut *mut core::ffi::c_void,
) -> HRESULT;

type ReleaseFn = unsafe extern "system" fn(this: *mut core::ffi::c_void) -> u32;

type CheckFeatureSupportFn = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    feature: u32,
    data: *mut core::ffi::c_void,
    data_size: u32,
) -> HRESULT;

// Real entry-point signatures resolved from the system dxgi.dll.
type RealCreateDXGIFactory =
    unsafe extern "system" fn(riid: *const GUID, factory: *mut *mut core::ffi::c_void) -> HRESULT;
type RealCreateDXGIFactory1 =
    unsafe extern "system" fn(riid: *const GUID, factory: *mut *mut core::ffi::c_void) -> HRESULT;
type RealCreateDXGIFactory2 = unsafe extern "system" fn(
    flags: u32,
    riid: *const GUID,
    factory: *mut *mut core::ffi::c_void,
) -> HRESULT;

// ---------------------------------------------------------------------------
// Per-vtable state: stores original function pointers for each distinct vtable.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct VtableState {
    orig_present: PresentFn,
    orig_present1: Option<Present1Fn>,
    orig_resize_buffers: ResizeBuffersFn,
    /// ResizeBuffers1 (IDXGISwapChain3, slot 39). Present on D3D12 swap chains,
    /// absent (null vtable slot) on D3D11 swap chains.
    orig_resize_buffers1: Option<ResizeBuffers1Fn>,
}

// Map from vtable raw pointer -> saved originals.
//
// DEVNOTE: Why map by `vtable` address and not `swap_chain` pointer instance?
// Many engines (like Bedrock) destroy and recreate COM instances constantly,
// and often these objects share the exact same underlying vtable memory.
// If we tracked by `swap_chain` pointer, we might double-hook the same vtable
// slot, accidentally storing our *own override* as the "original function",
// causing an immediate stack overflow (infinite recursion) upon calling Present.
static VTABLE_MAP: OnceLock<Mutex<HashMap<usize, VtableState>>> = OnceLock::new();

fn vtable_map() -> &'static Mutex<HashMap<usize, VtableState>> {
    VTABLE_MAP.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

/// Handle to the *real* system dxgi.dll, stored as a raw usize for Send/Sync.
static REAL_DXGI: AtomicUsize = AtomicUsize::new(0);

/// Whether we detected tearing support on this system.
static TEARING_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Debug counter for Present calls (to limit log spam on first N frames).
static PRESENT_LOG_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Debug counter for Present1 calls (separate from Present counter).
static PRESENT1_LOG_COUNTER: AtomicUsize = AtomicUsize::new(0);

// Factory-level original function pointers stored in OnceLock for safety.
// OnceLock guarantees write-once semantics without `static mut` data races.
static ORIG_CREATE_SWAP_CHAIN: OnceLock<CreateSwapChainFn> = OnceLock::new();
static ORIG_CREATE_SWAP_CHAIN_FOR_HWND: OnceLock<CreateSwapChainForHwndFn> = OnceLock::new();
static ORIG_CREATE_SWAP_CHAIN_FOR_CORE_WINDOW: OnceLock<CreateSwapChainForCoreWindowFn> =
    OnceLock::new();
static ORIG_CREATE_SWAP_CHAIN_FOR_COMPOSITION: OnceLock<CreateSwapChainForCompositionFn> =
    OnceLock::new();

// Track whether we've already hooked factory vtables.
static FACTORY_HOOKED: AtomicBool = AtomicBool::new(false);
static FACTORY2_HOOKED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Vtable helpers
// ---------------------------------------------------------------------------

/// Read a function pointer from a COM object's vtable at a given index.
unsafe fn vtable_fn<T>(obj: *mut core::ffi::c_void, index: usize) -> T {
    let vtable = *(obj as *const *const usize);
    let fn_ptr = *vtable.add(index);
    std::mem::transmute_copy(&fn_ptr)
}

/// Overwrite a single vtable slot (unprotect -> write -> restore).
/// Returns `None` if the slot already contains `new_fn` (avoids re-entrancy).
/// Returns `Some(original)` with the previous value if the patch was applied.
unsafe fn patch_vtable(
    obj: *mut core::ffi::c_void,
    index: usize,
    new_fn: usize,
) -> Option<usize> {
    use windows::Win32::System::Memory::{
        VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
    };

    let vtable = *(obj as *const *mut usize);
    let entry = vtable.add(index);
    let original = *entry;

    // If the slot already holds our hook, do not re-patch — this prevents us
    // from accidentally storing our own hook as the "original" pointer.
    if original == new_fn {
        return None;
    }

    let mut old_protect = PAGE_PROTECTION_FLAGS(0);
    let _ = VirtualProtect(
        entry as *const core::ffi::c_void,
        std::mem::size_of::<usize>(),
        PAGE_EXECUTE_READWRITE,
        &mut old_protect as *mut PAGE_PROTECTION_FLAGS,
    );

    *entry = new_fn;

    let _ = VirtualProtect(
        entry as *const core::ffi::c_void,
        std::mem::size_of::<usize>(),
        old_protect,
        &mut old_protect as *mut PAGE_PROTECTION_FLAGS,
    );

    Some(original)
}

// ---------------------------------------------------------------------------
// Hooked swap chain functions
// ---------------------------------------------------------------------------

unsafe extern "system" fn hooked_present(
    this: *mut core::ffi::c_void,
    _sync_interval: u32,
    flags: u32,
) -> HRESULT {
    let vtable_ptr = *(this as *const *const usize) as usize;
    let orig = match vtable_map().lock() {
        Ok(map) => map.get(&vtable_ptr).map(|s| s.orig_present),
        Err(_) => None,
    };

    let Some(orig_fn) = orig else {
        warn!("hooked_present: no original found for vtable {:#x}", vtable_ptr);
        return HRESULT(0);
    };

    // Always force SyncInterval = 0 to disable engine-side VSync.
    let sync = 0u32;

    let tearing_supported = TEARING_SUPPORTED.load(Ordering::Relaxed);
    let want_tearing = tearing_supported && (flags & DXGI_PRESENT_ALLOW_TEARING_FLAG) == 0;

    let hr = if want_tearing {
        let hr_tearing = orig_fn(this, sync, flags | DXGI_PRESENT_ALLOW_TEARING_FLAG);
        if hr_tearing.0 == DXGI_ERROR_INVALID_CALL {
            // DEVNOTE: Tearing and Exclusive Fullscreen
            // In legacy exclusive fullscreen, DXGI forbids ALLOW_TEARING and
            // returns DXGI_ERROR_INVALID_CALL. Swallowing that error would freeze
            // the screen. Gracefully retry without the flag.
            orig_fn(this, sync, flags)
        } else {
            hr_tearing
        }
    } else {
        orig_fn(this, sync, flags)
    };

    let count = PRESENT_LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
    if count < 100 {
        info!(
            "Present hook fired (frame={}, sync={}, flags={:#x}, want_tearing={})",
            count, sync, flags, want_tearing
        );
    } else if count == 100 {
        info!("Present hook: suppressing per-frame logs after frame 100");
    }

    hr
}

unsafe extern "system" fn hooked_present1(
    this: *mut core::ffi::c_void,
    _sync_interval: u32,
    flags: u32,
    params: *const DXGI_PRESENT_PARAMETERS,
) -> HRESULT {
    let vtable_ptr = *(this as *const *const usize) as usize;
    let orig = match vtable_map().lock() {
        Ok(map) => map.get(&vtable_ptr).and_then(|s| s.orig_present1),
        Err(_) => None,
    };

    let Some(orig_fn) = orig else {
        warn!("hooked_present1: no original found for vtable {:#x}", vtable_ptr);
        return HRESULT(0);
    };

    let sync = 0u32;

    let tearing_supported = TEARING_SUPPORTED.load(Ordering::Relaxed);
    let want_tearing = tearing_supported && (flags & DXGI_PRESENT_ALLOW_TEARING_FLAG) == 0;

    let hr = if want_tearing {
        let hr_tearing = orig_fn(this, sync, flags | DXGI_PRESENT_ALLOW_TEARING_FLAG, params);
        if hr_tearing.0 == DXGI_ERROR_INVALID_CALL {
            orig_fn(this, sync, flags, params)
        } else {
            hr_tearing
        }
    } else {
        orig_fn(this, sync, flags, params)
    };

    let count = PRESENT1_LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
    if count < 100 {
        info!(
            "Present1 hook fired (frame={}, sync={}, flags={:#x}, want_tearing={})",
            count, sync, flags, want_tearing
        );
    } else if count == 100 {
        info!("Present1 hook: suppressing per-frame logs after frame 100");
    }

    hr
}

unsafe extern "system" fn hooked_resize_buffers(
    this: *mut core::ffi::c_void,
    buffer_count: u32,
    width: u32,
    height: u32,
    new_format: i32,
    swap_chain_flags: u32,
) -> HRESULT {
    let vtable_ptr = *(this as *const *const usize) as usize;
    let orig = match vtable_map().lock() {
        Ok(map) => map.get(&vtable_ptr).map(|s| s.orig_resize_buffers),
        Err(_) => None,
    };

    let Some(orig_fn) = orig else {
        warn!("hooked_resize_buffers: no original for vtable {:#x}", vtable_ptr);
        return HRESULT(0);
    };

    // DEVNOTE: The F11 / Fullscreen Crash Fix (D3D11 path)
    // DXGI enforces a strict contract: if a swap chain was created with
    // DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, that exact flag MUST also be
    // passed on every subsequent ResizeBuffers call. Because we secretly
    // inject ALLOW_TEARING at creation time, the engine is unaware the flag
    // exists and omits it here, causing a fatal DXGI panic. We reinsert it.
    let modified_flags = if TEARING_SUPPORTED.load(Ordering::Relaxed) {
        swap_chain_flags | DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING
    } else {
        swap_chain_flags
    };

    info!(
        "ResizeBuffers (D3D11): {}x{} flags {:#x} -> {:#x}",
        width, height, swap_chain_flags, modified_flags
    );

    let hr = orig_fn(this, buffer_count, width, height, new_format, modified_flags);

    // Re-hook after resize in case the engine internally rebuilt the vtable
    // (some engines do this on fullscreen transitions).
    hook_swap_chain(this);

    hr
}

unsafe extern "system" fn hooked_resize_buffers1(
    this: *mut core::ffi::c_void,
    buffer_count: u32,
    width: u32,
    height: u32,
    new_format: i32,
    swap_chain_flags: u32,
    node_mask: *const u32,
    present_queue: *const *mut core::ffi::c_void,
) -> HRESULT {
    let vtable_ptr = *(this as *const *const usize) as usize;
    let orig = match vtable_map().lock() {
        Ok(map) => map.get(&vtable_ptr).and_then(|s| s.orig_resize_buffers1),
        Err(_) => None,
    };

    let Some(orig_fn) = orig else {
        warn!("hooked_resize_buffers1: no original for vtable {:#x}", vtable_ptr);
        return HRESULT(0);
    };

    // DEVNOTE: The F11 / Fullscreen Crash Fix (D3D12 path)
    // Identical contract to ResizeBuffers above, but called exclusively by
    // D3D12 engines. Minecraft Bedrock D3D12 calls ResizeBuffers1 on every
    // fullscreen toggle, which is why D3D12 crashed while D3D11 was fine:
    // D3D11 uses slot 13 (ResizeBuffers) which we were already intercepting,
    // but D3D12 uses slot 39 (ResizeBuffers1) which was previously unhoooked.
    //
    // node_mask and present_queue are D3D12-specific multi-GPU parameters
    // that we pass through unchanged. We only touch swap_chain_flags.
    let modified_flags = if TEARING_SUPPORTED.load(Ordering::Relaxed) {
        swap_chain_flags | DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING
    } else {
        swap_chain_flags
    };

    info!(
        "ResizeBuffers1 (D3D12): {}x{} flags {:#x} -> {:#x}",
        width, height, swap_chain_flags, modified_flags
    );

    let hr = orig_fn(
        this,
        buffer_count,
        width,
        height,
        new_format,
        modified_flags,
        node_mask,
        present_queue,
    );

    // Re-hook in case vtable was rebuilt.
    hook_swap_chain(this);

    hr
}

// ---------------------------------------------------------------------------
// Swap-chain vtable hooking
// ---------------------------------------------------------------------------

/// Returns true if a DXGI_SWAP_EFFECT value is Flip-model based.
/// Only Flip-model swap chains support DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.
/// Legacy DISCARD / SEQUENTIAL effects will fail if given that flag.
fn is_flip_model(swap_effect: DXGI_SWAP_EFFECT) -> bool {
    swap_effect == DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL
        || swap_effect == DXGI_SWAP_EFFECT_FLIP_DISCARD
}

/// Hook Present / Present1 / ResizeBuffers / ResizeBuffers1 on a swap chain.
///
/// Reads originals exclusively from `patch_vtable`'s return value — never
/// from a pre-read of the vtable — so partial re-hooks cannot store stale
/// (already-hooked) pointers as the "original".
unsafe fn hook_swap_chain(swap_chain: *mut core::ffi::c_void) {
    let vtable_ptr = *(swap_chain as *const *const usize) as usize;

    let mut new_present: Option<PresentFn> = None;
    let mut new_present1: Option<Present1Fn> = None;
    let mut new_resize: Option<ResizeBuffersFn> = None;
    let mut new_resize1: Option<ResizeBuffers1Fn> = None;

    // IDXGISwapChain::Present — slot 8 (all swap chains)
    if let Some(orig) = patch_vtable(swap_chain, 8, hooked_present as usize) {
        new_present = Some(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGISwapChain::Present (vtable[8]) — orig @ {:#x}", orig);
    }

    // IDXGISwapChain::ResizeBuffers — slot 13 (D3D11 fullscreen toggle path)
    if let Some(orig) = patch_vtable(swap_chain, 13, hooked_resize_buffers as usize) {
        new_resize = Some(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGISwapChain::ResizeBuffers (vtable[13]) — orig @ {:#x}", orig);
    }

    // Read the vtable pointer once for the null-slot probes below.
    let vtable = *(swap_chain as *const *const usize);

    // IDXGISwapChain1::Present1 — slot 22 (only exists on SwapChain1+)
    // Probe for null before patching; a null entry means this interface
    // level is not implemented by the current swap chain object.
    if *vtable.add(22) != 0 {
        if let Some(orig) = patch_vtable(swap_chain, 22, hooked_present1 as usize) {
            new_present1 = Some(std::mem::transmute_copy(&orig));
            info!("Hooked IDXGISwapChain1::Present1 (vtable[22]) — orig @ {:#x}", orig);
        }
    }

    // IDXGISwapChain3::ResizeBuffers1 — slot 39 (D3D12 fullscreen toggle path)
    //
    // DEVNOTE: This is the slot that was missing and caused D3D12 Minecraft
    // Bedrock to crash on fullscreen<->windowed transitions. D3D12 swap chains
    // implement IDXGISwapChain3 and call ResizeBuffers1 (slot 39). D3D11 swap
    // chains only reach IDXGISwapChain1 and call the legacy ResizeBuffers
    // (slot 13). We probe for null so we don't blindly index past the end of
    // a D3D11 swap chain's vtable.
    if *vtable.add(39) != 0 {
        if let Some(orig) = patch_vtable(swap_chain, 39, hooked_resize_buffers1 as usize) {
            new_resize1 = Some(std::mem::transmute_copy(&orig));
            info!("Hooked IDXGISwapChain3::ResizeBuffers1 (vtable[39]) — orig @ {:#x}", orig);
        }
    }

    // If nothing was patched this round, the vtable was already fully hooked.
    if new_present.is_none()
        && new_present1.is_none()
        && new_resize.is_none()
        && new_resize1.is_none()
    {
        return;
    }

    // Merge with any previously stored state for this vtable (e.g. if
    // ResizeBuffers1 fires for the first time on a re-hook after resize).
    if let Ok(mut map) = vtable_map().lock() {
        let entry = map.entry(vtable_ptr).or_insert_with(|| VtableState {
            orig_present: hooked_present,              // placeholder, overwritten below
            orig_present1: None,
            orig_resize_buffers: hooked_resize_buffers, // placeholder, overwritten below
            orig_resize_buffers1: None,
        });

        if let Some(f) = new_present {
            entry.orig_present = f;
        }
        if let Some(f) = new_resize {
            entry.orig_resize_buffers = f;
        }
        if new_present1.is_some() {
            entry.orig_present1 = new_present1;
        }
        if new_resize1.is_some() {
            entry.orig_resize_buffers1 = new_resize1;
        }

        info!("VtableState registered/updated for vtable {:#x}", vtable_ptr);
    }
}

// ---------------------------------------------------------------------------
// Factory vtable hooking  (CreateSwapChain, CreateSwapChainForHwnd, ...)
// ---------------------------------------------------------------------------

// DEVNOTE: Intercepting Creation is the "Nuclear Option"
// Without intercepting the exact moment the engine establishes the Swap Chain
// and slipping `ALLOW_TEARING` into the configuration structures, modern DWM
// totally overrides whatever commands we pass during `Present()`. Intercepting
// creation guarantees true uncapped frames without DWM composition override.

unsafe extern "system" fn hooked_create_swap_chain(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    desc: *mut DXGI_SWAP_CHAIN_DESC,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let Some(orig) = ORIG_CREATE_SWAP_CHAIN.get() else {
        error!("hooked_create_swap_chain: no original stored");
        return HRESULT(-1);
    };

    // Only inject ALLOW_TEARING for Flip-model swap effects.
    // Legacy DISCARD/SEQUENTIAL effects will return DXGI_ERROR_INVALID_CALL
    // if given this flag.
    let tearing = TEARING_SUPPORTED.load(Ordering::Relaxed);
    let injected = tearing && !desc.is_null() && is_flip_model((*desc).SwapEffect);

    if injected {
        info!(
            "CreateSwapChain: injecting ALLOW_TEARING (SwapEffect={:?})",
            (*desc).SwapEffect
        );
        (*desc).Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    let hr = orig(this, device, desc, swap_chain);
    if hr.is_ok() && !(*swap_chain).is_null() {
        info!("CreateSwapChain succeeded — hooking swap chain");
        hook_swap_chain(*swap_chain);
        return hr;
    }

    if hr.is_err() && injected {
        warn!("CreateSwapChain failed after injecting ALLOW_TEARING, retrying without it");
        (*desc).Flags &= !DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
        let hr_retry = orig(this, device, desc, swap_chain);
        if hr_retry.is_ok() && !(*swap_chain).is_null() {
            hook_swap_chain(*swap_chain);
        }
        return hr_retry;
    }

    hr
}

unsafe extern "system" fn hooked_create_swap_chain_for_hwnd(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    hwnd: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen_desc: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let Some(orig) = ORIG_CREATE_SWAP_CHAIN_FOR_HWND.get() else {
        error!("hooked_create_swap_chain_for_hwnd: no original stored");
        return HRESULT(-1);
    };

    let mut modified_desc = *desc;
    let tearing = TEARING_SUPPORTED.load(Ordering::Relaxed);
    let injected = tearing && is_flip_model(modified_desc.SwapEffect);

    if injected {
        info!(
            "CreateSwapChainForHwnd: injecting ALLOW_TEARING (SwapEffect={:?})",
            modified_desc.SwapEffect
        );
        modified_desc.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    let hr = orig(
        this,
        device,
        hwnd,
        &modified_desc,
        fullscreen_desc,
        restrict_to_output,
        swap_chain,
    );

    if hr.is_ok() && !(*swap_chain).is_null() {
        info!("CreateSwapChainForHwnd succeeded — hooking swap chain");
        hook_swap_chain(*swap_chain);
        return hr;
    }

    if hr.is_err() && injected {
        warn!("CreateSwapChainForHwnd failed with ALLOW_TEARING, retrying without it");
        let hr_retry = orig(
            this,
            device,
            hwnd,
            desc,
            fullscreen_desc,
            restrict_to_output,
            swap_chain,
        );
        if hr_retry.is_ok() && !(*swap_chain).is_null() {
            hook_swap_chain(*swap_chain);
        }
        return hr_retry;
    }

    hr
}

unsafe extern "system" fn hooked_create_swap_chain_for_core_window(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    window: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let Some(orig) = ORIG_CREATE_SWAP_CHAIN_FOR_CORE_WINDOW.get() else {
        error!("hooked_create_swap_chain_for_core_window: no original stored");
        return HRESULT(-1);
    };

    let mut modified_desc = *desc;
    let tearing = TEARING_SUPPORTED.load(Ordering::Relaxed);
    let injected = tearing && is_flip_model(modified_desc.SwapEffect);

    if injected {
        info!(
            "CreateSwapChainForCoreWindow: injecting ALLOW_TEARING (SwapEffect={:?})",
            modified_desc.SwapEffect
        );
        modified_desc.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    let hr = orig(this, device, window, &modified_desc, restrict_to_output, swap_chain);

    if hr.is_ok() && !(*swap_chain).is_null() {
        info!("CreateSwapChainForCoreWindow succeeded — hooking swap chain");
        hook_swap_chain(*swap_chain);
        return hr;
    }

    if hr.is_err() && injected {
        warn!("CreateSwapChainForCoreWindow failed with ALLOW_TEARING, retrying without it");
        let hr_retry = orig(this, device, window, desc, restrict_to_output, swap_chain);
        if hr_retry.is_ok() && !(*swap_chain).is_null() {
            hook_swap_chain(*swap_chain);
        }
        return hr_retry;
    }

    hr
}

unsafe extern "system" fn hooked_create_swap_chain_for_composition(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let Some(orig) = ORIG_CREATE_SWAP_CHAIN_FOR_COMPOSITION.get() else {
        error!("hooked_create_swap_chain_for_composition: no original stored");
        return HRESULT(-1);
    };

    let mut modified_desc = *desc;
    let tearing = TEARING_SUPPORTED.load(Ordering::Relaxed);
    let injected = tearing && is_flip_model(modified_desc.SwapEffect);

    if injected {
        info!(
            "CreateSwapChainForComposition: injecting ALLOW_TEARING (SwapEffect={:?})",
            modified_desc.SwapEffect
        );
        modified_desc.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    let hr = orig(this, device, &modified_desc, restrict_to_output, swap_chain);

    if hr.is_ok() && !(*swap_chain).is_null() {
        info!("CreateSwapChainForComposition succeeded — hooking swap chain");
        hook_swap_chain(*swap_chain);
        return hr;
    }

    if hr.is_err() && injected {
        warn!("CreateSwapChainForComposition failed with ALLOW_TEARING, retrying without it");
        let hr_retry = orig(this, device, desc, restrict_to_output, swap_chain);
        if hr_retry.is_ok() && !(*swap_chain).is_null() {
            hook_swap_chain(*swap_chain);
        }
        return hr_retry;
    }

    hr
}

// ---------------------------------------------------------------------------
// Factory vtable hooking
// ---------------------------------------------------------------------------

/// Hook IDXGIFactory::CreateSwapChain (vtable slot 10).
unsafe fn hook_factory(factory: *mut core::ffi::c_void) {
    if FACTORY_HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }

    if let Some(orig) = patch_vtable(factory, 10, hooked_create_swap_chain as usize) {
        let _ = ORIG_CREATE_SWAP_CHAIN.set(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory::CreateSwapChain (vtable[10])");
    }
}

/// Hook IDXGIFactory2 swap-chain-creation methods (vtable slots 15, 16, 24).
unsafe fn hook_factory2(factory: *mut core::ffi::c_void) {
    if FACTORY2_HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }

    if let Some(orig) = patch_vtable(factory, 15, hooked_create_swap_chain_for_hwnd as usize) {
        let _ = ORIG_CREATE_SWAP_CHAIN_FOR_HWND.set(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory2::CreateSwapChainForHwnd (vtable[15])");
    }

    if let Some(orig) =
        patch_vtable(factory, 16, hooked_create_swap_chain_for_core_window as usize)
    {
        let _ = ORIG_CREATE_SWAP_CHAIN_FOR_CORE_WINDOW.set(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory2::CreateSwapChainForCoreWindow (vtable[16])");
    }

    if let Some(orig) =
        patch_vtable(factory, 24, hooked_create_swap_chain_for_composition as usize)
    {
        let _ = ORIG_CREATE_SWAP_CHAIN_FOR_COMPOSITION.set(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory2::CreateSwapChainForComposition (vtable[24])");
    }
}

/// Hook both IDXGIFactory and IDXGIFactory2 methods on any factory pointer,
/// by QueryInterface-ing for IDXGIFactory2 if needed.
///
/// DEVNOTE: Even factories obtained via CreateDXGIFactory / CreateDXGIFactory1
/// on modern Windows implement IDXGIFactory2 internally. Without also hooking
/// slots 15/16/24, games that call CreateSwapChainForHwnd through an older
/// factory entry-point would not receive the tearing flag injection.
unsafe fn hook_factory_all(factory: *mut core::ffi::c_void) {
    hook_factory(factory);

    // IDXGIFactory2 IID: {50c83a1c-e072-4c48-87b0-3630fa36a6d0}
    let iid_factory2 = GUID {
        data1: 0x50c83a1c,
        data2: 0xe072,
        data3: 0x4c48,
        data4: [0x87, 0xb0, 0x36, 0x30, 0xfa, 0x36, 0xa6, 0xd0],
    };

    let qi: QueryInterfaceFn = vtable_fn(factory, 0);
    let mut factory2_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let hr = qi(factory, &iid_factory2, &mut factory2_ptr);
    if hr.is_ok() && !factory2_ptr.is_null() {
        hook_factory2(factory2_ptr);
        // Release the extra reference acquired via QueryInterface.
        let release: ReleaseFn = vtable_fn(factory2_ptr, 2);
        release(factory2_ptr);
    }
}

// ---------------------------------------------------------------------------
// Tearing support detection
// ---------------------------------------------------------------------------

unsafe fn check_tearing_support(factory: *mut core::ffi::c_void) {
    // IDXGIFactory5 IID: {7632e1f5-ee65-4dca-87fd-84cd75f8838d}
    let iid_factory5 = GUID {
        data1: 0x7632e1f5,
        data2: 0xee65,
        data3: 0x4dca,
        data4: [0x87, 0xfd, 0x84, 0xcd, 0x75, 0xf8, 0x83, 0x8d],
    };

    let qi: QueryInterfaceFn = vtable_fn(factory, 0);
    let mut factory5_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let hr = qi(factory, &iid_factory5, &mut factory5_ptr);

    if hr.is_err() || factory5_ptr.is_null() {
        info!("IDXGIFactory5 not available — tearing flag will not be used");
        return;
    }

    // IDXGIFactory5::CheckFeatureSupport is at vtable index 28.
    // Full count: IUnknown[0-2] + IDXGIObject[3-6] + IDXGIFactory[7-11]
    //           + IDXGIFactory1[12-13] + IDXGIFactory2[14-24]
    //           + IDXGIFactory3[25] + IDXGIFactory4[26-27] + IDXGIFactory5[28]
    let check: CheckFeatureSupportFn = vtable_fn(factory5_ptr, 28);
    let mut allow_tearing: i32 = 0;
    let hr2 = check(
        factory5_ptr,
        0, // DXGI_FEATURE_PRESENT_ALLOW_TEARING = 0
        &mut allow_tearing as *mut i32 as *mut core::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    );

    if hr2.is_ok() && allow_tearing != 0 {
        TEARING_SUPPORTED.store(true, Ordering::SeqCst);
        info!("Tearing (ALLOW_TEARING) is SUPPORTED on this system");
    } else {
        info!(
            "Tearing is NOT supported (hr={:?}, val={})",
            hr2, allow_tearing
        );
    }

    let release: ReleaseFn = vtable_fn(factory5_ptr, 2);
    release(factory5_ptr);
}

// ---------------------------------------------------------------------------
// Loading the real dxgi.dll
// ---------------------------------------------------------------------------

unsafe fn load_real_dxgi() -> Option<HMODULE> {
    let mut sys_dir = [0u8; 260];
    let len = windows::Win32::System::SystemInformation::GetSystemDirectoryA(Some(&mut sys_dir));
    if len == 0 {
        error!("GetSystemDirectoryA failed");
        return None;
    }
    let dir_str =
        std::str::from_utf8(&sys_dir[..len as usize]).unwrap_or("C:\\Windows\\System32");
    let path = format!("{}\\dxgi.dll", dir_str);
    let cpath = CString::new(path.clone()).ok()?;
    let h = LoadLibraryA(PCSTR(cpath.as_ptr() as *const u8)).ok()?;
    info!("Loaded real dxgi.dll from {}", path);
    Some(h)
}

unsafe fn get_real_proc(name: &str) -> Option<unsafe extern "system" fn() -> isize> {
    let raw = REAL_DXGI.load(Ordering::SeqCst);
    if raw == 0 {
        return None;
    }
    let h = HMODULE(raw as *mut core::ffi::c_void);
    let cname = CString::new(name).ok()?;
    GetProcAddress(h, PCSTR(cname.as_ptr() as *const u8))
}

// ---------------------------------------------------------------------------
// Exported entry points (proxied from real dxgi.dll)
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn CreateDXGIFactory(
    riid: *const GUID,
    factory: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let real: RealCreateDXGIFactory = match get_real_proc("CreateDXGIFactory") {
        Some(f) => std::mem::transmute(f),
        None => {
            error!("Failed to resolve real CreateDXGIFactory");
            return HRESULT(-1);
        }
    };
    let hr = real(riid, factory);
    if hr.is_ok() && !(*factory).is_null() {
        info!("CreateDXGIFactory succeeded");
        check_tearing_support(*factory);
        hook_factory_all(*factory);
    }
    hr
}

#[no_mangle]
pub unsafe extern "system" fn CreateDXGIFactory1(
    riid: *const GUID,
    factory: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let real: RealCreateDXGIFactory1 = match get_real_proc("CreateDXGIFactory1") {
        Some(f) => std::mem::transmute(f),
        None => {
            error!("Failed to resolve real CreateDXGIFactory1");
            return HRESULT(-1);
        }
    };
    let hr = real(riid, factory);
    if hr.is_ok() && !(*factory).is_null() {
        info!("CreateDXGIFactory1 succeeded");
        check_tearing_support(*factory);
        hook_factory_all(*factory);
    }
    hr
}

#[no_mangle]
pub unsafe extern "system" fn CreateDXGIFactory2(
    flags: u32,
    riid: *const GUID,
    factory: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let real: RealCreateDXGIFactory2 = match get_real_proc("CreateDXGIFactory2") {
        Some(f) => std::mem::transmute(f),
        None => {
            error!("Failed to resolve real CreateDXGIFactory2");
            return HRESULT(-1);
        }
    };
    let hr = real(flags, riid, factory);
    if hr.is_ok() && !(*factory).is_null() {
        info!("CreateDXGIFactory2 succeeded (flags={:#x})", flags);
        check_tearing_support(*factory);
        hook_factory_all(*factory);
    }
    hr
}

#[no_mangle]
pub unsafe extern "system" fn DXGIGetDebugInterface1(
    flags: u32,
    riid: *const GUID,
    debug: *mut *mut core::ffi::c_void,
) -> HRESULT {
    type RealFn =
        unsafe extern "system" fn(u32, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT;
    let real: RealFn = match get_real_proc("DXGIGetDebugInterface1") {
        Some(f) => std::mem::transmute(f),
        None => {
            error!("Failed to resolve real DXGIGetDebugInterface1");
            return HRESULT(-1);
        }
    };
    real(flags, riid, debug)
}

// ---------------------------------------------------------------------------
// DllMain
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    _hinst: HMODULE,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        let log_path = std::env::var("TEMP")
            .map(|t| PathBuf::from(t).join("dxgi_proxy.log"))
            .unwrap_or_else(|_| PathBuf::from("dxgi_proxy.log"));

        if let Ok(f) = File::create(&log_path) {
            let _ = WriteLogger::init(LevelFilter::Info, Config::default(), f);
        }
        info!("=== DXGI Proxy loaded (VSync OFF) ===");

        if let Some(h) = load_real_dxgi() {
            REAL_DXGI.store(h.0 as usize, Ordering::SeqCst);
            info!("Real DXGI module handle stored");
        } else {
            error!("FATAL: Could not load the real dxgi.dll — proxy non-functional");
        }
    }
    TRUE
}