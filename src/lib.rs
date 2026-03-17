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
//! 
//! 2. **At Resize (`ResizeBuffers`)**: DirectX holds a strict, fatal rule: if a 
//!    swap chain was born with the tearing flag, every single subsequent call 
//!    to `ResizeBuffers` (triggered by resizing the window or pressing F11) 
//!    **must exactly include that flag**. Since the engine doesn't know we 
//!    secretly injected the flag at birth, it will call `ResizeBuffers` without 
//!    it. If we don't intercept this and reinject the flag, DXGI crashes the app 
//!    instantly with `DXGI_ERROR_INVALID_CALL`.
//! 
//! 3. **At Presentation (`Present` / `Present1`)**: Finally, we intercept the 
//!    frame delivery, set `SyncInterval=0`, and forcefully append 
//!    `DXGI_PRESENT_ALLOW_TEARING`. 
//! 
//! Drop the compiled `dxgi.dll` next to a game's `.exe`. Overlays (MSI Afterburner, 
//! Discord, Steam, etc.) keep working because we hand out the *real* factory 
//! pointers – overlay injectors hook the same vtable after us.

#![allow(non_snake_case)]

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use log::{error, info, warn};
use simplelog::*;

use windows::core::{GUID, HRESULT, PCSTR};
use windows::Win32::Foundation::{BOOL, HMODULE, TRUE};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;

// ---------------------------------------------------------------------------
// Per-vtable state: stores original function pointers for each distinct vtable.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct VtableState {
    orig_present: PresentFn,
    orig_present1: Option<Present1Fn>,
    orig_resize_buffers: ResizeBuffersFn,
}

// Map from vtable raw pointer → saved originals.
// DEVNOTE: Why map by `vtable` address and not `swap_chain` pointer instance?
// Many engines (like Bedrock) destroy and recreate COM instances constantly, 
// and often these objects share the exact same underlying vtable memory. 
// If we tracked by `swap_chain` pointer, we might double-hook the same vtable 
// slot, accidentally storing our *own override* as the "original function", 
// causing an immediate stack overflow (infinite recursion) upon calling Present.
static VTABLE_MAP: once_cell::sync::Lazy<Mutex<HashMap<usize, VtableState>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

/// Handle to the *real* system dxgi.dll, stored as a raw usize for Send/Sync.
static REAL_DXGI: AtomicUsize = AtomicUsize::new(0);

/// Whether we detected tearing support on this system.
static TEARING_SUPPORTED: AtomicBool = AtomicBool::new(false);

/// Debug counter to log the first N Present calls to verify the hook is firing.
static PRESENT_LOG_COUNTER: AtomicUsize = AtomicUsize::new(0);

// Factory-level original function pointers (singleton per factory vtable).
static mut ORIG_CREATE_SWAP_CHAIN: Option<CreateSwapChainFn> = None;
static mut ORIG_CREATE_SWAP_CHAIN_FOR_HWND: Option<CreateSwapChainForHwndFn> = None;
static mut ORIG_CREATE_SWAP_CHAIN_FOR_CORE_WINDOW: Option<CreateSwapChainForCoreWindowFn> = None;
static mut ORIG_CREATE_SWAP_CHAIN_FOR_COMPOSITION: Option<CreateSwapChainForCompositionFn> = None;

// Track whether we've already hooked factory vtables.
static FACTORY_HOOKED: AtomicBool = AtomicBool::new(false);
static FACTORY2_HOOKED: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Function-pointer types for the COM vtable entries we hook.
// ---------------------------------------------------------------------------

type PresentFn = unsafe extern "system" fn(this: *mut core::ffi::c_void, sync: u32, flags: u32) -> HRESULT;
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

// Real entry-point signatures resolved from the system dxgi.dll.
type RealCreateDXGIFactory = unsafe extern "system" fn(riid: *const GUID, factory: *mut *mut core::ffi::c_void) -> HRESULT;
type RealCreateDXGIFactory1 = unsafe extern "system" fn(riid: *const GUID, factory: *mut *mut core::ffi::c_void) -> HRESULT;
type RealCreateDXGIFactory2 = unsafe extern "system" fn(flags: u32, riid: *const GUID, factory: *mut *mut core::ffi::c_void) -> HRESULT;

// ---------------------------------------------------------------------------
// Vtable helpers
// ---------------------------------------------------------------------------

/// Read a function pointer from a COM object's vtable.
unsafe fn vtable_fn<T>(obj: *mut core::ffi::c_void, index: usize) -> T {
    let vtable = *(obj as *const *const usize);
    let fn_ptr = *vtable.add(index);
    std::mem::transmute_copy(&fn_ptr)
}

/// Overwrite a single vtable slot (unprotect → write → restore).
/// Returns `None` if the slot already contains `new_fn` (avoids infinite recursion).
unsafe fn patch_vtable(obj: *mut core::ffi::c_void, index: usize, new_fn: usize) -> Option<usize> {
    use windows::Win32::System::Memory::{VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS};

    let vtable = *(obj as *const *mut usize);
    let entry = vtable.add(index);
    let original = *entry;

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
// Hooked functions
// ---------------------------------------------------------------------------

const DXGI_PRESENT_ALLOW_TEARING_FLAG: u32 = 0x200;

unsafe extern "system" fn hooked_present(
    this: *mut core::ffi::c_void,
    _sync_interval: u32,
    flags: u32,
) -> HRESULT {
    let vtable_ptr = *(this as *const *const usize) as usize;
    let orig = match VTABLE_MAP.lock() {
        Ok(map) => map.get(&vtable_ptr).map(|s| s.orig_present),
        Err(_) => None,
    };

    if let Some(orig_fn) = orig {
        let mut hr = HRESULT(0);
        
        // Force the engine to present immediately.
        let forced_sync = 0;
        let forced_flags = flags;
        
        // DEVNOTE: Tearing Support and Exclusive Fullscreen Lockups
        // We optimistically try to pass DXGI_PRESENT_ALLOW_TEARING.
        // However, if the game is somehow operating in legacy "Exclusive Fullscreen"
        // rather than the borderless Flip Model, DXGI absolutely forbids tearing 
        // flags and will immediately return DXGI_ERROR_INVALID_CALL. 
        // 
        // If we simply swallowed that error, the game screen would permanently 
        // freeze white/black because the frame never actually made it to the screen.
        // So we catch the error, strip the tearing flag, and gracefully `Present` 
        // again identically to how the engine intended.
        let mut try_tearing = TEARING_SUPPORTED.load(Ordering::Relaxed) && (forced_flags & DXGI_PRESENT_ALLOW_TEARING_FLAG) == 0;
        
        if try_tearing {
            hr = orig_fn(this, forced_sync, forced_flags | DXGI_PRESENT_ALLOW_TEARING_FLAG);
            if hr.0 == 0x887A0001_u32 as i32 { // DXGI_ERROR_INVALID_CALL
                try_tearing = false;
            }
        }
        
        // Debug logging for the first 100 frames
        let count = PRESENT_LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
        if count < 100 {
            info!("Present hooked! (sync={}, flags={:#x}, try_tearing={})", forced_sync, forced_flags, try_tearing);
        } else if count == 100 {
            info!("Present hooked! (stopping logs to prevent spam)");
        }

        if !try_tearing {
            hr = orig_fn(this, forced_sync, forced_flags);
        }
        hr
    } else {
        HRESULT(0)
    }
}

unsafe extern "system" fn hooked_present1(
    this: *mut core::ffi::c_void,
    _sync_interval: u32,
    flags: u32,
    params: *const DXGI_PRESENT_PARAMETERS,
) -> HRESULT {
    let vtable_ptr = *(this as *const *const usize) as usize;
    let orig = match VTABLE_MAP.lock() {
        Ok(map) => map.get(&vtable_ptr).and_then(|s| s.orig_present1),
        Err(_) => None,
    };

    if let Some(orig_fn) = orig {
        let mut hr = HRESULT(0);
        
        // Force sync=0
        let forced_sync = 0;
        let forced_flags = flags;
        
        let mut try_tearing = TEARING_SUPPORTED.load(Ordering::Relaxed) && (forced_flags & DXGI_PRESENT_ALLOW_TEARING_FLAG) == 0;
        
        if try_tearing {
            hr = orig_fn(this, forced_sync, forced_flags | DXGI_PRESENT_ALLOW_TEARING_FLAG, params);
            if hr.0 == 0x887A0001_u32 as i32 { // DXGI_ERROR_INVALID_CALL
                try_tearing = false;
            }
        }
        
        let count = PRESENT_LOG_COUNTER.fetch_add(1, Ordering::Relaxed);
        if count < 100 {
            info!("Present1 hooked! (sync={}, flags={:#x}, try_tearing={})", forced_sync, forced_flags, try_tearing);
        } else if count == 100 {
            info!("Present1 hooked! (stopping logs to prevent spam)");
        }

        if !try_tearing {
            hr = orig_fn(this, forced_sync, forced_flags, params);
        }
        hr
    } else {
        HRESULT(0)
    }
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
    let orig = match VTABLE_MAP.lock() {
        Ok(map) => map.get(&vtable_ptr).map(|s| s.orig_resize_buffers),
        Err(_) => None,
    };

    if let Some(orig_fn) = orig {
        let mut modified_flags = swap_chain_flags;
        if TEARING_SUPPORTED.load(Ordering::Relaxed) {
             // DEVNOTE: The F11 / Fullscreen Crash Fix
             // DXGI enforces a strict contract: "If a swap chain is created with 
             // DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING, that flag MUST also be passed to 
             // IDXGISwapChain::ResizeBuffers".
             // 
             // Because we surreptitiously inject ALLOW_TEARING under the hood during 
             // SwapChain creation, the engine is utterly unaware the flag exists. 
             // When the user resizes a window or presses F11, the engine politely 
             // calls `ResizeBuffers` with its original flagset (missing the tearing flag),
             // causing an instant, fatal DXGI panic.
             // We cure this by intercepting and secretly reinstating the tearing flag.
             let dxgi_swap_chain_flag_allow_tearing = 0x800;
             modified_flags |= dxgi_swap_chain_flag_allow_tearing;
        }

        let hr = orig_fn(this, buffer_count, width, height, new_format, modified_flags);
        // Re-hook after resize in case vtable changed entirely.
        hook_swap_chain_present(this);
        hr
    } else {
        HRESULT(0)
    }
}

// ---------------------------------------------------------------------------
// Swap-chain vtable hooking
// ---------------------------------------------------------------------------

/// Hook Present / Present1 / ResizeBuffers on a swap chain pointer.
/// Saves the original function pointers mapped by vtable.
unsafe fn hook_swap_chain_present(swap_chain: *mut core::ffi::c_void) {
    let vtable_ptr = *(swap_chain as *const *const usize) as usize;

    let mut state = VtableState {
        orig_present: vtable_fn(swap_chain, 8),
        orig_present1: None,
        orig_resize_buffers: vtable_fn(swap_chain, 13),
    };

    // If we've already patched this exact vtable, patch_vtable returns None.
    let patched_present = patch_vtable(swap_chain, 8, hooked_present as usize);
    if let Some(orig) = patched_present {
        state.orig_present = std::mem::transmute_copy(&orig);
        info!("Hooked Present (vtable[8]) — orig @ {:#x}", orig);
    }

    let patched_resize = patch_vtable(swap_chain, 13, hooked_resize_buffers as usize);
    if let Some(orig) = patched_resize {
        state.orig_resize_buffers = std::mem::transmute_copy(&orig);
        info!("Hooked ResizeBuffers (vtable[13]) — orig @ {:#x}", orig);
    }

    // Present1 only exists on IDXGISwapChain1+.
    let vtable = *(swap_chain as *const *const usize);
    let slot22 = *vtable.add(22);
    if slot22 != 0 {
        state.orig_present1 = Some(vtable_fn(swap_chain, 22));
        let patched_present1 = patch_vtable(swap_chain, 22, hooked_present1 as usize);
        if let Some(orig) = patched_present1 {
            state.orig_present1 = Some(std::mem::transmute_copy(&orig));
            info!("Hooked Present1 (vtable[22])");
        }
    }

    // Only insert if we actually patched something (i.e. it wasn't already hooked).
    if patched_present.is_some() || patched_resize.is_some() {
        if let Ok(mut map) = VTABLE_MAP.lock() {
            map.insert(vtable_ptr, state);
            info!("Registered vtable {:#x} in map", vtable_ptr);
        }
    }
}

// ---------------------------------------------------------------------------
// Factory vtable hooking  (CreateSwapChain, CreateSwapChainForHwnd, …)
// ---------------------------------------------------------------------------

const DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING: u32 = 0x800; // 2048

// DEVNOTE: Intercepting Creation is the "Nuclear Option"
// Without intercepting the exact moment the engine establishes the Swap Chain 
// and slipping `ALLOW_TEARING` into the configuration structures, modern DWM 
// totally overrides whatever commands we pass during `Present()`. It guarantees 
// the player enjoys true uncapped frames without Desktop composition tearing issues.
unsafe extern "system" fn hooked_create_swap_chain(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    desc: *mut DXGI_SWAP_CHAIN_DESC,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if TEARING_SUPPORTED.load(Ordering::Relaxed) && !desc.is_null() {
        info!("Injecting ALLOW_TEARING into CreateSwapChain");
        (*desc).Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    info!("CreateSwapChain called");
    if let Some(orig) = ORIG_CREATE_SWAP_CHAIN {
        let hr = orig(this, device, desc, swap_chain);
        if hr.is_ok() && !(*swap_chain).is_null() {
            info!("CreateSwapChain succeeded — hooking Present");
            hook_swap_chain_present(*swap_chain);
        }
        hr
    } else {
        HRESULT(-1)
    }
}

// For CreateSwapChainFor... the desc is unfortunately const.
// To modify Flags we must create a mutable copy and pass the copy.
unsafe extern "system" fn hooked_create_swap_chain_for_hwnd(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    hwnd: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen_desc: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let mut modified_desc = *desc;
    if TEARING_SUPPORTED.load(Ordering::Relaxed) {
        info!("Injecting ALLOW_TEARING into CreateSwapChainForHwnd");
        modified_desc.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    info!("CreateSwapChainForHwnd called");
    if let Some(orig) = ORIG_CREATE_SWAP_CHAIN_FOR_HWND {
        let hr = orig(this, device, hwnd, &modified_desc, fullscreen_desc, restrict_to_output, swap_chain);
        if hr.is_ok() && !(*swap_chain).is_null() {
            info!("CreateSwapChainForHwnd succeeded — hooking Present");
            hook_swap_chain_present(*swap_chain);
        }
        // If it fails with the tearing flag, try again without it just in case.
        if hr.is_err() && TEARING_SUPPORTED.load(Ordering::Relaxed) {
             warn!("CreateSwapChainForHwnd failed, retrying without ALLOW_TEARING...");
             let hr_retry = orig(this, device, hwnd, desc, fullscreen_desc, restrict_to_output, swap_chain);
             if hr_retry.is_ok() && !(*swap_chain).is_null() {
                 hook_swap_chain_present(*swap_chain);
             }
             return hr_retry;
        }
        hr
    } else {
        HRESULT(-1)
    }
}

unsafe extern "system" fn hooked_create_swap_chain_for_core_window(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    window: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let mut modified_desc = *desc;
    if TEARING_SUPPORTED.load(Ordering::Relaxed) {
        info!("Injecting ALLOW_TEARING into CreateSwapChainForCoreWindow");
        modified_desc.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    info!("CreateSwapChainForCoreWindow called");
    if let Some(orig) = ORIG_CREATE_SWAP_CHAIN_FOR_CORE_WINDOW {
        let hr = orig(this, device, window, &modified_desc, restrict_to_output, swap_chain);
        if hr.is_ok() && !(*swap_chain).is_null() {
            info!("CreateSwapChainForCoreWindow succeeded — hooking Present");
            hook_swap_chain_present(*swap_chain);
        }
        if hr.is_err() && TEARING_SUPPORTED.load(Ordering::Relaxed) {
             let hr_retry = orig(this, device, window, desc, restrict_to_output, swap_chain);
             if hr_retry.is_ok() && !(*swap_chain).is_null() {
                 hook_swap_chain_present(*swap_chain);
             }
             return hr_retry;
        }
        hr
    } else {
        HRESULT(-1)
    }
}

unsafe extern "system" fn hooked_create_swap_chain_for_composition(
    this: *mut core::ffi::c_void,
    device: *mut core::ffi::c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    restrict_to_output: *mut core::ffi::c_void,
    swap_chain: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let mut modified_desc = *desc;
    if TEARING_SUPPORTED.load(Ordering::Relaxed) {
        info!("Injecting ALLOW_TEARING into CreateSwapChainForComposition");
        modified_desc.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
    }

    info!("CreateSwapChainForComposition called");
    if let Some(orig) = ORIG_CREATE_SWAP_CHAIN_FOR_COMPOSITION {
        let hr = orig(this, device, &modified_desc, restrict_to_output, swap_chain);
        if hr.is_ok() && !(*swap_chain).is_null() {
            info!("CreateSwapChainForComposition succeeded — hooking Present");
            hook_swap_chain_present(*swap_chain);
        }
        if hr.is_err() && TEARING_SUPPORTED.load(Ordering::Relaxed) {
             let hr_retry = orig(this, device, desc, restrict_to_output, swap_chain);
             if hr_retry.is_ok() && !(*swap_chain).is_null() {
                 hook_swap_chain_present(*swap_chain);
             }
             return hr_retry;
        }
        hr
    } else {
        HRESULT(-1)
    }
}

/// Hook the factory's swap-chain-creation methods.
unsafe fn hook_factory(factory: *mut core::ffi::c_void) {
    if FACTORY_HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }
    let saved = patch_vtable(factory, 10, hooked_create_swap_chain as usize);
    if let Some(orig) = saved {
        ORIG_CREATE_SWAP_CHAIN = Some(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory::CreateSwapChain (vtable[10])");
    }
}

/// Hook IDXGIFactory2 methods.
unsafe fn hook_factory2(factory: *mut core::ffi::c_void) {
    if FACTORY2_HOOKED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(orig) = patch_vtable(factory, 15, hooked_create_swap_chain_for_hwnd as usize) {
        ORIG_CREATE_SWAP_CHAIN_FOR_HWND = Some(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory2::CreateSwapChainForHwnd (vtable[15])");
    }

    if let Some(orig) = patch_vtable(factory, 16, hooked_create_swap_chain_for_core_window as usize) {
        ORIG_CREATE_SWAP_CHAIN_FOR_CORE_WINDOW = Some(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory2::CreateSwapChainForCoreWindow (vtable[16])");
    }

    if let Some(orig) = patch_vtable(factory, 24, hooked_create_swap_chain_for_composition as usize) {
        ORIG_CREATE_SWAP_CHAIN_FOR_COMPOSITION = Some(std::mem::transmute_copy(&orig));
        info!("Hooked IDXGIFactory2::CreateSwapChainForComposition (vtable[24])");
    }
}

// ---------------------------------------------------------------------------
// Tearing support detection
// ---------------------------------------------------------------------------

unsafe fn check_tearing_support(factory: *mut core::ffi::c_void) {
    type QueryInterfaceFn = unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> HRESULT;

    let qi: QueryInterfaceFn = vtable_fn(factory, 0);

    // IDXGIFactory5 IID: {7632e1f5-ee65-4dca-87fd-84cd75f8838d}
    let iid_factory5 = GUID {
        data1: 0x7632e1f5,
        data2: 0xee65,
        data3: 0x4dca,
        data4: [0x87, 0xfd, 0x84, 0xcd, 0x75, 0xf8, 0x83, 0x8d],
    };
    let mut factory5_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
    let hr = qi(factory, &iid_factory5, &mut factory5_ptr);
    if hr.is_ok() && !factory5_ptr.is_null() {
        type CheckFeatureSupportFn = unsafe extern "system" fn(
            this: *mut core::ffi::c_void,
            feature: u32,
            data: *mut core::ffi::c_void,
            data_size: u32,
        ) -> HRESULT;
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
            info!("Tearing is NOT supported (hr={:?}, val={})", hr2, allow_tearing);
        }

        type ReleaseFn = unsafe extern "system" fn(this: *mut core::ffi::c_void) -> u32;
        let release: ReleaseFn = vtable_fn(factory5_ptr, 2);
        release(factory5_ptr);
    } else {
        info!("IDXGIFactory5 not available — tearing flag will not be used");
    }
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
    let dir_str = std::str::from_utf8(&sys_dir[..len as usize]).unwrap_or("C:\\Windows\\System32");
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
// Exported entry points
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
        hook_factory(*factory);
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
        hook_factory(*factory);
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
        hook_factory(*factory);
        hook_factory2(*factory);
    }
    hr
}

#[no_mangle]
pub unsafe extern "system" fn DXGIGetDebugInterface1(
    flags: u32,
    riid: *const GUID,
    debug: *mut *mut core::ffi::c_void,
) -> HRESULT {
    type RealFn = unsafe extern "system" fn(u32, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT;
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
            error!("FATAL: Could not load the real dxgi.dll");
        }
    }
    TRUE
}
